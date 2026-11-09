// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{ChannelLabel, Channels, Position, layouts};
use symphonia_core::errors::Error;
use symphonia_core::io::BufReader;

use super::{OPUS_MAX_PACKET_FRAMES, OpusHead, packet_frames};

/// A TOC byte for a configuration number and frame count code.
fn toc(config: u8, code: u8) -> u8 {
    (config << 3) | code
}

#[test]
fn verify_packet_frames_for_each_configuration() {
    // RFC 6716, §3.1, Table 2: the frame duration of each configuration in ms.
    let ms = [
        10.0, 20.0, 40.0, 60.0, 10.0, 20.0, 40.0, 60.0, 10.0, 20.0, 40.0, 60.0, 10.0, 20.0, 10.0,
        20.0, 2.5, 5.0, 10.0, 20.0, 2.5, 5.0, 10.0, 20.0, 2.5, 5.0, 10.0, 20.0, 2.5, 5.0, 10.0,
        20.0,
    ];

    for (config, ms) in ms.into_iter().enumerate() {
        let frame = (ms * 48.0) as u32;
        let config = config as u8;
        // Code 0: one frame; codes 1 and 2: two frames.
        assert_eq!(packet_frames(&[toc(config, 0)]), Some(frame), "config {config}, code 0");
        assert_eq!(packet_frames(&[toc(config, 1), 0]), Some(2 * frame), "config {config}, code 1");
        assert_eq!(packet_frames(&[toc(config, 2), 0]), Some(2 * frame), "config {config}, code 2");
    }
}

#[test]
fn verify_packet_frames_of_code_3_packets() {
    // The frame count is the 6 least significant bits of the byte after the TOC byte, whatever the
    // VBR and padding flags above them.
    assert_eq!(packet_frames(&[toc(16, 3), 0b0000_0011]), Some(3 * 120));
    assert_eq!(packet_frames(&[toc(16, 3), 0b1100_0011]), Some(3 * 120));
    // 48 frames of 2.5 ms are 120 ms, the longest packet (a frame count past 31 needs the sixth
    // bit).
    assert_eq!(packet_frames(&[toc(16, 3), 48]), Some(OPUS_MAX_PACKET_FRAMES));
    // 49 frames of 2.5 ms are too long, as are 7 frames of 20 ms.
    assert_eq!(packet_frames(&[toc(16, 3), 49]), None);
    assert_eq!(packet_frames(&[toc(1, 3), 7]), None);
    // 3 frames of 40 ms are 120 ms.
    assert_eq!(packet_frames(&[toc(2, 3), 3]), Some(OPUS_MAX_PACKET_FRAMES));
    // A code 3 packet must state at least one frame, and must have the frame count byte.
    assert_eq!(packet_frames(&[toc(1, 3), 0]), None);
    assert_eq!(packet_frames(&[toc(1, 3)]), None);
    // An empty packet has no TOC byte.
    assert_eq!(packet_frames(&[]), None);
}

/// An identification header: version 1, `channels`, pre-skip 312, 48 kHz, a gain of -1 (Q7.8),
/// then the mapping `family` and, for any family but 0, a stream count, coupled count, and
/// `table`.
fn opus_head(channels: u8, family: u8, table: &[u8]) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, channels, 0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0xff, 0xff, family]);
    if family != 0 {
        head.extend_from_slice(table);
    }
    head
}

fn read(head: &[u8]) -> symphonia_core::errors::Result<OpusHead> {
    OpusHead::read(&mut BufReader::new(head), 15)
}

#[test]
fn verify_opus_head_fields() {
    let head = read(&opus_head(2, 0, &[])).unwrap();
    assert_eq!(head.version, 1);
    assert_eq!(head.pre_skip, 312);
    assert_eq!(head.original_sample_rate, 48_000);
    assert_eq!(head.gain, -1);
    assert_eq!(head.channels, layouts::CHANNEL_LAYOUT_STEREO);
}

#[test]
fn verify_opus_head_families_0_and_1() {
    assert_eq!(read(&opus_head(1, 0, &[])).unwrap().channels, layouts::CHANNEL_LAYOUT_MONO);
    assert!(matches!(read(&opus_head(3, 0, &[])), Err(Error::DecodeError(_))));

    let surround = read(&opus_head(6, 1, &[4, 2, 0, 4, 1, 2, 3, 5])).unwrap();
    assert_eq!(surround.channels.count(), 6);
    assert!(matches!(surround.channels, Channels::Positioned(_)));
    assert!(matches!(read(&opus_head(9, 1, &[])), Err(Error::DecodeError(_))));
}

#[test]
fn verify_opus_head_ambisonic_families() {
    for family in [2, 3] {
        // A full first-order field.
        let field = read(&opus_head(4, family, &[4, 0, 0, 1, 2, 3])).unwrap();
        assert_eq!(field.channels, Channels::Ambisonic(1), "family {family}");

        // A full third-order field.
        let field = read(&opus_head(16, family, &[])).unwrap();
        assert_eq!(field.channels, Channels::Ambisonic(3), "family {family}");

        // A first-order field and a head-locked stereo pair.
        let field = read(&opus_head(6, family, &[])).unwrap();
        let labels: Vec<ChannelLabel> = (0..4)
            .map(ChannelLabel::Ambisonic)
            .chain([
                ChannelLabel::Positioned(Position::FRONT_LEFT),
                ChannelLabel::Positioned(Position::FRONT_RIGHT),
            ])
            .collect();
        assert_eq!(field.channels, Channels::Custom(labels.into()), "family {family}");

        // Neither a full field nor a full field and a pair.
        assert!(matches!(read(&opus_head(5, family, &[])), Err(Error::DecodeError(_))));
        assert!(matches!(read(&opus_head(7, family, &[])), Err(Error::DecodeError(_))));
    }
}

#[test]
fn verify_opus_head_discrete_and_reserved_families() {
    // Family 255, and a reserved family read as though it were 255 (RFC 7845, §5.1.1.4).
    for family in [255, 4, 254] {
        let head = read(&opus_head(5, family, &[5, 0, 0, 1, 2, 3, 4])).unwrap();
        assert_eq!(head.channels, Channels::Discrete(5), "family {family}");
    }
}

#[test]
fn verify_opus_head_versions_and_signature() {
    let mut head = opus_head(1, 0, &[]);
    head[8] = 15;
    assert_eq!(read(&head).unwrap().version, 15);
    head[8] = 16;
    assert!(matches!(read(&head), Err(Error::DecodeError(_))));
    head[8] = 1;
    head[0] = b'X';
    assert!(matches!(read(&head), Err(Error::Unsupported(_))));
}
