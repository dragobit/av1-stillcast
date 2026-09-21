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
    /// Output frame rate numerator (time_scale in the emitted timing_info;
    /// also used for IVF timestamps when the input has none). The effective
    /// rate is `fps`/`fps_den` frames per second.
    pub fps: u32,
    /// Output frame rate denominator (num_units_in_display_tick in the
    /// emitted timing_info). 0 is treated as 1, so `fps = 30000`,
    /// `fps_den = 1001` describes NTSC 29.97 exactly.
    pub fps_den: u32,
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

/// Upper bound on the total output frame count a request may ask for.
/// 5M frames ≈ 46h at 30fps — far beyond any real static video/podcast,
/// so anything above it is a bug or an abusive request, not a use case.
/// This is a parameter-sanity check, not an allocation guarantee: the
/// assembled TU list is materialized in memory, so actual memory cost
/// also depends on GOP size and TU payload sizes — requests near the cap
/// can still fail on small machines.
pub const MAX_TOTAL_FRAMES: u64 = 5_000_000;

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

/// One display segment of a multi-image stream: an image's (key TU, golden
/// TU) pair plus how long it is shown. Every segment must carry a sequence
/// header byte-identical to the first segment's (same encode settings and
/// dimensions); segment boundaries are always shown KEY_FRAMEs, which reset
/// the DPB, so each image gets a fresh reference-slot slate.
pub struct Segment<'a> {
    pub key_tu: &'a [u8],
    pub golden_tu: &'a [u8],
    pub frames: u64,
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
    assemble_multi(
        &[Segment {
            key_tu,
            golden_tu,
            frames: params.total_frames,
        }],
        params,
    )
}

