//! Full uncompressed-header bit walker (AV1 spec 5.9.2).
//!
//! `frame_header.rs` only walks to `refresh_frame_flags`. This module walks
//! the *entire* header so the assembler can splice
//! `buffer_removal_time_present_flag` into libaom-produced frame headers when
//! enabling the decoder model: it yields (a) the insertion bit position —
//! right after `primary_ref_frame` — and (b) the total header bit length so
//! the tail bits plus tile data can be re-packed around the insertion.
//!
//! Only syntax that libaom still-image encodes or our own assembler can emit
//! is supported; anything else (frame ids, film grain, short ref signaling,
//! reduced still headers) bails instead of guessing.

use anyhow::{ensure, Result};

use crate::bitio::{BitReader, BitWriter};
use crate::seq_header::SequenceHeader;

macro_rules! trace {
    ($r:expr, $name:literal) => {
        if std::env::var("UH_TRACE").is_ok() {
            eprintln!("[{:>4}] {}", $r.position(), $name);
        }
    };
}

const KEY_FRAME: u8 = 0;
const INTRA_ONLY_FRAME: u8 = 2;
const SWITCH_FRAME: u8 = 3;

const NUM_REF_FRAMES: usize = 8;
const REFS_PER_FRAME: usize = 7;
const PRIMARY_REF_NONE: u8 = 7;
const MAX_SEGMENTS: usize = 8;
const SEG_LVL_MAX: usize = 8;
const SEG_LVL_ALT_Q: usize = 0;
const MAX_LOOP_FILTER: u32 = 63;
const MAX_TILE_WIDTH: u64 = 4096;
const MAX_TILE_AREA: u64 = 4096 * 2304;
const MAX_TILE_COLS: u64 = 64;
const MAX_TILE_ROWS: u64 = 64;
const SUPERRES_DENOM_BITS: usize = 3;
const SUPERRES_DENOM_MIN: u64 = 9;
const SUPERRES_NUM: u64 = 8;
const GM_ABS_TRANS_BITS: u32 = 12;
const GM_ABS_TRANS_ONLY_BITS: u32 = 9;
const GM_ABS_ALPHA_BITS: u32 = 12;
const SELECT: u8 = 2;

