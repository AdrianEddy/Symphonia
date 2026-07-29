// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::{
    audio::{Channels, layouts},
    errors::{Result, decode_error, unsupported_error},
    io::ReadBytes,
};

use crate::xiph::audio::vorbis::vorbis_channels_to_channels;

const OPUS_MAGIC_SIGNATURE: &[u8] = b"OpusHead";

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

        // The next byte indicates the channel mapping. Most of these values are reserved.
        let channel_mapping = reader.read_byte()?;

        let channels = match channel_mapping {
            // Channel mapping family 0, the RTP mapping (RFC 7845, §5.1.1.1).
            // "Allowed numbers of channels: 1 or 2", assigned as "1 channel:
            // monophonic (mono)" and "2 channels: stereo (left, right)". A lone
            // Opus channel is the mono channel, which is front CENTRE — not a
            // front-left channel that happens to be alone.
            0 if channel_count == 1 => layouts::CHANNEL_LAYOUT_MONO,
            0 if channel_count == 2 => layouts::CHANNEL_LAYOUT_STEREO,
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
            // Reserved, and should NOT be supported for playback.
            _ => return unsupported_error("common (opus): unsupported channel mapping family"),
        };

        Ok(Self { version, channels, gain, original_sample_rate, pre_skip })
    }
}
