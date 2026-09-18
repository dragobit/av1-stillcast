# av1-stillcast (stillcast)

An **AV1 bitstream assembler** for static-image videos — podcast episodes and
music tracks that show the same jacket art / thumbnail for their whole
duration.

Instead of re-encoding every frame, `stillcast` takes a handful of real coded
frames from a standard encoder (libaom) and expands them into a long stream
where each temporal unit is just a `show_existing_frame` instruction pointing
at a stored "golden" frame. The result is a fully spec-legal AV1 bitstream
that plays in any conformant decoder, at roughly **6 bytes per frame** for the
repeated portion.

## Why

For a static-image video, a regular encoder emits a tiny skip P-frame every
frame (~20–40 B) and, more importantly, offers little control over GOP
structure vs. container/decoder behavior. `stillcast` exposes the real
trade-off knob directly:

- **File size** — show_existing_frame TUs cost ~6 B/frame (≈1.4 kbps at 30 fps).
  A 1-hour podcast's video track becomes smaller than a second of audio.
- **Seek granularity** — keyframes every `--gop` frames are the only random
  access points. Tune the size/seek frontier with one parameter.
- **Compatibility** — output is ordinary AV1 (no exotic profiles, normal
  resolution & fps), decodable by dav1d, libaom, and hardware decoders.

See [`docs/spec-notes.md`](docs/spec-notes.md) for the AV1 spec analysis that
makes this legal, and [`docs/design.md`](docs/design.md) for the architecture.

**Demo:** [`examples/demo.mp4`](examples/demo.mp4) — 30 s, 640×640@30, AV1 +
AAC, 193 KB (rebuild with `./examples/make-demo.sh`).

## Status

Working prototype. Produces IVF (`.ivf`) elementary streams and **MP4**
(av01 video track + optional mp4a/AAC audio track — YouTube-compatible
layout), verified to decode frame-exact with libdav1d. Assembly is fully
deterministic (same input → byte-identical output) and does no AV1
compression work of its own — a 1-hour @30fps stream expands in ~30 ms.

## Usage

The main flow is **image + audio → video**. One command (requires ffmpeg on
PATH for the real-frame encode and audio handling):

```bash
# jacket art + podcast audio -> 1-hour YouTube-ready mp4.
stillcast make -i jacket.png -a podcast.m4a -o episode.mp4 --gop 300
# duration defaults to the audio duration; --fps/--crf/--audio-bitrate tunable

# size/seek frontier for this input before choosing:
stillcast plan -i src.ivf --duration 3600
# ...or just state the seek requirement and let it pick gop:
stillcast make -i jacket.png -a podcast.m4a -o episode.mp4 --target-seek 5
# ...or state a size budget (raises crf until it fits):
stillcast make -i jacket.png -a podcast.m4a -o episode.mp4 --max-size 500MB
```

The explicit pipeline — drive libaom yourself, then assemble — stays
first-class (full control over the encode, encoder swaps, scripting):

```bash
# 1. Produce the source frames: a keyframe + one inter frame of the still.
ffmpeg -loop 1 -i jacket.png -vf format=yuv420p \
    -c:v libaom-av1 -crf 32 -b:v 0 -r 30 -frames:v 2 src.ivf

# 2. Inspect the input (finds the golden reference slot).
stillcast info -i src.ivf

# 3a. Expand to IVF (1 hour, 30 fps, keyframe every 300 frames = 10 s seek).
stillcast assemble -i src.ivf -o out.ivf --duration 3600 --gop 300

# 3b. Or go straight to MP4 with audio (any format ffmpeg reads).
stillcast assemble -i src.ivf -o episode.mp4 \
    --duration 3600 --gop 300 --audio podcast.m4a

# 3c. Size budget: raise gop until the file fits (degrades seek).
stillcast assemble -i src.ivf -o out.ivf --duration 3600 --max-size 2MB

# IVF output can also be remuxed with plain ffmpeg.
ffmpeg -i out.ivf -c copy out.mkv
```

Each emitted GOP is: keyframe TU → golden TU → `show_existing_frame` TU ×
(gop−2). The golden is re-shown for every remaining frame; at each GOP
boundary a new coded video sequence restarts the pattern, giving uniform
seek points.

## Build & test

```bash
cargo build --release
cargo test            # unit tests
./scripts/e2e.sh      # end-to-end: encode → assemble → dav1d decode-verify
```

## Roadmap

- [x] OBU / sequence header / uncompressed header parsing
- [x] show_existing_frame TU synthesis + GOP assembler + IVF I/O
- [x] MP4/ISOBMFF output (`av01` + `av1C`, `stss` sync table) + AAC mux
- [x] `stillcast make`: image + audio → video in one command (drives ffmpeg
      for the encode/audio conversion)
- [x] Audio input: any ffmpeg-readable format → AAC (both `make` and
      `assemble --audio`)
- [ ] WebM/MKV output
- [ ] Decoder-model / `temporal_point_info` support
- [x] Limit-tracer: `stillcast plan` size/seek table, `--target-seek N`
      (gop = N×fps), `--max-size` (assemble: gop growth, make: crf ladder)
- [ ] Compatibility matrix: hw decoders, browsers, mobile players
      → [`docs/compat.md`](docs/compat.md)
- [ ] Multi-image playlists (multiple goldens, timed image switches)
- [ ] Long-term: same transformation as an **ffmpeg bitstream filter**
      (`av1_stillcast` bsf) — an *additional* path, not a replacement for
      the CLI flow above

## License

MIT
