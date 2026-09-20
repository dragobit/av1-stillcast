//! Sequence header OBU parser + emitter (AV1 spec 5.5).
//!
//! The parser stores every field needed to re-emit the header bit-exactly,
//! so the assembler can rebuild the sequence header with additional sections
//! (timing_info now, decoder_model_info next) while leaving the rest
//! untouched. The unit test asserts parse→emit is a byte-exact round trip.

use anyhow::{bail, Result};

use crate::bitio::{BitReader, BitWriter};

const SELECT_SCREEN_CONTENT_TOOLS: u8 = 2;
const SELECT_INTEGER_MV: u8 = 2;

/// One operating point entry (operating_point_idc + level + optional
/// decoder-model and initial-display-delay sub-fields).
#[derive(Debug, Clone, Default)]
pub struct OperatingPoint {
    pub idc: u16,
    pub seq_level_idx: u8,
    pub seq_tier: bool,
    pub decoder_model_present: bool,
    pub decoder_buffer_delay: u64,
    pub encoder_buffer_delay: u64,
    pub low_delay_mode: bool,
    pub initial_display_delay_present: bool,
    pub initial_display_delay_minus_1: u8,
}

#[derive(Debug, Clone)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    pub timing_info_present: bool,
    pub equal_picture_interval: bool,
    pub decoder_model_info_present: bool,
    pub frame_id_numbers_present: bool,
    /// idLen when frame_id_numbers_present (bits for current/display_frame_id).
    pub id_len: usize,
    pub use_128x128_superblock: bool,
    pub enable_order_hint: bool,
    pub order_hint_bits: usize,
    pub seq_force_screen_content_tools: u8,
    pub seq_force_integer_mv: u8,
    pub film_grain_params_present: bool,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub bit_depth: u8,
    pub mono_chrome: bool,
    // --- needed to build the av1C record for mp4 muxing ---
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub subsample_x: bool,
    pub subsample_y: bool,
    pub chroma_sample_position: u8,
    // --- stored verbatim for bit-exact re-emission ---
    pub num_units_in_display_tick: u32,
    pub time_scale: u32,
    pub num_ticks_per_picture_minus_1: u64,
    pub buffer_delay_length_minus_1: u8,
    pub num_units_in_decoding_tick: u32,
    pub buffer_removal_time_length_minus_1: u8,
    pub frame_presentation_time_length_minus_1: u8,
    pub initial_display_delay_present: bool,
    pub operating_points: Vec<OperatingPoint>,
    pub frame_width_bits_minus_1: u8,
    pub frame_height_bits_minus_1: u8,
    pub delta_frame_id_length_minus_2: u8,
    pub additional_frame_id_length_minus_1: u8,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_interintra_compound: bool,
    pub enable_masked_compound: bool,
    pub enable_warped_motion: bool,
    pub enable_dual_filter: bool,
    pub enable_jnt_comp: bool,
    pub enable_ref_frame_mvs: bool,
    pub seq_choose_screen_content_tools: bool,
    pub seq_choose_integer_mv: bool,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub high_bitdepth: bool,
    /// profile2-only: selects 12-bit when high_bitdepth is set.
    pub twelve_bit: bool,
    pub color_description_present: bool,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    /// `None` when the syntax implies full range (BT709+sRGB+identity).
    pub color_range: Option<bool>,
    pub separate_uv_delta_q: bool,
}

fn timing_info(r: &mut BitReader, sh: &mut SequenceHeader) -> Result<()> {
    sh.num_units_in_display_tick = r.f(32)? as u32;
    sh.time_scale = r.f(32)? as u32;
    sh.equal_picture_interval = r.f(1)? == 1;
    if sh.equal_picture_interval {
        sh.num_ticks_per_picture_minus_1 = r.uvlc()?;
    }
    Ok(())
}

fn decoder_model_info(r: &mut BitReader, sh: &mut SequenceHeader) -> Result<()> {
    sh.buffer_delay_length_minus_1 = r.f(5)? as u8;
    sh.num_units_in_decoding_tick = r.f(32)? as u32;
    sh.buffer_removal_time_length_minus_1 = r.f(5)? as u8;
    sh.frame_presentation_time_length_minus_1 = r.f(5)? as u8;
    Ok(())
}

