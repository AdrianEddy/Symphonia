// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::formats::Track;
use symphonia_core::packet::{Packet, PacketBuilder};
use symphonia_core::units::{Duration, Timestamp};

use super::common::SideData;
use super::mappings::Mapper;
use super::mappings::{MapResult, PacketParser};
use super::page::Page;

use log::{debug, warn};

#[derive(Copy, Clone, Debug)]
struct Bound {
    /// The page sequence number.
    seq: u32,
    /// Indicates if this is the last page.
    is_last_page: bool,
    /// The start or end timestamp (depends on the type of bound).
    ts: Timestamp,
    /// The samples to discard from the start or end (depends on type of bound).
    discard: Duration,
}

#[derive(Copy, Clone, Debug)]
struct PageInfo {
    /// The page sequence number.
    seq: u32,
    /// Indicates if this is the last page.
    is_last_page: bool,
    /// The page's end timestamp derived from the absolute granule position.
    end_ts: Option<Timestamp>,
}

/// A page on which a packet ends, read at the end of a logical stream.
#[derive(Copy, Clone, Debug)]
struct EndingPage {
    /// The page sequence number.
    seq: u32,
    /// The end timestamp of the page before it on which a packet ends, if read: where the decoded
    /// data of the packets that end on this page begins.
    prev_end: Option<Timestamp>,
    /// The page's end timestamp.
    end_ts: Timestamp,
    /// The decoded duration of the packets that end on the page, if known.
    dur: Option<Duration>,
}

/// What the pages read so far at the end of a logical stream tell of its end.
#[derive(Default)]
pub struct InspectState {
    parser: Option<Box<dyn PacketParser>>,
    /// The packet before the next one was parsed. A packet's decoded duration can depend on the
    /// packet before it (Vorbis block lapping), so it is known only if that packet was parsed: not
    /// after a page that was lost or not read.
    history: bool,
    /// The sequence number of the last page read.
    seq: Option<u32>,
    /// A page after the first one read is missing, or a packet outgrew the longest read: what the
    /// pages read leave unknown of the end is not read from earlier pages either.
    lost: bool,
    /// The start of the packet the last page read left unfinished, or `None` if no packet was left
    /// unfinished or the packet began on a page not read.
    partial: Option<Vec<u8>>,
    /// The last page read on which a packet ends.
    last: Option<EndingPage>,
}

pub struct LogicalStream {
    mapper: Box<dyn Mapper>,
    packets: VecDeque<Packet>,
    part_buf: Vec<u8>,
    part_len: usize,
    prev_page_info: Option<PageInfo>,
    /// The sequence number of the stream's first page of data, if its headers were read when the
    /// physical stream started.
    data_seq: Option<u32>,
    start_bound: Option<Bound>,
    end_bound: Option<Bound>,
    /// The last page was read, but not every page before it that the end padding is read from,
    /// and none of the pages read is missing: earlier pages may hold the rest.
    end_needs_earlier: bool,
}

impl LogicalStream {
    const MAX_PACKET_LEN: usize = 16 * 1024 * 1024;

    pub fn new(mapper: Box<dyn Mapper>) -> Self {
        LogicalStream {
            mapper,
            packets: Default::default(),
            part_buf: Default::default(),
            part_len: 0,
            prev_page_info: None,
            data_seq: None,
            start_bound: None,
            end_bound: None,
            end_needs_earlier: false,
        }
    }

    /// Reset the logical stream after a page discontinuity.
    pub fn reset(&mut self) {
        self.part_len = 0;
        self.prev_page_info = None;
        self.packets.clear();
        self.mapper.reset();
    }

    /// Returns true if the stream is ready.
    pub fn is_ready(&self) -> bool {
        self.mapper.is_ready()
    }

    /// Get the maximum duration between two random access points.
    pub fn max_rap_period(&self) -> Duration {
        self.mapper.max_rap_period()
    }

    /// Get the `Track` for the logical stream.
    pub fn track(&self) -> &Track {
        self.mapper.track()
    }

    /// If known, returns whether the logical stream read the last page.
    pub fn has_read_last_page(&self) -> Option<bool> {
        self.prev_page_info.map(|info| info.is_last_page)
    }

    /// Reads a page.
    pub fn read_page(&mut self, page: &Page<'_>) -> Result<Vec<SideData>> {
        self.read_page_init(page, false)
    }

