// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opus in MP4: the `dOps` box is handed to the decoder as the identification header, the encoder
//! delay and the padding are trimmed as the edit list states, and a seek takes the 80 ms of pre-roll
//! an Opus decoder needs, or the longer pre-roll the roll recovery group states. The fixtures are
//! made by `fixtures/regenerate.sh`: FFmpeg's MP4 muxer states the 312-frame encoder delay as the
//! edit's media time, 1 s as its duration, and a pre-roll of 4 packets (80 ms) for every packet but
//! the first 4. In a fragmented file, it states neither.

use std::io::Cursor;
use std::path::Path;

use symphonia_common::xiph::audio::opus::packet_frames;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo, Track};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_isomp4::IsoMp4Reader;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn open(data: Vec<u8>) -> IsoMp4Reader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    IsoMp4Reader::try_new(mss, Default::default()).expect("the reader opens")
}

/// The position of the body of the first box of type `kind` in `data`, after its type.
fn body(data: &[u8], kind: &[u8; 4]) -> usize {
    data.windows(4).position(|w| w == kind).unwrap_or_else(|| panic!("a {kind:?} box")) + 4
}

fn get(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

fn put(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// The position of the roll distance of the one entry of the roll recovery group description of
/// opus_mono.mp4: a version 1 `sgpd` of 2-byte entries.
fn roll_distance(data: &[u8]) -> usize {
    let at = body(data, b"sgpd");
    assert_eq!((data[at], &data[at + 4..at + 8]), (1, b"roll".as_slice()));
    assert_eq!((get(data, at + 8), get(data, at + 12)), (2, 1), "one entry of 2 bytes");
    at + 16
}

/// Seek the track `track_id` to the timestamp `ts`, and return the timestamp of the packet decoding
/// begins at.
fn seek(reader: &mut IsoMp4Reader<'_>, track_id: u32, ts: i64) -> Timestamp {
    let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id };
    let seeked = reader.seek(SeekMode::Accurate, to).expect("a seek");
    assert_eq!(seeked.required_ts, Timestamp::new(ts));
    seeked.actual_ts
}

/// The audio track.
fn audio_track(reader: &IsoMp4Reader<'_>) -> Track {
    let track =
        reader.tracks().iter().find(|t| matches!(t.codec_params, Some(CodecParameters::Audio(_))));
    track.expect("an audio track").clone()
}

/// Read the packets of the track `track_id` to the end of the stream, and return them.
fn packets(reader: &mut IsoMp4Reader<'_>, track_id: u32) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().expect("a packet") {
        if packet.track_id == track_id {
            packets.push(packet);
        }
    }
    packets
}

/// The frames the packets present after their trims, checking that each packet's trims and
/// duration add up to the frames it decodes to (read from its TOC), and that the presented frames
/// are contiguous.
fn presented(packets: &[Packet]) -> u64 {
    let mut end = None;
    let mut frames = 0;

    for packet in packets {
        let decoded = u64::from(packet_frames(&packet.data).expect("an Opus packet"));
        assert_eq!(packet.block_dur().get(), decoded, "{packet:?}: the trims and the duration");

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

#[test]
fn the_identification_header_is_handed_to_the_decoder() {
    let reader = open(fixture("opus_mono.mp4"));
    let track = audio_track(&reader);

    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        unreachable!()
    };

    // RFC 7845, §5.1: version 1, 1 channel, a pre-skip of 312, 48 kHz input, no gain, and channel
    // mapping family 0, its fields little-endian.
    let head = params.extra_data.as_deref().expect("the identification header");
    assert_eq!(head, b"OpusHead\x01\x01\x38\x01\x80\xbb\x00\x00\x00\x00\x00");
}

#[test]
fn every_field_of_a_dops_box_reaches_the_identification_header() {
    // Give the 5.1 track's `dOps` box (big-endian) a pre-skip, input rate, and output gain whose
    // bytes all differ.
    let mut data = fixture("opus_51.mp4");
    let at = data.windows(4).position(|w| w == b"dOps").expect("a dOps box") + 4;
    assert_eq!(data[at], 0, "version 0");
    data[at + 2..at + 4].copy_from_slice(&0x0139u16.to_be_bytes());
    data[at + 4..at + 8].copy_from_slice(&44_100u32.to_be_bytes());
    data[at + 8..at + 10].copy_from_slice(&(-0x0180i16).to_be_bytes());
    let table = data[at + 11..at + 19].to_vec();

    let reader = open(data);
    let track = audio_track(&reader);

    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        unreachable!()
    };
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(6));

    let mut expected = b"OpusHead\x01\x06".to_vec();
    expected.extend_from_slice(&0x0139u16.to_le_bytes());
    expected.extend_from_slice(&44_100u32.to_le_bytes());
    expected.extend_from_slice(&(-0x0180i16).to_le_bytes());
    // Family 1, four streams, two coupled, and the mapping of each of the six channels.
    expected.push(1);
    expected.extend_from_slice(&table);
    assert_eq!(&table[..2], &[4, 2]);

    assert_eq!(params.extra_data.as_deref(), Some(expected.as_slice()));
}