fn operating_parameters_info(
    r: &mut BitReader,
    buffer_delay_len: usize,
    op: &mut OperatingPoint,
) -> Result<()> {
    op.decoder_buffer_delay = r.f(buffer_delay_len)?;
    op.encoder_buffer_delay = r.f(buffer_delay_len)?;
    op.low_delay_mode = r.f(1)? == 1;
    Ok(())
}

fn color_config(r: &mut BitReader, sh: &mut SequenceHeader) -> Result<()> {
    sh.high_bitdepth = r.f(1)? == 1;
    let bit_depth = if sh.seq_profile == 2 && sh.high_bitdepth {
        if r.f(1)? == 1 {
            sh.twelve_bit = true;
            12
        } else {
            10
        }
    } else if sh.high_bitdepth {
        10
    } else {
        8
    };
    sh.bit_depth = bit_depth;

    let mono_chrome = if sh.seq_profile == 1 {
        false
    } else {
        r.f(1)? == 1
    };
    sh.mono_chrome = mono_chrome;

    sh.color_description_present = r.f(1)? == 1;
    let (cp, tc, mc) = if sh.color_description_present {
        (r.f(8)? as u8, r.f(8)? as u8, r.f(8)? as u8)
    } else {
        (2u8, 2u8, 2u8) // unspecified
    };
    sh.color_primaries = cp;
    sh.transfer_characteristics = tc;
    sh.matrix_coefficients = mc;

    if mono_chrome {
        sh.color_range = Some(r.f(1)? == 1);
        // subsampling fixed at 4:0:0; chroma_sample_position not signaled
        sh.separate_uv_delta_q = r.f(1)? == 1;
        return Ok(());
    }

    // BT709 primaries (1), sRGB transfer (13), identity matrix (0)
    if cp == 1 && tc == 13 && mc == 0 {
        // color_range implicitly full; subsampling 4:4:4
    } else {
        sh.color_range = Some(r.f(1)? == 1);
        let (sub_x, sub_y) = match sh.seq_profile {
            0 => (true, true),   // 4:2:0
            1 => (false, false), // 4:4:4
            _ => {
                // profile 2
                if bit_depth == 12 {
                    let sx = r.f(1)? == 1;
                    let sy = if sx { r.f(1)? == 1 } else { false };
                    (sx, sy)
                } else {
                    (true, false) // 4:2:2
                }
            }
        };
        sh.subsample_x = sub_x;
        sh.subsample_y = sub_y;
        if sub_x && sub_y {
            sh.chroma_sample_position = r.f(2)? as u8;
        }
    }
    sh.separate_uv_delta_q = r.f(1)? == 1;
    Ok(())
}

