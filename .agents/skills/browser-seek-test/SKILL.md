---
name: browser-seek-test
description: How to run scripts/browser_seek_test.py against stillcast outputs in the Devin sandbox (real Chrome via CDP).
---

# Running the browser seek test on Devin machines

`scripts/browser_seek_test.py` loads an mp4 in a real browser and asserts the
HTMLMediaElement seek contract (mid-GOP seeks on show_existing_frame TUs).

## Devin machine specifics

- **Always set `BROWSER=chrome` explicitly.** The sandbox exports
  `BROWSER=/opt/.devin/browser.sh`, which the script rejects with
  `FAIL: unknown BROWSER=...`. Correct invocation:
  `BROWSER=chrome CDP_URL=http://localhost:29229 python3 scripts/browser_seek_test.py <file.mp4>`
- **CDP attach, no managed browser needed.** Chrome for Testing is already
  running with `--remote-debugging-port=29229`; playwright just needs
  `pip3 install playwright` (no `playwright install` browser download).
- **No HTTP server needed** — the script opens `file://<abs path>` directly.
- The new tab opens in the **visible** Chrome window, so seeks are observable
  on screen while a screen recording runs.

## Verifying pass vs. fail

- PASS: prints `opened <file>: <dur>s <w>x<h>`, four `PASS seek` lines, then
  `RESULT: PASS` (exit 0).
- A stream that Chrome cannot decode stalls at
  `wait_for_function(...readyState >= 2)` and raises a playwright
  `TimeoutError` after ~30s (exit 1) — the tab left behind shows a black
  player stuck at `0:00` even though container duration parses.

## Cheap corruption check without a browser

`ffmpeg -v error -c:v libdav1d -i <file.ivf> -f null -` prints nothing on a
clean stream; on a corrupted stream it prints e.g.
`zero_bit out of range` / `Failed to parse temporal unit`.

## Building a pre-fix control binary

`git worktree add /tmp/stillcast-old HEAD~1 && cd /tmp/stillcast-old && cargo build`
gives a standalone parent-commit binary at
`/tmp/stillcast-old/target/debug/stillcast` without touching the main checkout.