const SEG_FEATURE_BITS: [usize; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
const SEG_FEATURE_SIGNED: [bool; SEG_LVL_MAX] = [true, true, true, true, true, false, false, false];
const SEG_FEATURE_MAX: [i64; SEG_LVL_MAX] = [
    255,
    MAX_LOOP_FILTER as i64,
    MAX_LOOP_FILTER as i64,
    MAX_LOOP_FILTER as i64,
    MAX_LOOP_FILTER as i64,
    7,
    0,
    0,
];
// Remap_Lr_Type: { NONE, SWITCHABLE, WIENER, SGRPROJ }
const RESTORE_NONE: u8 = 0;
const REMAP_LR_TYPE: [u8; 4] = [0, 3, 1, 2];

/// su(n): sign bit + magnitude, n bits total.
fn su(r: &mut BitReader, n: usize) -> Result<i64> {
    let sign = r.f(1)? == 1;
    let magnitude = r.f(n - 1)? as i64;
    Ok(if sign { -magnitude } else { magnitude })
}

/// ns(n): near-uniform code; w-1 or w bits depending on the prefix value.
fn ns(r: &mut BitReader, n: u64) -> Result<u64> {
    ensure!(n >= 1, "ns(0)");
    let w = 64 - n.leading_zeros() as usize; // floor(log2(n)) + 1
    let m = (1u64 << w) - n;
    let v = r.f(w - 1)?;
    if v < m {
        Ok(v)
    } else {
        let extra = r.f(1)?;
        Ok((v << 1) - m + extra)
    }
}

fn tile_log2(blk_size: u64, target: u64) -> u32 {
    let mut k = 0u32;
    while (blk_size << k) < target {
        k += 1;
    }
    k
}

/// decode_subexp — consumes the spec's variable-length subexponential code.
fn decode_subexp(r: &mut BitReader, num_syms: u64) -> Result<u64> {
    let mut i = 0u64;
    let mut mk = 0u64;
    let k = 3u64;
    loop {
        let b2 = if i != 0 { k + i - 1 } else { k };
        let a = 1u64 << b2;
        if num_syms <= mk + 3 * a {
            return Ok(ns(r, num_syms.saturating_sub(mk).max(1))? + mk);
        }
        if r.f(1)? == 1 {
            i += 1;
            mk += a;
        } else {
            return Ok(r.f(b2 as usize)? + mk);
        }
    }
}

/// gm type indices (only ordering matters: 0=IDENTITY 1=TRANSLATION
/// 2=ROTZOOM 3=AFFINE).
fn read_global_param(
    r: &mut BitReader,
    gm_type: u8,
    allow_high_precision_mv: bool,
    idx: usize,
) -> Result<()> {
    let mut abs_bits = GM_ABS_ALPHA_BITS;
    if idx < 2 {
        if gm_type == 1 {
            abs_bits = GM_ABS_TRANS_ONLY_BITS - u32::from(!allow_high_precision_mv);
        } else {
            abs_bits = GM_ABS_TRANS_BITS;
        }
    }
    let mx = 1u64 << abs_bits;
    // decode_signed_subexp_with_ref(-mx, mx+1, r): numSyms = 2mx + 1; the
    // reference value only picks the recenter direction — the consumed bit
    // count comes entirely from decode_subexp.
    decode_subexp(r, 2 * mx + 1)?;
    Ok(())
}

/// Per-slot decoder state the header syntax depends on.
#[derive(Clone, Copy, Default)]
pub struct RefSlot {
    pub order_hint: u64,
    pub upscaled_width: u64,
    pub frame_width: u64,
    pub frame_height: u64,
    pub render_width: u64,
    pub render_height: u64,
    pub is_key_frame: bool,
}

/// The reference-buffer state an inter frame's header is parsed against.
/// For our assembler this is seeded once from the parsed key frame.
#[derive(Clone, Default)]
pub struct Dpb {
    pub slots: [RefSlot; NUM_REF_FRAMES],
}

impl Dpb {
    /// DPB state right after a shown KEY_FRAME of the given geometry:
    /// all slots refreshed to the key frame's parameters.
    pub fn after_key_frame(w: u64, h: u64, render_w: u64, render_h: u64, upscaled_w: u64) -> Self {
        let s = RefSlot {
            order_hint: 0,
            upscaled_width: upscaled_w,
            frame_width: w,
            frame_height: h,
            render_width: render_w,
            render_height: render_h,
            is_key_frame: true,
        };
        Dpb {
            slots: [s; NUM_REF_FRAMES],
        }
    }
}

struct Ctx {
    frame_type: u8,
    frame_is_intra: bool,
    show_frame: bool,
    showable_frame: bool,
    error_resilient: bool,
    disable_cdf_update: bool,
    allow_screen_content_tools: bool,
    force_integer_mv: bool,
    frame_size_override_flag: bool,
    order_hint: u64,
    primary_ref_frame: u8,
    refresh_frame_flags: u8,
    ref_frame_idx: [u8; REFS_PER_FRAME],
    allow_high_precision_mv: bool,
    allow_intrabc: bool,
    frame_width: u64,
    frame_height: u64,
    upscaled_width: u64,
    render_width: u64,
    render_height: u64,
    num_planes: usize,
    base_q_idx: u64,
    delta_q_y_dc: i64,
    delta_q_u_dc: i64,
    delta_q_u_ac: i64,
    delta_q_v_dc: i64,
    delta_q_v_ac: i64,
    delta_q_present: bool,
    reference_select: bool,
    feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    feature_data: [[i64; SEG_LVL_MAX]; MAX_SEGMENTS],
}

pub struct HeaderScan {
    /// Bit position right after `primary_ref_frame` — where
    /// `buffer_removal_time_present_flag` is spliced in.
    pub removal_insert_pos: usize,
    /// Bit length of the complete uncompressed_header (before byte_alignment).
    pub end_pos: usize,
    pub frame_type: u8,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub refresh_frame_flags: u8,
    /// Frame geometry after superres (for seeding the DPB).
    pub frame_width: u64,
    pub frame_height: u64,
    pub upscaled_width: u64,
    pub render_width: u64,
    pub render_height: u64,
    pub order_hint: u64,
}

fn superres_params(r: &mut BitReader, sh: &SequenceHeader, c: &mut Ctx) -> Result<()> {
    let denom = if sh.enable_superres && r.f(1)? == 1 {
        r.f(SUPERRES_DENOM_BITS)? + SUPERRES_DENOM_MIN
    } else {
        SUPERRES_NUM
    };
    c.upscaled_width = c.frame_width;
    c.frame_width = (c.upscaled_width * SUPERRES_NUM + denom / 2) / denom;
    Ok(())
}

fn compute_image_size(c: &Ctx) -> (u64, u64) {
    (
        2 * ((c.frame_width + 7) >> 3),
        2 * ((c.frame_height + 7) >> 3),
    )
}

fn frame_size(r: &mut BitReader, sh: &SequenceHeader, c: &mut Ctx) -> Result<()> {
    if c.frame_size_override_flag {
        c.frame_width = r.f(usize::from(sh.frame_width_bits_minus_1) + 1)? + 1;
        c.frame_height = r.f(usize::from(sh.frame_height_bits_minus_1) + 1)? + 1;
    } else {
        c.frame_width = u64::from(sh.max_frame_width);
        c.frame_height = u64::from(sh.max_frame_height);
    }
    superres_params(r, sh, c)
}

fn render_size(r: &mut BitReader, c: &mut Ctx) -> Result<()> {
    if r.f(1)? == 1 {
        c.render_width = r.f(16)? + 1;
        c.render_height = r.f(16)? + 1;
    } else {
        c.render_width = c.upscaled_width;
        c.render_height = c.frame_height;
    }
    Ok(())
}

fn frame_size_with_refs(
    r: &mut BitReader,
    sh: &SequenceHeader,
    c: &mut Ctx,
    dpb: &Dpb,
) -> Result<()> {
    for i in 0..REFS_PER_FRAME {
        if r.f(1)? == 1 {
            let slot = &dpb.slots[c.ref_frame_idx[i] as usize];
            c.upscaled_width = slot.upscaled_width;
            c.frame_width = slot.upscaled_width;
            c.frame_height = slot.frame_height;
            c.render_width = slot.render_width;
            c.render_height = slot.render_height;
            superres_params(r, sh, c)?;
            return Ok(());
        }
    }
    frame_size(r, sh, c)?;
    render_size(r, c)
}

fn read_delta_q(r: &mut BitReader) -> Result<i64> {
    if r.f(1)? == 1 {
        su(r, 1 + 6)
    } else {
        Ok(0)
    }
}

fn quantization_params(r: &mut BitReader, sh: &SequenceHeader, c: &mut Ctx) -> Result<()> {
    c.base_q_idx = r.f(8)?;
    c.delta_q_y_dc = read_delta_q(r)?;
    if c.num_planes > 1 {
        let diff_uv_delta = if sh.separate_uv_delta_q {
            r.f(1)? == 1
        } else {
            false
        };
        c.delta_q_u_dc = read_delta_q(r)?;
        c.delta_q_u_ac = read_delta_q(r)?;
        if diff_uv_delta {
            c.delta_q_v_dc = read_delta_q(r)?;
            c.delta_q_v_ac = read_delta_q(r)?;
        } else {
            c.delta_q_v_dc = c.delta_q_u_dc;
            c.delta_q_v_ac = c.delta_q_u_ac;
        }
    }
    if r.f(1)? == 1 {
        // using_qmatrix
        r.f(4)?; // qm_y
        r.f(4)?; // qm_u
        if sh.separate_uv_delta_q {
            r.f(4)?; // qm_v
        }
    }
    Ok(())
}

fn segmentation_params(r: &mut BitReader, c: &mut Ctx) -> Result<()> {
    if r.f(1)? == 0 {
        c.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
        c.feature_data = [[0; SEG_LVL_MAX]; MAX_SEGMENTS];
        return Ok(());
    }
    let update_data = if c.primary_ref_frame == PRIMARY_REF_NONE {
        true
    } else {
        let update_map = r.f(1)? == 1;
        if update_map {
            r.f(1)?; // segmentation_temporal_update
        }
        r.f(1)? == 1 // segmentation_update_data
    };
    if update_data {
        for i in 0..MAX_SEGMENTS {
            for j in 0..SEG_LVL_MAX {
                let enabled = r.f(1)? == 1;
                c.feature_enabled[i][j] = enabled;
                let mut clipped = 0i64;
                if enabled {
                    let bits = SEG_FEATURE_BITS[j];
                    let limit = SEG_FEATURE_MAX[j];
                    if SEG_FEATURE_SIGNED[j] {
                        clipped = su(r, 1 + bits)?.clamp(-limit, limit);
                    } else {
                        clipped = (r.f(bits)? as i64).clamp(0, limit);
                    }
                }
                c.feature_data[i][j] = clipped;
            }
        }
    }
    Ok(())
}

fn tile_info(r: &mut BitReader, sh: &SequenceHeader, c: &Ctx) -> Result<()> {
    let (mi_cols, mi_rows) = compute_image_size(c);
    let (sb_cols, sb_rows, sb_shift) = if sh.use_128x128_superblock {
        ((mi_cols + 31) >> 5, (mi_rows + 31) >> 5, 5u64)
    } else {
        ((mi_cols + 15) >> 4, (mi_rows + 15) >> 4, 4u64)
    };
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
    let max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
    let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
    let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));

    let (cols_log2, rows_log2) = if r.f(1)? == 1 {
        // uniform_tile_spacing
        let mut tcl = min_log2_tile_cols;
        while tcl < max_log2_tile_cols && r.f(1)? == 1 {
            tcl += 1;
        }
        let mut trl = min_log2_tiles.saturating_sub(tcl);
        while trl < max_log2_tile_rows && r.f(1)? == 1 {
            trl += 1;
        }
        (tcl, trl)
    } else {
        let mut widest = 0u64;
        let mut start_sb = 0u64;
        let mut cols = 0u32;
        while start_sb < sb_cols {
            let max_width = (sb_cols - start_sb).min(max_tile_width_sb);
            let size_sb = ns(r, max_width)? + 1;
            widest = widest.max(size_sb);
            start_sb += size_sb;
            cols += 1;
        }
        let max_tile_area = if min_log2_tiles > 0 {
            (sb_rows * sb_cols) >> (min_log2_tiles + 1)
        } else {
            sb_rows * sb_cols
        };
        let max_tile_height_sb = (max_tile_area / widest).max(1);
        let mut start_sb = 0u64;
        let mut rows = 0u32;
        while start_sb < sb_rows {
            let max_height = (sb_rows - start_sb).min(max_tile_height_sb);
            start_sb += ns(r, max_height)? + 1;
            rows += 1;
        }
        (tile_log2(1, u64::from(cols)), tile_log2(1, u64::from(rows)))
    };
    if cols_log2 > 0 || rows_log2 > 0 {
        r.f((cols_log2 + rows_log2) as usize)?; // context_update_tile_id
        r.f(2)?; // tile_size_bytes_minus_1
    }
    Ok(())
}

