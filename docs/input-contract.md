# Input contract

`expand` consumes temporal-unit packets produced by a real encoder and
splices them into a long stream. This document is about the edge of the
pipe: what the encoder must produce, what actually breaks in the wild, and
how input acceptance should evolve so that environments with thin encoder
control (WebCodecs, hardware encoders, platform APIs) can feed it.

## The contract, restated as conditions

**Implemented:** `split_input` scans a bounded window (`INPUT_SCAN_TUS` =
the first 8 TUs) and tests per-TU *conditions*, not positions:

- **anchor TU** — contains the sequence header and a shown `KEY_FRAME`
  (decoder reset + random-access point; all 8 ref slots refresh to it).
  A later seq+keyframe TU *re-anchors* the search, so multi-keyframe or
  re-emitted-header encodes are tolerated.
- **golden TU** — the first TU after the anchor that is shown,
  `frame_type != KEY_FRAME`, `showable_frame` (auto-derived for shown
  non-key frames), `refresh_frame_flags != 0`, and decodable directly
  after the anchor — its references must resolve to slots that all hold
  the keyframe. Detected via `order_hint == anchor.order_hint + 1`
  (coded as the next display frame).

Skipped without failing: TD-only / seq-header-only / metadata / padding
TUs, invisible frames, show_existing TUs. When nothing qualifies, the
error lists every scanned TU and *why* it missed (e.g. `TU2: KEY_FRAME —
encoder forced keyframes; drop "-g 1"`, `TU4: order_hint=5 vs
anchor+1=1 — coded against other frames`).

Degradation when order hints are absent: some thin-control encoders
emit `order_hint_bits = 0` (e.g. Chrome's WebCodecs AV1 encoder — the
`order_hint` field is then absent from frame headers entirely). The
adjacency guard is unverifiable there and is skipped rather than forced:
the golden reduces to shown + non-key + showable + refreshes a slot.
Streams with real order hints keep the strict check.

Why a second frame exists at all: `show_existing_frame` may only
re-display a `showable_frame` frame, and keyframes are never showable
(spec-notes §2). The frame on screen for ~99% of the stream is the golden;
the anchor is shown once per GOP.

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

Positional acceptance was the fragile part. Status of the fix that
dissolves per-encoder dependence:

1. **Done** — `split_input` scans a bounded window (first 8 TUs) for the
   anchor TU, then the golden TU under the conditions above — including
   the `order_hint` adjacency guard (skipped when the stream carries no
   order hints), which guarantees the golden was coded against only the
   keyframe's decoder state, so splicing it after the anchor cannot change
   its decode.
2. **Done** — encode more than 2 input frames so drops and leading
   invisible frames are survivable; `encode` emits 4 and the scan ignores
   the tail. ~1 s worth is the recommended headroom.
3. **Probe per environment** (open): at acquisition time (not assembly
   time), run a probe encode of a still and check the contract — same
   pattern as `plan`'s probe-encode-for-cost-model. Encoder identity never
   enters the decision; only whether the produced TU stream satisfies the
   conditions.
4. Per-encoder "known-good settings" degrade to documentation — a
   `compat.md`-style matrix of encoder × settings → pass/fail, not code
   branches.
5. Fallback when nothing qualifies: decode to pixels and re-encode under
   controlled settings (pixel-level adoption).

## Relaxations worth keeping in pocket

- **Invisible golden**: spec-wise a `show_frame=0, showable_frame=1` frame
  is a valid re-show source; the code currently requires shown. GOP would
  become `KF + hidden golden + SE×(gop-1)` — one extra ~0-payload TU.
  Accepts alt-ref-style inputs.
- **`INTRA_ONLY_FRAME` golden**: already passes validation (non-key,
  shown, refreshes slots) and needs no references — permits mixing
  encoders for the two halves.

## Open items

- Probe-verify at acquisition time (item 3 above) and pixel-level
  re-encode fallback (item 5).
- The two source TUs may sit in different segments — each segment's
  source gets the same scan (segment boundaries reset the DPB anyway).
  Confirmed working.
