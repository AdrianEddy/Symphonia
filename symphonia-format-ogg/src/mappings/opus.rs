// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::SideData;

use super::{MapResult, Mapper, PacketParser};

use symphonia_common::xiph::audio::opus::{self, OPUS_SAMPLE_RATE};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::io::{BufReader, ReadBytes};
use symphonia_core::meta::MetadataBuilder;

use symphonia_core::units::{Duration, Timestamp};
use symphonia_metadata::embedded::vorbis::{self, VORBIS_COMMENT_METADATA_INFO};

use log::warn;

/// The minimum expected size of an Opus identification packet.
const OGG_OPUS_MIN_IDENTIFICATION_PACKET_SIZE: usize = 19;

/// The signature for an Opus identification packet.
const OGG_OPUS_HEAD_SIGNATURE: &[u8] = b"OpusHead";

/// The signature for an Opus metadata packet.
const OGG_OPUS_COMMENT_SIGNATURE: &[u8] = b"OpusTags";

/// The maximum support Opus OGG mapping version.
const OGG_OPUS_MAPPING_VERSION_MAX: u8 = 0x0f;

/// The amount of audio to decode before the seek target to let the decoder converge, 80 ms
/// (RFC 7845, §4.6), in frames at 48 kHz.
const OGG_OPUS_SEEK_PRE_ROLL: u64 = 80 * OPUS_SAMPLE_RATE as u64 / 1000;

pub fn detect(serial: u32, buf: &[u8]) -> Result<Option<Box<dyn Mapper>>> {
    // The identification packet for Opus must be a minimum size, and start with the signature.
    if buf.len() < OGG_OPUS_MIN_IDENTIFICATION_PACKET_SIZE
        || !buf.starts_with(OGG_OPUS_HEAD_SIGNATURE)
    {
        return Ok(None);
    }

    let mut reader = BufReader::new(buf);

    // The stream is an Opus stream. If its identification header is malformed, refuse it by
    // name.
    let opus_head = opus::OpusHead::read(&mut reader, OGG_OPUS_MAPPING_VERSION_MAX)?;

    // Populate the codec parameters with the information read from identification header.
    let mut codec_params = AudioCodecParameters::new();

    codec_params
        .for_codec(CODEC_ID_OPUS)
        .with_sample_rate(OPUS_SAMPLE_RATE)
        .with_channels(opus_head.channels)
        .with_extra_data(Box::from(buf));

    // Create the track.
    let mut track = Track::new(serial);

    track
        .with_codec_params(CodecParameters::Audio(codec_params))
        .with_delay(u32::from(opus_head.pre_skip));

    // Instantiate the Opus mapper.
    let pre_skip = u64::from(opus_head.pre_skip);

    let mapper = Box::new(OpusMapper {
        track,
        need_comment: true,
        pre_skip,
        parser: OpusPacketParser { pre_skip },
    });

    Ok(Some(mapper))
}

/// Parses the duration of Opus packets, and the frames of the pre-skip they carry.
///
/// The decoder's first `pre_skip` frames are the encoder's delay, which must be discarded
/// (RFC 7845, §4.2). A parser is created at the start of the stream, so it reports the pre-skip as
/// the frames to discard from the first packets it parses.
pub struct OpusPacketParser {
    /// The frames of the pre-skip still to be discarded.
    pre_skip: u64,
}

impl PacketParser for OpusPacketParser {
    fn parse_next_packet_dur(&mut self, packet: &[u8]) -> (Duration, Duration) {
        // The duration is read from the packet's TOC byte (RFC 6716, §3.1).
        let Some(frames) = opus::packet_frames(packet)
        else {
            warn!("ogg (opus): malformed packet");
            return (Duration::ZERO, Duration::ZERO);
        };

        let dur = u64::from(frames);
        let discard = self.pre_skip.min(dur);

        self.pre_skip -= discard;

        (Duration::new(dur), Duration::new(discard))
    }
}

struct OpusMapper {
    track: Track,
    need_comment: bool,
    /// The pre-skip, in frames at 48 kHz.
    pre_skip: u64,
    /// The packet parser of the stream's packets.
    parser: OpusPacketParser,
}

impl Mapper for OpusMapper {
    fn name(&self) -> &'static str {
        "opus"
    }

    fn reset(&mut self) {
        // After a discontinuity the packets are not the first packets of the stream, unless the
        // stream is rewound (see `rewind`), so the parser has no pre-skip left to discard. Where a
        // pre-skip outlasts the stream's first page, the logical stream trims the rest of it by
        // timestamp (the frames before the track's start).
        self.parser.pre_skip = 0;
    }

    fn rewind(&mut self) {
        // The packets are the first packets of the stream again, so the pre-skip is discarded
        // again.
        self.parser.pre_skip = self.pre_skip;
    }

    fn track(&self) -> &Track {
        &self.track
    }

    fn track_mut(&mut self) -> &mut Track {
        &mut self.track
    }

    fn absgp_to_ts(&self, absgp: u64) -> Timestamp {
        // The PCM sample position of a granule position is the granule position less the pre-skip
        // (RFC 7845, §4.3).
        Timestamp::from(absgp as i64).saturating_sub(Duration::new(self.pre_skip))
    }

    fn max_rap_period(&self) -> Duration {
        // A decoder resuming from any packet converges within 80 ms, so decoding SHOULD begin at
        // least 80 ms before the seek target (RFC 7845, §4.6).
        Duration::new(OGG_OPUS_SEEK_PRE_ROLL)
    }

    fn make_parser(&self) -> Option<Box<dyn super::PacketParser>> {
        Some(Box::new(OpusPacketParser { pre_skip: self.pre_skip }))
    }

    fn map_packet(&mut self, packet: &[u8]) -> Result<MapResult> {
        if !self.need_comment {
            let (dur, discard) = self.parser.parse_next_packet_dur(packet);
            Ok(MapResult::StreamData { dur, discard })
        }
        else {
            let mut reader = BufReader::new(packet);

            // Read the header signature.
            let mut sig = [0; 8];
            reader.read_buf_exact(&mut sig)?;

            if sig == *OGG_OPUS_COMMENT_SIGNATURE {
                // This packet should be a metadata packet containing a Vorbis Comment.
                let mut builder = MetadataBuilder::new(VORBIS_COMMENT_METADATA_INFO);
                let mut side_data = Default::default();

                vorbis::read_vorbis_comment(&mut reader, &mut builder, &mut side_data)?;

                let rev = builder.build();

                self.need_comment = false;

                Ok(MapResult::SideData { data: SideData::Metadata { rev, side_data } })
            }
            else {
                warn!("ogg (opus): invalid packet type");
                Ok(MapResult::Unknown)
            }
        }
    }
}