fn get_qindex(c: &Ctx, seg: usize) -> i64 {
    let mut q = c.base_q_idx as i64;
    if c.feature_enabled[seg][SEG_LVL_ALT_Q] {
        q += c.feature_data[seg][SEG_LVL_ALT_Q];
    }
    q.clamp(0, 255)
}

fn coded_lossless(c: &Ctx) -> bool {
    (0..MAX_SEGMENTS).all(|s| {
        get_qindex(c, s) == 0
            && c.delta_q_y_dc == 0
            && c.delta_q_u_ac == 0
            && c.delta_q_u_dc == 0
            && c.delta_q_v_ac == 0
            && c.delta_q_v_dc == 0
    })
}

fn loop_filter_params(r: &mut BitReader, c: &Ctx, lossless: bool) -> Result<()> {
    if lossless || c.allow_intrabc {
        return Ok(());
    }
    let lf0 = r.f(6)?;
    let lf1 = r.f(6)?;
    if c.num_planes > 1 && (lf0 != 0 || lf1 != 0) {
        r.f(6)?;
        r.f(6)?;
    }
    r.f(3)?; // sharpness
    if r.f(1)? == 1 {
        // loop_filter_delta_enabled
        if r.f(1)? == 1 {
            // loop_filter_delta_update
            for _ in 0..8 {
                if r.f(1)? == 1 {
                    su(r, 1 + 6)?;
                }
            }
            for _ in 0..2 {
                if r.f(1)? == 1 {
                    su(r, 1 + 6)?;
                }
            }
        }
    }
    Ok(())
}

