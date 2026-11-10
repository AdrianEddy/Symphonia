// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Seeks by the Cues in files of several clusters land on the frame that holds the target, in
//! every cluster, the last one included, on the track the cue points index and on one they don't,
//! where that frame is stored in the cluster before the cue point's, and on a track whose frames
//! state no duration. Without Cues a seek lands so too, from wherever the reader is, and on a
//! source that can't seek it reads on to a target ahead of the reader; a file without Cues reads to
//! its end and ends cleanly. Cue points stored out of time order are searched by time, and a
//! track's `TrackTimestampScale` scales its ticks. The fixtures are made by
//! `fixtures/regenerate.sh`, and the time of each packet is ffprobe's, from its
//! `<fixture>.ffprobe.csv`. The timebase of each track is 1 ms; the Opus tracks state an 80 ms
//! `SeekPreRoll`, the video and PCM tracks none.

mod common;

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use symphonia_core::errors::{Error, Result, SeekErrorKind};
use symphonia_core::formats::{FormatReader, SeekMode, SeekTo, SeekedTo};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes, ReadOnlySource};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_mkv::MkvReader;

use common::within_deadline;

/// 2.5 s of Opus in six clusters of at most 0.5 s, the last of two blocks, each cued at its start.
const OPUS: &str = "opus_clusters.mka";

/// The Opus track of `OPUS` and a video track at 5 fps, in six clusters: the cue points index the
/// video alone, every frame, so most of them are inside a cluster, after blocks of the Opus track.
const VIDEO_AND_OPUS: &str = "mjpeg_opus_clusters.mkv";

/// 2.5 s of PCM in packets of 128 ms that state no duration (no BlockDuration, no DefaultDuration),
/// in seven clusters of three packets, the last of two, each cued at its start. The Segment states
/// its duration, 2.5 s.
const PCM: &str = "pcm_clusters.mka";

/// A video track at 5 fps for 2.5 s and `PCM`'s track for 1.5 s, in six clusters: the cue points
/// index the video alone, every frame. The PCM track's last packet, from 1408 ms, is in the fifth
/// cluster, and the sixth, from 2 s, holds only video.
const VIDEO_AND_PCM: &str = "mjpeg_pcm_clusters.mkv";

/// A video track at 5 fps for 2 s and two PCM tracks for 1.5 s in packets of 128 ms that state no
/// duration, in five clusters: the cue points index the video alone, every frame. FFmpeg's muxer
/// holds one audio packet back to open the cluster of a video keyframe, the third track's, so the
/// second track's packet that spans the keyframe ends the cluster before (from 384, 768 and 1152
/// ms), as does its last packet, from 1408 ms: the fifth cluster holds none of its packets. The
/// Segment states its duration, 2 s.
const VIDEO_AND_TWO_PCM: &str = "mjpeg_2pcm_clusters.mkv";

/// `OPUS`'s track written through a pipe: no Cues, and a Segment of unknown size.
const NO_CUES: &str = "opus_no_cues.mka";

/// 1 s of Opus in one cluster of Xiph-laced blocks, five frames to a block.
const LACED: &str = "opus_laced.mka";

/// The seek pre-roll of an Opus track in milliseconds, the timebase's ticks.
const OPUS_PRE_ROLL: i64 = 80;

fn path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn open(name: &str) -> MkvReader<'static> {
    let file = File::open(path(name)).unwrap_or_else(|err| panic!("{name}: {err}"));
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    MkvReader::try_new(mss, Default::default()).expect("the reader opens")
}

/// A reader of a fixture through a source that can't seek and doesn't know its length.
fn open_unseekable(name: &str) -> MkvReader<'static> {
    let file = File::open(path(name)).unwrap_or_else(|err| panic!("{name}: {err}"));
    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(file)), Default::default());
    MkvReader::try_new(mss, Default::default()).expect("the reader opens")
}

/// The bytes of a fixture.
fn read(name: &str) -> Vec<u8> {
    std::fs::read(path(name)).unwrap_or_else(|err| panic!("{name}: {err}"))
}

