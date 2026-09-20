# WebCodecs → `stillcast expand` adapter (proof of concept)

A static page (no build step) that turns a browser AV1 encode into the IVF
input `stillcast expand` consumes: open `index.html` from any secure-context
origin (`http://localhost:*` or `https://*`; `VideoEncoder` is not exposed on
`file://` in some builds), pick a still image (or use the generated one),
and click **Encode + build IVF**.

What it does, per `docs/input-contract.md`:

1. Draws the still onto a canvas, encodes N identical `VideoFrame`s at the
   configured fps (`keyFrame` forced on frame 0 only).
2. Collects `EncodedVideoChunk`s — each chunk is one low-overhead OBU
   temporal unit; no repackaging is needed.
3. Runs a **minimal OBU scan** in JS (walk OBU headers; read
   `show_existing_frame`/`frame_type`/`show_frame` from each frame OBU's
   leading bits) to find:
   - the **anchor TU** — first chunk containing a `SEQUENCE_HEADER` OBU plus
     a shown `KEY_FRAME`;
   - the **golden TU** — the first TU after the anchor that contains a
     coded frame, required to be shown and non-key. It must be decodable
     directly after the anchor; skipping past a hidden coded frame would
     let it reference decoder state the anchor never produced (its
     `ref_frame_idx` could point at the hidden frame's slot).
   The scan exists because `expand`'s `split_input` is positional today
   (packet 0 = anchor, packet 1 = golden); the page reorders chunks as
   `[anchor, golden, ...rest in encode order]` instead of assuming packet 1
   qualifies. When scan-based input acceptance lands in `expand`, the
   reorder becomes unnecessary.
4. Detects dropped frames by comparing submitted timestamps
   (`i * 1e6/fps` µs) against chunk timestamps — `latencyMode:"realtime"`
   drops are silent, this is the only observable.
5. Writes a `DKIF` IVF (`AV01` fourcc, timebase = fps) and offers it for
   download. Packet timestamps carry the chunk's own timestamp in timebase
   units, so a reordered golden keeps its provenance.

Automation hook: `window.stillcastRun(opts)` performs a full encode and
returns `{ok, ivfB64, chunks, picks, missingFrames, decoderConfig}` — usable
from CDP/Playwright (`Runtime.evaluate` with `awaitPromise`).

## Measured on this box (Chrome 137.0.7118.2, Linux x86_64, libaom software path)

`VideoEncoder.isConfigSupported`:

| config | supported |
|---|---|
| `av01.0.04M.08`, `av01.0.08M.08` — quality/realtime × prefer-software/no-preference | yes |
| any `av01.*` + `prefer-hardware` | **no** (no AV1 HW encoder on this VM) |
| `av01.0.04M.10` (10-bit) | **no** |

Encode results (640×360@30, 30 frames, identical still → all rows; also
1280×720@60 × 120 frames where noted):

| latencyMode | hw | bitrateMode | chunks | dropped | anchor | golden | expand |
|---|---|---|---|---|---|---|---|
| quality | prefer-software | constant | 30/30 | none | ch 0 | ch 1 | OK |
| quality | prefer-software | variable | 30/30 | none | ch 0 | ch 1 | OK |
| quality | prefer-software | quantizer q=32 | 30/30 | none | ch 0 | ch 1 | OK |
| quality | no-preference | constant | 30/30 | none | ch 0 | ch 1 | OK |
| realtime | prefer-software | constant | 30/30 | **none** | ch 0 | ch 1 | OK |
| realtime | no-preference | constant | 30/30 | none | ch 0 | ch 1 | OK |
| realtime | prefer-software | const, 720p60 | 120/120 | none | ch 0 | ch 1 | OK |
| quality | prefer-software | const, 720p60 | 120/120 | none | ch 0 | ch 1 | OK |
| * | prefer-hardware | * | — | — | — | — | n/a (unsupported) |

Findings:

- **Chunk 0 is the anchor TU.** `TD + SEQUENCE_HEADER + FRAME(KEY, shown)`
  — the sequence header rides in-band; `metadata.decoderConfig.description`
  is **absent** for AV1 (confirmed, matching the W3C registration).
- **Chunk 1 is a valid golden.** `TD + FRAME(INTER, shown, showable,
  refresh=00000010)`. Its `ref_frame_idx` all point at slots holding the
  keyframe (slot 0), so it decodes directly after the anchor. Every delta
  chunk is a shown INTER — no hidden/alt-ref frames observed in `quality`
  mode for identical stills.
- **No frame drops in any working combo** — including `realtime` at
  720p60 — on an idle VM. Chromium's drop path only engages under encoder
  backpressure; the timestamp-gap detector is implemented and will surface
  drops if they occur.
- **The Chrome seq header sets `order_hint_bits=0`** (no order hints at
  all) and no `timing_info` — `info` prints `oh=0` per TU. The golden
  refreshes a single slot (`refresh=00000010` → slot 1), which is where
  `expand` parks its `show_existing` loop.
- `latencyMode` changes only the size profile (realtime keyframe 4149 B vs
  quality 3872 B at the same bitrate), not the TU structure.
- `decoderConfig.hardwareAcceleration` reports `no-preference` even when
  `prefer-software` was requested — don't trust it for "what was used".
- **End-to-end verified**: every working combo's IVF was accepted by
  `stillcast expand` (e.g. `--duration 60 --gop 300` → 1800 TUs, 54 KB),
  passed `stillcast info --check` (shown-KF start, no re-shown keyframe,
  all TUs shown, golden INTER+showable), and frame-exact decoded via
  ffmpeg/dav1d (1800 frames: 6 keyframe + 1794 golden-shown pixels).
- Concatenating the raw chunks (no IVF wrap) into an `.obu` stream is also
  accepted natively by `expand` — IVF remains the nicer artifact since it
  carries fps + geometry.

## Reproduce

```bash
python3 -m http.server -d . 8778   # from repo root
# open http://localhost:8778/examples/webcodecs/ in Chrome, click
# "Encode + build IVF", download webcodecs-src.ivf

stillcast info   -i webcodecs-src.ivf --verbose   # TU 0 anchor, TU 1 golden
stillcast expand -i webcodecs-src.ivf -o out.ivf --duration 60 --gop 300
stillcast info   -i out.ivf --check
ffmpeg -i out.ivf -f framemd5 -                   # decode-verify
```
