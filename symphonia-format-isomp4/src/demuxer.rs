// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_common::xiph::audio::opus::{self, OPUS_SAMPLE_RATE};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia_core::support_format;

use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ISOMP4;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::units::Time;

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::num::NonZero;
use std::sync::Arc;

use crate::atoms::{AtomError, AtomIterator, AtomType, ReadAtom};
use crate::atoms::{FtypAtom, MetaAtom, MoofAtom, MoovAtom, SidxAtom, TrakAtom};
use crate::edit::{Presentation, rescale};
use crate::stream::*;

use log::{debug, info, trace, warn};

const ISOMP4_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_ISOMP4,
    short_name: "isomp4",
    long_name: "ISO Base Media File Format",
};

/// The amount of audio to decode before the seek target to let an Opus decoder converge, 80 ms
/// (RFC 7845, §4.6), in frames at 48 kHz.
const OPUS_SEEK_PRE_ROLL: u64 = 80 * OPUS_SAMPLE_RATE as u64 / 1000;

/// The number of frames a sample of an audio track decodes to.
#[derive(Copy, Clone, Debug)]
enum DecodedLength {
    /// Read from the TOC of each Opus packet.
    Opus,
    /// Every sample decodes to the same number of frames (an AAC frame).
    Fixed(u64),
    /// The duration of the sample.
    Duration,
}

pub struct TrackState {
    /// The track number.
    track_num: usize,
    /// The track ID.
    track_id: u32,
    /// The current segment.
    cur_seg: usize,
    /// The current sample index relative to the track.
    next_sample: u32,
    /// The current sample byte position relative to the start of the track.
    next_sample_pos: u64,
    /// The media timescale.
    timescale: NonZero<u32>,
    /// For an audio track, the sample rate of the decoded audio.
    sample_rate: Option<NonZero<u32>>,
    /// The number of frames a sample decodes to.
    decoded_len: DecodedLength,
    /// For an audio track, the presentation of its media, if known: the one its edit list states,
    /// or else all of the media.
    presentation: Option<Presentation>,
    /// The least duration, in the media timescale, that a seek begins decoding before its target,
    /// whatever the roll recovery group states: Opus's 80 ms, or else 0.
    seek_pre_roll: u64,
}

