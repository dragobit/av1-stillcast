//! The bitstream assembler: turns a handful of real coded frames into a long
//! static-video AV1 stream via show_existing_frame temporal units.
//!
//! GOP structure we emit:
//!   [ seq hdr + KEY_FRAME TU ] [ golden TU ] [ show_existing TU x (n-2) ]
//! repeated per GOP. The golden is a shown non-key frame (auto showable)
//! stored in a known reference slot; every following TU just re-displays it.

use anyhow::{bail, Context, Result};

use crate::bitio::BitWriter;
use crate::frame_header::{self, KEY_FRAME};
use crate::ivf::IvfFile;
use crate::obu::{parse_obus, Obu, ObuType};
use crate::seq_header::SequenceHeader;

/// Parameters controlling the assembled stream.
pub struct AssembleParams {
    /// Output frame rate (also used for IVF timestamps when input has none).
    pub fps: u32,
    /// Total output frames.
    pub total_frames: u64,
    /// Frames per GOP (distance between keyframes). Controls seek granularity.
    pub gop_size: u64,
}

/// A temporal unit (the OBU payload sequence of one IVF packet).
pub type TemporalUnit = Vec<u8>;

/// Locate the shown frame's header inside a TU and return its FrameHeaderInfo.
fn tu_frame_info(tu: &[u8], sh: &SequenceHeader) -> Result<frame_header::FrameHeaderInfo> {
    for obu in parse_obus(tu)? {
        let payload = match obu.obu_type {
            ObuType::Frame | ObuType::FrameHeader => Some(obu.payload.clone()),
            _ => None,
        };
        if let Some(p) = payload {
            return frame_header::parse_frame_header_info(&p, sh);
        }
    }
    bail!("temporal unit contains no frame OBU")
}

fn tu_has_seq_header(tu: &[u8]) -> Result<bool> {
    Ok(parse_obus(tu)?
        .iter()
        .any(|o| o.obu_type == ObuType::SequenceHeader))
}

/// Build a temporal delimiter + show_existing_frame frame-header TU.
fn show_existing_tu(frame_to_show_map_idx: u8) -> TemporalUnit {
    let mut w = BitWriter::new();
    w.f(1, 1); // show_existing_frame
    w.f(3, u64::from(frame_to_show_map_idx));
    w.trailing_bits();
    let obu = Obu {
        obu_type: ObuType::FrameHeader,
        extension: None,
        payload: w.into_bytes(),
    };
    let mut tu = Vec::with_capacity(8);
    Obu::temporal_delimiter().write(&mut tu);
    obu.write(&mut tu);
    tu
}

/// Pick the lowest slot index refreshed by `refresh_frame_flags`.
fn lowest_refreshed_slot(flags: u8) -> Option<u8> {
    (0..8).find(|i| flags & (1 << i) != 0)
}

/// Assemble the output temporal units.
///
/// `key_tu`: TU containing (optionally seq header) + shown KEY_FRAME.
/// `golden_tu`: TU containing a shown non-key frame of identical content
///   (must be decodable right after the key TU — i.e. produced by the real
///   encoder as the frame following the keyframe).
/// Returns (temporal units, golden slot idx).
pub fn assemble(
    key_tu: &[u8],
    golden_tu: &[u8],
    params: &AssembleParams,
) -> Result<(Vec<TemporalUnit>, u8)> {
    // --- validate the input TUs ---
    let seq_payload = parse_obus(key_tu)?
        .into_iter()
        .find(|o| o.obu_type == ObuType::SequenceHeader)
        .map(|o| o.payload)
        .context("key temporal unit must contain a sequence header OBU")?;
    let sh = crate::seq_header::parse_sequence_header(&seq_payload)?;
    sh.check_supported()
        .context("sequence header enables features this assembler does not support")?;

    let key_info = tu_frame_info(key_tu, &sh)?;
    anyhow::ensure!(
        key_info.frame_type == Some(KEY_FRAME) && key_info.show_frame,
        "first TU must contain a shown KEY_FRAME (got {:?})",
        key_info.frame_type
    );

    let golden_info = tu_frame_info(golden_tu, &sh)?;
    let gft = golden_info
        .frame_type
        .context("golden TU cannot be show_existing")?;
    anyhow::ensure!(golden_info.show_frame, "golden frame must be shown");
    anyhow::ensure!(
        golden_info.showable_frame && gft != KEY_FRAME,
        "golden frame is not showable (frame_type={gft})"
    );
    let golden_slot = lowest_refreshed_slot(golden_info.refresh_frame_flags)
        .context("golden frame refreshes no reference slots")?;

    anyhow::ensure!(
        params.gop_size >= 2,
        "gop size must be >= 2 (keyframe + golden)"
    );
    anyhow::ensure!(
        params.total_frames >= 2,
        "need at least 2 output frames (keyframe + golden)"
    );

    // --- emit ---
    let show_tu = show_existing_tu(golden_slot);
    let mut tus: Vec<TemporalUnit> = Vec::new();
    let mut emitted = 0u64;
    while emitted < params.total_frames {
        let remaining = params.total_frames - emitted;
        let gop = remaining.min(params.gop_size);
        if gop == 1 {
            // trailing single frame: just re-show the golden
            tus.push(show_tu.clone());
            break;
        }
        tus.push(key_tu.to_vec());
        tus.push(golden_tu.to_vec());
        for _ in 2..gop {
            tus.push(show_tu.clone());
        }
        emitted += gop;
    }
    Ok((tus, golden_slot))
}

/// Build an IVF from assembled TUs, preserving geometry and timebase.
pub fn to_ivf(src: &IvfFile, tus: Vec<TemporalUnit>, fps: Option<u32>) -> IvfFile {
    let (den, num) = match fps {
        Some(f) => (f, 1),
        None => (src.timebase_den, src.timebase_num),
    };
    IvfFile {
        width: src.width,
        height: src.height,
        timebase_den: den,
        timebase_num: num,
        frames: tus
            .into_iter()
            .enumerate()
            .map(|(i, tu)| (i as u64, tu))
            .collect(),
    }
}

/// Extract (key TU, golden TU) from a coded IVF produced by a real encoder:
/// first packet must be the keyframe TU (with seq header), second the golden.
pub fn split_input(ivf: &IvfFile) -> Result<(TemporalUnit, TemporalUnit)> {
    anyhow::ensure!(ivf.frames.len() >= 2, "input needs at least 2 coded frames");
    let key_tu = ivf.frames[0].1.clone();
    anyhow::ensure!(
        tu_has_seq_header(&key_tu)?,
        "first packet must contain the sequence header OBU"
    );
    let golden_tu = ivf.frames[1].1.clone();
    Ok((key_tu, golden_tu))
}