fn cdef_params(r: &mut BitReader, sh: &SequenceHeader, c: &Ctx, lossless: bool) -> Result<()> {
    if lossless || c.allow_intrabc || !sh.enable_cdef {
        return Ok(());
    }
    r.f(2)?; // cdef_damping_minus_3
    let bits = r.f(2)? as usize;
    for _ in 0..(1 << bits) {
        r.f(4)?; // y primary strength
        r.f(2)?; // y secondary strength
        if c.num_planes > 1 {
            r.f(4)?; // uv primary
            r.f(2)?; // uv secondary
        }
    }
    Ok(())
}

fn lr_params(r: &mut BitReader, sh: &SequenceHeader, c: &Ctx, all_lossless: bool) -> Result<()> {
    if all_lossless || c.allow_intrabc || !sh.enable_restoration {
        return Ok(());
    }
    let mut uses_lr = false;
    let mut uses_chroma_lr = false;
    for i in 0..c.num_planes {
        if REMAP_LR_TYPE[r.f(2)? as usize] != RESTORE_NONE {
            uses_lr = true;
            if i > 0 {
                uses_chroma_lr = true;
            }
        }
    }
    if uses_lr {
        if sh.use_128x128_superblock {
            r.f(1)?; // lr_unit_shift
        } else if r.f(1)? == 1 {
            r.f(1)?; // lr_unit_extra_shift
        }
        if sh.subsample_x && sh.subsample_y && uses_chroma_lr {
            r.f(1)?; // lr_uv_shift
        }
    }
    Ok(())
}