/// Parse a sequence header OBU payload.
pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeader> {
    let mut r = BitReader::new(payload);
    let mut sh = SequenceHeader::default_for_parse();
    sh.seq_profile = r.f(3)? as u8;
    sh.still_picture = r.f(1)? == 1;
    sh.reduced_still_picture_header = r.f(1)? == 1;
    anyhow::ensure!(sh.seq_profile <= 2, "seq_profile > 2");

    if sh.reduced_still_picture_header {
        sh.seq_level_idx = r.f(5)? as u8;
        sh.operating_points.push(OperatingPoint {
            seq_level_idx: sh.seq_level_idx,
            ..Default::default()
        });
    } else {
        sh.timing_info_present = r.f(1)? == 1;
        if sh.timing_info_present {
            timing_info(&mut r, &mut sh)?;
            sh.decoder_model_info_present = r.f(1)? == 1;
            if sh.decoder_model_info_present {
                decoder_model_info(&mut r, &mut sh)?;
            }
        }
        sh.initial_display_delay_present = r.f(1)? == 1;
        let operating_points_cnt = (r.f(5)? + 1) as usize;

        for i in 0..operating_points_cnt {
            let mut op = OperatingPoint {
                idc: r.f(12)? as u16,
                seq_level_idx: r.f(5)? as u8,
                ..Default::default()
            };
            if op.seq_level_idx > 7 {
                op.seq_tier = r.f(1)? == 1;
            }
            if i == 0 {
                sh.seq_level_idx = op.seq_level_idx;
                sh.seq_tier = op.seq_tier as u8;
            }
            if sh.decoder_model_info_present {
                op.decoder_model_present = r.f(1)? == 1;
                if op.decoder_model_present {
                    operating_parameters_info(
                        &mut r,
                        usize::from(sh.buffer_delay_length_minus_1) + 1,
                        &mut op,
                    )?;
                }
            }
            if sh.initial_display_delay_present {
                op.initial_display_delay_present = r.f(1)? == 1;
                if op.initial_display_delay_present {
                    op.initial_display_delay_minus_1 = r.f(4)? as u8;
                }
            }
            sh.operating_points.push(op);
        }
    }

    sh.frame_width_bits_minus_1 = r.f(4)? as u8;
    sh.frame_height_bits_minus_1 = r.f(4)? as u8;
    sh.max_frame_width = (r.f(usize::from(sh.frame_width_bits_minus_1) + 1)? + 1) as u32;
    sh.max_frame_height = (r.f(usize::from(sh.frame_height_bits_minus_1) + 1)? + 1) as u32;

    if !sh.reduced_still_picture_header {
        sh.frame_id_numbers_present = r.f(1)? == 1;
        if sh.frame_id_numbers_present {
            sh.delta_frame_id_length_minus_2 = r.f(4)? as u8;
            sh.additional_frame_id_length_minus_1 = r.f(3)? as u8;
            sh.id_len = usize::from(sh.additional_frame_id_length_minus_1)
                + usize::from(sh.delta_frame_id_length_minus_2)
                + 3;
        }
    }

    sh.use_128x128_superblock = r.f(1)? == 1;
    sh.enable_filter_intra = r.f(1)? == 1;
    sh.enable_intra_edge_filter = r.f(1)? == 1;

    if sh.reduced_still_picture_header {
        // fixed values per spec
        sh.enable_order_hint = false;
        sh.seq_force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS;
        sh.seq_force_integer_mv = SELECT_INTEGER_MV;
    } else {
        sh.enable_interintra_compound = r.f(1)? == 1;
        sh.enable_masked_compound = r.f(1)? == 1;
        sh.enable_warped_motion = r.f(1)? == 1;
        sh.enable_dual_filter = r.f(1)? == 1;
        sh.enable_order_hint = r.f(1)? == 1;
        if sh.enable_order_hint {
            sh.enable_jnt_comp = r.f(1)? == 1;
            sh.enable_ref_frame_mvs = r.f(1)? == 1;
        }
        sh.seq_choose_screen_content_tools = r.f(1)? == 1;
        sh.seq_force_screen_content_tools = if sh.seq_choose_screen_content_tools {
            SELECT_SCREEN_CONTENT_TOOLS
        } else {
            r.f(1)? as u8
        };
        if sh.seq_force_screen_content_tools > 0 {
            sh.seq_choose_integer_mv = r.f(1)? == 1;
            sh.seq_force_integer_mv = if sh.seq_choose_integer_mv {
                SELECT_INTEGER_MV
            } else {
                r.f(1)? as u8
            };
        }
        if sh.enable_order_hint {
            sh.order_hint_bits = (r.f(3)? + 1) as usize;
        }
    }

    sh.enable_superres = r.f(1)? == 1;
    sh.enable_cdef = r.f(1)? == 1;
    sh.enable_restoration = r.f(1)? == 1;
    color_config(&mut r, &mut sh)?;
    sh.film_grain_params_present = r.f(1)? == 1;

    Ok(sh)
}