    /// Read a page. Specifying whether this is the initial bitstream page.
    pub fn read_page_init(&mut self, page: &Page<'_>, is_init_page: bool) -> Result<Vec<SideData>> {
        // Side data vector. This will not allocate unless data is pushed to it (normal case).
        let mut side_data = Vec::new();

        // If the last sequence number is available, detect non-monotonicity and discontinuities
        // in the stream. In these cases, clear any partial packet data.
        if let Some(last_ts) = &self.prev_page_info {
            if page.header.sequence < last_ts.seq {
                warn!("detected stream page non-monotonicity");
                self.part_len = 0;
            }
            else if page.header.sequence - last_ts.seq > 1 {
                warn!(
                    "detected stream discontinuity of {} page(s)",
                    page.header.sequence - last_ts.seq
                );
                self.part_len = 0;
            }
        }

        // Keep a copy of the previous page information.
        let prev_page_info = self.prev_page_info.take();

        // After a discontinuity, if this page is the first bitstream page of the stream, the
        // packets that follow are the first packets of the stream again.
        if prev_page_info.is_none()
            && self.start_bound.is_some_and(|b| b.seq == page.header.sequence)
        {
            self.mapper.rewind();
        }

        // Update with new page information.
        self.prev_page_info = Some(PageInfo {
            seq: page.header.sequence,
            is_last_page: page.header.is_last_page,
            // Propagate the last known valid bitstream timestamp. This will be updated later if
            // a bitstream packet ends in this page.
            end_ts: prev_page_info.and_then(|info| info.end_ts),
        });

        // If there is partial packet data buffered, a continuation page is expected.
        if !page.header.is_continuation && self.part_len > 0 {
            warn!("expected a continuation page");

            // Clear partial packet data.
            self.part_len = 0;
        }

        let mut iter = page.packets();

        // If there is no partial packet data buffered, a continuation page is not expected.
        if page.header.is_continuation && self.part_len == 0 {
            // If the continuation page contains packets, drop the first packet since it would
            // require partial packet data to be complete. Otherwise, ignore this page entirely.
            if page.num_packets() > 0 {
                warn!("unexpected continuation page, ignoring incomplete first packet");
                iter.next();
            }
            else {
                warn!("unexpected continuation page, ignoring page");
                return Ok(side_data);
            }
        }

        let mut total_pkt_dur = Duration::ZERO;
        let mut total_pkt_discard = Duration::ZERO;

        let num_prev_packets = self.packets.len();

        for buf in &mut iter {
            // Get a packet with data from the partial packet buffer, the page, or both.
            let data = self.get_packet(buf);

            // Perform packet mapping. If the packet contains stream data, queue it onto the packet
            // queue. If it contains side data, then add it to the side data list. Ignore other
            // types of packet data.
            match self.mapper.map_packet(&data) {
                Ok(MapResult::StreamData { dur, discard }) => {
                    total_pkt_dur = total_pkt_dur.saturating_add(dur);
                    total_pkt_discard = total_pkt_discard.saturating_add(discard);

                    let packet = PacketBuilder::new()
                        .track_id(page.header.serial)
                        .pts(Timestamp::ZERO)
                        .dur(dur)
                        .data(data)
                        .trim_start(discard)
                        .build();

                    self.packets.push_back(packet);
                }
                Ok(MapResult::SideData { data }) => side_data.push(data),
                Err(e) => {
                    warn!("mapping packet failed ({e}), skipping")
                }
                _ => (),
            }
        }

        // If the page contains partial packet data, then save the partial packet data for later
        // as the packet will be completed on a later page.
        if let Some(buf) = iter.partial_packet() {
            self.save_partial_packet(buf)?;
        }

        // While the physical stream starts, the stream's data begins on this page if its first
        // packets end here, else after it: a mapping begins the data on a page of its own after
        // the headers (e.g., RFC 7845, section 3).
        if is_init_page && num_prev_packets == 0 {
            let seq = page.header.sequence;
            self.data_seq = Some(if self.packets.is_empty() { seq.wrapping_add(1) } else { seq });
        }

        // If one or more bitstream packets ended in this page, process them.
        if num_prev_packets < self.packets.len() {
            // The page's timestamp is one past the last valid frame in the last completed packet
            // in this page.
            let page_end_ts = self.mapper.absgp_to_ts(page.header.absgp);

            // Update previous page information with the timestamp.
            let prev_page = self
                .prev_page_info
                .as_mut()
                .expect("prev_page_info unconditionally assigned above");
            prev_page.end_ts = Some(page_end_ts);

            // Compute the page's start timestamp.
            let page_start_ts = if let Some(ts) = prev_page_info.and_then(|prev| prev.end_ts) {
                // The previous page is known and it has a valid end timestamp. Use it as this
                // page's start timestamp.
                ts
            }
            else {
                let is_single_page_stream =
                    page.header.is_last_page && (is_init_page || self.is_single_page_stream());

                // The lower-bound for the page's start timestamp.
                let page_start_ts_raw = page_end_ts.saturating_sub(total_pkt_dur);

                if is_single_page_stream {
                    // In a single-page stream that begins at t = 0, the page end timestamp is the
                    // exact length of valid (no delay or padding) frames. Therefore, the page start
                    // timestamp is known to be equal to -delay frames. All additional frames are
                    // padding.
                    //
                    // If the stream does not start at t = 0, then it is not possible to determine
                    // whether frames should be discarded from the start or the end of the stream.
                    // Therefore, we assume t = 0 for the remainder.
                    let total_dur_no_padding =
                        match total_pkt_discard.checked_add((page_end_ts.get() as u64).into()) {
                            Some(v) => v,
                            None => return decode_error("ogg: duration overflow"),
                        };

                    if total_pkt_dur >= total_dur_no_padding {
                        // The total packet duration is >= delay + valid (assuming t = 0) frames,
                        // the extra frames must be padding.
                        //
                        // If the assumption the stream begins at t = 0 is false, then the following
                        // misbehaviour will occur:
                        //
                        // If the encoder set t < 0 to discard additional frames at the start of the
                        // stream, then the additional frames will be discard from the end, not the
                        // beginning.
                        //
                        // If the encoder set t > 0 to indicate the media begins later, then no
                        // padding frames will get discarded.
                        Timestamp::from(-(total_pkt_discard.get() as i64))
                    }
                    else {
                        // Stream starts at t > 0.
                        page_start_ts_raw
                    }
                }
                else {
                    // In a multi-page stream, all pages other than the last have no padding.
                    // Therefore, the naive calculation is always valid because the total packet
                    // duration would only include valid or discarded frames.
                    //
                    // For the last page, this calculation fails because the total packet duration
                    // includes the padding and it is not possible to know how many frames are
                    // valid. If the end bound is known, the correct page start timestamp can be
                    // found. A seek doesn't land on the last page for this reason (see
                    // `OggReader::do_seek`).
                    page_start_ts_raw
                }
            };

            let mut next_pkt_pts = page_start_ts;

            // Frames decoded before the track's first presented frame are the delay, wherever
            // reading began: a seek may land on a later page of a delay that outlasts the first
            // page. The track's start is settled once the physical stream is open, after its
            // initial pages are read.
            let start_ts = (!is_init_page).then(|| self.mapper.track().start_ts);

            for packet in self.packets.iter_mut().skip(num_prev_packets) {
                packet.pts = next_pkt_pts;

                // Trim the frames before the track's start. Don't trim more frames than available.
                if let Some(delay) = start_ts.and_then(|ts| ts.duration_from(packet.pts)) {
                    packet.trim_start = packet.trim_start.max(delay.min(packet.dur));
                }

                // The packet's duration is populated with the packet's decoded duration (includes
                // delay and padding). Calculate the next packet's PTS. This is also the end PTS of
                // the current packet.
                next_pkt_pts = next_pkt_pts.saturating_add(packet.dur);

                // Remove the start trim from the packet's duration. The start trim was populated
                // earlier.
                packet.dur = packet.dur.saturating_sub(packet.trim_start);

                // If the end of the current packet exceeds the page end, trim the end. Don't trim
                // more frames than available.
                if next_pkt_pts > page_end_ts {
                    packet.trim_end = page_end_ts.abs_delta(next_pkt_pts).min(packet.dur);
                }

                // Remove the end trim from the packet's duration.
                packet.dur = packet.dur.saturating_sub(packet.trim_end);
            }
        }

        Ok(side_data)
    }

