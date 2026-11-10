// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::num::NonZero;

use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_MKV;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::TimeBase;

use log::{info, warn};

use crate::codecs::make_track_codec_params;
use crate::ebml::{
    EbmlElementInfo, EbmlError, EbmlIterator, EbmlIteratorState, EbmlSchema, ReadEbml,
};
use crate::lacing::{Frame, calculate_block_pts, extract_frames};
use crate::opus::OpusTrim;
use crate::schema::{MkvElement, MkvSchema};
use crate::segment::{
    AttachmentsElement, BlockGroupElement, ChaptersElement, CuesElement, EbmlHeaderElement,
    InfoElement, NonZeroMatroskaTicks, SeekHeadElement, SegmentTicks, SignedTrackTicks,
    TagsElement, TargetTagsMap, TrackTicks, TracksElement,
};

const MKV_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MKV, short_name: "matroska", long_name: "Matroska / WebM" };

pub struct TrackState {
    /// The Matroska track number (Symphonia's track ID).
    track_num: u32,
    /// The default frame duration.
    pub(crate) default_frame_duration: Option<NonZeroMatroskaTicks>,
    /// The codec delay, subtracted from the timestamp of each block: in track ticks, rounded to
    /// the nearest tick, as FFmpeg's muxer adds it to the block timestamps and its demuxer
    /// subtracts it.
    pub(crate) codec_delay: TrackTicks,
    /// The track's timebase.
    pub(crate) track_time_base: TimeBase,
    /// The track's timestamp scale.
    pub(crate) track_timestamp_scale: f64,
    /// The seek pre-roll: the duration of data the decoder must decode after a discontinuity
    /// before its output is valid.
    seek_pre_roll: TrackTicks,
    /// For an Opus track, its gapless trimming state.
    pub(crate) opus: Option<OpusTrim>,
    /// The timestamp and duration (0 if unknown) of the track's frame the reader read past last,
    /// handed out or dropped by a seek: on a source the reader can't seek in, a seek can't reach it
    /// or a frame before it.
    last_read: Option<(Timestamp, TrackTicks)>,
}

/// Matroska (MKV) and WebM demultiplexer.
///
/// `MkvReader` implements a demuxer for the Matroska and WebM formats.
pub struct MkvReader<'s> {
    /// Iterator over EBML element headers
    iter: EbmlIterator<MediaSourceStream<'s>, MkvSchema>,
    /// The state of the iterator at the start of the Segment element's children.
    segment_state: EbmlIteratorState<MkvSchema>,
    /// The position of the Segment element's data, which its children's positions are relative to.
    segment_pos: u64,
    /// The position of the first Cluster element, relative to the Segment element's data.
    first_cluster_pos: u64,
    /// Whether the source can seek. A seek on a source that can't is read on from where the reader
    /// is.
    is_seekable: bool,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    track_states: HashMap<u32, TrackState>,
    attachments: Vec<Attachment>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    cues: Option<CuesElement>,
    /// Without Cues, on a source that can seek: the timestamp and the position (relative to the
    /// Segment element's data) of each cluster read, in order from the first, an index built while
    /// reading that a seek searches as it would the Cues.
    clusters: Vec<(SegmentTicks, u64)>,
    current_cluster: Option<ClusterState>,
    frames: VecDeque<Frame>,
    /// Every element from the start of the first cluster up to this position has been read.
    read_to: u64,
    /// Every element from the start of the first cluster up to the iterator's position has been
    /// read, so the first frame of a track that is read is the track's first frame.
    read_all_before: bool,
}

#[derive(Copy, Clone, Debug)]
struct ClusterState {
    /// The cluster timestamp in Segment ticks..
    timestamp: Option<SegmentTicks>,
    /// The position of the cluster element, relative to the Segment element's data.
    start: u64,
}

