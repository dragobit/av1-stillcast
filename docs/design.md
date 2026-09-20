# Design

## Principle

Don't write an encoder — write a **bitstream assembler**. A standard encoder
(libaom) produces the small set of real coded frames; this tool handles GOP
policy, reference-slot bookkeeping, and cheap TU synthesis. That gives full
control over the size/seek/compatibility frontier for ~5% of the effort of a
codec.

## Layers

The tool is split into three layers, by who owns each decision:

| layer | commands | owns |
|---|---|---|
| **core** | `expand`, `plan`, `info` | bitstream-structure decisions only: GOP/TU synthesis, golden-slot bookkeeping, the size↔seek cost model (`gop`, `--target-seek`, `--max-size`, `--duration`, `--fps`, `--playlist` segments, `--decoder-model`) |
| **orchestration** | `encode`, `make` | encode *policy* — probe encodes, the CRF ladder, uniform encoder settings — while codec *execution* stays delegated to libaom via ffmpeg |
| **external** | — | container muxing, audio processing, metadata, thumbnails: ffmpeg/MP4Box's job |

The core layer is a pure bitstream→bitstream transform: coded AV1 frames
(IVF/OBU) in, expanded AV1 elementary stream out. Everything else is built on
top of it or delegated away from it.

`encode` (image → 2-frame IVF) lives in the orchestration layer rather than
outside the tool for two reasons:

- **seq-header identity**: every `--playlist` segment must share a
  byte-identical sequence-header payload. That invariant is only enforceable
  when the tool drives every encode with identical settings; for arbitrary
  user-supplied IVF, `expand` can only detect and reject mismatches.
- **the cost model needs a probe**: `plan`/`--max-size` predict
  `kf_size` from a 2-frame probe encode — effectively a two-pass flow
  (measure → pick policy → real encode) that only works when the
  orchestration layer can invoke the encoder itself.

As an independent subcommand, `encode` is also a composable part for
pipe-oriented users, e.g.
`stillcast encode -i jacket.png | stillcast expand --gop 300 | ffmpeg -f ivf -i - -i audio.m4a -c copy out.mp4`.

The hand-rolled MP4 muxer is demoted accordingly: not the only output path,
but a dependency-free fallback and a verification reference. Distribution
stays one binary with subcommands; splitting into separate tools is
deferred until the ffmpeg bsf path (below) materializes.

## Stream layout

```
GOP (repeated):
  TU:  TD + SEQ_HEADER + FRAME(keyframe, shown)          <- random access point
  TU:  TD + FRAME(inter "golden", shown, refreshes slot g)
  TU × (gop-2):  TD + FRAME_HEADER(show_existing_frame, idx=g)
```

- **Golden** = a shown non-key frame identical to the displayed image.
  Being shown and non-key makes it `showable_frame = 1`; it must refresh at
  least one ref slot, and we re-show the lowest such slot.
- After each keyframe the whole DPB points at the keyframe, so the *same*
  golden TU bytes are valid in every GOP.
- `gop` is the single knob on the size ↔ seek frontier:
  bitrate ≈ keyframe_bits/gop + fps×6 B;  seek error < gop frames.

### Segments (`--playlist`)

A playlist is a sequence of **segments** — each with its own 2-frame encode
(own keyframe + own golden). Segments are emitted back to back; every
segment boundary starts with a shown keyframe, so:

- each switch is a real random-access point (`stss` entry, DPB reset) — no
  reference-slot juggling across images;
- within a segment the same GOP pattern repeats (golden re-shown for every
  remaining frame);
- a 1-frame segment emits just its keyframe TU.

Constraint: all segments must share a byte-identical sequence-header
payload — same dimensions/colour/encoder settings. `make` enforces this by
running every image through the same encode settings; `assemble` rejects
mismatched inputs. Decoder-model flag splicing walks a single shared DPB
model across segments in decode order.

## Crate layout

| module | role |
|---|---|
| `bitio` | MSB-first bit reader/writer, uvlc, leb128, trailing bits |
| `obu` | OBU header parse/serialize, OBU type enum |
| `seq_header` | full sequence-header walk → flags needed downstream |
| `frame_header` | partial uncompressed-header parse (stops after refresh_frame_flags) |
| `ivf` | IVF container read/write (packets = temporal units) |
| `container` | input sniffing/demux: IVF + low-overhead OBU + Annex-B → `IvfFile` (see `docs/input-formats.md`) |
| `mp4` | ISOBMFF writer: ftyp+mdat+moov, av01/av1C + mp4a/esds, stss |
| `adts` | ADTS parser → raw AAC frames + AudioSpecificConfig |
| `api` | stable crate API: bytes-in→bytes-out `expand_ivf*` |
| `ffi` | C ABI shim over `api` (`stillcast_expand`/`stillcast_free`) |
| `assemble` | input validation, golden-slot selection, GOP expansion |
| `main` | `stillcast` CLI (`make`/`expand`/`encode`/`plan`/`info`) |

