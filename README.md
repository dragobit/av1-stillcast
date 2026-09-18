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

## Status

Early prototype. Produces IVF (`.ivf`) AV1 elementary streams; mux with
ffmpeg (`ffmpeg -i out.ivf -c copy out.mkv/out.mp4`). Verified to decode
frame-exact with libdav1d.

## Usage

```bash
# 1. Produce the source frames: a keyframe + one inter frame of the still.
ffmpeg -loop 1 -i jacket.png -vf format=yuv420p \
    -c:v libaom-av1 -crf 32 -b:v 0 -r 30 -frames:v 2 src.ivf

# 2. Inspect the input (finds the golden reference slot).
stillcast info -i src.ivf

# 3. Expand to a 1-hour video: 30 fps, keyframe every 300 frames (10 s seek).
stillcast assemble -i src.ivf -o out.ivf --duration 3600 --gop 300

# 4. Mux with audio.
ffmpeg -i out.ivf -i podcast.opus -c copy episode.mkv
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
- [ ] MP4/ISOBMFF output (`av01` sample entry, sync-sample table)
- [ ] WebM/MKV output
- [ ] Optional encode step (call libaom/ffmpeg directly on a still image)
- [ ] Decoder-model / `temporal_point_info` support
- [ ] Compatibility matrix: hw decoders, browsers, mobile players
- [ ] "Limit-tracer" mode: pick gop/quality automatically from size or
      seek-granularity targets

## License

MIT
