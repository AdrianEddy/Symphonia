// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AAC in MP4: the encoder delay and the padding are trimmed as the edit list states, and the AAC
//! decoder applies the trims. The fixtures are made by `fixtures/regenerate.sh`: FFmpeg's MP4 muxer
//! states the 1024-frame encoder delay as the edit's media time and the encoded length as its
//! duration, and shortens the duration of the last sample to its valid frames.

use std::io::Cursor;
use std::path::Path;

use symphonia_codec_aac::AacDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
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

fn double(data: &mut [u8], at: usize) {
    let value = get(data, at);
    put(data, at, 2 * value);
}

/// The position of the 16.16 sample rate of the audio sample entry: after the sample entry's
/// reserved bytes and data reference index, and the audio sample entry's version, revision,
/// vendor, channel count, sample size, compression ID, and packet size.
fn sample_entry_rate(data: &[u8]) -> usize {
    body(data, b"mp4a") + 24
}

/// The position of the duration of the first edit of a version 0 edit list. Its media time
/// follows.
fn edit_duration(data: &[u8]) -> usize {
    let at = body(data, b"elst");
    assert_eq!(data[at], 0, "version 0");
    at + 8
}

/// A decoder of the track, applying the trims if `gapless`.
fn decoder(reader: &IsoMp4Reader<'_>, gapless: bool) -> AacDecoder {
    let Some(CodecParameters::Audio(params)) = &reader.tracks()[0].codec_params
    else {
        panic!("an audio track");
    };
    let opts = AudioDecoderOptions::default().gapless(gapless);
    AacDecoder::try_new(params, &opts).expect("the decoder opens")
}

/// Decode the packets to the end of the stream, and return the frames decoded.
fn decode(reader: &mut IsoMp4Reader<'_>, decoder: &mut AacDecoder) -> usize {
    let mut frames = 0;
    while let Some(packet) = reader.next_packet().expect("a packet") {
        frames += decoder.decode(&packet).expect("a decoded packet").frames();
    }
    frames
}

/// The frames the track of `data` presents: the frames decoded with the trims applied.
fn presented(data: Vec<u8>) -> usize {
    let mut reader = open(data);
    let mut decoder = decoder(&reader, true);
    decode(&mut reader, &mut decoder)
}

#[test]
fn a_track_states_the_span_its_edit_list_presents() {
    let mut reader = open(fixture("aac_stereo.m4a"));
    let track = reader.tracks()[0].clone();

    assert_eq!(track.start_ts, Timestamp::new(0));
    assert_eq!(track.delay, Some(1024));
    assert_eq!(track.num_frames, Some(48_000));
    // 48 AAC frames decode to 49152 frames: 1024 of delay, 48000 presented, 128 of padding.
    assert_eq!(track.padding, Some(128));

    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap() {
        packets.push(packet);
    }

    // The first frame is all encoder delay. The last sample lasts 896 frames, but decodes to 1024.
    assert_eq!(packets[0].pts, Timestamp::new(-1024));
    assert_eq!((packets[0].trim_start.get(), packets[0].dur.get()), (1024, 0));
    let last = packets.last().unwrap();
    assert_eq!((last.trim_end.get(), last.dur.get()), (128, 896));
    assert_eq!(packets.iter().map(|p| p.dur.get()).sum::<u64>(), 48_000);
}

#[test]
fn the_aac_decoder_presents_exactly_the_span_the_edit_list_states() {
    let mut reader = open(fixture("aac_stereo.m4a"));
    let mut decoder = decoder(&reader, true);

    assert_eq!(decode(&mut reader, &mut decoder), 48_000);

    // Again from the start: the encoder delay is trimmed again.
    let to = SeekTo::Time { time: Time::ZERO, track_id: None };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
    assert_eq!(seeked.actual_ts, Timestamp::new(-1024));
    decoder.reset();
    assert_eq!(decode(&mut reader, &mut decoder), 48_000);
}

#[test]
fn an_aac_seek_takes_the_pre_roll_of_the_roll_recovery_group() {
    // FFmpeg's roll recovery group states a pre-roll of one frame for every AAC frame. 0.5 s
    // presents media timestamp 25024, in the frame from 24576 (number 24), so decoding begins at
    // the frame from 23552, presented at 22528.
    let mut reader = open(fixture("aac_stereo.m4a"));
    let track_id = reader.tracks()[0].id;

    let to = SeekTo::Timestamp { ts: Timestamp::new(24_000), track_id };
    let seeked = reader.seek(SeekMode::Accurate, to).expect("a seek");
    assert_eq!(seeked.actual_ts, Timestamp::new(22_528));
}

#[test]
fn without_gapless_the_aac_decoder_trims_nothing() {
    let mut reader = open(fixture("aac_stereo.m4a"));
    let mut decoder = decoder(&reader, false);

    assert_eq!(decode(&mut reader, &mut decoder), 48 * 1024);
}

#[test]
fn a_sample_entry_rate_of_0_trims_at_the_rate_of_the_audio_specific_config() {
    let mut data = fixture("aac_stereo.m4a");
    let at = sample_entry_rate(&data);
    assert_eq!(get(&data, at), 48_000 << 16);
    put(&mut data, at, 0);

    let track = open(data.clone()).tracks()[0].clone();
    assert_eq!(track.delay, Some(1024));
    assert_eq!(track.padding, Some(128));
    assert_eq!(track.num_frames, Some(48_000));
    assert_eq!(presented(data), 48_000);
}

