// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The end probe: a track's end padding is read from the last pages of its stream, however far
//! back they begin, also in a physical stream that another follows, and the probe looks back no
//! further than a stream needs: not past a page that is lost, and not for a stream whose start it
//! reads among the last pages; a page that fails its CRC hides no page after it. The streams are
//! the fixtures' packets paginated again in memory, as a muxer may write them, so that the pages
//! the probe needs lie where its first look at the end doesn't reach, or a fixture's pages with
//! one damaged. The fixtures are made by `fixtures/regenerate.sh`.

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use symphonia_common::xiph::audio::opus::packet_frames;
use symphonia_core::checksum::Crc32;
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatReader, Track};
use symphonia_core::io::{MediaSource, MediaSourceStream, Monitor};
use symphonia_core::packet::Packet;
use symphonia_core::units::Timestamp;
use symphonia_format_ogg::OggReader;

/// The largest Ogg page: its header, a full segment table, and 255 full segments. The end probe
/// first reads this many bytes for each logical stream at the end of the physical stream, and the
/// start probe reads no further than this from the first page of data.
const OGG_PAGE_MAX_SIZE: usize = 27 + 255 + 255 * 255;

/// The header type flags of a page (RFC 3533, section 6).
const CONTINUED: u8 = 1;
const FIRST: u8 = 2;
const LAST: u8 = 4;

/// The granule position of a page on which no packet ends (RFC 3533, section 6).
const NO_POSITION: u64 = u64::MAX;

/// The frames of a 20 ms Opus packet.
const OPUS_PACKET: u64 = 960;

/// The pre-skip of the Opus fixtures, and the frames FFmpeg's last packet decodes past the end.
const PRE_SKIP: u64 = 312;
const OPUS_PADDING: u64 = 648;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// A logical stream of an Ogg stream: its packets, and for each of its pages the granule position,
/// the header type flags, and the number of its packets that ended up to and with the page.
struct Stream {
    packets: Vec<Vec<u8>>,
    pages: Vec<(u64, u8, usize)>,
}

/// Read the logical stream `serial` of an Ogg stream.
fn read_stream(data: &[u8], serial: u32) -> Stream {
    let mut stream = Stream { packets: Vec::new(), pages: Vec::new() };
    let mut packet = Vec::new();
    let mut at = 0;

    while at < data.len() {
        assert_eq!(&data[at..at + 4], b"OggS", "a page at {at}");
        let flags = data[at + 5];
        let granule = u64::from_le_bytes(data[at + 6..at + 14].try_into().expect("8 bytes"));
        let page_serial = u32::from_le_bytes(data[at + 14..at + 18].try_into().expect("4 bytes"));
        let segments = &data[at + 27..at + 27 + usize::from(data[at + 26])];

        at += 27 + segments.len();

        for &len in segments {
            if page_serial == serial {
                packet.extend_from_slice(&data[at..at + usize::from(len)]);
                if len < 255 {
                    stream.packets.push(std::mem::take(&mut packet));
                }
            }
            at += usize::from(len);
        }

        if page_serial == serial {
            stream.pages.push((granule, flags, stream.packets.len()));
        }
    }

    stream
}

/// A piece of a packet on a page: the packet's bytes on the page, and whether it ends there.
type Piece<'a> = (&'a [u8], bool);

/// Each of `packets` whole on a page.
fn whole(packets: &[Vec<u8>]) -> impl Iterator<Item = Piece<'_>> {
    packets.iter().map(|packet| (packet.as_slice(), true))
}

/// Write an Ogg page of `packets`, each whole.
fn page_of(serial: u32, seq: u32, granule: u64, flags: u8, packets: &[Vec<u8>]) -> Vec<u8> {
    page(serial, seq, granule, flags, &whole(packets).collect::<Vec<_>>())
}

/// Write an Ogg page of `pieces`.
fn page(serial: u32, seq: u32, granule: u64, flags: u8, pieces: &[Piece<'_>]) -> Vec<u8> {
    let mut lacing = Vec::new();

    for &(data, ends) in pieces {
        assert!(ends || data.len() % 255 == 0, "a packet continues after a whole segment");
        lacing.extend(std::iter::repeat_n(255, data.len() / 255));
        if ends {
            lacing.push((data.len() % 255) as u8);
        }
    }
    assert!(lacing.len() <= 255, "a page holds at most 255 segments");

    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0; 4]);
    page.push(lacing.len() as u8);
    page.extend_from_slice(&lacing);

    for &(data, _) in pieces {
        page.extend_from_slice(data);
    }

    seal(&mut page);

    page
}

