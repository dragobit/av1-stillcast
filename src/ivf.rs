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
    let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
    anyhow::ensure!(version == 0, "unsupported IVF version {version}");
    // Packets start at the declared header length, not at 32: muxers may
    // pad the header, and skipping the extension is what keeps the first
    // packet's framing intact.
    let header_len = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
    anyhow::ensure!(
        header_len >= 32,
        "IVF header length {header_len} is smaller than the fixed 32-byte header"
    );
    anyhow::ensure!(
        header_len <= data.len(),
        "IVF header length {header_len} exceeds file length {}",
        data.len()
    );
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
    let mut off = header_len;
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

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_IVF: &[u8] = include_bytes!("../tests/fixtures/src.ivf");

    /// Re-declare the header length and pad the file so packets start
    /// there (the fixed header stays 32 bytes; the extension is zeroed).
    fn with_header_len(data: &[u8], header_len: u16) -> Vec<u8> {
        let mut out = data.to_vec();
        out[6..8].copy_from_slice(&header_len.to_le_bytes());
        if header_len as usize > 32 {
            let pad = vec![0u8; header_len as usize - 32];
            out.splice(32..32, pad);
        }
        out
    }

    #[test]
    fn extended_header_is_honored() {
        let base = read(SRC_IVF).unwrap();
        let padded = read(&with_header_len(SRC_IVF, 64)).unwrap();
        assert_eq!(padded.frames, base.frames);
        assert_eq!((padded.width, padded.height), (base.width, base.height));
        assert_eq!(
            (padded.timebase_den, padded.timebase_num),
            (base.timebase_den, base.timebase_num)
        );
    }

    #[test]
    fn rejects_bad_header_len_and_version() {
        let mut bad = SRC_IVF.to_vec();
        bad[6..8].copy_from_slice(&16u16.to_le_bytes());
        let err = read(&bad).err().expect("read succeeded").to_string();
        assert!(err.contains("header length"), "{err}");

        let mut bad = SRC_IVF.to_vec();
        bad[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
        let err = read(&bad).err().expect("read succeeded").to_string();
        assert!(err.contains("exceeds"), "{err}");

        let mut bad = SRC_IVF.to_vec();
        bad[4..6].copy_from_slice(&1u16.to_le_bytes());
        let err = read(&bad).err().expect("read succeeded").to_string();
        assert!(err.contains("version"), "{err}");
    }
}
