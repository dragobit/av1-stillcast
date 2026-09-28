# Browser `make`: WebCodecs + WASM + Mediabunny

Design for reproducing `stillcast make` (image + audio → faststart mp4 with
metadata, language tags and cover art) entirely in the browser, without
ffmpeg. The three stages map onto three browser-native pieces:

```
make pipeline                          browser equivalent
─────────────────────────────────────  ────────────────────────────────────
encode (libaom via ffmpeg, 2 frames)   VideoEncoder('av01.*')  [proven:
                                       examples/webcodecs]
expand (stillcast core, Rust)          same crate → wasm32 (api.rs is already
                                       bytes-in→bytes-out)
mux mp4 (ffmpeg: -c copy, -map_metadata Mediabunny: EncodedVideo/Audio
  PacketSource, setMetadataTags,       PacketSource,
  -disposition attached_pic,           languageCode, images[], fastStart
  +faststart)
```

Everything below is checked against Mediabunny 1.55's public API
(`EncodedVideoPacketSource`, `EncodedAudioPacketSource`,
`Mp4OutputFormat`, `Output.setMetadataTags`, `Input.getMetadataTags`,
track `languageCode`/`disposition` metadata).

## Stage 1 — encode: WebCodecs AV1

`examples/webcodecs` already proves this stage end-to-end on Chrome 137
(libaom software path): a 2+ frame encode of an identical still yields

- chunk 0 = anchor TU (`TD + SEQUENCE_HEADER + FRAME(KEY, shown)`);
- chunk 1 = a valid golden (`TD + FRAME(INTER, shown, showable,
  refresh=00000010)`) that `expand` accepts;
- no hidden frames, no drops in quality mode, `order_hint_bits=0`.

Consequences for the design:

- **The `encode` contract holds on WebCodecs.** The JS-side OBU scan in
  `examples/webcodecs/app.js` (find anchor, find first decode-adjacent
  shown non-key TU, reorder) is still needed — `split_input` is positional.
