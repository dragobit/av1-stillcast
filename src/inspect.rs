//! Stream structure inspector: walks every temporal unit of an assembled
//! IVF and reports what each one is (KEY_FRAME / INTER / show_existing),
//! which reference slots are touched, and whether the stream satisfies the
//! invariants stillcast relies on.
//!
//! This doubles as a debug/verification tool: `--check` turns the analysis
//! into assertions (exit non-zero on violation).

use anyhow::{bail, Context, Result};

use crate::bitio::BitReader;
use crate::frame_header::{self, INTER_FRAME, KEY_FRAME};
use crate::ivf::IvfFile;
use crate::obu::{parse_obus, ObuType};
use crate::seq_header::{self, SequenceHeader};
use crate::uheader::{self, Dpb};

/// What one temporal unit showed / coded.
#[derive(Debug)]
pub enum TuKind {
    /// A coded frame (any of KEY/INTER/INTRA_ONLY/SWITCH).
    Coded {
        frame_type: u8,
        show_frame: bool,
        showable_frame: bool,
        refresh_frame_flags: u8,
        order_hint: u64,
        /// Slots this frame reads as references (ref_frame_idx), empty for intra.
        ref_slots: [u8; 7],
    },
    /// A show_existing_frame TU redisplaying `slot`.
    ShowExisting {
        slot: u8,
        /// What the referenced slot held at display time.
        slot_was_key: bool,
    },
}

pub struct TuReport {
    /// 0-based TU index = output frame index.
    pub index: usize,
    pub timestamp: u64,
    pub bytes: usize,
    pub has_seq_header: bool,
    pub kind: TuKind,
}

pub struct StreamReport {
    pub tus: Vec<TuReport>,
    pub seq_header: SequenceHeader,
    /// TU indices whose shown frame is a KEY_FRAME (decoder-reset points).
    pub key_tus: Vec<usize>,
    /// TU indices of coded frames that are non-key (golden candidates).
    pub golden_tus: Vec<usize>,
    /// TU indices of show_existing TUs.
    pub show_existing_tus: Vec<usize>,
}

fn frame_type_name(ft: u8) -> &'static str {
    match ft {
        KEY_FRAME => "KEY_FRAME",
        INTER_FRAME => "INTER_FRAME",
        2 => "INTRA_ONLY_FRAME",
        3 => "SWITCH_FRAME",
        _ => "?",
    }
}

/// Scan the `ref_frame_idx` fields out of an INTER frame's uncompressed
/// header. We don't expose them from `scan_uncompressed_header` (the
/// assembler never needed them), so this re-walks the minimal prefix using
/// the lightweight frame_header parser and then steps past order_hint /
/// primary_ref / removal times / refresh / ref_order_hints to reach
/// ref_frame_idx.
fn inter_ref_slots(payload: &[u8], sh: &SequenceHeader) -> Result<[u8; 7]> {
    let mut r = BitReader::new(payload);
    anyhow::ensure!(r.f(1)? == 0, "not a coded frame");
    let ft = r.f(2)? as u8;
    if ft == KEY_FRAME || ft == 2 {
        bail!("intra frame has no ref_frame_idx");
    }
    let show_frame = r.f(1)? == 1;
    if show_frame && sh.decoder_model_info_present && !sh.equal_picture_interval {
        r.f(usize::from(sh.frame_presentation_time_length_minus_1) + 1)?;
    }
    if !show_frame {
        r.f(1)?; // showable_frame
    }
    let er = if ft == 3 { true } else { r.f(1)? == 1 };
    r.f(1)?; // disable_cdf_update
    let allow_sct = if sh.seq_force_screen_content_tools == 2 {
        r.f(1)? == 1
    } else {
        sh.seq_force_screen_content_tools == 1
    };
    if allow_sct && sh.seq_force_integer_mv == 2 {
        r.f(1)?; // force_integer_mv
    }
    r.f(1)?; // frame_size_override_flag (SWITCH_FRAME excluded above? keep for ft!=3)
    r.f(sh.order_hint_bits)?; // order_hint
    if !er {
        r.f(3)?; // primary_ref_frame
    }
    if sh.decoder_model_info_present && r.f(1)? == 1 {
        let n = usize::from(sh.buffer_removal_time_length_minus_1) + 1;
        for op in &sh.operating_points {
            if op.decoder_model_present {
                r.f(n)?;
            }
        }
    }
    let refresh = if ft == 3 { 0xff } else { r.f(8)? };
    let _ = refresh;
    // (frame_is_intra || refresh==0xff) is false here unless refresh==0xff;
    // handle error_resilient ref_order_hints
    if er && sh.enable_order_hint && refresh != 0xff {
        for _ in 0..8 {
            r.f(sh.order_hint_bits)?;
        }
    }
    // frame_refs_short_signaling
    if sh.enable_order_hint {
        let s = r.f(1)? == 1;
        if s {
            bail!("frame_refs_short_signaling unsupported");
        }
    }
    let mut refs = [0u8; 7];
    for slot in &mut refs {
        *slot = r.f(3)? as u8;
    }
    Ok(refs)
}

