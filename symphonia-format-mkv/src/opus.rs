// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gapless trimming of Opus tracks (codec ID `A_OPUS`).
//!
//! An Opus track's `CodecDelay` is the encoder's delay (the Opus pre-skip), which must be discarded
//! from the start of the track, and a block's `DiscardPadding` is the padding to discard from the
//! end of the block (or from its start, if negative). Both are stated in nanoseconds, while the
//! trims are counted in frames at 48 kHz, the rate an Opus decoder always outputs. They are
//! converted from the nanosecond values, not from the rounded track timestamps. The number of
//! frames each Opus packet decodes to is read from its TOC byte.

use symphonia_common::xiph::audio::opus::{self, OPUS_SAMPLE_RATE};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, TimeBase};

use crate::segment::{MatroskaTicks, SignedMatroskaTicks, TrackTicks};

/// An Opus frame of a block: the frames at 48 kHz its packet decodes to, read from its TOC, and
/// its share of the block's `DiscardPadding`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct OpusFrame {
    /// The frames at 48 kHz the packet decodes to.
    frames: u32,
    /// The frames of a negative discard padding to trim from the start of the packet.
    discard_start: u32,
    /// The frames of a positive discard padding to trim from the end of the packet.
    discard_end: u32,
}

/// Share a block's `DiscardPadding` out over its Opus frames, given in their order in the block.
/// A positive padding is at the end of the block, so it is shared from the last frame back, and a
/// negative padding is at its start, so it is shared from the first frame on. Each frame takes as
/// much of the padding left as it holds.
pub(crate) fn discard_padding<'a, I>(frames: I, discard_padding: SignedMatroskaTicks)
where
    I: DoubleEndedIterator<Item = &'a mut OpusFrame>,
{
    let padding = discard_padding.get();
    let mut left = ns_to_frames(padding.unsigned_abs());

    let mut share = |frames: u32| {
        let share = left.min(u64::from(frames));
        left -= share;
        share as u32
    };

    if padding < 0 {
        frames.for_each(|frame| frame.discard_start = share(frame.frames));
    }
    else {
        frames.rev().for_each(|frame| frame.discard_end = share(frame.frames));
    }
}

/// Frames at 48 kHz in `ns` nanoseconds, rounded to the nearest frame.
fn ns_to_frames(ns: u64) -> u64 {
    ((u128::from(ns) * u128::from(OPUS_SAMPLE_RATE) + 500_000_000) / 1_000_000_000) as u64
}

/// Track ticks in `frames` frames at 48 kHz, rounded down.
fn frames_to_ticks(frames: u64, track_time_base: TimeBase) -> u64 {
    // The timebase is the length of a tick in seconds.
    let numer = u128::from(OPUS_SAMPLE_RATE) * u128::from(track_time_base.numer.get());
    let denom = u128::from(track_time_base.denom.get());
    (u128::from(frames) * denom / numer) as u64
}

/// The durations of the Opus frames of a block, read from their TOC bytes.
///
/// The timestamp of each frame is the block's timestamp plus the frames decoded before it, and each
/// duration is the difference of two such timestamps, so that rounding to track ticks does not
/// accumulate across the frames of a laced block.
pub(crate) struct OpusBlockDurations {
    track_time_base: TimeBase,
    /// The frames at 48 kHz decoded from the block so far.
    frames: u64,
    /// `frames` in track ticks.
    ticks: u64,
}

impl OpusBlockDurations {
    pub(crate) fn new(track_time_base: TimeBase) -> Self {
        OpusBlockDurations { track_time_base, frames: 0, ticks: 0 }
    }

    /// The duration of the next frame of the block, and the frame, or `None` if the frame is not a
    /// well-formed Opus packet.
    pub(crate) fn next(&mut self, data: &[u8]) -> Option<(TrackTicks, OpusFrame)> {
        let frames = opus::packet_frames(data)?;

        self.frames += u64::from(frames);

        let ticks = frames_to_ticks(self.frames, self.track_time_base);
        let dur = ticks - self.ticks;

        self.ticks = ticks;

        Some((TrackTicks::from(dur), OpusFrame { frames, discard_start: 0, discard_end: 0 }))
    }
}

