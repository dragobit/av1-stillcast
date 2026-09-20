'use strict';
/* WebCodecs AV1 → stillcast input adapter.
 *
 * Encodes N identical frames of a still image with VideoEncoder(av01.*),
 * then picks the two temporal units `stillcast expand` needs positionally:
 *   IVF packet 0 = anchor TU (sequence header OBU + shown KEY_FRAME)
 *   IVF packet 1 = golden TU (the first TU coded after the anchor — must be
 *                  a shown non-key frame; skipping intervening hidden frames
 *                  would let it reference decoder state the anchor lacks)
 * The remaining chunks follow in encoder order so `stillcast info --verbose`
 * shows the whole acquired stream.
 */
const $ = id => document.getElementById(id);
const logEl = $('log');
const log = (...a) => { logEl.textContent += a.join(' ') + '\n'; };

/* ---------- minimal AV1 OBU scan ----------
 * Only what's needed to classify a TU: walk OBU headers, and inside
 * FRAME/FRAME_HEADER payloads read show_existing_frame(1) then
 * frame_type(2) + show_frame(1) — MSB-first, per spec.
 */
const OBU_NAMES = {
  0: 'RESERVED', 1: 'SEQ_HDR', 2: 'TD', 3: 'FRAME_HDR', 4: 'TG',
  5: 'META', 6: 'FRAME', 7: 'REDUNDANT_FH', 8: 'TILE_LIST', 15: 'PAD',
};
const FRAME_TYPES = ['KEY', 'INTER', 'INTRA_ONLY', 'SWITCH'];

function leb128(u8, i) {
  let v = 0, n = 0;
  for (; n < 8 && i + n < u8.length; n++) {
    const b = u8[i + n];
    v += (b & 0x7f) * Math.pow(2, 7 * n);
    if (!(b & 0x80)) return [v, n + 1];
  }
  return [v, Math.max(n, 1)];
}

function parseObus(u8) {
  const out = [];
  let i = 0;
  while (i < u8.length) {
    const hdr = u8[i]; i++;
    const type = (hdr >> 3) & 0x0f;
    if (hdr & 0x04) i++; // extension byte
    let size;
    if (hdr & 0x02) { const [v, n] = leb128(u8, i); size = v; i += n; }
    else size = u8.length - i; // only the last OBU may omit the size
    out.push({ type, typeName: OBU_NAMES[type] || String(type), payloadStart: i, size });
    i += size;
  }
  return out;
}

// Read the leading bits of one coded-frame OBU payload (MSB-first).
function classifyFramePayload(u8, off) {
  let bit = 0;
  const f = n => {
    let v = 0;
    for (let k = 0; k < n; k++) {
      const idx = off + ((bit + k) >> 3);
      v = (v << 1) | ((idx < u8.length ? u8[idx] : 0) >> (7 - ((bit + k) & 7)) & 1);
    }
    bit += n;
    return v;
  };
  if (f(1)) return { kind: 'SHOW_EXISTING', slot: f(3) };
  const frameType = f(2);
  const showFrame = !!f(1);
  return { kind: 'CODED', frameType, frameTypeName: FRAME_TYPES[frameType], showFrame };
}

function classifyTu(u8) {
  const obus = parseObus(u8);
  const r = { hasSeqHdr: false, obuTypes: [], frames: [] };
  for (const o of obus) {
    r.obuTypes.push(o.typeName);
    if (o.type === 1) r.hasSeqHdr = true;
    if (o.type === 3 || o.type === 6)
      r.frames.push(classifyFramePayload(u8, o.payloadStart));
  }
  return r;
}

const tuIsAnchor = c => c.scan.hasSeqHdr &&
  c.scan.frames.some(f => f.kind === 'CODED' && f.frameType === 0 && f.showFrame);
const tuIsGolden = c =>
  c.scan.frames.some(f => f.kind === 'CODED' && f.frameType !== 0 && f.showFrame);

