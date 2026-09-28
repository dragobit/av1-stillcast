# Browser `make`: the reference application

Application-layer design for an in-browser `stillcast make` equivalent.
It consumes **only** the integration API surfaces from
[`docs/integration-api.md`](integration-api.md) — no core internals leak
into the app. If something here needs a core change, the API doc is wrong
first.

```
image(s) ──► WebCodecs VideoEncoder(av01) ──┐
                                           ├─► wasm: accept_tus ─► plan ─► expand
user-supplied IVF/OBU (fallback) ──────────┘         │
audio file ─► Mediabunny Input (demux/tags/cover) ───┤
                                                   ▼
                              Mediabunny Output (mp4 mux adapter)
                              EncodedVideoPacketSource('av1')
                              EncodedAudioPacketSource('aac')
                              setMetadataTags / languageCode / covr
                              fastStart → download (.mp4)
```

## wasm binding — the API projection

One `wasm-pack`-style module wrapping `api.rs`; JS sees the same four
surfaces, serialized flat (no serde wasm needed — arrays and strings only):

```ts
// accept — browser path uses the TU-level entry: WebCodecs already hands
// back one EncodedVideoChunk per temporal unit, so no container sniff is
// needed at all. The OBU scan from examples/webcodecs orders chunks into
// [anchor, golden, ...] before calling.
acceptTus(tus: Uint8Array[], fpsNum: number, fpsDen: number)
  : { ok: true, stream: AcceptedStream }
  | { ok: false, diagnostics: { tu: number, reason: string }[] }

plan(req: { keyframeBytes, goldenBytes, showExistingBytes,
            durationNum/Den, fpsNum/Den, targetSeek?, maxSize? })
  : { gop, totalFrames, estimatedBytes, candidates[] }

expand(streams: AcceptedStream[], req)
  : { packets: Uint8Array[],  // low-overhead OBU per TU
      keyframes: Uint8Array,  // bitmap or index list
      codecString: string,    // 'av01.P.LLT.DD…'
      seqHeaderObu: Uint8Array, width, height }

checkStream(stream: ExpandedStream) : { findings: Diagnostic[] }
```

Boundary notes:

- `accept_tus` (not `accept_input`) is the browser's natural entry:
  `EncodedVideoChunk` payloads *are* low-overhead OBU TUs — the webcodecs
  example confirmed chunk↔TU 1:1. `accept_input` still ships for the
  user-supplied-IVF fallback path.
- Packets cross the wasm boundary once, as an array of `Uint8Array` views
  into a single flat buffer — no per-packet copies.
- The codec string is exported, not derived in JS: `mp4.rs` already parses
  the seq header; the wasm emits `av01.*` from that parse (MediaBunny
  builds `av1C` itself — `description` is unused for AV1 per its codec
  registry).

## Pipeline, per stage

### 1. Encode (or accept)

Primary path — `examples/webcodecs` is the proven recipe:

- `VideoEncoder({codec:'av01.0.08M.08', width, height, framerate:fps,
  bitrateMode:'quantizer', av1:{quantizer:q}, latencyMode:'quality',
  hardwareAcceleration:'prefer-software'})`, N identical `VideoFrame`s,
  `keyFrame` forced on frame 0 only.
- The JS OBU scan (already in `examples/webcodecs/app.js`) picks
  anchor/golden positionally and reorders chunks — carried over verbatim.
- **Drop detection**: compare submitted vs returned chunk timestamps; a
  gap means the encoder silently dropped → re-encode or surface via
  diagnostics.
- **Playlist**: all segments share one `VideoEncoderConfig`; wasm
  `accept_tus` per image + the API's byte-identical seq-header check
  enforces the constraint. (Chrome's libaom path is deterministic for
  fixed config+geometry — verified by the check, not assumed.)

Fallbacks, in order, when `VideoEncoder.isConfigSupported` says no
(Firefox/Safari today):

1. **User-supplied 2-frame IVF/OBU** — "advanced" file input feeding
   `accept_input` directly. Same contract, zero encoder.
2. **wasm libaom** (~1–2 MB lazy-loaded) — universal but slow; only if
   the fallback alone proves insufficient in practice.

### 2. Plan

