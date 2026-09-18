//! Open Bitstream Unit (OBU) parsing and writing (AV1 spec section 5.3).

use anyhow::{bail, Context, Result};

use crate::bitio::{leb128_encode, BitReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObuType {
    Reserved0 = 0,
    SequenceHeader = 1,
    TemporalDelimiter = 2,
    FrameHeader = 3,
    TileGroup = 4,
    Metadata = 5,
    Frame = 6,
    RedundantFrameHeader = 7,
    TileList = 8,
    Padding = 15,
}

impl ObuType {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => ObuType::Reserved0,
            1 => ObuType::SequenceHeader,
            2 => ObuType::TemporalDelimiter,
            3 => ObuType::FrameHeader,
            4 => ObuType::TileGroup,
            5 => ObuType::Metadata,
            6 => ObuType::Frame,
            7 => ObuType::RedundantFrameHeader,
            8 => ObuType::TileList,
            15 => ObuType::Padding,
            other => bail!("unknown OBU type {other}"),
        })
    }
}

/// A single OBU: header fields + payload bytes (size prefix excluded).
#[derive(Debug, Clone)]
pub struct Obu {
    pub obu_type: ObuType,
    /// temporal_id (3 bits) | spatial_id (2 bits) | 3 reserved bits, when present.
    pub extension: Option<u8>,
    pub payload: Vec<u8>,
}

impl Obu {
    /// Serialize including the 1-byte header, optional extension byte and leb128 size.
    /// `has_size` controls obu_has_size_field; streams we emit always set it.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut header = (self.obu_type as u8) << 3;
        header |= 1 << 1; // obu_has_size_field
        if self.extension.is_some() {
            header |= 1 << 2;
        }
        out.push(header);
        if let Some(ext) = self.extension {
            out.push(ext);
        }
        out.extend_from_slice(&leb128_encode(self.payload.len() as u64));
        out.extend_from_slice(&self.payload);
    }

    pub fn temporal_delimiter() -> Self {
        Obu {
            obu_type: ObuType::TemporalDelimiter,
            extension: None,
            payload: Vec::new(),
        }
    }
}

/// Split a byte stream (a temporal unit / IVF packet) into OBUs.
/// OBUs must carry a size field; if the last OBU lacks it, it runs to the end.
pub fn parse_obus(data: &[u8]) -> Result<Vec<Obu>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let header = data[i];
        i += 1;
        anyhow::ensure!(header & 0x80 == 0, "obu_forbidden_bit set");
        let obu_type = ObuType::from_u8((header >> 3) & 0x0f)?;
        let has_ext = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let extension = if has_ext {
            let e = *data.get(i).context("truncated OBU extension")?;
            i += 1;
            Some(e)
        } else {
            None
        };
        let size = if has_size {
            let (v, n) = BitReader::leb128(&data[i..]).context("bad leb128 OBU size")?;
            i += n;
            v as usize
        } else {
            data.len() - i
        };
        anyhow::ensure!(i + size <= data.len(), "OBU payload overruns packet");
        out.push(Obu {
            obu_type,
            extension,
            payload: data[i..i + size].to_vec(),
        });
        i += size;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_then_write_roundtrip() {
        let mut buf = Vec::new();
        Obu::temporal_delimiter().write(&mut buf);
        Obu {
            obu_type: ObuType::SequenceHeader,
            extension: None,
            payload: vec![0xde, 0xad, 0xbe, 0xef],
        }
        .write(&mut buf);

        let obus = parse_obus(&buf).unwrap();
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].obu_type, ObuType::TemporalDelimiter);
        assert_eq!(obus[1].payload, vec![0xde, 0xad, 0xbe, 0xef]);
    }
}