/* ---------- IVF writer ---------- */
function buildIvf(width, height, fps, packets /* [{data:u8, tsUs}] */) {
  const head = new DataView(new ArrayBuffer(32));
  head.setUint8(0, 0x44); head.setUint8(1, 0x4b); head.setUint8(2, 0x49); head.setUint8(3, 0x46); // DKIF
  head.setUint16(4, 0, true); head.setUint16(6, 32, true);
  head.setUint8(8, 0x41); head.setUint8(9, 0x56); head.setUint8(10, 0x30); head.setUint8(11, 0x31); // AV01
  head.setUint16(12, width, true); head.setUint16(14, height, true);
  head.setUint32(16, fps, true); head.setUint32(20, 1, true);
  head.setUint32(24, packets.length, true); head.setUint32(28, 0, true);
  const parts = [new Uint8Array(head.buffer)];
  let total = 32;
  packets.forEach((p, i) => {
    const ph = new DataView(new ArrayBuffer(12));
    ph.setUint32(0, p.data.byteLength, true);
    // Timestamp in timebase units, derived from the chunk's own timestamp so
    // reordered packets keep their provenance.
    ph.setBigUint64(4, BigInt(Math.round(p.tsUs * fps / 1e6)), true);
    parts.push(new Uint8Array(ph.buffer), p.data);
    total += 12 + p.data.byteLength;
  });
  const out = new Uint8Array(total);
  let o = 0;
  for (const p of parts) { out.set(p, o); o += p.byteLength; }
  return out;
}

/* ---------- image handling ---------- */
function drawSynthetic(canvas) {
  const ctx = canvas.getContext('2d');
  const g = ctx.createLinearGradient(0, 0, canvas.width, canvas.height);
  g.addColorStop(0, '#1d3557'); g.addColorStop(1, '#e63946');
  ctx.fillStyle = g; ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = '#f1faee';
  ctx.font = `bold ${canvas.height / 8}px system-ui`;
  ctx.textAlign = 'center';
  ctx.fillText('STILLCAST', canvas.width / 2, canvas.height / 2);
  ctx.font = `${canvas.height / 20}px system-ui`;
  ctx.fillText('WebCodecs → AV1 → IVF', canvas.width / 2, canvas.height / 2 + canvas.height / 8);
  for (let i = 0; i < 24; i++) {
    ctx.fillStyle = `hsl(${i * 15} 70% 55%)`;
    ctx.fillRect(i * canvas.width / 24, canvas.height - 24, canvas.width / 48, 24);
  }
}

async function loadFileIntoCanvas(file, canvas) {
  const bmp = await createImageBitmap(file);
  const scale = Math.min(1, 1280 / bmp.width, 720 / bmp.height);
  canvas.width = Math.max(2, Math.floor(bmp.width * scale / 2) * 2);
  canvas.height = Math.max(2, Math.floor(bmp.height * scale / 2) * 2);
  canvas.getContext('2d').drawImage(bmp, 0, 0, canvas.width, canvas.height);
  bmp.close();
}

/* ---------- encode ---------- */
function readConfig() {
  const q = $('quantizer').value;
  return {
    codec: $('codec').value.trim(),
    latencyMode: $('latencyMode').value,
    hardwareAcceleration: $('hw').value,
    bitrateMode: $('bitrateMode').value,
    bitrate: Number($('bitrate').value),
    quantizer: q === '' ? null : Number(q),
    fps: Number($('fps').value),
    nFrames: Number($('frames').value),
  };
}

function encoderConfig(o, width, height) {
  const cfg = {
    codec: o.codec, width, height,
    bitrate: o.bitrate,
    framerate: o.fps,
    latencyMode: o.latencyMode,
    hardwareAcceleration: o.hardwareAcceleration,
    bitrateMode: o.bitrateMode,
  };
  if (o.quantizer !== null) cfg.av1 = { quantizer: o.quantizer };
  return cfg;
}