impl TrackState {
    /// Make the state and the track of the track atom `trak`. The track's media ends at
    /// `media_end`, if its sample table states it.
    pub fn make(
        track_num: usize,
        trak: &TrakAtom,
        timespan: &TimeSpan,
        media_end: Option<u64>,
        movie_timescale: NonZero<u32>,
    ) -> (Self, Track) {
        let mut track = Track::new(trak.tkhd.id);

        // Create the codec parameters using the sample description atom.
        if let Some(codec_params) = trak.mdia.minf.stbl.stsd.make_codec_params() {
            track.with_codec_params(codec_params);
        }

        let timescale = timespan.timescale;

        // The rate of the decoded audio, and the number of frames each sample decodes to. An Opus
        // track always decodes at 48 kHz. An AAC sample decodes to a frame of the length its audio
        // specific config states, even if the duration of the last sample is shortened to its
        // valid frames, at the rate the config states. The sample entry may state another rate:
        // the SBR output rate under implicit SBR (as the media timescale does), or, from FFmpeg,
        // half of a rate above 65535 Hz.
        let (sample_rate, decoded_len) = match &track.codec_params {
            Some(CodecParameters::Audio(audio)) => match audio.codec {
                CODEC_ID_OPUS => (NonZero::new(OPUS_SAMPLE_RATE), DecodedLength::Opus),
                CODEC_ID_AAC => match audio
                    .extra_data
                    .as_deref()
                    .and_then(|asc| AudioSpecificConfig::read(asc).ok())
                    .filter(|asc| asc.samples > 0)
                {
                    Some(asc) => {
                        (NonZero::new(asc.sample_rate), DecodedLength::Fixed(asc.samples as u64))
                    }
                    None => (audio.sample_rate.and_then(NonZero::new), DecodedLength::Duration),
                },
                _ => (audio.sample_rate.and_then(NonZero::new), DecodedLength::Duration),
            },
            _ => (None, DecodedLength::Duration),
        };

        // An Opus decoder converges within 80 ms, so a seek begins decoding at least that long
        // before its target (RFC 7845, §4.6), as FFmpeg's MP4 reader states for every Opus track. A
        // roll recovery group may state a longer pre-roll, but not a shorter one. FFmpeg's states
        // none for the first 80 ms of the track, nor anywhere in a fragmented file.
        let seek_pre_roll = match (decoded_len, sample_rate) {
            (DecodedLength::Opus, Some(sample_rate)) => {
                rescale(OPUS_SEEK_PRE_ROLL, sample_rate, timescale)
            }
            _ => 0,
        };

        // The presentation of an audio track's media: the one its edit list states, or else all of
        // the media if its end is known. Neither presents past the end of the media.
        let presentation = sample_rate.and_then(|_| {
            trak.edts
                .as_ref()
                .and_then(|edts| edts.elst.as_ref())
                .and_then(|elst| Presentation::from_edit_list(elst, movie_timescale, timescale))
                .or_else(|| media_end.map(|_| Presentation::whole()))
                .map(|presentation| presentation.bounded_by(media_end))
        });

        // The presented duration excludes the trimmed frames.
        let duration = match presentation {
            Some(presentation) => match presentation.media_end() {
                Some(media_end) => media_end - presentation.media_start(),
                None => timespan.duration.get().saturating_sub(presentation.media_start()),
            },
            None => timespan.duration.get(),
        };

        // Populate timing information.
        track
            .with_time_base(TimeBase::from_recip(timespan.timescale))
            .with_duration(duration.into());

        // If the track is an audio track, and the timescale is equal to the sample rate, then the
        // number of frames is equal to the duration. This is the case for almost all audio tracks.
        // If not, there is no generic, low overhead, & precise way to determine the number of
        // frames.
        if sample_rate == Some(timescale) {
            track.with_num_frames(duration);
        }

        // The frames before the presentation are the encoder delay, and the frames after it are
        // padding.
        if let (Some(presentation), Some(sample_rate)) = (presentation, sample_rate) {
            track.with_start_ts(Timestamp::from(presentation.start_ts() as i64));

            let delay = rescale(presentation.media_start(), timescale, sample_rate);

            if let Ok(delay @ 1..) = u32::try_from(delay) {
                track.with_delay(delay);
            }

            // The padding is known from the sample table if each sample decodes to its duration,
            // or to a fixed number of frames.
            let stts = &trak.mdia.minf.stbl.stts;

            let decoded_end = match decoded_len {
                DecodedLength::Duration => Some(stts.total_duration),
                // The last sample is in the last entry that states any samples. It starts its
                // duration before the end of the table, and decodes to a whole frame.
                DecodedLength::Fixed(frame_len) => {
                    stts.entries.iter().rfind(|entry| entry.sample_count > 0).and_then(|last| {
                        let last_start = stts.total_duration - u64::from(last.sample_delta);
                        last_start.checked_add(rescale(frame_len, sample_rate, timescale))
                    })
                }
                DecodedLength::Opus => None,
            };

            if let (Some(media_end), Some(decoded_end)) = (presentation.media_end(), decoded_end) {
                let padding =
                    rescale(decoded_end.saturating_sub(media_end), timescale, sample_rate);

                if let Ok(padding @ 1..) = u32::try_from(padding) {
                    track.with_padding(padding);
                }
            }
        }

        let state = Self {
            track_num,
            track_id: trak.tkhd.id,
            cur_seg: 0,
            next_sample: 0,
            next_sample_pos: 0,
            timescale,
            sample_rate,
            decoded_len,
            presentation,
            seek_pre_roll,
        };

        (state, track)
    }