/// Write the CRC of an Ogg page into its header.
fn seal(page: &mut [u8]) {
    page[22..26].fill(0);

    let mut crc = Crc32::new(0);
    crc.process_buf_bytes(page);
    page[22..26].copy_from_slice(&crc.crc().to_le_bytes());
}

/// Paginate `packets` from page `seq` on as a muxer does at its largest: each page fills its 255
/// segments, and a packet that doesn't fit continues on the next page. A page by the end of which
/// `n` packets ended has the granule position `granule(n)`, and the last page `end`.
fn paginate(
    serial: u32,
    seq: u32,
    packets: &[Vec<u8>],
    granule: impl Fn(usize) -> u64,
    end: u64,
) -> Vec<Vec<u8>> {
    let mut pages = Vec::new();
    // The next packet, and how much of it the pages hold.
    let (mut next, mut paged) = (0, 0);

    while next < packets.len() {
        let flags = if paged > 0 { CONTINUED } else { 0 };
        let ended = next;
        let mut pieces = Vec::new();
        let mut room = 255;

        while next < packets.len() && room > 0 {
            let rest = &packets[next][paged..];
            let segments = rest.len() / 255 + 1;

            if segments <= room {
                pieces.push((rest, true));
                room -= segments;
                (next, paged) = (next + 1, 0);
            }
            else {
                pieces.push((&rest[..room * 255], false));
                paged += room * 255;
                room = 0;
            }
        }

        let seq = seq + pages.len() as u32;
        let page = if next == packets.len() {
            page(serial, seq, end, flags | LAST, &pieces)
        }
        else if next > ended {
            page(serial, seq, granule(next), flags, &pieces)
        }
        else {
            page(serial, seq, NO_POSITION, flags, &pieces)
        };
        pages.push(page);
    }

    pages
}

/// The header pages and the pages of data of an Opus stream of `count` packets of 20 ms, as a
/// muxer at its largest pages writes them: the packets of `opus_small_pages.opus` (510 kbps CBR)
/// over and over, 42 or 43 of them on a page of about 54 KB. Its last packet decodes 648 frames
/// past its end.
fn long_opus_stream(serial: u32, count: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let source = read_stream(&fixture("opus_small_pages.opus"), 0);
    let packets: Vec<_> = source.packets[2..].iter().cycle().take(count).cloned().collect();

    let headers = vec![
        page(serial, 0, 0, FIRST, &[(&source.packets[0], true)]),
        page(serial, 1, 0, 0, &[(&source.packets[1], true)]),
    ];

    let end = count as u64 * OPUS_PACKET - OPUS_PADDING;
    let data = paginate(serial, 2, &packets, |n| n as u64 * OPUS_PACKET, end);

    (headers, data)
}

/// The granule position of a page: where its stream's timeline is at its end.
fn granule(page: &[u8]) -> u64 {
    u64::from_le_bytes(page[6..14].try_into().expect("8 bytes"))
}

/// The serial and the sequence number of a page.
fn serial_and_seq(page: &[u8]) -> (u32, u32) {
    let field = |at: usize| u32::from_le_bytes(page[at..at + 4].try_into().expect("4 bytes"));
    (field(14), field(18))
}

/// Where page `n` of `pages` begins.
fn bytes_before(pages: &[Vec<u8>], n: usize) -> usize {
    pages[..n].iter().map(Vec::len).sum()
}

/// Multiplex the pages of data of a second stream into those of a first, each page before the
/// first page of the first stream that ends later, as FFmpeg does. Pages on which no packet ends
/// are not expected.
fn multiplex(first: Vec<Vec<u8>>, second: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut pages = Vec::with_capacity(first.len() + second.len());
    let mut second = second.into_iter().peekable();

    for page in first {
        while let Some(other) = second.next_if(|other| granule(other) <= granule(&page)) {
            pages.push(other);
        }
        pages.push(page);
    }
    pages.extend(second);

    pages
}