    /// Returns true if the logical stream has packets buffered.
    pub fn has_packets(&self) -> bool {
        !self.packets.is_empty()
    }

    /// Examine, but do not consume, the next packet.
    pub fn peek_packet(&self) -> Option<&Packet> {
        self.packets.front()
    }

    /// Consumes and returns the next packet.
    pub fn next_packet(&mut self) -> Option<Packet> {
        self.packets.pop_front()
    }

    /// Consumes the next packet.
    pub fn consume_packet(&mut self) {
        self.packets.pop_front();
    }

    /// Examine the first page of the non-setup codec bitstream to obtain the start time and start
    /// delay parameters.
    pub fn inspect_start_page(&mut self, page: &Page<'_>) {
        if self.start_bound.is_some() {
            debug!("start page already found");
            return;
        }

        let mut parser = match self.mapper.make_parser() {
            Some(parser) => parser,
            _ => {
                debug!("failed to make start bound packet parser");
                return;
            }
        };

        // Sum the total duration of all packets, and the amount to discard.
        let mut total_pkt_dur = Duration::ZERO;
        let mut total_pkt_discard = Duration::ZERO;

        for buf in page.packets() {
            let (pkt_dur, pkt_discard) = parser.parse_next_packet_dur(buf);

            // On overflow, it will not be possible to determine the start bound.
            total_pkt_dur = match total_pkt_dur.checked_add(pkt_dur) {
                Some(total) => total,
                _ => return,
            };
            total_pkt_discard = match total_pkt_discard.checked_add(pkt_discard) {
                Some(total) => total,
                _ => return,
            };
        }

        // Map the absolute granule position to a timestamp.
        let page_end_ts = self.mapper.absgp_to_ts(page.header.absgp);

        // For single page streams, the first page is also the last page. If the page is
        // explicitly marked as the last page, then we assume the stream starts and ends on the
        // same page. Therefore, we can only assume it starts at pts=0, and any discarded frames
        // are padding frames, not delay frames.
        let page_start_ts = if !page.header.is_last_page {
            match page_end_ts.checked_sub(total_pkt_dur) {
                Some(ts) => ts,
                _ => return,
            }
        }
        else {
            Timestamp::new(-(total_pkt_discard.get() as i64))
        };

        let bound = Bound {
            seq: page.header.sequence,
            is_last_page: page.header.is_last_page,
            ts: page_start_ts,
            discard: total_pkt_discard,
        };

        // Update codec parameters. The track starts at its first presented frame, the first frame
        // after the frames discarded from the start of the stream (the delay). A delay the mapper
        // read from the stream's header (e.g., the Opus pre-skip) may outlast this page, whose
        // discard is then only the delay's first part.
        let track = self.mapper.track_mut();

        let delay = Duration::from(u64::from(track.delay.unwrap_or(0))).max(bound.discard);

        track.with_start_ts(bound.ts.saturating_add(delay));

        if delay > Duration::ZERO {
            track.with_delay(delay.get() as u32);
        }

        // Update start bound.
        self.start_bound = Some(bound);
    }

