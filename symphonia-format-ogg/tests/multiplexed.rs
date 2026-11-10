// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A physical stream of several logical streams: a seek on any of them ends, and every stream
//! reads on from it as a linear read does; a seek that names no track seeks the first track with a
//! timebase; every stream that ends with the physical stream states its length and end padding,
//! and a stream that no mapper reads states neither. The fixtures are made by
//! `fixtures/regenerate.sh`.

mod common;

use std::io::Cursor;
use std::path::Path;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_ogg::OggReader;

use common::within_deadline;

/// The largest Ogg page: its header, a full segment table, and 255 full segments. A seek bisects a
/// stream larger than two of them.
const OGG_PAGE_MAX_SIZE: usize = 27 + 255 + 255 * 255;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn open(data: Vec<u8>) -> OggReader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    OggReader::try_new(mss, Default::default()).expect("an Ogg stream")
}

/// Read the packets to the end of the stream, and return them.
fn packets(reader: &mut OggReader<'_>) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().expect("a packet") {
        packets.push(packet);
    }
    packets
}

/// The packets of one track.
fn of_track(packets: &[Packet], track_id: u32) -> Vec<&Packet> {
    packets.iter().filter(|packet| packet.track_id == track_id).collect()
}

/// Check that the packets read after a seek are the last packets of a linear read of the track,
/// with the same data, and the same timestamps and trims after the first: the first packet after a
/// discontinuity may be timed apart (a Vorbis packet's duration laps into the packet before it).
fn assert_tail_of(after: &[&Packet], linear: &[&Packet], context: &str) {
    assert!(after.len() <= linear.len(), "{context}: more packets than the track");

    let tail = &linear[linear.len() - after.len()..];

    for (i, (seeked, read)) in after.iter().zip(tail).enumerate() {
        assert_eq!(seeked.data, read.data, "{context}: the data of packet {i} after the seek");

        if i > 0 {
            assert_eq!(
                (seeked.pts, seeked.dur, seeked.trim_start, seeked.trim_end),
                (read.pts, read.dur, read.trim_start, read.trim_end),
                "{context}: {seeked:?} where a linear read has {read:?}"
            );
        }
    }
}

/// A seek to every 50 ms of each track that has a timebase lands at or before its target, and from
/// there every track reads on as a linear read does: in Opus and Vorbis (the Vorbis stream at 44.1
/// kHz), in Opus and FLAC (at 16 kHz), in two Opus streams (larger than two maximum pages, so the
/// seek bisects them), and in Opus beside a Theora stream that no mapper reads.
#[test]
fn a_seek_on_any_stream_of_a_multiplexed_stream_ends_where_a_linear_read_reads_on() {
    within_deadline(|| {
        for name in ["opus_vorbis.ogg", "opus_flac.ogg", "opus_opus.ogg", "theora_opus.ogg"] {
            let data = fixture(name);
            let bisects = data.len() > 2 * OGG_PAGE_MAX_SIZE;
            assert_eq!(bisects, name == "opus_opus.ogg", "{name}: a seek bisects it");

            let mut reader = open(data);
            let linear = packets(&mut reader);

            let tracks: Vec<_> =
                reader.tracks().iter().filter(|track| track.time_base.is_some()).cloned().collect();
            assert_eq!(tracks.len(), if name == "theora_opus.ogg" { 1 } else { 2 }, "{name}");

            for track in &tracks {
                let rate = track.time_base.unwrap().denom.get();
                let start = track.start_ts.get();
                let end = start + track.num_frames.expect("the track's length") as i64;

                for ts in (start..=end).step_by(rate as usize / 20) {
                    let context = format!("{name}: a seek of track {} to {ts}", track.id);

                    let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: track.id };
                    let seeked = reader.seek(SeekMode::Accurate, to).expect(&context);
                    assert_eq!(seeked.track_id, track.id, "{context}");
                    assert!(seeked.actual_ts.get() <= ts, "{context}: {seeked:?}");

                    let after = packets(&mut reader);
                    let first = after.iter().find(|p| p.track_id == track.id).expect(&context);
                    assert_eq!(first.pts, seeked.actual_ts, "{context}: the first packet");

                    for other in &tracks {
                        let context = format!("{context}, track {}", other.id);
                        assert_tail_of(
                            &of_track(&after, other.id),
                            &of_track(&linear, other.id),
                            &context,
                        );
                    }
                }
            }
        }
    });
}