/// An Ogg stream with the serial of every page set to `serial`, its pages otherwise as they were.
fn with_serial(data: &[u8], serial: u32) -> Vec<u8> {
    let mut data = data.to_vec();
    let mut at = 0;

    while at < data.len() {
        let segments = &data[at + 27..at + 27 + usize::from(data[at + 26])];
        let len = 27 + segments.len() + segments.iter().map(|&len| usize::from(len)).sum::<usize>();

        let page = &mut data[at..at + len];
        page[14..18].copy_from_slice(&serial.to_le_bytes());
        seal(page);

        at += len;
    }

    data
}

/// The pages of an Ogg stream, each as it is.
fn split_pages(data: &[u8]) -> Vec<Vec<u8>> {
    let mut pages = Vec::new();
    let mut at = 0;

    while at < data.len() {
        let segments = &data[at + 27..at + 27 + usize::from(data[at + 26])];
        let len = 27 + segments.len() + segments.iter().map(|&len| usize::from(len)).sum::<usize>();

        pages.push(data[at..at + len].to_vec());
        at += len;
    }

    pages
}

/// Damage the last byte of a page, so that the page fails its CRC.
fn fail_crc(page: &mut [u8]) {
    *page.last_mut().expect("a page") ^= 0xff;
}

/// The number of packets that end on a page.
fn packets_ending_on(page: &[u8]) -> usize {
    page[27..27 + usize::from(page[26])].iter().filter(|&&len| len < 255).count()
}

/// A media source that counts the bytes read from it.
struct Counted {
    data: Cursor<Vec<u8>>,
    read: Arc<AtomicU64>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.data.read(buf)?;
        self.read.fetch_add(len as u64, Ordering::Relaxed);
        Ok(len)
    }
}

impl Seek for Counted {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.data.seek(pos)
    }
}

impl MediaSource for Counted {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.data.get_ref().len() as u64)
    }
}

/// Open a physical stream of `pages`, and return the reader and the bytes it read to open it.
fn open(pages: &[Vec<u8>]) -> (OggReader<'static>, u64) {
    let read = Arc::new(AtomicU64::new(0));
    let source = Counted { data: Cursor::new(pages.concat()), read: Arc::clone(&read) };

    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let reader = OggReader::try_new(mss, Default::default()).expect("an Ogg stream");

    (reader, read.load(Ordering::Relaxed))
}

fn track(reader: &OggReader<'_>, id: u32) -> Track {
    reader.tracks().iter().find(|track| track.id == id).expect("the track").clone()
}

/// Read the packets to the end of the stream, and return them.
fn packets(reader: &mut OggReader<'_>) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().expect("a packet") {
        packets.push(packet);
    }
    packets
}

/// The frames an Opus track's packets present after their trims, and the first frame they present.
fn presented(packets: &[Packet], track_id: u32) -> (u64, i64) {
    let packets: Vec<_> = packets.iter().filter(|p| p.track_id == track_id).collect();

    for packet in &packets {
        let decoded = packet_frames(&packet.data).expect("a well-formed Opus packet");
        assert_eq!(packet.block_dur().get(), u64::from(decoded), "{packet:?}");
    }

    let first = packets.iter().find(|p| p.dur.get() > 0).expect("a packet that presents frames");
    let frames = packets.iter().map(|p| p.dur.get()).sum();

    (frames, first.pts.get() + first.trim_start.get() as i64)
}