/// A reader of a file in memory.
fn open_bytes(file: Vec<u8>) -> MkvReader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(file)), Default::default());
    MkvReader::try_new(mss, Default::default()).expect("the reader opens")
}

/// A file in memory that records the position of each seek of it.
struct Recorded {
    file: Cursor<Vec<u8>>,
    seeks: Arc<Mutex<Vec<u64>>>,
}

impl Read for Recorded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for Recorded {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let pos = self.file.seek(pos)?;
        self.seeks.lock().expect("the seeks").push(pos);
        Ok(pos)
    }
}

impl MediaSource for Recorded {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.file.get_ref().len() as u64)
    }
}

/// A reader of a file in memory, and the positions it seeks the file to.
fn open_recorded(file: Vec<u8>) -> (MkvReader<'static>, Arc<Mutex<Vec<u64>>>) {
    let seeks = Arc::new(Mutex::new(Vec::new()));
    let source = Recorded { file: Cursor::new(file), seeks: Arc::clone(&seeks) };
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    (MkvReader::try_new(mss, Default::default()).expect("the reader opens"), seeks)
}

/// Read up to `n` packets, fewer at the end of the stream, and return them.
fn next_packets(reader: &mut MkvReader<'_>, n: usize) -> Vec<Packet> {
    std::iter::from_fn(|| reader.next_packet().expect("a packet")).take(n).collect()
}

/// Read the packets to the end of the stream, and return them.
fn packets(reader: &mut MkvReader<'_>) -> Vec<Packet> {
    next_packets(reader, usize::MAX)
}

/// Seek the track `track_id` to `ms` milliseconds.
fn seek(reader: &mut MkvReader<'_>, track_id: u32, ms: i64) -> Result<SeekedTo> {
    reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ms), track_id })
}

/// A packet as ffprobe lists it.
struct Probed {
    /// The index of the packet's track among the file's tracks.
    track: usize,
    /// Its timestamp and duration, in milliseconds.
    pts: i64,
    dur: i64,
}

/// The packets of a fixture as ffprobe lists them, in the order it reads them:
/// `ffprobe -show_entries packet=stream_index,pts,duration -of csv=p=0`.
fn probed(name: &str) -> Vec<Probed> {
    let csv =
        std::fs::read_to_string(path(&format!("{name}.ffprobe.csv"))).expect("ffprobe's list");
    csv.lines()
        .map(|line| {
            let mut fields = line.split(',').map(|field| field.parse().expect(line));
            let mut field = || fields.next().expect(line);
            Probed { track: field() as usize, pts: field(), dur: field() }
        })
        .collect()
}

/// Assert that the packets are those expected: the same tracks, timestamps, durations, trims and
/// data.
fn assert_same(packets: &[Packet], expected: &[Packet], what: &str) {
    let fields = |p: &Packet| (p.track_id, p.pts, p.dur, p.trim_start, p.trim_end);
    assert_eq!(
        packets.iter().map(fields).collect::<Vec<_>>(),
        expected.iter().map(fields).collect::<Vec<_>>(),
        "{what}"
    );
    assert!(packets.iter().zip(expected).all(|(p, e)| p.data == e.data), "{what}: other data");
}

/// Whether a seek failed because its target lies behind the reader of a source that can't seek.
fn refused(result: &Result<SeekedTo>) -> bool {
    matches!(result, Err(Error::SeekError(SeekErrorKind::ForwardOnly)))
}

/// Whether a seek failed because its target lies past the end.
fn out_of_range(result: &Result<SeekedTo>) -> bool {
    matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange)))
}

/// The bits of the first byte of an EBML variable-size integer of `len` bytes that hold its value.
fn value_bits(len: usize) -> u8 {
    0xff_u8.checked_shr(len as u32).unwrap_or(0)
}

