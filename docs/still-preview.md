# Still-frame quality preview — decision record

Surfaced in discussion 2026-09-29. Records the intent and the chosen
approach; the concrete CLI/API shape is intentionally left open.

## Goal

stillcast is a two-stage pipeline: (1) encode the real coded frames
(keyframe + golden inter frame per segment — the "frame factory"), then
(2) assemble/expand them into a long stream via `show_existing_frame`.
The pixel quality of the final video is already fully determined at the
end of stage 1: every frame the decoder ever shows is a decode of one of
those coded frames. Stage 2 only decides *when* and *how often* each is
shown — it adds zero new picture data.

So for "I want to check what quality this image will be displayed at",
decoding a stage-1 output frame is sufficient and exact — no assembly,
no container, no duration math required.

## Decision: decode stage-1 frames to PNG

Export each stage-1 coded frame decoded to PNG. PNG is lossless, so the
files are pixel-faithful captures of exactly what any compliant decoder
will put on screen — for AV1 today, for VP9 or any future codec unchanged.

Rationale:

- **Codec-agnostic, single code path.** One decode→PNG path covers AV1,
  VP9, and anything else. Alternatives that wrap the coded frame itself
  (see below) need per-codec container logic.
- **Inter frames are covered.** The golden is an inter frame; it cannot
  stand alone in a still-image container, but decoding it yields its
  exact displayed quality — the frame the stream re-shows for most of
  the video's runtime.
- **Negligible cost.** Still-image encoding with libaom/libvpx is
  seconds to tens of seconds per frame; dav1d/libvpx decode plus PNG
  encode is milliseconds. The preview adds well under ~1% to stage-1
  time — it does not move the wait the user cares about (encode).
- **Universal viewer support.** The encode host and the review host are
  often different machines/contexts (batch/CI generation, then artifact
  review). PNG opens everywhere; there is no "the environment can't read
  this" case.

## Alternatives considered

- **Wrap the AV1 keyframe as AVIF.** Literally valid — AVIF *is* an AV1
  keyframe in HEIF — and opens in browsers/image viewers. Deferred:
  covers only the keyframe (the golden inter frame can't go in a still
  AVIF), AVIF sequence (`avis`) support in viewers is spotty, and it
  needs an HEIF muxer. Could be added later as an AV1-only extra.
- **AVIF sequence as the stage-1 transport (replacing IVF).** Rejected:
  technically possible (`avis` tracks hold raw AV1 samples), but swaps
  the ~zero-cost IVF frame dump for full ISOBMFF read/write, and turns
  an intermediate artifact into a distribution format. IVF stays the
  stage boundary; PNG/AVIF are preview byproducts, not transport.
- **VP8-based check (WebP).** Out of scope: the trick has no
  `show_existing_frame` in VP8, so VP8 is not a stillcast target codec.
  WebP being a repurposed VP8 keyframe is a format-historical note only.

## Why this lives at the stage-1 boundary

The motivating UX: "keep displaying this image at *this* quality" is a
stage-1 promise. Catching an over-quantized encode at stage 1 avoids
paying for assembly only to find the still was wrong. Quality checks
belong to whoever runs the encode — orchestration (`encode`) or the
user's own encoder — so the preview is a side output of stage 1, not a
mode of the assembler.

Open when implemented: the exact surface — e.g. a `preview` flag on
encode, or a separate `stillcast preview` reading the stage-1 IVF —
and whether keyframes additionally get an `.avif` wrap.