fn get_relative_dist(a: u64, b: u64, bits: usize) -> i64 {
    let m = 1i64 << (bits - 1);
    let diff = a as i64 - b as i64;
    (diff & (m - 1)) - (diff & m)
}

fn skip_mode_params(r: &mut BitReader, sh: &SequenceHeader, c: &Ctx, dpb: &Dpb) -> Result<()> {
    if c.frame_is_intra || !c.reference_select || !sh.enable_order_hint {
        return Ok(());
    }
    let mut forward: Option<(usize, u64)> = None;
    let mut backward: Option<(usize, u64)> = None;
    for i in 0..REFS_PER_FRAME {
        let hint = dpb.slots[c.ref_frame_idx[i] as usize].order_hint;
        let d = get_relative_dist(hint, c.order_hint, sh.order_hint_bits);
        if d < 0 {
            if forward.is_none_or(|(_, fh)| get_relative_dist(hint, fh, sh.order_hint_bits) > 0) {
                forward = Some((i, hint));
            }
        } else if d > 0
            && backward.is_none_or(|(_, bh)| get_relative_dist(hint, bh, sh.order_hint_bits) < 0)
        {
            backward = Some((i, hint));
        }
    }
    let allowed = match (forward, backward) {
        (None, _) => false,
        (Some(_), Some(_)) => true,
        (Some((_, fh)), None) => {
            let mut second: Option<(usize, u64)> = None;
            for i in 0..REFS_PER_FRAME {
                let hint = dpb.slots[c.ref_frame_idx[i] as usize].order_hint;
                if get_relative_dist(hint, fh, sh.order_hint_bits) < 0
                    && second
                        .is_none_or(|(_, sh2)| get_relative_dist(hint, sh2, sh.order_hint_bits) > 0)
                {
                    second = Some((i, hint));
                }
            }
            second.is_some()
        }
    };
    if allowed {
        r.f(1)?; // skip_mode_present
    }
    Ok(())
}