async function encode(canvas, o) {
  const cfg = encoderConfig(o, canvas.width, canvas.height);
  const support = await VideoEncoder.isConfigSupported(cfg);
  if (!support.supported) {
    throw new Error('isConfigSupported=false for ' + JSON.stringify(cfg));
  }
  const chunks = [], metas = [];
  let encErr = null;
  const enc = new VideoEncoder({
    output: (c, m) => {
      const d = new Uint8Array(c.byteLength);
      c.copyTo(d);
      chunks.push({ data: d, tsUs: c.timestamp, durUs: c.duration ?? 0, type: c.type });
      metas.push(m ? JSON.parse(JSON.stringify(m)) : null);
    },
    error: e => { encErr = e; },
  });
  enc.configure(cfg);
  const step = 1e6 / o.fps;
  for (let i = 0; i < o.nFrames; i++) {
    const vf = new VideoFrame(canvas, {
      timestamp: Math.round(i * step),
      duration: Math.round(step),
    });
    enc.encode(vf, { keyFrame: i === 0 });
    vf.close();
  }
  await enc.flush().catch(() => {});
  if (encErr) throw encErr;
  enc.close();
  return { cfg, chunks, metas };
}

function analyze({ cfg, chunks, metas }, o) {
  const stepUs = 1e6 / o.fps;
  for (const c of chunks) c.scan = classifyTu(c.data);

  // Frame-drop detector: a submitted frame's timestamp is preserved in its
  // chunk; a missing index means the encoder dropped it.
  const got = new Map(chunks.map(c => [c.tsUs, c]));
  const missing = [];
  for (let i = 0; i < o.nFrames; i++) {
    const t = Math.round(i * stepUs);
    if (!got.has(t)) missing.push(i);
  }

  // Positional contract: packet 0 = anchor, packet 1 = golden. The golden
  // must be decodable directly after the anchor, so it must be the FIRST TU
  // after it containing a coded frame — TUs without coded frames (TD-only,
  // metadata, show_existing) don't touch the DPB and may sit in between.
  // Skipping past a hidden or key coded frame would risk picking a golden
  // that references state the anchor never produced.
  const anchor = chunks.findIndex(tuIsAnchor);
  let golden = -1;
  for (let i = anchor + 1; anchor >= 0 && i < chunks.length; i++) {
    if (chunks[i].scan.frames.some(f => f.kind === 'CODED')) {
      golden = tuIsGolden(chunks[i]) ? i : -1;
      break;
    }
  }

  const order = [];
  if (anchor >= 0) order.push(anchor);
  if (golden >= 0) order.push(golden);
  for (let i = 0; i < chunks.length; i++) if (!order.includes(i)) order.push(i);

  const dcMeta = metas.find(m => m && m.decoderConfig);
  return { anchor, golden, order, missing, dcMeta };
}

function frameSummary(scan) {
  return scan.frames.map(f =>
    f.kind === 'SHOW_EXISTING' ? `SHOW_EXISTING(slot=${f.slot})`
      : `${f.frameTypeName}${f.showFrame ? '' : ' hidden'}`).join('+') || '—';
}

function renderTable(chunks, picks) {
  const tb = document.querySelector('#chunks tbody');
  tb.innerHTML = '';
  chunks.forEach((c, i) => {
    const tr = tb.insertRow();
    const role = i === picks.anchor ? 'anchor' : i === picks.golden ? 'golden' : '';
    if (role) tr.className = role;
    const cells = [i, c.tsUs, c.type, c.data.byteLength,
      c.scan.obuTypes.join('+'), frameSummary(c.scan), role,
      c.meta && c.meta.decoderConfig ? 'decoderConfig' : (c.meta ? 'meta' : '')];
    cells.forEach(v => { const td = tr.insertCell(); td.textContent = v; });
  });
  $('chunks').hidden = false;
}

let lastBlobUrl = null;
function resetDownload() {
  const a = $('download');
  if (lastBlobUrl) { URL.revokeObjectURL(lastBlobUrl); lastBlobUrl = null; }
  a.removeAttribute('href');
  a.textContent = '';
}

