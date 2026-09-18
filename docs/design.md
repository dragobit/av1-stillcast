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
| `mp4` | ISOBMFF writer: ftyp+mdat+moov, av01/av1C + mp4a/esds, stss |
| `adts` | ADTS parser → raw AAC frames + AudioSpecificConfig |
| `assemble` | input validation, golden-slot selection, GOP expansion |
| `main` | `stillcast assemble` / `stillcast info` CLI |

## Input contract

Input IVF must contain ≥2 packets produced by a conformant encoder for the
same static picture: packet 0 = seq header + shown keyframe; packet 1 = the
golden (a shown inter frame is what libaom emits for identical content).
Rejected up front: reduced still-picture headers, frame id numbers,
decoder-model timing info, film grain.

## MP4 output

Layout is `ftyp | mdat | moov` (mdat first, so chunk offsets are known in
one pass). The video track carries an `av01` sample entry whose `av1C` box
is derived from the parsed sequence header; keyframes land in `stss`, so
seek granularity = gop. The audio track takes an ADTS file, strips the
7-byte headers into `mp4a` samples, and writes `esds` with the
AudioSpecificConfig. Timescales: video = fps, audio = sample rate; all
creation/modification times are zeroed to keep output byte-deterministic.

## What is deliberately not done

- **Encoding** — libaom/ffmpeg remains the frame factory. Later we may drive
  it for a one-command flow.
- **Refreshed/motion content** — the target is exactly-static visuals.
  Periodic jacket changes could be layered later as additional golden frames.

## Path to ffmpeg

The assembler is a pure bitstream→bitstream transform, which maps cleanly
onto an **ffmpeg bitstream filter** (same shape as `av1_metadata` bsf):

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
the modern ffmpeg option convention. Planned as the long-term landing so
the behavior is reachable through ffmpeg itself.