fn global_motion_params(r: &mut BitReader, c: &Ctx) -> Result<()> {
    if c.frame_is_intra {
        return Ok(());
    }
    for _ in 0..REFS_PER_FRAME {
        let gm_type = if r.f(1)? == 0 {
            0 // IDENTITY
        } else if r.f(1)? == 1 {
            2 // ROTZOOM
        } else if r.f(1)? == 1 {
            1 // TRANSLATION
        } else {
            3 // AFFINE
        };
        if gm_type >= 2 {
            read_global_param(r, gm_type, c.allow_high_precision_mv, 2)?;
            read_global_param(r, gm_type, c.allow_high_precision_mv, 3)?;
            if gm_type == 3 {
                read_global_param(r, gm_type, c.allow_high_precision_mv, 4)?;
                read_global_param(r, gm_type, c.allow_high_precision_mv, 5)?;
            }
        }
        if gm_type >= 1 {
            read_global_param(r, gm_type, c.allow_high_precision_mv, 0)?;
            read_global_param(r, gm_type, c.allow_high_precision_mv, 1)?;
        }
    }
    Ok(())
}

/// Walk a complete uncompressed_header for an OBU_FRAME / OBU_FRAME_HEADER
/// payload. `dpb` holds reference-slot state going into the frame and is
/// updated for the caller's next frame.
pub fn scan_uncompressed_header(
    payload: &[u8],
    sh: &SequenceHeader,
    dpb: &mut Dpb,
) -> Result<HeaderScan> {
    ensure!(
        !sh.reduced_still_picture_header,
        "reduced still picture header unsupported"
    );
    ensure!(
        !sh.film_grain_params_present,
        "film grain streams unsupported"
    );
    ensure!(
        !sh.frame_id_numbers_present,
        "frame_id_numbers_present unsupported"
    );
    let mut r = BitReader::new(payload);
    let mut c = Ctx {
        frame_type: 0,
        frame_is_intra: false,
        show_frame: false,
        showable_frame: false,
        error_resilient: false,
        disable_cdf_update: false,
        allow_screen_content_tools: false,
        force_integer_mv: false,
        frame_size_override_flag: false,
        order_hint: 0,
        primary_ref_frame: PRIMARY_REF_NONE,
        refresh_frame_flags: 0,
        ref_frame_idx: [0; REFS_PER_FRAME],
        allow_high_precision_mv: false,
        allow_intrabc: false,
        frame_width: 0,
        frame_height: 0,
        upscaled_width: 0,
        render_width: 0,
        render_height: 0,
        num_planes: if sh.mono_chrome { 1 } else { 3 },
        base_q_idx: 0,
        delta_q_y_dc: 0,
        delta_q_u_dc: 0,
        delta_q_u_ac: 0,
        delta_q_v_dc: 0,
        delta_q_v_ac: 0,
        delta_q_present: false,
        reference_select: false,
        feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
        feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
    };

    ensure!(
        r.f(1)? == 0,
        "show_existing_frame inputs are synthesized by us"
    );
    c.frame_type = r.f(2)? as u8;
    c.frame_is_intra = c.frame_type == INTRA_ONLY_FRAME || c.frame_type == KEY_FRAME;
    c.show_frame = r.f(1)? == 1;
    if c.show_frame && sh.decoder_model_info_present && !sh.equal_picture_interval {
        // temporal_point_info()
        r.f(usize::from(sh.frame_presentation_time_length_minus_1) + 1)?;
    }
    c.showable_frame = if c.show_frame {
        c.frame_type != KEY_FRAME
    } else {
        r.f(1)? == 1
    };
    c.error_resilient =
        if c.frame_type == SWITCH_FRAME || (c.frame_type == KEY_FRAME && c.show_frame) {
            true
        } else {
            r.f(1)? == 1
        };
    if c.frame_type == KEY_FRAME && c.show_frame {
        *dpb = Dpb::default(); // decoder reset
    }
    c.disable_cdf_update = r.f(1)? == 1;
    c.allow_screen_content_tools = if sh.seq_force_screen_content_tools == SELECT {
        r.f(1)? == 1
    } else {
        sh.seq_force_screen_content_tools == 1
    };
    if c.allow_screen_content_tools {
        c.force_integer_mv = if sh.seq_force_integer_mv == SELECT {
            r.f(1)? == 1
        } else {
            sh.seq_force_integer_mv == 1
        };
    }
    if c.frame_is_intra {
        c.force_integer_mv = true;
    }
    c.frame_size_override_flag = if c.frame_type == SWITCH_FRAME {
        true
    } else {
        r.f(1)? == 1
    };
    c.order_hint = r.f(sh.order_hint_bits)?;
    trace!(r, "post order_hint");
    if !(c.frame_is_intra || c.error_resilient) {
        c.primary_ref_frame = r.f(3)? as u8;
    }

    let removal_insert_pos = r.position();
    if sh.decoder_model_info_present && r.f(1)? == 1 {
        // buffer_removal_time_present_flag
        let n = usize::from(sh.buffer_removal_time_length_minus_1) + 1;
        for op in &sh.operating_points {
            if op.decoder_model_present {
                // opPtIdc==0 (single operating point) is always in-layer
                r.f(n)?;
            }
        }
    }

    c.refresh_frame_flags =
        if c.frame_type == SWITCH_FRAME || (c.frame_type == KEY_FRAME && c.show_frame) {
            0xff
        } else {
            r.f(8)? as u8
        };
    trace!(r, "post refresh_frame_flags");
    if (!c.frame_is_intra || c.refresh_frame_flags != 0xff)
        && c.error_resilient
        && sh.enable_order_hint
    {
        for _ in 0..NUM_REF_FRAMES {
            r.f(sh.order_hint_bits)?; // ref_order_hint
        }
    }

    if c.frame_is_intra {
        trace!(r, "pre frame_size");
        frame_size(&mut r, sh, &mut c)?;
        trace!(r, "post frame_size");
        render_size(&mut r, &mut c)?;
        if c.allow_screen_content_tools && c.upscaled_width == c.frame_width {
            c.allow_intrabc = r.f(1)? == 1;
        }
    } else {
        let short_sig = if !sh.enable_order_hint {
            false
        } else {
            let s = r.f(1)? == 1;
            if s {
                r.f(3)?; // last_frame_idx
                r.f(3)?; // gold_frame_idx
            }
            s
        };
        ensure!(
            !short_sig,
            "frame_refs_short_signaling needs set_frame_refs — unsupported input"
        );
        for i in 0..REFS_PER_FRAME {
            c.ref_frame_idx[i] = r.f(3)? as u8;
        }
        if c.frame_size_override_flag && !c.error_resilient {
            frame_size_with_refs(&mut r, sh, &mut c, dpb)?;
        } else {
            frame_size(&mut r, sh, &mut c)?;
            render_size(&mut r, &mut c)?;
        }
        if !c.force_integer_mv {
            c.allow_high_precision_mv = r.f(1)? == 1;
        }
        if r.f(1)? == 0 {
            r.f(2)?; // interpolation_filter
        }
        r.f(1)?; // is_motion_mode_switchable
        if !(c.error_resilient || !sh.enable_ref_frame_mvs) {
            r.f(1)?; // use_ref_frame_mvs
        }
    }
    if !c.disable_cdf_update {
        r.f(1)?; // disable_frame_end_update_cdf
    }

    trace!(r, "pre tile_info");
    tile_info(&mut r, sh, &c)?;
    trace!(r, "post tile_info");
    quantization_params(&mut r, sh, &mut c)?;
    trace!(r, "post quantization");
    segmentation_params(&mut r, &mut c)?;
    trace!(r, "post segmentation");
    if c.base_q_idx > 0 {
        c.delta_q_present = r.f(1)? == 1;
        if c.delta_q_present {
            r.f(2)?; // delta_q_res
        }
    }
    if c.delta_q_present {
        let present = if !c.allow_intrabc {
            r.f(1)? == 1
        } else {
            false
        };
        if present {
            r.f(2)?; // delta_lf_res
            r.f(1)?; // delta_lf_multi
        }
    }

    let lossless = coded_lossless(&c);
    let all_lossless = lossless && c.frame_width == c.upscaled_width;
    trace!(r, "pre loopfilter");
    loop_filter_params(&mut r, &c, lossless)?;
    trace!(r, "post loopfilter");
    cdef_params(&mut r, sh, &c, lossless)?;
    trace!(r, "post cdef");
    lr_params(&mut r, sh, &c, all_lossless)?;
    trace!(r, "post lr");
    if !lossless {
        r.f(1)?; // tx_mode_select
    }
    if !c.frame_is_intra {
        c.reference_select = r.f(1)? == 1;
    }
    trace!(r, "pre skip_mode");
    skip_mode_params(&mut r, sh, &c, dpb)?;
    trace!(r, "post skip_mode");
    if !(c.frame_is_intra || c.error_resilient || !sh.enable_warped_motion) {
        r.f(1)?; // allow_warped_motion
    }
    r.f(1)?; // reduced_tx_set
    trace!(r, "pre global_motion");
    global_motion_params(&mut r, &c)?;
    trace!(r, "post global_motion");
    // film_grain_params rejected at entry

    // update caller-side DPB
    for i in 0..NUM_REF_FRAMES {
        if c.refresh_frame_flags & (1 << i) != 0 {
            dpb.slots[i] = RefSlot {
                order_hint: c.order_hint,
                upscaled_width: c.upscaled_width,
                frame_width: c.frame_width,
                frame_height: c.frame_height,
                render_width: c.render_width,
                render_height: c.render_height,
                is_key_frame: c.frame_type == KEY_FRAME,
            };
        }
    }

    Ok(HeaderScan {
        removal_insert_pos,
        end_pos: r.position(),
        frame_type: c.frame_type,
        show_frame: c.show_frame,
        showable_frame: c.showable_frame,
        refresh_frame_flags: c.refresh_frame_flags,
        frame_width: c.frame_width,
        frame_height: c.frame_height,
        upscaled_width: c.upscaled_width,
        render_width: c.render_width,
        render_height: c.render_height,
        order_hint: c.order_hint,
    })
}