#[test]
fn a_track_presents_exactly_the_span_its_edit_list_states() {
    for name in ["opus_mono.mp4", "opus_51.mp4"] {
        let mut reader = open(fixture(name));
        let track = audio_track(&reader);

        assert_eq!(track.start_ts, Timestamp::new(0), "{name}");
        assert_eq!(track.delay, Some(312), "{name}");
        assert_eq!(track.num_frames, Some(48_000), "{name}");
        assert_eq!(track.duration.map(|d| d.get()), Some(48_000), "{name}");

        let packets = packets(&mut reader, track.id);
        assert_eq!(presented(&packets), 48_000, "{name}");

        // The encoder delay is trimmed from the first packet, and the padding past the edit from
        // the last, whose sample lasts only its 312 valid frames but decodes to 960.
        assert_eq!(packets[0].pts, Timestamp::new(-312), "{name}");
        assert_eq!(packets[0].trim_start.get(), 312, "{name}");
        assert_eq!(packets.last().unwrap().trim_end.get(), 648, "{name}");
    }
}

#[test]
fn an_edit_past_the_last_sample_ends_with_the_last_sample() {
    // An edit's duration is in the movie timescale (1 kHz here), which may be coarser than the
    // media's: an edit rounded up to it, or one spanning a longer movie, ends after the last
    // sample, whose duration is its 312 valid frames.
    for duration in [1001u32, 2000] {
        let mut data = fixture("opus_mono.mp4");
        let at = data.windows(4).position(|w| w == b"elst").expect("an elst box") + 4;
        assert_eq!(data[at], 0, "version 0");
        assert_eq!(data[at + 8..at + 12], 1000u32.to_be_bytes(), "an edit of 1 s");
        data[at + 8..at + 12].copy_from_slice(&duration.to_be_bytes());

        let mut reader = open(data);
        let track = audio_track(&reader);
        assert_eq!(track.num_frames, Some(48_000), "{duration}");
        assert_eq!(track.duration.map(|d| d.get()), Some(48_000), "{duration}");

        let packets = packets(&mut reader, track.id);
        assert_eq!(presented(&packets), 48_000, "{duration}");
        assert_eq!(packets.last().unwrap().trim_end.get(), 648, "{duration}");
    }
}

#[test]
fn without_an_edit_list_the_last_packet_presents_its_duration() {
    let mut data = fixture("opus_mono.mp4");
    let at = data.windows(4).position(|w| w == b"edts").expect("an edts box");
    data[at..at + 4].copy_from_slice(b"free");

    // The media's timeline is presented, from 0 to the end of the last sample, which decodes to 960
    // frames but lasts only its 312 valid frames.
    let mut reader = open(data);
    let track_id = audio_track(&reader).id;
    let packets = packets(&mut reader, track_id);
    assert_eq!((packets[0].pts, packets[0].trim_start.get()), (Timestamp::new(0), 0));
    let last = packets.last().unwrap();
    assert_eq!((last.trim_end.get(), last.dur.get()), (648, 312));
    assert_eq!(presented(&packets), 48_312);
}

