// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opus in Matroska: the `CodecDelay` and the `DiscardPadding` are trimmed by the reader in frames
//! at 48 kHz, the duration of each packet is read from its TOC, and a seek takes the `SeekPreRoll`
//! and trims the codec delay only at the start of the track. The fixtures are made by
//! `fixtures/regenerate.sh`: each Opus track is 1 s long, starts at 0 unless stated otherwise, and
//! states a `CodecDelay` of 6.5 ms (312 frames), an 80 ms `SeekPreRoll`, and a `DiscardPadding` of
//! 13.5 ms (648 frames) on its last block. The timebase of each track is 1 ms. The files of a single
//! laced block, for its `DiscardPadding` and its frames' durations, are built in the test
//! (`laced_block`).

use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use symphonia_common::xiph::audio::opus::packet_frames;
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_mkv::MkvReader;

const FIXTURES: [&str; 3] = ["opus_stereo.mka", "opus_51.mka", "opus_laced.mka"];

/// The laced fixture: 5 frames to a block in one cluster, with a cue point without
/// CueRelativePosition.
const LACED: &str = "opus_laced.mka";

/// Video from 0 to 2 s and the Opus track from 1 s, in five clusters: the track's first block is in
/// the second cluster, which begins at 600 ms.
const LATE_AUDIO: &str = "opus_late_audio.mkv";

fn open(name: &str) -> MkvReader<'static> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let file = File::open(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    MkvReader::try_new(mss, Default::default()).expect("the reader opens")
}

/// Read the packets to the end of the stream, and return them.
fn packets(reader: &mut MkvReader<'_>) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().expect("a packet") {
        packets.push(packet);
    }
    packets
}

/// The Opus track of a file with video.
fn opus_track(reader: &MkvReader<'_>) -> u32 {
    let track = reader
        .tracks()
        .iter()
        .find(|track| track.codec_params.as_ref().is_some_and(|params| params.audio().is_some()));
    track.expect("an Opus track").id
}

/// Read the packets of the track `track_id` to the end of the stream, and return them.
fn track_packets(reader: &mut MkvReader<'_>, track_id: u32) -> Vec<Packet> {
    packets(reader).into_iter().filter(|packet| packet.track_id == track_id).collect()
}

/// Read the packets up to the first at or after `ms`, all of them before the track `track_id`
/// starts.
fn read_before_track(reader: &mut MkvReader<'_>, track_id: u32, ms: i64) {
    loop {
        let packet = reader.next_packet().expect("a packet").expect("not the end of the stream");
        assert_ne!(packet.track_id, track_id, "the track starts before {ms} ms");
        if packet.pts.get() >= ms {
            break;
        }
    }
}

/// The frames the packets present after their trims: the frames each decodes to, read from its
/// TOC, less its trims.
fn presented(packets: &[Packet]) -> u64 {
    packets
        .iter()
        .map(|packet| {
            let decoded = u64::from(packet_frames(&packet.data).expect("an Opus packet"));
            let trims = packet.trim_start.get() + packet.trim_end.get();
            assert!(trims <= decoded, "{packet:?}: trims more than it decodes");
            decoded - trims
        })
        .sum()
}

/// The index of the packet that a seek to `ms` lands on: the first that ends at or after the 80 ms
/// pre-roll before `ms`.
fn landing(packets: &[Packet], ms: i64) -> usize {
    // Where a packet ends before its trims: 48 of its decoded frames at 48 kHz are a millisecond.
    let end = |packet: &Packet| {
        let decoded = i64::from(packet_frames(&packet.data).expect("an Opus packet"));
        packet.pts.get() + decoded / 48
    };
    packets.iter().position(|packet| end(packet) >= ms - 80).expect("a packet holds the time")
}

/// Assert that the packets are those expected: the same tracks, timestamps, durations, trims and
/// data.
fn assert_same(packets: &[Packet], expected: &[Packet], what: &str) {
    let fields = |p: &Packet| (p.track_id, p.pts, p.dur, p.trim_start, p.trim_end);
    assert_eq!(
        packets.iter().map(fields).collect::<Vec<_>>(),
        expected.iter().map(fields).collect::<Vec<_>>(),
        "{what}"
    );
    assert!(packets.iter().zip(expected).all(|(p, e)| p.data == e.data), "{what}: other data");
}

