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
| ffmpeg `av1` native decoder | ⬜ | ⬜ | ⬜ | untested (fails in our env on all av1 files) |
| libgav1 | ⬜ | ⬜ | ⬜ | |
| ffplay | ⬜ | ⬜ | ⬜ | |
| VLC | ⬜ | ⬜ | ⬜ | |
| mpv | ⬜ | ⬜ | ⬜ | |
| Windows Media Foundation (AV1 ext.) | ⬜ | ⬜ | ⬜ | |
| Chrome (dav1d/hw) | ⬜ | ⬜ | ⬜ | file:// or MSE |
| Firefox | ⬜ | ⬜ | ⬜ | |
| Safari (Apple silicon AV1 hw) | ⬜ | ⬜ | ⬜ | hw only, M3+/A17+ |
| Android MediaCodec AV1 | ⬜ | ⬜ | ⬜ | |
| YouTube ingest | ⬜ | — | — | upload unlisted; does it accept av01 mp4? |

How to report: edit this table in a PR with device/browser versions and
observed behavior (warnings, stalls, fallback to sw decode).

Known-spec risks this matrix measures:
- Streams that are >97% show_existing are unusual in the wild.
- Seek latency up to one GOP of cheap TUs on players that decode
  sequentially rather than landing on the keyframe.
