# Research: Mediabunny VP9 encoder support

Reference: the AV1 encoder investigation in `examples/webcodecs/README.md`
and the browser-`make` design in `docs/browser-make.md`. Same method:
docs/codec-registry review + measured `VideoEncoder` probing in this
VM's Chrome (CDP, `scripts/vp9_probe.py`).

## TL;DR

VP9 is a first-class Mediabunny video codec on both the encode path
(WebCodecs-backed sources) and the passthrough path
(`EncodedVideoPacketSource('vp9')`). On this VM, Chrome encodes
`vp09.00.*` (profile 0) and `vp09.02.*` (10-bit) in software; profile 1
and `prefer-hardware` are unsupported. Unlike AV1, VP9 chunks carry no
sequence header — but the VP9 uncompressed header natively includes
`show_existing_frame` + `frame_to_show_map_idx`, so the stillcast
expansion trick maps onto VP9 conceptually.

## Mediabunny side

- `'vp9'` is in `VIDEO_CODECS`; muxable into mp4 / mov / mkv / webm
  (codec-container matrix, mediabunny.dev/guide/supported-formats-and-codecs).
- Codec registry (`/codec-registry/vp9`):
  - `EncodedPacket.data` = one raw VP9 frame (Bitstream spec §6).
  - `'key'` packets must have `frame_type == KEY_FRAME` (§7.2).
  - Codec string `vp09.*` per the VP Codec ISO binding; `description`
    is not required — Mediabunny builds `vpcC` itself (same shape as AV1).
- Encode paths:
  - High-level sources (`CanvasSource`, `VideoSampleSource`, …) accept
    `VideoEncodingConfig{codec:'vp9', quality, latencyMode,
    keyFrameInterval, fullCodecString, hardwareAcceleration,
    scalabilityMode, …}` — they drive WebCodecs internally.
  - `canEncodeVideo('vp9', config)` checks encodability up front.
  - `EncodedVideoPacketSource('vp9')` for bring-your-own-encoder — the
    path the stillcast design already uses for AV1.
  - Custom-coder API (`CustomVideoEncoder` + `registerEncoder`) can
    supply a wasm libvpx fallback where WebCodecs vp9 encode is absent —
    the analog of the "wasm libaom" fallback in `browser-make.md`.
- WebM is the natural VP9 target (browser-native playback everywhere);
  mp4+vp09 also writes but playback support is only partial
  (MDN codec-selection notes).

## Measured on this box (Chrome, Linux x86_64, libvpx software path)

`VideoEncoder.isConfigSupported` (640×360@30, bitrateMode constant):

| config | supported |
|---|---|
| `vp09.00.10.08`, `vp09.00.31.08`, `vp09.00.40.08`, `vp09.00.50.08` — quality/realtime × no-preference/prefer-software | yes |
| any `vp09.*` + `prefer-hardware` | **no** |
| `vp09.01.40.08` (profile 1, 4:4:4) | **no** (all modes) |
| `vp09.02.40.10` (profile 2, 10-bit) | **yes** (software; unlike `av01.*.10` which was unsupported) |
| `vp09.00.40.08` + `bitrateMode:'quantizer'`, `vp9:{quantizer:32}` | yes |

Real encode (30 identical stills, 640×360@30, quality/prefer-software,
constant 300 kbps):

- 30 chunks, 0 drops, `key` only on chunk 0 (1260 B); all deltas are
  shown INTER frames (`show_frame=1`, `frame_type=1` in byte 0).
- Periodic larger deltas (~0.5–1.5 KB at chunks 4, 5, 14, 23, 24) —
  libvpx refreshing references internally; not hidden/alt-ref-only
  frames (every delta has `show_frame=1`).
- `decoderConfig.description` **absent** (same as AV1; VP9 needs no
  out-of-band config). `decoderConfig.codec` = `vp09.00.40.08`.

## Per-browser VP9 encode support (WebCodecs)

Measured on this VM with `scripts/vp9_probe.py` (`BROWSER=chrome|firefox|chromium`),
640×360@30. Safari is not runnable here — public data only.

| config | Chrome 137 (CDP) | Chromium headless 153 | Firefox headless |
|---|---|---|---|
| `vp09.00.*` quality/realtime, SW | yes | yes | yes |
| `prefer-hardware` | no (no HW VP9 encoder) | no | no |
| `vp09.01.40.08` (profile 1, 4:4:4) | **no** | **no** | **yes** |
| `vp09.02.40.10` (10-bit) | yes | yes | yes |
| `bitrateMode:'quantizer'` | yes | yes | yes |

Encode of 30 identical stills — all three produced 30 chunks, 0 drops,
key on chunk 0 only, all deltas shown INTER (`show_frame=1`):

- **Firefox deltas are dramatically smaller**: 26–31 B every frame
  (key 2612 B). Chrome/Chromium deltas spike to 0.5–1.6 KB periodically
  (libvpx refreshes). For the stillcast use case (identical frames,
  then `show_existing_frame` expansion), Firefox's encoder output is
  notably leaner.
- `decoderConfig.description` absent in all three; codec string echoes
  the configured `vp09.00.40.08`.

External data (webcodecsfundamentals.org, ~67M isConfigSupported tests):
- Chrome/Chromium: ~26% of VP9 encode variants supported — profile 0
  only, matching our measurement.