#[test]
fn a_track_presents_exactly_its_length() {
    for name in FIXTURES {
        let mut reader = open(name);
        let packets = packets(&mut reader);

        assert_eq!(presented(&packets), 48_000, "{name}");

        // The codec delay is trimmed from the first packet, which starts 7 ms before 0 (6.5 ms
        // rounded to the nearest millisecond, as FFmpeg's muxer adds it to the block timestamps
        // and its demuxer subtracts it), and only from it.
        assert_eq!(packets[0].pts, Timestamp::new(-7), "{name}");
        assert_eq!(packets[0].trim_start.get(), 312, "{name}");
        assert!(packets[1..].iter().all(|p| p.trim_start.get() == 0), "{name}");

        // The second packet starts at 13.5 ms, its block stamped 21 ms by the muxer: 14 ms, as
        // FFmpeg's demuxer stamps it. (A laced block's later frames start where the frames
        // before them end, 13 ms.)
        if name != LACED {
            assert_eq!(packets[1].pts, Timestamp::new(14), "{name}");
        }

        // The discard padding is trimmed from the last packet, and only from it.
        let last = packets.last().unwrap();
        assert_eq!(last.trim_end.get(), 648, "{name}");
        assert!(packets[..packets.len() - 1].iter().all(|p| p.trim_end.get() == 0), "{name}");
    }
}

#[test]
fn the_duration_of_a_packet_is_read_from_its_toc() {
    for name in FIXTURES {
        let mut reader = open(name);
        let packets = packets(&mut reader);

        // The blocks state no duration. Each 20 ms packet lasts 20 ms less what is trimmed from it:
        // 13.5 ms after the codec delay, 6.5 ms after the padding (in milliseconds, rounded down).
        assert_eq!(packets[0].dur.get(), 13, "{name}");
        assert!(packets[1..packets.len() - 1].iter().all(|p| p.dur.get() == 20), "{name}");
        assert_eq!(packets.last().unwrap().dur.get(), 6, "{name}");

        // Each packet starts where the packet before it ends, before trimming, to within the
        // millisecond the muxer rounds each block timestamp to.
        for pair in packets.windows(2) {
            let step = pair[1].pts.get() - pair[0].pts.get();
            assert!((19..=21).contains(&step), "{name}: {:?} after {:?}", pair[1], pair[0]);
        }
    }
}

#[test]
fn a_seek_to_the_start_trims_the_codec_delay_again() {
    for name in FIXTURES {
        let mut reader = open(name);
        assert_eq!(presented(&packets(&mut reader)), 48_000, "{name}");

        let to = SeekTo::Time { time: Time::ZERO, track_id: None };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
        assert_eq!(seeked.required_ts, Timestamp::new(0), "{name}");
        assert_eq!(seeked.actual_ts, Timestamp::new(-7), "{name}");

        let packets = packets(&mut reader);
        assert_eq!(packets[0].trim_start.get(), 312, "{name}");
        assert_eq!(presented(&packets), 48_000, "{name}");
    }
}

#[test]
fn a_seek_before_reading_trims_the_codec_delay_at_the_start() {
    let mut reader = open("opus_stereo.mka");

    let to = SeekTo::Time { time: Time::ZERO, track_id: None };
    reader.seek(SeekMode::Accurate, to).unwrap();

    let packets = packets(&mut reader);
    assert_eq!(packets[0].trim_start.get(), 312);
    assert_eq!(presented(&packets), 48_000);
}

#[test]
fn a_seek_into_the_track_takes_the_pre_roll_and_trims_nothing_at_the_start() {
    for name in FIXTURES {
        let mut reader = open(name);
        let track_id = reader.tracks()[0].id;

        // Read past the start, so the start of the track is known.
        reader.next_packet().unwrap();

        let to = SeekTo::Timestamp { ts: Timestamp::new(500), track_id };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

        // Decoding begins at least 80 ms before the required timestamp.
        assert_eq!(seeked.required_ts, Timestamp::new(500), "{name}");
        assert!(seeked.actual_ts <= Timestamp::new(420), "{name}: {seeked:?}");

        let packets = packets(&mut reader);
        assert_eq!(packets[0].pts, seeked.actual_ts, "{name}");
        assert_eq!(packets[0].trim_start.get(), 0, "{name}: no codec delay in the middle");

        // The packets present from the start of the first packet's block to the end of the track,
        // 20 ms blocks, less the codec delay before the first.
        let blocks = (48_000 + 312) / 960 + 1;
        assert_eq!(
            presented(&packets),
            48_000 - (blocks - packets.len() as u64) * 960 + 312,
            "{name}"
        );
    }
}