/// The EBML element at `pos` of `file`: where its size begins, the length of its size and where the
/// element ends (if its size is known). An ID and a size are variable-size integers, whose length
/// is told by the leading zeros of their first byte.
fn element(file: &[u8], pos: usize) -> (usize, usize, usize) {
    let size_pos = pos + file[pos].leading_zeros() as usize + 1;
    let size_len = file[size_pos].leading_zeros() as usize + 1;
    let first = u64::from(file[size_pos] & value_bits(size_len));
    let size = file[size_pos + 1..size_pos + size_len]
        .iter()
        .fold(first, |size, &b| size << 8 | u64::from(b));
    (size_pos, size_len, size_pos + size_len + size as usize)
}

/// The ID of a Cluster element.
const CLUSTER: [u8; 4] = [0x1f, 0x43, 0xb6, 0x75];

/// The ID of a Cues element.
const CUES: [u8; 4] = [0x1c, 0x53, 0xbb, 0x6b];

/// Where the data of the EBML element at `pos` of `file` begins and ends (if its size is known).
fn data(file: &[u8], pos: usize) -> (usize, usize) {
    let (size_pos, size_len, end) = element(file, pos);
    (size_pos + size_len, end)
}

/// The position of each EBML element in `file[start..end]`, children of one element, each of known
/// size.
fn children(file: &[u8], start: usize, end: usize) -> Vec<usize> {
    let mut children = Vec::new();
    let mut pos = start;

    while pos < end {
        children.push(pos);
        pos = element(file, pos).2;
    }

    children
}

/// The position of each child of the Matroska file's Segment, which ends with the file (its size
/// may be unknown), whose IDs begin with `id`.
fn segment_children(file: &[u8], id: &[u8]) -> Vec<usize> {
    // The EBML header, then the Segment.
    let (_, _, segment) = element(file, 0);
    let mut children = children(file, data(file, segment).0, file.len());
    children.retain(|&pos| file[pos..].starts_with(id));
    children
}

/// `file`, a Matroska file whose Segment is of unknown size, with every Cluster of unknown size too,
/// as browsers' MediaRecorder writes them (FFmpeg writes the sizes of clusters): the size of each
/// is rewritten as all ones, the unknown size, of the same length.
fn with_clusters_of_unknown_size(mut file: Vec<u8>) -> Vec<u8> {
    let clusters = segment_children(&file, &CLUSTER);

    for &cluster in &clusters {
        let (size_pos, size_len, _) = element(&file, cluster);
        file[size_pos] |= value_bits(size_len);
        file[size_pos + 1..size_pos + size_len].fill(0xff);
    }

    assert!(clusters.len() > 1, "clusters of unknown size: {}", clusters.len());
    file
}

/// `file` without Cues: the ID of its Cues element renamed to one Matroska does not define, which
/// the reader skips (as it does where a SeekHead leads to it).
fn without_cues(mut file: Vec<u8>) -> Vec<u8> {
    let cues = segment_children(&file, &CUES);
    assert_eq!(cues.len(), 1, "one Cues element");

    file[cues[0] + 3] = 0x6c;
    file
}

/// `file` with the cue points of its Cues stored in reverse, from the latest to the earliest.
fn with_cue_points_reversed(mut file: Vec<u8>) -> Vec<u8> {
    const CUE_POINT: u8 = 0xbb;

    let cues = segment_children(&file, &CUES);
    let (start, end) = data(&file, cues[0]);
    let points: Vec<_> = children(&file, start, end)
        .into_iter()
        .filter(|&pos| file[pos] == CUE_POINT)
        .map(|pos| pos..element(&file, pos).2)
        .collect();
    assert!(points.len() > 1, "cue points: {}", points.len());
    assert!(points.windows(2).all(|pair| pair[0].end == pair[1].start), "one after another");

    let reversed: Vec<u8> =
        points.iter().rev().flat_map(|point| file[point.clone()].to_vec()).collect();
    let first = points[0].start;
    file[first..first + reversed.len()].copy_from_slice(&reversed);
    file
}

