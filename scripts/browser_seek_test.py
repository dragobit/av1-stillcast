#!/usr/bin/env python3
"""Browser seek/playback compat test for stillcast outputs.

Opens an mp4 in a real browser, seeks to mid-GOP positions, and asserts the
HTMLMediaElement contract:

  - 'seeked' fires within a timeout (no decoder stall on our weird stream)
  - currentTime lands at the requested time (browser rewound to the
    previous sync sample and fast-forwarded through ~6B TUs)
  - readyState >= HAVE_FUTURE_DATA, no MediaError

Usage:
  BROWSER=chrome  CDP_URL=http://localhost:29229  (attach to a running Chrome
                                                  via --remote-debugging-port)
  BROWSER=firefox                                 (playwright-managed, headless)
  BROWSER=chromium                                (playwright-managed, headless)
  python3 scripts/browser_seek_test.py [file.mp4]

Requires: pip install playwright; playwright install firefox / chromium for
managed launches, or a Chrome with --remote-debugging-port for CDP attach.
"""
import json
import os
import sys

from playwright.sync_api import sync_playwright

SEEK_TIMEOUT_MS = 10_000


def main() -> int:
    mp4 = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.abspath(
        os.path.join(os.path.dirname(__file__), "..", "examples", "demo.mp4")
    )
    browser_name = os.environ.get("BROWSER", "chrome")

    with sync_playwright() as p:
        if browser_name == "chrome":
            cdp = os.environ.get("CDP_URL", "http://localhost:29229")
            browser = p.chromium.connect_over_cdp(cdp)
        elif browser_name in ("firefox", "chromium"):
            launcher = getattr(p, browser_name)
            browser = launcher.launch(headless=True)
        else:
            print(f"FAIL: unknown BROWSER={browser_name} (chrome|firefox|chromium)")
            return 1
        ctx = browser.contexts[0] if browser.contexts else browser.new_context()
        page = ctx.new_page()
        page.goto(f"file://{mp4}")
        page.wait_for_function("document.querySelector('video') !== null")
        page.wait_for_function("document.querySelector('video').readyState >= 2")
        page.evaluate("document.querySelector('video').pause()")

        meta = page.evaluate(
            """() => { const v = document.querySelector('video'); return {
                dur: v.duration, w: v.videoWidth, h: v.videoHeight }; }"""
        )
        dur = meta["dur"]
        if not dur or dur <= 0:
            print(f"FAIL: no video duration (metadata {meta})")
            return 1
        print(f"opened {mp4}: {dur:.2f}s {meta['w']}x{meta['h']}")

        # Seek targets: midpoints of each quarter — lands mid-GOP for any
        # reasonable gop, i.e. never exactly on a keyframe.
        targets = [dur * f for f in (0.17, 0.38, 0.63, 0.85)]
        ok = True
        for t in targets:
            r = page.evaluate(
                """(t) => new Promise(res => {
                    const v = document.querySelector('video');
                    const t0 = performance.now();
                    const to = setTimeout(
                        () => res({timeout: true, ct: v.currentTime}), %d);
                    v.addEventListener('seeked', () => {
                        clearTimeout(to);
                        res({ms: Math.round(performance.now() - t0),
                             ct: v.currentTime, rs: v.readyState,
                             err: v.error ? v.error.code : null});
                    }, {once: true});
                    v.currentTime = t;
                })""" % SEEK_TIMEOUT_MS,
                t,
            )
            good = (
                not r.get("timeout")
                and r.get("err") is None
                and abs(r["ct"] - t) < 0.05
                and r.get("rs", 0) >= 3
            )
            ok &= good
            print(
                f"{'PASS' if good else 'FAIL'} seek {t:.2f}s -> "
                f"{json.dumps(r)}"
            )

        # Basic playback sanity: unpause briefly and confirm time advances.
        adv = page.evaluate(
            """() => new Promise(res => {
                const v = document.querySelector('video');
                const t0 = v.currentTime;
                v.play();
                setTimeout(() => { v.pause(); res(v.currentTime - t0); }, 1500);
            })"""
        )
        good = adv > 0.5
        ok &= good
        print(f"{'PASS' if good else 'FAIL'} playback advanced {adv:.2f}s in 1.5s")
        page.close()
        print("RESULT:", "PASS" if ok else "FAIL")
        return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
