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
use crate::seq_header::{self, SequenceHeader};
use crate::uheader;

/// Parameters controlling the assembled stream.
pub struct AssembleParams {
    /// Output frame rate (also used for IVF timestamps when input has none).
    pub fps: u32,
    /// Total output frames.
    pub total_frames: u64,
    /// Frames per GOP (distance between keyframes). Controls seek granularity.
    pub gop_size: u64,
    /// Emit decoder_model_info() in the sequence header. Requires rewriting
    /// the real frames' headers (buffer_removal_time_present_flag is
    /// unconditional when the model is declared); we always write it as 0.
    pub decoder_model: bool,
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

/// Rewrite `tu`: splice `buffer_removal_time_present_flag=0` into each
/// Frame/FrameHeader OBU payload. `orig_sh` is the sequence header WITHOUT
/// decoder_model_info (matching the bytes being scanned); `dpb` carries
/// ref-slot state across the TUs in decode order.
fn tu_insert_removal_flag(
    tu: &[u8],
    orig_sh: &SequenceHeader,
    dpb: &mut uheader::Dpb,
) -> Result<TemporalUnit> {
    let mut out = Vec::with_capacity(tu.len() + 4);
    for obu in parse_obus(tu)? {
        match obu.obu_type {
            ObuType::Frame | ObuType::FrameHeader => {
                let scan = uheader::scan_uncompressed_header(&obu.payload, orig_sh, dpb)?;
                let payload = uheader::splice_header_bits(
                    &obu.payload,
                    scan.removal_insert_pos,
                    scan.end_pos,
                    &[(1, 0)],
                )?;
                Obu {
                    obu_type: obu.obu_type,
                    extension: obu.extension,
                    payload,
                }
                .write(&mut out);
            }
            _ => obu.write(&mut out),
        }
    }
    Ok(out)
}

/// Rewrite `tu`, replacing the sequence header OBU payload with `new_payload`
/// (all other OBUs verbatim).
fn tu_replace_seq_header(tu: &[u8], new_payload: &[u8]) -> Result<TemporalUnit> {
    let mut out = Vec::with_capacity(tu.len() + 32);
    for obu in parse_obus(tu)? {
        if obu.obu_type == ObuType::SequenceHeader {
            Obu {
                obu_type: ObuType::SequenceHeader,
                extension: obu.extension,
                payload: new_payload.to_vec(),
            }
            .write(&mut out);
        } else {
            obu.write(&mut out);
        }
    }
    Ok(out)
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

pub struct AssembleOutput {
    pub tus: Vec<TemporalUnit>,
    /// Reference slot the golden frame lives in (for debugging/inspection).
    pub golden_slot: u8,
    /// 1-based TU indices that are keyframes (seek anchors / mp4 stss).
    pub key_samples: Vec<u32>,
    /// The parsed sequence header of the input stream.
    pub seq_header: crate::seq_header::SequenceHeader,
    /// Full OBU bytes (header+size+payload) of the sequence header,
    /// for embedding into av1C.
    pub seq_header_obu: Vec<u8>,
}

/// Assemble the output temporal units.
///
/// `key_tu`: TU containing (optionally seq header) + shown KEY_FRAME.
/// `golden_tu`: TU containing a shown non-key frame of identical content
///   (must be decodable right after the key TU — i.e. produced by the real
///   encoder as the frame following the keyframe).
pub fn assemble(
    key_tu: &[u8],
    golden_tu: &[u8],
    params: &AssembleParams,
) -> Result<AssembleOutput> {
    // --- validate the input TUs ---
    let mut seq_header_obu = Vec::new();
    let mut seq_payload = None;
    for obu in parse_obus(key_tu)? {
        if obu.obu_type == ObuType::SequenceHeader {
            let mut full = Vec::new();
            obu.write(&mut full);
            seq_header_obu = full;
            seq_payload = Some(obu.payload);
            break;
        }
    }
    let seq_payload =
        seq_payload.context("key temporal unit must contain a sequence header OBU")?;
    let sh = crate::seq_header::parse_sequence_header(&seq_payload)?;
    sh.check_supported()
        .context("sequence header enables features this assembler does not support")?;

    // Streams that don't declare timing get it injected: a constant-rate
    // timing_info matching the output fps. With --decoder-model we go further:
    // decoder_model_info() is declared AND the two passthrough frames' headers
    // are spliced to carry buffer_removal_time_present_flag=0 (the flag is
    // unconditional once the model is declared; equal_picture_interval keeps
    // temporal_point_info out of every header).
    let key_tu_owned;
    let golden_tu_owned;
    let key_tu2_owned;
    let golden_tu2_owned;
    let (sh, key_tu, golden_tu, seq_header_obu) = if params.decoder_model || !sh.timing_info_present
    {
        let sh2 = if params.decoder_model {
            seq_header::with_decoder_model(&sh, params.fps.max(1))
                .context("decoder model injection")?
        } else {
            seq_header::with_timing_info(&sh, params.fps.max(1))
        };
        let payload = seq_header::emit_sequence_header(&sh2);
        let mut obu_bytes = Vec::new();
        Obu {
            obu_type: ObuType::SequenceHeader,
            extension: None,
            payload: payload.clone(),
        }
        .write(&mut obu_bytes);
        key_tu_owned = tu_replace_seq_header(key_tu, &payload)?;
        let mut golden_v = golden_tu.to_vec();
        if tu_has_seq_header(&golden_v)? {
            golden_v = tu_replace_seq_header(&golden_v, &payload)?;
        }
        golden_tu_owned = golden_v;
        if params.decoder_model {
            // Scan with the ORIGINAL seq header: the flag is absent from the
            // libaom-emitted headers; we insert it where the model wants it.
            let mut dpb = uheader::Dpb::default();
            key_tu2_owned = tu_insert_removal_flag(&key_tu_owned, &sh, &mut dpb)
                .context("splicing removal flag into keyframe")?;
            golden_tu2_owned = tu_insert_removal_flag(&golden_tu_owned, &sh, &mut dpb)
                .context("splicing removal flag into golden frame")?;
            (
                sh2,
                key_tu2_owned.as_slice(),
                golden_tu2_owned.as_slice(),
                obu_bytes,
            )
        } else {
            (
                sh2,
                key_tu_owned.as_slice(),
                golden_tu_owned.as_slice(),
                obu_bytes,
            )
        }
    } else {
        (sh, key_tu, golden_tu, seq_header_obu)
    };

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
    let mut key_samples = Vec::new();
    let mut emitted = 0u64;
    while emitted < params.total_frames {
        let remaining = params.total_frames - emitted;
        let gop = remaining.min(params.gop_size);
        if gop == 1 {
            // trailing single frame: just re-show the golden
            tus.push(show_tu.clone());
            break;
        }
        key_samples.push(tus.len() as u32 + 1);
        tus.push(key_tu.to_vec());
        tus.push(golden_tu.to_vec());
        for _ in 2..gop {
            tus.push(show_tu.clone());
        }
        emitted += gop;
    }
    Ok(AssembleOutput {
        tus,
        golden_slot,
        key_samples,
        seq_header: sh,
        seq_header_obu,
    })
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
