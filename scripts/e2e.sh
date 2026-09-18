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

echo "== --decoder-model: decoder_model_info + buffer_removal_time_present_flag"
cargo run --quiet -- assemble -i "$WORK/src.ivf" -o "$WORK/dm.ivf" \
    --frames 900 --gop 300 --decoder-model
# ffmpeg's strict cbs parser must accept every OBU (regression: stray '1'
# in byte_alignment used to trip "zero_bit out of range")
ffmpeg -hide_banner -c:v libdav1d -i "$WORK/dm.ivf" -f null - \
    2> "$WORK/dm.log" || true
if grep -qE "out of range|Failed to (read|parse)" "$WORK/dm.log"; then
    cat "$WORK/dm.log"
    exit 1
fi
DECODED=$(ffmpeg -hide_banner -c:v libdav1d -i "$WORK/dm.ivf" \
    -f null - 2>&1 | grep -oE 'frame= *[0-9]+' | tail -1 | grep -oE '[0-9]+')
echo "decoded frames: $DECODED (expected 900)"
[ "$DECODED" = "900" ]
# pixel-identical to the non-DM stream
ffmpeg -hide_banner -loglevel error -c:v libdav1d -i "$WORK/out.ivf" \
    -f framemd5 "$WORK/out.md5"
ffmpeg -hide_banner -loglevel error -c:v libdav1d -i "$WORK/dm.ivf" \
    -f framemd5 "$WORK/dm.md5"
diff <(awk '{print $6}' "$WORK/out.md5") <(awk '{print $6}' "$WORK/dm.md5")

echo "== structure verification: info --check (stillcast invariants)"
cargo run --quiet -- info -i "$WORK/out.ivf" --check
cargo run --quiet -- info -i "$WORK/dm.ivf" --check
# mp4 input path (demuxes internally), and keyframe positions must land
# at the GOP boundary (gop=300 → TUs 0,300,600)
cargo run --quiet -- info -i "$WORK/out.mp4" --check | tee "$WORK/info.txt"
grep -q 'seek points) at \[0, 300, 600\]' "$WORK/info.txt"

echo "== remux + seek sanity"
ffmpeg -hide_banner -loglevel error -ss 20 -i "$WORK/out.mp4" -frames:v 1 -f null -

echo "== seek lands on prior keyframe, output starts at target"
# -ss mid-GOP + -copyts: first output pts must be the requested time
# (demuxer used stss to land on the previous keyframe, decoded forward)
FIRST=$(ffmpeg -hide_banner -ss 21.5 -copyts -i "$WORK/out.mp4" \
    -vf showinfo -frames:v 1 -f null - 2>&1 | grep -oE 'pts_time:[0-9.]+' \
    | head -1 | cut -d: -f2)
echo "first output pts after -ss 21.5: $FIRST"
[ "$FIRST" = "21.5" ]
KEYS=$(ffprobe -v error -select_streams v -show_entries packet=pts_time,flags \
    -of csv=p=0 "$WORK/out.mp4" | grep -c 'K_')
echo "keyframe packets: $KEYS (expected 3: gop 300 over 900 frames)"
[ "$KEYS" = "3" ]

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

echo "== --playlist: two images, keyframe at the switch"
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "color=c=blue:size=320x180" -frames:v 1 -y "$WORK/still2.png"
ffmpeg -hide_banner -loglevel error -loop 1 -i "$WORK/still2.png" \
    -vf format=yuv420p -c:v libaom-av1 -crf 32 -b:v 0 -cpu-used 8 \
    -r 30 -frames:v 4 -y "$WORK/src2.ivf"
printf '%s 5\n%s\n' "$WORK/src.ivf" "$WORK/src2.ivf" > "$WORK/list.txt"
cargo run --quiet -- assemble --playlist "$WORK/list.txt" -o "$WORK/pl.mp4" \
    --duration 12 --fps 30 --gop 300 --audio "$WORK/audio.aac"
# 360 frames total; keyframes at t=0 and the t=5s switch
DECODED=$(ffmpeg -hide_banner -i "$WORK/pl.mp4" -map 0:v:0 -f null - \
    2>&1 | grep -oE 'frame= *[0-9]+' | tail -1 | grep -oE '[0-9]+')
echo "decoded frames: $DECODED (expected 360)"
[ "$DECODED" = "360" ]
ffprobe -v error -select_streams v -show_entries packet=pts_time,flags \
    -of csv=p=0 "$WORK/pl.mp4" | grep 'K_' > "$WORK/keys.txt"
grep -q '^5\.0*0*,K_' "$WORK/keys.txt"
[ "$(wc -l < "$WORK/keys.txt")" = "2" ]
# pixels actually change: segment hashes must differ, and the second
# segment's hash must equal a pure-src2 assembly's hash
cargo run --quiet -- assemble -i "$WORK/src2.ivf" -o "$WORK/b.ivf" \
    --frames 4 --gop 4
ffmpeg -hide_banner -loglevel error -c:v libdav1d -i "$WORK/pl.mp4" \
    -f framemd5 "$WORK/pl.md5"
ffmpeg -hide_banner -loglevel error -c:v libdav1d -i "$WORK/b.ivf" \
    -f framemd5 "$WORK/b.md5"
B_HASH=$(awk '$1=="0,"{print $6; exit}' "$WORK/b.md5")
awk -v b="$B_HASH" 'BEGIN{n=0} $1=="0," {n++; if (n==1) h1=$6; if (n==200 && $6!=b) exit 1} END{if (h1==b) exit 1}' "$WORK/pl.md5"
# seek mid-playlist lands at the 5s keyframe and shows image 2
ffmpeg -hide_banner -loglevel error -ss 6.5 -c:v libdav1d -i "$WORK/pl.mp4" \
    -f framemd5 "$WORK/ss.md5"
SS_HASH=$(awk '$1=="0,"{print $6; exit}' "$WORK/ss.md5")
[ "$SS_HASH" = "$B_HASH" ]
# structural check: 2 shown keyframes at TUs 0 and 150, all else show_existing
cargo run --quiet -- info -i "$WORK/pl.mp4" --check | tee "$WORK/plinfo.txt"
grep -q 'seek points) at \[0, 150\]' "$WORK/plinfo.txt"
grep -q '356 show_existing' "$WORK/plinfo.txt"
echo "playlist OK: switch at 5s keyframe, pixels verified"

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