/// Re-emit a sequence header OBU payload (with trailing bits), mirroring the
/// parse order exactly.
pub fn emit_sequence_header(sh: &SequenceHeader) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.f(3, u64::from(sh.seq_profile));
    w.f(1, u64::from(sh.still_picture));
    w.f(1, u64::from(sh.reduced_still_picture_header));

    if sh.reduced_still_picture_header {
        w.f(5, u64::from(sh.seq_level_idx));
    } else {
        w.f(1, u64::from(sh.timing_info_present));
        if sh.timing_info_present {
            w.f(32, u64::from(sh.num_units_in_display_tick));
            w.f(32, u64::from(sh.time_scale));
            w.f(1, u64::from(sh.equal_picture_interval));
            if sh.equal_picture_interval {
                w.uvlc(sh.num_ticks_per_picture_minus_1);
            }
            w.f(1, u64::from(sh.decoder_model_info_present));
            if sh.decoder_model_info_present {
                w.f(5, u64::from(sh.buffer_delay_length_minus_1));
                w.f(32, u64::from(sh.num_units_in_decoding_tick));
                w.f(5, u64::from(sh.buffer_removal_time_length_minus_1));
                w.f(5, u64::from(sh.frame_presentation_time_length_minus_1));
            }
        }
        w.f(1, u64::from(sh.initial_display_delay_present));
        w.f(5, sh.operating_points.len() as u64 - 1);
        let bdlen = usize::from(sh.buffer_delay_length_minus_1) + 1;
        for op in &sh.operating_points {
            w.f(12, u64::from(op.idc));
            w.f(5, u64::from(op.seq_level_idx));
            if op.seq_level_idx > 7 {
                w.f(1, u64::from(op.seq_tier));
            }
            if sh.decoder_model_info_present {
                w.f(1, u64::from(op.decoder_model_present));
                if op.decoder_model_present {
                    w.f(bdlen, op.decoder_buffer_delay);
                    w.f(bdlen, op.encoder_buffer_delay);
                    w.f(1, u64::from(op.low_delay_mode));
                }
            }
            if sh.initial_display_delay_present {
                w.f(1, u64::from(op.initial_display_delay_present));
                if op.initial_display_delay_present {
                    w.f(4, u64::from(op.initial_display_delay_minus_1));
                }
            }
        }
    }

    w.f(4, u64::from(sh.frame_width_bits_minus_1));
    w.f(4, u64::from(sh.frame_height_bits_minus_1));
    w.f(
        usize::from(sh.frame_width_bits_minus_1) + 1,
        u64::from(sh.max_frame_width) - 1,
    );
    w.f(
        usize::from(sh.frame_height_bits_minus_1) + 1,
        u64::from(sh.max_frame_height) - 1,
    );

    if !sh.reduced_still_picture_header {
        w.f(1, u64::from(sh.frame_id_numbers_present));
        if sh.frame_id_numbers_present {
            w.f(4, u64::from(sh.delta_frame_id_length_minus_2));
            w.f(3, u64::from(sh.additional_frame_id_length_minus_1));
        }
    }

    w.f(1, u64::from(sh.use_128x128_superblock));
    w.f(1, u64::from(sh.enable_filter_intra));
    w.f(1, u64::from(sh.enable_intra_edge_filter));

    if !sh.reduced_still_picture_header {
        w.f(1, u64::from(sh.enable_interintra_compound));
        w.f(1, u64::from(sh.enable_masked_compound));
        w.f(1, u64::from(sh.enable_warped_motion));
        w.f(1, u64::from(sh.enable_dual_filter));
        w.f(1, u64::from(sh.enable_order_hint));
        if sh.enable_order_hint {
            w.f(1, u64::from(sh.enable_jnt_comp));
            w.f(1, u64::from(sh.enable_ref_frame_mvs));
        }
        w.f(1, u64::from(sh.seq_choose_screen_content_tools));
        if !sh.seq_choose_screen_content_tools {
            w.f(1, u64::from(sh.seq_force_screen_content_tools));
        }
        if sh.seq_force_screen_content_tools > 0 {
            w.f(1, u64::from(sh.seq_choose_integer_mv));
            if !sh.seq_choose_integer_mv {
                w.f(1, u64::from(sh.seq_force_integer_mv));
            }
        }
        if sh.enable_order_hint {
            w.f(3, sh.order_hint_bits as u64 - 1);
        }
    }

    w.f(1, u64::from(sh.enable_superres));
    w.f(1, u64::from(sh.enable_cdef));
    w.f(1, u64::from(sh.enable_restoration));

    // color_config
    w.f(1, u64::from(sh.high_bitdepth));
    if sh.seq_profile == 2 && sh.high_bitdepth {
        w.f(1, u64::from(sh.twelve_bit));
    }
    if sh.seq_profile != 1 {
        w.f(1, u64::from(sh.mono_chrome));
    }
    w.f(1, u64::from(sh.color_description_present));
    if sh.color_description_present {
        w.f(8, u64::from(sh.color_primaries));
        w.f(8, u64::from(sh.transfer_characteristics));
        w.f(8, u64::from(sh.matrix_coefficients));
    }
    let implied_444 =
        sh.color_primaries == 1 && sh.transfer_characteristics == 13 && sh.matrix_coefficients == 0;
    if sh.mono_chrome {
        w.f(1, u64::from(sh.color_range.unwrap_or(false)));
        w.f(1, u64::from(sh.separate_uv_delta_q));
    } else {
        if !implied_444 {
            w.f(1, u64::from(sh.color_range.unwrap_or(false)));
            if sh.seq_profile == 2 && sh.bit_depth == 12 {
                w.f(1, u64::from(sh.subsample_x));
                if sh.subsample_x {
                    w.f(1, u64::from(sh.subsample_y));
                }
            }
            if sh.subsample_x && sh.subsample_y {
                w.f(2, u64::from(sh.chroma_sample_position));
            }
        }
        w.f(1, u64::from(sh.separate_uv_delta_q));
    }

    w.f(1, u64::from(sh.film_grain_params_present));
    w.trailing_bits();
    w.into_bytes()
}