/// Walk every TU of an assembled IVF. `sh` may be `None` when the stream
/// carries no sequence header (rejected upstream); otherwise it's parsed
/// from the first TU that has one.
pub fn analyze(ivf: &IvfFile) -> Result<StreamReport> {
    let mut sh: Option<SequenceHeader> = None;
    let mut dpb = Dpb::default();
    let mut have_dpb = false;
    let mut tus = Vec::new();
    let mut key_tus = Vec::new();
    let mut golden_tus = Vec::new();
    let mut show_existing_tus = Vec::new();

    for (i, (ts, packet)) in ivf.frames.iter().enumerate() {
        let obus = parse_obus(packet).with_context(|| format!("TU {i}: bad OBU framing"))?;
        let mut has_seq = false;
        let mut kind = None;
        for obu in &obus {
            match obu.obu_type {
                ObuType::SequenceHeader => {
                    has_seq = true;
                    let parsed = seq_header::parse_sequence_header(&obu.payload)
                        .with_context(|| format!("TU {i}: bad sequence header"))?;
                    if let Some(first) = &sh {
                        anyhow::ensure!(
                            seq_header::emit_sequence_header(first)
                                == seq_header::emit_sequence_header(&parsed),
                            "TU {i}: sequence header changed mid-stream"
                        );
                    } else {
                        sh = Some(parsed);
                    }
                }
                ObuType::Frame | ObuType::FrameHeader => {
                    let seq = sh.as_ref().context("frame OBU before sequence header")?;
                    let info = frame_header::parse_frame_header_info(&obu.payload, seq)
                        .with_context(|| format!("TU {i}: bad frame header"))?;
                    if info.show_existing_frame {
                        let slot = info.frame_to_show_map_idx.unwrap_or(0);
                        ensure_dpb(have_dpb, i)?;
                        // spec: a shown KEY_FRAME can't be re-shown; recorded
                        // for the report (checked in `checks`).
                        let slot_was_key = dpb.slots[slot as usize].is_key_frame;
                        show_existing_tus.push(i);
                        kind = Some(TuKind::ShowExisting { slot, slot_was_key });
                    } else {
                        // Full header scan: validates the whole uncompressed
                        // header and updates the DPB for later frames.
                        let scan = uheader::scan_uncompressed_header(&obu.payload, seq, &mut dpb)
                            .with_context(|| format!("TU {i}: header scan failed"))?;
                        have_dpb = true;
                        let ref_slots = if scan.frame_type == INTER_FRAME || scan.frame_type == 3 {
                            inter_ref_slots(&obu.payload, seq)
                                .with_context(|| format!("TU {i}: ref slots"))?
                        } else {
                            [0; 7]
                        };
                        if scan.frame_type == KEY_FRAME && scan.show_frame {
                            key_tus.push(i);
                        }
                        if scan.frame_type != KEY_FRAME && scan.showable_frame {
                            golden_tus.push(i);
                        }
                        kind = Some(TuKind::Coded {
                            frame_type: scan.frame_type,
                            show_frame: scan.show_frame,
                            showable_frame: scan.showable_frame,
                            refresh_frame_flags: scan.refresh_frame_flags,
                            order_hint: scan.order_hint,
                            ref_slots,
                        });
                    }
                }
                _ => {}
            }
        }
        tus.push(TuReport {
            index: i,
            timestamp: *ts,
            bytes: packet.len(),
            has_seq_header: has_seq,
            kind: kind.context(format!("TU {i}: no frame OBU"))?,
        });
    }

    Ok(StreamReport {
        tus,
        seq_header: sh.context("stream has no sequence header")?,
        key_tus,
        golden_tus,
        show_existing_tus,
    })
}