async function run(opts) {
  const o = Object.assign(readConfig(), opts || {});
  const canvas = $('src');
  resetDownload();
  log(`encode: ${o.nFrames}f ${canvas.width}x${canvas.height}@${o.fps} ` +
      `${o.codec} latency=${o.latencyMode} hw=${o.hardwareAcceleration} ` +
      `bitrateMode=${o.bitrateMode} bitrate=${o.bitrate}` +
      (o.quantizer !== null ? ` q=${o.quantizer}` : ''));

  const { cfg, chunks, metas } = await encode(canvas, o);
  chunks.forEach((c, i) => c.meta = metas[i]);
  const picks = analyze({ cfg, chunks, metas }, o);

  log(`received ${chunks.length}/${o.nFrames} chunks`);
  if (picks.missing.length)
    log(`DROPPED frames (timestamp gaps): ${picks.missing.join(',')}`);
  if (picks.dcMeta) {
    const dc = picks.dcMeta.decoderConfig;
    log(`decoderConfig: codec=${dc.codec} hw=${dc.hardwareAcceleration || '?'} ` +
        `description=${dc.description ? dc.description.byteLength + 'B PRESENT' : 'absent'}`);
  }
  log(`anchor=chunk ${picks.anchor}  golden=chunk ${picks.golden}`);

  renderTable(chunks, picks);

  if (picks.anchor < 0 || picks.golden < 0) {
    resetDownload();
    $('summary').innerHTML = '<b class="bad">contract unsatisfied — no IVF written</b>';
    return { ok: false, error: 'no anchor or golden TU', chunks: chunkReport(chunks, picks) };
  }
  const ivf = buildIvf(canvas.width, canvas.height, o.fps,
    picks.order.map(i => chunks[i]));
  $('summary').innerHTML =
    `<b class="ok">IVF: ${ivf.byteLength} B, ${chunks.length} packets ` +
    `(order ${picks.order.join(',')})</b>`;
  const a = $('download');
  lastBlobUrl = URL.createObjectURL(new Blob([ivf], { type: 'application/octet-stream' }));
  a.href = lastBlobUrl;
  a.download = 'webcodecs-src.ivf';
  a.textContent = `⇩ webcodecs-src.ivf (${ivf.byteLength} B)`;

  let bin = '';
  for (let i = 0; i < ivf.length; i += 8192)
    bin += String.fromCharCode.apply(null, ivf.subarray(i, i + 8192));
  return {
    ok: true, ivfB64: btoa(bin), config: cfg,
    decoderConfig: picks.dcMeta ? picks.dcMeta.decoderConfig : null,
    missingFrames: picks.missing,
    picks: { anchor: picks.anchor, golden: picks.golden, order: picks.order },
    chunks: chunkReport(chunks, picks),
  };
}

function chunkReport(chunks, picks) {
  return chunks.map((c, i) => ({
    i, tsUs: c.tsUs, type: c.type, bytes: c.data.byteLength,
    obus: c.scan.obuTypes, frames: frameSummary(c.scan),
    role: i === picks.anchor ? 'anchor' : i === picks.golden ? 'golden' : '',
    headHex: Array.from(c.data.slice(0, 16)).map(b => b.toString(16).padStart(2, '0')).join(' '),
  }));
}

/* ---------- UI ---------- */
$('gen').onclick = () => drawSynthetic($('src'));
$('file').onchange = e => e.target.files[0] && loadFileIntoCanvas(e.target.files[0], $('src'));
$('probe').onclick = async () => {
  const o = readConfig(), c = $('src');
  const cfg = encoderConfig(o, c.width, c.height);
  try {
    const s = await VideoEncoder.isConfigSupported(cfg);
    log(`isConfigSupported ${o.codec} ${c.width}x${c.height} latency=${o.latencyMode} ` +
        `hw=${o.hardwareAcceleration} bitrateMode=${o.bitrateMode}: ${s.supported}`);
  } catch (e) { log('isConfigSupported threw: ' + e.message); }
};
$('go').onclick = () => run().catch(e => log('FAILED: ' + e.message));

drawSynthetic($('src'));
window.stillcastRun = run; // automation entry point (returns {ok, ivfB64, ...})
