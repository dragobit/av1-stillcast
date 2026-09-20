//! Stable crate-level API: the pure bytes-in → bytes-out transform.
//!
//! This is the contract the C ABI (`ffi` module) and any in-process
//! embedding build on. Everything here works on IVF byte buffers — no
//! filesystem, no ffmpeg, no clap.

use anyhow::{Context, Result};

use crate::assemble::{self, AssembleParams, Segment};
use crate::container;
use crate::ivf;

/// Parameters for [`expand_ivf`] / [`expand_ivf_multi`].
pub struct ExpandParams {
    /// Output frame rate. `None` keeps the input IVF timebase.
    pub fps: Option<u32>,
    /// Total output frame count.
    pub total_frames: u64,
    /// Frames per GOP = distance between keyframes = seek granularity.
    pub gop_size: u64,
    /// Declare decoder_model_info() in the sequence header (rewrites the
    /// real frames' headers; costs ~1 bit/frame).
    pub decoder_model: bool,
}

/// One input segment for [`expand_ivf_multi`]: an encoded IVF (keyframe +
/// golden frames, e.g. produced by `stillcast encode` or a libaom pipeline)
/// plus how many output frames it is shown for.
pub struct SegmentInput<'a> {
    /// Complete input bytes: IVF, low-overhead OBU, or Annex-B (scanned for
    /// an anchor keyframe TU + golden TU pair).
    pub ivf: &'a [u8],
    /// Output frame count for this segment.
    pub frames: u64,
}

/// Expand a short encode into a long static-video AV1 stream.
///
/// `input` is a complete IVF file, low-overhead OBU stream, or Annex-B
/// stream; its leading temporal units are scanned for the anchor TU
/// (sequence header + shown keyframe) and the golden TU (the shown non-key
/// frame coded directly after it — see `assemble::split_input`). Returns a
/// complete IVF file containing the expanded temporal units.
pub fn expand_ivf(input: &[u8], params: &ExpandParams) -> Result<Vec<u8>> {
    expand_ivf_multi(
        &[SegmentInput {
            ivf: input,
            frames: params.total_frames,
        }],
        params,
    )
}

/// Multi-segment variant: each `SegmentInput` becomes a display segment.
/// All inputs must share a byte-identical sequence-header payload (same
/// dimensions/encoder settings); every segment boundary is a shown keyframe.
/// Total output frames = the sum of segment frame counts.
pub fn expand_ivf_multi(segments: &[SegmentInput], params: &ExpandParams) -> Result<Vec<u8>> {
    anyhow::ensure!(!segments.is_empty(), "no input segments");

    let mut donor = None;
    let mut pairs = Vec::with_capacity(segments.len());
    for (i, s) in segments.iter().enumerate() {
        let ivf = container::read(s.ivf)
            .with_context(|| format!("parsing segment {i} input"))?
            .ivf;
        let pair =
            assemble::split_input(&ivf).with_context(|| format!("splitting segment {i} input"))?;
        if donor.is_none() {
            donor = Some(ivf);
        }
        pairs.push(pair);
    }
    let donor = donor.unwrap();
    let segs: Vec<Segment> = pairs
        .iter()
        .zip(segments.iter())
        .map(|((k, g), s)| Segment {
            key_tu: k,
            golden_tu: g,
            frames: s.frames,
        })
        .collect();

    let eff_fps = params.fps.unwrap_or_else(|| {
        donor
            .timebase_den
            .checked_div(donor.timebase_num.max(1))
            .unwrap_or(30)
            .max(1)
    });
    let out = assemble::assemble_multi(
        &segs,
        &AssembleParams {
            fps: eff_fps,
            total_frames: segs.iter().map(|s| s.frames).sum(),
            gop_size: params.gop_size,
            decoder_model: params.decoder_model,
        },
    )?;
    Ok(ivf::write(&assemble::to_ivf(&donor, out.tus, params.fps)))
}