/// Return a copy of `sh` declaring constant-rate timing for `fps`
/// (timing_info + equal_picture_interval, no decoder model).
pub fn with_timing_info(sh: &SequenceHeader, fps: u32) -> SequenceHeader {
    with_timing_info_rate(sh, fps, 1)
}

/// Return a copy of `sh` declaring constant-rate timing at
/// `rate_num`/`rate_den` frames per second — `time_scale = rate_num`,
/// `num_units_in_display_tick = rate_den` (e.g. 30000/1001 for NTSC).
pub fn with_timing_info_rate(sh: &SequenceHeader, rate_num: u32, rate_den: u32) -> SequenceHeader {
    let mut out = sh.clone();
    out.timing_info_present = true;
    out.num_units_in_display_tick = rate_den.max(1);
    out.time_scale = rate_num.max(1);
    out.equal_picture_interval = true;
    out.num_ticks_per_picture_minus_1 = 0;
    out
}

/// Return a copy of `sh` with decoder_model_info() enabled (requires and
/// implies timing_info). Frames gain only `buffer_removal_time_present_flag`
/// (we always emit it as 0): equal_picture_interval makes decode timing
/// implicit, so no removal-time fields are ever written.
///
/// A stream that already declares the model is returned unchanged: its
/// declared field widths are load-bearing for the flag/removal-time bits
/// already coded in its frame headers.
pub fn with_decoder_model(sh: &SequenceHeader, fps: u32) -> Result<SequenceHeader> {
    with_decoder_model_rate(sh, fps, 1)
}

/// Rational-rate variant of [`with_decoder_model`]: `rate_num`/`rate_den`
/// frames per second, as with [`with_timing_info_rate`].
pub fn with_decoder_model_rate(
    sh: &SequenceHeader,
    rate_num: u32,
    rate_den: u32,
) -> Result<SequenceHeader> {
    if sh.decoder_model_info_present {
        return Ok(sh.clone());
    }
    let mut out = with_timing_info_rate(sh, rate_num, rate_den);
    if sh.timing_info_present {
        if !sh.equal_picture_interval {
            bail!("decoder model on variable-interval streams would need temporal_point_info");
        }
        // Keep the source's declared timing.
        out.num_units_in_display_tick = sh.num_units_in_display_tick;
        out.time_scale = sh.time_scale;
        out.num_ticks_per_picture_minus_1 = sh.num_ticks_per_picture_minus_1;
    }
    out.decoder_model_info_present = true;
    out.buffer_delay_length_minus_1 = 31; // 32-bit delays
    out.num_units_in_decoding_tick = rate_num;
    // removal times we never emit would need <= 32 bits; 16 suffices as a
    // legal declaration since the flag is 0 in every header.
    out.buffer_removal_time_length_minus_1 = 15;
    // presentation time must index every shown frame; 24 bits ≈ 155h @30fps.
    out.frame_presentation_time_length_minus_1 = 23;
    for op in &mut out.operating_points {
        op.decoder_model_present = true;
        // ~1 second of buffer at the nominal 1/90000 s units.
        op.decoder_buffer_delay = 90_000;
        op.encoder_buffer_delay = 90_000;
        op.low_delay_mode = false;
    }
    Ok(out)
}

