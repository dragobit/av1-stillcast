# Roadmap detail

Concrete plans for the open roadmap items. `design.md` covers architecture;
this covers what to build next and how to measure it.

## 1. Compatibility matrix (measure, don't speculate)

A >97% `show_existing_frame` stream is spec-legal but unusual — the risk is
decoder/player quirks, not conformance. Plan: publish a standard test asset
(`examples/demo.mp4` + a 1 h variant) and a results table in
`docs/compat.md`, filled incrementally.

Axes:
- **software decoders**: libdav1d ✅ (e2e), ffmpeg native `av1` decoder
  (unverified — fails in our env on plain files too), libgav1
- **players**: VLC, mpv, ffplay, Windows Media Foundation (AV1 extension)
- **browsers / MSE**: Chrome, Firefox, Safari (AV1 hw only on Apple silicon
  M3+/A17+), Edge — test seek-to-middle + full playthrough
- **hardware / mobile**: Android MediaCodec AV1, Apple AV1 hw, TVs
- **YouTube**: unlisted upload → check it ingests `av01` mp4 and what it
  re-encodes to (av01 vs vp9 transcode)

Tests per cell: full playthrough, seek to mid-GOP (worst case), seek to
keyframe boundary, A/V sync after seek. Automated cells (decoders) can run
in CI; hw/browser cells are manual — contributions welcome via PR to
`docs/compat.md`.

## 2. Limit-tracer mode (`--auto` / `stillcast plan`)

Expose the size↔seek frontier directly instead of making users pick `gop`
blindly. The cost model is closed-form:

```
video_bytes ≈ n_keyframes × kf_size + total_frames × ~6 B
seek_error  <  gop frames        compat risk ∝ stream oddity
```

where `kf_size` comes from one probe encode (libaom is deterministic at
fixed CRF, so a 2-frame probe predicts exactly).

Status:
- ✅ `stillcast plan -i src.ivf --duration 3600` — gop sweep table:
  stream bytes, kbps, worst-case seek latency.
- ✅ `--target-seek N` on `make`/`assemble` — picks `gop = N×fps`.
- ✅ `--max-size` — `assemble` grows gop geometrically until the file
  fits (seek granularity degrades, warns); `make` walks a crf ladder
  (requested → 40/48/56/63) re-encoding until it fits. Both measure
  the real output bytes, not the model.
- Remaining: a CRF column in the plan sweep, `--explain` output,
  multi-image playlists.

No extra dependencies; the model needs only `kf_size` (measured) and
constants already known (~6 B/TU + container overhead ~4 B/sample).

## 3. Decoder model (`decoder_model_info` / `temporal_point_info`), phased

Goal: emit strict conformance metadata so HRD-checking decoders and some
hw/broadcast pipelines accept our streams.

- ✅ Phase A: the sequence header parser now stores every field and can
  re-emit the header bit-exactly; `assemble`/`make` inject a constant-rate
  `timing_info` (fps + `equal_picture_interval`) when the source lacks one.
  `equal_picture_interval=1` also means `temporal_point_info` is never
  required in frame headers.
- Remaining Phase B: `decoder_model_info` + per-frame
  `buffer_removal_time_present_flag` requires rewriting the two libaom
  frame headers, which needs a full uncompressed-header parser/re-packer —
  the heavy step, kept for a later PR (round-trip byte-identity is the
  validation strategy).

## 4. ffmpeg bitstream filter (long-term, additive path)

See `docs/design.md` §“Path to ffmpeg”. Key point for positioning: the
CLI/`make` flow (image + audio → video) remains the product; the bsf makes
the *stream transformation* reachable inside ffmpeg pipelines
(`-bsf:v av1_stillcast=gop=300:duration=3600`) for users already living in
ffmpeg. Parameters via `AVOption`/`AVClass`.

Likely delivery: an FFmpeg fork tree or a standalone
`libavcodec/bsf/av1_stillcast.c` patch + build doc, reusing this crate's
assembler logic (either port to C or expose a C ABI from Rust).
