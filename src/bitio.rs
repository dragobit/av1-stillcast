//! Big-endian bit-level reader/writer for AV1 bitstream syntax.

#[derive(Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next bit position (0 = MSB of first byte).
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    /// f(n): read n bits as a fixed-length unsigned integer.
    pub fn f(&mut self, n: usize) -> anyhow::Result<u64> {
        anyhow::ensure!(n <= 64, "f(n) with n > 64");
        anyhow::ensure!(
            self.pos + n <= self.data.len() * 8,
            "unexpected end of OBU payload"
        );
        let mut v = 0u64;
        for _ in 0..n {
            let byte = self.data[self.pos >> 3];
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | u64::from(bit);
            self.pos += 1;
        }
        Ok(v)
    }

    /// uvlc(): unsigned variable-length code.
    pub fn uvlc(&mut self) -> anyhow::Result<u64> {
        let mut leading_zeros = 0usize;
        loop {
            anyhow::ensure!(leading_zeros < 32, "uvlc value too large");
            if self.f(1)? == 1 {
                break;
            }
            leading_zeros += 1;
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        Ok((1u64 << leading_zeros) - 1 + self.f(leading_zeros)?)
    }

    /// leb128(): little-endian base-128 unsigned integer (used for OBU sizes).
    pub fn leb128(data: &[u8]) -> anyhow::Result<(u64, usize)> {
        let mut val = 0u64;
        for (i, &b) in data.iter().enumerate() {
            anyhow::ensure!(i < 8, "leb128 too long");
            val |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok((val, i + 1));
            }
        }
        anyhow::bail!("unterminated leb128")
    }
}

#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    bit_pos: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append the low `n` bits of `v`, MSB first.
    pub fn f(&mut self, n: usize, v: u64) {
        for i in (0..n).rev() {
            if self.bit_pos == 0 {
                self.bytes.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            let last = self.bytes.last_mut().unwrap();
            *last |= bit << (7 - self.bit_pos);
            self.bit_pos = (self.bit_pos + 1) & 7;
        }
    }

    /// uvlc(): unsigned variable-length code — leading_zeros 0-bits, a 1,
    /// then a leading_zeros-bit suffix.
    pub fn uvlc(&mut self, v: u64) {
        let leading_zeros = 63 - (v + 1).leading_zeros() as usize;
        for _ in 0..leading_zeros {
            self.f(1, 0);
        }
        self.f(1, 1);
        if leading_zeros > 0 {
            self.f(leading_zeros, (v + 1) & ((1u64 << leading_zeros) - 1));
        }
    }

    /// AV1 trailing_bits: a single 1 bit followed by zero padding to a byte boundary.
    pub fn trailing_bits(&mut self) {
        self.f(1, 1);
        while self.bit_pos != 0 {
            self.f(1, 0);
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Encode a value as leb128.
pub fn leb128_encode(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_bits() {
        let mut w = BitWriter::new();
        w.f(1, 1);
        w.f(3, 0b001);
        w.trailing_bits();
        let bytes = w.into_bytes();
        assert_eq!(bytes, vec![0b1001_1000]);

        let mut r = BitReader::new(&bytes);
        assert_eq!(r.f(1).unwrap(), 1);
        assert_eq!(r.f(3).unwrap(), 1);
    }

    #[test]
    fn leb128_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64] {
            let enc = leb128_encode(v);
            let (dec, len) = BitReader::leb128(&enc).unwrap();
            assert_eq!(dec, v);
            assert_eq!(len, enc.len());
        }
    }
}