    /// Make the packet of the sample of the track at the media timestamp `ts` with the duration
    /// `dur`.
    ///
    /// The packet's timestamp is its presentation timestamp, and the frames it decodes to outside
    /// the presentation are trimmed.
    fn make_packet(&self, ts: Timestamp, dur: Duration, data: Box<[u8]>) -> Packet {
        let (Some(presentation), Some(sample_rate)) = (self.presentation, self.sample_rate)
        else {
            return Packet::new(self.track_id, ts, dur, data);
        };

        let ts = ts.get() as u64;

        // The duration of the frames the sample decodes to. The duration of the last sample of an
        // Opus or AAC track is shortened to its valid frames (for Opus, Opus in ISOBMFF, §4.3.4),
        // but it decodes to as many frames as any other.
        let decoded_frames = match self.decoded_len {
            DecodedLength::Opus => opus::packet_frames(&data).map(u64::from),
            DecodedLength::Fixed(frame_len) => Some(frame_len),
            DecodedLength::Duration => None,
        };

        let decoded = match decoded_frames {
            Some(frames) => rescale(frames, sample_rate, self.timescale),
            None => dur.get(),
        };

        let (trim_start, trim_end) = presentation.trims(ts, decoded);

        let pts = Timestamp::from(presentation.media_to_pts(ts));
        let mut packet =
            Packet::new(self.track_id, pts, Duration::new(decoded - trim_start - trim_end), data);

        // The trims are in decoded frames.
        packet.trim_start = Duration::new(rescale(trim_start, self.timescale, sample_rate));
        packet.trim_end = Duration::new(rescale(trim_end, self.timescale, sample_rate));

        packet
    }
}

/// Information regarding the next sample.
#[derive(Debug)]
struct NextSampleInfo {
    /// The track number of the next sample.
    track_num: usize,
    /// The media timestamp of the next sample.
    ts: Timestamp,
    /// The presentation time of the next sample, in seconds.
    time: Time,
    /// The duration of the next sample.
    dur: Duration,
    /// The segment containing the next sample.
    seg_idx: usize,
}

/// Information regarding a sample.
#[derive(Debug)]
struct SampleDataInfo {
    /// The position of the sample in the track.
    pos: u64,
    /// The length of the sample.
    len: u32,
}

/// A representation of time, defining a duration relative to a specific frequency
#[derive(Debug)]
pub struct TimeSpan {
    pub timescale: NonZero<u32>,
    pub duration: Duration,
}

impl Default for TimeSpan {
    fn default() -> Self {
        Self { timescale: NonZero::new(1).expect("1 is non-zero"), duration: Duration::ZERO }
    }
}

impl TimeSpan {
    pub fn new(timescale: NonZero<u32>, duration: Duration) -> Self {
        TimeSpan { timescale, duration }
    }
}

/// ISO Base Media File Format (MP4, M4A, MOV, etc.) demultiplexer.
///
/// `IsoMp4Reader` implements a demuxer for the ISO Base Media File Format.
pub struct IsoMp4Reader<'s> {
    iter: AtomIterator<MediaSourceStream<'s>>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// Segments of the movie. Sorted in ascending order by sequence number.
    segs: Vec<Box<dyn StreamSegment>>,
    /// State tracker for each track.
    track_states: Vec<TrackState>,
    /// Optional, movie extends atom used for fragmented streams.
    moov: Arc<MoovAtom>,
    /// All atoms of the stream have been iterated, so no more segments can be read.
    atoms_ended: bool,
}