    /// Examines the last pages of the codec bitstream, in order, to obtain the end time and the end
    /// padding. The padding is what the packets that end on the last page decode to past the end
    /// of the stream: it is read from the end of the page before that page, and from those
    /// packets whole, so also from the pages a packet continued onto that page began on, and from
    /// the packet before them, which their decoded duration may depend on. The stream's first page
    /// of data, if the start probe did not reach it, is examined for the start first. The state
    /// returned by each iteration of this function should be passed into the subsequent iteration.
    pub fn inspect_end_page(&mut self, state: &mut InspectState, page: &Page<'_>) {
        // Do nothing if the end bound was found.
        if self.end_bound.is_some() {
            debug!("end page already found");
            return;
        }

        // The start probe reads the first pages of the physical stream only: the start of a stream
        // whose data begins later is read here, before the stream's length is counted from it.
        if self.start_bound.is_none() && self.data_seq == Some(page.header.sequence) {
            self.inspect_start_page(page);
        }

        // Get and/or create the packet parser.
        let parser = match &mut state.parser {
            Some(parser) => parser,
            None => {
                state.parser = self.mapper.make_parser();

                let Some(parser) = &mut state.parser
                else {
                    debug!("failed to make end bound packet parser");
                    return;
                };

                parser
            }
        };

        // A packet the page continues began on the pages before it, and is whole only if those
        // pages were read. A page missing after the first page read is missing to any scan.
        let follows = state.seq.is_some_and(|seq| seq.wrapping_add(1) == page.header.sequence);
        state.lost |= state.seq.is_some() && !follows;
        state.seq = Some(page.header.sequence);

        let mut begun = state.partial.take().filter(|_| follows && page.header.is_continuation);

        // The packet before the page's first packet was parsed if the page follows the page of the
        // last packet parsed. On the stream's first page of data, the first packet follows none.
        let starts = self.start_bound.is_some_and(|b| b.seq == page.header.sequence);
        state.history = starts || (state.history && follows);

        // Calculate the decoded duration of the packets that end on the page. Note that even though
        // only the last page uses this duration, it is important to feed the packet parser so that
        // the first packet of the final page doesn't have a duration of 0 due to lapping on some
        // codecs. It is unknown if the first packet began on a page that was not read, if the
        // packet before a packet was not parsed (it began on a page that was not read, or a page
        // before is lost), or if the duration overflows.
        let mut total_pkt_dur = Some(Duration::ZERO);
        let mut packets = page.packets();

        if page.header.is_continuation {
            if let Some(end) = packets.next() {
                let dur = begun.take().map(|mut packet| {
                    packet.extend_from_slice(end);
                    parser.parse_next_packet_dur(&packet).0
                });
                total_pkt_dur = dur.filter(|_| state.history);
                state.history = dur.is_some();
            }
        }

        for buf in &mut packets {
            let (pkt_dur, _) = parser.parse_next_packet_dur(buf);
            total_pkt_dur = total_pkt_dur
                .filter(|_| state.history)
                .and_then(|total| total.checked_add(pkt_dur));
            state.history = true;
        }

        // Keep the start of the packet left unfinished: one that begins on this page, or one that
        // this page carries on, no longer than a packet is read (a longer one is missing to any
        // scan).
        state.partial = match packets.partial_packet() {
            Some(start) if page.num_packets() > 0 || !page.header.is_continuation => {
                Some(start.to_vec())
            }
            Some(more) => begun.and_then(|mut packet| {
                let fits = packet.len() + more.len() <= LogicalStream::MAX_PACKET_LEN;
                state.lost |= !fits;
                fits.then(|| {
                    packet.extend_from_slice(more);
                    packet
                })
            }),
            None => None,
        };

        // A page on which no packet ends states no position (RFC 3533, section 6).
        if page.num_packets() > 0 {
            state.last = Some(EndingPage {
                seq: page.header.sequence,
                prev_end: state.last.map(|last| last.end_ts),
                // Map the absolute granule position to a timestamp.
                end_ts: self.mapper.absgp_to_ts(page.header.absgp),
                dur: total_pkt_dur,
            });
        }

        // Only the last page ends the stream.
        if !page.header.is_last_page {
            return;
        }

        // The stream ends where the last page on which a packet ends does.
        let Some(EndingPage { seq: last_seq, prev_end, end_ts, dur: last_dur }) = state.last
        else {
            debug!("no packet ends on the pages read at the end of the stream");
            return;
        };

        // The end padding, or `None` if the pages it is read from were not all read.
        let padding = if let Some(prev_end) = prev_end {
            // The decoded data of the page begins where the page before it ends: the padding is
            // what it decodes to past the page's end. A granule position past the decoded data
            // trims nothing. A page lost between the two leaves the duration unknown (no packet was
            // parsed since), so the padding is not counted from a page before the loss.
            last_dur.map(|dur| {
                prev_end.saturating_add(dur).duration_from(end_ts).unwrap_or(Duration::ZERO)
            })
        }
        else if self.start_bound.is_some_and(|b| b.seq == last_seq) {
            // The start and end page is the same page. The end delay is the amount of excess
            // page duration after subtracting the page's end timestamp.

            // The amount of start delay, if available.
            let delay = self.start_bound.map(|s| s.discard).unwrap_or(Duration::ZERO);

            // The page end timestamp does not include delay or padding. Therefore, it is also
            // the duration of valid content assuming it is non-negative.
            let valid = end_ts.duration_from(Timestamp::ZERO);

            // Take the valid duration from the valid and padding duration to yield the end
            // padding.
            last_dur.map(|dur| {
                dur.checked_sub(delay)
                    .zip(valid)
                    .and_then(|(valid_and_padding, valid)| valid_and_padding.checked_sub(valid))
                    .unwrap_or(Duration::ZERO)
            })
        }
        else {
            // The page before the last was not read.
            None
        };

        // The track's length counts its frames from its first presented frame.
        // TODO: Bounds are in timestamp units, not number of frames. Fix this up because it only
        // works for audio codecs.
        let track = self.mapper.track_mut();

        let num_frames = end_ts.duration_from(track.start_ts).unwrap_or(Duration::ZERO);

        track.with_num_frames(num_frames.get());
        track.with_duration(num_frames);

        if let Some(padding) = padding.filter(|padding| !padding.is_zero()) {
            track.with_padding(u32::try_from(padding.get()).unwrap_or(u32::MAX));
        }

        self.end_needs_earlier = padding.is_none() && !state.lost;

        self.end_bound = Some(Bound {
            seq: page.header.sequence,
            is_last_page: true,
            ts: end_ts,
            discard: padding.unwrap_or(Duration::ZERO),
        });
    }

