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

Planned behavior:
- `stillcast plan -i src.ivf` — sweep gop ∈ {150,300,600,1200,3600,∞} ×
  crf ∈ {26,32,40}, print a Pareto table: video bytes, effective kbps,
  worst-case seek latency. Cheap to compute (expansion is ~ms).
- `stillcast make --target-seek 5s` — solve for gop directly
  (`gop = target × fps`), choose crf by a size budget if given
  (`--max-size`).
- `--explain` — print the chosen (gop, crf) and the projected numbers.

No extra dependencies; the model needs only `kf_size` (measured) and
constants already known (~6 B/TU + container overhead ~4 B/sample).

## 3. ffmpeg bitstream filter (long-term, additive path)

See `docs/design.md` §“Path to ffmpeg”. Key point for positioning: the
CLI/`make` flow (image + audio → video) remains the product; the bsf makes
the *stream transformation* reachable inside ffmpeg pipelines
(`-bsf:v av1_stillcast=gop=300:duration=3600`) for users already living in
ffmpeg. Parameters via `AVOption`/`AVClass`.

Likely delivery: an FFmpeg fork tree or a standalone
`libavcodec/bsf/av1_stillcast.c` patch + build doc, reusing this crate's
assembler logic (either port to C or expose a C ABI from Rust).