/// `file`, whose one track states no TrackTimestampScale, with a TrackTimestampScale of `scale` and
/// every frame at the same time, as a muxer writes it: the track's FlagLacing and Language elements
/// become a TrackTimestampScale element of the same length (a 4-byte float, its size in 3 bytes),
/// and each SimpleBlock's timestamp relative to its cluster's, in Track ticks, is divided by
/// `scale`. (The CRC-32 elements, which the reader doesn't check, are left as they are.)
fn with_track_timestamp_scale(mut file: Vec<u8>, scale: f32) -> Vec<u8> {
    const FLAG_LACING_AND_LANGUAGE: [u8; 10] =
        [0x9c, 0x81, 0x00, 0x22, 0xb5, 0x9c, 0x83, b'u', b'n', b'd'];
    const TRACK_TIMESTAMP_SCALE: [u8; 6] = [0x23, 0x31, 0x4f, 0x20, 0x00, 0x04];
    const SIMPLE_BLOCK: u8 = 0xa3;
    const BLOCK_GROUP: u8 = 0xa0;

    let at: Vec<usize> =
        (0..file.len() - 9).filter(|&i| file[i..i + 10] == FLAG_LACING_AND_LANGUAGE).collect();
    assert_eq!(at.len(), 1, "FlagLacing and Language, once");

    file[at[0]..at[0] + 6].copy_from_slice(&TRACK_TIMESTAMP_SCALE);
    file[at[0] + 6..at[0] + 10].copy_from_slice(&scale.to_be_bytes());

    for cluster in segment_children(&file, &CLUSTER) {
        let (start, end) = data(&file, cluster);

        for block in children(&file, start, end) {
            assert_ne!(file[block], BLOCK_GROUP, "a BlockGroup");

            if file[block] == SIMPLE_BLOCK {
                // The block's track number, a variable-size integer, then its timestamp.
                let track = data(&file, block).0;
                let ts = track + file[track].leading_zeros() as usize + 1;
                let scaled = f32::from(i16::from_be_bytes([file[ts], file[ts + 1]])) / scale;
                assert_eq!(scaled.fract(), 0.0, "a timestamp the scale divides");

                file[ts..ts + 2].copy_from_slice(&(scaled as i16).to_be_bytes());
            }
        }
    }

    file
}

/// Assert that a seek to the first and to the last millisecond of every packet of `name` lands on
/// that packet, its pre-roll before it (`audio_pre_roll` on the audio track, the last, none on the
/// video), and that every track reads on from it as a linear read does.
fn assert_every_seek_lands(name: &str, audio_pre_roll: i64) {
    let mut reader = open(name);
    let tracks: Vec<_> = reader.tracks().iter().map(|track| track.id).collect();
    let audio = tracks.len() - 1;
    let linear = packets(&mut reader);

    // The reader times every packet as ffprobe does.
    let probed = probed(name);
    assert_eq!(
        linear.iter().map(|p| (p.track_id, p.pts.get())).collect::<Vec<_>>(),
        probed.iter().map(|p| (tracks[p.track], p.pts)).collect::<Vec<_>>(),
        "{name}"
    );

    for (i, packet) in probed.iter().enumerate() {
        let track_id = tracks[packet.track];
        let pre_roll = if packet.track == audio { audio_pre_roll } else { 0 };

        for ms in [packet.pts + 1, packet.pts + packet.dur - 1] {
            let required = ms + pre_roll;
            let what = format!("{name}: a seek of track {track_id} to {required} ms");

            let seeked = seek(&mut reader, track_id, required).expect(&what);
            assert_eq!(seeked.required_ts, Timestamp::new(required), "{what}");
            assert_eq!(seeked.actual_ts, Timestamp::new(packet.pts), "{what}");

            assert_same(&packets(&mut reader), &linear[i..], &what);
        }
    }
}