impl<'s> IsoMp4Reader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // To get to beginning of the atom.
        mss.seek_buffered_rel(-4);

        let is_seekable = mss.is_seekable();

        let mut ftyp = None;
        let mut moov = None;

        // Get the total length of the stream, if possible.
        let total_len = if is_seekable {
            let pos = mss.pos();
            let len = mss.seek(SeekFrom::End(0))?;
            mss.seek(SeekFrom::Start(pos))?;
            info!("stream is seekable with len={len} bytes.");
            Some(len)
        }
        else {
            None
        };

        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Parse all atoms if the stream is seekable, otherwise parse all atoms up-to the mdat atom.
        let mut it = AtomIterator::new(mss, total_len);
        // Maps each track id to its cumulative duration (TimeSpan) as parsed from the segment
        // index.
        let mut sidx_timespans: HashMap<u32, TimeSpan> = HashMap::new();

        while let Some(header) = it.next_header()? {
            // Top-level atoms.
            match header.atom_type() {
                AtomType::FileType => {
                    ftyp = Some(it.read_atom::<FtypAtom>()?);
                }
                AtomType::Movie => {
                    moov = Some(it.read_atom::<MoovAtom>()?);
                }
                AtomType::SegmentIndex => {
                    let sidx = it.read_atom::<SidxAtom>()?;

                    // Calculate the total duration, per track, from the segment index atoms.
                    let sidx_timespan = sidx_timespans
                        .entry(sidx.reference_id)
                        .or_insert(TimeSpan::new(sidx.timescale, Duration::ZERO));

                    if sidx_timespan.timescale != sidx.timescale {
                        return unsupported_error(
                            "isomp4: different sidx timescale for the same track",
                        );
                    }

                    // Don't overflow.
                    // TODO: Duration should maybe be None since SIDX is non-authoritative.
                    sidx_timespan.duration = sidx_timespan
                        .duration
                        .checked_add(Duration::new(sidx.total_duration))
                        .ok_or(Error::DecodeError("isomp4: sidx total duration overflow"))?
                }
                AtomType::MediaData | AtomType::MovieFragment => {
                    // The mdat atom contains the codec bitstream data. For fragmented streams, a
                    // moof + mdat pair is required. If the ftyp and moov atoms have been read, then
                    // the top-level atom scan can exit here and begin playback immediately as an
                    // optimization. If not, then the scan must continue.
                    //
                    // The scan must also exit if the source is unseekable because in that case
                    // the format reader cannot skip past these atoms without dropping packets.
                    let is_playable = moov.is_some() && ftyp.is_some();

                    if is_playable || !is_seekable {
                        if !is_playable {
                            warn!("mp4 is not streamable.");
                        }
                        break;
                    }
                }
                AtomType::Meta => {
                    // Read the metadata atom and append it to the log.
                    let mut meta = it.read_atom::<MetaAtom>()?;

                    if let Some(rev) = meta.take_metadata() {
                        metadata.push(rev);
                    }
                }
                AtomType::Free => (),
                AtomType::Skip => (),
                _ => {
                    info!("skipping top-level atom: {:?}.", header.atom_type());
                }
            }
        }

        if ftyp.is_none() {
            return unsupported_error("isomp4: missing ftyp atom");
        }

        if moov.is_none() {
            return unsupported_error("isomp4: missing moov atom");
        }

        // If the top-level atom scan iterated across the entire source (e.g., if moov was the last
        // atom), then the iterator must return to the first moof or mdat atom. This is only
        // possible if the source is seekable. If it's not, then the media will be effectively
        // unplayable.
        if is_seekable && it.pending().is_none() {
            let mut mss = it.into_inner();
            mss.seek(SeekFrom::Start(0))?;

            it = AtomIterator::new(mss, total_len);

            while let Some(header) = it.next_header()? {
                if let AtomType::MovieFragment | AtomType::MediaData = header.atom_type() {
                    break;
                }
            }
        }

        // Fragments (moof + mdat pairs) are streamed. So if the pending atom is a moof, seek the
        // iterator to the start of the moof atom.
        if let Some(atom) = it.pending() {
            if atom.atom_type() == AtomType::MovieFragment {
                it.seek_atom_start()?;
            }
        }

        let mut moov = moov.expect("moov is checked for None above");

        if moov.is_fragmented() {
            if !sidx_timespans.is_empty() {
                info!("stream is segmented with a segment index.");
            }
            else {
                info!("stream is segmented without a segment index.");
            }
        }

        if let Some(rev) = moov.take_metadata() {
            metadata.push(rev);
        }

        // Create a track and track state for each Track (trak) atom.
        let mut tracks = Vec::with_capacity(moov.traks.len());
        let mut track_states = Vec::with_capacity(moov.traks.len());

        for (t, trak) in moov.traks.iter().enumerate() {
            // Determine the timespan of the track.
            let timespan = if moov.is_fragmented() {
                // If fragmented, prefer the duration from the sidx, if it is provided. Otherwise,
                // fallback to the mdhd.
                sidx_timespans
                    .get(&trak.tkhd.id)
                    .map(|sidx_tspan| TimeSpan::new(sidx_tspan.timescale, sidx_tspan.duration))
                    .unwrap_or_else(|| {
                        TimeSpan::new(trak.mdia.mdhd.timescale, trak.mdia.mdhd.duration.into())
                    })
            }
            else {
                // If non-fragmented, use the total duration (media timescale) from the track's
                // stts atom. Since edits are not currently supported, this is the duration of all
                // samples that will be yielded.
                //
                // TODO: Support edits. Once supported, prefer the tkhd duration.
                let duration = Duration::from(trak.mdia.minf.stbl.stts.total_duration);

                TimeSpan::new(trak.mdia.mdhd.timescale, duration)
            };

            // The end of the media, if the sample table states it. A fragmented file's samples
            // are in its fragments.
            let media_end =
                (!moov.is_fragmented()).then_some(trak.mdia.minf.stbl.stts.total_duration);

            let (track_state, track) =
                TrackState::make(t, trak, &timespan, media_end, moov.mvhd.timescale);

            tracks.push(track);
            track_states.push(track_state);
        }

        // The number of tracks specified in the moov atom must match the number in the mvex atom.
        if let Some(mvex) = &moov.mvex {
            if mvex.trexs.len() != moov.traks.len() {
                return decode_error("isomp4: mvex and moov track number mismatch");
            }
        }

        // The moov atom will be shared among all segments and the demuxer using an Arc.
        let moov = Arc::new(moov);

        let segs: Vec<Box<dyn StreamSegment>> = vec![Box::new(MoovSegment::new(moov.clone()))];

        // Populate media information.
        let mut media_info = MediaInfo::new();
        media_info.with_time_base(TimeBase::from_recip(moov.mvhd.timescale));
        media_info.with_duration(Duration::new(moov.mvhd.duration));

        Ok(IsoMp4Reader {
            iter: it,
            media_info,
            tracks,
            metadata,
            track_states,
            segs,
            moov,
            atoms_ended: false,
        })
    }

    /// Idempotently gets information regarding the next sample of the media stream. This function
    /// selects the next sample with the lowest presentation time of all tracks.
    fn next_sample_info(&self) -> Result<Option<NextSampleInfo>> {
        let mut earliest = None;

        // TODO: Consider returning samples based on lowest byte position in the track instead of
        // timestamp. This may be important if video tracks are ever decoded (i.e., DTS vs. PTS).

        for (state, track) in self.track_states.iter().zip(&self.tracks) {
            // Get the timebase of the track used to calculate the presentation time.
            let tb = track.time_base.expect("track always created with a timebase");

            // Get the next timestamp for the next sample of the current track. The next sample may
            // be in a future segment.
            for (seg_idx_delta, seg) in self.segs[state.cur_seg..].iter().enumerate() {
                // Try to get the timestamp for the next sample of the track from the segment.
                if let Some(timing) = seg.sample_timing(state.track_num, state.next_sample)? {
                    let Some(ts) = timing.ts.try_into().ok()
                    else {
                        return Ok(None);
                    };

                    // Calculate the presentation time on the timeline the track's packets are
                    // stamped with: the presentation of an audio track's media, if known, or else
                    // the media's.
                    let pts = match state.presentation {
                        Some(presentation) => Timestamp::from(presentation.media_to_pts(timing.ts)),
                        None => ts,
                    };

                    let Some(sample_time) = tb.calc_time(pts)
                    else {
                        return Ok(None);
                    };

                    // Compare the presentation time of the sample from this track to other tracks,
                    // and select the track with the earliest presentation time.
                    match earliest {
                        Some(NextSampleInfo { time, .. }) if time <= sample_time => {
                            // Earliest is less than or equal to the track's next sample
                            // presentation time. No need to update earliest.
                        }
                        _ => {
                            // Earliest was either None, or greater than the track's next sample
                            // presentation time. Update earliest.
                            earliest = Some(NextSampleInfo {
                                track_num: state.track_num,
                                ts,
                                time: sample_time,
                                dur: Duration::from(timing.dur),
                                seg_idx: seg_idx_delta + state.cur_seg,
                            });
                        }
                    }

                    // Either the next sample of the track had the earliest presentation time seen
                    // thus far, or it was greater than those from other tracks, but there is no
                    // reason to check samples in future segments.
                    break;
                }
            }
        }

        Ok(earliest)
    }

    fn consume_next_sample(&mut self, info: &NextSampleInfo) -> Result<Option<SampleDataInfo>> {
        // Get the track state.
        let track = &mut self.track_states[info.track_num];

        // Get the segment associated with the sample.
        let seg = &self.segs[info.seg_idx];

        // Get the sample data descriptor.
        let sample_data_desc = seg.sample_data(track.track_num, track.next_sample, false)?;

        // The sample base position in the sample data descriptor remains constant if the sample
        // followed immediately after the previous sample. In this case, the track state's
        // next_sample_pos is the position of the current sample. If the base position has jumped,
        // then the base position is the position of current the sample.
        let pos = if sample_data_desc.base_pos > track.next_sample_pos {
            sample_data_desc.base_pos
        }
        else {
            track.next_sample_pos
        };

        // Advance the track's current segment to the next sample's segment.
        track.cur_seg = info.seg_idx;

        // Advance the track's next sample number and position.
        track.next_sample += 1;
        track.next_sample_pos = pos + u64::from(sample_data_desc.size);

        Ok(Some(SampleDataInfo { pos, len: sample_data_desc.size }))
    }

    fn try_read_more_segments(&mut self) -> Result<bool> {
        // If all tracks ended in the last segment, then do not try to read anymore segments.
        //
        // Note, there will always be one segment because the moov atom was converted into a segment
        // when the reader was instantiated.
        let last_seg = self.segs.last().expect("always at least one segment from moov atom");
        if last_seg.all_tracks_ended() {
            return Ok(false);
        }

        // If all atoms were iterated, then there are no more segments. The reader has since been
        // used to read samples, so iterating atoms again would read from wherever the last sample
        // ended.
        if self.atoms_ended {
            return Ok(false);
        }

        // Continue iterating over atoms until a segment (a moof + mdat atom pair) is found. All
        // other atoms will be ignored.
        loop {
            let header = match self.iter.next_header() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                // If fragmented, an EOF is the only way to truly detect the end of stream.
                Err(AtomError::Other(Error::IoError(err)))
                    if self.moov.is_fragmented()
                        && err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                // Passthrough other errors.
                Err(err) => return Err(err.into()),
            };

            match header.atom_type() {
                AtomType::MediaData => {
                    return Ok(true);
                }
                AtomType::MovieFragment => {
                    let moof = self.iter.read_atom::<MoofAtom>()?;

                    // A moof segment can only be created if the media is fragmented.
                    if self.moov.is_fragmented() {
                        // Get the last segment.
                        let last_seg =
                            self.segs.last().expect("always at least one segment from moov atom");

                        // Create a new segment for the moof atom.
                        let seg = MoofSegment::new(moof, self.moov.clone(), last_seg.as_ref());

                        // Segments should have a monotonic sequence number.
                        if seg.sequence_num() <= last_seg.sequence_num() {
                            warn!("moof fragment has a non-monotonic sequence number.");
                        }

                        // Push the segment.
                        self.segs.push(Box::new(seg));
                    }
                    else {
                        return decode_error("isomp4: moof atom present without mvex atom");
                    }
                }
                _ => {
                    trace!("skipping atom: {:?}.", header.atom_type());
                }
            }
        }

        // If no atoms were returned above, then the end-of-stream has been reached.
        self.atoms_ended = true;

        Ok(false)
    }

    fn seek_track_by_time(&mut self, track_num: usize, time: Time) -> Result<SeekedTo> {
        // Convert time to timestamp for the track.
        if let Some(track) = self.tracks.get(track_num) {
            let tb = track.time_base.expect("track always created with a timebase");
            let ts = tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;
            self.seek_track_by_ts(track_num, ts)
        }
        else {
            seek_error(SeekErrorKind::Unseekable)
        }
    }

    fn seek_track_by_ts(&mut self, track_num: usize, ts: Timestamp) -> Result<SeekedTo> {
        debug!("seeking track_num={track_num} to frame_ts={ts}");

        struct SeekLocation {
            seg_idx: usize,
            sample_num: u32,
        }

        // Can only seek to 0 or positive timestamps.
        if ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The timestamp is a presentation timestamp. Get the media timestamp it presents.
        let presentation = self.track_states[track_num].presentation;
        let seek_pre_roll = self.track_states[track_num].seek_pre_roll;

        let media_ts = match presentation {
            Some(presentation) => presentation.pts_to_media(ts.get() as u64),
            None => ts.get() as u64,
        };

        let mut seg_skip = 0;

        let mut seek_loc = 'locate: loop {
            // Iterate over all segments and attempt to find the segment and sample number that
            // contains the desired timestamp. Skip segments already examined.
            for (seg_idx, seg) in self.segs.iter().enumerate().skip(seg_skip) {
                if let Some(sample_num) = seg.ts_sample(track_num, media_ts)? {
                    break 'locate SeekLocation { seg_idx, sample_num };
                }

                // Mark the segment as examined.
                seg_skip = seg_idx + 1;
            }

            // Otherwise, try to read more segments from the stream.
            if !self.try_read_more_segments()? {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        };

        // If samples before the sample must be decoded for it to be decoded correctly (the
        // pre-roll), begin decoding at the first of them. The roll recovery group states how many
        // there are, and the track's seek pre-roll how long before the target they begin at the
        // least. The longer pre-roll of the two is taken.
        let mut sample_num = match &self.moov.traks[track_num].mdia.minf.stbl.roll {
            Some(roll) => seek_loc.sample_num.saturating_sub(roll.pre_roll(seek_loc.sample_num)),
            None => seek_loc.sample_num,
        };

        if seek_pre_roll > 0 {
            let start = media_ts.saturating_sub(seek_pre_roll);

            // The sample holding the start of the pre-roll, which may be in an earlier segment.
            if let Some(seg) = self.segs[..=seek_loc.seg_idx]
                .iter()
                .rfind(|seg| seg.track_ts_range(track_num).contains(&start))
            {
                if let Some(start_sample_num) = seg.ts_sample(track_num, start)? {
                    sample_num = sample_num.min(start_sample_num);
                }
            }
        }

        if sample_num < seek_loc.sample_num {
            // The pre-roll may begin in an earlier segment.
            if let Some(seg_idx) = self.segs[..=seek_loc.seg_idx]
                .iter()
                .rposition(|seg| seg.track_sample_range(track_num).contains(&sample_num))
            {
                seek_loc = SeekLocation { seg_idx, sample_num };
            }
        }

        let seg = &self.segs[seek_loc.seg_idx];

        // Get the sample timing.
        let timing = match seg.sample_timing(track_num, seek_loc.sample_num)? {
            Some(t) => t,
            None => return seek_error(SeekErrorKind::OutOfRange),
        };

        // Try to convert the sample timing to a presentation timestamp.
        let actual_ts = match presentation {
            Some(presentation) => Timestamp::from(presentation.media_to_pts(timing.ts)),
            None => match Timestamp::try_from(timing.ts) {
                Ok(ts) => ts,
                _ => return seek_error(SeekErrorKind::OutOfRange),
            },
        };

        // Get the sample information.
        let data_desc = seg.sample_data(track_num, seek_loc.sample_num, true)?;

        // Update the track's next sample information to point to the seeked sample.
        let track = &mut self.track_states[track_num];

        track.cur_seg = seek_loc.seg_idx;
        track.next_sample = seek_loc.sample_num;
        track.next_sample_pos = data_desc.base_pos
            + match data_desc.offset {
                Some(o) => o,
                None => return seek_error(SeekErrorKind::OutOfRange),
            };

        debug!(
            "seeked track_num={} (track_id={}) to packet_ts={} (delta={})",
            track_num,
            track.track_id,
            actual_ts,
            actual_ts.saturating_delta(ts),
        );

        Ok(SeekedTo { track_id: track.track_id, required_ts: ts, actual_ts })
    }
}

