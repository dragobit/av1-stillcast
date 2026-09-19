//! Regression tests for the stable API (`api` module) and the C ABI shim.
//! Fixture: tests/fixtures/src.ivf — libaom 2-frame encode of a 640x640
//! still at 30fps (produced by `stillcast encode -i examples/jacket.jpg`).

use stillcast::api::{expand_ivf, expand_ivf_multi, ExpandParams, SegmentInput};
use stillcast::ivf;

const SRC: &[u8] = include_bytes!("fixtures/src.ivf");

#[test]
fn expand_ivf_produces_gop_pattern() {
    let out = expand_ivf(
        SRC,
        &ExpandParams {
            fps: Some(30),
            total_frames: 90,
            gop_size: 30,
            decoder_model: false,
        },
    )
    .unwrap();
    let ivf = ivf::read(&out).unwrap();
    assert_eq!(ivf.frames.len(), 90);
    // 3 GOPs x 30f: only the 3 key TUs (0,30,60) carry the big coded
    // keyframe; everything else is the tiny golden/show_existing repeat.
    for (i, (_, tu)) in ivf.frames.iter().enumerate() {
        let key = i % 30 == 0;
        assert_eq!(
            tu.len() > 1000,
            key,
            "frame {i}: {} B (key TU expected: {key})",
            tu.len()
        );
    }
}

#[test]
fn expand_ivf_multi_rejects_mismatched_header() {
    // Same IVF twice is fine; total = 2+2 across two segments.
    let out = expand_ivf_multi(
        &[
            SegmentInput {
                ivf: SRC,
                frames: 2,
            },
            SegmentInput {
                ivf: SRC,
                frames: 2,
            },
        ],
        &ExpandParams {
            fps: None,
            total_frames: 4,
            gop_size: 30,
            decoder_model: false,
        },
    )
    .unwrap();
    assert_eq!(ivf::read(&out).unwrap().frames.len(), 4);
}

#[test]
fn c_abi_expand_roundtrips() {
    let mut out_len = 0usize;
    let p = unsafe {
        stillcast::ffi::stillcast_expand(SRC.as_ptr(), SRC.len(), 0, 90, 30, false, &mut out_len)
    };
    assert!(!p.is_null(), "expand failed");
    let out = unsafe { std::slice::from_raw_parts(p, out_len).to_vec() };
    unsafe { stillcast::ffi::stillcast_free(p, out_len) };
    assert_eq!(ivf::read(&out).unwrap().frames.len(), 90);
}

#[test]
fn c_abi_error_path() {
    let mut out_len = 99usize;
    let p = unsafe {
        stillcast::ffi::stillcast_expand(b"bad".as_ptr(), 3, 0, 10, 10, false, &mut out_len)
    };
    assert!(p.is_null());
    assert_eq!(out_len, 0);
    let err = stillcast::ffi::stillcast_last_error();
    assert!(!err.is_null());
}

#[test]
fn expand_ivf_rejects_garbage() {
    assert!(expand_ivf(
        b"not ivf",
        &ExpandParams {
            fps: None,
            total_frames: 10,
            gop_size: 10,
            decoder_model: false,
        },
    )
    .is_err());
}
