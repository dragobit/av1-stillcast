//! Regression tests for the stable API (`api` module) and the C ABI shim.
//! Fixture: tests/fixtures/src.ivf — libaom 2-frame encode of a 640x640
//! still at 30fps (produced by `stillcast encode -i examples/jacket.jpg`).

use stillcast::api::{expand_ivf, expand_ivf_multi, ExpandParams, SegmentInput};
use stillcast::ivf;
use stillcast::obu::{Obu, ObuType};

const SRC: &[u8] = include_bytes!("fixtures/src.ivf");

fn src_tus() -> Vec<Vec<u8>> {
    ivf::read(SRC)
        .unwrap()
        .frames
        .into_iter()
        .map(|(_, tu)| tu)
        .collect()
}

/// A temporal unit with no coded frame (TD + metadata + padding).
fn junk_tu() -> Vec<u8> {
    let mut tu = Vec::new();
    Obu::temporal_delimiter().write(&mut tu);
    Obu {
        obu_type: ObuType::Metadata,
        extension: None,
        payload: vec![0, 0, 0xaa],
    }
    .write(&mut tu);
    Obu {
        obu_type: ObuType::Padding,
        extension: None,
        payload: vec![0; 4],
    }
    .write(&mut tu);
    tu
}

fn ivf_bytes(tus: &[Vec<u8>]) -> Vec<u8> {
    ivf::write(&ivf::IvfFile {
        width: 640,
        height: 640,
        timebase_den: 30,
        timebase_num: 1,
        frames: tus
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, t)| (i as u64, t))
            .collect(),
    })
}

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
fn expand_accepts_obu_and_annexb_inputs() {
    // The 2-frame input contract is container-agnostic: the same encode as
    // a raw OBU stream and as Annex-B must expand identically to the IVF.
    let packets: Vec<Vec<u8>> = ivf::read(SRC)
        .unwrap()
        .frames
        .into_iter()
        .map(|(_, tu)| tu)
        .collect();
    let params = ExpandParams {
        fps: Some(30),
        total_frames: 60,
        gop_size: 30,
        decoder_model: false,
    };
    let expected = expand_ivf(SRC, &params).unwrap();
    for bytes in [
        stillcast::container::write_obu_stream(&packets),
        stillcast::container::write_annexb(&packets),
    ] {
        let out = expand_ivf(&bytes, &params).unwrap();
        assert_eq!(out, expected);
    }
}

#[test]
fn expand_scans_past_leading_junk_tus() {
    // Condition-based acceptance: a leading frameless TU (metadata/padding)
    // doesn't displace the anchor+golden pair. For Annex-B the junk
    // TU is skipped entirely, so output stays byte-identical; the IVF and
    // OBU-stream demuxers merge leading frameless OBUs into the first
    // coded TU, so there it only has to succeed.
    let mut tus = vec![junk_tu()];
    tus.extend(src_tus());
    let params = ExpandParams {
        fps: Some(30),
        total_frames: 60,
        gop_size: 30,
        decoder_model: false,
    };
    let expected = expand_ivf(SRC, &params).unwrap();
    let out = expand_ivf(&stillcast::container::write_annexb(&tus), &params).unwrap();
    assert_eq!(out, expected);
    for bytes in [
        ivf_bytes(&tus),
        stillcast::container::write_obu_stream(&tus),
    ] {
        let out = expand_ivf(&bytes, &params).unwrap();
        assert_eq!(ivf::read(&out).unwrap().frames.len(), 60);
    }
}

#[test]
fn expand_multi_scans_each_segment_source() {
    // Playlist path: segment 2's input carries the pair at non-zero offsets
    // (junk TU in front, trailing coded frames behind). Both sources get the
    // same scan.
    let mut offset = vec![junk_tu()];
    offset.extend(src_tus());
    let offset = ivf_bytes(&offset);
    let out = expand_ivf_multi(
        &[
            SegmentInput {
                ivf: SRC,
                frames: 2,
            },
            SegmentInput {
                ivf: &offset,
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
fn expand_rejects_all_keyframe_input() {
    // -g 1 style: seq+KF on every TU — nearest-miss diagnostics must name
    // the forced keyframes.
    let src = src_tus();
    let mut tus = Vec::new();
    for _ in 0..3 {
        tus.push(src[0].clone());
    }
    let err = expand_ivf(
        &ivf_bytes(&tus),
        &ExpandParams {
            fps: None,
            total_frames: 10,
            gop_size: 10,
            decoder_model: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("KEY_FRAME"), "{msg}");
}

#[test]
fn expand_treats_zero_timebase_as_unset() {
    // Muxers that don't know the rate write timebase 0/0; it must fall
    // back to 30/1 and the output IVF must never carry a zero-rate pair.
    let mut src = ivf::read(SRC).unwrap();
    src.timebase_den = 0;
    src.timebase_num = 0;
    let input = ivf::write(&src);
    let out = expand_ivf(
        &input,
        &ExpandParams {
            fps: None,
            total_frames: 10,
            gop_size: 10,
            decoder_model: false,
        },
    )
    .unwrap();
    let out_ivf = ivf::read(&out).unwrap();
    assert_eq!(out_ivf.rate(), (30, 1));
    assert_eq!((out_ivf.timebase_den, out_ivf.timebase_num), (30, 1));

    // An OBU stream built from the same TUs goes through
    // container::finish's path and must land on the same default when the
    // sequence header declares no usable timing.
    let packets: Vec<Vec<u8>> = ivf::read(SRC)
        .unwrap()
        .frames
        .into_iter()
        .map(|(_, tu)| tu)
        .collect();
    let obu_stream = stillcast::container::write_obu_stream(&packets);
    let out = expand_ivf(
        &obu_stream,
        &ExpandParams {
            fps: None,
            total_frames: 10,
            gop_size: 10,
            decoder_model: false,
        },
    )
    .unwrap();
    assert_eq!(ivf::read(&out).unwrap().rate(), (30, 1));
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
