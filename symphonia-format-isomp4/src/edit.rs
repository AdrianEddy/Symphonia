// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::num::NonZero;

use crate::atoms::ElstAtom;

use log::warn;

/// Rescale `value` from one timescale to another, rounding to the nearest unit.
pub(crate) fn rescale(value: u64, from: NonZero<u32>, to: NonZero<u32>) -> u64 {
    if from == to {
        return value;
    }
    let (from, to) = (u128::from(from.get()), u128::from(to.get()));
    ((u128::from(value) * to + from / 2) / from) as u64
}

/// The presentation of a track's media stated by its edit list (ISO/IEC 14496-12, §8.6.6).
///
/// An encoder's delay and padding are stated by the edit list: the media before the edit's media
/// time is the encoder delay (priming), and the media after the edit's duration is padding. The
/// presentation is read from an edit list of empty edits, which delay the start of the media, and
/// one edit of the media played at the normal rate. Any other edit list cannot be presented by
/// trimming the start and end of the media, so it is not read: all of the media is presented, as
/// without an edit list.
///
/// Nothing past the end of the media is presented. The media ends with the duration of its last
/// sample, which an encoder shortens to the sample's valid frames, while an edit's duration is
/// rounded to the movie timescale, and may span a longer movie.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Presentation {
    /// The media timestamp of the first presented frame. Decoded frames before it are trimmed.
    media_start: u64,
    /// The media timestamp after the last presented frame, if known. Decoded frames from it on
    /// are trimmed.
    media_end: Option<u64>,
    /// The presentation timestamp of the first presented frame: the duration of the empty edits
    /// before the media's edit.
    start_ts: u64,
}

impl Presentation {
    /// Read the presentation from the edit list `elst` of a track whose media timescale is
    /// `media_timescale`, in a movie whose timescale is `movie_timescale`.
    pub(crate) fn from_edit_list(
        elst: &ElstAtom,
        movie_timescale: NonZero<u32>,
        media_timescale: NonZero<u32>,
    ) -> Option<Presentation> {
        let mut start_ts = 0u64;
        let mut presentation = None;

        for entry in &elst.entries {
            match entry.media_time {
                // An empty edit. Before the media's edit, it delays the start of the media. After
                // it, it has no effect on the media.
                -1 => {
                    if presentation.is_none() {
                        let duration =
                            rescale(entry.segment_duration, movie_timescale, media_timescale);
                        start_ts = start_ts.saturating_add(duration);
                    }
                }
                media_time @ 0.. if presentation.is_none() => {
                    if entry.media_rate_int != 1 || entry.media_rate_frac != 0 {
                        warn!("isomp4: ignoring an edit list that changes the media rate");
                        return None;
                    }

                    let media_start = media_time as u64;

                    // A duration of 0 states no end: the edit extends to the end of the media.
                    let media_end = match entry.segment_duration {
                        0 => None,
                        duration => Some(media_start.saturating_add(rescale(
                            duration,
                            movie_timescale,
                            media_timescale,
                        ))),
                    };

                    presentation = Some(Presentation { media_start, media_end, start_ts });
                }
                0.. => {
                    warn!("isomp4: ignoring an edit list of more than one edit of the media");
                    return None;
                }
                media_time => {
                    warn!("isomp4: ignoring an edit list with an invalid media time {media_time}");
                    return None;
                }
            }
        }

        presentation
    }

    /// The presentation of all of the media, for a track whose edit list is absent or not read.
    pub(crate) fn whole() -> Presentation {
        Presentation { media_start: 0, media_end: None, start_ts: 0 }
    }

    /// Bound the presentation by the end of the media, `media_end`, if it is known.
    pub(crate) fn bounded_by(self, media_end: Option<u64>) -> Presentation {
        let end = match (self.media_end, media_end) {
            (Some(edit_end), Some(media_end)) => Some(edit_end.min(media_end)),
            (edit_end, media_end) => edit_end.or(media_end),
        };

        // An edit that starts after the end of the media presents nothing.
        Presentation { media_end: end.map(|end| end.max(self.media_start)), ..self }
    }

    /// The media timestamp of the first presented frame.
    pub(crate) fn media_start(&self) -> u64 {
        self.media_start
    }

    /// The media timestamp after the last presented frame, if known.
    pub(crate) fn media_end(&self) -> Option<u64> {
        self.media_end
    }

    /// The presentation timestamp of the first presented frame.
    pub(crate) fn start_ts(&self) -> u64 {
        self.start_ts
    }

    /// Get the presentation timestamp of the media timestamp `ts`.
    pub(crate) fn media_to_pts(&self, ts: u64) -> i64 {
        let pts = i128::from(ts) - i128::from(self.media_start) + i128::from(self.start_ts);
        pts.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    /// Get the media timestamp of the presentation timestamp `pts`. Presentation timestamps before
    /// the first presented frame map to the first presented frame.
    pub(crate) fn pts_to_media(&self, pts: u64) -> u64 {
        pts.saturating_sub(self.start_ts).saturating_add(self.media_start)
    }

    /// Get the durations to trim from the start and the end of a sample decoded to `decoded` media
    /// timescale units from the media timestamp `ts` on.
    pub(crate) fn trims(&self, ts: u64, decoded: u64) -> (u64, u64) {
        let end = ts.saturating_add(decoded);

        let trim_start = self.media_start.saturating_sub(ts).min(decoded);
        let trim_end = match self.media_end {
            Some(media_end) => end.saturating_sub(media_end).min(decoded - trim_start),
            None => 0,
        };

        (trim_start, trim_end)
    }
}