/// Assert that seeks of the first track of `file` and of `like`, two files laid out alike, to each
/// of `targets` (in milliseconds) seek their sources to the same positions and land at the same
/// time on a packet of the same data, or fail alike.
fn assert_seeks_alike(file: Vec<u8>, like: Vec<u8>, targets: impl IntoIterator<Item = i64>) {
    let mut readers = [open_recorded(file), open_recorded(like)];

    for ms in targets {
        let [seeked, seeked_like] = readers.each_mut().map(|(reader, seeks)| {
            seeks.lock().expect("the seeks").clear();

            let track = &reader.tracks()[0];
            let (track_id, tb) = (track.id, track.time_base.expect("a timebase"));
            let to = SeekTo::Time { time: Time::from_millis(ms), track_id: Some(track_id) };

            // Where the seek lands and the packet read next, times in milliseconds.
            let landing = reader.seek(SeekMode::Accurate, to).map_err(|err| format!("{err:?}"));
            let landing = landing.map(|seeked| {
                let ms = |ts| tb.calc_time(ts).expect("a time").as_millis();
                let next = reader.next_packet().expect("a read").expect("a packet");
                (ms(seeked.actual_ts), ms(next.pts), next.data)
            });

            (landing, seeks.lock().expect("the seeks").clone())
        });

        let (landing, positions) = seeked;
        let (landing_like, positions_like) = seeked_like;
        let times = |landing: &std::result::Result<(i128, i128, Box<[u8]>), String>| {
            landing.as_ref().map(|&(at, next, _)| (at, next)).map_err(Clone::clone)
        };

        assert_eq!(times(&landing), times(&landing_like), "a seek to {ms} ms: the landing");
        assert!(landing == landing_like, "a seek to {ms} ms: the data read next");
        assert_eq!(positions, positions_like, "a seek to {ms} ms: the positions sought");
    }
}

/// A seek to the first and to the last millisecond of every packet lands on that packet, its
/// pre-roll before it: every frame of every cluster, on each track. A cue point of the track a seek
/// is on leads to the cued block, and one of another track to the start of its cluster, where the
/// blocks before the cued one may hold the target.
#[test]
fn a_seek_lands_on_the_frame_that_holds_its_target_in_every_cluster() {
    within_deadline(|| {
        for name in [OPUS, VIDEO_AND_OPUS] {
            assert_every_seek_lands(name, OPUS_PRE_ROLL);
        }
    });
}

/// The frame that holds a target may be stored in the cluster before the one a seek reads from:
/// a muxer may store an audio frame that spans a video keyframe at the end of the cluster before
/// the keyframe's, as FFmpeg's does for every audio track but one. A seek by a cue point of another
/// track, or without Cues by the clusters read, lands on it all the same: a seek of each PCM track
/// to every millisecond lands on the packet that holds it, the track's last up to the end of the
/// Segment, whose cluster is the one before the last, and every track reads on from there as a
/// linear read does.
#[test]
fn a_seek_lands_on_the_frame_that_holds_its_target_in_the_cluster_before() {
    within_deadline(|| {
        assert_every_seek_lands(VIDEO_AND_TWO_PCM, 0);

        let probed = probed(VIDEO_AND_TWO_PCM);
        let file = read(VIDEO_AND_TWO_PCM);

        for (layout, file) in [("by the Cues", file.clone()), ("without Cues", without_cues(file))]
        {
            // A linear read, which indexes every cluster for the seeks without Cues.
            let mut reader = open_bytes(file);
            let tracks: Vec<_> = reader.tracks().iter().map(|track| track.id).collect();
            let linear = packets(&mut reader);

            for track in [1, 2] {
                let track_id = tracks[track];

                for ms in 0..=2000 {
                    let what = format!("{layout}: a seek of track {track_id} to {ms} ms");

                    // The track's last packet that begins at or before the target.
                    let at = probed.iter().rposition(|p| p.track == track && p.pts <= ms).unwrap();

                    let seeked = seek(&mut reader, track_id, ms).expect(&what);
                    assert_eq!(seeked.actual_ts, Timestamp::new(probed[at].pts), "{what}");

                    let expected = &linear[at..(at + 3).min(linear.len())];
                    assert_same(&next_packets(&mut reader, 3), expected, &what);
                }

                let seeked = seek(&mut reader, track_id, 2001);
                assert!(out_of_range(&seeked), "{layout}: past the end: {seeked:?}");
            }
        }
    });
}