/// A Vorbis packet decodes to the frames from the middle of the block before it to the middle of
/// its own (Vorbis I, section 1.3.2), so its duration depends on the packet before. The padding of
/// a last page is read once the packet before its first packet is parsed, though that packet began
/// too far back for the probe's first look at the end, whichever block sizes the two packets have,
/// and also when the last page continues its first packet.
#[test]
fn a_vorbis_padding_is_read_once_the_packet_before_the_last_page_is_parsed() {
    const PADDING: u64 = 300;

    let source = read_stream(&fixture("vorbis_bursts.ogg"), 0);
    let (ident, audio) = (&source.packets[0], &source.packets[3..]);

    // libvorbis writes two modes, of the short and of the long block, so a packet's mode is bit 1
    // of its first byte. The block sizes are in the identification header.
    let (short, long) = (1u64 << (ident[28] & 0xf), 1u64 << (ident[28] >> 4));
    let block: Vec<_> = audio.iter().map(|p| if p[0] & 2 != 0 { long } else { short }).collect();

    // The end of each packet's frames: the first packet laps with none, and decodes to none.
    let ends: Vec<u64> = (0..audio.len())
        .scan(0, |end, k| {
            *end += if k > 0 { (block[k - 1] + block[k]) / 4 } else { 0 };
            Some(*end)
        })
        .collect();

    for &(granule, flags, ended) in &source.pages[2..] {
        if flags & LAST == 0 {
            assert_eq!(granule, ends[ended - 4], "the granule positions of vorbis_bursts.ogg");
        }
    }

    // The packet before the last page's first packet: from the last of each pair of block sizes.
    let before = |pair: (u64, u64)| {
        (4..audio.len() - 5).rev().find(|&k| (block[k], block[k + 1]) == pair).expect("a pair")
    };

    for (p, next_continues) in [
        (before((long, short)), false),
        (before((short, long)), false),
        (before((long, short)), true),
    ] {
        let context = format!("{}-{} blocks", block[p], block[p + 1]);

        // Packet p is grown with zero bytes after its audio data (the reader times a Vorbis packet
        // by its first byte), so it begins on a page too far back for the probe's first look at
        // the end and ends on the page before the last. Its next packet, grown too, may begin
        // there and end on the last page.
        let mut grown = audio[p].clone();
        grown.resize(70_000, 0);
        let mut next = audio[p + 1].clone();
        if next_continues {
            next.resize(600, 0);
        }

        // The last page ends the stream 300 frames before its packets' end, after the page before.
        let last = p + 4;
        assert!(ends[last] - PADDING > ends[p], "{context}");

        let head = 252 * 255;
        let mut z: Vec<Piece<'_>> = whole(&audio[p - 3..p]).collect();
        z.push((&grown[..head], false));

        let (a, b): (Vec<Piece<'_>>, Vec<Piece<'_>>) = if next_continues {
            let a = vec![(&grown[head..], true), (&next[..510], false)];
            (a, [(&next[510..], true)].into_iter().chain(whole(&audio[p + 2..=last])).collect())
        }
        else {
            (vec![(&grown[head..], true)], whole(&audio[p + 1..=last]).collect())
        };

        let last_flags = LAST | if next_continues { CONTINUED } else { 0 };

        let pages = [
            page(0, 0, 0, FIRST, &[(ident, true)]),
            page(0, 1, 0, 0, &[(&source.packets[1], true), (&source.packets[2], true)]),
            page_of(0, 2, ends[p - 4], 0, &audio[..p - 3]),
            page(0, 3, ends[p - 1], 0, &z),
            page(0, 4, ends[p], CONTINUED, &a),
            page(0, 5, ends[last] - PADDING, last_flags, &b),
        ];

        // The probe's first look at the end begins inside the page packet p begins on.
        let tail = pages[4].len() + pages[5].len();
        assert!(
            tail <= OGG_PAGE_MAX_SIZE && tail + pages[3].len() > OGG_PAGE_MAX_SIZE,
            "{context}"
        );

        let (mut reader, _) = open(&pages);
        assert_eq!(reader.tracks()[0].padding, Some(PADDING as u32), "{context}");

        let trimmed: u64 = packets(&mut reader).iter().map(|p| p.trim_end.get()).sum();
        assert_eq!(trimmed, PADDING, "{context}: the padding the reader trims");
    }
}

