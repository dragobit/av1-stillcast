#!/usr/bin/env bash
# Player-level compat checks for stillcast outputs.
# Runs decode/seek smoke tests with whatever players are installed
# (ffplay, mpv, VLC). Missing players are reported SKIP, not FAIL.
# Usage: ./scripts/compat_players.sh [file.mp4]   (default: examples/demo.mp4)
set -u
cd "$(dirname "$0")/.."

MP4=${1:-examples/demo.mp4}
[ -f "$MP4" ] || { echo "missing $MP4"; exit 1; }
fail=0

check() { # name, rc
    if [ "$2" -eq 0 ]; then echo "PASS  $1"; else echo "FAIL  $1"; fail=1; fi
}

# --- ffplay (ffmpeg-native decode path, -ss mid-GOP) ---
if command -v ffplay >/dev/null; then
    timeout 60 ffplay -hide_banner -loglevel error -nodisp -autoexit \
        -ss 5 -t 3 "$MP4" </dev/null >/dev/null 2>&1
    check "ffplay -ss5 -t3" $?
else
    echo "SKIP  ffplay (not installed)"
fi

# --- mpv (libavformat + decoder of choice, mid-seek + short play) ---
if command -v mpv >/dev/null; then
    timeout 90 mpv --vo=null --ao=null --no-terminal --start=5 --length=3 \
        "$MP4" >/dev/null 2>&1
    check "mpv --start=5 --length=3" $?
else
    echo "SKIP  mpv (not installed)"
fi

# --- VLC (independent demux+decode stack, seek + playthrough) ---
if command -v cvlc >/dev/null; then
    timeout 90 cvlc --demux=mp4 --start-time=5 --stop-time=8 \
        --vout=dummy --aout=dummy --no-video-title-show --play-and-exit \
        "$MP4" </dev/null >/dev/null 2>&1
    check "cvlc --start-time=5 --stop-time=8" $?
else
    echo "SKIP  cvlc (not installed)"
fi

# --- ffprobe structure check (always available with ffmpeg) ---
if command -v ffprobe >/dev/null; then
    KEYS=$(ffprobe -v error -select_streams v \
        -show_entries packet=flags -of csv=p=0 "$MP4" | grep -c 'K_')
    echo "INFO  keyframe packets: $KEYS"
    [ "$KEYS" -ge 1 ]
    check "ffprobe stss entries" $?
fi

echo "RESULT: $([ $fail -eq 0 ] && echo PASS || echo FAIL)"
exit $fail
