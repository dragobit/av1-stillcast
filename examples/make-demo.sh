#!/usr/bin/env bash
# Reproduces examples/demo.mp4: jacket.jpg + generated chord -> 30s AV1/AAC mp4.
# Requires: cargo build --release, ffmpeg.
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Three sine tones (A4 + C#5 + E5) -> 30 s AAC (ADTS).
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "sine=frequency=440:duration=30[a];sine=frequency=554.37:duration=30[b];sine=frequency=659.25:duration=30[c];[a][b][c]amix=inputs=3,volume=0.35" \
    -c:a aac -b:a 96k -f adts -y "$WORK/demo.aac"

./target/release/stillcast make \
    -i examples/jacket.jpg -a "$WORK/demo.aac" -o examples/demo.mp4
