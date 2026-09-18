# AV1 spec analysis for the show_existing_frame approach

Target stream: an essentially arbitrary-length video made of a few real coded
frames plus temporal units that only say "display the stored frame again".
This documents why that stream is legal and which spec constraints had to be
satisfied. Section numbers refer to the AV1 spec (v1.0.0-errata1 / current
editor's draft; same numbering).

## Terminology: what AV1 separates that other codecs conflate

Codecs like H.264 fuse three ideas into one ("keyframe" = intra-coded =
decoder reset = what you seek to). AV1 keeps them independent, and this
project exists because of that separation:

| concept | AV1 mechanism | stillcast usage |
|---|---|---|
| **intra coding** (coded without references) | `frame_type = KEY_FRAME` **or** `INTRA_ONLY_FRAME` — both are intra-coded | only at GOP boundaries |
| **decoder reset / random access** | shown `KEY_FRAME` resets decoder state + refills all 8 ref slots; `INTRA_ONLY_FRAME` is intra but does *not* reset (and can't refresh all slots) | periodic `KEY_FRAME`s are the seek anchors |
| **presentation** | `show_existing_frame` re-outputs a stored ref-buffer frame with zero coded data | ~99% of all TUs |

Corollaries that matter here:

- **`KEY_FRAME` is always intra** — "keyframe but not intra" is not a thing
  in AV1. The independence is intra ↔ reset (KEY vs INTRA_ONLY) and
  coding ↔ presentation (show_existing), not intra ↔ keyframe.
- **The displayed picture need not be intra at all**: our golden — the frame
  on screen for nearly the whole video — is a *shown inter frame*. It is
  never itself coded again; only re-presented.
- libaom already uses the same machinery internally (alt-ref/golden frame
  hierarchy, show_existing for spatial layers); stillcast just drives it
  explicitly at stream level.

## show_existing_frame semantics (5.9.2)

A frame header may set `show_existing_frame = 1` followed by
`frame_to_show_map_idx f(3)` selecting one of the 8 reference buffer slots.
The decoder then outputs the stored frame **without decoding a new picture**
— the header returns early; no tile data follows. Optional fields
(`display_frame_id`, `temporal_point_info`) appear only when the sequence
header enables frame ids or a decoder model with unequal picture intervals —
we emit sequence headers / accept inputs where both are absent, so the whole
TU costs: TD OBU (2 B) + FRAME_HEADER OBU (1 B header + 1 B size + 1 B
payload) ≈ **6 bytes**.

## Constraints found and how the design satisfies them

1. **Must be an OBU_FRAME_HEADER, not OBU_FRAME.**
   "If obu_type is equal to OBU_FRAME, show_existing_frame must be 0."
   → We emit OBU type 3 (FRAME_HEADER) with no tile group. ✔

2. **The referenced frame must have been decoded with `showable_frame = 1`.**
   Derived value: `showable_frame = (frame_type != KEY_FRAME)` for shown
   frames, or an explicit flag for invisible (`show_frame = 0`) frames.
   → A shown **keyframe can never be re-shown** — so the golden must be a
   non-key frame. Our golden is a shown INTER_FRAME (auto-showable); an
   invisible `show_frame=0, showable_frame=1` frame works too. ✔

3. **A KEY_FRAME may be output via show_existing_frame at most once**
   (and never, in practice, since shown keyframes are not showable).
   → We never point `frame_to_show_map_idx` at a keyframe slot. ✔

4. **One shown frame per temporal unit** (non-scalable streams).
   → Each TU carries exactly one OBU_FRAME_HEADER show_existing. ✔

5. **Reference-buffer persistence.**
   `refresh_frame_flags` of a show_existing frame is 0 → nothing overwrites
   the golden slot. `RefValid[i]` is only cleared by (a) a shown keyframe
   resetting decoder state, or (b) error-resilient order-hint mismatch —
   neither applies inside a GOP. The golden survives arbitrarily many
   re-shows. ✔

6. **Order hints.** A re-shown frame reuses its stored `RefOrderHint`.
   The spec explicitly notes OrderHint need not reflect true output order,
   so repeating it every re-show is conformant. ✔

7. **Random access.** A temporal unit containing a sequence header +
   shown KEY_FRAME starts a new coded video sequence — periodic keyframes
   are proper seek anchors. On each shown keyframe all 8 ref slots are
   refreshed to it, so re-emitting the same golden TU after each keyframe
   decodes identically every GOP (its refs resolve to the fresh keyframe). ✔

8. **`INTRA_ONLY_FRAME` cannot refresh all slots** (`refresh_frame_flags
   != 0xff`); we don't depend on it anyway. ✔

9. **`still_picture` / `reduced_still_picture_header`** is for single-frame
   sequences only — not usable, and not needed. ✔

## Empirical verification (2026-09, this repo's e2e path)

- Stream: KF + inter-golden + 120×show_existing, twice (2 GOPs) → 244 TUs,
  11 KB for 8.1 s @30 fps (~11 kbps total incl. 2 keyframes).
- **libdav1d: decodes all 244 frames, zero warnings**, output pixel-exact vs
  the encode (PSNR ≈60 dB = encoder noise only).
- Muxes to MP4 and MKV with `-c copy`; `ffmpeg -ss` seek lands correctly at
  GOP boundaries.
- ffmpeg's *native* AV1 decoder could not be exercised in our environment
  (fails on the plain libaom file too — build/hwaccel quirk, unrelated).

### Caveats / open risks

- show_existing_frame is heavily used in production only for **spatial
  scalability**; a stream that is >97% show_existing is unusual. Spec-legal,
  but hardware decoder / browser MSE behavior should be measured
  (compat matrix is on the roadmap).
- Seeks land on keyframes: worst-case latency = one GOP length of cheap
  show_existing TUs — fast, but a hard boundary. `--gop` trades seek
  granularity against size.
- Decoder model: `--decoder-model` emits `decoder_model_info` in the
  sequence header and splices `buffer_removal_time_present_flag=0` into
  the two real frame headers. `equal_picture_interval=1` keeps
  `temporal_point_info` unnecessary everywhere, so the flag is always 0
  and no removal times are written. Input streams that already declare a
  decoder model with unequal picture intervals are still rejected.
- Still not supported: `frame_id_numbers_present`, film grain (grain
  params are re-loaded per shown frame and would need emitting per TU).
