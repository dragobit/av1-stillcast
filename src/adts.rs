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
    let mut first_config: Option<(u8, u8, u8)> = None;
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
        // channel_config 0 means the layout is defined by a Program Config
        // Element inside the AAC payload (common for 5.1/7.1). There is no
        // fixed channel count to declare in the mp4 mp4a entry, so the mux
        // would emit an invalid channelcount of 0 — reject instead of
        // producing a malformed file.
        anyhow::ensure!(
            chan_cfg != 0,
            "ADTS channel_config 0 (channel layout defined in-stream by a \
             Program Config Element) is not supported; transcode to a \
             standard-layout AAC with `ffmpeg -i <input> -c:a aac -f adts out.aac`"
        );
        let frame_len = (((data[i + 3] & 0x3) as usize) << 11)
            | ((data[i + 4] as usize) << 3)
            | ((data[i + 5] as usize) >> 5);
        anyhow::ensure!(frame_len >= 7, "bad ADTS frame length {frame_len}");
        anyhow::ensure!(i + frame_len <= data.len(), "truncated ADTS frame at {i}");
        anyhow::ensure!(freq_idx != 15, "explicit ADTS sample rates unsupported");
        let rate = SAMPLE_RATES[freq_idx as usize];
        anyhow::ensure!(rate != 0, "reserved ADTS frequency index");

        match first_config {
            // The AudioSpecificConfig written into the mp4 is taken from the
            // first frame; a mid-stream change would silently mismatch it.
            Some(first) => anyhow::ensure!(
                (profile, freq_idx, chan_cfg) == first,
                "ADTS AAC config changes mid-stream at offset {i} \
                 (profile/frequency/channel_config must be constant); \
                 transcode with `ffmpeg -i <input> -c:a aac -f adts out.aac`"
            ),
            None => {
                first_config = Some((profile, freq_idx, chan_cfg));
                // AudioSpecificConfig: objectType(5) freqIdx(4) chanCfg(4) | 000
                let object_type = profile + 1;
                asc = Some([
                    (object_type << 3) | (freq_idx >> 1),
                    ((freq_idx & 1) << 7) | (chan_cfg << 3),
                ]);
                sample_rate = rate;
                channels = u16::from(chan_cfg);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one ADTS frame (protection_absent=1, AAC-LC) wrapping `payload`.
    fn adts_frame(payload: &[u8], freq_idx: u8, chan_cfg: u8) -> Vec<u8> {
        let frame_len = (7 + payload.len()) as u16;
        let mut v = vec![
            0xff,
            0xf1,
            (1 << 6) | (freq_idx << 2) | (chan_cfg >> 2),
            (chan_cfg << 6) | ((frame_len >> 11) as u8),
            (frame_len >> 3) as u8,
            (((frame_len & 0x7) << 5) | 0x1f) as u8,
            0xfc,
        ];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn rejects_channel_config_zero() {
        // chan_cfg 0 = layout lives in a Program Config Element inside the
        // payload; the mp4 mp4a entry would get an invalid channelcount 0.
        let data = adts_frame(&[0xde, 0xad], 4, 0);
        let err = parse(&data).err().unwrap().to_string();
        assert!(err.contains("channel_config 0"), "{err}");
        assert!(err.contains("ffmpeg"), "{err}");
    }

    #[test]
    fn rejects_mid_stream_config_change() {
        // chan_cfg changing between frames would leave the mp4 declaring
        // the first frame's layout for the whole track.
        let mut data = adts_frame(&[0x01], 4, 2);
        data.extend_from_slice(&adts_frame(&[0x02], 4, 6));
        let err = parse(&data).err().unwrap().to_string();
        assert!(err.contains("mid-stream"), "{err}");
    }

    #[test]
    fn parses_stereo_frame() {
        let data = adts_frame(&[0xaa; 4], 4, 2);
        let s = parse(&data).unwrap();
        assert_eq!(s.sample_rate, 44100);
        assert_eq!(s.channels, 2);
        assert_eq!(s.frames.len(), 1);
        assert_eq!(s.frames[0], [0xaa; 4]);
    }
}