/// A page lost between the page the end padding is counted from and the last page leaves the
/// padding unknown: the page after the loss is the last page, or begins the packet that the last
/// page ends. The padding would otherwise be counted from the end of the page before the loss.
/// The last page's packets decode 1500 frames past the end, which is past the end of the page
/// before it.
#[test]
fn a_page_lost_before_the_last_page_leaves_the_padding_unknown() {
    const PADDING: u64 = 1500;

    let source = read_stream(&fixture("opus_small_pages.opus"), 0);
    let (head, tags, audio) = (&source.packets[0], &source.packets[1], &source.packets[2..]);
    let at = |packets: usize| packets as u64 * OPUS_PACKET;
    let end = at(11) - PADDING;

    // ..., [0, 4), [4, 8), lost: [8], then [9, 10] on the last page.
    let last_page = vec![
        page(0, 0, 0, FIRST, &[(head, true)]),
        page(0, 1, 0, 0, &[(tags, true)]),
        page_of(0, 2, at(4), 0, &audio[..4]),
        page_of(0, 3, at(8), 0, &audio[4..8]),
        page_of(0, 4, at(9), 0, &audio[8..9]),
        page_of(0, 5, end, LAST, &audio[9..11]),
    ];

    // ..., [0, 4), [4, 8), lost: [8], then the start of 9, and the last page: the rest of 9, 10.
    let (start, rest) = audio[9].split_at(3 * 255);
    let continued = vec![
        page(0, 0, 0, FIRST, &[(head, true)]),
        page(0, 1, 0, 0, &[(tags, true)]),
        page_of(0, 2, at(4), 0, &audio[..4]),
        page_of(0, 3, at(8), 0, &audio[4..8]),
        page_of(0, 4, at(9), 0, &audio[8..9]),
        page(0, 5, NO_POSITION, 0, &[(start, false)]),
        page(0, 6, end, LAST | CONTINUED, &[(rest, true), (&audio[10], true)]),
    ];

    for (name, mut pages) in
        [("the last page", last_page), ("a page the last page continues", continued)]
    {
        let (mut reader, _) = open(&pages);
        assert_eq!(reader.tracks()[0].padding, Some(PADDING as u32), "{name}, none lost");

        let trimmed: u64 = packets(&mut reader).iter().map(|p| p.trim_end.get()).sum();
        assert_eq!(trimmed, PADDING, "{name}, none lost: the padding the reader trims");

        pages.remove(4);
        let (reader, _) = open(&pages);
        assert_eq!(reader.tracks()[0].num_frames, Some(end - PRE_SKIP), "{name} after the loss");
        assert_eq!(reader.tracks()[0].padding, None, "{name} after the loss");
    }
}

/// The last page continues a packet that began on the page before it, which begins too far back
/// for the probe's first look at the end, so the probe looks again from further back. With that
/// page lost (cut, or failing its CRC: the probe skips it, as the reader does), no earlier page
/// gives the padding, and the probe stops looking back once it has read past the loss: to open the
/// stream it reads less than the stream.
#[test]
fn the_end_probe_stops_looking_back_once_a_page_the_padding_needs_is_lost() {
    // Ten pages of data: the last continues the packet the ninth begins.
    let count = 85 * 4 + 43 + 15;
    let (headers, data) = long_opus_stream(0, count);
    let length = count as u64 * OPUS_PACKET - OPUS_PADDING - PRE_SKIP;

    let (before, last) = (&data[data.len() - 2], &data[data.len() - 1]);
    assert_eq!(data.len(), 10);
    assert_eq!(last[5], LAST | CONTINUED, "the last page continues a packet");
    assert!(before.len() + last.len() > OGG_PAGE_MAX_SIZE, "the page before begins far back");

    let intact = [headers.as_slice(), &data].concat();
    let (reader, _) = open(&intact);
    assert_eq!(reader.tracks()[0].num_frames, Some(length));
    assert_eq!(reader.tracks()[0].padding, Some(OPUS_PADDING as u32));

    let mut cut = intact.clone();
    cut.remove(cut.len() - 2);

    let mut damaged = intact;
    let before_last = damaged.len() - 2;
    fail_crc(&mut damaged[before_last]);

    for (name, lost) in [("cut", cut), ("failing its CRC", damaged)] {
        let len = bytes_before(&lost, lost.len());

        let (reader, read) = open(&lost);
        assert_eq!(reader.tracks()[0].num_frames, Some(length), "{name}");
        assert_eq!(reader.tracks()[0].padding, None, "{name}");
        assert!(read < len as u64, "{name}: opening read {read} bytes of the stream's {len}");
    }
}

