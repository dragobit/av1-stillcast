#!/usr/bin/env bash
# End-to-end check: encode a still → assemble → decode-verify with libdav1d.
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "== generate still"
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "testsrc2=size=320x180:rate=1:duration=1" -frames:v 1 "$WORK/still.png"

echo "== encode source frames (KF + golden)"
ffmpeg -hide_banner -loglevel error -loop 1 -i "$WORK/still.png" \
    -vf format=yuv420p -c:v libaom-av1 -crf 32 -b:v 0 -cpu-used 8 \
    -r 30 -frames:v 4 "$WORK/src.ivf"

echo "== stillcast info"
cargo run --quiet -- info -i "$WORK/src.ivf"

echo "== stillcast assemble"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/out.ivf" \
    --frames 900 --gop 300

echo "== decode-verify with libdav1d"
DECODED=$(ffmpeg -hide_banner -c:v libdav1d -i "$WORK/out.ivf" \
    -f null - 2>&1 | grep -oE 'frame= *[0-9]+' | tail -1 | grep -oE '[0-9]+')
echo "decoded frames: $DECODED (expected 900)"
[ "$DECODED" = "900" ]

echo "== remux + seek sanity"
ffmpeg -hide_banner -loglevel error -i "$WORK/out.ivf" -c copy -y "$WORK/out.mkv"
ffmpeg -hide_banner -loglevel error -ss 20 -i "$WORK/out.mkv" -frames:v 1 -f null -

echo "E2E OK"