/// The gapless trimming state of an Opus track.
pub(crate) struct OpusTrim {
    /// The track's timebase.
    track_time_base: TimeBase,
    /// The codec delay, in frames at 48 kHz.
    delay: u64,
    /// The codec delay in track ticks, as subtracted from the timestamp of each block.
    delay_ticks: u64,
    /// The frames of the codec delay still to be trimmed.
    delay_left: u64,
    /// The timestamp of the track's first frame, once the reader has read it.
    start: Option<i64>,
    /// The reader was moved by a seek, so the next frame of the track decides how much of the
    /// codec delay applies again.
    relocated: bool,
}

impl OpusTrim {
    /// `codec_delay` is the track's `CodecDelay`, and `delay_ticks` the same in track ticks, as
    /// subtracted from the timestamp of each block.
    pub(crate) fn new(
        codec_delay: MatroskaTicks,
        delay_ticks: TrackTicks,
        track_time_base: TimeBase,
    ) -> Self {
        let delay = ns_to_frames(codec_delay.get());

        OpusTrim {
            track_time_base,
            delay,
            delay_ticks: delay_ticks.get(),
            delay_left: delay,
            start: None,
            relocated: false,
        }
    }

    /// Called after the reader is moved by a seek.
    pub(crate) fn relocate(&mut self) {
        self.relocated = true;
    }

    /// Called with the timestamp of each block of the track that the reader reads having read every
    /// block before it: the first such block holds the track's first frame.
    pub(crate) fn observe(&mut self, pts: i64) {
        self.start.get_or_insert(pts);
    }

    /// Whether the track's first frame has been read.
    pub(crate) fn knows_start(&self) -> bool {
        self.start.is_some()
    }

    /// Frames at 48 kHz in `ticks` track ticks, rounded to the nearest frame.
    fn ticks_to_frames(&self, ticks: u64) -> u64 {
        let numer = u128::from(OPUS_SAMPLE_RATE) * u128::from(self.track_time_base.numer.get());
        let denom = u128::from(self.track_time_base.denom.get());
        ((u128::from(ticks) * numer + denom / 2) / denom) as u64
    }

    /// Set the trims of the packet of the track's next frame, and if it is trimmed, its duration to
    /// the duration of its frames that remain after trimming. An untrimmed packet keeps the duration
    /// of its block's frames (`OpusBlockDurations`), so that it ends where the next frame of a laced
    /// block begins. `frame` is the Opus frame of the packet, or `None` if the packet is not a
    /// well-formed Opus packet.
    pub(crate) fn trim(&mut self, packet: &mut Packet, frame: Option<OpusFrame>) {
        let pts = packet.pts.get();

        if self.relocated {
            // The codec delay left at this frame is the codec delay less the frames from the start
            // of the track to this frame. If the start of the track is not known, because the
            // reader has not read the track's first frame, the track is taken to start at the
            // timestamp of 0 (before the codec delay is subtracted).
            let start = self.start.unwrap_or(-(self.delay_ticks as i64));

            let since_start = pts.saturating_sub(start);

            self.delay_left =
                self.delay.saturating_sub(self.ticks_to_frames(since_start.max(0) as u64));
            self.relocated = false;
        }

        let Some(OpusFrame { frames, discard_start, discard_end }) = frame
        else {
            return;
        };

        let decoded = u64::from(frames);

        // The codec delay is trimmed from the start of the track's first frames.
        let delay = self.delay_left.min(decoded);

        self.delay_left -= delay;

        // Then the frame's share of its block's discard padding.
        let trim_start = (delay + u64::from(discard_start)).min(decoded);
        let trim_end = u64::from(discard_end).min(decoded - trim_start);

        packet.trim_start = Duration::new(trim_start);
        packet.trim_end = Duration::new(trim_end);
        if trim_start + trim_end > 0 {
            let presented = decoded - trim_start - trim_end;
            packet.dur = Duration::new(frames_to_ticks(presented, self.track_time_base));
        }
    }
}