/// A stream of a single page of data, written at the end of a longer stream (FFmpeg writes a short
/// stream so with `-itsoffset`): the start probe doesn't reach its page, and the end probe reads its
/// start there. So no earlier page is needed for its padding (the stream starts on the page that
/// ends it), and to open the physical stream the probes read less than it.
#[test]
fn a_single_page_stream_at_the_end_needs_no_further_look_back() {
    let (headers, data) = long_opus_stream(0, 85 * 5);
    let end = granule(data.last().expect("a last page"));

    // opus_short.opus, 250 ms of a single page, offset to end 500 ms before the first stream.
    let short = read_stream(&fixture("opus_short.opus"), 0);
    let offset = end - 24_000 - short.pages[2].0;

    let pages = [
        &headers[..1],
        &[page(1, 0, 0, FIRST, &[(&short.packets[0], true)])],
        &headers[1..],
        &[page(1, 1, 0, 0, &[(&short.packets[1], true)])],
        &multiplex(data, vec![page_of(1, 2, short.pages[2].0 + offset, LAST, &short.packets[2..])]),
    ]
    .concat();

    let len = bytes_before(&pages, pages.len());
    let at = pages.iter().position(|page| serial_and_seq(page) == (1, 2)).expect("its page");
    let pos = bytes_before(&pages, at);
    assert!(pos > bytes_before(&pages, 4) + OGG_PAGE_MAX_SIZE, "past the start probe's reach");
    assert!(pos >= len - 2 * OGG_PAGE_MAX_SIZE, "within the end probe's first look");

    let (reader, read) = open(&pages);

    // A single page doesn't tell a stream that starts late from one that starts at 0 (RFC 7845,
    // section 4.5): it starts at 0, as FFmpeg states too.
    let track = track(&reader, 1);
    assert_eq!(track.start_ts, Timestamp::new(0));
    assert_eq!(track.num_frames, Some(offset + 12_000));
    assert_eq!(reader.tracks()[0].padding, Some(OPUS_PADDING as u32));

    assert!(read < len as u64, "opening read {read} bytes of the stream's {len}");
}

/// A stream whose data begins past the start probe's reach, among the pages the end probe reads:
/// its start is read there, so it starts at its first presented frame and its length counts from
/// it, as the reader presents it.
#[test]
fn a_stream_whose_data_begins_among_the_last_pages_starts_where_it_begins() {
    let (headers, data) = long_opus_stream(0, 85 * 5);
    let end = granule(data.last().expect("a last page"));

    // opus_stereo.opus, 1 s in pages of 200 ms, offset to end 500 ms before the first stream.
    let stereo = read_stream(&fixture("opus_stereo.opus"), 0);
    let audio = &stereo.packets[2..];
    let last_end = audio.len() as u64 * OPUS_PACKET - OPUS_PADDING;
    let offset = end - 24_000 - last_end;

    let chunks: Vec<_> = audio.chunks(10).collect();
    let second: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(n, packets)| {
            let seq = 2 + n as u32;
            if n + 1 == chunks.len() {
                page_of(1, seq, offset + last_end, LAST, packets)
            }
            else {
                page_of(1, seq, offset + 10 * (n as u64 + 1) * OPUS_PACKET, 0, packets)
            }
        })
        .collect();

    let pages = [
        &headers[..1],
        &[page(1, 0, 0, FIRST, &[(&stereo.packets[0], true)])],
        &headers[1..],
        &[page(1, 1, 0, 0, &[(&stereo.packets[1], true)])],
        &multiplex(data, second),
    ]
    .concat();

    let len = bytes_before(&pages, pages.len());
    let at = pages.iter().position(|page| serial_and_seq(page) == (1, 2)).expect("its first page");
    let pos = bytes_before(&pages, at);
    assert!(pos > bytes_before(&pages, 4) + OGG_PAGE_MAX_SIZE, "past the start probe's reach");
    assert!(pos >= len - 2 * OGG_PAGE_MAX_SIZE, "within the end probe's first look");

    let (mut reader, _) = open(&pages);

    let track = track(&reader, 1);
    assert_eq!(track.start_ts, Timestamp::new(offset as i64));
    assert_eq!(track.num_frames, Some(48_000));
    assert_eq!(track.padding, Some(OPUS_PADDING as u32));

    let (frames, first) = presented(&packets(&mut reader), 1);
    assert_eq!((frames, first), (48_000, offset as i64), "what the reader presents");
}