/// Cue points need not be stored in time order (RFC 9559 recommends it), and are searched by time
/// all the same: with `OPUS`'s cue points stored in reverse, a seek to the first and the last
/// millisecond of every packet reads from the same cluster and block, and lands on the same packet,
/// as with them in order.
#[test]
fn cue_points_out_of_time_order_are_searched_by_time() {
    within_deadline(|| {
        let targets = probed(OPUS)
            .iter()
            .flat_map(|p| [p.pts + 1, p.pts + p.dur - 1])
            .map(|ms| ms + OPUS_PRE_ROLL)
            .collect::<Vec<_>>();

        assert_seeks_alike(read(OPUS), with_cue_points_reversed(read(OPUS)), targets);
    });
}

/// A Track tick is `TrackTimestampScale` Segment ticks. `PCM` with a `TrackTimestampScale` of 2 or
/// 0.5, its blocks' timestamps divided by it so every frame keeps its time, has a tick of 2 or 0.5
/// ms, times every packet as ffprobe times `PCM`, and seeks by the Cues alike: a seek to the start
/// of every packet, the start of each cluster and cue point among them, and to a tick of 2 ms
/// either side of it reads from the same cluster and block and lands on the same packet, and so do
/// a seek to the end of the Segment and one past it, which fails. (The targets are whole ticks of
/// either scale: a seek is to a whole tick.)
#[test]
fn a_track_timestamp_scale_scales_the_ticks_of_the_track() {
    within_deadline(|| {
        let probed = probed(PCM);

        for (scale, ms_per_1000_ticks) in [(2.0, 2000), (0.5, 500)] {
            let file = with_track_timestamp_scale(read(PCM), scale);

            let mut reader = open_bytes(file.clone());
            let tb = reader.tracks()[0].time_base.expect("a timebase");
            let ms = |ts| tb.calc_time(ts).expect("a time").as_millis();
            assert_eq!(ms(Timestamp::new(1000)), ms_per_1000_ticks, "scale {scale}");

            let times: Vec<_> = packets(&mut reader).iter().map(|p| ms(p.pts)).collect();
            let expected: Vec<_> = probed.iter().map(|p| i128::from(p.pts)).collect();
            assert_eq!(times, expected, "scale {scale}: the packets' times");

            let targets = probed
                .iter()
                .flat_map(|p| [p.pts - 2, p.pts, p.pts + 2])
                .filter(|&ms| ms >= 0)
                .chain([2500, 2502]);

            assert_seeks_alike(read(PCM), file, targets);
        }
    });
}

/// A track whose frames state no duration seeks as one whose frames do: a frame ends where the
/// track's next frame begins, which the seek reads to tell whether the frame holds its target. So a
/// seek lands on the frame that holds its target in every cluster, the track's last frame included,
/// which holds a target up to the end of the Segment.
#[test]
fn a_seek_lands_on_the_frame_that_holds_its_target_when_frames_state_no_duration() {
    within_deadline(|| {
        for name in [PCM, VIDEO_AND_PCM] {
            assert_every_seek_lands(name, 0);
        }

        // `PCM`'s last frame, from 2432 ms, holds the Segment's end. A seek past it fails, and the
        // reader reads on from where it was.
        let mut reader = open(PCM);
        let track_id = reader.tracks()[0].id;

        let seeked = seek(&mut reader, track_id, 2500).expect("a seek to the end");
        assert_eq!(seeked.actual_ts, Timestamp::new(2432));

        let seeked = seek(&mut reader, track_id, 2501);
        assert!(out_of_range(&seeked), "{seeked:?}");
        assert_eq!(reader.next_packet().unwrap().map(|p| p.pts), Some(Timestamp::new(2432)));

        // A seek into the last frame of `VIDEO_AND_PCM`'s PCM track, from 1408 ms, in the fifth
        // cluster, lands on it once the sixth cluster begins after the target: it doesn't read on to
        // the end of the file.
        let mut reader = open(VIDEO_AND_PCM);
        let track_id = reader.tracks()[1].id;

        let seeked = seek(&mut reader, track_id, 1450).expect("a seek into the last frame");
        assert_eq!(seeked.actual_ts, Timestamp::new(1408));

        let read = Box::new(reader).into_inner().pos();
        let len = std::fs::metadata(path(VIDEO_AND_PCM)).unwrap().len();
        assert!(read < len, "{read} of {len} bytes read");
    });
}