impl Scoreable for IsoMp4Reader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for IsoMp4Reader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(IsoMp4Reader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            ISOMP4_FORMAT_INFO,
            &["mp4", "m4a", "m4p", "m4b", "m4r", "m4v", "mov"],
            &["video/mp4", "audio/m4a"],
            &[b"ftyp"] // Top-level atoms
        )]
    }
}

impl FormatReader for IsoMp4Reader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ISOMP4_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        // Get the index of the track with the next-nearest (minimum) timestamp.
        let next_sample_info = loop {
            // Using the current set of segments, try to get the next sample info.
            if let Some(info) = self.next_sample_info()? {
                break info;
            }
            else {
                // The inner reader of the atom iterator has been used/seeked around to read
                // packets, so resync the reader and iterator by seeking to the end of the current
                // pending atom. Under regular circumstances, no actual expensive seek operation is
                // performed since the reader should be at the end of the last iterated atom if we
                // are trying to read another.
                match self.iter.seek_atom_end() {
                    Ok(_) | Err(AtomError::NoPendingAtom) => (),
                    Err(_) => return decode_error("sync lost"),
                };

                // No more segments. If the stream is unseekable, it may be the case that there are
                // more segments coming. If the stream is seekable it might be fragmented and no
                // segments are found in the moov atom. Iterate atoms until a new segment is found
                // or the end-of-stream is reached
                if !self.try_read_more_segments()? {
                    return Ok(None);
                }
            }
        };

        // Get the position and length information of the next sample.
        let sample_info = match self.consume_next_sample(&next_sample_info)? {
            Some(s) => s,
            None => return decode_error("isomp4: failed to get next sample data"),
        };

        let data =
            self.iter.read_raw_boxed_slice_exact(sample_info.pos, sample_info.len as usize)?;

        let track = &self.track_states[next_sample_info.track_num];

        Ok(Some(track.make_packet(next_sample_info.ts, next_sample_info.dur, data)))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Timestamp { ts, track_id } => {
                // The seek timestamp is in timebase units specific to the selected track. Get the
                // selected track and use the timebase to convert the timestamp into time units so
                // that the other tracks can be seeked.
                if let Some((track_num, track)) =
                    self.tracks.iter().enumerate().find(|(_, track)| track.id == track_id)
                {
                    // Convert to time units.
                    let tb = match track.time_base {
                        Some(tb) => tb,
                        None => return seek_error(SeekErrorKind::Unseekable),
                    };
                    let time =
                        tb.calc_time(ts).ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                    // Seek all tracks excluding the primary track to the desired time.
                    for t in 0..self.track_states.len() {
                        if t != track_num {
                            self.seek_track_by_time(t, time)?;
                        }
                    }

                    // Seek the primary track and return the result.
                    self.seek_track_by_ts(track_num, ts)
                }
                else {
                    seek_error(SeekErrorKind::InvalidTrack)
                }
            }
            SeekTo::Time { time, track_id } => {
                // If provided, find the track number of the track with the desired track_id, or
                // default to the first track.
                let track_num = match track_id {
                    Some(id) => self
                        .tracks
                        .iter()
                        .position(|track| track.id == id)
                        .ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?,
                    None => 0,
                };

                // Seek all tracks excluding the selected track and discard the result.
                for t in 0..self.track_states.len() {
                    if t != track_num {
                        self.seek_track_by_time(t, time)?;
                    }
                }

                // Seek the primary track and return the result.
                self.seek_track_by_time(track_num, time)
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

impl ReadAtom for MediaSourceStream<'_> {}

impl From<AtomError> for Error {
    fn from(value: AtomError) -> Self {
        // Map all atom iteration errors to decode errors.
        let msg = match value {
            AtomError::InvalidAtomSize => "isomp4: invalid atom size",
            AtomError::InvalidUtf8 => "isomp4: invalid utf-8 string",
            AtomError::MaximumDepthReached => "isomp4: maximum recursion depth reached",
            AtomError::NoParentAtom => "isomp4: no parent atom",
            AtomError::NoPendingAtom => "isomp4: no atom pending read",
            AtomError::Overrun => "isomp4: overrun while reading atom",
            AtomError::SeekOutOfRange => "isomp4: out-of-bounds seek for a non-seekable stream",
            AtomError::UnexpectedEndOfAtom => "isomp4: unexpected end of atom",
            AtomError::UnexpectedPosition => "isomp4: unexpected position",
            AtomError::UnexpectedUnknownSizeAtom => "isomp4: unknown size atom has sized parent",
            AtomError::UnexpectedReadOperation => "isomp4: unexpected read operation",
            AtomError::UnknownAtomSize => "isomp4: unknown atom size",
            AtomError::Other(err) => return err,
        };
        Error::DecodeError(msg)
    }
}

// fn convert_timescale(
//     duration: u64,
//     src_timescale: NonZero<u32>,
//     dst_timescale: NonZero<u32>,
// ) -> Duration {
//     if src_timescale == dst_timescale {
//         return Duration::from(duration);
//     }
//     Duration::from(
//         ((duration as u128 * dst_timescale.get() as u128) / src_timescale.get() as u128) as u64,
//     )
// }