fn ensure_dpb(have: bool, i: usize) -> Result<()> {
    if !have {
        bail!("TU {i}: show_existing before any coded frame");
    }
    Ok(())
}

/// The invariant checks `stillcast` streams must satisfy — each returns a
/// (name, ok, detail) triple; `ok == false` means the stream is NOT a valid
/// stillcast stream in the way described.
pub fn checks(rep: &StreamReport) -> Vec<(&'static str, bool, String)> {
    let mut out = Vec::new();

    let n = rep.tus.len();
    out.push(("stream nonempty", n > 0, format!("{n} temporal units")));

    let first_ok = matches!(
        rep.tus.first().map(|t| &t.kind),
        Some(TuKind::Coded { frame_type, show_frame, .. })
            if *frame_type == KEY_FRAME && *show_frame
    );
    out.push((
        "first TU is a shown KEY_FRAME",
        first_ok,
        rep.tus
            .first()
            .map(|t| describe_kind(&t.kind))
            .unwrap_or_else(|| "empty".into()),
    ));

    let bad_show: Vec<usize> = rep
        .tus
        .iter()
        .filter_map(|t| match &t.kind {
            TuKind::ShowExisting {
                slot_was_key: true, ..
            } => Some(t.index),
            _ => None,
        })
        .collect();
    out.push((
        "no show_existing of a KEY_FRAME",
        bad_show.is_empty(),
        if bad_show.is_empty() {
            format!(
                "{} show_existing TUs, all non-key",
                rep.show_existing_tus.len()
            )
        } else {
            format!("TUs {bad_show:?} re-show keyframes")
        },
    ));

    let hidden: Vec<usize> = rep
        .tus
        .iter()
        .filter_map(|t| match &t.kind {
            TuKind::Coded {
                show_frame: false, ..
            } => Some(t.index),
            _ => None,
        })
        .collect();
    out.push((
        "every TU shows a frame",
        hidden.is_empty(),
        if hidden.is_empty() {
            "all TUs shown".into()
        } else {
            format!("hidden coded frames at {hidden:?}")
        },
    ));

    let n_golden = rep.golden_tus.len();
    out.push((
        "golden frames are INTER+showable",
        rep.golden_tus.iter().all(|&i| {
            matches!(
                &rep.tus[i].kind,
                TuKind::Coded { frame_type, showable_frame, .. }
                    if (*frame_type == INTER_FRAME || *frame_type == 3) && *showable_frame
            )
        }),
        format!("{n_golden} non-key coded frames"),
    ));

    out.push((
        "keyframe count ≥ 1",
        !rep.key_tus.is_empty(),
        format!("{} shown keyframes", rep.key_tus.len()),
    ));

    out
}

pub fn describe_kind(k: &TuKind) -> String {
    match k {
        TuKind::Coded {
            frame_type,
            show_frame,
            showable_frame,
            refresh_frame_flags,
            order_hint,
            ref_slots,
        } => {
            let mut s = format!(
                "{} show={} showable={} oh={} refresh={:08b}",
                frame_type_name(*frame_type),
                *show_frame as u8,
                *showable_frame as u8,
                order_hint,
                refresh_frame_flags
            );
            if *frame_type == INTER_FRAME || *frame_type == 3 {
                s.push_str(&format!(" refs={ref_slots:?}"));
            }
            s
        }
        TuKind::ShowExisting { slot, slot_was_key } => format!(
            "show_existing slot={} (holds {})",
            slot,
            if *slot_was_key { "KEY_FRAME" } else { "inter" }
        ),
    }
}
