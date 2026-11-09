#!/usr/bin/env bash
# Regenerate the Ogg Opus test fixtures from synthetic FFmpeg lavfi sources with FFmpeg's libopus
# encoder. The tests check properties of the files (their lengths, trims, and channels), not their
# bytes, so any FFmpeg build with libopus regenerates them. Generated with FFmpeg n9.0.
#
#   bash symphonia-format-ogg/tests/fixtures/regenerate.sh
#
#   - opus_stereo.opus: 1 s of stereo (440 Hz left, 660 Hz right) at 48 kHz in pages of 200 ms, so
#     the stream starts and ends on different pages.
#   - opus_short.opus: 250 ms of mono 440 Hz at 48 kHz, a stream of a single page.
#   - opus_foa.opus: 1 s of a first-order ambisonic field (AmbiX: W Y Z X) in channel mapping
#     family 2 (RFC 8486).
#   - opus_discrete.opus: 1 s of 3 channels in channel mapping family 255 (RFC 7845, §5.1.1.3).
#   - opus_big_pages.opus: 2.2 s of stereo at 510 kbps CBR in full pages (about 42 packets, 54 KB),
#     so the stream is larger than two maximum Ogg pages (a seek bisects it) and its last page (26
#     packets, with the end padding) is one the bisection probes.
#   - opus_small_pages.opus: 2.1 s of stereo at 510 kbps CBR in pages of one 20 ms packet, larger
#     than two maximum Ogg pages. The tests patch its pre-skip to span many pages; at 65535 frames
#     the pre-skip is the first ~89 KB, where a seek's bisection lands past the first page.

set -euo pipefail

cd "$(dirname "$0")"

FFMPEG="${FFMPEG:-ffmpeg}"
COMMON=(-loglevel error -nostdin -hide_banner -y)
EXACT=(-fflags +bitexact -flags:a +bitexact -map_metadata -1)

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*660*t):s=48000:d=1:c=stereo" \
    -c:a libopus -b:a 96k "${EXACT[@]}" -page_duration 200000 -f ogg opus_stereo.opus

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=0.25:sample_rate=48000" \
    -c:a libopus -b:a 64k "${EXACT[@]}" -f ogg opus_short.opus

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.4*sin(2*PI*300*t)|0.4*sin(2*PI*500*t)|0.4*sin(2*PI*700*t)|0.4*sin(2*PI*900*t):s=48000:d=1:c=ambisonic 1" \
    -c:a libopus -mapping_family 2 -b:a 192k "${EXACT[@]}" -f ogg opus_foa.opus

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.4*sin(2*PI*300*t)|0.4*sin(2*PI*500*t)|0.4*sin(2*PI*700*t):s=48000:d=1:c=3c" \
    -c:a libopus -mapping_family 255 -b:a 144k "${EXACT[@]}" -f ogg opus_discrete.opus

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*660*t):s=48000:d=2.2:c=stereo" \
    -c:a libopus -b:a 510k -vbr off "${EXACT[@]}" -f ogg opus_big_pages.opus

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*660*t):s=48000:d=2.1:c=stereo" \
    -c:a libopus -b:a 510k -vbr off "${EXACT[@]}" -page_duration 20000 -f ogg opus_small_pages.opus
