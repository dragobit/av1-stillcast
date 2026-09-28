# Integration API: external encoders and muxers

`make` is not the product — it is a *reference application* that proves the
interface. This doc designs the thin layer between stillcast's core and the
applications that drive it, so that CLI `make`, an ffmpeg pipeline, and a
browser app all consume the same contract.

```
stillcast core — true core        assemble: only the two TU generations
  ├─ shown-frame passthrough      (anchor KF TU + golden TU, verbatim)
  └─ show_existing_frame synthesis (repeat loop, golden-slot bookkeeping,
                                    seq-header rewrite for decoder model)

stillcast periphery               core-adjacent; membership debatable —
                                  see the boundary table below
  ├─ container sniff + input scan (container::read, split_input)
  ├─ sequence-header services     (av1C / codec string derivation)
  ├─ plan / size↔seek policy      (cost model over probe measurements)
  ├─ validation / diagnostics     (per-TU reasons, info --check)
  └─ IVF writer                   (the original container adapter)

thin integration API              owns: nothing — just types
  ├─ called from ffmpeg (bsf / pipe)
  ├─ called from WASM (browser app)
  └─ called from other muxers/pipelines

applications                      own: encode policy, mux, UX
  ├─ CLI make    = ffmpeg encode → API → ffmpeg mux
  ├─ ffmpeg pipe = libaom → API (bsf) → ffmpeg mux
  └─ browser make= WebCodecs → API (wasm) → Mediabunny mux
```

The key property: **the core's output is never shaped for one muxer.** It
produces AV1 temporal units plus the metadata a muxer needs; containers are
always the adapter's problem — including IVF, which today is the incidental
return type of `expand_ivf`.

## Core vs periphery — a contested boundary

"stillcast core" is not one thing. The *true* core is `assemble`'s two
kinds of AV1 generation — passing the two real coded frames through, and
synthesizing `show_existing_frame` TUs with correct golden-slot
bookkeeping. Everything around it is periphery, and **where each
peripheral piece belongs is a legitimate open question** — this design
takes positions below, but they are defaults to argue against, not axioms:

| periphery piece | case for core side | case for API/app side | this doc's default |
|---|---|---|---|
| container sniff (`container::read`: IVF/OBU/Annex-B) | "hand us bytes" is a friendly contract; adapters stay dumb | a bsf adapter receives `AVPacket`s, not a container — the sniff is dead code for it; a bare-TU entry point is needed anyway | keep in periphery, **two entry levels**: raw container bytes *or* an explicit TU list |
| input scan (`split_input`: anchor/golden acceptance) | it *is* the input contract — the invariant that makes expansion valid; contract enforcement can't be outsourced | — (nobody argues to move it) | core-adjacent, mandatory |
| seq-header services (av1C, `av01.*` string) | derivation needs the parsed header — already in hand | av1C/codec strings are container-registry knowledge, not AV1 semantics | periphery service the API exposes; core keeps only the parse |
| decoder-model header splice | already inside `assemble`'s output path (bit-level surgery on its own TUs) | — | stays in true core |
| plan / cost model | one canonical frontier keeps CLI and browser from drifting apart | pure arithmetic over probe sizes; could live in each app | periphery, exposed via API — one model, many UIs |
| diagnostics (`TuDiagnostic`, checks) | — | — | periphery, always |
| timestamps | index×fps is trivial arithmetic | muxers may want to snap to their own timescale | exported by the API as a convenience, computed in periphery |
| IVF writer | — | it's a container, like any other | adapter, same standing as `mp4.rs` |

The practical consequence of drawing the boundary *inside* "core" rather
than around it: the integration API ends up exposing some periphery
(diagnostics, plan) alongside the true core, and the debate reduces to
"what does `api.rs` re-export" — cheap to revisit without breaking
adapters.

## What exists today

`api.rs` is already the first cut at this boundary: pure bytes-in →
bytes-out, no filesystem/ffmpeg/clap. Decomposing `expand_ivf`'s internals
exposes the surfaces the API needs:

```
expand_ivf(input, params):
  container::read(input)         → IvfFile            # sniff IVF/OBU/Annex-B
  split_input(ivf)               → (key_tu, golden_tu) # per-TU diagnostics exist
  assemble_multi(segments, params)
                               → AssembleOutput {
                                   tus,              # the product
                                   key_samples,      # stss-equivalent ✓
                                   seq_header,       # parsed config ✓
                                   seq_header_obu,   # av1C source ✓
                                   golden_slot }
  ivf::write(...)              → Vec<u8>             # container — adapter work
```

Everything the integration API needs is already computed inside; the work
is *exposing* it, not building it. The IVF writer and `mp4.rs` become two
container adapters of equal standing — the latter stays the deterministic
verification reference.

## API surfaces

Rust-first (the C ABI and wasm bindings are projections of it); exact type
shapes are proposals, not commits.

### 1. Input acceptance — the short-encode contract

```rust
/// What any external encoder must supply: a short AV1 stream containing
/// an anchor TU (seq header + shown KEY_FRAME) and a golden TU (the shown
/// non-key frame coded directly after it). Container is sniffed:
/// IVF / low-overhead OBU / Annex-B.
pub fn accept_input(stream: &[u8]) -> Result<AcceptedStream, Rejection>;

pub struct AcceptedStream {
    pub key_tu: TemporalUnit,
    pub golden_tu: TemporalUnit,
    /// Parsed sequence header: dims, profile, level, colour — everything
    /// needed to derive av1C / the av01.* codec string.
    pub config: SequenceHeader,
    /// Rational fps of the donor stream, if the container declared one.
    pub timebase: Option<Rational>,
}

pub struct Rejection {
    /// Per-TU "nearest miss" reasons — split_input already computes these;
    /// today they only reach the user through anyhow error text.
    pub diagnostics: Vec<TuDiagnostic>,
}
```

Per the boundary table, acceptance gets a second entry level for adapters
that never see a container — an ffmpeg bsf holds `AVPacket`s, not a file:

```rust
/// Same contract, pre-demuxed input: bare temporal units in decode order.
pub fn accept_tus(tus: &[TemporalUnit], timebase: Option<Rational>)
    -> Result<AcceptedStream, Rejection>;
```

Why a separate entry point instead of folding this into `expand`:

- The encoder adapter (ffmpeg pipe, WebCodecs page, user-supplied IVF)
  needs to validate *before* the app commits to expansion parameters —
  e.g. the browser app wants to tell the user "your encoder produced X,
  here's why it's not usable" with structured reasons, not a string.
- `plan` needs `key_tu.len()`/`golden_tu.len()` from a probe encode —
  acceptance is the probe's output.
- Playlist = `accept_input` per segment + the existing
  byte-identical-seq-header check across `AcceptedStream`s.

### 2. Conditional packet/TU generation — expand

```rust
pub struct ExpandRequest<'a> {
    pub segments: &'a [AcceptedSegment<'a>], // accepted pair + frame count
    pub total_frames: u64,
    pub gop_size: u64,
    pub fps: Option<Rational>,   // None = keep donor timebase (e.g. 30000/1001)
    pub decoder_model: bool,
}

pub struct ExpandedStream {
    /// One low-overhead AV1 temporal unit per entry — the exact shape
    /// MediaBunny's EncodedPacket and ffmpeg's AVPacket both take.
    pub packets: Vec<TemporalUnit>,
    /// stss-equivalent: which packets are random-access points.
    /// (AssembleOutput.key_samples, de-1-based.)
    pub keyframes: Vec<bool>,
    /// Presentation timestamps — derivable from index×fps, exported so no
    /// adapter re-derives timing policy.
    pub timestamps: Vec<Rational>,
    pub codec: Av1CodecConfig,   // codec string + raw seq header OBU + dims
}

pub struct Av1CodecConfig {
    pub codec_string: String,    // 'av01.P.LLT.DD.CP…' — new: mp4.rs builds
                                 // av1C from seq_header today; formatting the
                                 // string from the same parse is additive
    pub seq_header_obu: Vec<u8>, // for muxers that take the OBU (mkv/webm)
    pub width: u32,
    pub height: u32,
}
```