/// Each stream states its length and its end padding, read from its own last pages among the
/// pages of the others. Every stream here ends with the physical stream, so its last page lies
/// among the physical stream's last pages (the end probe doesn't look for a stream whose last page
/// lies before them). Every Opus stream here is 20 ms packets over whole pages but its last, and
/// the last packet decodes 648 frames past the stream's end; the Vorbis stream ends at 2 s. A
/// stream that no mapper reads states no length, padding or start: nothing times its packets (a
/// Theora granule position counts the keyframes and the frames since the last, not frames).
#[test]
fn each_stream_of_a_multiplexed_stream_states_its_length_and_padding() {
    for (name, lengths) in [
        ("opus_vorbis.ogg", [(0, 96_000), (1, 88_200)].as_slice()),
        ("opus_opus.ogg", &[(0, 100_800), (1, 100_800)]),
        ("theora_opus.ogg", &[(8, 96_000)]),
    ] {
        let reader = open(fixture(name));

        for &(id, length) in lengths {
            let track = reader.tracks().iter().find(|track| track.id == id).unwrap();
            assert_eq!(track.num_frames, Some(length), "{name}: track {id}");
            assert!(track.padding.is_some(), "{name}: track {id}");

            if matches!(&track.codec_params, Some(CodecParameters::Audio(params)) if params.codec == CODEC_ID_OPUS)
            {
                assert_eq!(track.padding, Some(648), "{name}: track {id}");
            }
        }
    }

    let reader = open(fixture("theora_opus.ogg"));
    let theora = reader.tracks().iter().find(|track| track.id == 7).unwrap();
    assert_eq!(
        (theora.num_frames, theora.duration, theora.padding, theora.start_ts),
        (None, None, None, Timestamp::ZERO),
        "theora_opus.ogg: the Theora stream"
    );
}

/// A seek that names no track seeks the first track with a timebase. FFmpeg numbers the serials
/// from 7 here: the Theora stream, which no mapper reads, is the first track. A seek on the Theora
/// stream itself ends too: by time it is refused, and by timestamp it reads on through the Opus
/// stream's pages, out of range: the stream has no packet to land on.
#[test]
fn a_seek_that_names_no_track_seeks_the_first_track_with_a_timebase() {
    within_deadline(|| {
        let mut reader = open(fixture("theora_opus.ogg"));

        let tracks: Vec<_> =
            reader.tracks().iter().map(|track| (track.id, track.time_base.is_some())).collect();
        assert_eq!(tracks, [(7, false), (8, true)], "each track is its stream's serial");

        let linear = packets(&mut reader);

        let to = SeekTo::Time { time: Time::from_millis(1000), track_id: None };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
        assert_eq!(seeked.track_id, 8);
        assert_eq!(seeked.required_ts, Timestamp::new(48_000));
        assert_tail_of(&of_track(&packets(&mut reader), 8), &of_track(&linear, 8), "a seek to 1 s");

        // A stream that no mapper reads cannot be seeked by time: it has no timebase.
        let to = SeekTo::Time { time: Time::from_millis(1000), track_id: Some(7) };
        let err = reader.seek(SeekMode::Accurate, to).unwrap_err();
        assert!(matches!(err, Error::SeekError(SeekErrorKind::Unseekable)), "{err:?}");

        let to = SeekTo::Timestamp { ts: reader.tracks()[0].start_ts, track_id: 7 };
        let err = reader.seek(SeekMode::Accurate, to).unwrap_err();
        assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)), "{err:?}");
    });
}