impl<'s> MkvReader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Get the total length of the stream, if possible.
        let (is_seekable, total_len) = (mss.is_seekable(), mss.byte_len());

        match total_len {
            Some(len) if is_seekable => info!("stream is seekable with len={len} bytes."),
            _ => (),
        }

        let mut it = EbmlIterator::new(mss, MkvSchema, total_len);

        // Read the EBML header.
        let ebml = it.next_element::<EbmlHeaderElement>()?;

        if !matches!(ebml.doc_type.as_str(), "matroska" | "webm") {
            return unsupported_error("mkv: not a matroska / webm file");
        }

        // Read the root element, Segment.
        let segment_pos = match it.next_header()? {
            Some(elem) if elem.element_type() == MkvElement::Segment => elem.data_pos(),
            _ => return unsupported_error("mkv: missing segment element"),
        };

        // Descend into the Segment element.
        it.push_element()?;

        let segment_state = it.save_state();

        let mut segment_tracks = None;
        let mut info = None;
        let mut cues = None;
        let mut current_cluster = None;
        let mut seek_positions = Vec::new();
        let mut tags = Vec::new();
        let mut attachments = None;
        let mut chapters = None;

        while let Ok(Some(header)) = it.next_header() {
            match header.element_type() {
                MkvElement::SeekHead => {
                    let seek_head = it.read_master_element::<SeekHeadElement>()?;
                    for element in seek_head.seeks.into_vec() {
                        let element_type = match it.schema().get_element_info(element.id as u32) {
                            Some(info) => info.element_type(),
                            None => continue,
                        };
                        seek_positions.push((element_type, element.position));
                    }
                }
                MkvElement::Tracks => {
                    segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                }
                MkvElement::Info => {
                    info = Some(it.read_master_element::<InfoElement>()?);
                }
                MkvElement::Cues => {
                    cues = Some(it.read_master_element::<CuesElement>()?);
                }
                MkvElement::Tags => {
                    // Multiple tags element per segment allowed.
                    tags.push(it.read_master_element::<TagsElement>()?);
                }
                MkvElement::Cluster => {
                    // Set state for current cluster for the first call of `next_element`.
                    current_cluster =
                        Some(ClusterState { timestamp: None, start: header.pos() - segment_pos });

                    // Don't look forward into the stream since we can't be sure that we'll
                    // find anything useful.
                    break;
                }
                MkvElement::Attachments => {
                    // Only one attachments element per segment is expected.
                    if attachments.is_some() {
                        log::warn!("unexpected attachments element");
                    }
                    attachments = Some(it.read_master_element::<AttachmentsElement>()?);
                }
                MkvElement::Chapters => {
                    // Only one chapters element per segment is expected.
                    if chapters.is_some() {
                        log::warn!("unexpected chapters element");
                    }
                    chapters = Some(it.read_master_element::<ChaptersElement>()?);
                }
                other => {
                    log::debug!("top-level scan ignored element {other:?}");
                }
            }
        }

        if is_seekable {
            // All elements preceeding the element iterator's current position have already been
            // read and do not need to be revisited.
            seek_positions.retain(|sp| sp.1 >= it.pos());
            // Make sure we don't jump backwards unnecessarily.
            seek_positions.sort_by_key(|sp| sp.1);

            for (_, pos) in seek_positions {
                // Ascend back to the segment element.
                it.pop_elements_upto(MkvElement::Segment)?;

                // Seek the iterator to the child element.
                it.seek_to_child(pos)?;

                // Resume iteration.
                let element_type = match it.next_header()? {
                    Some(header) => header.element_type(),
                    _ => continue,
                };

                // Safety: The element type or position may be incorrect. The element iterator will
                // validate the type (as declared in the header) of the element at the seeked
                // position against the element type asked to be read.
                match element_type {
                    MkvElement::Tracks => {
                        segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                    }
                    MkvElement::Info => {
                        info = Some(it.read_master_element::<InfoElement>()?);
                    }
                    MkvElement::Tags => {
                        // Multiple tags element per segment allowed.
                        tags.push(it.read_master_element::<TagsElement>()?);
                    }
                    MkvElement::Cues => {
                        cues = Some(it.read_master_element::<CuesElement>()?);
                    }
                    MkvElement::Attachments => {
                        // Only one attachments element per segment is expected.
                        if attachments.is_some() {
                            log::warn!("unexpected attachments element after meta seek");
                        }
                        attachments = Some(it.read_master_element::<AttachmentsElement>()?);
                    }
                    MkvElement::Chapters => {
                        // Only one chapters element per segment is expected.
                        if chapters.is_some() {
                            log::warn!("unexpected chapters element after meta seek");
                        }
                        chapters = Some(it.read_master_element::<ChaptersElement>()?)
                    }
                    _ => (),
                }
            }
        }

        let segment_tracks =
            segment_tracks.ok_or(Error::DecodeError("mkv: missing Tracks element"))?;

        // If seekable, seek to the start of the first cluster, if known, or the start of the
        // segment. If unseekable, the element iterator is already positioned at the start of the
        // first cluster.
        let first_cluster_pos = current_cluster.as_ref().map(|cluster| cluster.start).unwrap_or(0);

        if is_seekable {
            it.seek_to_child(first_cluster_pos)?;
            let _ = it.next_header()?;
        }

        // Descend into the cluster.
        it.push_element()?;

        let read_to = it.pos();

        let info = info.ok_or(Error::DecodeError("mkv: missing Info element"))?;

        // Create a hashmap of all per-target tags (edition, chapter, & attachment tags).
        let mut per_target_tags: TargetTagsMap = Default::default();

        segment_tracks.get_target_uids(&mut per_target_tags);

        if let Some(chapters) = &chapters {
            chapters.get_target_uids(&mut per_target_tags);
        }
        if let Some(attachments) = &attachments {
            attachments.get_target_uids(&mut per_target_tags);
        }

        // Begin with externally provided metadata and chapters.
        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Post-process all tag elements into metadata revisions, while also collecting per-target
        // tags.
        let is_video = segment_tracks.tracks.as_ref().iter().any(|t| t.video.is_some());
        for tag in tags {
            metadata.push(tag.into_metadata(&mut per_target_tags, is_video));
        }

        // Post-process chapters element.
        let chapters = chapters
            .map(|chapters| chapters.into_chapter_group(&mut per_target_tags))
            .unwrap_or(opts.external_data.chapters);

        // Process attachments element.
        let attachments = attachments
            .map(|attachments| attachments.into_attachments(&mut per_target_tags))
            .unwrap_or_default();

        // Should TimeBase use a u64/u64 rational?
        let timestamp_scale: NonZero<u32> = info
            .timestamp_scale
            .try_into()
            .map_err(|_| Error::Unsupported("mkv: timestamp scale too large (report this)"))?;

        // Reduce the timebase to reduce the chance of overflows later.
        let time_base = TimeBase::new(
            timestamp_scale,
            NonZero::new(1_000_000_000).expect("1_000_000_000 is non-zero"),
        )
        .reduce();

        let mut tracks = Vec::new();
        let mut track_states = HashMap::new();

        for track in segment_tracks.tracks {
            // A Track tick is `TimestampScale * TrackTimestampScale` nanoseconds, rounded to a whole
            // nanosecond: the track's timebase before it is reduced, so a fractional scale is kept.
            // A scale that is not positive, or not a number, gives no nanoseconds (`as` saturates).
            let tick_ns = (f64::from(timestamp_scale.get()) * track.track_timestamp_scale).round();

            let track_time_base = TimeBase::try_new(tick_ns as u32, 1_000_000_000)
                .filter(|_| tick_ns <= f64::from(u32::MAX))
                .ok_or(Error::DecodeError("mkv: track timebase is invalid"))?
                .reduce();

            let codec_delay = track.codec_delay.into_track_ticks_rounded(track_time_base);

            // Create the track state. Opus tracks are trimmed by the reader (see `OpusTrim`).
            let state = TrackState {
                // TODO: This should be 64-bit, but track IDs are 32-bit.
                track_num: u32::try_from(track.number.get())
                    .map_err(|_| Error::Unsupported("mkv: track number too large (report this)"))?,
                default_frame_duration: track.default_duration,
                codec_delay,
                track_time_base,
                track_timestamp_scale: track.track_timestamp_scale,
                seek_pre_roll: track.seek_pre_roll.into_track_ticks_rounded_up(track_time_base),
                opus: (track.codec_id == "A_OPUS")
                    .then(|| OpusTrim::new(track.codec_delay, codec_delay, track_time_base)),
                last_read: None,
            };

            // Create the track.
            let mut tr = Track::new(state.track_num);

            tr.with_time_base(track_time_base);

            if let Some(lang_bcp47) = &track.lang_bcp47 {
                tr.with_language(lang_bcp47);
            }
            else {
                tr.with_language(&track.lang);
            }

            tr.with_flags(track.flags);

            if let Some(codec_params) = make_track_codec_params(track)? {
                tr.with_codec_params(codec_params);
            }

            tracks.push(tr);
            track_states.insert(state.track_num, state);
        }

        // Populate media information.
        let mut media_info = MediaInfo::new();

        media_info.with_time_base(time_base);

        if let Some(duration) = info.duration {
            media_info.with_duration(Duration::new(duration.get().round() as u64));
        }

        Ok(Self {
            iter: it,
            segment_state,
            segment_pos,
            first_cluster_pos,
            is_seekable,
            media_info,
            tracks,
            track_states,
            attachments,
            chapters,
            metadata,
            cues,
            clusters: Vec::new(),
            current_cluster,
            frames: VecDeque::new(),
            read_to,
            read_all_before: true,
        })
    }

    /// Skip the frames before the frame of the track `track_id` that holds `ts`: the first frame of
    /// the track that ends at or after `ts`. A frame whose duration is unknown (no BlockDuration, no
    /// DefaultDuration, none from the codec) ends where the track's next frame that begins later
    /// begins, so it is held until that frame, a cluster that begins after `ts`, or the end is read.
    /// `required_ts` is the timestamp the seek was asked for, which `ts` precedes by the seek
    /// pre-roll. If `behind_if_after`, the frame of the track the reader read past last, of unknown
    /// duration, may hold `required_ts`: it does, and the target lies behind the reader, if the
    /// track's next frame begins after `required_ts`. `before` is the position of a cluster before
    /// the one whose start the reader reads from, where the frame that holds `ts` may lie: if the
    /// reader reads no frame of the track that begins at or before `ts` before one that begins
    /// after it, a cluster that begins after it or the end, it reads from that cluster, once.
    fn seek_track_by_ts_forward(
        &mut self,
        track_id: u32,
        ts: Timestamp,
        required_ts: Timestamp,
        mut behind_if_after: bool,
        mut before: Option<u64>,
    ) -> Result<SeekedTo> {
        // The index of the next frame to look at. While it is not 0, the frame at the front of the
        // queue is held: a frame of the track that begins before `ts`, of unknown duration. The
        // frames after it are kept with it, as they follow it if it holds `ts`.
        let mut next = 0;

        let actual_ts = 'out: loop {
            while let Some(frame) = self.frames.get(next) {
                let (track_num, pts, dur) = (frame.track_num, frame.pts.into_ts(), frame.dur);

                if next > 0 {
                    let held = self.frames[0].pts.into_ts();

                    if track_num != track_id || pts <= held {
                        // A frame of another track, or one laced with the held frame (it begins
                        // where the held frame does).
                        next += 1;
                    }
                    else if pts > ts {
                        // The held frame ends after `ts`, where this frame begins: it holds `ts`.
                        break 'out held;
                    }
                    else {
                        // The held frame ends at or before `ts`. Look at this frame as the first.
                        self.drop_frames(next);
                        next = 0;
                    }
                    continue;
                }

                if track_num != track_id {
                    self.drop_frames(1);
                    continue;
                }

                // The track's first frame read decides on `before`: if it begins after `ts`, the
                // frame that holds `ts` may lie in a cluster before. Read from there.
                if let Some(pos) = before.take().filter(|_| pts > ts) {
                    self.read_from_cluster(pos, None)?;
                    continue;
                }

                if behind_if_after {
                    if pts > required_ts {
                        return seek_error(SeekErrorKind::ForwardOnly);
                    }
                    behind_if_after = false;
                }

                if dur.get() == 0 && pts < ts {
                    next = 1;
                    continue;
                }

                let end = pts
                    .checked_add(dur.into_dur())
                    .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                if end >= ts {
                    break 'out pts;
                }

                self.drop_frames(1);
            }

            let read = self.next_element()?;

            // No frame of the track read yet, and none read on begins at or before `ts` (at the end,
            // or in a cluster that begins after it): the frame that holds `ts` may lie in a cluster
            // before. Read from there.
            if let Some(pos) = before.take_if(|_| !read || self.cluster_begins_after(track_id, ts))
            {
                self.read_from_cluster(pos, None)?;
                continue;
            }

            if !read {
                // There are no more elements. The track's last frame, held or read past, holds a
                // timestamp the Segment does not end before.
                if next > 0 && !self.ends_before(track_id, ts) {
                    break 'out self.frames[0].pts.into_ts();
                }
                if behind_if_after && !self.ends_before(track_id, required_ts) {
                    return seek_error(SeekErrorKind::ForwardOnly);
                }
                return seek_error(SeekErrorKind::OutOfRange);
            }

            // The track's frames in a cluster that begins after a timestamp begin after it too, so
            // the track's frame held, or read past last, holds it.
            if next > 0 && self.cluster_begins_after(track_id, ts) {
                break 'out self.frames[0].pts.into_ts();
            }
            if behind_if_after && self.cluster_begins_after(track_id, required_ts) {
                return seek_error(SeekErrorKind::ForwardOnly);
            }
        };

        Ok(SeekedTo { track_id, required_ts, actual_ts })
    }

    /// Whether the frames of the track `track_id` in the cluster being read begin after `ts`: the
    /// cluster begins after `ts`. (A block's timestamp relative to its cluster's is negative only
    /// where frames are reordered.)
    fn cluster_begins_after(&self, track_id: u32, ts: Timestamp) -> bool {
        let Some(cluster_ts) = self.current_cluster.and_then(|cluster| cluster.timestamp)
        else {
            return false;
        };

        let state = &self.track_states[&track_id];
        let begin = calculate_block_pts(cluster_ts, SignedTrackTicks::default(), state);

        begin.is_some_and(|begin| begin.into_ts() > ts)
    }

    /// Drop the first `n` frames of the queue, which the reader reads past.
    fn drop_frames(&mut self, n: usize) {
        for frame in self.frames.drain(..n) {
            if let Some(state) = self.track_states.get_mut(&frame.track_num) {
                state.last_read = Some((frame.pts.into_ts(), frame.dur));
            }
        }
    }

    /// Whether the Segment, if it states its duration, ends before the timestamp `ts` of the track
    /// `track_id`.
    fn ends_before(&self, track_id: u32, ts: Timestamp) -> bool {
        let Some(duration) = self.media_info.duration
        else {
            return false;
        };

        let scale = self.track_states[&track_id].track_timestamp_scale;
        let end = SegmentTicks::from(duration.get()).into_track_ticks(scale).try_into_signed();

        end.is_some_and(|end| end.into_ts() < ts)
    }

    fn seek_track_by_ts_atomic(&mut self, id: u32, ts: Timestamp) -> Result<SeekedTo> {
        // A seek on a source that can seek moves the reader. Save the iterator and cluster states to
        // restore in case of an error, and set the frames still queued from the block read last
        // aside, to restore too: they do not follow where the reader lands. On a source that can't
        // seek, a seek reads on from where the reader is, and a failed seek can't return: the reader
        // reads on from where the seek stopped.
        let saved = self.is_seekable.then(|| {
            let frames = std::mem::take(&mut self.frames);
            (self.iter.save_state(), self.current_cluster, self.read_all_before, frames)
        });

        match self.seek_track_by_ts(id, ts) {
            Err(err) => {
                // Restore saved iterator and cluster states, and the frames set aside.
                if let Some((iter_state, cluster_state, read_all_before, frames)) = saved {
                    self.iter.restore_state(iter_state)?;
                    self.current_cluster = cluster_state;
                    self.read_all_before = read_all_before;
                    self.frames = frames;
                }
                Err(err)
            }
            value => {
                // The reader moved, so the frames read next are not those that followed the frames
                // read before.
                for state in self.track_states.values_mut() {
                    if let Some(opus) = &mut state.opus {
                        opus.relocate();
                    }
                }
                value
            }
        }
    }

    fn seek_track_by_ts(&mut self, id: u32, required_ts: Timestamp) -> Result<SeekedTo> {
        log::debug!("seeking track_id={id} to ts={required_ts}");

        let state = &self.track_states[&id];
        let (pre_roll, track_timestamp_scale) = (state.seek_pre_roll, state.track_timestamp_scale);

        // Decoding begins the seek pre-roll before the required timestamp, so that the decoded
        // data is valid from the required timestamp on.
        let ts = required_ts.saturating_sub(pre_roll.into_dur());

        // A source that can't seek is read on from where the reader is. That reaches the target
        // unless the reader has read past the frame of the track that holds it: the track's frame
        // the reader read past last must end at or before the target, and if its duration is
        // unknown, the track's next frame shows where it ends. The seek pre-roll is read as far as
        // the reader has not read past it.
        if !self.is_seekable {
            let mut behind_if_after = false;

            if let Some((pts, dur)) = self.track_states[&id].last_read {
                let behind = if dur.get() > 0 {
                    required_ts < pts.saturating_add(dur.into_dur())
                }
                else {
                    match self.frames.iter().find(|frame| frame.track_num == id) {
                        Some(frame) => required_ts < frame.pts.into_ts(),
                        None if required_ts < pts => true,
                        None => {
                            // The track's next frame is not read yet.
                            behind_if_after = true;
                            false
                        }
                    }
                };

                if behind {
                    return seek_error(SeekErrorKind::ForwardOnly);
                }
            }

            return self.seek_track_by_ts_forward(id, ts, required_ts, behind_if_after, None);
        }

        // Read from the cluster of the cue point at or before the timestamp or, without cues, from
        // the last cluster read that begins at or before it. Unless the cue point is the track's,
        // the frame of the track that holds the timestamp may lie in a cluster before that one: a
        // muxer may store the audio frame that spans a video keyframe at the end of the cluster
        // before the keyframe's (FFmpeg does, for every audio track but one). `before` is a
        // cluster before it to read from then.
        let (cluster_pos, cued_block_pos, before) = match &self.cues {
            Some(cues) => {
                // Cue points store unsigned timestamps in Segment ticks while the timestamp being
                // seeked to is in signed Track ticks. Convert the cue points' timestamps into the
                // track's ticks to compare them. If the required timestamp is negative, then this
                // is an error because cue points only contain unsigned timestamps.
                if required_ts.is_negative() {
                    return seek_error(SeekErrorKind::OutOfRange);
                }

                let ts = TrackTicks::from(ts.max(Timestamp::ZERO).get() as u64);

                // The cue points are sorted by time (`CuesElement::read`).
                let after = cues.points.partition_point(|cue_point| {
                    cue_point.time.into_track_ticks(track_timestamp_scale) <= ts
                });

                // Read from the cluster of the last cue point at or before the timestamp. If the
                // timestamp precedes every cue point, read from the first cluster: the cue points
                // may index another track, so the track's blocks may begin before the first of
                // them.
                match after.checked_sub(1) {
                    Some(last) => {
                        let cue_point = &cues.points[last];
                        let positions = &cue_point.positions;

                        log::debug!(
                            "found cue point: track_id={}, ts={}, seg_pos={}",
                            positions.track.get(),
                            cue_point.time,
                            positions.cluster_pos
                        );

                        // A cue point of the track being seeked leads to its block: the track's
                        // frames before that block end by the cue point's time. In the cluster of
                        // another track's cue point, frames of the track that hold the timestamp
                        // may precede the cued block, so that cluster is read from its start, or
                        // lie in a cluster before: the cluster of an earlier cue point, or the
                        // first.
                        if positions.track.get() == u64::from(id) {
                            (positions.cluster_pos, positions.cluster_rel_pos, None)
                        }
                        else {
                            let before = cues.points[..last]
                                .iter()
                                .rev()
                                .map(|cue_point| cue_point.positions.cluster_pos)
                                .chain([self.first_cluster_pos])
                                .find(|&pos| pos < positions.cluster_pos);

                            (positions.cluster_pos, None, before)
                        }
                    }
                    None => {
                        log::debug!("no cue point at or before ts={ts}, reading the first cluster");
                        (self.first_cluster_pos, None, None)
                    }
                }
            }
            None => {
                // The clusters read are indexed in order from the first (`next_element`), so the
                // last cluster indexed that begins at or before the timestamp is the last cluster
                // read that does, and the cluster indexed before that one is the cluster before
                // it. If none does, read from the first cluster.
                let ts = TrackTicks::from(ts.max(Timestamp::ZERO).get() as u64);

                let after = self.clusters.partition_point(|&(time, _)| {
                    time.into_track_ticks(track_timestamp_scale) <= ts
                });
                let cluster_pos =
                    self.clusters[..after].last().map_or(self.first_cluster_pos, |&(_, pos)| pos);
                let before = after.checked_sub(2).map(|i| self.clusters[i].1);

                log::debug!("no cues, reading from the cluster at seg_pos={cluster_pos}");
                (cluster_pos, None, before)
            }
        };

        self.read_from_cluster(cluster_pos, cued_block_pos)?;

        // Seek to exact block.
        self.seek_track_by_ts_forward(id, ts, required_ts, false, before)
    }

    /// Move the reader to the cluster at `cluster_pos` (relative to the Segment element's data),
    /// with no frames queued: to its start or, if `cued_block_pos` leads to a block of it, to that
    /// block once the cluster's Timestamp element is read.
    fn read_from_cluster(&mut self, cluster_pos: u64, cued_block_pos: Option<u64>) -> Result<()> {
        // Ascend back to the segment element. If the end of the segment was reached, the iterator
        // has left the segment element, so return into it.
        match self.iter.pop_elements_upto(MkvElement::Segment) {
            Err(EbmlError::NotAnAncestor) => {
                self.iter.restore_state(self.segment_state.clone())?;
            }
            result => result?,
        }

        // Seek to the specific cluster element.
        self.iter.seek_to_child(cluster_pos)?;

        // Resume iteration.
        let cluster = match self.iter.next_header()? {
            // The seeked element is a cluster.
            Some(header) if header.element_type() == MkvElement::Cluster => header,
            // The seeked element is not a cluster or there were no more elements at the cue
            // position. The cue point was malformed.
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        // Whether every element before the cluster has been read.
        let read_before = cluster.pos() <= self.read_to;

        // Descend into the cluster element.
        self.iter.push_element()?;

        self.read_all_before = read_before;

        // While the first frame of an Opus track has not been read, a cluster before which every
        // element has been read is read from its start, not from the cue point's block: the track's
        // first frame may precede that block, and a frame is known to be the track's first only if
        // every element before it has been read.
        let from_start = read_before
            && self
                .track_states
                .values()
                .any(|state| state.opus.as_ref().is_some_and(|opus| !opus.knows_start()));

        // Otherwise, if a cluster relative position leads to the block, seek to the exact simple
        // block or block group element, once the cluster's timestamp is read.
        let mut timestamp = None;

        if let Some(cued_block_pos) = cued_block_pos.filter(|_| !from_start) {
            timestamp = self.read_cluster_timestamp()?;

            if timestamp.is_some() {
                self.iter.seek_to_child(cued_block_pos)?;
                self.read_all_before = self.iter.pos() <= self.read_to;
            }
            else {
                // The cluster states no timestamp before its blocks. Read it from its start.
                self.iter.seek_to_child(0)?;
            }
        }

        // Update the current cluster metadata. Unless read above, the cluster's timestamp is read
        // from its Timestamp element: a cue point states the time of its block.
        self.current_cluster = Some(ClusterState { timestamp, start: cluster_pos });
        self.frames.clear();

        Ok(())
    }

    /// Read the children of the cluster the iterator has descended into, up to its Timestamp
    /// element, which comes before its blocks (after a CRC-32 or Void element, if any), and return
    /// the cluster's timestamp. Returns `None` if the cluster states no timestamp before a block,
    /// or before its end.
    fn read_cluster_timestamp(&mut self) -> Result<Option<SegmentTicks>> {
        while let Some(child) = self.iter.next_header()? {
            match child.element_type() {
                MkvElement::Timestamp => return Ok(self.iter.read_u64()?.map(SegmentTicks::from)),
                MkvElement::SimpleBlock | MkvElement::BlockGroup => return Ok(None),
                // Other leading children (CRC-32, Void, Position, PrevSize) are skipped.
                _ => (),
            }
        }

        Ok(None)
    }

    fn next_element(&mut self) -> Result<bool> {
        match self.iter.next_header()? {
            None => {
                // The EBML iterator has consumed all child elements at the current level of the
                // document.
                match self.iter.parent() {
                    None => {
                        // The parent is the document. The media has ended.
                        return Ok(false);
                    }
                    Some(parent) if parent.element_type() == MkvElement::Cluster => {
                        // The parent was a cluster element. Reset the cluster state.
                        self.current_cluster = None;
                    }
                    _ => (),
                }

                // Ascend to its parent.
                self.iter.pop_element()?;
            }
            Some(child) => {
                match child.element_type() {
                    // Cluster element.
                    MkvElement::Cluster => {
                        let start = child.pos() - self.segment_pos;
                        self.current_cluster = Some(ClusterState { timestamp: None, start });

                        // Descend into the cluster.
                        self.iter.push_element()?;
                    }
                    // Children of a cluster element.
                    MkvElement::Timestamp => {
                        // Cluster timestamp element.
                        match self.current_cluster.as_mut() {
                            Some(cc) => {
                                cc.timestamp = self.iter.read_u64()?.map(SegmentTicks::from);

                                // Without cues, on a source that can seek, index a cluster read
                                // past those indexed. The reader reads on from the first cluster
                                // or from an indexed one, so the index holds every cluster from
                                // the first to the last it holds.
                                let index = self.cues.is_none()
                                    && self.is_seekable
                                    && self.clusters.last().is_none_or(|&(_, pos)| pos < cc.start);

                                if let Some(timestamp) = cc.timestamp.filter(|_| index) {
                                    self.clusters.push((timestamp, cc.start));
                                }
                            }
                            _ => log::warn!("expected to have cluster"),
                        }
                    }
                    block_type @ (MkvElement::SimpleBlock | MkvElement::BlockGroup) => {
                        // Get the current cluster information.
                        let Some(cluster) = self.current_cluster.as_ref()
                        else {
                            log::warn!("expected to have cluster");
                            return Ok(true);
                        };

                        // Get the cluster timestamp.
                        let Some(cluster_ts) = cluster.timestamp
                        else {
                            log::warn!("missing cluster timestamp");
                            return Ok(true);
                        };

                        // Get block data, duration, and discard padding.
                        let (data, duration, discard_padding) = match block_type {
                            MkvElement::SimpleBlock => (self.iter.read_binary()?, None, None),
                            MkvElement::BlockGroup => {
                                let group = self.iter.read_master_element::<BlockGroupElement>()?;
                                (group.data, group.duration, group.discard_padding)
                            }
                            _ => unreachable!(),
                        };

                        let first_frame = self.frames.len();

                        // Extract frames.
                        if !extract_frames(
                            &data,
                            duration,
                            discard_padding,
                            cluster_ts,
                            &self.track_states,
                            &mut self.frames,
                        )? {
                            warn!("pts for block is too large");
                            return Ok(false);
                        }

                        // Learn where an Opus track starts: while every block before this one has
                        // been read, the first block of a track that is read is its first block.
                        if self.read_all_before {
                            if let Some(frame) = self.frames.get(first_frame) {
                                let state = self.track_states.get_mut(&frame.track_num);
                                if let Some(opus) = state.and_then(|state| state.opus.as_mut()) {
                                    opus.observe(frame.pts.into_ts().get());
                                }
                            }
                        }
                    }
                    // All other elements.
                    other => {
                        log::debug!("ignored element {other:?}");
                    }
                }
            }
        }

        // The element has been read.
        if self.read_all_before {
            self.read_to = self.read_to.max(self.iter.pos());
        }

        Ok(true)
    }
}