#[test]
fn a_seek_by_the_cues_drops_the_rest_of_a_laced_block() {
    let from_open = packets(&mut open(LACED));

    // Once the first frames of a laced block are read, the rest of the block is still to be read. A
    // seek back to the start or into the track, or forward into the rest of the block, lands as it
    // would from anywhere else, the codec delay trimmed at the start.
    for (read, ms) in [(1, 0), (28, 300), (26, 560)] {
        let mut reader = open(LACED);
        for _ in 0..read {
            reader.next_packet().unwrap();
        }

        let to = SeekTo::Time { time: Time::from_millis(ms), track_id: None };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

        let at = landing(&from_open, ms);
        let what = format!("{read} packets read, then a seek to {ms} ms");
        assert_eq!(seeked.actual_ts, from_open[at].pts, "{what}");
        assert_same(&packets(&mut reader), &from_open[at..], &what);
    }
}

#[test]
fn a_failed_seek_keeps_the_rest_of_a_laced_block() {
    let from_open = packets(&mut open(LACED));

    let mut reader = open(LACED);
    for _ in 0..26 {
        reader.next_packet().unwrap();
    }

    // A seek past the end fails, and the reader reads on from where it was, in a laced block.
    let to = SeekTo::Time { time: Time::from_millis(10_000), track_id: None };
    let seeked = reader.seek(SeekMode::Accurate, to);
    assert!(matches!(seeked, Err(Error::SeekError(SeekErrorKind::OutOfRange))), "{seeked:?}");
    assert_same(&packets(&mut reader), &from_open[26..], "the packets after the failed seek");
}

#[test]
fn a_seek_before_reading_trims_the_codec_delay_of_a_track_that_starts_late() {
    let mut reader = open(LATE_AUDIO);
    let opus = opus_track(&reader);
    let from_open = track_packets(&mut reader, opus);

    // The track starts 1 s after the video (its first block 1001 ms, less the codec delay).
    assert_eq!(from_open[0].pts, Timestamp::new(994));
    assert_eq!(from_open[0].trim_start.get(), 312);
    assert_eq!(presented(&from_open), 48_000);

    // A seek to the start before anything was read, on the video track or on the Opus track, reads
    // the Opus track's first frame and trims its codec delay.
    for track_id in [None, Some(opus)] {
        let mut reader = open(LATE_AUDIO);
        let to = SeekTo::Time { time: Time::ZERO, track_id };
        reader.seek(SeekMode::Accurate, to).unwrap();

        let what = format!("a seek to the start on {track_id:?}");
        assert_same(&track_packets(&mut reader, opus), &from_open, &what);
    }
}

#[test]
fn a_seek_to_a_late_track_after_reading_up_to_its_cluster_trims_its_codec_delay() {
    let mut reader = open(LATE_AUDIO);
    let opus = opus_track(&reader);
    let from_open = track_packets(&mut reader, opus);

    // Read into the second cluster, before the Opus track's first block.
    let mut reader = open(LATE_AUDIO);
    read_before_track(&mut reader, opus, 600);

    // A seek to the start of the Opus track, into what follows what was read, reads the track's
    // first frame and trims its codec delay.
    let to = SeekTo::Time { time: Time::from_millis(1000), track_id: Some(opus) };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

    assert_eq!(seeked.actual_ts, from_open[0].pts);
    assert_same(&track_packets(&mut reader, opus), &from_open, "a seek to 1 s");
}

