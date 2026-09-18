//! Minimal ADTS (.aac) parser → raw AAC access units + AudioSpecificConfig.

use anyhow::{Context, Result};

const SAMPLE_RATES: [u32; 16] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0,
    0,
];

pub struct AdtsStream {
    /// Raw AAC access units (ADTS headers stripped).
    pub frames: Vec<Vec<u8>>,
    /// AudioSpecificConfig (2 bytes for AAC-LC).
    pub audio_specific_config: [u8; 2],
    pub sample_rate: u32,
    pub channels: u16,
}

/// Parse an ADTS stream. Only protection_absent=1 headers are supported
/// (the common case; CRC-protected frames bail).
pub fn parse(data: &[u8]) -> Result<AdtsStream> {
    let mut frames = Vec::new();
    let mut asc = None;
    let mut sample_rate = 0;
    let mut channels = 0;
    let mut i = 0usize;

    while i < data.len() {
        anyhow::ensure!(i + 7 <= data.len(), "truncated ADTS header at {i}");
        anyhow::ensure!(
            data[i] == 0xff && data[i + 1] & 0xf0 == 0xf0,
            "ADTS sync lost at offset {i} (input must be raw .aac/ADTS)"
        );
        let protection_absent = data[i + 1] & 1;
        anyhow::ensure!(protection_absent == 1, "ADTS CRC not supported");

        let profile = (data[i + 2] >> 6) & 0x3; // AAC profile minus 1
        let freq_idx = (data[i + 2] >> 2) & 0xf;
        let chan_cfg = ((data[i + 2] & 1) << 2) | (data[i + 3] >> 6);
        let frame_len = (((data[i + 3] & 0x3) as usize) << 11)
            | ((data[i + 4] as usize) << 3)
            | ((data[i + 5] as usize) >> 5);
        anyhow::ensure!(frame_len >= 7, "bad ADTS frame length {frame_len}");
        anyhow::ensure!(i + frame_len <= data.len(), "truncated ADTS frame at {i}");
        anyhow::ensure!(freq_idx != 15, "explicit ADTS sample rates unsupported");
        let rate = SAMPLE_RATES[freq_idx as usize];
        anyhow::ensure!(rate != 0, "reserved ADTS frequency index");

        if asc.is_none() {
            // AudioSpecificConfig: objectType(5) freqIdx(4) chanCfg(4) | 000
            let object_type = profile + 1;
            asc = Some([
                (object_type << 3) | (freq_idx >> 1),
                ((freq_idx & 1) << 7) | (chan_cfg << 3),
            ]);
            sample_rate = rate;
            channels = u16::from(chan_cfg);
        }

        frames.push(data[i + 7..i + frame_len].to_vec());
        i += frame_len;
    }

    anyhow::ensure!(!frames.is_empty(), "no ADTS frames found");
    Ok(AdtsStream {
        frames,
        audio_specific_config: asc.context("no frames")?,
        sample_rate,
        channels,
    })
}