Consumer mapping — the point of returning packets, not IVF:

| adapter | consumes |
|---|---|
| ffmpeg bsf | each `TemporalUnit` → one `AVPacket`; `keyframes` → `AV_PKT_FLAG_KEY`; `timestamps` → pts |
| Mediabunny (wasm) | each TU → `EncodedPacket(data, ts, type)` into `EncodedVideoPacketSource('av1')`; `codec_string` → `decoderConfig.codec` (Mediabunny builds `av1C` itself — `description` is unused for AV1, verified against the codec registry) |
| mp4.rs (internal writer) | same packet list — becomes one mp4 adapter among peers |
| IVF writer | wrap packets back in `DKIF` headers — a container adapter, not core |

`expand_ivf` survives as `expand()` + IVF adapter — the stable bytes→bytes
contract is unchanged for existing C ABI users.

### 3. plan — policy as API

Today's plan logic lives inline in the CLI (probe encode → kf/golden/se
sizes → gop sweep printed to stdout). It is a pure function and should be
one:

```rust
pub struct PlanRequest {
    pub keyframe_bytes: u64,      // from a probe accept_input/expand
    pub golden_bytes: u64,
    pub show_existing_bytes: u64, // ~6 B, measured from a probe expand
    pub duration: Rational,
    pub fps: Rational,
    pub target_seek: Option<Rational>,
    pub max_size: Option<u64>,
}

pub struct PlanResult {
    pub gop: u64,
    pub total_frames: u64,
    pub estimated_stream_bytes: u64,
    /// The sweep table the CLI prints — for UIs that want to render it.
    pub candidates: Vec<PlanCandidate>, // gop → bytes, kbps, worst seek
}
```

CLI `plan` becomes a formatter over this. Browser `make` feeds
`PlanRequest` from its probe encode and uses the result in its
quantizer/encode/mux loop — same policy, different encoder knob (see §5).

### 4. Diagnostics as data

`split_input`'s per-TU reasons and `info --check`'s findings are currently
strings on stderr. As structured values they serve both applications:

- `TuDiagnostic { tu_index, reason }` inside `Rejection` — a browser UI can
  render "chunk 3 was a hidden frame, so it couldn't be the golden" instead
  of scraping an error message; the ffmpeg bsf can log them through
  `av_log`.
- `check_stream(&ExpandedStream) -> Vec<Finding>` — what `info --check`
  does, callable so adapters self-verify (the wasm app asserts its own
  output before muxing; e2e asserts the same).

## Adapter contract

The API freezes what each side owes:

**Encoder adapter owes**: a short AV1 stream meeting the input contract —
any encoder, any container we sniff. Core never embeds an encoder; how the
anchor/golden pair came to be is the adapter's business.

**Muxer adapter owes**: nothing upstream knows about — it receives
`packets` + `keyframes` + `timestamps` + `codec` and owns stbl/mvhd/Cues
equivalents, timescales, and format quirks.

## Reference application: browser `make`

With the surfaces above, the browser app is a thin orchestrator:
WebCodecs → `accept_input` → `plan` → `expand` → Mediabunny.
`examples/webcodecs` already proves the encoder side on Chrome 137 (libaom
software path): chunk 0 = anchor, chunk 1 = valid golden, no hidden frames,
no drops in quality mode.

Mediabunny coverage of the post-#40/#41 ffmpeg mux spec, verified against
Mediabunny 1.55's API:

| `mux_mp4_ffmpeg` behaviour | Mediabunny equivalent |
|---|---|
| `-c:v copy` (IVF → av01 track) | `EncodedVideoPacketSource('av1')`, one `EncodedPacket` per TU — already low-overhead OBU, the registry's required shape |
| `-c:a copy` for AAC input | `Input` + `EncodedPacketSink` → `EncodedAudioPacketSource('aac')` — true packet passthrough |
| `-c:a aac -b:a` otherwise | decode via `Input` → re-encode `AudioEncoder('mp4a.40.2')` at `bitrate` |
| `-map 1:a:0` (first audio track only) | `getPrimaryAudioTrack()` — ignores extra audio tracks / real video streams in the container |
| `-map_metadata 1` | `input.getMetadataTags()` → `output.setMetadataTags()`; `metadataFormat:'mdir'` = same ilst atoms; `MetadataTags.raw` carries nonstandard keys |
| `-map_metadata:s:a:0` (language) | `track.getLanguageCode()`/`getName()` → `addAudioTrack(src, {languageCode, name})` |
| `-disposition:v:1 attached_pic` cover track | **different mechanism** — see below |
| `+faststart` | `fastStart:'in-memory'`, or `'reserve'` — packet counts are known exactly before muxing, so `maximumPacketCount` is settable and media never buffers |

### Cover art: the one semantic difference

ffmpeg writes the cover as a second video track (jpeg/png sample entry)
flagged `attached_pic`; #41 probes `attached_pic=1` on the source and only
accepts jpeg/png fallbacks. Mediabunny cannot reproduce that literally —
`TrackDisposition` has no attached-picture flag and its mp4 writer takes no
jpeg/png video track. Use `setMetadataTags({images:[{data, mimeType,
kind:'coverFront'}]})` → an iTunes-style `covr` item, which is what
iTunes/Music, Android and most web players read anyway. Source order
preserved: the audio's embedded image wins (`getMetadataTags().images`),
else the still re-encoded via `canvas.toBlob('image/jpeg')` — the jpeg/png
gate becomes a mimeType filter.

### Deliberate non-parities (and why they're fine)

- **CRF ladder → quantizer ladder**: WebCodecs has no libaom `-crf`; it has
  `bitrateMode:'quantizer'` + `av1:{quantizer}` (verified in the example's
  matrix). `qindex ≈ crf×4`, so the ladder [requested,40,48,56,63] →
  q [req,160,192,224,255]. The policy loop is unchanged — sizes are
  measured, not modelled.
- **Encoder availability**: AV1 `VideoEncoder` is Chrome/Edge-only today.
  Fallbacks in order: accept user-supplied IVF/OBU (the input contract is
  encoder-agnostic by design), optional wasm libaom (~1–2 MB, universal).
- **Opus/other audio in mp4**: ffmpeg's mux takes it experimentally;
  browser path transcodes to AAC — same as `make`'s existing fallback.

## Milestones (ordered by interface, not by app)

1. **Expose the surfaces in `api.rs`** — `accept_input`, `expand`
   returning `ExpandedStream`, `plan`, `TuDiagnostic`. `expand_ivf`
   reimplemented as expand+IVF-wrap; `mp4.rs` and CLI `plan`/`info`
   migrate onto the new returns. No behaviour change — refactor + exports.
2. **CLI `make` on the new API** — proves the interface serves the
   existing app; e2e unchanged.
3. **wasm binding** — wasm-packable wrapper projecting the same types;
   browser app = `examples/mediabunny/` static page consuming them.
4. **browser `make` end-to-end** — encode → expand → Mediabunny mux, with
   audio copy/transcode, tags, language, covr cover, faststart.
5. **ffmpeg bsf** (`av1_stillcast`) — the third consumer; validates the
   API from the other direction.
6. **compat sweep** — `docs/compat.md` matrix on browser-muxed outputs.

Validation throughout reuses existing harnesses: `info --check` →
`check_stream` on wasm output; `browser_seek_test.py` (CDP) on the
browser-muxed file; `ffprobe` diff vs the ffmpeg-muxed file asserting every
row of the mapping table.
