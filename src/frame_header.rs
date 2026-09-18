//! Minimal uncompressed-header parser (AV1 spec 5.9.2).
//!
//! Parses only what the assembler needs: frame_type / show_frame /
//! showable_frame / show_existing_frame / frame_to_show_map_idx /
//! refresh_frame_flags. Anything after refresh_frame_flags is left unread.

use anyhow::{bail, Result};

use crate::bitio::BitReader;
use crate::seq_header::SequenceHeader;

pub const KEY_FRAME: u8 = 0;
pub const INTER_FRAME: u8 = 1;
pub const INTRA_ONLY_FRAME: u8 = 2;
pub const SWITCH_FRAME: u8 = 3;

const SELECT: u8 = 2; // SELECT_SCREEN_CONTENT_TOOLS / SELECT_INTEGER_MV

#[derive(Debug, Clone)]
pub struct FrameHeaderInfo {
    pub show_existing_frame: bool,
    /// Present only when show_existing_frame is set.
    pub frame_to_show_map_idx: Option<u8>,
    /// For show_existing_frame frames this is the stored RefFrameType;
    /// callers resolve it against the tracked DPB state.
    pub frame_type: Option<u8>,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub refresh_frame_flags: u8,
}

impl FrameHeaderInfo {
    pub fn is_shown(&self) -> bool {
        self.show_frame || self.show_existing_frame
    }
}

/// Parse the uncompressed header of a frame OBU or frame header OBU payload.
/// `sh` is the sequence header governing this stream.
pub fn parse_frame_header_info(payload: &[u8], sh: &SequenceHeader) -> Result<FrameHeaderInfo> {
    anyhow::ensure!(
        !sh.reduced_still_picture_header,
        "reduced still picture header is unsupported"
    );

    let mut r = BitReader::new(payload);

    if sh.frame_id_numbers_present {
        bail!("frame_id_numbers_present streams are not supported");
    }

    let show_existing_frame = r.f(1)? == 1;
    if show_existing_frame {
        let idx = r.f(3)? as u8;
        if sh.needs_temporal_point_info() {
            bail!("decoder model with unequal picture intervals is unsupported");
        }
        // refresh_frame_flags = 0, unless the referenced frame is a KEY_FRAME
        // (resolved by the caller which tracks RefFrameType).
        return Ok(FrameHeaderInfo {
            show_existing_frame: true,
            frame_to_show_map_idx: Some(idx),
            frame_type: None,
            show_frame: false,
            showable_frame: false,
            refresh_frame_flags: 0,
        });
    }

    let frame_type = r.f(2)? as u8;
    let frame_is_intra = frame_type == INTRA_ONLY_FRAME || frame_type == KEY_FRAME;
    let show_frame = r.f(1)? == 1;
    if show_frame && sh.needs_temporal_point_info() {
        bail!("decoder model with unequal picture intervals is unsupported");
    }
    let showable_frame = if show_frame {
        frame_type != KEY_FRAME
    } else {
        r.f(1)? == 1
    };

    let error_resilient = if frame_type == SWITCH_FRAME || (frame_type == KEY_FRAME && show_frame) {
        true
    } else {
        r.f(1)? == 1
    };

    r.f(1)?; // disable_cdf_update

    let allow_screen_content_tools = if sh.seq_force_screen_content_tools == SELECT {
        r.f(1)? == 1
    } else {
        sh.seq_force_screen_content_tools == 1
    };
    if allow_screen_content_tools && sh.seq_force_integer_mv == SELECT {
        r.f(1)?; // force_integer_mv
    }

    // frame_id_numbers_present rejected above.
    if frame_type != SWITCH_FRAME && !sh.reduced_still_picture_header {
        r.f(1)?; // frame_size_override_flag
    }
    r.f(sh.order_hint_bits)?; // order_hint

    if !frame_is_intra && !error_resilient {
        r.f(3)?; // primary_ref_frame
    }

    if sh.decoder_model_info_present {
        // buffer_removal_time_present_flag, then removal times per operating
        // point. Rejected upstream by check_supported for the unequal-interval
        // case; even with equal_picture_interval removal_time fields would
        // still need parsing, so bail conservatively.
        bail!("decoder_model_info_present streams are not supported");
    }

    let refresh_frame_flags =
        if frame_type == SWITCH_FRAME || (frame_type == KEY_FRAME && show_frame) {
            0xff
        } else {
            r.f(8)? as u8
        };

    Ok(FrameHeaderInfo {
        show_existing_frame: false,
        frame_to_show_map_idx: None,
        frame_type: Some(frame_type),
        show_frame,
        showable_frame,
        refresh_frame_flags,
    })
}
