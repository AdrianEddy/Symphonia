#!/usr/bin/env bash
# Regenerate the Matroska test fixtures from synthetic FFmpeg lavfi sources, Opus with FFmpeg's
# libopus encoder. The tests check properties of the files (their lengths, trims, and what a seek
# lands on), not their bytes, so any FFmpeg build with libopus regenerates them. The checked-in
# opus_stereo.mka and opus_51.mka were made with FFmpeg 7.1 and libopus 1.5.2, the others with
# FFmpeg n9.0 (and Python 3 for lace.py). A `<fixture>.ffprobe.csv` lists the packets of a fixture
# as ffprobe reads them (track index, timestamp and duration in milliseconds), the times the
# tests expect: regenerate it with its fixture.
#
#   bash symphonia-format-mkv/tests/fixtures/regenerate.sh
#
# FFmpeg's Matroska muxer states the Opus pre-skip as the track's `CodecDelay` (6.5 ms, 312
# frames), an 80 ms `SeekPreRoll`, and the padding of the last block as its `DiscardPadding`. It
# writes Cues with CueRelativePosition and, in a file with video, cues only the video track.
#   - opus_stereo.mka: 1 s of stereo (440 Hz left, 660 Hz right) at 48 kHz.
#   - opus_51.mka: 1 s of 5.1 in channel mapping family 1 (four streams, two coupled), a tone per
#     speaker.
#   - opus_laced.mka: opus_stereo.mka with its blocks Xiph-laced, 5 frames to a block, and a cue
#     point without CueRelativePosition (lace.py: FFmpeg's muxer never laces).
#   - opus_offset.mka: opus_stereo.mka's track stored from 100 ms, where its only cue point is.
#   - opus_late_audio.mkv: 16x16 black MJPEG at 5 fps from 0 to 2 s, and opus_stereo.mka's track
#     from 1 s, in clusters of at most 0.5 s (five): the track's first block is in the second
#     cluster, after the video's cue points at 600 and 800 ms.
#   - opus_late_video.mkv: opus_stereo.mka's track from 0, and 16x16 black MJPEG at 5 fps from 0.6
#     s, so the first cue point is in the second cluster, 0.6 s into the Opus track.
#   - opus_clusters.mka: 2.5 s of mono 440 Hz in clusters of at most 0.5 s: six, the last of two
#     blocks, each cued at its first block.
#   - mjpeg_opus_clusters.mkv: 16x16 black MJPEG at 5 fps and opus_clusters.mka's track, in six
#     clusters: every video frame is cued, mostly inside a cluster, after blocks of the Opus track.
#   - opus_no_cues.mka: opus_clusters.mka written through a pipe, so with no Cues and a Segment of
#     unknown size.
#   - pcm_clusters.mka: 2.5 s of mono 16-bit PCM at 8 kHz in packets of 1024 samples (128 ms), in
#     clusters of at most 0.5 s: seven, the last of two blocks, each cued at its first block. FFmpeg
#     states no duration for PCM: no BlockDuration, no DefaultDuration.
#   - mjpeg_pcm_clusters.mkv: 16x16 black MJPEG at 5 fps for 2.5 s and pcm_clusters.mka's track for
#     1.5 s, in six clusters: every video frame is cued, and the last cluster holds only video.
#   - mjpeg_2pcm_clusters.mkv: 16x16 black MJPEG at 5 fps for 2 s and two tracks of mono 16-bit PCM
#     at 4 kHz (440 and 660 Hz) for 1.5 s in packets of 512 samples (128 ms), in five clusters:
#     every video frame is cued. FFmpeg's muxer holds back one audio packet to open the cluster of a
#     video keyframe, the last track's, so the other track's packet that spans the keyframe ends the
#     cluster before.

set -euo pipefail

cd "$(dirname "$0")"

