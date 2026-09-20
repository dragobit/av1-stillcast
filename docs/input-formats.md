# Input containers — decision record

Context: the 2-frame AV1-encoded input (`keyframe + golden inter`) was
historically accepted only as IVF. This records the discussion and the
decision on which other containers `expand`/`plan`/`info` should accept.

## Requirements surfaced in discussion

- **The core must stay ffmpeg-free.** `expand`/`plan`/`info` (and the
  `api`/`ffi` bytes-in→bytes-out surface) are pure Rust so the transform can
  be embedded in encoders with their own interfaces — e.g. browser/WebCodecs
  pipelines or encoders that emit only one specific container — without
  dragging an ffmpeg subprocess dependency into the core.
- **An ffmpeg bitstream filter (`av1_stillcast` bsf) may still be built,
  but it is just one adapter.** It sits on top of `libstillcast` via the C
  ABI, alongside the CLI and any other frontends — it is not "the" product.
- **The transform itself is already container-agnostic.** `split_input`
  scans the leading TUs for the anchor (seq header + shown keyframe) and
  the golden (the shown non-key frame coded right after it) — positions are
  never hard-coded — plus width/height and an fps hint.

## Candidates evaluated

| format | verdict | rationale |
|---|---|---|
| Low-overhead OBU (§5 stream) | **accepted** | The codec's native wire format; aomenc `--obu`, SVT-AV1, and WebCodecs `EncodedVideoChunk` data land here with zero repackaging. Parser is ~100 lines. |
| Annex-B (`.av1b`) | **accepted** | leb128 length-delimited TUs; trivially parsed, covers broadcast/HW pipelines. |
| MP4 input | **rejected for expand/plan** (kept for `info` via ffmpeg) | A native ISOBMFF reader is hundreds of lines plus fragmented-mp4/edit-list/non-av01 edge cases — the largest cost for the least additional value on a 2-frame input (timing/metadata/audio benefits matter on output, which already exists). |
| WebM/MKV | **rejected** | Needs an EBML parser; `ffmpeg -c copy -f ivf` is an adequate escape hatch. |

OBU was judged the better lingua franca than mp4 for the 2-frame input:
every encoder can emit an elementary stream, while mp4's strengths
(timing, metadata, audio cohabitation) are irrelevant to the input.

## Implementation

- New `src/container.rs`: content-sniffed demux. `DKIF` → IVF, `ftyp` →
  reject-with-remux-hint, EBML → reject, else try low-overhead OBU then
  Annex-B. Everything normalizes to the existing `IvfFile` shape
  (`Vec<(ts, TU)>`, width/height, timebase).
- OBU streams: TU = bytes between TemporalDelimiter OBUs; streams without
  TDs are grouped by coded frame; frameless leading groups merge into the
  following TU. OBUs are re-serialized with explicit size fields.
- Annex-B: `temporal_unit_size`/`frame_unit_size`/`obu_size` leb128 nesting
  unwrapped; OBU headers re-framed.
- Non-IVF inputs have no container timebase: the rate comes from the
  sequence header's `timing_info` when present — kept as an exact
  rational (`time_scale` / frame period, so 30000/1001 stays 30000/1001,
  not a truncated 29) — else 30/1. `expand` prints a note and `--fps`
  overrides. IVF inputs with a zero in either timebase field are treated
  as unset (30/1) the same way, and output never carries a zero rate.
- `stillcast info` additionally accepts mp4/other containers by demuxing
  through ffmpeg (pre-existing path), unchanged.
- `expand_ivf`/`expand_ivf_multi` now sniff the input format themselves, so
  FFI/embedders get OBU/Annex-B support for free.

## Rejected alternative

Routing `expand` through ffmpeg demux (as `info` does) would have covered
every container at once but breaks the pure-Rust/deterministic/no-subprocess
property that makes the core embeddable. ffmpeg stays in the orchestration
layer (`make`/`encode`) only.