/// Multi-image variant: `segments` plays in order; each segment is carved
/// into <= `gop_size` chunks of `key + golden + show_existing` so seek
/// granularity (== max keyframe distance) is preserved across image switches.
pub fn assemble_multi(segments: &[Segment], params: &AssembleParams) -> Result<AssembleOutput> {
    anyhow::ensure!(!segments.is_empty(), "no segments");
    anyhow::ensure!(
        params.gop_size >= 2,
        "gop size must be >= 2 (keyframe + golden)"
    );
    let total: u64 = segments
        .iter()
        .fold(0u64, |t, s| t.saturating_add(s.frames));
    anyhow::ensure!(total >= 2, "need at least 2 output frames");
    for (i, seg) in segments.iter().enumerate() {
        anyhow::ensure!(seg.frames >= 1, "segment {i} requests 0 frames");
    }
    anyhow::ensure!(
        total <= MAX_TOTAL_FRAMES,
        "requested {total} output frames exceeds the {MAX_TOTAL_FRAMES} limit"
    );

    // --- canonical sequence header from segment 0; all segments must match ---
    let mut seq_header_obu = Vec::new();
    let mut seq_payload = None;
    for obu in parse_obus(segments[0].key_tu)? {
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
    for (i, seg) in segments.iter().enumerate().skip(1) {
        let mut found = None;
        for obu in parse_obus(seg.key_tu)? {
            if obu.obu_type == ObuType::SequenceHeader {
                found = Some(obu.payload);
            }
        }
        anyhow::ensure!(
            found.as_deref() == Some(seq_payload.as_slice()),
            "segment {i} has a different sequence header — encode all images \
             with identical dimensions/settings"
        );
    }

    // Streams that don't declare timing get it injected: a constant-rate
    // timing_info matching the output fps. With --decoder-model we go further:
    // decoder_model_info() is declared AND the passthrough frames' headers
    // are spliced to carry buffer_removal_time_present_flag=0 (the flag is
    // unconditional once the model is declared; equal_picture_interval keeps
    // temporal_point_info out of every header).
    let rewrite_ctx: Option<(SequenceHeader, Vec<u8>)> =
        if params.decoder_model || !sh.timing_info_present {
            let sh2 = if params.decoder_model {
                seq_header::with_decoder_model_rate(&sh, params.fps.max(1), params.fps_den.max(1))
                    .context("decoder model injection")?
            } else {
                seq_header::with_timing_info_rate(&sh, params.fps.max(1), params.fps_den.max(1))
            };
            let payload = seq_header::emit_sequence_header(&sh2);
            Some((sh2, payload))
        } else {
            None
        };
    if let Some((_, payload)) = &rewrite_ctx {
        let mut obu_bytes = Vec::new();
        Obu {
            obu_type: ObuType::SequenceHeader,
            extension: None,
            payload: payload.clone(),
        }
        .write(&mut obu_bytes);
        seq_header_obu = obu_bytes;
    }

    // --- per-segment rewrite + validation ---
    struct Prepared {
        key_tu: TemporalUnit,
        golden_tu: TemporalUnit,
        show_tu: TemporalUnit,
        golden_slot: u8,
        frames: u64,
    }
    let mut prepped: Vec<Prepared> = Vec::new();
    let mut dpb = uheader::Dpb::default(); // shared across segments, decode order
    for (i, seg) in segments.iter().enumerate() {
        let key_tu = match &rewrite_ctx {
            Some((_, payload)) => tu_replace_seq_header(seg.key_tu, payload)?,
            None => seg.key_tu.to_vec(),
        };
        let mut golden_tu = seg.golden_tu.to_vec();
        if let Some((_, payload)) = &rewrite_ctx {
            if tu_has_seq_header(&golden_tu)? {
                golden_tu = tu_replace_seq_header(&golden_tu, payload)?;
            }
        }
        // Scan with the ORIGINAL seq header: the flag is absent from the
        // encoder-emitted headers; we insert it where the model wants it.
        // Inputs that already declare decoder_model_info code the flag in
        // every frame header — splicing another bit in would shift the
        // entire remainder of the header and corrupt the stream.
        let (key_tu, golden_tu) = if params.decoder_model && !sh.decoder_model_info_present {
            (
                tu_insert_removal_flag(&key_tu, &sh, &mut dpb).with_context(|| {
                    format!("splicing removal flag into keyframe (segment {i})")
                })?,
                tu_insert_removal_flag(&golden_tu, &sh, &mut dpb)
                    .with_context(|| format!("splicing removal flag into golden (segment {i})"))?,
            )
        } else {
            (key_tu, golden_tu)
        };

        let sh_eff = match &rewrite_ctx {
            Some((sh2, _)) => sh2,
            None => &sh,
        };
        let key_info = tu_frame_info(&key_tu, sh_eff)?;
        anyhow::ensure!(
            key_info.frame_type == Some(KEY_FRAME) && key_info.show_frame,
            "segment {i}: first TU must contain a shown KEY_FRAME (got {:?})",
            key_info.frame_type
        );
        let golden_info = tu_frame_info(&golden_tu, sh_eff)?;
        let gft = golden_info
            .frame_type
            .context("golden TU cannot be show_existing")?;
        anyhow::ensure!(
            golden_info.show_frame,
            "segment {i}: golden frame must be shown"
        );
        anyhow::ensure!(
            golden_info.showable_frame && gft != KEY_FRAME,
            "segment {i}: golden frame is not showable (frame_type={gft})"
        );
        let golden_slot = lowest_refreshed_slot(golden_info.refresh_frame_flags)
            .context("golden frame refreshes no reference slots")?;

        prepped.push(Prepared {
            key_tu,
            golden_tu,
            show_tu: show_existing_tu(golden_slot),
            golden_slot,
            frames: seg.frames,
        });
    }

    // --- emit ---
    let mut tus: Vec<TemporalUnit> = Vec::new();
    let mut key_samples = Vec::new();
    for seg in &prepped {
        let mut left = seg.frames;
        let mut golden_emitted = false;
        while left > 0 {
            let chunk = left.min(params.gop_size);
            if chunk == 1 && golden_emitted {
                // trailing single frame: re-show this segment's golden
                tus.push(seg.show_tu.clone());
                break;
            }
            key_samples.push(tus.len() as u32 + 1);
            tus.push(seg.key_tu.clone());
            if chunk >= 2 {
                tus.push(seg.golden_tu.clone());
                golden_emitted = true;
                for _ in 2..chunk {
                    tus.push(seg.show_tu.clone());
                }
            }
            left -= chunk;
        }
    }
    Ok(AssembleOutput {
        tus,
        golden_slot: prepped[0].golden_slot,
        key_samples,
        seq_header: match rewrite_ctx {
            Some((sh2, _)) => sh2,
            None => sh,
        },
        seq_header_obu,
    })
}

/// Build an IVF from assembled TUs, preserving geometry and timebase.
/// `fps` is an explicit integer-rate override (`(fps, 1)`); otherwise the
/// source's timebase is kept verbatim — never a zero-rate pair.
pub fn to_ivf(src: &IvfFile, tus: Vec<TemporalUnit>, fps: Option<u32>) -> IvfFile {
    let (den, num) = match fps {
        Some(f) if f > 0 => (f, 1),
        _ => src.rate(),
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

/// Hard cap on how many leading temporal units `split_input` scans for the
/// (anchor keyframe, golden) pair. This is *not* a semantic bound: any later
/// TU is a valid candidate, so the scan runs until the pair is found or the
/// input runs out. The cap only guards pathological inputs — every scanned TU
/// is OBU-parsed and logged into the failure diagnostics, so an unbounded
/// scan could burn real work and emit an unbounded error report on a stream
/// that never yields a pair.
pub const INPUT_SCAN_LIMIT: usize = 256;

/// What the scan classifies one temporal unit as.
enum ScannedTu {
    /// OBU framing or an enclosed sequence header failed to parse.
    Malformed(String),
    /// TD / sequence-header / metadata / padding only — no coded frame.
    NoCodedFrame { seq_header: bool },
    /// A show_existing_frame TU: re-display only, no coded picture.
    ShowExisting,
    /// A TU with at least one coded frame OBU; `info` parses the first.
    Coded {
        seq_header: bool,
        /// Additional frame OBUs in the same TU (layered encodes).
        extra_frames: usize,
        info: Result<frame_header::FrameHeaderInfo>,
    },
}

fn frame_type_name(ft: u8) -> &'static str {
    match ft {
        KEY_FRAME => "KEY_FRAME",
        frame_header::INTER_FRAME => "INTER_FRAME",
        frame_header::INTRA_ONLY_FRAME => "INTRA_ONLY_FRAME",
        frame_header::SWITCH_FRAME => "SWITCH_FRAME",
        _ => "?",
    }
}

fn describe_coded(f: &frame_header::FrameHeaderInfo) -> String {
    format!(
        "{} {}",
        if f.show_frame { "shown" } else { "invisible" },
        frame_type_name(f.frame_type.unwrap_or(0xff))
    )
}

/// Classify one TU for pair selection. `sh`/`sh_raw` track the most recently
/// seen sequence header and are updated in OBU order, so a frame in the same
/// TU as its sequence header parses under it. Returns the class plus whether
/// this TU carried a sequence header with *different* bytes than the
/// governing one — a generation change that invalidates any earlier anchor.
fn classify_tu(
    tu: &[u8],
    sh: &mut Option<SequenceHeader>,
    sh_raw: &mut Option<Vec<u8>>,
) -> (ScannedTu, bool) {
    let obus = match parse_obus(tu) {
        Ok(o) => o,
        Err(e) => return (ScannedTu::Malformed(format!("{e:#}")), false),
    };
    let mut seq_header = false;
    let mut seq_changed = false;
    let mut frames: Vec<&Obu> = Vec::new();
    for obu in &obus {
        match obu.obu_type {
            ObuType::SequenceHeader => {
                seq_header = true;
                match seq_header::parse_sequence_header(&obu.payload) {
                    Ok(p) => {
                        if sh_raw.as_deref() != Some(obu.payload.as_slice()) {
                            seq_changed = true;
                            *sh_raw = Some(obu.payload.clone());
                        }
                        *sh = Some(p);
                    }
                    Err(e) => {
                        return (
                            ScannedTu::Malformed(format!("bad sequence header: {e:#}")),
                            seq_changed,
                        );
                    }
                }
            }
            ObuType::Frame | ObuType::FrameHeader => frames.push(obu),
            _ => {}
        }
    }
    let Some(first) = frames.first() else {
        return (ScannedTu::NoCodedFrame { seq_header }, seq_changed);
    };
    let info = match sh.as_ref() {
        Some(sh) => frame_header::parse_frame_header_info(&first.payload, sh),
        None => Err(anyhow::anyhow!("coded frame before any sequence header")),
    };
    if let Ok(f) = &info {
        if f.show_existing_frame {
            return (ScannedTu::ShowExisting, seq_changed);
        }
    }
    (
        ScannedTu::Coded {
            seq_header,
            extra_frames: frames.len() - 1,
            info,
        },
        seq_changed,
    )
}

/// Extract (key TU, golden TU) from a coded stream produced by a real
/// encoder. Instead of requiring packets 0 and 1 positionally, this scans
/// leading TUs — bounded only by [`INPUT_SCAN_LIMIT`] as a pathological-input
/// guard — and tests each TU against the actual contract conditions:
///
/// - **anchor**: contains the sequence header OBU and a shown KEY_FRAME.
///   A later seq+KEY_FRAME TU re-anchors the search (multi-keyframe
///   encodes); a *changed* sequence header in any other TU invalidates the
///   anchor — the returned key TU and the golden must share one header.
/// - **golden**: first TU after the anchor with a shown non-key frame that
///   is showable, refreshes at least one reference slot, and is
///   *decode-adjacent* to the anchor: no inter-coded frame may sit between
///   them, since a dropped coded frame could have refreshed the reference
///   slots the golden reads (order_hint is display order, not decode
///   adjacency). Intra-coded frames (KEY/INTRA_ONLY/SWITCH) are exempt —
///   they decode without references, so each re-bases the DPB to its own
///   picture and becomes the new predecessor. When the stream carries
///   order hints, the golden must also have `order_hint == predecessor + 1`.
///
/// TD-only, seq-header-only, metadata/padding and show_existing TUs decode
/// nothing new, so they are skipped, not fatal. When nothing qualifies,
/// the error lists per-TU why each candidate missed.
pub fn split_input(ivf: &IvfFile) -> Result<(TemporalUnit, TemporalUnit)> {
    anyhow::ensure!(!ivf.frames.is_empty(), "input carries no temporal units");
    let scan_limit = ivf.frames.len().min(INPUT_SCAN_LIMIT);

    let mut sh: Option<SequenceHeader> = None;
    let mut sh_raw: Option<Vec<u8>> = None;
    let mut lines: Vec<String> = Vec::new();
    // (TU index, order_hint, order_hint_bits of the governing seq header)
    let mut anchor: Option<(usize, u64, usize)> = None;
    // The last coded frame that provably re-based the DPB to a known state:
    // the anchor first, then any intra-family frame (each decodes without
    // references). Golden order hints are checked against this predecessor.
    let mut pred: Option<(usize, u64)> = None;
    // First post-anchor coded TU whose decode could have changed what a
    // later candidate sees — splicing candidates out from under it is
    // unverifiable. Cleared when an intra-coded frame re-bases the DPB.
    let mut dirty: Option<usize> = None;
    let mut golden = None;
    // Coded frames seen since the latest anchor / how many were non-key —
    // for the "forced keyframes" hint in failure diagnostics.
    let mut after_anchor = (0usize, 0usize); // (coded, non_key)
    let mut n_anchors = 0usize;
    let mut key_only = true; // every parsed coded frame so far is a KEY_FRAME

    for (i, (_, tu)) in ivf.frames.iter().enumerate().take(scan_limit) {
        let (scan, seq_changed) = classify_tu(tu, &mut sh, &mut sh_raw);
        // A different sequence header mid-window orphans an anchor taken
        // under the previous one — unless this same TU re-anchors under it.
        let reanchors = matches!(
            &scan,
            ScannedTu::Coded {
                seq_header: true,
                extra_frames: 0,
                info: Ok(f),
            } if f.frame_type == Some(KEY_FRAME) && f.show_frame
        );
        let invalidated = if seq_changed && !reanchors {
            anchor.take().map(|(ai, _, _)| ai)
        } else {
            None
        };
        if invalidated.is_some() {
            pred = None;
            dirty = None;
        }
        let mut line = match &scan {
            ScannedTu::Malformed(e) => format!("TU{i}: unreadable — {e}"),
            ScannedTu::NoCodedFrame { seq_header } => format!(
                "TU{i}: {} — skipped",
                if *seq_header {
                    "sequence header only, no coded frame"
                } else {
                    "no coded frame (TD / metadata / padding only)"
                }
            ),
            ScannedTu::ShowExisting => {
                format!("TU{i}: show_existing_frame TU — skipped")
            }
            ScannedTu::Coded {
                seq_header,
                extra_frames,
                info,
            } => match info {
                Err(e) => format!("TU{i}: frame header unreadable — {e:#}"),
                Ok(f) => {
                    let ft = f.frame_type.unwrap_or(0xff);
                    if ft != KEY_FRAME {
                        key_only = false;
                    }
                    let is_anchor = *seq_header && ft == KEY_FRAME && f.show_frame;
                    if anchor.is_some() && !is_anchor {
                        after_anchor.0 += 1;
                        if ft != KEY_FRAME {
                            after_anchor.1 += 1;
                        }
                    }
                    // An intra-coded frame after the anchor decodes without
                    // references and overwrites the slots it refreshes with
                    // its own still: it re-bases the decode state and becomes
                    // the predecessor a candidate's order_hint is measured
                    // against.
                    if anchor.is_some()
                        && !is_anchor
                        && matches!(
                            ft,
                            KEY_FRAME | frame_header::INTRA_ONLY_FRAME | frame_header::SWITCH_FRAME
                        )
                    {
                        pred = Some((i, f.order_hint));
                        dirty = None;
                    }
                    if *extra_frames > 0 {
                        format!(
                            "TU{i}: {} coded frames in one TU (spatial/SVC layering) — unsupported",
                            extra_frames + 1
                        )
                    } else if is_anchor {
                        let prev = match anchor {
                            Some((prev, _, _)) => format!(" (replaces TU{prev})"),
                            None => String::new(),
                        };
                        anchor = Some((
                            i,
                            f.order_hint,
                            sh.as_ref().map_or(0, |s| s.order_hint_bits),
                        ));
                        pred = Some((i, f.order_hint));
                        dirty = None;
                        after_anchor = (0, 0);
                        n_anchors += 1;
                        format!(
                            "TU{i}: anchor — sequence header + shown KEY_FRAME (order_hint {}){prev}",
                            f.order_hint
                        )
                    } else {
                        match anchor {
                            None => {
                                if ft == KEY_FRAME && f.show_frame {
                                    format!(
                                        "TU{i}: shown KEY_FRAME without a sequence header OBU — \
                                         cannot anchor (the anchor TU must carry both)"
                                    )
                                } else {
                                    format!(
                                        "TU{i}: {} — skipped (before the anchor)",
                                        describe_coded(f)
                                    )
                                }
                            }
                            Some(_) if ft == KEY_FRAME => {
                                format!(
                                    "TU{i}: KEY_FRAME — encoder forced keyframes; drop \"-g 1\" \
                                     (golden must be a shown non-key frame)"
                                )
                            }
                            Some(_) if let Some(j) = dirty => {
                                format!(
                                    "TU{i}: {} — preceded by coded TU{j}; decode state \
                                     after splicing is unverifiable",
                                    describe_coded(f)
                                )
                            }
                            Some(_) if !f.show_frame => {
                                format!("TU{i}: {} — skipped (not shown)", describe_coded(f))
                            }
                            Some(_) if !f.showable_frame => {
                                format!(
                                    "TU{i}: {} is not showable — cannot be re-shown",
                                    describe_coded(f)
                                )
                            }
                            Some(_) if f.refresh_frame_flags == 0 => {
                                format!(
                                    "TU{i}: {} refreshes no reference slots — cannot be re-shown",
                                    describe_coded(f)
                                )
                            }
                            Some((ai, aoh, bits)) => {
                                let (pi, poh) = pred.unwrap_or((ai, aoh));
                                let want = (poh + 1) % (1u64 << bits.max(1));
                                if bits == 0 {
                                    golden = Some(i);
                                    format!(
                                        "TU{i}: golden — {}; stream carries no order hints, \
                                         adjacency verified by decode order only",
                                        describe_coded(f)
                                    )
                                } else if f.order_hint == want {
                                    golden = Some(i);
                                    format!(
                                        "TU{i}: golden — {}, order_hint {} == TU{pi}+1",
                                        describe_coded(f),
                                        f.order_hint
                                    )
                                } else {
                                    format!(
                                        "TU{i}: order_hint={} vs TU{pi}+1={} — \
                                         coded against other frames",
                                        f.order_hint, want
                                    )
                                }
                            }
                        }
                    }
                }
            },
        };
        // Whatever this TU decodes changes what a later candidate sees: a
        // skipped coded frame that refreshed slots (an invisible alt-ref
        // counts), a multi-frame TU we can't fully parse, an unparsable
        // frame or a malformed TU all make the next candidate unverifiable.
        // Intra-coded frames re-based instead (handled above); frames with
        // refresh_frame_flags == 0 decode without touching the DPB.
        let dirties = match &scan {
            ScannedTu::Malformed(_) => true,
            ScannedTu::Coded {
                extra_frames, info, ..
            } => {
                *extra_frames > 0
                    || info.is_err()
                    || matches!(info, Ok(f) if f.frame_type == Some(frame_header::INTER_FRAME)
                        && f.refresh_frame_flags != 0)
            }
            _ => false,
        };
        if anchor.is_some() && dirties && dirty.is_none() {
            dirty = Some(i);
        }
        if let Some(ai) = invalidated {
            line = format!("{line}; new sequence header — anchor TU{ai} no longer applies");
        }
        lines.push(line);
        if golden.is_some() {
            break;
        }
    }

    if let (Some((ai, _, _)), Some(gi)) = (anchor, golden) {
        return Ok((ivf.frames[ai].1.clone(), ivf.frames[gi].1.clone()));
    }

    let scanned = lines.len();
    let mut msg = format!(
        "no usable keyframe+golden pair — scanned {scanned} leading temporal \
         unit(s) of {}",
        ivf.frames.len()
    );
    if ivf.frames.len() > scanned {
        msg.push_str(&format!(
            " (stopped at the INPUT_SCAN_LIMIT={INPUT_SCAN_LIMIT} guard)"
        ));
    }
    msg.push(':');
    for line in &lines {
        msg.push_str(&format!("\n  {line}"));
    }
    match anchor {
        None => msg.push_str(
            "\nwanted: a TU containing the sequence header and a shown KEY_FRAME, \
             then a shown non-key golden",
        ),
        Some((ai, _, bits)) => {
            let adjacency = if bits > 0 {
                ", order_hint == keyframe's + 1"
            } else {
                ""
            };
            msg.push_str(&format!(
                "\nanchor is TU{ai}; wanted after it: a shown non-key showable frame \
                 with refresh_frame_flags != 0{adjacency}"
            ));
            if after_anchor.0 == 0 && n_anchors <= 1 {
                msg.push_str(" — no coded frame follows it in the scanned TUs");
            } else if key_only {
                msg.push_str(
                    " — every coded frame in the window is a KEY_FRAME \
                     (encoder forced keyframes, e.g. \"-g 1\")",
                );
            }
        }
    }
    bail!("{msg}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ivf;
    use crate::obu::Obu;

    /// libaom 4-frame encode of a 640x640 still: TU0 = TD+seq+KF(oh=0),
    /// TUs 1..3 = shown INTER (oh=1,2,3).
    const SRC_IVF: &[u8] = include_bytes!("../tests/fixtures/src.ivf");

    fn src_tus() -> Vec<Vec<u8>> {
        ivf::read(SRC_IVF)
            .unwrap()
            .frames
            .into_iter()
            .map(|(_, tu)| tu)
            .collect()
    }

    fn src_sh() -> SequenceHeader {
        let payload = parse_obus(&src_tus()[0])
            .unwrap()
            .into_iter()
            .find(|o| o.obu_type == ObuType::SequenceHeader)
            .unwrap()
            .payload;
        seq_header::parse_sequence_header(&payload).unwrap()
    }

    fn ivf_of(tus: Vec<Vec<u8>>) -> IvfFile {
        IvfFile {
            width: 640,
            height: 640,
            timebase_den: 30,
            timebase_num: 1,
            frames: tus
                .into_iter()
                .enumerate()
                .map(|(i, t)| (i as u64, t))
                .collect(),
        }
    }

    fn wrap_tu(obus: Vec<Obu>) -> TemporalUnit {
        let mut tu = Vec::new();
        Obu::temporal_delimiter().write(&mut tu);
        for obu in &obus {
            obu.write(&mut tu);
        }
        tu
    }

    /// The sequence header alone in its own TU (as section-5 streams emit).
    fn seq_only_tu() -> TemporalUnit {
        wrap_tu(vec![Obu {
            obu_type: ObuType::SequenceHeader,
            extension: None,
            payload: seq_header::emit_sequence_header(&src_sh()),
        }])
    }

    /// A metadata + padding TU carrying no coded frame.
    fn meta_padding_tu() -> TemporalUnit {
        wrap_tu(vec![
            Obu {
                obu_type: ObuType::Metadata,
                extension: None,
                payload: vec![0, 0, 0xaa, 0xbb],
            },
            Obu {
                obu_type: ObuType::Padding,
                extension: None,
                payload: vec![0; 3],
            },
        ])
    }

    /// The fixture's keyframe TU with the sequence header stripped out.
    fn kf_only_tu() -> TemporalUnit {
        let mut tu = Vec::new();
        for obu in parse_obus(&src_tus()[0]).unwrap() {
            if obu.obu_type != ObuType::SequenceHeader {
                obu.write(&mut tu);
            }
        }
        tu
    }

    /// A minimal coded-frame TU whose header `parse_frame_header_info` can
    /// walk (header only — no tile data; split_input never reads those).
    #[allow(clippy::too_many_arguments)]
    fn coded_frame_tu(
        sh: &SequenceHeader,
        frame_type: u8,
        show: bool,
        showable: bool,
        error_resilient: bool,
        order_hint: u64,
        refresh: u8,
    ) -> TemporalUnit {
        let intra = frame_type == KEY_FRAME || frame_type == frame_header::INTRA_ONLY_FRAME;
        let implicit =
            frame_type == frame_header::SWITCH_FRAME || (frame_type == KEY_FRAME && show);
        let mut w = BitWriter::new();
        w.f(1, 0); // show_existing_frame
        w.f(2, u64::from(frame_type));
        w.f(1, u64::from(show));
        if !show {
            w.f(1, u64::from(showable));
        }
        if !implicit {
            w.f(1, u64::from(error_resilient));
        }
        w.f(1, 0); // disable_cdf_update
        if sh.seq_force_screen_content_tools == 2 {
            w.f(1, 0); // allow_screen_content_tools = false
        }
        if frame_type != frame_header::SWITCH_FRAME {
            w.f(1, 0); // frame_size_override_flag
        }
        w.f(sh.order_hint_bits, order_hint);
        if !intra && !error_resilient {
            w.f(3, 7); // primary_ref_frame
        }
        // fixture seq header: no decoder model
        if !implicit {
            w.f(8, u64::from(refresh));
        }
        w.trailing_bits();
        wrap_tu(vec![Obu {
            obu_type: ObuType::Frame,
            extension: None,
            payload: w.into_bytes(),
        }])
    }

    #[test]
    fn clean_input_keeps_first_two_packets() {
        // Byte-identical with the old positional behavior: the scan picks
        // packets 0 and 1 unchanged on a canonical libaom encode.
        let ivf = ivf::read(SRC_IVF).unwrap();
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, ivf.frames[0].1);
        assert_eq!(g, ivf.frames[1].1);
    }

    #[test]
    fn skips_leading_frameless_tus() {
        let src = src_tus();
        let ivf = ivf_of(vec![
            wrap_tu(vec![]), // TD only
            seq_only_tu(),
            meta_padding_tu(),
            src[0].clone(),
            src[1].clone(),
        ]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, src[1]);
    }

    #[test]
    fn skips_extra_keyframe_before_golden() {
        let src = src_tus();
        let ivf = ivf_of(vec![src[0].clone(), kf_only_tu(), src[1].clone()]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, src[1]);
    }

    #[test]
    fn skips_show_existing_tu_before_golden() {
        // show_existing decodes nothing and refreshes nothing — the golden
        // after it still sees the anchor's DPB.
        let src = src_tus();
        let ivf = ivf_of(vec![src[0].clone(), show_existing_tu(0), src[1].clone()]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, src[1]);
    }

    #[test]
    fn tolerates_invisible_frame_that_refreshes_nothing() {
        // refresh_frame_flags == 0: decoded but wrote no ref slots, so the
        // golden's decode environment is provably unchanged.
        let src = src_tus();
        let sh = src_sh();
        let invisible = coded_frame_tu(&sh, frame_header::INTER_FRAME, false, true, true, 1, 0);
        let ivf = ivf_of(vec![src[0].clone(), invisible, src[1].clone()]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, src[1]);
    }

    #[test]
    fn rejects_golden_after_a_refreshing_inter_frame() {
        // An invisible alt-ref between anchor and golden refreshes slots the
        // golden may read — order_hint adjacency cannot detect that, so the
        // candidate is unverifiable.
        let src = src_tus();
        let sh = src_sh();
        let invisible = coded_frame_tu(&sh, frame_header::INTER_FRAME, false, true, true, 1, 0xff);
        let ivf = ivf_of(vec![src[0].clone(), invisible, src[1].clone()]);
        let err = split_input(&ivf).unwrap_err().to_string();
        assert!(err.contains("unverifiable"), "{err}");
        assert!(err.contains("TU1"), "{err}");
    }

    #[test]
    fn rebases_on_an_intra_only_frame() {
        // INTRA_ONLY decodes without references, so it re-bases the decode
        // state: an inter coded right after it is adjacency-checkable
        // against *its* order_hint, not the anchor's.
        let src = src_tus();
        let sh = src_sh();
        let intra = coded_frame_tu(
            &sh,
            frame_header::INTRA_ONLY_FRAME,
            false,
            false,
            true,
            1,
            0xff,
        );
        let golden = coded_frame_tu(&sh, frame_header::INTER_FRAME, true, true, true, 2, 0x02);
        let ivf = ivf_of(vec![src[0].clone(), intra, golden.clone()]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, golden);
    }

    #[test]
    fn changed_sequence_header_invalidates_the_anchor() {
        // A seq-only TU carrying a *different* sequence header orphans the
        // anchor taken under the previous one — the golden can no longer be
        // paired with the old key TU.
        let src = src_tus();
        let mut sh2 = src_sh();
        sh2.use_128x128_superblock = !sh2.use_128x128_superblock;
        let alt_seq_tu = wrap_tu(vec![Obu {
            obu_type: ObuType::SequenceHeader,
            extension: None,
            payload: seq_header::emit_sequence_header(&sh2),
        }]);
        let ivf = ivf_of(vec![src[0].clone(), alt_seq_tu, src[1].clone()]);
        let err = split_input(&ivf).unwrap_err().to_string();
        assert!(err.contains("anchor TU0 no longer applies"), "{err}");
    }

    #[test]
    fn identical_sequence_header_keeps_the_anchor() {
        // Encoders that re-emit an identical sequence header mid-stream
        // don't invalidate the anchor.
        let src = src_tus();
        let ivf = ivf_of(vec![src[0].clone(), seq_only_tu(), src[1].clone()]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, src[1]);
    }

    #[test]
    fn reanchors_on_a_second_sequence_keyframe() {
        // [seq+KF(0), seq+KF(1), INTER(2)]: the golden was coded after the
        // second keyframe — only the (KF1, INTER) pair satisfies adjacency.
        let src = src_tus();
        let sh = src_sh();
        let kf1 = coded_frame_tu(&sh, KEY_FRAME, true, false, false, 1, 0);
        let mut kf1_tu = seq_only_tu();
        for obu in parse_obus(&kf1).unwrap() {
            if obu.obu_type != ObuType::TemporalDelimiter {
                obu.write(&mut kf1_tu);
            }
        }
        let golden2 = coded_frame_tu(&sh, frame_header::INTER_FRAME, true, true, true, 2, 0x02);
        let ivf = ivf_of(vec![src[0].clone(), kf1_tu.clone(), golden2]);
        let (k, g) = split_input(&ivf).unwrap();
        assert_eq!(k, kf1_tu);
        assert!(!g.is_empty());
    }

    #[test]
    fn rejects_all_keyframe_input() {
        let src = src_tus();
        let ivf = ivf_of(vec![src[0].clone(), kf_only_tu(), kf_only_tu()]);
        let err = split_input(&ivf).unwrap_err().to_string();
        assert!(err.contains("TU1: KEY_FRAME"), "{err}");
        assert!(err.contains("forced keyframes"), "{err}");
    }

    #[test]
    fn rejects_order_hint_gap() {
        // Golden-coded-against-other-frames: src[2] has order_hint 2, but
        // the anchor requires anchor+1 = 1.
        let src = src_tus();
        let ivf = ivf_of(vec![src[0].clone(), src[2].clone()]);
        let err = split_input(&ivf).unwrap_err().to_string();
        assert!(err.contains("order_hint=2 vs TU0+1=1"), "{err}");
    }

    #[test]
    fn finds_pair_buried_behind_many_junk_tus() {
        // Regression for the old fixed 8-TU window: leading junk TUs
        // (redundant sequence headers, metadata/padding, TD-only) must not
        // bury the anchor+golden pair.
        let src = src_tus();
        let mut tus = Vec::new();
        for i in 0..12 {
            tus.push(if i % 2 == 0 {
                seq_only_tu()
            } else {
                meta_padding_tu()
            });
        }
        tus.push(src[0].clone());
        tus.push(src[1].clone());
        let (k, g) = split_input(&ivf_of(tus)).unwrap();
        assert_eq!(k, src[0]);
        assert_eq!(g, src[1]);
        // The recovered pair expands normally.
        let params = AssembleParams {
            fps: 30,
            fps_den: 1,
            total_frames: 8,
            gop_size: 8,
            decoder_model: false,
        };
        let out = assemble(&k, &g, &params).unwrap();
        assert_eq!(out.tus.len(), 8);
        assert_eq!(out.key_samples, vec![1]);
    }

    #[test]
    fn scan_stops_at_the_guard_limit() {
        // The golden one TU past the guard cap is not seen; the error reports
        // how many leading TUs were scanned and that the cap was hit.
        let src = src_tus();
        let mut tus = vec![src[0].clone()];
        for _ in 1..INPUT_SCAN_LIMIT {
            tus.push(show_existing_tu(0));
        }
        tus.push(src[1].clone());
        let err = split_input(&ivf_of(tus)).unwrap_err().to_string();
        assert!(err.contains("scanned 256 leading"), "{err}");
        assert!(err.contains("INPUT_SCAN_LIMIT"), "{err}");
        assert!(err.contains("no coded frame follows"), "{err}");
    }

    #[test]
    fn rejects_input_without_sequence_header_in_anchor_tu() {
        let src = src_tus();
        let ivf = ivf_of(vec![seq_only_tu(), kf_only_tu(), src[1].clone()]);
        let err = split_input(&ivf).unwrap_err().to_string();
        assert!(err.contains("cannot anchor"), "{err}");
    }

    #[test]
    fn decoder_model_reexpand_is_idempotent() {
        // Feeding a --decoder-model output back through expand must not
        // splice a second buffer_removal_time_present_flag into headers
        // that already carry it — that shifts every following bit and
        // silently corrupts the stream.
        let src = src_tus();
        let params = AssembleParams {
            fps: 30,
            fps_den: 1,
            total_frames: 8,
            gop_size: 8,
            decoder_model: true,
        };
        let first = assemble(&src[0], &src[1], &params).unwrap();
        let first_tus = first.tus.clone();
        let ivf2 = ivf_of(first.tus);
        let (k, g) = split_input(&ivf2).unwrap();
        let second = assemble(&k, &g, &params).unwrap();
        assert!(second.seq_header.decoder_model_info_present);
        // Passthrough frames survive bit-exactly: no second flag splice.
        assert_eq!(second.tus[0], first_tus[0]);
        assert_eq!(second.tus[1], first_tus[1]);
        // Every coded header still scans cleanly under the emitted
        // sequence header.
        let mut dpb = uheader::Dpb::default();
        let mut scanned = 0;
        for tu in &second.tus {
            for obu in parse_obus(tu).unwrap() {
                if matches!(obu.obu_type, ObuType::Frame | ObuType::FrameHeader)
                    && obu.payload[0] & 0x80 == 0
                // not show_existing_frame
                {
                    uheader::scan_uncompressed_header(&obu.payload, &second.seq_header, &mut dpb)
                        .unwrap();
                    scanned += 1;
                }
            }
        }
        assert_eq!(scanned, 2); // key + golden
    }
}