FFMPEG="${FFMPEG:-ffmpeg}"
FFPROBE="${FFPROBE:-ffprobe}"
PYTHON="${PYTHON:-python3}"
COMMON=(-loglevel error -nostdin -hide_banner -y)
EXACT=(-fflags +bitexact -flags:a +bitexact -map_metadata -1)
STEREO="aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*660*t):s=48000:d=1:c=stereo"
BLACK="color=black:s=16x16:r=5"
MONO="sine=frequency=440:duration=2.5:sample_rate=48000"
PCM="sine=frequency=440:sample_rate=8000"

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$STEREO" \
    -c:a libopus -b:a 96k "${EXACT[@]}" -f matroska opus_stereo.mka

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.4*sin(2*PI*300*t)|0.4*sin(2*PI*500*t)|0.4*sin(2*PI*700*t)|0.4*sin(2*PI*50*t)|0.4*sin(2*PI*900*t)|0.4*sin(2*PI*1100*t):s=48000:d=1:c=5.1" \
    -c:a libopus -mapping_family 1 -b:a 256k "${EXACT[@]}" -f matroska opus_51.mka

"$PYTHON" lace.py opus_stereo.mka opus_laced.mka 5

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$STEREO" \
    -c:a libopus -b:a 96k "${EXACT[@]}" -output_ts_offset 0.1 -f matroska opus_offset.mka

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$BLACK:d=2" -itsoffset 1 -f lavfi -i "$STEREO" \
    -c:v mjpeg -flags:v +bitexact -c:a libopus -b:a 96k "${EXACT[@]}" \
    -cluster_time_limit 500 -f matroska opus_late_audio.mkv

"$FFMPEG" "${COMMON[@]}" \
    -itsoffset 0.6 -f lavfi -i "$BLACK:d=0.4" -f lavfi -i "$STEREO" \
    -c:v mjpeg -flags:v +bitexact -c:a libopus -b:a 96k "${EXACT[@]}" \
    -f matroska opus_late_video.mkv

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$MONO" \
    -c:a libopus -b:a 32k "${EXACT[@]}" -cluster_time_limit 500 -f matroska opus_clusters.mka

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$BLACK:d=2.5" -f lavfi -i "$MONO" \
    -c:v mjpeg -flags:v +bitexact -c:a libopus -b:a 32k "${EXACT[@]}" \
    -cluster_time_limit 500 -f matroska mjpeg_opus_clusters.mkv

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$MONO" \
    -c:a libopus -b:a 32k "${EXACT[@]}" -cluster_time_limit 500 -f matroska pipe:1 > opus_no_cues.mka

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$PCM:duration=2.5" \
    -c:a pcm_s16le "${EXACT[@]}" -cluster_time_limit 500 -f matroska pcm_clusters.mka

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$BLACK:d=2.5" -f lavfi -i "$PCM:duration=1.5" \
    -c:v mjpeg -flags:v +bitexact -c:a pcm_s16le "${EXACT[@]}" \
    -cluster_time_limit 500 -f matroska mjpeg_pcm_clusters.mkv

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "$BLACK:d=2" \
    -f lavfi -i "sine=frequency=440:sample_rate=4000:samples_per_frame=512:duration=1.5" \
    -f lavfi -i "sine=frequency=660:sample_rate=4000:samples_per_frame=512:duration=1.5" \
    -map 0 -map 1 -map 2 -c:v mjpeg -flags:v +bitexact -c:a pcm_s16le "${EXACT[@]}" \
    -cluster_time_limit 500 -f matroska mjpeg_2pcm_clusters.mkv

for fixture in opus_clusters.mka mjpeg_opus_clusters.mkv pcm_clusters.mka mjpeg_pcm_clusters.mkv \
    mjpeg_2pcm_clusters.mkv; do
    "$FFPROBE" -v error -show_entries packet=stream_index,pts,duration -of csv=p=0 "$fixture" \
        | tr -d '\r' > "$fixture.ffprobe.csv"
done
