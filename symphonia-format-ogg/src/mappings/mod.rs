// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::common::SideData;

use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::units::{Duration, Timestamp};

mod flac;
mod opus;
mod vorbis;

/// Detect a `Mapper` for a logical stream given the identification packet of the stream.
pub fn detect(serial: u32, buf: &[u8]) -> Result<Option<Box<dyn Mapper>>> {
    let mapper = flac::detect(serial, buf)?
        .or(vorbis::detect(serial, buf)?)
        .or(opus::detect(serial, buf)?)
        .or_else(|| make_null_mapper(serial));

    Ok(mapper)
}

/// Result of a packet map operation.
pub enum MapResult {
    /// The packet contained side data.
    SideData { data: SideData },
    /// The packet contained setup data.
    Setup,
    /// The packet contained stream data.
    StreamData { dur: Duration, discard: Duration },
    /// The packet contained unknown data.
    Unknown,
}

/// A `PacketParser` implements a packet parser that decodes the timestamp and duration for a
/// packet.
pub trait PacketParser: Send + Sync {
    fn parse_next_packet_dur(&mut self, packet: &[u8]) -> (Duration, Duration);
}

/// A `Mapper` implements packet-handling for a specific `Codec`.
pub trait Mapper: Send + Sync {
    /// Gets the name of the mapper.
    fn name(&self) -> &'static str;

    /// Soft-reset the mapper after a discontinuity in packets.
    fn reset(&mut self);

    /// Called after a discontinuity in packets, when the packets that follow are the first
    /// packets of the stream again (e.g., after seeking to the start of the stream). State that
    /// only applies at the start of the stream, such as encoder delay to discard, applies again.
    fn rewind(&mut self) {}

    /// Gets an immutable reference to the `Track` for the stream belonging to this `Mapper`. If
    /// the stream is not ready then the track may be incomplete.
    fn track(&self) -> &Track;

    /// Gets a mutable reference to the `Track` for the stream belonging to this `Mapper`.
    /// If the stream is not ready then the track may be incomplete.
    fn track_mut(&mut self) -> &mut Track;

    /// Convert an absolute granular position to a timestamp.
    fn absgp_to_ts(&self, absgp: u64) -> Timestamp {
        Timestamp::from(absgp as i64)
    }

    /// Get the maximum duration between two random access points.
    fn max_rap_period(&self) -> Duration {
        Duration::new(0)
    }

    /// Make a packet parser for parsing packet timing, or `None` if the packets can't be timed.
    fn make_parser(&self) -> Option<Box<dyn PacketParser>>;

    /// Map a packet.
    fn map_packet(&mut self, packet: &[u8]) -> Result<MapResult>;

    /// Returns `true` if the stream can is ready for usage. If the stream is not ready then the
    /// mapper needs to consume more setup packets.
    fn is_ready(&self) -> bool {
        true
    }
}

fn make_null_mapper(serial: u32) -> Option<Box<dyn Mapper>> {
    Some(Box::new(NullMapper::new(serial)))
}

struct NullMapper {
    track: Track,
}

impl NullMapper {
    fn new(serial: u32) -> Self {
        // The track of a stream is identified by the stream's serial, as for every mapper.
        NullMapper { track: Track::new(serial) }
    }
}

impl Mapper for NullMapper {
    fn name(&self) -> &'static str {
        "null"
    }

    fn track(&self) -> &Track {
        &self.track
    }

    fn track_mut(&mut self) -> &mut Track {
        &mut self.track
    }

    fn reset(&mut self) {
        // Nothing to do!
    }

    fn make_parser(&self) -> Option<Box<dyn PacketParser>> {
        // Nothing reads the stream's packets, so nothing times them: its granule positions are the
        // codec's (Theora's count keyframes and the frames since), not timestamps. The stream
        // states no start, length or padding.
        None
    }

    fn map_packet(&mut self, _: &[u8]) -> Result<MapResult> {
        Ok(MapResult::Unknown)
    }
}