impl Default for SequenceHeader {
    fn default() -> Self {
        SequenceHeader {
            seq_profile: 0,
            still_picture: false,
            reduced_still_picture_header: false,
            timing_info_present: false,
            equal_picture_interval: false,
            decoder_model_info_present: false,
            frame_id_numbers_present: false,
            id_len: 0,
            use_128x128_superblock: false,
            enable_order_hint: false,
            order_hint_bits: 0,
            seq_force_screen_content_tools: SELECT_SCREEN_CONTENT_TOOLS,
            seq_force_integer_mv: SELECT_INTEGER_MV,
            film_grain_params_present: false,
            max_frame_width: 0,
            max_frame_height: 0,
            bit_depth: 8,
            mono_chrome: false,
            seq_level_idx: 0,
            seq_tier: 0,
            subsample_x: false,
            subsample_y: false,
            chroma_sample_position: 0,
            num_units_in_display_tick: 0,
            time_scale: 0,
            num_ticks_per_picture_minus_1: 0,
            buffer_delay_length_minus_1: 0,
            num_units_in_decoding_tick: 0,
            buffer_removal_time_length_minus_1: 0,
            frame_presentation_time_length_minus_1: 0,
            initial_display_delay_present: false,
            operating_points: Vec::new(),
            frame_width_bits_minus_1: 0,
            frame_height_bits_minus_1: 0,
            delta_frame_id_length_minus_2: 0,
            additional_frame_id_length_minus_1: 0,
            enable_filter_intra: false,
            enable_intra_edge_filter: false,
            enable_interintra_compound: false,
            enable_masked_compound: false,
            enable_warped_motion: false,
            enable_dual_filter: false,
            enable_jnt_comp: false,
            enable_ref_frame_mvs: false,
            seq_choose_screen_content_tools: false,
            seq_choose_integer_mv: false,
            enable_superres: false,
            enable_cdef: false,
            enable_restoration: false,
            high_bitdepth: false,
            twelve_bit: false,
            color_description_present: false,
            color_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            color_range: None,
            separate_uv_delta_q: false,
        }
    }
}

impl SequenceHeader {
    fn default_for_parse() -> Self {
        Self::default()
    }

    /// Whether show_existing_frame headers need temporal_point_info.
    pub fn needs_temporal_point_info(&self) -> bool {
        self.decoder_model_info_present && !self.equal_picture_interval
    }

    /// Constraints that must hold for us to emit show_existing_frame TUs
    /// against this sequence header.
    pub fn check_supported(&self) -> Result<()> {
        if self.reduced_still_picture_header {
            bail!("reduced_still_picture_header streams cannot use show_existing_frame");
        }
        if self.frame_id_numbers_present {
            bail!("frame_id_numbers_present streams are not yet supported");
        }
        if self.needs_temporal_point_info() {
            bail!("decoder model with unequal picture intervals is not yet supported");
        }
        if self.film_grain_params_present {
            bail!("film_grain_params_present streams are not supported: grain params would need to be loaded per shown frame");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_header_roundtrip_is_byte_exact() {
        // A typical libaom-emitted header shape (no timing info).
        let mut sh = SequenceHeader {
            operating_points: vec![OperatingPoint {
                idc: 0,
                seq_level_idx: 20,
                ..Default::default()
            }],
            frame_width_bits_minus_1: 10,
            frame_height_bits_minus_1: 10,
            max_frame_width: 640,
            max_frame_height: 640,
            use_128x128_superblock: true,
            enable_filter_intra: true,
            enable_intra_edge_filter: true,
            enable_interintra_compound: true,
            enable_masked_compound: true,
            enable_warped_motion: true,
            enable_dual_filter: true,
            enable_order_hint: true,
            enable_jnt_comp: false,
            enable_ref_frame_mvs: false,
            seq_choose_screen_content_tools: true,
            seq_force_screen_content_tools: SELECT_SCREEN_CONTENT_TOOLS,
            seq_choose_integer_mv: true,
            seq_force_integer_mv: SELECT_INTEGER_MV,
            order_hint_bits: 8,
            enable_superres: false,
            enable_cdef: true,
            enable_restoration: true,
            color_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            color_range: Some(false),
            subsample_x: true,
            subsample_y: true,
            chroma_sample_position: 0,
            ..Default::default()
        };
        let bytes = emit_sequence_header(&sh);
        let reparsed = parse_sequence_header(&bytes).unwrap();
        assert_eq!(emit_sequence_header(&reparsed), bytes);
        assert!(!reparsed.timing_info_present);

        // Timing injection flips only the timing section.
        sh = with_timing_info(&reparsed, 30);
        let bytes2 = emit_sequence_header(&sh);
        let reparsed2 = parse_sequence_header(&bytes2).unwrap();
        assert_eq!(emit_sequence_header(&reparsed2), bytes2);
        assert!(reparsed2.timing_info_present);
        assert!(reparsed2.equal_picture_interval);
        assert_eq!(reparsed2.time_scale, 30);
        assert_eq!(reparsed2.num_ticks_per_picture_minus_1, 0);
    }
}
