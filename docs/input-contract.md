# Input contract

`expand` consumes temporal-unit packets produced by a real encoder and
splices them into a long stream. This document is about the edge of the
pipe: what the encoder must produce, what actually breaks in the wild, and
how input acceptance should evolve so that environments with thin encoder
control (WebCodecs, hardware encoders, platform APIs) can feed it.

## The contract, restated as conditions

Today `split_input` requires ≥2 packets and treats them positionally:
packet 0 must carry a sequence-header OBU + shown KEY_FRAME, packet 1 is
the golden. The real requirements are per-TU *conditions*, not positions:

- **anchor TU** — contains the sequence header and a shown `KEY_FRAME`
  (decoder reset + random-access point; all 8 ref slots refresh to it).
- **golden TU** — shown, `frame_type != KEY_FRAME`, `showable_frame`
  (auto-derived for shown non-key frames), `refresh_frame_flags != 0`, and
  decodable directly after the anchor — its references must resolve to
  slots that all hold the keyframe. Detectable via
  `order_hint == anchor.order_hint + 1` (coded as the next display frame).

Why a second frame exists at all: `show_existing_frame` may only
re-display a `showable_frame` frame, and keyframes are never showable
(spec-notes §2). The frame on screen for ~99% of the stream is the golden;
the anchor is shown once per GOP. One coded frame can never satisfy the
contract — see "single-frame sources" below.

## What actually breaks (measured, libaom via ffmpeg, identical-still input)

| setting | packet 1 | verdict |
|---|---|---|
| `-cpu-used 8` (current recipe) | shown INTER, 31 B | OK |
| default (quality path, lag on) | shown INTER, 26 B | OK |
| `-cpu-used 4` | shown INTER | OK |
| `-usage realtime` | shown INTER | OK |
| `-lag-in-frames 25 -auto-alt-ref 1` | shown INTER | OK |
| `-g 1` (all-keyframe) | **KEY_FRAME** | rejected |
| moving content (contract violation — input must be a still) | shown INTER, but early `show_existing` TUs appear | TU shape diverges |

For identical stills, libaom emits a shown skip-inter at TU1 under every
tested mode — the shown inter is the encoder's cheapest legal choice for
duplicate content, which is why the contract holds broadly. `cpu-used 8`
is not load-bearing; what *is* load-bearing is "don't force keyframes" and
"the input is actually a still".

## Thin-control environments

The contract's enemy is not an encoder picking an exotic frame type — it
is runtimes that reshape the TU stream around the two packets:

- **WebCodecs** (`VideoEncoder`, codec `av01.*`): chunks are low-overhead
  OBU sequences and the sequence header rides in-band
  (`VideoDecoderConfig.description` is unused for AV1 per the W3C codec
  registration), so chunk 0 ≈ anchor TU already. The config surface is
  thin: bitrate(mode), framerate, `latencyMode`, `hardwareAcceleration`,
  per-frame `quantizer`, forced `keyFrame` — no lag/alt-ref/speed knobs.
  - `latencyMode:"quality"` (default): never drops frames, but may hold
    outputs — the lookahead-capable path where invisible frames can appear.
  - `latencyMode:"realtime"`: **may silently drop frames** (Chromium does;
    no drop notification — detectable only via chunk timestamp gaps). The
    golden packet can simply never exist.
- **Hardware/platform encoders** (Android MediaCodec, NVENC/QSV/VAAPI,
  WebCodecs `prefer-hardware`): typically 1-input→1-output shown frames,
  which suits the contract — but e.g. Chrome wraps HW output with its own
  OBU builders (`media/gpu/av1_builder.cc`), so TU shapes vary per
  implementation.
- **Runtime adaptation**: thermal/load-driven resolution or fps changes
  emit a new sequence header mid-stream; `scalabilityMode`/SVC puts
  multiple frames in a TU (hard violation). Don't request them; treat a
  mid-input seq-header change as a segment boundary.
- **Distracting extras**: repeated sequence headers, metadata/padding OBUs,
  leading TD OBUs — harmless; scan-based acceptance skips them naturally.

## Direction: scan, don't index

Positional acceptance is the fragile part. The fix that dissolves
per-encoder dependence:

1. Accept ≥2 packets, scan a bounded window (e.g. first ~8 TUs) for the
   anchor TU, then the golden TU under the conditions above — including
   the `order_hint` adjacency guard, which guarantees the golden was coded
   against only the keyframe's decoder state, so splicing it after the
   anchor cannot change its decode.
2. Encode more than 2 input frames (~1 s worth) so drops and leading
   invisible frames are survivable; `encode` already emits 4 and discards
   the tail — keep that headroom explicit.
3. **Probe per environment**: at acquisition time (not assembly time), run
   a probe encode of a still and check the contract — same pattern as
   `plan`'s probe-encode-for-cost-model. Encoder identity never enters the
   decision; only whether the produced TU stream satisfies the conditions.
4. Per-encoder "known-good settings" degrade to documentation — a
   `compat.md`-style matrix of encoder × settings → pass/fail, not code
   branches.
5. Fallback when nothing qualifies: decode to pixels and re-encode under
   controlled settings (pixel-level adoption). In-browser that means a
   second encoder (wasm) or the deferred synthesized-golden path below.

## Single-frame sources

A still-AVIF coded frame or a 1-frame IVF is exactly the *anchor* half of
the contract: a `KEY_FRAME` can never be re-shown, and AVIF additionally
mandates `still_picture=1` + `reduced_still_picture_header=1` — a
single-frame-only sequence form that disables inter tools and that we
reject by design. Adoption paths, in order of feasibility:

- **pixels**: decode the AVIF → normal encode path (needs an AVIF-capable
  demuxer/decoder in ffmpeg, or `avifdec` up front). Zero core changes;
  the source coding isn't preserved, but the displayed frame is the golden
  — which is re-encoded regardless.
- **coded bits**: adopt the KF after normalizing its sequence header to
  full form (the emit path exists) and splicing the omitted
  `frame_size_override_flag` bit back into its frame header (the splice
  machinery exists). The blocker is unchanged: a showable golden still has
  to come from somewhere — re-encoding decoded pixels (displays the
  re-encode, not the AVIF — pointless), borrowing a near-all-skip inter
  (fragile, unverifiable without decoding), or synthesizing an all-skip
  inter frame — a mini-encoder writing entropy-coded tile data, i.e. the
  first step across this project's "no codec execution" boundary.

## Relaxations worth keeping in pocket

- **Invisible golden**: spec-wise a `show_frame=0, showable_frame=1` frame
  is a valid re-show source; the code currently requires shown. GOP would
  become `KF + hidden golden + SE×(gop-1)` — one extra ~0-payload TU.
  Accepts alt-ref-style inputs.
- **`INTRA_ONLY_FRAME` golden**: already passes validation (non-key,
  shown, refreshes slots) and needs no references — permits mixing
  encoders for the two halves.

## Open items

- Scan-window bound, and "nearest miss" diagnostics (report *why* each
  scanned TU failed, not just that none qualified).
- Multi-keyframe tolerance: skip leading KFs/SE TUs before the golden.
- Whether the two source TUs may sit in different segments (they may —
  segment boundaries reset the DPB anyway).
