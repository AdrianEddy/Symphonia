// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opus in Ogg: the pre-skip and the end padding are trimmed by the reader, a seek takes the
//! pre-roll and trims the pre-skip only at the start of the stream, wherever it lands, and every
//! channel mapping family is carried to the decoder. The fixtures are made by
//! `fixtures/regenerate.sh`.

use std::io::Cursor;
use std::path::Path;

use symphonia_common::xiph::audio::opus::packet_frames;
use symphonia_core::audio::Channels;
use symphonia_core::checksum::Crc32;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia_core::io::{MediaSourceStream, Monitor};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_ogg::OggReader;

/// The identification packet starts after the first page's 27-byte header and its 1-byte segment
/// table.
const HEAD: usize = 28;

/// The largest Ogg page: its header, a full segment table, and 255 full segments. A seek bisects a
/// stream larger than two of them.
const OGG_PAGE_MAX_SIZE: usize = 27 + 255 + 255 * 255;

/// The frames of the seek pre-roll, 80 ms (RFC 7845, §4.6).
const PRE_ROLL: i64 = 3840;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn open(data: Vec<u8>) -> symphonia_core::errors::Result<OggReader<'static>> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    OggReader::try_new(mss, Default::default())
}

/// The frames a packet decodes to, read from its TOC.
fn decoded(packet: &Packet) -> u64 {
    u64::from(packet_frames(&packet.data).expect("a well-formed Opus packet"))
}

/// Read the packets to the end of the stream, and return them.
fn packets(reader: &mut OggReader<'_>) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().expect("a packet") {
        packets.push(packet);
    }
    packets
}

/// The frames the packets present after their trims, checking that each packet's trims and
/// duration add up to the frames it decodes to, and that the presented frames are contiguous.
fn presented(packets: &[Packet]) -> u64 {
    let mut end = None;
    let mut frames = 0;

    for packet in packets {
        let decoded = decoded(packet);
        assert_eq!(packet.block_dur().get(), decoded, "{packet:?}: the trims and the duration");

        // A packet trimmed whole presents nothing.
        if packet.dur.get() == 0 {
            continue;
        }

        // The timebase is 1 / 48 kHz, so the first presented frame is at the PTS plus the trim.
        let start = packet.pts.get() + packet.trim_start.get() as i64;
        if let Some(end) = end {
            assert_eq!(start, end, "{packet:?}: contiguous with the packet before");
        }
        end = Some(start + packet.dur.get() as i64);

        frames += packet.dur.get();
    }

    frames
}

/// The first frame the packets present.
fn first_presented(packets: &[Packet]) -> i64 {
    let first = packets.iter().find(|p| p.dur.get() > 0).expect("a packet that presents frames");
    first.pts.get() + first.trim_start.get() as i64
}

/// Check that the packets read after a seek are the last packets of a linear read of the stream,
/// with the same data, timestamps and trims.
fn assert_tail_of(after: &[Packet], linear: &[Packet], context: &str) {
    assert!(after.len() <= linear.len(), "{context}: more packets than the stream");

    for (seeked, read) in after.iter().zip(&linear[linear.len() - after.len()..]) {
        assert_eq!(
            (seeked.pts, seeked.dur, seeked.trim_start, seeked.trim_end),
            (read.pts, read.dur, read.trim_start, read.trim_end),
            "{context}: {seeked:?} where a linear read has {read:?}"
        );
        assert_eq!(seeked.data, read.data, "{context}: the data of {seeked:?}");
    }
}

/// Patch the pre-skip of the identification header of a stream's first page.
fn patch_pre_skip(data: &mut [u8], pre_skip: u16) {
    let [lo, hi] = pre_skip.to_le_bytes();
    patch_first_page(data, HEAD + 10, lo);
    patch_first_page(data, HEAD + 11, hi);
}

#[test]
fn a_multi_page_stream_presents_exactly_its_length() {
    let mut reader = open(fixture("opus_stereo.opus")).unwrap();

    let track = reader.tracks()[0].clone();
    assert_eq!(track.start_ts, Timestamp::new(0), "the track starts at its first presented frame");
    assert_eq!(track.delay, Some(312));
    assert_eq!(track.padding, Some(648));
    assert_eq!(track.num_frames, Some(48_000));

    let packets = packets(&mut reader);
    assert_eq!(presented(&packets), 48_000);

    // The pre-skip is trimmed from the first packet, which starts that far before 0.
    assert_eq!(packets[0].pts, Timestamp::new(-312));
    assert_eq!(packets[0].trim_start.get(), 312);
    assert!(packets[1..].iter().all(|p| p.trim_start.get() == 0));

    // The padding is trimmed from the last packet.
    let last = packets.last().unwrap();
    assert_eq!(last.trim_end.get(), 648);
    assert!(packets[..packets.len() - 1].iter().all(|p| p.trim_end.get() == 0));
}