#[test]
fn a_seek_to_the_start_trims_the_encoder_delay_again() {
    let mut reader = open(fixture("opus_mono.mp4"));
    let track_id = audio_track(&reader).id;
    assert_eq!(presented(&packets(&mut reader, track_id)), 48_000);

    let to = SeekTo::Time { time: Time::ZERO, track_id: Some(track_id) };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
    assert_eq!(seeked.required_ts, Timestamp::new(0));
    assert_eq!(seeked.actual_ts, Timestamp::new(-312));

    let packets = packets(&mut reader, track_id);
    assert_eq!(packets[0].trim_start.get(), 312);
    assert_eq!(presented(&packets), 48_000);
}

#[test]
fn a_seek_takes_the_pre_roll_of_the_roll_recovery_group() {
    let mut reader = open(fixture("opus_mono.mp4"));
    let track_id = audio_track(&reader).id;

    // 0.5 s presents media timestamp 24312, in the packet from 24000 (number 25). Its pre-roll is 4
    // packets (80 ms), so decoding begins at the packet from 20160, presented at 19848.
    let to = SeekTo::Timestamp { ts: Timestamp::new(24_000), track_id };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
    assert_eq!(seeked.required_ts, Timestamp::new(24_000));
    assert_eq!(seeked.actual_ts, Timestamp::new(19_848));

    let packets = packets(&mut reader, track_id);
    assert_eq!(packets[0].pts, Timestamp::new(19_848));
    assert_eq!(packets[0].trim_start.get(), 0);
    assert_eq!(presented(&packets), 48_000 - 19_848);

    // A pre-roll the group states longer than 80 ms is taken: 6 packets.
    let mut data = fixture("opus_mono.mp4");
    let at = roll_distance(&data);
    data[at..at + 2].copy_from_slice(&(-6i16).to_be_bytes());

    let mut reader = open(data);
    assert_eq!(seek(&mut reader, track_id, 24_000), Timestamp::new(17_928));
}

#[test]
fn an_opus_seek_begins_at_least_80_ms_before_its_target() {
    // An Opus decoder converges within 80 ms (RFC 7845, §4.6). FFmpeg's roll recovery group puts
    // the first 4 packets, which have less than 80 ms before them, in no group: a seek into the
    // second packet begins at the first.
    let data = fixture("opus_mono.mp4");
    let mut reader = open(data.clone());
    let track_id = audio_track(&reader).id;
    assert_eq!(seek(&mut reader, track_id, 700), Timestamp::new(-312));

    // A roll recovery group stating a shorter pre-roll, or none, doesn't shorten it: 4 packets.
    let mut shorter = data.clone();
    let at = roll_distance(&shorter);
    shorter[at..at + 2].copy_from_slice(&(-2i16).to_be_bytes());

    let mut none = data;
    let at = body(&none, b"sgpd") - 4;
    none[at..at + 4].copy_from_slice(b"free");

    for (case, data) in [("shorter", shorter), ("none", none)] {
        let mut reader = open(data);
        assert_eq!(seek(&mut reader, track_id, 24_000), Timestamp::new(19_848), "{case}");
    }
}

#[test]
fn a_seek_in_a_fragmented_file_takes_80_ms_of_pre_roll() {
    // FFmpeg's fragmented file states neither a roll recovery group nor an edit list, so the media
    // is presented whole. Its fragments hold 25 packets of 20 ms each.
    let mut reader = open(fixture("opus_frag.mp4"));
    let track = audio_track(&reader);
    assert_eq!(track.delay, None);

    // 0.5 s is the first packet of the second fragment, number 25. Decoding begins 80 ms (4
    // packets) before it, in the first fragment, which a reader that has read no fragment yet
    // reads first.
    assert_eq!(seek(&mut reader, track.id, 24_000), Timestamp::new(20_160));

    let packets = packets(&mut reader, track.id);
    assert_eq!(packets.len(), 51 - 21);
    assert_eq!(packets[0].pts, Timestamp::new(20_160));

    // The same once every fragment is read, and within a fragment. Near the start, decoding begins
    // at the first packet.
    assert_eq!(seek(&mut reader, track.id, 24_000), Timestamp::new(20_160));
    assert_eq!(seek(&mut reader, track.id, 36_500), Timestamp::new(32_640));
    assert_eq!(seek(&mut reader, track.id, 1_920), Timestamp::new(0));
}

