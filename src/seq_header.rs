//! Sequence header OBU parser (AV1 spec 5.5).
//!
//! We only need the fields that govern uncompressed-header parsing and
//! show_existing_frame generation, but the syntax is sequential so we walk
//! everything up to film_grain_params_present.

use anyhow::{bail, Result};

use crate::bitio::BitReader;

const SELECT_SCREEN_CONTENT_TOOLS: u8 = 2;
const SELECT_INTEGER_MV: u8 = 2;

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
}

fn timing_info(r: &mut BitReader, sh: &mut SequenceHeader) -> Result<()> {
    r.f(32)?; // num_units_in_display_tick
    r.f(32)?; // time_scale
    sh.equal_picture_interval = r.f(1)? == 1;
    if sh.equal_picture_interval {
        r.uvlc()?; // num_ticks_per_picture_minus_1
    }
    Ok(())
}

fn decoder_model_info(r: &mut BitReader) -> Result<usize> {
    let buffer_delay_len = (r.f(5)? + 1) as usize;
    r.f(32)?; // num_units_in_decoding_tick
    r.f(5)?; // buffer_removal_time_length_minus_1
    r.f(5)?; // frame_presentation_time_length_minus_1
    Ok(buffer_delay_len)
}

fn operating_parameters_info(r: &mut BitReader, buffer_delay_len: usize) -> Result<()> {
    r.f(buffer_delay_len)?; // decoder_buffer_delay
    r.f(buffer_delay_len)?; // encoder_buffer_delay
    r.f(1)?; // low_delay_mode_flag
    Ok(())
}

fn color_config(r: &mut BitReader, sh: &mut SequenceHeader) -> Result<()> {
    let high_bitdepth = r.f(1)? == 1;
    let bit_depth = if sh.seq_profile == 2 && high_bitdepth {
        if r.f(1)? == 1 {
            12
        } else {
            10
        }
    } else if high_bitdepth {
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

    let color_description_present = r.f(1)? == 1;
    let (cp, tc, mc) = if color_description_present {
        (r.f(8)? as u8, r.f(8)? as u8, r.f(8)? as u8)
    } else {
        (2u8, 2u8, 2u8) // unspecified
    };

    if mono_chrome {
        r.f(1)?; // color_range
                 // subsampling fixed at 4:0:0; chroma_sample_position not signaled
        r.f(1)?; // separate_uv_delta_q
        return Ok(());
    }

    // BT709 primaries (1), sRGB transfer (13), identity matrix (0)
    if cp == 1 && tc == 13 && mc == 0 {
        // color_range implicitly full; subsampling 4:4:4
    } else {
        r.f(1)?; // color_range
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
        if sub_x && sub_y {
            r.f(2)?; // chroma_sample_position
        }
    }
    r.f(1)?; // separate_uv_delta_q
    Ok(())
}

/// Parse a sequence header OBU payload.
pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeader> {
    let mut r = BitReader::new(payload);
    let mut sh = SequenceHeader {
        seq_profile: r.f(3)? as u8,
        still_picture: r.f(1)? == 1,
        reduced_still_picture_header: r.f(1)? == 1,
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
    };
    anyhow::ensure!(sh.seq_profile <= 2, "seq_profile > 2");

    if sh.reduced_still_picture_header {
        // seq_level_idx[0]
        r.f(5)?;
    } else {
        sh.timing_info_present = r.f(1)? == 1;
        let mut buffer_delay_len = 32usize;
        if sh.timing_info_present {
            timing_info(&mut r, &mut sh)?;
            sh.decoder_model_info_present = r.f(1)? == 1;
            if sh.decoder_model_info_present {
                buffer_delay_len = decoder_model_info(&mut r)?;
            }
        }
        let initial_display_delay_present = r.f(1)? == 1;
        let operating_points_cnt = (r.f(5)? + 1) as usize;

        for _ in 0..operating_points_cnt {
            r.f(12)?; // operating_point_idc
            let seq_level_idx = r.f(5)?;
            if seq_level_idx > 7 {
                r.f(1)?; // seq_tier
            }
            if sh.decoder_model_info_present {
                let decoder_model_present_for_this_op = r.f(1)? == 1;
                if decoder_model_present_for_this_op {
                    operating_parameters_info(&mut r, buffer_delay_len)?;
                }
            }
            if initial_display_delay_present {
                let present = r.f(1)? == 1;
                if present {
                    r.f(4)?; // initial_display_delay_minus_1
                }
            }
        }
    }

    let frame_width_bits = (r.f(4)? + 1) as usize;
    let frame_height_bits = (r.f(4)? + 1) as usize;
    sh.max_frame_width = (r.f(frame_width_bits)? + 1) as u32;
    sh.max_frame_height = (r.f(frame_height_bits)? + 1) as u32;

    if !sh.reduced_still_picture_header {
        sh.frame_id_numbers_present = r.f(1)? == 1;
        if sh.frame_id_numbers_present {
            let delta_len = (r.f(4)? + 2) as usize;
            let additional_len = (r.f(3)? + 1) as usize;
            sh.id_len = additional_len + delta_len;
        }
    }

    sh.use_128x128_superblock = r.f(1)? == 1;
    r.f(1)?; // enable_filter_intra
    r.f(1)?; // enable_intra_edge_filter

    if sh.reduced_still_picture_header {
        // fixed values per spec
        sh.enable_order_hint = false;
        sh.seq_force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS;
        sh.seq_force_integer_mv = SELECT_INTEGER_MV;
    } else {
        r.f(1)?; // enable_interintra_compound
        r.f(1)?; // enable_masked_compound
        r.f(1)?; // enable_warped_motion
        r.f(1)?; // enable_dual_filter
        sh.enable_order_hint = r.f(1)? == 1;
        if sh.enable_order_hint {
            r.f(1)?; // enable_jnt_comp
            r.f(1)?; // enable_ref_frame_mvs
        }
        let seq_choose_screen_content_tools = r.f(1)? == 1;
        sh.seq_force_screen_content_tools = if seq_choose_screen_content_tools {
            SELECT_SCREEN_CONTENT_TOOLS
        } else {
            r.f(1)? as u8
        };
        if sh.seq_force_screen_content_tools > 0 {
            let seq_choose_integer_mv = r.f(1)? == 1;
            sh.seq_force_integer_mv = if seq_choose_integer_mv {
                SELECT_INTEGER_MV
            } else {
                r.f(1)? as u8
            };
        }
        if sh.enable_order_hint {
            sh.order_hint_bits = (r.f(3)? + 1) as usize;
        }
    }

    r.f(1)?; // enable_superres
    r.f(1)?; // enable_cdef
    r.f(1)?; // enable_restoration
    color_config(&mut r, &mut sh)?;
    sh.film_grain_params_present = r.f(1)? == 1;

    Ok(sh)
}

impl SequenceHeader {
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
