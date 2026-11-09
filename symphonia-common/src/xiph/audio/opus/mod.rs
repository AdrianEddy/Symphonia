// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::{
    audio::{ChannelLabel, Channels, Position, layouts},
    errors::{Result, decode_error, unsupported_error},
    io::ReadBytes,
};

use crate::xiph::audio::vorbis::vorbis_channels_to_channels;

const OPUS_MAGIC_SIGNATURE: &[u8] = b"OpusHead";

/// The sample rate an Opus decoder always outputs, and the rate every Opus duration is counted in
/// (the pre-skip, granule positions, and packet durations).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// The longest an Opus packet may last, in frames at 48 kHz: 120 ms (RFC 6716, §3.2.5).
pub const OPUS_MAX_PACKET_FRAMES: u32 = 5_760;

/// The number of frames at 48 kHz an Opus packet decodes to.
///
/// The count is read from the packet's table-of-contents (TOC) byte (RFC 6716, §3.1), and for a
/// code 3 packet from its frame count byte (RFC 6716, §3.2.5). Returns `None` for an empty packet,
/// for a code 3 packet without its frame count byte or with a frame count of 0, and for a packet
/// longer than 120 ms. Such packets are malformed (RFC 6716, §3.4, requirements R1, R5, and R6).
pub fn packet_frames(packet: &[u8]) -> Option<u32> {
    // The duration of one frame for each configuration number, the 5 most significant bits of the
    // TOC byte (RFC 6716, §3.1, Table 2), in frames at 48 kHz.
    #[rustfmt::skip]
    const CONFIG_FRAME_DURATION: [u32; 32] = [
        // SILK-only NB, MB, and WB at 10, 20, 40, and 60 ms.
        480, 960, 1920, 2880,
        480, 960, 1920, 2880,
        480, 960, 1920, 2880,
        // Hybrid SWB and FB at 10 and 20 ms.
        480, 960,
        480, 960,
        // CELT-only NB, WB, SWB, and FB at 2.5, 5, 10, and 20 ms.
        120, 240, 480, 960,
        120, 240, 480, 960,
        120, 240, 480, 960,
        120, 240, 480, 960,
    ];

    let toc = *packet.first()?;

    // The frame count code is the 2 least significant bits of the TOC byte (RFC 6716, §3.1).
    let num_frames = match toc & 0x3 {
        0 => 1,
        1 | 2 => 2,
        // A code 3 packet follows the TOC byte with a frame count byte whose 6 least significant
        // bits are the frame count, M, which must not be 0 (RFC 6716, §3.2.5).
        _ => match packet.get(1)? & 0x3f {
            0 => return None,
            m => u32::from(m),
        },
    };

    let frames = CONFIG_FRAME_DURATION[usize::from(toc >> 3)] * num_frames;

    if frames > OPUS_MAX_PACKET_FRAMES {
        return None;
    }

    Some(frames)
}

#[derive(Debug, Default)]
pub struct OpusHead {
    pub version: u8,
    pub channels: Channels,
    pub original_sample_rate: u32,
    pub gain: i16,
    pub pre_skip: u16,
}

impl OpusHead {
    pub fn read<B: ReadBytes>(reader: &mut B, max_version: u8) -> Result<Self> {
        // The first 8 bytes are the magic signature ASCII bytes.
        let mut magic = [0; 8];
        reader.read_buf_exact(&mut magic)?;

        if magic != *OPUS_MAGIC_SIGNATURE {
            return unsupported_error("common (opus): invalid magic signature");
        }

        // The next byte is the encapsulation version. The max version is specified by the caller
        // since it depends on the container format used.
        let version = reader.read_byte()?;
        if version > max_version {
            return decode_error("common (opus): invalid version");
        }

        // The next byte is the number of channels and must not be 0.
        let channel_count = reader.read_byte()?;

        if channel_count == 0 {
            return decode_error("common (opus): invalid channel count");
        }

        // The next 16-bit integer is the pre-skip padding (# of samples at 48kHz to subtract from
        // the OGG granule position to obtain the PCM sample position).
        let pre_skip = reader.read_u16()?;

        // The next 32-bit integer is the sample rate of the original audio.
        let original_sample_rate = reader.read_u32()?;

        // Next, the 16-bit gain value.
        let gain = reader.read_i16()?;

        // The next byte indicates the channel mapping family.
        let channel_mapping = reader.read_byte()?;

        let channels = match channel_mapping {
            // Channel mapping family 0, the RTP mapping (RFC 7845, §5.1.1.1).
            // "Allowed numbers of channels: 1 or 2", assigned as "1 channel:
            // monophonic (mono)" and "2 channels: stereo (left, right)". A lone
            // Opus channel is the mono channel, which is front CENTRE — not a
            // front-left channel that happens to be alone.
            0 if channel_count == 1 => layouts::CHANNEL_LAYOUT_MONO,
            0 if channel_count == 2 => layouts::CHANNEL_LAYOUT_STEREO,
            0 => return decode_error("common (opus): invalid channel count for mapping family 0"),
            // Channel mapping family 1 (RFC 7845, §5.1.1.2). "Allowed numbers of
            // channels: 1...8. Vorbis channel order", and of the speaker locations
            // it lists: "This set of surround options and speaker location
            // orderings is the same as those used by the Vorbis codec". So it is
            // the Vorbis table, verbatim — including "1 channel: monophonic
            // (mono)".
            1 => match vorbis_channels_to_channels(channel_count) {
                Some(channels) => channels,
                // Family 1 permits at most 8 channels.
                None => return decode_error("common (opus): invalid vorbis channel mapping"),
            },
            // Channel mapping families 2 and 3 (RFC 8486, §3.1 and §3.2): an ambisonic sound field
            // of (1 + n)^2 components in ACN order (SN3D), n = 0...14, optionally followed by a
            // head-locked stereo pair. Family 2 codes the channels as they are, family 3 through a
            // demixing matrix, and both decode to the same channels.
            2 | 3 => match ambisonic_channels(channel_count) {
                Some(channels) => channels,
                None => return decode_error("common (opus): invalid ambisonic channel count"),
            },
            // Channel mapping family 255 (RFC 7845, §5.1.1.3): channels with no defined meaning.
            // The remaining families are reserved, and "a demuxer implementation encountering a
            // reserved 'Channel Mapping Family' value SHOULD act as though the value is 255"
            // (RFC 7845, §5.1.1.4).
            _ => Channels::Discrete(u16::from(channel_count)),
        };

        Ok(Self { version, channels, gain, original_sample_rate, pre_skip })
    }
}

/// The channels of an ambisonic channel mapping (families 2 and 3) of `channel_count` channels: a
/// full sound field of order n, or a full sound field followed by a head-locked stereo pair.
fn ambisonic_channels(channel_count: u8) -> Option<Channels> {
    let count = u16::from(channel_count);

    (0..=14u8).find_map(|order| {
        let components = (1 + u16::from(order)).pow(2);

        if count == components {
            Some(Channels::Ambisonic(order))
        }
        else if count == components + 2 {
            let labels = (0..components)
                .map(ChannelLabel::Ambisonic)
                .chain([
                    ChannelLabel::Positioned(Position::FRONT_LEFT),
                    ChannelLabel::Positioned(Position::FRONT_RIGHT),
                ])
                .collect();

            Some(Channels::Custom(labels))
        }
        else {
            None
        }
    })
}

#[cfg(test)]
mod tests;
