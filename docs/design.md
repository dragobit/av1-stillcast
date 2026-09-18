# Design

## Principle

Don't write an encoder — write a **bitstream assembler**. A standard encoder
(libaom) produces the small set of real coded frames; this tool handles GOP
policy, reference-slot bookkeeping, and cheap TU synthesis. That gives full
control over the size/seek/compatibility frontier for ~5% of the effort of a
codec.

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

## Crate layout

| module | role |
|---|---|
| `bitio` | MSB-first bit reader/writer, uvlc, leb128, trailing bits |
| `obu` | OBU header parse/serialize, OBU type enum |
| `seq_header` | full sequence-header walk → flags needed downstream |
| `frame_header` | partial uncompressed-header parse (stops after refresh_frame_flags) |
| `ivf` | IVF container read/write (packets = temporal units) |
| `assemble` | input validation, golden-slot selection, GOP expansion |
| `main` | `stillcast assemble` / `stillcast info` CLI |

## Input contract

Input IVF must contain ≥2 packets produced by a conformant encoder for the
same static picture: packet 0 = seq header + shown keyframe; packet 1 = the
golden (a shown inter frame is what libaom emits for identical content).
Rejected up front: reduced still-picture headers, frame id numbers,
decoder-model timing info, film grain.

## What is deliberately not done

- **Muxing real containers** — IVF is a demo format; ffmpeg remuxes to
  mp4/mkv fine. Native mp4 writer (stss sync table) is roadmap.
- **Encoding** — libaom/ffmpeg remains the frame factory. Later we may drive
  it for a one-command flow.
- **Refreshed/motion content** — the target is exactly-static visuals.
  Periodic jacket changes could be layered later as additional golden frames.
