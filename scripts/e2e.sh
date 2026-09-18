#!/usr/bin/env bash
# End-to-end check: encode a still → assemble (ivf + mp4+audio) → verify.
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "== generate still + audio"
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "testsrc2=size=320x180:rate=1:duration=1" -frames:v 1 "$WORK/still.png"
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "sine=frequency=440:duration=30" -c:a aac -b:a 96k -f adts -y "$WORK/audio.aac"

echo "== encode source frames (KF + golden)"
ffmpeg -hide_banner -loglevel error -loop 1 -i "$WORK/still.png" \
    -vf format=yuv420p -c:v libaom-av1 -crf 32 -b:v 0 -cpu-used 8 \
    -r 30 -frames:v 4 "$WORK/src.ivf"

echo "== stillcast info"
cargo run --quiet -- info -i "$WORK/src.ivf"

echo "== stillcast assemble → ivf"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/out.ivf" \
    --frames 900 --gop 300

echo "== decode-verify with libdav1d"
DECODED=$(ffmpeg -hide_banner -c:v libdav1d -i "$WORK/out.ivf" \
    -f null - 2>&1 | grep -oE 'frame= *[0-9]+' | tail -1 | grep -oE '[0-9]+')
echo "decoded frames: $DECODED (expected 900)"
[ "$DECODED" = "900" ]

echo "== stillcast assemble → mp4 (av01 + aac)"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/out.mp4" \
    --frames 900 --gop 300 --audio "$WORK/audio.aac"
ffprobe -v error -show_entries stream=codec_name,nb_frames -of csv=p=0 \
    "$WORK/out.mp4" | tee "$WORK/streams.txt"
grep -q '^av1,900$' "$WORK/streams.txt"

echo "== mp4 decode-verify"
DECODED=$(ffmpeg -hide_banner -i "$WORK/out.mp4" -map 0:v:0 -f null - \
    2>&1 | grep -oE 'frame= *[0-9]+' | tail -1 | grep -oE '[0-9]+')
echo "decoded frames: $DECODED (expected 900)"
[ "$DECODED" = "900" ]

echo "== remux + seek sanity"
ffmpeg -hide_banner -loglevel error -ss 20 -i "$WORK/out.mp4" -frames:v 1 -f null -

echo "== stillcast make (image + audio -> mp4 in one step)"
cargo run --quiet -- make -i "$WORK/still.png" -a "$WORK/audio.aac" \
    -o "$WORK/made.mp4" --gop 300
ffprobe -v error -show_entries stream=codec_name -of csv=p=0 \
    "$WORK/made.mp4" | tee "$WORK/made.txt"
grep -q '^av1$' "$WORK/made.txt"
grep -q '^aac$' "$WORK/made.txt"
DECODED=$(ffmpeg -hide_banner -i "$WORK/made.mp4" -map 0:v:0 -f null - \
    2>&1 | grep -oE 'frame= *[0-9]+' | tail -1 | grep -oE '[0-9]+')
echo "decoded frames: $DECODED (expected ~900)"
[ "$DECODED" -ge 890 ]

echo "== stillcast plan + --target-seek"
cargo run --quiet -- plan -i "$WORK/src.ivf" --duration 60 | tee "$WORK/plan.txt"
grep -q 'repeat TU' "$WORK/plan.txt"
cargo run --quiet -- make -i "$WORK/still.png" -a "$WORK/audio.aac" \
    -o "$WORK/ts.mp4" --target-seek 5 --duration 10
ffprobe -v error -show_entries stream=nb_frames -select_streams v \
    -of csv=p=0 "$WORK/ts.mp4" | grep -q '^300$'

echo "== --max-size + non-ADTS audio"
ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=330:duration=10" \
    -c:a aac -b:a 128k -y "$WORK/audio.m4a"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/ms.mp4" \
    --frames 300 --gop 150 --audio "$WORK/audio.m4a"
ffprobe -v error -show_entries stream=codec_name -of csv=p=0 \
    "$WORK/ms.mp4" | grep -q '^aac$'
MS_OUT=$(cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/ms2.ivf" \
    --duration 3600 --gop 300 --max-size 2MB 2>&1)
echo "$MS_OUT" | grep -q 'raised gop'
S2=$(stat -c%s "$WORK/ms2.ivf")
echo "max-size output: $S2 B (<= 2MB expected)"
[ "$S2" -le 2097152 ]
cargo run --quiet -- make -i "$WORK/still.png" -a "$WORK/audio.m4a" \
    -o "$WORK/mm.mp4" --max-size 400KB --duration 10
S3=$(stat -c%s "$WORK/mm.mp4")
[ "$S3" -le 409600 ]

echo "== determinism: two runs must be byte-identical"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/a.ivf" --frames 300 --gop 300
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/b.ivf" --frames 300 --gop 300
cmp "$WORK/a.ivf" "$WORK/b.ivf"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/a.mp4" \
    --frames 300 --gop 300 --audio "$WORK/audio.aac"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/b.mp4" \
    --frames 300 --gop 300 --audio "$WORK/audio.aac"
cmp "$WORK/a.mp4" "$WORK/b.mp4"
echo "deterministic OK"

echo "== speed check: assemble 1 hour @30fps"
time cargo run --quiet --release -- assemble -i "$WORK/src.ivf" \
    -o "$WORK/big.ivf" --duration 3600 --gop 300
ls -lh "$WORK/big.ivf"

echo "E2E OK"