/// Splice `insert` (a list of (nbits, value) fields) into `payload` at bit
/// position `pos`, keeping bits [pos..header_end) of the original header,
/// re-emitting byte_alignment, then copying the bytes that followed the
/// original header's alignment (tile data) verbatim.
pub fn splice_header_bits(
    payload: &[u8],
    pos: usize,
    header_end: usize,
    insert: &[(usize, u64)],
) -> Result<Vec<u8>> {
    ensure!(pos <= header_end, "insert position past header end");
    ensure!(header_end <= payload.len() * 8, "header end past payload");
    let mut w = BitWriter::new();
    let mut r = BitReader::new(payload);
    for _ in 0..pos {
        w.f(1, r.f(1)?);
    }
    for (n, v) in insert {
        w.f(*n, *v);
    }
    for _ in pos..header_end {
        w.f(1, r.f(1)?);
    }
    w.byte_alignment();
    let mut bytes = w.into_bytes();
    let tile_start = header_end.div_ceil(8);
    if tile_start < payload.len() {
        bytes.extend_from_slice(&payload[tile_start..]);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_no_insert_is_bit_and_tile_identical() {
        // header 10 bits spanning 2 bytes + 2 bytes of "tile" data
        let payload = vec![0b1010_1101, 0b0110_0011, 0xAB, 0xCD];
        let header_end = 10;
        let out = splice_header_bits(&payload, 5, header_end, &[]).unwrap();
        let mut ro = BitReader::new(&out);
        let mut ri = BitReader::new(&payload);
        for _ in 0..header_end {
            assert_eq!(ro.f(1).unwrap(), ri.f(1).unwrap());
        }
        // alignment of 10 bits + 1 => 11 → pads to 16
        assert_eq!(out.len(), 2 + 2);
        assert_eq!(&out[2..], &payload[2..]);
    }

    #[test]
    fn splice_insert_shifts_tail() {
        let payload = vec![0b1010_1101, 0b0110_0011, 0xAB];
        let out = splice_header_bits(&payload, 5, 10, &[(1, 0)]).unwrap();
        let mut r = BitReader::new(&out);
        let mut ri = BitReader::new(&payload);
        for _ in 0..5 {
            assert_eq!(r.f(1).unwrap(), ri.f(1).unwrap());
        }
        assert_eq!(r.f(1).unwrap(), 0); // inserted bit
        for _ in 5..10 {
            assert_eq!(r.f(1).unwrap(), ri.f(1).unwrap());
        }
    }
}
