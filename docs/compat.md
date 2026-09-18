# Compatibility matrix

Test asset: [`examples/demo.mp4`](../examples/demo.mp4) (30 s, AV1+AAC,
>99% `show_existing_frame` TUs). Rebuild variants with
`examples/make-demo.sh` or `stillcast make`.

Per cell, check: **play** (full playthrough), **mid-seek** (seek into a
non-keyframe position — worst case), **sync** (A/V stays aligned after
seek). ✅ verified, ⬜ untested, ⚠️ partial/known issue.

| decoder / player | play | mid-seek | sync | notes |
|---|---|---|---|---|
| aomdec (AV1 reference decoder) | ✅ | — | — | aom git tip, decode-only build: 913/913 frames on `demo.mp4`'s stream, 360/360 on a 2-image playlist stream incl. `--decoder-model` variant; zero errors |
| libdav1d (sw decoder) | ✅ | ✅ | ✅ | e2e-verified, frame-exact |
| ffmpeg demux+seek path | ✅ | ✅ | ✅ | `-ss` lands on prior stss keyframe; `-copyts` output starts at exact target pts |
| ffmpeg `av1` native decoder | ⬜ | ⬜ | ⬜ | untested (fails in our env on all av1 files) |
| ffplay | ✅ | ✅ | — | `scripts/compat_players.sh`: `-ss 5 -t 3` clean |
| VLC | ✅ | ✅ | — | 3.0.16: `--start-time=5 --stop-time=8` clean |
| mpv | ✅ | ✅ | — | 0.34: `--start=5 --length=3`, libdav1d decode |
| Windows Media Foundation (AV1 ext.) | ⬜ | ⬜ | ⬜ | |
| Chrome 137 (dav1d/hw) | ✅ | ✅ | ✅ | `scripts/browser_seek_test.py`: mid-GOP seeks complete in 4–13 ms, `seeked` fires, `currentTime` preserved (rewind-to-keyframe + fast-forward through cheap TUs), playthrough OK |
| Firefox 155 | ✅ | ✅ | ✅ | `BROWSER=firefox scripts/browser_seek_test.py`: seeks 4–31 ms, playthrough OK (Playwright headless) |
| Safari (Apple silicon AV1 hw) | ⬜ | ⬜ | ⬜ | hw only, M3+/A17+ |
| Android MediaCodec AV1 | ✅ | ⬜ | — | Android 14 emulator (api 34, google_apis): `dumpsys media.player` shows `mime(video/av01)` → `c2.android.av1.decoder` (libgav1) actively rendering — 359 frames, ~2% dropped; NuPlayer state playing |
| libgav1 | ✅ | — | — | = `c2.android.av1.decoder`, verified via Android emulator above |
| YouTube ingest | ⬜ | — | — | upload unlisted; does it accept av01 mp4? |
| HTTP range seek (progressive) | ✅ | ✅ | — | 45 MB / 1 h file over plain HTTP: Chrome computed byte offsets from the front-loaded moov tables and issued mid-file `Range` requests only (starts at 34.9 MB for t=2700 s, 41.6 MB for t=3300 s); `seeked` in ~220–250 ms. Verified on a 206-capable local server. |

How to report: edit this table in a PR with device/browser versions and
observed behavior (warnings, stalls, fallback to sw decode). Cells marked
"verified" come from automated checks:

- `scripts/compat_players.sh` — ffplay/mpv/VLC smoke checks (skip if not
  installed)
- `scripts/browser_seek_test.py` — mid-GOP seek assertions in a real
  browser (`BROWSER=chrome` attaches to a running Chrome over CDP;
  `BROWSER=firefox`/`chromium` launch a Playwright-managed headless
  browser)
- `.github/workflows/compat.yml` — runs the above plus an Android
  emulator job (MediaCodec `c2.android.av1.decoder` renders frames) on
  demand / weekly

Known-spec risks this matrix measures:
- Streams that are >97% show_existing are unusual in the wild.
- Seek latency up to one GOP of cheap TUs on players that decode
  sequentially rather than landing on the keyframe.
