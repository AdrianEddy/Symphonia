#!/usr/bin/env bash
# Regenerate the Ogg test fixtures from synthetic FFmpeg lavfi sources with FFmpeg's libopus,
# libvorbis, libtheora and FLAC encoders. The tests check properties of the files (their lengths,
# trims, channels, and what a seek lands on), not their bytes, so any FFmpeg build with these
# encoders regenerates them. Generated with FFmpeg n9.0.
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
#   - opus_late.opus: 1 s of mono 440 Hz written 0.5 s late: the first page's granule position is
#     0.5 s past its samples (RFC 7845, §4.5).
#   - opus_vorbis.ogg: 2 s of mono 440 Hz in Opus (serial 0) and of mono 660 Hz at 44.1 kHz in
#     Vorbis (serial 1), in pages of 100 ms, interleaved.
#   - opus_flac.ogg: 2 s of mono 440 Hz in Opus (serial 0) and of mono 660 Hz at 16 kHz in FLAC
#     (serial 1), in pages of 100 ms, interleaved.
#   - opus_opus.ogg: 2.1 s of mono 440 Hz in Opus at 32 kbps (serial 0) and of stereo at 510 kbps
#     CBR (serial 1), in pages of 100 ms, interleaved: larger than two maximum Ogg pages, so a seek
#     bisects it.
#   - theora_opus.ogg: 2 s of 16x16 black Theora at 5 fps (serial 7, the lowest) and of mono 440 Hz
#     in Opus (serial 8), in pages of at most 100 ms.
#   - vorbis_bursts.ogg: 1 s of mono 440 Hz in bursts of 125 ms at 44.1 kHz in Vorbis q0: libvorbis
#     switches between its short and long blocks (256 and 2048 frames) at each burst's start and
#     end. The end probe's tests paginate its packets again.

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

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=1:sample_rate=48000" \
    -c:a libopus -b:a 32k "${EXACT[@]}" -output_ts_offset 0.5 -f ogg opus_late.opus

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" \
    -f lavfi -i "sine=frequency=660:duration=2:sample_rate=44100" \
    -map 0 -map 1 -c:a:0 libopus -b:a:0 32k -c:a:1 libvorbis -q:a:1 0 "${EXACT[@]}" \
    -page_duration 100000 -f ogg opus_vorbis.ogg

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" \
    -f lavfi -i "sine=frequency=660:duration=2:sample_rate=16000" \
    -map 0 -map 1 -c:a:0 libopus -b:a:0 32k -c:a:1 flac "${EXACT[@]}" \
    -page_duration 100000 -f ogg opus_flac.ogg

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "sine=frequency=440:duration=2.1:sample_rate=48000" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*660*t):s=48000:d=2.1:c=stereo" \
    -map 0 -map 1 -c:a libopus -b:a:0 32k -b:a:1 510k -vbr:a:1 off "${EXACT[@]}" \
    -page_duration 100000 -f ogg opus_opus.ogg

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "color=black:s=16x16:r=5:d=2" \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" \
    -map 0 -map 1 -c:v libtheora -flags:v +bitexact -c:a libopus -b:a 32k "${EXACT[@]}" \
    -serial_offset 7 -page_duration 100000 -f ogg theora_opus.ogg

"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i "aevalsrc=0.8*sin(2*PI*440*t)*lt(mod(t\,0.25)\,0.125):s=44100:d=1" \
    -c:a libvorbis -q:a 0 "${EXACT[@]}" -f ogg vorbis_bursts.ogg