/// A file without Cues, a Segment of unknown size, ends where its last cluster does: the end of the
/// file ends the Segment. So it does through a source that can't seek and doesn't know its length.
/// It holds the packets of the same file with Cues.
#[test]
fn a_file_without_cues_ends_cleanly() {
    let expected = packets(&mut open(OPUS));

    let mut reader = open(NO_CUES);
    assert_same(&packets(&mut reader), &expected, NO_CUES);
    assert!(reader.next_packet().unwrap().is_none(), "the end again");

    let mut reader = open_unseekable(NO_CUES);
    assert_same(&packets(&mut reader), &expected, "unseekable");
    assert!(reader.next_packet().unwrap().is_none(), "the end again, unseekable");
}

/// Without Cues, a seek reads from the last cluster read that begins at or before its target, or
/// from the first cluster, and lands as a seek by the Cues does from wherever the reader is: before
/// the target, in its cluster or another, past it, or at the end of the file, which a seek reads
/// again. So it does in a file whose clusters are of unknown size, as MediaRecorder writes them.
#[test]
fn a_seek_without_cues_lands_on_the_frame_that_holds_its_target_from_anywhere() {
    within_deadline(|| {
        let linear = packets(&mut open(OPUS));
        let probed = probed(OPUS);
        let no_cues = read(NO_CUES);

        for (layout, file) in [
            ("clusters of known size", no_cues.clone()),
            ("clusters of unknown size", with_clusters_of_unknown_size(no_cues)),
        ] {
            let mut reader = open_bytes(file.clone());
            let track_id = reader.tracks()[0].id;
            assert_same(&packets(&mut reader), &linear, layout);

            // Seek to the first and to the last millisecond of each packet in turn, forward or
            // backward through the file, and read none, three or all of the packets after each
            // landing before the next seek.
            for backward in [false, true] {
                for read_on in [0, 3, usize::MAX] {
                    let mut reader = open_bytes(file.clone());
                    let mut order: Vec<_> = (0..probed.len()).collect();
                    if backward {
                        order.reverse();
                    }

                    for i in order {
                        let packet = &probed[i];

                        for ms in [packet.pts + 1, packet.pts + packet.dur - 1] {
                            let required = ms + OPUS_PRE_ROLL;
                            let what = format!(
                                "{layout}, backward: {backward}, {read_on} read on: a seek to \
                                 {required} ms"
                            );

                            let seeked = seek(&mut reader, track_id, required).expect(&what);
                            assert_eq!(seeked.actual_ts, Timestamp::new(packet.pts), "{what}");

                            let expected = &linear[i..i.saturating_add(read_on).min(linear.len())];
                            assert_same(&next_packets(&mut reader, read_on), expected, &what);
                        }
                    }
                }
            }
        }
    });
}

/// A seek without Cues that fails, past the end, leaves the reader where it was: it reads on with
/// the packets after those it read, the rest of a laced block too. A seek back from a laced block
/// lands as by the Cues.
#[test]
fn a_failed_seek_without_cues_leaves_the_reader_where_it_was() {
    within_deadline(|| {
        // 60 packets read of `NO_CUES`, inside its third cluster; 26 of `LACED`, inside its sixth
        // block.
        let files = [(NO_CUES, read(NO_CUES), 60), (LACED, without_cues(read(LACED)), 26)];

        for (name, file, first) in files {
            let linear = packets(&mut open_bytes(file.clone()));
            let mut reader = open_bytes(file);
            next_packets(&mut reader, first);

            let to = SeekTo::Time { time: Time::from_millis(10_000), track_id: None };
            let seeked = reader.seek(SeekMode::Accurate, to);
            assert!(out_of_range(&seeked), "{name}: {seeked:?}");
            assert_same(&packets(&mut reader), &linear[first..], name);
        }

        let seek_back = |mut reader: MkvReader<'static>| {
            next_packets(&mut reader, 26);
            let to = SeekTo::Time { time: Time::from_millis(300), track_id: None };
            let seeked = reader.seek(SeekMode::Accurate, to).expect("a seek to 300 ms");
            (seeked.actual_ts, packets(&mut reader))
        };

        let (cued_ts, cued) = seek_back(open(LACED));
        let (actual_ts, after) = seek_back(open_bytes(without_cues(read(LACED))));
        assert_eq!(actual_ts, cued_ts);
        assert_same(&after, &cued, "a seek back to 300 ms");
    });
}