Probe encode at the requested quantizer → `accept_tus` → feed TU sizes
into `plan()`. `--max-size` becomes the same measure→retry ladder as CLI
`make`, walking quantizers (`qindex ≈ crf×4` → e.g. q 128/160/192/224/255)
with expand+mux per candidate. The browser is *faster* at this than the
CLI: wasm expand is ~30 ms and mux is in-memory, so each candidate costs
one short WebCodecs encode.

### 3. Mux — the Mediabunny adapter

```ts
const output = new Output({
  format: new Mp4OutputFormat({ fastStart, metadataFormat: 'mdir' }),
  target: new BufferTarget(),          // or StreamTarget + 'reserve'
});

const video = new EncodedVideoPacketSource('av1');
output.addVideoTrack(video, { frameRate: fps });
// audio + tracks below, then:
output.setMetadataTags(tags);          // before output.start()
await output.start();
expanded.packets.forEach((p, i) => video.add(
  new EncodedPacket(p, expanded.keyframes[i] ? 'key' : 'delta',
                    ts(i), duration(i)),
  i === 0 ? { decoderConfig: { codec: expanded.codecString,
            codedWidth: w, codedHeight: h } } : undefined));
```

Mapping to the ffmpeg spec (`mux_mp4_ffmpeg`, post-#41):

| ffmpeg behaviour | browser adapter |
|---|---|
| `-c:v copy` | packets added verbatim — no re-encode ever |
| `-map 1:a:0` first audio only | `input.getPrimaryAudioTrack()` — extra audio tracks and real video streams ignored |
| `-c:a copy` (AAC) | `EncodedPacketSink` → `EncodedAudioPacketSource('aac')`, packet passthrough |
| `-c:a aac -b:a` (else) | track → decode → `AudioEncoder('mp4a.40.2', {bitrate})` |
| `-map_metadata 1` | `getMetadataTags()` → `setMetadataTags()`; nonstandard keys ride `MetadataTags.raw` |
| `-map_metadata:s:a:0` | `getLanguageCode()`/`getName()` → `addAudioTrack(src, {languageCode, name})` |
| attached_pic cover | **covr instead** — `setMetadataTags({images:[{kind:'coverFront', data, mimeType}]})`; source order kept: audio's embedded `images[0]` wins, else the still via `canvas.toBlob` (jpeg/png only, mirroring the `is_mp4_coverable` gate). Semantic difference vs ffmpeg's second video track is documented, not hidden — Mediabunny has no attached-picture disposition |
| `+faststart` | `'in-memory'` default; `'reserve'` + `StreamTarget` for long outputs — video packet count is known pre-mux, audio count known after one demux pass, so `maximumPacketCount` is exact |

### 4. Verify + deliver

- `checkStream()` runs before mux — the browser self-verifies the same
  invariants `info --check` asserts, findings rendered in-page.
- `accept_tus` diagnostics render per-chunk rejections ("chunk 3: hidden
  frame — can't be golden") instead of a wall of error text.
- Download via `BufferTarget` → `Blob`; or `StreamTarget` straight to
  `showSaveFilePicker()` where supported.

## Non-goals (inherited from the API design)

- No encoder inside stillcast core — WebCodecs/wasm-libaom are app-side.
- No mp4 writer in the browser path — `mp4.rs` stays the CLI's
  deterministic verification reference; the browser muxes through
  Mediabunny alone.
- No parity claim on `attached_pic` — `covr` is a deliberate, documented
  substitution.

## Failure modes and UX answers

| failure | surface |
|---|---|
| `isConfigSupported` false for av01 encode | IVF upload fallback; wasm-libaom note |
| encoder drops frames | timestamp-gap detection → auto-retry once, else diagnostic |
| golden TU rejected | `accept_tus` diagnostics rendered per chunk |
| audio codec not AAC & encode unsupported | error listing the codec; suggest AAC source |
| output size over budget | quantizer ladder exhausts → report best-effort size |

## Validation

- `scripts/browser_seek_test.py` (CDP) against the muxed mp4: seek to
  mid-GOP + full playthrough in the producing browser.
- `ffprobe`/`mp4dump` diff vs a `make`-produced reference file: stream
  order, language, tags, `covr` presence, moov-before-mdat.
- `checkStream` output cross-checked against `stillcast info --check` on
  the same wasm-produced stream.
- `docs/compat.md` player matrix once real files exist.
