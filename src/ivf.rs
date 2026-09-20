//! Minimal IVF demuxer/muxer. IVF packets correspond to temporal units.

use anyhow::Result;

pub struct IvfFile {
    pub width: u16,
    pub height: u16,
    /// timebase denominator (frame rate numerator as stored in IVF).
    pub timebase_den: u32,
    /// timebase numerator (frame rate denominator as stored in IVF).
    pub timebase_num: u32,
    /// (timestamp, temporal unit bytes)
    pub frames: Vec<(u64, Vec<u8>)>,
}

/// Greatest common divisor (Euclid).
pub(crate) fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

impl IvfFile {
    /// Frame rate as a reduced rational `(num, den)` in frames/second
    /// (e.g. `(30000, 1001)` for NTSC). IVF stores it directly as
    /// `timebase_den`/`timebase_num`; a zero in either field means the
    /// muxer didn't know the rate — treated as unset, defaulting to
    /// 30/1.
    pub fn rate(&self) -> (u32, u32) {
        if self.timebase_den == 0 || self.timebase_num == 0 {
            return (30, 1);
        }
        let g = gcd(u64::from(self.timebase_den), u64::from(self.timebase_num));
        (
            (u64::from(self.timebase_den) / g) as u32,
            (u64::from(self.timebase_num) / g) as u32,
        )
    }

    /// Frame rate as frames per second (`rate().0 / rate().1`).
    pub fn fps(&self) -> f64 {
        let (num, den) = self.rate();
        f64::from(num) / f64::from(den)
    }
}

pub fn read(data: &[u8]) -> Result<IvfFile> {
    anyhow::ensure!(data.len() >= 32, "IVF header truncated");
    anyhow::ensure!(&data[0..4] == b"DKIF", "not an IVF file");
    anyhow::ensure!(&data[8..12] == b"AV01", "IVF fourcc is not AV01");
    let width = u16::from_le_bytes(data[12..14].try_into().unwrap());
    let height = u16::from_le_bytes(data[14..16].try_into().unwrap());
    let den = u32::from_le_bytes(data[16..20].try_into().unwrap());
    let num = u32::from_le_bytes(data[20..24].try_into().unwrap());
    // A zero in either field means the muxer didn't know the rate —
    // normalize to the unset default so downstream never sees a
    // zero-rate timebase.
    let (timebase_den, timebase_num) = if den == 0 || num == 0 {
        (30, 1)
    } else {
        (den, num)
    };

    let mut frames = Vec::new();
    let mut off = 32usize;
    while off < data.len() {
        anyhow::ensure!(off + 12 <= data.len(), "truncated IVF frame header");
        let size = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let ts = u64::from_le_bytes(data[off + 4..off + 12].try_into().unwrap());
        off += 12;
        anyhow::ensure!(off + size <= data.len(), "truncated IVF frame payload");
        frames.push((ts, data[off..off + size].to_vec()));
        off += size;
    }
    Ok(IvfFile {
        width,
        height,
        timebase_den,
        timebase_num,
        frames,
    })
}

pub fn write(f: &IvfFile) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + f.frames.len() * 12);
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes()); // version
    out.extend_from_slice(&32u16.to_le_bytes()); // header length
    out.extend_from_slice(b"AV01");
    out.extend_from_slice(&f.width.to_le_bytes());
    out.extend_from_slice(&f.height.to_le_bytes());
    out.extend_from_slice(&f.timebase_den.to_le_bytes());
    out.extend_from_slice(&f.timebase_num.to_le_bytes());
    out.extend_from_slice(&(f.frames.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (ts, tu) in &f.frames {
        out.extend_from_slice(&(tu.len() as u32).to_le_bytes());
        out.extend_from_slice(&ts.to_le_bytes());
        out.extend_from_slice(tu);
    }
    out
}