#[test]
fn a_high_rate_track_trims_at_the_rate_of_its_audio_specific_config() {
    // Above 65535 Hz, FFmpeg states half the rate in the sample entry. The media timescale and the
    // audio specific config state the rate.
    for (name, rate, frames, padding) in
        [("aac_96000.m4a", 96_000, 48_000, 128), ("aac_88200.m4a", 88_200, 44_100, 956)]
    {
        let data = fixture(name);
        assert_eq!(get(&data, sample_entry_rate(&data)), (rate / 2) << 16, "{name}");

        let track = open(data.clone()).tracks()[0].clone();
        assert_eq!(track.delay, Some(1024), "{name}");
        assert_eq!(track.padding, Some(padding), "{name}");
        assert_eq!(track.num_frames, Some(frames), "{name}");
        assert_eq!(presented(data), frames as usize, "{name}");
    }
}

#[test]
fn an_implicit_sbr_track_trims_the_frames_of_its_aac_core() {
    // Write the timing of an implicit-SBR file over the 24 kHz AAC-LC file: its sample entry and
    // media timescale state the 48 kHz SBR output rate, a frame lasts 2048 ticks, and the edit
    // starts after 2048, while its audio specific config states the 24 kHz core. The edit's
    // duration is in the movie timescale. Symphonia's AAC decoder decodes the core, 1024 frames
    // at 24 kHz, so this file decodes as an implicit-SBR file does.
    let mut data = fixture("aac_24000.m4a");

    let at = sample_entry_rate(&data);
    put(&mut data, at, 48_000 << 16);

    let mdhd = body(&data, b"mdhd");
    assert_eq!((data[mdhd], get(&data, mdhd + 12)), (0, 24_000), "version 0, 24 kHz");
    put(&mut data, mdhd + 12, 48_000);
    double(&mut data, mdhd + 16);

    let stts = body(&data, b"stts");
    for entry in 0..get(&data, stts + 4) as usize {
        double(&mut data, stts + 12 + 8 * entry);
    }

    let at = edit_duration(&data) + 4;
    assert_eq!(get(&data, at), 1024);
    put(&mut data, at, 2048);

    let mut reader = open(data.clone());
    let track = reader.tracks()[0].clone();

    // 13 AAC frames decode to 13312 frames: 1024 of delay, 12000 presented, 288 of padding.
    assert_eq!(track.delay, Some(1024));
    assert_eq!(track.padding, Some(288));
    assert_eq!(track.duration.map(|d| d.get()), Some(24_000));

    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap() {
        packets.push(packet);
    }

    // The packets last their ticks at 48 kHz, and trim frames at 24 kHz.
    assert_eq!((packets[0].trim_start.get(), packets[0].dur.get()), (1024, 0));
    assert_eq!(packets[1].dur.get(), 2048);
    let last = packets.last().unwrap();
    assert_eq!((last.trim_end.get(), last.dur.get()), (288, 1472));
    assert_eq!(packets.iter().map(|p| p.dur.get()).sum::<u64>(), 24_000);

    assert_eq!(presented(data), 12_000);
}

#[test]
fn an_edit_past_the_last_sample_presents_no_more_than_the_samples() {
    // An edit's duration is in the movie timescale, which may be coarser than the media's: an edit
    // rounded up to it, or one spanning a longer movie, ends after the last sample.
    for duration in [48_047, 96_000] {
        let mut data = fixture("aac_stereo.m4a");
        let at = edit_duration(&data);
        assert_eq!(get(&data, at), 48_000);
        put(&mut data, at, duration);

        let track = open(data.clone()).tracks()[0].clone();
        assert_eq!(track.delay, Some(1024), "{duration}");
        assert_eq!(track.padding, Some(128), "{duration}");
        assert_eq!(track.num_frames, Some(48_000), "{duration}");
        assert_eq!(track.duration.map(|d| d.get()), Some(48_000), "{duration}");
        assert_eq!(presented(data), 48_000, "{duration}");
    }
}

#[test]
fn without_an_edit_list_the_aac_decoder_presents_the_duration_of_the_samples() {
    let mut data = fixture("aac_stereo.m4a");
    let at = body(&data, b"edts") - 4;
    data[at..at + 4].copy_from_slice(b"free");

    // Without an edit list, the encoder delay is presented, but the padding after the last sample's
    // duration is not.
    let track = open(data.clone()).tracks()[0].clone();
    assert_eq!(track.delay, None);
    assert_eq!(track.padding, Some(128));
    assert_eq!(track.num_frames, Some(49_024));
    assert_eq!(presented(data), 49_024);
}

#[test]
fn the_padding_is_read_from_the_last_stts_entry_that_states_samples() {
    // The fixture's sample table states 47 samples of 1024 ticks, and 1 of 896. Each table here
    // states 48 samples of 1024 ticks, and then an entry of no samples.
    for empty_delta in [60_000, 896] {
        let mut data = fixture("aac_stereo.m4a");
        let stts = body(&data, b"stts");
        let entries: Vec<u32> = (0..5).map(|i| get(&data, stts + 4 + 4 * i)).collect();
        assert_eq!(entries, [2, 47, 1024, 1, 896]);
        for (i, value) in [48, 1024, 0, empty_delta].into_iter().enumerate() {
            put(&mut data, stts + 8 + 4 * i, value);
        }

        let track = open(data.clone()).tracks()[0].clone();
        assert_eq!(track.padding, Some(128), "{empty_delta}");
        assert_eq!(presented(data), 48_000, "{empty_delta}");
    }
}

#[test]
fn a_sample_table_ending_at_the_largest_timestamp_states_no_padding() {
    // The table ends at u64::MAX, and its last sample, of 2 ticks, decodes past it.
    let mut data = fixture("aac_stereo.m4a");
    let stts = body(&data, b"stts");
    for (i, value) in [u32::MAX, u32::MAX, u32::MAX, 2].into_iter().enumerate() {
        put(&mut data, stts + 8 + 4 * i, value);
    }

    assert_eq!(open(data).tracks()[0].padding, None);
}
