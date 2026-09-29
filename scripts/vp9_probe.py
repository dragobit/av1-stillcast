#!/usr/bin/env python3
"""Probe WebCodecs VP9 encode support in the sandbox Chrome via CDP.

Mirrors the measured-findings approach of examples/webcodecs/README.md,
but for vp09: isConfigSupported across a config matrix, then a real
encode of identical stills to inspect chunk structure (keyframe in-band,
inter frames, decoderConfig.description presence).
"""
import json
import os

from playwright.sync_api import sync_playwright

PAGE = "http://localhost:8778/examples/webcodecs/index.html"

JS = r"""
async () => {
  const out = { support: [], encode: null, errors: [] };
  const codecs = [
    'vp09.00.10.08', 'vp09.00.31.08', 'vp09.00.40.08', 'vp09.00.50.08',
    'vp09.01.40.08', 'vp09.02.40.10',
  ];
  const modes = ['quality', 'realtime'];
  const hws = ['no-preference', 'prefer-software', 'prefer-hardware'];
  for (const codec of codecs) {
    for (const latencyMode of modes) {
      for (const hw of hws) {
        const cfg = { codec, width: 640, height: 360, framerate: 30,
                      latencyMode, hardwareAcceleration: hw,
                      bitrate: 300_000, bitrateMode: 'constant' };
        try {
          const r = await VideoEncoder.isConfigSupported(cfg);
          out.support.push({ codec, latencyMode, hw, supported: !!r.supported });
        } catch (e) {
          out.support.push({ codec, latencyMode, hw, supported: 'err:' + e.name });
        }
      }
    }
  }
  // quantizer mode?
  try {
    const r = await VideoEncoder.isConfigSupported({
      codec: 'vp09.00.40.08', width: 640, height: 360, framerate: 30,
      bitrateMode: 'quantizer', vp9: { quantizer: 32 } });
    out.support.push({ codec: 'vp09.00.40.08', mode: 'quantizer q32', supported: !!r.supported });
  } catch (e) { out.errors.push('quantizer cfg: ' + e.message); }

  // Real encode: 30 identical stills
  const cvs = document.createElement('canvas');
  cvs.width = 640; cvs.height = 360;
  const g = cvs.getContext('2d');
  g.fillStyle = '#203040'; g.fillRect(0, 0, 640, 360);
  g.fillStyle = '#ffcc00'; g.font = '48px sans-serif';
  g.fillText('vp9 still', 200, 180);
  const chunks = [];
  let meta0 = null;
  const enc = new VideoEncoder({
    output: (c, m) => { chunks.push(c); if (!meta0) meta0 = m; },
    error: e => out.errors.push('encoder: ' + e.message),
  });
  enc.configure({ codec: 'vp09.00.40.08', width: 640, height: 360,
                  framerate: 30, bitrate: 300_000, bitrateMode: 'constant',
                  latencyMode: 'quality', hardwareAcceleration: 'prefer-software' });
  for (let i = 0; i < 30; i++) {
    const f = new VideoFrame(cvs, { timestamp: i * 1e6 / 30, duration: 1e6 / 30 });
    enc.encode(f, { keyFrame: i === 0 });
    f.close();
  }
  await enc.flush();
  enc.close();
  const desc = meta0 && meta0.decoderConfig ? meta0.decoderConfig.description : undefined;
  out.encode = {
    chunks: chunks.length,
    types: chunks.map(c => c.type),
    sizes: chunks.map(c => c.byteLength),
    ts0: chunks.length ? chunks[0].timestamp : null,
    hasDescription: desc !== undefined && desc !== null,
    decoderConfigCodec: meta0 && meta0.decoderConfig ? meta0.decoderConfig.codec : null,
  };
  // Peek at frame 0 uncompressed header bits: show_existing_frame(1), frame_type(1), show_frame(1)
  if (chunks.length) {
    const b = new Uint8Array(chunks[0].byteLength);
    chunks[0].copyTo(b);
    const byte0 = b[0];
    out.hdr0 = {
      frameMarker: (byte0 >> 6) & 3,
      profile: ((byte0 >> 4) & 1) | ((byte0 & 0x10) >> 3),
      showExisting: (byte0 >> 3) & 1,
      frameType: (byte0 >> 2) & 1,
      showFrame: (byte0 >> 1) & 1,
      errorResilient: byte0 & 1,
    };
    const b1 = new Uint8Array(chunks[1].byteLength);
    chunks[1].copyTo(b1);
    const c0 = b1[0];
    out.hdr1 = {
      showExisting: (c0 >> 3) & 1, frameType: (c0 >> 2) & 1,
      showFrame: (c0 >> 1) & 1, errorResilient: c0 & 1,
    };
  }
  return out;
}
"""

def main():
    with sync_playwright() as p:
        browser_name = os.environ.get("BROWSER", "chrome")
        if browser_name == "chrome":
            browser = p.chromium.connect_over_cdp(
                os.environ.get("CDP_URL", "http://localhost:29229"))
        elif browser_name in ("firefox", "chromium"):
            browser = getattr(p, browser_name).launch(headless=True)
        else:
            raise SystemExit(f"unknown BROWSER={browser_name}")
        ctx = browser.contexts[0] if browser.contexts else browser.new_context()
        page = ctx.new_page()
        page.goto(PAGE)
        res = page.evaluate(JS)
        print(json.dumps(res, indent=2))

if __name__ == "__main__":
    main()