- **CRF ladder → quantizer ladder.** WebCodecs has no libaom `-crf`; it
  exposes `bitrateMode: 'quantizer'` with `av1: { quantizer: 0..255 }`
  (verified supported in the example's matrix). `--max-size` keeps its
  probe→expand→mux→measure loop unchanged, walking a quantizer ladder
  (libaom `qindex ≈ crf × 4`, so crf 32/40/48/56/63 → q ≈ 128/160/192/224/255).
  Sizes stay measured, not modelled, so the policy code is identical.
- **Availability is the real gap.** AV1 `VideoEncoder` exists in
  Chrome/Edge only today; Firefox/Safari have AV1 *decode* but no encode.
  Fallback options, in order of preference: (a) accept a user-supplied IVF/
  OBU encode (UI already needs an "advanced: supply your own 2-frame IVF"
  path — same as `expand` on the CLI), (b) ship a WASM libaom build
  (~1–2 MB, slow but universal). Gate on
  `VideoEncoder.isConfigSupported({codec:'av01.0.08M.08', ...})`.

## Stage 2 — expand: stillcast core on wasm32

The core layer is pure bitstream→bitstream, no I/O, no deps beyond the
crate itself — the ideal WASM citizen. `api.rs` already defines the stable
boundary (`expand_ivf`/`expand_ivf_multi`), so the work is a thin
wasm-bindgen (or plain `cdylib` + JS glue) wrapper, not a port.

Proposed wasm API (returns what the mux stage needs, not just bytes):

```ts
expand(ivf: Uint8Array, opts: {gop?, targetSeek?, duration, fps, ...})
  → { packets: Uint8Array[] /* one low-overhead OBU TU each */,
      keyframe: boolean[]      /* stss equivalent */,
      codecString: string      /* 'av01.P.LLT.DD…' from seq header */,
      timescale: {num, den}    /* rational fps, preserved */ }
```

Notes:

- Emitting packets directly skips IVF re-parsing, but keeping the IVF
  wrapper costs nothing and preserves byte-parity with the CLI for
  debugging; either is fine — pick packets + flags to feed Mediabunny
  without a second scan. Keyframe indices are already known inside
  `assemble` (every GOP start / segment boundary), so this is an export,
  not a new analysis.
- `av1C`/codec string: `mp4.rs` already derives `av1C` from the parsed
  sequence header; Mediabunny wants the `av01.*` codec string instead
  (it builds `av1C` itself — `description` is *not* used for AV1), so the
  wasm should emit the codec string from the same parse.
- Playlist = `expand_ivf_multi`; seq-header identity across segments must
  hold, which means one identical `VideoEncoderConfig` for all images —
  Chrome's libaom path is deterministic for fixed config+geometry, but
  the wasm wrapper should still verify and report (same rejection the
  Rust code already does).

## Stage 3 — mux: Mediabunny replaces the ffmpeg invocation

`mux_mp4_ffmpeg` (src/main.rs) does five things; each has a direct
Mediabunny equivalent:

| ffmpeg flag / behaviour | Mediabunny |
|---|---|
| `-c:v copy` (IVF packets → `av01` track) | `EncodedVideoPacketSource('av1')`, one `EncodedPacket` per TU; packets are already low-overhead OBU — the exact format Mediabunny's AV1 registry requires. First `add()` carries `decoderConfig.codec` = wasm-emitted codec string |
| `-c:a copy` when input is AAC | `Input` + `EncodedPacketSink` → `EncodedAudioPacketSource('aac')` — true packet-level copy, bitstream untouched |
| `-c:a aac -b:a …` otherwise | decode the input track and re-encode via `AudioSource`/WebCodecs `AudioEncoder('mp4a.40.2')` with `bitrate` |
| `-map_metadata 1` | `input.getMetadataTags()` → `output.setMetadataTags(...)`; `metadataFormat: 'mdir'` gives the same ilst-style atoms ffmpeg writes; nonstandard keys survive via `MetadataTags.raw` |
| `-map_metadata:s:a 1:s:a` (language etc.) | `track.getLanguageCode()`/`getName()`/`getDisposition()` → `addAudioTrack(src, {languageCode, name})` |
| `-disposition:v:1 attached_pic` (cover as a second video track) | **different mechanism — see below** |
| `-movflags +faststart` | `fastStart: 'in-memory'` (moov before mdat) or `'reserve'` — see below |

### Cover art: `attached_pic` track vs `covr` atom

ffmpeg writes the cover as a *second video track* (jpeg/png sample entry)
with the `attached_pic` disposition. Mediabunny cannot reproduce this
exactly: `TrackDisposition` has no attached-picture flag, and its mp4
writer doesn't accept jpeg/png "video" tracks.

Use `setMetadataTags({images: [{data, mimeType, kind: 'coverFront'}]})`
instead, which writes an iTunes-style `covr` item in `ilst`. This is equal
or better in practice — `covr` is what iTunes/Music, Android and most web
players actually read — but it is a semantic difference worth stating in
the UI ("cover embedded as metadata"). Source order mirrors
`mux_mp4_ffmpeg`: the audio's own embedded image
(`getMetadataTags().images`) wins; otherwise the still image itself is
encoded to JPEG/PNG via `canvas.toBlob()` and used.

### faststart: `in-memory` vs `reserve`

`fastStart: 'in-memory'` matches `+faststart` semantics directly (moov
first, media buffered until finalize). But stillcast is unusually suited
to `'reserve'`: the video packet count is known *exactly* before muxing
(deterministic expansion), and the audio packet count is known after the
first demux pass, so `maximumPacketCount` can be set precisely and the
media never needs buffering. Recommendation: `'in-memory'` by default
(simplest, stillcast files are small — the audio dominates at ~10 MB/h
for 128 kbps AAC), `'reserve'` when supporting very long durations where
buffering the full file in a tab is undesirable.

### Audio demux coverage

Mediabunny `Input` parses mp4/m4a, mkv/webm, mp3, wav, flac, ogg, aac/adts
— covering everything `ensure_adts` handles via ffmpeg except exotic
containers. AAC-in-anything → copy; anything else → AAC re-encode. One
parity gap: ffmpeg's mux accepts Opus in mp4; the browser path can either
transcode Opus→AAC (safest, matches today's `-c:a aac` fallback) or emit
`.webm`/`ogg` variants later. Keep AAC-only for v1.

## Proposed shape

```
examples/mediabunny/          static page, no build step (like webcodecs/)
  app.js                      orchestrator = the `make` loop
  stillcast_expand.wasm       built from crate, wasm-pack target
  mediabunny (esm/cdn)        mux + demux + tags
```

Loop mirrors `make` exactly:

```
for q in quantizerLadder:            # only when --max-size
  tu2 = webcodecsEncode(image, q)    # anchor + golden
  pkts = wasmExpand(tu2, policy)     # gop/duration/fps/playlist
  mp4 = mediabunnyMux(pkts, audio,   # copy-or-transcode
                      tags, lang, cover, fastStart)
  if fits(mp4, budget): break
```

## Differences from the ffmpeg path — summary

| area | status |
|---|---|
| video bitstream | identical (same assembler) |
| audio AAC copy | identical (packet passthrough) |
| audio re-encode | WebCodecs AAC instead of ffmpeg AAC — same codec, different encoder tuning |
| metadata tags | same ilst atoms (`mdir`); arbitrary keys via `raw` |
| language | preserved via `languageCode` |
| cover art | `covr` metadata instead of an `attached_pic` video track — deliberate semantic change |
| faststart | identical layout goal; `in-memory` or `reserve` |
| encode policy | quantizer ladder instead of CRF ladder |
| encoder availability | Chrome/Edge only; IVF-input fallback + optional wasm libaom |
| determinism | expand output stays byte-deterministic; mp4 bytes differ (different muxer) — fine, determinism is only promised for `expand` |

## Validation plan

Reuses the existing harnesses rather than new infrastructure:

- `stillcast info --check` on the wasm-emitted stream (keeps the CLI's
  verification contract).
- `scripts/browser_seek_test.py` (CDP harness) against the browser-muxed
  mp4 — seek-to-mid-GOP and full playthrough in the same browser that
  produced it.
- `ffprobe` diff against the ffmpeg-muxed file: stream disposition,
  language, tags, `covr`, moov-before-mdat — asserting every row of the
  mapping table above, not just decode-ability.
- `scripts/compat_players.sh` + `docs/compat.md` matrix once the pipeline
  produces files.

## Milestones

1. **wasm build of the core** — wrapper over `api.rs`, npm-consumable.
   No repo logic changes needed beyond the export list.
2. **video-only mp4** — webcodecs example + wasm expand + Mediabunny
   `EncodedVideoPacketSource`; verify vs `expand -o .mp4`.
3. **audio + metadata** — packet copy, language, tags, `covr` cover.
4. **transcode + `--max-size` ladder + playlist** — feature parity with
   `make`.
5. **compat sweep** — run the muxed files through `docs/compat.md`'s
   player matrix.