    /// Returns true if the stream's last page was read, but not every page before it that its end
    /// padding is read from, and none of the pages read is missing: earlier pages may hold the
    /// rest. A stream whose packets can't be timed (no mapper reads them) finds no end, so it never
    /// needs any.
    pub fn end_needs_earlier_pages(&self) -> bool {
        self.end_bound.is_some() && self.end_needs_earlier
    }

    /// Forget the end of the stream, so that its last pages are examined again, from earlier.
    pub fn forget_end(&mut self) {
        self.end_bound = None;
    }

    /// Examine a page in isolation and return the start and end timestamps as a tuple.
    pub fn inspect_page(&mut self, page: &Page<'_>) -> (Timestamp, Timestamp) {
        // Get the cumulative duration of all packets within this page.
        let mut total_pkt_dur = Duration::ZERO;
        let mut total_pkt_discard = Duration::ZERO;

        if let Some(mut parser) = self.mapper.make_parser() {
            for buf in page.packets() {
                let (pkt_dur, pkt_discard) = parser.parse_next_packet_dur(buf);

                total_pkt_dur = total_pkt_dur.saturating_add(pkt_dur);
                total_pkt_discard = total_pkt_discard.saturating_add(pkt_discard);
            }
        }

        let page_end_ts = self.mapper.absgp_to_ts(page.header.absgp);

        // Map the absolute granule position to a timestamp.
        let page_start_ts_raw = page_end_ts.saturating_sub(total_pkt_dur);

        let page_start_ts = match self.start_bound {
            // Start bound is known, and this is the first bitstream page.
            Some(b) if b.seq == page.header.sequence => b.ts,
            // Start bound is known, but this is not the first bitstream page.
            Some(_) => page_start_ts_raw,
            // Start bound is not known. Assume this is the first bitstream page.
            None if page_start_ts_raw.is_negative() => {
                Timestamp::new(-(total_pkt_discard.get() as i64))
            }
            // Start bound is not known, but the page start timestamp is positive. The
            // stream likely started with a positive timestamp.
            None => page_start_ts_raw,
        };

        (page_start_ts, page_end_ts)
    }