#[test]
fn a_seek_back_from_past_what_was_read_trims_by_where_the_track_starts() {
    let mut reader = open(LATE_AUDIO);
    let opus = opus_track(&reader);
    let from_open = track_packets(&mut reader, opus);

    // Read into the second cluster, before the Opus track's first block, then seek past everything
    // read, to the fourth cluster, and read a packet of the track there.
    let mut reader = open(LATE_AUDIO);
    read_before_track(&mut reader, opus, 600);

    let to = SeekTo::Time { time: Time::from_millis(1500), track_id: Some(opus) };
    reader.seek(SeekMode::Accurate, to).unwrap();
    while reader.next_packet().unwrap().expect("a packet").track_id != opus {}

    // A seek back to 100 ms after the track's start takes the pre-roll and trims nothing...
    let to = SeekTo::Time { time: Time::from_millis(1100), track_id: Some(opus) };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

    let at = landing(&from_open, 1100);
    assert_eq!(seeked.actual_ts, from_open[at].pts);
    assert_same(&track_packets(&mut reader, opus), &from_open[at..], "a seek to 1.1 s");

    // ...and a seek to the start trims the codec delay.
    let to = SeekTo::Time { time: Time::ZERO, track_id: Some(opus) };
    reader.seek(SeekMode::Accurate, to).unwrap();
    assert_same(&track_packets(&mut reader, opus), &from_open, "a seek to the start");
}

#[test]
fn a_seek_before_the_first_cue_point_reads_from_the_first_cluster() {
    let mut reader = open("opus_offset.mka");
    let from_open = packets(&mut reader);

    // The track is stored from 100 ms, where its only cue point is.
    assert_eq!(from_open[0].pts, Timestamp::new(93));
    assert_eq!(from_open[0].trim_start.get(), 312);
    assert_eq!(presented(&from_open), 48_000);

    // A seek to the start, or to within the pre-roll after the cue point, lands on the first frame,
    // before anything was read and after reading to the end.
    for ms in [0, 120] {
        for read_first in [false, true] {
            let mut reader = open("opus_offset.mka");
            if read_first {
                packets(&mut reader);
            }

            let to = SeekTo::Time { time: Time::from_millis(ms), track_id: None };
            let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

            let what = format!("a seek to {ms} ms, the track read first: {read_first}");
            assert_eq!(seeked.actual_ts, from_open[0].pts, "{what}");
            assert_same(&packets(&mut reader), &from_open, &what);
        }
    }
}

#[test]
fn a_seek_before_the_first_cue_point_of_another_track_takes_the_pre_roll() {
    let mut reader = open("opus_late_video.mkv");
    let opus = opus_track(&reader);
    let from_open = track_packets(&mut reader, opus);
    assert_eq!(presented(&from_open), 48_000);

    // The video, which the cue points index, starts in the second cluster, 0.6 s into the Opus
    // track. A seek on the Opus track before then lands on the frame the pre-roll begins in.
    for ms in [0, 100, 500] {
        let mut reader = open("opus_late_video.mkv");
        let to = SeekTo::Time { time: Time::from_millis(ms), track_id: Some(opus) };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

        let at = landing(&from_open, ms);
        let what = format!("a seek to {ms} ms");
        assert_eq!(seeked.actual_ts, from_open[at].pts, "{what}");
        assert_same(&track_packets(&mut reader, opus), &from_open[at..], &what);
    }
}

/// An EBML element: its ID, its size (an 8-byte variable-size integer) and its data.
fn element(id: u32, data: &[u8]) -> Vec<u8> {
    let mut element: Vec<u8> = id.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
    element.push(0x01);
    element.extend_from_slice(&(data.len() as u64).to_be_bytes()[1..]);
    element.extend_from_slice(data);
    element
}