#[test]
fn a_single_page_stream_presents_exactly_its_length() {
    let mut reader = open(fixture("opus_short.opus")).unwrap();

    assert_eq!(reader.tracks()[0].start_ts, Timestamp::new(0));
    assert_eq!(reader.tracks()[0].num_frames, Some(12_000));

    let packets = packets(&mut reader);
    assert_eq!(presented(&packets), 12_000);
    assert_eq!(packets[0].pts, Timestamp::new(-312));
    assert_eq!(packets[0].trim_start.get(), 312);
}

#[test]
fn a_seek_to_the_start_trims_the_pre_skip_again() {
    for (name, length) in [("opus_stereo.opus", 48_000), ("opus_short.opus", 12_000)] {
        let mut reader = open(fixture(name)).unwrap();
        assert_eq!(presented(&packets(&mut reader)), length, "{name}");

        let seeked =
            reader.seek(SeekMode::Accurate, SeekTo::Time { time: Time::ZERO, track_id: None });
        let seeked = seeked.unwrap();
        assert_eq!(seeked.required_ts, Timestamp::new(0), "{name}");
        assert_eq!(seeked.actual_ts, Timestamp::new(-312), "{name}");

        let packets = packets(&mut reader);
        assert_eq!(packets[0].trim_start.get(), 312, "{name}");
        assert_eq!(presented(&packets), length, "{name}");
    }
}

#[test]
fn a_seek_into_the_stream_takes_the_pre_roll_and_trims_nothing_at_the_start() {
    let mut reader = open(fixture("opus_stereo.opus")).unwrap();
    let track_id = reader.tracks()[0].id;

    let to = SeekTo::Timestamp { ts: Timestamp::new(24_000), track_id };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

    // Decoding begins at least 80 ms (3840 frames) before the required timestamp.
    assert_eq!(seeked.required_ts, Timestamp::new(24_000));
    assert!(seeked.actual_ts <= Timestamp::new(24_000 - 3840), "{seeked:?}");

    let packets = packets(&mut reader);
    assert_eq!(packets[0].pts, seeked.actual_ts);
    assert_eq!(packets[0].trim_start.get(), 0, "no pre-skip in the middle of the stream");
    assert_eq!(presented(&packets), 48_000 - seeked.actual_ts.get() as u64);
}

#[test]
fn a_seek_near_the_start_takes_the_whole_pre_roll() {
    // The pre-skip trimmed from the first packet is decoded all the same, so a pre-roll that begins
    // inside it begins at that packet.
    let mut reader = open(fixture("opus_stereo.opus")).unwrap();
    let track_id = reader.tracks()[0].id;
    let linear = packets(&mut reader);
    let start = linear[0].pts.get();

    for ts in (0..=9600).step_by(120) {
        let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

        // Decoding begins at least 80 ms before the required timestamp, or at the first packet.
        assert!(seeked.actual_ts.get() <= (ts - PRE_ROLL).max(start), "{ts}: {seeked:?}");
        assert_tail_of(&packets(&mut reader), &linear, &format!("a seek to {ts}"));
    }
}

#[test]
fn a_seek_into_the_last_page_trims_its_padding() {
    // The stream is large enough for a seek to bisect it, and its last page is one the bisection
    // probes. That page's packets include the end padding, so its start is not its end less their
    // duration: it is the end of the page before.
    let data = fixture("opus_big_pages.opus");
    assert!(data.len() > 2 * OGG_PAGE_MAX_SIZE, "a seek bisects the stream");

    let mut reader = open(data).unwrap();
    let track_id = reader.tracks()[0].id;
    assert_eq!(reader.tracks()[0].num_frames, Some(105_600));

    let linear = packets(&mut reader);
    assert_eq!(presented(&linear), 105_600);
    assert_eq!(linear.last().unwrap().trim_end.get(), 648);

    // The last page holds about the last 0.5 s. Seek to every 10 ms of the last second.
    for ts in (105_600 - 48_000..=105_600).step_by(480) {
        let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
        assert!(seeked.actual_ts.get() <= ts - PRE_ROLL, "{ts}: {seeked:?}");

        let after = packets(&mut reader);
        assert_eq!(after[0].pts, seeked.actual_ts, "{ts}");
        assert_tail_of(&after, &linear, &format!("a seek to {ts}"));
    }
}

