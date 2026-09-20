# Roadmap detail

Concrete plans for the open roadmap items. `design.md` covers architecture
(including the core / orchestration / external layer split); this covers
what to build next and how to measure it.

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
- Remaining: a CRF column in the plan sweep, `--explain` output.

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
- ✅ Phase B (`--decoder-model`): a full uncompressed-header walker
  (`uheader.rs`) computes each real frame's header bit length, then
  `splice_header_bits` inserts `buffer_removal_time_present_flag=0` after
  `primary_ref_frame` and re-emits `byte_alignment` (zeros only — not
  `trailing_bits`). The sequence header is rewritten with
  `decoder_model_info` (90000-tick delays, removal/presentation field
  widths) and copied into both KF and golden TUs. Validation: bit-exact
  rescan of the spliced header, libdav1d full decode pixel-identical to
  the non-DM stream, and ffmpeg's strict cbs parser accepts every OBU
  (e2e asserts no `zero_bit`/`Failed to read` warnings).
  `equal_picture_interval=1` still means `temporal_point_info` is never
  required in frame headers.

## 4. Layered CLI + ffmpeg delivery

See `docs/design.md` §“Layers” and §“Path to ffmpeg”. Direction agreed:
keep one binary, but organize commands by responsibility — the **core**
commands (`expand`, `plan`, `info`) stay a pure coded-frames-in →
elementary-stream-out transform, while **orchestration** (`encode`,
`make`) owns encode policy (probe, CRF ladder, uniform seq headers across
playlist segments) and delegates codec execution to libaom via ffmpeg.
Muxing, audio, and metadata belong to external tools; the built-in MP4
writer becomes a dependency-free fallback and verification reference.

Staged delivery to ffmpeg-centric users:

- [x] **Pipe mode** — `expand`/`encode` read stdin and write stdout
  (IVF/OBU), so the transform slots into stock ffmpeg pipelines today:
  `stillcast encode -i img.png | stillcast expand --gop N | ffmpeg -f ivf -i - -i audio -c copy out.mp4`.
  Includes renaming `assemble` → `expand` (alias kept) and splitting the
  image→IVF step out of `make` into a public `encode` subcommand.
- [x] **C ABI** — `api` module holds the stable bytes-in→bytes-out
  contract (`expand_ivf` / `expand_ivf_multi`); `ffi` is a thin shim
  (`stillcast_expand` / `stillcast_free` / `stillcast_last_error`, see
  `include/stillcast.h`). Build emits `libstillcast.{so,a}` via the
  `cdylib` crate-type. Prerequisite for in-process embedding (bsf,
  GStreamer element, server-side use).
- [ ] **ffmpeg bitstream filter** — `av1_stillcast` bsf
  (`-bsf:v av1_stillcast=gop=300:duration=3600`), parameters via
  `AVOption`/`AVClass`. Two delivery shapes: an FFmpeg fork/static build
  that calls the C ABI above (`libavcodec/bsf/av1_stillcast.c` as a thin
  wrapper), or a C port of the assembler if upstreaming is the goal
  (upstream ffmpeg takes no Rust dep — decide by distribution target).

Beyond a single tool/distribution: a VP9 variant (VP9 also has
`show_existing_frame`, reaching older AV1-less hardware) and library
embedding for server-side on-demand generation are open possibilities the
layer split keeps cheap.

## 5. Input-contract hardening

The 2-frame requirement is spec-derived (keyframes can never be re-shown
via `show_existing_frame`), but positional acceptance (`packet 0`/`packet
1`) is an implementation choice — and the fragile one for encoders that
can't be configured (WebCodecs, HW/platform encoders, load-adaptive
runtimes that silently drop frames). Direction: scan a bounded TU window
for an anchor (seq header + shown KF) and a golden (shown, non-key,
showable, refreshes ≥1 slot, `order_hint == anchor + 1`); encode ~1 s of
input frames for drop tolerance; probe-verify per environment. Full
analysis and the single-frame/AVIF feasibility ladder:
[`docs/input-contract.md`](input-contract.md).