/// A reader of a stereo Opus track with no codec delay and one block, a BlockGroup of `frames`
/// Xiph-laced packets of one frame each, `toc` their TOC byte, with a `DiscardPadding` of
/// `discard_padding` nanoseconds. FFmpeg's muxer never laces, so the file is built here. Each
/// packet is only its TOC byte, all the reader reads of it.
fn laced_block(toc: u8, frames: usize, discard_padding: i64) -> MkvReader<'static> {
    // An OpusHead: version 1, 2 channels, no pre-skip, 48 kHz, no gain, channel mapping family 0.
    let head = [b"OpusHead".as_slice(), &[1, 2, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]].concat();
    let audio = [element(0xb5, &48_000f64.to_be_bytes()), element(0x9f, &[2])].concat();
    let track = [
        element(0xd7, &[1]),
        element(0x73c5, &[1]),
        element(0x83, &[2]),
        element(0x86, b"A_OPUS"),
        element(0x63a2, &head),
        element(0xe1, &audio),
    ]
    .concat();

    // Track 1 at the cluster's timestamp, Xiph-laced: the sizes of the frames but the last, then
    // the frames, each its TOC byte.
    let mut block = vec![0x81, 0, 0, 0x02, frames as u8 - 1];
    block.extend(std::iter::repeat_n(1, frames - 1));
    block.extend(std::iter::repeat_n(toc, frames));

    let group = [element(0xa1, &block), element(0x75a2, &discard_padding.to_be_bytes())].concat();
    let cluster = [element(0xe7, &[0]), element(0xa0, &group)].concat();
    let info = [
        element(0x2ad7b1, &1_000_000u32.to_be_bytes()),
        element(0x4d80, b"t"),
        element(0x5741, b"t"),
    ];
    let segment = [
        element(0x1549a966, &info.concat()),
        element(0x1654ae6b, &element(0xae, &track)),
        element(0x1f43b675, &cluster),
    ]
    .concat();
    let header = [element(0x4282, b"matroska"), element(0x4287, &[4]), element(0x4285, &[2])];
    let file = [element(0x1a45dfa3, &header.concat()), element(0x18538067, &segment)].concat();

    let mss = MediaSourceStream::new(Box::new(Cursor::new(file)), Default::default());
    MkvReader::try_new(mss, Default::default()).expect("the reader opens")
}

/// The TOC byte of a 20 ms CELT fullband stereo packet of one frame (configuration 31, RFC 6716
/// §3.1).
const CELT_FB_20_MS: u8 = 0xfc;

/// The TOC byte of a 2.5 ms CELT fullband stereo packet of one frame (configuration 28).
const CELT_FB_2_5_MS: u8 = 0xe4;

#[test]
fn a_laced_blocks_discard_padding_is_trimmed_from_as_many_frames_as_it_spans() {
    let trims = |frames, discard_padding| {
        let packets = packets(&mut laced_block(CELT_FB_20_MS, frames, discard_padding));
        packets.iter().map(|p| (p.trim_start.get(), p.trim_end.get())).collect::<Vec<_>>()
    };

    // A positive padding is trimmed from the end of the block: from its last frame (13.5 ms, 648
    // frames), and on from the frames before it (33.5 ms, 1608 frames, over two; 53.5 ms, 2568
    // frames, over three).
    assert_eq!(trims(2, 13_500_000), [(0, 0), (0, 648)]);
    assert_eq!(trims(2, 33_500_000), [(0, 648), (0, 960)]);
    assert_eq!(trims(3, 53_500_000), [(0, 648), (0, 960), (0, 960)]);

    // A negative padding is trimmed from the start of the block: from its first frame (5 ms, 240
    // frames), and on from the frames after it (25 ms, 1200 frames, over two).
    assert_eq!(trims(2, -5_000_000), [(240, 0), (0, 0)]);
    assert_eq!(trims(2, -25_000_000), [(960, 0), (240, 0)]);

    // A padding as long as the block, or longer, trims all of it.
    assert_eq!(trims(2, 40_000_000), [(0, 960), (0, 960)]);
    assert_eq!(trims(2, -50_000_000), [(960, 0), (960, 0)]);
}

#[test]
fn each_frame_of_a_laced_block_ends_where_the_next_begins() {
    let timing = |discard_padding| {
        let packets = packets(&mut laced_block(CELT_FB_2_5_MS, 4, discard_padding));
        packets.iter().map(|p| (p.pts.get(), p.dur.get())).collect::<Vec<_>>()
    };

    // Four 2.5 ms frames in 1 ms ticks: each starts where the frames before it end, rounded down
    // (0, 2.5, 5 and 7.5 ms), and lasts to where the next starts, 2 and 3 ticks in turn, so the
    // block lasts its 10 ms.
    assert_eq!(timing(0), [(0, 2), (2, 3), (5, 2), (7, 3)]);

    // A trimmed frame lasts the frames it presents, rounded down: 1.5 ms of the last after a
    // padding of 1 ms (48 frames).
    assert_eq!(timing(1_000_000), [(0, 2), (2, 3), (5, 2), (7, 1)]);
}