#[test]
fn a_malformed_roll_recovery_group_is_ignored() {
    // The roll recovery group is a seek hint: a malformed one is dropped and the file opens, as
    // FFmpeg's reader opens it. A seek still takes the 80 ms of pre-roll of an Opus track.
    let src = fixture("opus_mono.mp4");

    // FFmpeg's version 1 `sgpd` of one entry of 2 bytes, and its version 0 `sbgp` of two runs.
    let (sgpd, sbgp) = (body(&src, b"sgpd"), body(&src, b"sbgp"));
    assert_eq!((get(&src, sgpd + 8), get(&src, sgpd + 12)), (2, 1));
    assert_eq!((&src[sbgp..sbgp + 8], get(&src, sbgp + 8)), (b"\0\0\0\0roll".as_slice(), 2));

    let patched = |at, value| {
        let mut data = src.clone();
        put(&mut data, at, value);
        data
    };

    // Over the two (26 and 36 bytes), a version 2 `sgpd` whose default group, a pre-roll of 6
    // packets, describes every sample no run maps, and an `sbgp` stating 2 runs but holding one and
    // a half. The group is dropped whole: without its runs, every sample would take 6 packets.
    let start = sgpd - 8;
    assert_eq!((sbgp - 8, get(&src, sbgp - 8)), (start + 26, 36), "adjacent");

    let mut v2 = src.clone();
    v2[start..start + 30]
        .copy_from_slice(b"\0\0\0\x1esgpd\x02\0\0\0roll\0\0\0\x02\0\0\0\x01\0\0\0\x01\xff\xfa");
    v2[start + 30..start + 62]
        .copy_from_slice(b"\0\0\0\x20sbgp\0\0\0\0roll\0\0\0\x02\0\0\0\x33\0\0\0\x01\0\0\0\x33");

    let cases = [
        ("an entry shorter than its roll distance", patched(sgpd + 8, 1)),
        ("more entries than the sgpd holds", patched(sgpd + 12, 2)),
        ("more runs than the sbgp holds", patched(sbgp + 8, 3)),
        ("a truncated sbgp beside a version 2 sgpd", v2),
    ];

    for (case, data) in cases {
        let mut reader = open(data);
        let track_id = audio_track(&reader).id;
        assert_eq!(packets(&mut reader, track_id).len(), 51, "{case}");
        assert_eq!(seek(&mut reader, track_id, 24_000), Timestamp::new(19_848), "{case}");
    }
}

#[test]
fn the_packets_of_every_track_come_in_presentation_order() {
    // opus_51.mp4's Opus track presents its media 312 frames (6.5 ms) early, beside an H.264 track
    // that presents it as it is: the packets of the two are interleaved by presentation time.
    let mut reader = open(fixture("opus_51.mp4"));
    let time_bases: Vec<_> =
        reader.tracks().iter().map(|t| (t.id, t.time_base.expect("a time base"))).collect();
    assert_eq!(time_bases.len(), 2);

    let mut count = 0;
    let mut last = None;
    while let Some(packet) = reader.next_packet().expect("a packet") {
        let (_, time_base) =
            time_bases.iter().find(|(id, _)| *id == packet.track_id).expect("the packet's track");
        let time = time_base.calc_time(packet.pts).expect("a time");
        let (track_id, pts) = (packet.track_id, packet.pts);
        assert!(last <= Some(time), "track {track_id} at {pts:?}: before the packet before it");
        last = Some(time);
        count += 1;
    }
    assert_eq!(count, 25 + 51);
}
