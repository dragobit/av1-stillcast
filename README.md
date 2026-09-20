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

The explicit pipeline — drive libaom yourself, then expand — stays
first-class (full control over the encode, encoder swaps, scripting).
All of `encode`, `expand`, `plan`, and `info` accept `-i -` / `-o -`
for stdin/stdout, so they compose in shell pipelines with ffmpeg:

```bash
# 1. Produce the source frames: a keyframe + one inter frame of the still.
ffmpeg -loop 1 -i jacket.png -vf format=yuv420p \
    -c:v libaom-av1 -crf 32 -b:v 0 -r 30 -frames:v 2 src.ivf

# 2. Inspect the input (finds the golden reference slot).
stillcast info -i src.ivf

# Input containers: expand/plan/info accept IVF, raw OBU streams
# (aomenc --obu, SVT-AV1, WebCodecs chunks) and Annex-B natively —
# no ffmpeg needed for the demux. mp4/mkv inputs can be remuxed with
# `ffmpeg -i in -c:v copy -f ivf out.ivf`; `info` reads them directly.

# 3a. Expand to IVF (1 hour, 30 fps, keyframe every 300 frames = 10 s seek).
stillcast expand -i src.ivf -o out.ivf --duration 3600 --gop 300

# 3b. Or go straight to MP4 with audio (any format ffmpeg reads).
stillcast expand -i src.ivf -o episode.mp4 \
    --duration 3600 --gop 300 --audio podcast.m4a

# 3c. Size budget: raise gop until the file fits (degrades seek).
stillcast expand -i src.ivf -o out.ivf --duration 3600 --max-size 2MB

# IVF output can also be remuxed with plain ffmpeg.
ffmpeg -i out.ivf -c copy out.mkv
```

(`assemble` remains as an alias of `expand`.)

Or as a pure pipeline — stillcast does encode + expand, ffmpeg does all
container/audio work:

```bash
stillcast encode -i jacket.png -o - --fps 30 | \
    stillcast expand -i - -o - --duration 3600 --gop 300 | \
    ffmpeg -f ivf -i - -i podcast.m4a -c copy episode.mp4
```

Multi-image playlists (e.g. per-song jacket switches in an album video) —
each switch lands on a real keyframe, so every segment start is a seek
point:

```bash
stillcast make --playlist tracks.txt -a album.m4a -o album.mp4
# tracks.txt — `path [duration]` per line, `#` comments; the last entry may
# omit its duration and fills the rest of the audio/--duration:
#   cover1.png  180           # seconds
#   cover2.png  5400f         # exact frame count (fps-independent)
#   cover3.png  03:05.500     # ffmpeg-style MM:SS.mmm / HH:MM:SS.mmm / Ns
#   cover4.png                # remainder
stillcast expand --playlist encoded.txt -o out.ivf --duration 3600
# encoded.txt lists .ivf/.obu/.av1b sources instead of images
```

Each emitted GOP is: keyframe TU → golden TU → `show_existing_frame` TU ×
(gop−2). The golden is re-shown for every remaining frame; at each GOP
boundary a new coded video sequence restarts the pattern, giving uniform
seek points.

### Inspecting / verifying the stream structure

`stillcast info` on an assembled stream dumps or validates exactly the
structure above — frame type per TU, which reference slot each
show_existing rediscovers, refresh flags, and the invariants the design
relies on:

```bash
stillcast info -i out.ivf --verbose   # per-TU dump (works on .mp4 too)
stillcast info -i out.mp4 --check     # assert invariants, nonzero exit on fail
```

`--check` verifies: first TU is a shown KEY_FRAME, no `show_existing`
ever re-displays a keyframe (spec-forbidden), every TU shows a frame, and
every non-key coded frame is `INTER`+showable. `ffprobe -show_frames`
(2 I-frames, rest P) and the AV1 reference decoder `aomdec` provide
independent cross-checks of the same structure.

## Build & test

```bash
cargo build --release
cargo test            # unit tests
./scripts/e2e.sh      # end-to-end: encode → assemble → dav1d decode-verify,
                      # mp4/ffprobe checks, seek landing, determinism

# real-browser seek test (needs Chrome with --remote-debugging-port):
CDP_URL=http://localhost:29229 python3 scripts/browser_seek_test.py examples/demo.mp4
```

## Roadmap

- [x] OBU / sequence header / uncompressed header parsing
- [x] show_existing_frame TU synthesis + GOP assembler + IVF I/O
- [x] MP4/ISOBMFF output (`av01` + `av1C`, `stss` sync table) + AAC mux
- [x] `stillcast make`: image + audio → video in one command (drives ffmpeg
      for the encode/audio conversion)
- [x] Audio input: any ffmpeg-readable format → AAC (both `make` and
      `expand --audio`)
- [x] Pipe mode: `encode`/`expand`/`plan`/`info` accept `-i -` / `-o -`
      (stdin/stdout); `expand` = renamed `assemble` (alias kept), public
      `encode` subcommand split out of `make`
- [x] Library + C ABI: `expand_ivf`/`expand_ivf_multi` (bytes in → IVF
      bytes out) and `stillcast_expand`/`stillcast_free` →
      `libstillcast.{so,a}` + `include/stillcast.h`
- [x] Input containers: IVF + low-overhead OBU + Annex-B sniffed natively
      (ffmpeg-free core; see `docs/input-formats.md`)
- [ ] WebM/MKV output
- [x] Decoder-model: `--decoder-model` emits `decoder_model_info` +
      `buffer_removal_time_present_flag` (opt-in; `equal_picture_interval`
      keeps `temporal_point_info` unnecessary)
- [x] Limit-tracer: `stillcast plan` size/seek table, `--target-seek N`
      (gop = N×fps), `--max-size` (assemble: gop growth, make: crf ladder)
- [ ] Compatibility matrix: hw decoders, browsers, mobile players
      → [`docs/compat.md`](docs/compat.md)
- [x] Multi-image playlists: `--playlist` on `make`/`assemble`, timed
      switches, every switch is a keyframe (a real seek point)
- [ ] Long-term: same transformation as an **ffmpeg bitstream filter**
      (`av1_stillcast` bsf) — an *additional* path, not a replacement for
      the CLI flow above

## License

MIT