/// A physical stream that another follows (a chain, RFC 3533, section 4) states its length and end
/// padding as it does alone. The probe finds where it ends by bisection, which stops before the
/// end of its last page, and reads on from there to the first page of the next physical stream.
/// `opus_big_pages.opus` (2.2 s, its last two pages of 54 KB and 33 KB) followed by
/// `opus_small_pages.opus` as serial 1 has the bisection stop inside its last page, and the probe
/// look back further for the page before it.
#[test]
fn a_chained_physical_stream_states_its_length_and_padding() {
    let first = fixture("opus_big_pages.opus");
    let second = with_serial(&fixture("opus_small_pages.opus"), 1);
    assert!(second.len() > OGG_PAGE_MAX_SIZE, "the probe's first look at the end misses the first");

    for (name, chain) in [("alone", vec![first.clone()]), ("chained", vec![first, second])] {
        let (reader, _) = open(&chain);
        let track = track(&reader, 0);
        assert_eq!(
            (track.num_frames, track.padding),
            (Some(105_600), Some(OPUS_PADDING as u32)),
            "{name}"
        );
    }
}

/// A page that fails its CRC is skipped, by the end probe as by the reader, and the probe reads on
/// past it to the last page of every stream. `opus_flac.ogg` (2 s of Opus and of FLAC in pages of
/// 100 ms) ends with the FLAC stream's last page, then the Opus stream's. With either of the FLAC
/// stream's last two pages damaged, the Opus stream states its length and padding as intact, and
/// the FLAC stream its length unless its last page is the damaged one. The reader reads every Opus
/// packet and trims the padding stated, and every FLAC packet but those that end on the damaged
/// page. It ends the stream once every stream has read its last page, so with the FLAC stream's
/// last page lost, the stream ends unexpectedly, as a stream cut short does.
#[test]
fn the_end_probe_reads_on_past_a_page_that_fails_its_crc() {
    const OPUS_LENGTH: u64 = 96_000;
    const FLAC_LENGTH: u64 = 32_000;

    let intact = split_pages(&fixture("opus_flac.ogg"));
    let last_of = |serial: u32, pages: &[Vec<u8>]| {
        pages.iter().rposition(|page| serial_and_seq(page).0 == serial).expect("a page")
    };
    let flac_last = last_of(1, &intact);
    let flac_before = last_of(1, &intact[..flac_last]);
    assert!(last_of(0, &intact) > flac_last, "the Opus stream's last page after the FLAC stream's");

    let (mut reader, _) = open(&intact);
    let opus = track(&reader, 0);
    assert_eq!((opus.num_frames, opus.padding), (Some(OPUS_LENGTH), Some(OPUS_PADDING as u32)));
    assert_eq!(track(&reader, 1).num_frames, Some(FLAC_LENGTH));

    let all = packets(&mut reader);
    let count = |of: &[Packet], id: u32| of.iter().filter(|p| p.track_id == id).count();

    for (name, at, flac_length) in [
        ("the page before the FLAC stream's last", flac_before, Some(FLAC_LENGTH)),
        ("the FLAC stream's last page", flac_last, None),
    ] {
        let mut pages = intact.clone();
        fail_crc(&mut pages[at]);

        let (mut reader, _) = open(&pages);
        let opus = track(&reader, 0);
        assert_eq!(
            (opus.num_frames, opus.padding),
            (Some(OPUS_LENGTH), Some(OPUS_PADDING as u32)),
            "{name}"
        );
        assert_eq!(track(&reader, 1).num_frames, flac_length, "{name}");

        let mut read = Vec::new();
        let end = loop {
            match reader.next_packet() {
                Ok(Some(packet)) => read.push(packet),
                end => break end,
            }
        };

        assert_eq!(count(&read, 0), count(&all, 0), "{name}: the Opus packets");
        let trimmed: u64 = read.iter().filter(|p| p.track_id == 0).map(|p| p.trim_end.get()).sum();
        assert_eq!(trimmed, OPUS_PADDING, "{name}: the padding the reader trims");
        assert_eq!(
            count(&read, 1),
            count(&all, 1) - packets_ending_on(&intact[at]),
            "{name}: the FLAC packets"
        );

        match (flac_length, end) {
            (Some(_), Ok(None)) => (),
            (None, Err(Error::IoError(err))) if err.kind() == io::ErrorKind::UnexpectedEof => (),
            (_, end) => panic!("{name}: the reader ended with {end:?}"),
        }
    }
}
