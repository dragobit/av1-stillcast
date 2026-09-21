//! CLI-level regression tests (assert_cmd). Fixture: tests/fixtures/src.ivf
//! — a libaom 2-frame 640x640 30fps encode.

use assert_cmd::Command;

fn stillcast() -> Command {
    Command::cargo_bin("stillcast").unwrap()
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn make_audio_requires_mp4_output() {
    // `make --audio x.aac -o out.ivf` used to silently drop the audio;
    // it must bail like `expand` does.
    let out = stillcast()
        .args([
            "make",
            "-i",
            "still.png",
            "--audio",
            "track.aac",
            "-o",
            "out.ivf",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--audio requires mp4"), "{stderr}");
}

#[test]
fn expand_rejects_degenerate_frame_counts() {
    // --frames 0 fails in segment_frames ("total 0 frames is shorter than
    // the sum of earlier segments"); --frames 1 reaches the >= 2 check in
    // build_output. Both must fail with a clear error, not emit a stream.
    for frames in ["0", "1"] {
        let out = stillcast()
            .args([
                "expand",
                "-i",
                &fixture("src.ivf"),
                "--frames",
                frames,
                "-o",
                "out.ivf",
            ])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("at least 2 output frames") || stderr.contains("shorter than the sum"),
            "frames={frames}: {stderr}"
        );
    }
}

#[test]
fn expand_rejects_gop_below_two() {
    for gop in ["0", "1"] {
        let out = stillcast()
            .args([
                "expand",
                "-i",
                &fixture("src.ivf"),
                "--frames",
                "10",
                "--gop",
                gop,
                "-o",
                "out.ivf",
            ])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("gop size must be >= 2"),
            "gop={gop}: {stderr}"
        );
    }
}

#[test]
fn expand_rejects_absurd_frame_count() {
    let out = stillcast()
        .args([
            "expand",
            "-i",
            &fixture("src.ivf"),
            "--frames",
            "1000000000000",
            "-o",
            "out.ivf",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("exceeds"), "{stderr}");
}