    /// Returns true if the stream is only a single page.
    fn is_single_page_stream(&self) -> bool {
        match self.start_bound {
            // The starting page is known, and it is marked as the last page. This is a single-page
            // stream.
            Some(start) if start.is_last_page => true,
            // The starting page is known, and it is not marked as the last page. This can be
            // assumed to be a multi-page stream. NOTE: This assumption relies on the starting page
            // being the page with first completed packet.
            Some(start) => {
                // However, if the starting and ending pages are known and have mismatched sequence
                // numbers (this shouldn't happen), then the previous assumption was false.
                match self.end_bound {
                    Some(end) => start.seq == end.seq,
                    _ => true,
                }
            }
            // The starting page is not known, cannot assume this is a single-page stream.
            _ => false,
        }
    }

    fn get_packet(&mut self, packet_buf: &[u8]) -> Box<[u8]> {
        if self.part_len == 0 {
            Box::from(packet_buf)
        }
        else {
            let mut buf = vec![0u8; self.part_len + packet_buf.len()];

            // Split packet buffer into two portions: saved and new.
            let (vec0, vec1) = buf.split_at_mut(self.part_len);

            // Copy and consume the saved partial packet.
            vec0.copy_from_slice(&self.part_buf[..self.part_len]);
            self.part_len = 0;

            // Read the remainder of the partial packet from the page.
            vec1.copy_from_slice(packet_buf);

            buf.into_boxed_slice()
        }
    }

    fn save_partial_packet(&mut self, buf: &[u8]) -> Result<()> {
        let new_part_len = self.part_len + buf.len();

        if new_part_len > self.part_buf.len() {
            // Do not exceed an a certain limit to prevent unbounded memory growth.
            if new_part_len > LogicalStream::MAX_PACKET_LEN {
                return decode_error("ogg: packet buffer would exceed max size");
            }

            // New partial packet buffer size, rounded up to the nearest 8K block.
            let new_buf_len = (new_part_len + (8 * 1024 - 1)) & !(8 * 1024 - 1);
            debug!("grow packet buffer to {new_buf_len} bytes");

            self.part_buf.resize(new_buf_len, Default::default());
        }

        self.part_buf[self.part_len..new_part_len].copy_from_slice(buf);
        self.part_len = new_part_len;

        Ok(())
    }
}
