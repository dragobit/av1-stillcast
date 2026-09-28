# Layer-3 I/O: transport-agnostic OBU plumbing

Design record for how stillcast moves AV1 data at the bitstream
serialization layer. Companion to
[`integration-api.md`](integration-api.md) (which owns the API surfaces)
and [`browser-make.md`](browser-make.md) (which consumes them); this doc
owns only the *transport* question: in what byte-shape does AV1 enter and
leave stillcast.

## The layer model

```
1. syntax unit      OBU (header + payload)
2. grouping         TU = the OBU sequence for one decode/display step
3. serialization    low-overhead OBU stream  |  Annex-B
4. container        IVF / MP4 / MKV           (adds timing, audio, metadata)
```

stillcast's transform operates on layer 2. Everything this doc decides is
how layers 3–4 present data to it and take it back.

Two facts drive the whole design:

- **The transports collapse to one thing.** File, pipe, and in-memory
  buffer are just access patterns over the same byte sequence; no
  transport choice changes the data. An interface that takes `&[u8]`
  already serves all three — the host does the I/O.
- **The layer-3 formats are losslessly interconvertible.** Low-overhead
  ⇄ Annex-B is pure re-framing (leb128 nesting added or removed); no
  decode, no re-encode, OBU payloads untouched. `container.rs` already
  implements both directions (`split_annexb` / `write_annexb` /
  `write_obu_stream`, round-trip-tested).

## Positions

### 1. Canonical wire form: low-overhead OBU, one TU per packet

The exchange unit between stillcast and any adapter is a **single
temporal unit serialized in the §5 low-overhead form** — the exact shape
Mediabunny's `EncodedPacket.data`, ffmpeg's `AVPacket`, and WebCodecs'
`EncodedVideoChunk` all carry. This is already what
`ExpandedStream.packets` returns in the integration API; this doc makes
it the *stated* contract rather than an emergent one.

Consequences:

- TU boundaries inside a serialized stream are marked by a leading
  TemporalDelimiter OBU; streams that omit TDs are grouped by coded frame
  (existing `split_obu_stream` behaviour).
- Canonical TUs carry explicit `obu_size` fields and at most one leading
  TD (existing `canonicalize_tu` normalization).

### 2. Input: three serializations, one normalization

Unchanged from `input-formats.md`: `container::read` sniffs
IVF (layer 4), low-overhead OBU, and Annex-B, and normalizes everything
to `(timestamp, TU)` pairs. Adapters that never see a container
(ffmpeg bsf with `AVPacket`s, WebCodecs with per-chunk TUs) enter at
`accept_tus` with pre-demuxed TUs — also per `integration-api.md`.

Annex-B stays an *input* acceptance path plus an optional *output* form —
it is never the exchange contract, because Mediabunny doesn't speak it.

### 3. Output: expose the layer-3 writers

`write_obu_stream` / `write_annexb` exist today but are only exercised by
round-trip tests — no CLI or FFI surface can select them. Add output
format selection so stillcast can be a layer-3 → layer-3 pipe:

| output selection | writer | notes |
|---|---|---|
| `.ivf` extension (default) | `ivf::write` | unchanged |
| `.mp4` extension | `mp4.rs` | unchanged |
| `.obu` extension or `--format obu` | `write_obu_stream` | pure layer-3 output; no timing. fps-dependent metadata is caller's business |
| `.av1b` extension or `--format annexb` | `write_annexb` | for broadcast/HW pipelines that want length-delimited TUs |

`-o -` (stdout) keeps writing IVF as today; an explicit `--format`
overrides the extension guess, so a pipe can emit raw OBU
(`expand - -o - --format obu` → OBU stream on stdout → `ffmpeg -f obu -i -`).

Layer-3 output carries **no timestamps** — the timebase the adapter
needs is exported through `ExpandedStream.timestamps` / `Av1CodecConfig`
in the API, and `--fps` notes in the CLI. Documenting that drop is part
of this contract: an adapter picking `.obu` output has opted out of
container timing.

### 4. Transport matrix per host

| host | in | out |
|---|---|---|
| CLI | file path, or `-` = stdin | file path by extension, or `-` = stdout, `--format` overrides |
| C ABI (`ffi.rs`) | `(ptr, len)` buffer | `(ptr, len)` buffer — add a format field alongside `params` |
| wasm binding | `Uint8Array` (container bytes) *or* `Uint8Array[]` (TUs via `acceptTus`) | `Uint8Array[]` packets (per `browser-make.md`), plus `Uint8Array` when a serialized stream is requested |
| ffmpeg bsf | `AVPacket`s → `accept_tus` | `AVPacket`s — packets pass through unchanged, layer 4 never appears |

The Rust core signature is already the common denominator:
`fn(&[u8]) -> Result<Vec<u8>>` for container-level and
`fn(&[TemporalUnit]) -> Result<Vec<TemporalUnit>>` for TU-level.
Every binding above is a projection of one of those two.

### 5. Naming: generic entry, format-named writers, frozen aliases

Because one function may accept several serializations (sniffed) or emit
any of them, names must not lie about the format:

| tier | names | rule |
|---|---|---|
| **Generic entry points** | `accept_input(&[u8])`, `accept_tus(&[TemporalUnit])`, `expand(..)` | named for the *contract* (bytes-sniffed input, pre-demuxed TUs), never for a container. `expand_ivf` is a counterexample: it reads OBU/Annex-B too — don't repeat that mistake |
| **Format-specific writers** | `ivf::write`, `mp4` writer, `write_obu_stream`, `write_annexb` | named for exactly what they emit; adapters only |
| **Compat aliases** | `expand_ivf`, `expand_ivf_multi` | frozen C ABI surface, kept for existing callers. Semantically "`expand` + IVF adapter", and the doc comment already says so — treat as deprecated spellings of the generic API, not the canonical entry |

So the public surface a new adapter should meet is
`accept_input`/`accept_tus` → `expand` → a writer of choice; the `*ivf`
names survive only where renaming would break ABI.

### 6. Streaming: explicit non-goal (for now)

The core is `Vec<TU>`-based — whole input in memory, whole output in
memory. For the actual workloads (a 2-frame input expanded to a known
frame count) this is correct: input is tiny, output is ~show_existing
overhead × N, and even a 3-hour output is a few MB.

A chunked/push API (`accept` → repeated `expand_next(n)` calls) is a
legitimate future surface for pipelines that want to interleave muxing
with expansion, but nothing in the adapter table needs it. Revisit only
if a real adapter asks for backpressure.

## What this deliberately does not do

- **No layer-4 thickening.** MP4 input stays out of `expand`/`plan`
  (ffmpeg escape hatch per `input-formats.md`); no EBML/WebM parser;
  no fragmented-mp4 reader. The layer-3 surface is where embedding
  breadth lives.
- **No transport machinery in the core.** No async, no file handles, no
  sockets. Pipe support is "the CLI reads stdin", not a protocol.
- **No format negotiation.** Annex-B output exists because it's ten lines
  over `write_annexb`, not because adapters are expected to want it;
  low-overhead remains the only contract adapters must meet.

## Milestones

1. `.obu` / `--format obu` output on `expand` (+ `.av1b` if one line
   costs it) — wires the existing writers to the CLI.
2. `format` field on the FFI expand params — same choice at the C layer.
3. Doc-note in `integration-api.md`: `ExpandedStream.packets` defined as
   "§5 low-overhead, one TU per packet, TD-permitted leading" — the
   sentence adapters code against.

Each is additive; nothing about IVF/MP4 output or the input contract
changes.