- **Firefox: ~99%** — broadest VP9 encode support of any browser
  (profiles 0/1/2).
- Safari (26+): ~19.6% on macOS/iOS — partial; treat VP9 encode as
  unreliable there and gate on `isConfigSupported`.

Implication for the browser-`make` design: Firefox is not only a
working fallback for VP9 encode, it's the *better* encoder for
identical-still workloads (smaller delta TUs). Safari needs the
upload-fallback or `CustomVideoEncoder` (wasm libvpx) path.

## Can stillcast-style "proper frame" generation work for VP9?

The AV1 expand trick needs: (a) an anchor coded frame, (b) a shown
golden coded frame that populates a DPB slot, (c) a cheap bitstream
element that re-displays that slot. VP9 has all three natively —
`show_existing_frame` (1 bit) + `frame_to_show_map_idx` (3 bits) sit in
the uncompressed header, right after `frame_marker`/`profile`:

```
byte0 = 0b10 p0 p1 se ft sf er   (profile 0)
se=1 → next 3 bits = frame_to_show_map_idx; nothing else required.
```

Verified end-to-end (`scripts/vp9_probe.py` + `scripts/vp9_show_existing_poc.py` +
ffmpeg/libvpx + headless browsers, this VM):

- Crafted a 1-byte show_existing packet `0x88|slot_idx` after
  `[KEY_FRAME, INTER]` from a libvpx encode. libvpx decodes it and
  outputs the referenced frame — framemd5 of SE packets is
  bit-identical to the golden INTER frame. Slot indices 0/1/7 all work
  (the keyframe refreshes all 8 DPB slots).
- Appending garbage after the 3-bit index (`header_size_in_bytes`
  field does not exist in the SE branch — libvpx returns early)
  produces `Invalid frame marker` errors, so the packet is literally
  one byte of header with the rest of the "frame" empty.
- ffmpeg remuxes the stream to WebM and MP4 cleanly
  (`vp09` codec tag, no errors).
- Chromium and Firefox both play and seek both containers
  (readyState 4, no MediaError).
- Mediabunny `EncodedVideoPacketSource('vp9')` accepts the 1-byte
  packets → WebM that libvpx decodes back to 5 frames.

So the VP9 "expand" primitive is: emit `[anchor KF, golden INTER]`,
then one-byte SE packets; insert a real KF per GOP for seeking —
same shape as the AV1 assembler.

What's needed to implement (vs AV1):

| AV1 machinery | VP9 equivalent |
|---|---|
| OBU scan for SH+KEY_FRAME + shown INTER | VP9 uncompressed-header parse: `frame_type`, `show_frame`, `refresh_frame_flags` (8b, byte 1–2 region of inter frames) to learn which slot the golden lands in |
| `accept_tus` / TU = one OBU group | input unit = one raw VP9 frame (WebCodecs chunk is already 1:1) |
| seq-header byte-identical check for playlist | no seq header — constrain via (width,height,profile,colorspace) parsed from each segment's KF |
| `show_existing_frame` OBU (~6 B) | 1-byte SE packet (~1 B) — even cheaper per repeated frame |
| IVF `AV01` / mp4 `av01` | IVF `VP90` / `vp09` in webm or mp4 |
| dav1d decode-verify | libvpx (ffmpeg `-c:v libvpx-vp9`) — verified above |

Open conformance questions before coding:
- Spec §8: SE must reference a slot containing a previously *decoded*
  frame; after a KF all 8 slots hold the keyframe so any index is
  legal. Re-showing the golden INTER needs its `refresh_frame_flags`
  parsed to pick the right index.
- Whether a stream can *start* mid-GOP (SE before any KF in that
  segment) — stillcast's per-GOP KF design sidesteps this anyway.
- Encoders under load may emit `show_frame=0` (hidden) frames
  (alt-ref); the pick-golden scan must skip them, same as AV1.
- VP9 superframes: registry requires one frame per packet; must split
  if an encoder ever bundles them (not observed in WebCodecs or
  ffmpeg outputs tested).

## Delta vs the AV1 findings

| AV1 (previous work) | VP9 |
|---|---|
| chunk = one OBU temporal unit (TD+SH+FRAME) | chunk = one raw VP9 frame (no TU/OBU layer) |
| anchor needs in-band SEQUENCE_HEADER | no seq header; keyframe is self-contained |
| golden = shown INTER refreshing one slot | same idea via uncompressed-header `refresh_frame_flags` + `ref_frame_idx` (slot indices implicit in bitstream order) |
| expansion via `show_existing_frame` OBU | VP9 header has `show_existing_frame` + `frame_to_show_map_idx` — the same primitive exists natively |
| IVF fourcc `AV01` | `VP90` |
| OBU scan in JS for anchor/golden pick | needs a VP9 uncompressed-header parser instead (byte-0 flags + refresh/refresh-order fields) |

Caveats to verify if pursued:
- Whether libvpx-in-WebCodecs can ever emit `show_existing_frame`
  itself (unlikely — it's a bitstream-assembler job, same as AV1).
- Superframes: registry wants "a frame" per packet; if a chunk ever
  bundles a superframe it must be split before `add()` (none observed).
- Chrome emits no hidden/alt-ref frames for stills here, but libvpx
  alt-ref behavior under `latencyMode:'quality'` on non-identical input
  deserves the same scan-don't-assume treatment the AV1 path got.