impl ProbeableFormat<'_> for MkvReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(MkvReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            MKV_FORMAT_INFO,
            &["webm", "mkv"],
            &["video/webm", "video/x-matroska"],
            &[b"\x1A\x45\xDF\xA3"] // Top-level element Ebml element
        )]
    }
}

impl FormatReader for MkvReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MKV_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Time { time, track_id } => {
                let track = match track_id {
                    Some(id) => self.tracks.iter().find(|track| track.id == id),
                    None => self.tracks.first(),
                };
                let track = track.ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?;
                let tb = track.time_base.expect("track always has a timebase");
                let ts = match tb.calc_timestamp(time) {
                    Some(ts) => ts,
                    None => {
                        warn!("seek ts is too large");
                        return seek_error(SeekErrorKind::OutOfRange);
                    }
                };
                let track_id = track.id;
                self.seek_track_by_ts_atomic(track_id, ts)
            }
            SeekTo::Timestamp { ts, track_id } => {
                if self.tracks.iter().any(|t| t.id == track_id) {
                    self.seek_track_by_ts_atomic(track_id, ts)
                }
                else {
                    seek_error(SeekErrorKind::InvalidTrack)
                }
            }
        }
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                let mut packet = Packet::new(
                    frame.track_num,
                    frame.pts.into_ts(),
                    frame.dur.into_dur(),
                    frame.data,
                );

                if let Some(state) = self.track_states.get_mut(&frame.track_num) {
                    state.last_read = Some((frame.pts.into_ts(), frame.dur));

                    // Trim the encoder delay and padding of an Opus track.
                    if let Some(opus) = &mut state.opus {
                        opus.trim(&mut packet, frame.opus);
                    }
                }

                return Ok(Some(packet));
            }

            if !self.next_element()? {
                // Reached the end of stream.
                return Ok(None);
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.iter.into_inner()
    }
}

