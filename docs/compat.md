# Compatibility matrix

Test asset: [`examples/demo.mp4`](../examples/demo.mp4) (30 s, AV1+AAC,
>99% `show_existing_frame` TUs). Rebuild variants with
`examples/make-demo.sh` or `stillcast make`.

Per cell, check: **play** (full playthrough), **mid-seek** (seek into a
non-keyframe position — worst case), **sync** (A/V stays aligned after
seek). ✅ verified, ⬜ untested, ⚠️ partial/known issue.

| decoder / player | play | mid-seek | sync | notes |
|---|---|---|---|---|
| libdav1d (sw decoder) | ✅ | ✅ | ✅ | e2e-verified, frame-exact |
| ffmpeg demux+seek path | ✅ | ✅ | ✅ | `-ss` lands on prior stss keyframe; `-copyts` output starts at exact target pts |
| ffmpeg `av1` native decoder | ⬜ | ⬜ | ⬜ | untested (fails in our env on all av1 files) |
| libgav1 | ⬜ | ⬜ | ⬜ | |
| ffplay | ⬜ | ⬜ | ⬜ | |
| VLC | ⬜ | ⬜ | ⬜ | |
| mpv | ⬜ | ⬜ | ⬜ | |
| Windows Media Foundation (AV1 ext.) | ⬜ | ⬜ | ⬜ | |
| Chrome 137 (dav1d/hw) | ✅ | ✅ | ✅ | `scripts/browser_seek_test.py`: mid-GOP seeks complete in 4–13 ms, `seeked` fires, `currentTime` preserved (rewind-to-keyframe + fast-forward through cheap TUs), playthrough OK |
| Firefox | ⬜ | ⬜ | ⬜ | |
| Safari (Apple silicon AV1 hw) | ⬜ | ⬜ | ⬜ | hw only, M3+/A17+ |
| Android MediaCodec AV1 | ⬜ | ⬜ | ⬜ | |
| YouTube ingest | ⬜ | — | — | upload unlisted; does it accept av01 mp4? |
| HTTP range seek (progressive) | ✅ | ✅ | — | 45 MB / 1 h file over plain HTTP: Chrome computed byte offsets from the front-loaded moov tables and issued mid-file `Range` requests only (starts at 34.9 MB for t=2700 s, 41.6 MB for t=3300 s); `seeked` in ~220–250 ms. Verified on a 206-capable local server. |

How to report: edit this table in a PR with device/browser versions and
observed behavior (warnings, stalls, fallback to sw decode). Browser
cells are best filled with `scripts/browser_seek_test.py` (Playwright
CDP) so mid-GOP seek behavior is measured, not eyeballed.

Known-spec risks this matrix measures:
- Streams that are >97% show_existing are unusual in the wild.
- Seek latency up to one GOP of cheap TUs on players that decode
  sequentially rather than landing on the keyframe.
