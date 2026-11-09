#!/usr/bin/env bash
# Regenerate the ISO/MP4 test fixtures from synthetic FFmpeg lavfi sources. The tests check
# properties of the files (their lengths, trims, and headers), not their bytes, so any FFmpeg build
# with libopus and libx264 regenerates them. opus_mono.mp4 and opus_51.mp4 were made with FFmpeg
# 7.1 and libopus 1.5.2, the other files with FFmpeg n9.0.
#
#   bash symphonia-format-isomp4/tests/fixtures/regenerate.sh
#
# FFmpeg's MP4 muxer states the encoder delay and the presented duration of an audio track with an
# edit list, and the pre-roll of Opus and AAC with a roll recovery sample group.
#   - opus_mono.mp4: 1 s of mono 440 Hz at 48 kHz in Opus (a `dOps` of channel mapping family 0).
#   - opus_51.mp4: 1 s of 5.1 in Opus channel mapping family 1 (a `dOps` with a channel mapping
#     table: four streams, two coupled), a tone per speaker, beside a small H.264 picture.
#   - opus_frag.mp4: 1 s of mono 440 Hz in Opus, fragmented every 0.5 s after an empty `moov`. In a
#     fragmented file, FFmpeg's MP4 muxer states neither an edit list nor a roll recovery group.
#   - aac_stereo.m4a: 1 s of stereo (440 Hz left, 660 Hz right) at 48 kHz in AAC-LC.
#   - aac_96000.m4a, aac_88200.m4a: 0.5 s of mono 440 Hz at 96 and 88.2 kHz in AAC-LC. Above
#     65535 Hz, FFmpeg's MP4 muxer states half the rate in the sample entry.
#   - aac_24000.m4a: 0.5 s of mono 440 Hz at 24 kHz in AAC-LC, the core of the implicit-SBR file
#     an AAC test makes of it.

set -euo pipefail

cd "$(dirname "$0")"

FFMPEG="${FFMPEG:-ffmpeg}"
COMMON=(-loglevel error -nostdin -hide_banner -y)
EXACT=(-fflags +bitexact -flags:a +bitexact -map_metadata -1)

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=1:sample_rate=48000" \
    -c:a libopus -b:a 64k "${EXACT[@]}" -f mp4 opus_mono.mp4

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "testsrc=duration=1:size=160x90:rate=25" \
    -f lavfi -i "aevalsrc=0.4*sin(2*PI*300*t)|0.4*sin(2*PI*500*t)|0.4*sin(2*PI*700*t)|0.4*sin(2*PI*50*t)|0.4*sin(2*PI*900*t)|0.4*sin(2*PI*1100*t):s=48000:d=1:c=5.1" \
    -c:v libx264 -preset veryfast -crf 35 -pix_fmt yuv420p -g 25 -bf 0 -flags:v +bitexact \
    -c:a libopus -mapping_family 1 -b:a 256k "${EXACT[@]}" -f mp4 opus_51.mp4

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=1:sample_rate=48000" \
    -c:a libopus -b:a 32k "${EXACT[@]}" \
    -movflags +frag_keyframe+empty_moov+default_base_moof -frag_duration 500000 -f mp4 opus_frag.mp4

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*660*t):s=48000:d=1:c=stereo" \
    -c:a aac -b:a 128k "${EXACT[@]}" -f mp4 aac_stereo.m4a

for rate in 96000 88200 24000; do
    "$FFMPEG" "${COMMON[@]}" \
        -f lavfi -i "sine=frequency=440:duration=0.5:sample_rate=$rate" \
        -c:a aac -b:a 32k "${EXACT[@]}" -f mp4 "aac_$rate.m4a"
done