impl Scoreable for MkvReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ReadEbml for MediaSourceStream<'_> {}

impl From<EbmlError> for Error {
    fn from(value: EbmlError) -> Self {
        // All non-IO EBML errors are mapped to a decode error.
        let msg = match value {
            EbmlError::IoError(err) => return Error::IoError(err),
            EbmlError::InvalidEbmlElementIdLength => "mkv (ebml): invalid ebml element id length",
            EbmlError::InvalidEbmlDataLength => "mkv (ebml): invalid ebml vint length",
            EbmlError::UnknownElement => "mkv (ebml): the element is unknown",
            EbmlError::UnknownElementDataSize => "mkv (ebml): the element data size is unknown",
            EbmlError::UnexpectedElement => "mkv (ebml): encountered an unexpected element",
            EbmlError::UnexpectedElementDataType => {
                "mkv (ebml): unexpected element data type for the operation"
            }
            EbmlError::UnexpectedElementDataSize => {
                "mkv (ebml): unexpected data size for the element's data type"
            }
            EbmlError::NoElement => "mkv (ebml): no current element",
            EbmlError::NoParent => "mkv (ebml): no parent element",
            EbmlError::NotAnAncestor => {
                "mkv (ebml): the element is not an ancestor of the current element"
            }
            EbmlError::Overrun => "mkv (ebml): the element was overrun when read",
            EbmlError::ExpectedMasterElement => "mkv (ebml): expected a master element",
            EbmlError::ExpectedNonMasterElement => "mkv (ebml): expected a non-master element",
            EbmlError::SeekOutOfRange => "mkv (ebml): the seek was out of range",
            EbmlError::BufferTooSmall => "mkv (ebml): the buffer is too small",
            EbmlError::MaximumDepthReached => "mkv (ebml): maximum ebml document depth reached",
            EbmlError::ElementError(err) => err,
        };
        Error::DecodeError(msg)
    }
}