## Input contract

Input is a short encode of the same static picture from a conformant
encoder. `split_input` scans a bounded window (`INPUT_SCAN_TUS` = 8 leading
TUs, positions never hard-coded) for the two TUs it needs:

- **anchor** — a TU containing the sequence header and a shown keyframe;
- **golden** — the first TU after the anchor that is shown, non-key,
  showable, refreshes ≥1 reference slot, and is decode-adjacent: no
  slot-refreshing coded frame may sit between it and the anchor (intra
  frames re-base the DPB and become the new predecessor), and when the
  stream carries order hints it must have `order_hint == predecessor + 1`
  — i.e. it was coded directly after the keyframe, so splicing it in
  cannot change its decode.

Frameless TUs (TD/metadata/padding/seq-only), show_existing TUs, and
extra keyframes in the window are skipped; a later seq+keyframe TU
re-anchors the search; a *changed* sequence header invalidates the
anchor. Failures report per-TU why each candidate missed. Accepted
containers: IVF, low-overhead OBU stream, Annex-B — sniffed by content,
no ffmpeg involvement (see
`docs/input-formats.md` for why mp4/mkv are deliberately excluded).
Rejected up front: reduced still-picture headers, frame id numbers,
unequal-interval decoder-model timing, film grain.

How the contract holds under encoders/environments that can't be
configured (WebCodecs, HW encoders, runtimes that drop frames), and the
measured encoder matrix: [`docs/input-contract.md`](input-contract.md).

## MP4 output

Layout is `ftyp | moov | mdat` (faststart: moov first, via a two-pass
build — the stco count doesn't depend on moov size, so offsets patch
cleanly). The video track carries an `av01` sample entry whose `av1C` box
is derived from the parsed sequence header; keyframes land in `stss`, so
seek granularity = gop. The audio track takes an ADTS file, strips the
7-byte headers into `mp4a` samples, and writes `esds` with the
AudioSpecificConfig. Timescales: video = fps, audio = sample rate; all
creation/modification times are zeroed to keep output byte-deterministic.

## What is deliberately not done

- **Codec execution** — libaom/ffmpeg remains the frame factory; the tool
  owns encode policy (probe, CRF ladder, uniform settings), never the
  compression itself.
- **Muxing, audio, metadata** — delegated to ffmpeg et al. The built-in MP4
  writer is a fallback/verification path, not the product boundary.
- **Refreshed/motion content** — the target is exactly-static visuals.
  Timed image switches exist via `--playlist` (per-segment keyframes).

## Path to ffmpeg

Delivery to ffmpeg-centric users is staged, each stage standing on its own:

1. **Unix filter** (done) — `expand`/`encode` accept stdin/emit stdout, so
   the transform composes with stock ffmpeg today
   (`ffmpeg … -f ivf - | stillcast expand | ffmpeg -i - …`).
2. **C ABI** (done) — `src/api.rs` is the stable bytes-in→bytes-out crate
   API (`expand_ivf`/`expand_ivf_multi`); `src/ffi.rs` is a thin shim over
   it (`stillcast_expand`, `stillcast_free`, `stillcast_last_error`,
   `include/stillcast.h`). `cargo build --release` emits
   `libstillcast.{so,a}` (crate-type `cdylib`).
3. **ffmpeg bitstream filter** — the transform maps cleanly onto a bsf
   (same shape as `av1_metadata`):

```bash
# one pipeline: libaom encodes the 2 real frames, bsf expands the stream
ffmpeg -loop 1 -i jacket.png -i audio.m4a \
    -c:v libaom-av1 -crf 32 -b:v 0 -r 30 -frames:v 2 \
    -bsf:v av1_stillcast=gop=300:duration=3600 \
    -c:a copy out.mp4
```

Division of labor inside ffmpeg: the `libaom-av1` encoder produces the real
coded frames (keyframe + golden inter frame); the bsf does the GOP/TU
expansion — no compression work. Parameters (`gop`, `duration`/`frames`,
timescale) would be declared in a standard `AVOption`/`AVClass` table, so
`key=val:key=val` syntax and `-h bsf=av1_stillcast` help come for free —
the modern ffmpeg option convention. Since a bsf's I/O shape is identical
to a pipe stage's, the layer split above is what makes this path cheap.
Delivery splits by target: a fork/static build calls the C ABI (Rust logic
reused verbatim); upstreaming requires a C port since ffmpeg takes no Rust
dependency — decide when the distribution target is known.

Beyond a single tool, the same technique generalizes: VP9 has a
`show_existing_frame` equivalent, opening a variant for older hardware
without AV1 decode; and the ~30 ms fully-deterministic expansion suits
embedding as a library in server-side pipelines (audio+cover → video at
request time).