/// On a source that can't seek, a seek reads on from where the reader is. It reaches a target ahead
/// of the reader as a seek does on a source that can seek, its pre-roll as far as the reader has not
/// read past it. A target behind the reader, in a frame the reader read past, can't be reached: the
/// seek fails with `ForwardOnly`, and the reader reads on from where it was. Where the frame read
/// last states no duration, the seek reads the track's next frame to tell. A seek past the end
/// fails, the reader at the end.
#[test]
fn a_seek_on_a_source_that_cant_seek_reads_on_and_refuses_a_target_behind() {
    within_deadline(|| {
        // Opus, whose frames state their durations.
        let linear = packets(&mut open(OPUS));
        let probed = probed(OPUS);
        let mut reader = open_unseekable(NO_CUES);
        let track_id = reader.tracks()[0].id;

        let first = next_packets(&mut reader, 60);
        let last = first.last().unwrap();
        let end = last.pts.get() + last.dur.get() as i64;
        assert_eq!(linear[60].pts.get(), end, "the next frame begins where the last read ends");

        for ms in [0, last.pts.get(), end - 1] {
            let seeked = seek(&mut reader, track_id, ms);
            assert!(refused(&seeked), "a seek to {ms} ms: {seeked:?}");
        }

        // A seek to where the frame read last ends lands on the next frame, which begins there: the
        // reader has read past the whole pre-roll.
        let what = "a seek to where the reader is";
        let seeked = seek(&mut reader, track_id, end).expect(what);
        assert_eq!(seeked.actual_ts, Timestamp::new(end), "{what}");
        assert_same(&next_packets(&mut reader, 3), &linear[60..63], what);

        // A seek further ahead than the pre-roll lands on the frame that holds the target less the
        // pre-roll, and a seek past the end fails, the reader at the end.
        let at = probed.iter().position(|p| p.pts <= 1920 && 1920 < p.pts + p.dur).unwrap();
        let seeked = seek(&mut reader, track_id, 2000).expect("a seek to 2 s");
        assert_eq!(seeked.actual_ts, linear[at].pts);
        assert_same(&next_packets(&mut reader, 3), &linear[at..at + 3], "a seek to 2 s");

        let seeked = seek(&mut reader, track_id, 10_000);
        assert!(out_of_range(&seeked), "{seeked:?}");
        assert!(reader.next_packet().unwrap().is_none(), "the end");

        // PCM, whose frames state no duration. The frame read last, from 640 ms, ends where the next
        // begins, 768 ms, which a seek reads (or the timestamp of its cluster) to tell.
        let linear = packets(&mut open(PCM));
        let mut reader = open_unseekable(PCM);
        let track_id = reader.tracks()[0].id;

        let first = next_packets(&mut reader, 6);
        assert_eq!(first.last().unwrap().pts, Timestamp::new(640));

        for ms in [0, 639, 640, 767] {
            let seeked = seek(&mut reader, track_id, ms);
            assert!(refused(&seeked), "PCM: a seek to {ms} ms: {seeked:?}");
        }
        assert_same(&next_packets(&mut reader, 1), &linear[6..7], "PCM: the packet after");

        let seeked = seek(&mut reader, track_id, 1000).expect("PCM: a seek to 1 s");
        assert_eq!(seeked.actual_ts, Timestamp::new(896));
        assert_same(&packets(&mut reader), &linear[7..], "PCM: a seek to 1 s");
    });
}