#[test]
fn a_pre_skip_longer_than_the_first_page_is_the_delay_and_is_trimmed_whole() {
    // FFmpeg writes a pre-skip shorter than a packet. Patch in the pre-skip RFC 7845 (§4.5)
    // recommends for a cropped stream, 80 ms, and the largest, both longer than the stream's first
    // page (one 20 ms packet).
    for pre_skip in [3840, u16::MAX] {
        let mut data = fixture("opus_small_pages.opus");
        patch_pre_skip(&mut data, pre_skip);

        // The last granule position counts the 2.1 s and FFmpeg's pre-skip of 312 frames.
        let length = 100_800 + 312 - u64::from(pre_skip);

        let mut reader = open(data).unwrap();

        let track = reader.tracks()[0].clone();
        assert_eq!(track.start_ts, Timestamp::new(0), "{pre_skip}");
        assert_eq!(track.delay, Some(u32::from(pre_skip)), "{pre_skip}");
        assert_eq!(track.num_frames, Some(length), "{pre_skip}");

        let packets = packets(&mut reader);
        assert_eq!(presented(&packets), length, "{pre_skip}");
        assert_eq!(first_presented(&packets), 0, "{pre_skip}");

        let trimmed: u64 = packets.iter().map(|p| p.trim_start.get()).sum();
        assert_eq!(trimmed, u64::from(pre_skip), "{pre_skip}");
    }
}

#[test]
fn a_seek_landing_inside_a_long_pre_skip_trims_it() {
    // With the largest pre-skip, the first ~89 KB of the stream are pre-skip, so the bisection of a
    // seek near the start lands on a page inside it past the first page.
    let mut data = fixture("opus_small_pages.opus");
    assert!(data.len() > 2 * OGG_PAGE_MAX_SIZE, "a seek bisects the stream");
    patch_pre_skip(&mut data, u16::MAX);

    let mut reader = open(data).unwrap();
    let track_id = reader.tracks()[0].id;
    let linear = packets(&mut reader);
    let length = presented(&linear);

    for ts in (0..=4800).step_by(480) {
        let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
        assert!(seeked.actual_ts.get() <= ts - PRE_ROLL, "{ts}: {seeked:?}");

        let after = packets(&mut reader);
        assert_tail_of(&after, &linear, &format!("a seek to {ts}"));

        if ts == 0 {
            assert_eq!(presented(&after), length, "a seek to the start presents the whole stream");
        }
    }
}

#[test]
fn ambisonic_and_discrete_channel_mapping_families_are_carried() {
    for (name, channels, family) in [
        ("opus_foa.opus", Channels::Ambisonic(1), 2),
        ("opus_discrete.opus", Channels::Discrete(3), 255),
    ] {
        let data = fixture(name);
        let mut reader = open(data).unwrap();

        let Some(CodecParameters::Audio(params)) = &reader.tracks()[0].codec_params
        else {
            panic!("{name}: an audio track");
        };
        assert_eq!(params.channels, Some(channels), "{name}");

        // The identification header reaches the decoder whole, the mapping table included.
        let head = params.extra_data.as_deref().expect("the identification header");
        assert_eq!(&head[..8], b"OpusHead", "{name}");
        assert_eq!(head[18], family, "{name}");
        assert_eq!(head.len(), 21 + usize::from(head[9]), "{name}: the mapping table");

        assert_eq!(presented(&packets(&mut reader)), 48_000, "{name}");
    }
}

/// Replace byte `at` of the first page of an Ogg stream, and update the page's checksum.
fn patch_first_page(data: &mut [u8], at: usize, value: u8) {
    assert_eq!(&data[..4], b"OggS");
    let len = 27 + usize::from(data[26]);
    let len = len + data[27..len].iter().map(|&s| usize::from(s)).sum::<usize>();

    data[at] = value;
    data[22..26].fill(0);

    let mut crc = Crc32::new(0);
    crc.process_buf_bytes(&data[..len]);
    data[22..26].copy_from_slice(&crc.crc().to_le_bytes());
}

#[test]
fn a_malformed_identification_header_is_refused_by_name() {
    // A version past 15.
    let mut data = fixture("opus_short.opus");
    patch_first_page(&mut data, HEAD + 8, 16);
    let err = open(data).err().expect("refused");
    assert!(matches!(err, Error::DecodeError("common (opus): invalid version")), "{err:?}");

    // Channel mapping family 0 of 3 channels.
    let mut data = fixture("opus_short.opus");
    patch_first_page(&mut data, HEAD + 9, 3);
    let err = open(data).err().expect("refused");
    assert!(
        matches!(
            err,
            Error::DecodeError("common (opus): invalid channel count for mapping family 0")
        ),
        "{err:?}"
    );
}
