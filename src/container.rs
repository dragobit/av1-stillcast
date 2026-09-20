//! Input container detection and demuxing.
//!
//! The assembler core works on temporal-unit byte strings; this module is the
//! thin layer that turns the containers encoders actually emit into that
//! shape. Supported natively (no ffmpeg):
//!
//! - **IVF** (`DKIF` magic) — the canonical input, carries geometry + timebase.
//! - **Low-overhead OBU stream** (`.obu`, AV1 spec section 5) — what aomenc
//!   `--obu`, SVT-AV1, and WebCodecs-style chunk APIs emit.
//! - **Annex-B** (`.av1b`, AV1 spec annex B) — length-delimited temporal units,
//!   emitted by broadcast/hardware-oriented pipelines.
//!
//! MP4/ISOBMFF and WebM/MKV are *rejected* with a remux hint: parsing them
//! natively is out of scope for the core (an mp4 reader is hundreds of lines
//! plus edit-list/fmp4 edge cases). `stillcast info` additionally accepts
//! them by delegating the demux to ffmpeg.
//!
//! Non-IVF inputs carry no reliable container timebase: fps is derived from
//! the sequence header's `timing_info` when present, else defaults to 30 —
//! pass `--fps` to override.

use anyhow::{bail, Context, Result};

use crate::bitio::{leb128_encode, BitReader};
use crate::ivf::{self, IvfFile};
use crate::obu::{parse_obus, Obu, ObuType};
use crate::seq_header;
use crate::uheader::{self, Dpb};

/// Detected input container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Ivf,
    /// Low-overhead OBU stream (section 5): OBUs back to back, temporal
    /// units delimited by TemporalDelimiter OBUs.
    Obu,
    /// Annex-B: leb128-delimited temporal units of frame units of OBUs.
    AnnexB,
}

pub struct Input {
    /// Normalized view: `frames` are TU byte strings (OBUs re-serialized
    /// with size fields), geometry/timebase filled from the container or
    /// the sequence header.
    pub ivf: IvfFile,
    pub format: Format,
}

/// Detect the container and demux to an IVF-shaped input.
/// Errors carry a remux hint for unsupported or unrecognizable inputs.
pub fn read(data: &[u8]) -> Result<Input> {
    if data.starts_with(b"DKIF") {
        let mut ivf = ivf::read(data)?;
        // IVF packets bypass finish(), so canonicalize TU bytes here —
        // a mid-packet TemporalDelimiter would otherwise reach the
        // assembler as a non-compliant TU.
        for (_, tu) in &mut ivf.frames {
            *tu = canonicalize_tu(tu);
        }
        return Ok(Input {
            ivf,
            format: Format::Ivf,
        });
    }
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        bail!(
            "MP4 input is not supported by expand/plan (core stays ffmpeg-free); \
             remux with `ffmpeg -i <input> -map 0:v:0 -c:v copy -f ivf out.ivf` \
             (`stillcast info` accepts mp4 directly)"
        );
    }
    if data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        bail!(
            "WebM/MKV input is not supported; remux with \
             `ffmpeg -i <input> -map 0:v:0 -c:v copy -f ivf out.ivf`"
        );
    }

    // Section-5 OBU stream first: its structure (frame OBUs with valid
    // headers) is far easier to satisfy by accident than Annex-B's nested
    // leb128 units, so we only fall through when it can't yield a usable
    // input (>= 2 TUs, first TU carries the sequence header).
    if let Ok(tus) = split_obu_stream(data) {
        if let Ok(input) = finish(tus, Format::Obu) {
            return Ok(input);
        }
    }
    let tus = split_annexb(data)
        .context("unrecognized input: expected IVF, a low-overhead OBU stream, or Annex-B")?;
    finish(tus, Format::AnnexB)
}

/// Assemble demuxed TUs into the normalized `Input`: re-serialize every OBU
/// with an explicit size field (canonicalizes Annex-B and streams whose last
/// OBU omits it) and pull geometry/fps from the sequence header.
fn finish(tus: Vec<Vec<u8>>, format: Format) -> Result<Input> {
    anyhow::ensure!(tus.len() >= 2, "input needs at least 2 temporal units");

    let mut seq_header_found = None;
    let mut frames = Vec::with_capacity(tus.len());
    for (i, tu) in tus.iter().enumerate() {
        let obus = parse_obus(tu).with_context(|| format!("TU {i}: bad OBU framing"))?;
        let mut out = Vec::with_capacity(tu.len());
        for (j, obu) in obus.iter().enumerate() {
            if is_stray_td(j, obu) {
                continue;
            }
            obu.write(&mut out);
            if seq_header_found.is_none() && obu.obu_type == ObuType::SequenceHeader {
                seq_header_found = Some(
                    seq_header::parse_sequence_header(&obu.payload)
                        .with_context(|| format!("TU {i}: bad sequence header"))?,
                );
            }
        }
        frames.push((i as u64, out));
    }
    let sh = seq_header_found.context("input carries no sequence header OBU")?;
    let fps = if sh.timing_info_present
        && sh.equal_picture_interval
        && sh.num_units_in_display_tick > 0
    {
        let ticks_per_picture = sh
            .num_ticks_per_picture_minus_1
            .checked_add(1)
            .context("ticks per picture overflow")?;
        let frame_period = u64::from(sh.num_units_in_display_tick)
            .checked_mul(ticks_per_picture)
            .context("frame period overflow")?;
        (u64::from(sh.time_scale) / frame_period).max(1) as u32
    } else {
        30
    };

    // Geometry comes from the first TU carrying a decodable coded frame:
    // show_existing TUs carry no picture (the uncompressed-header walker
    // rejects them), frames before any sequence header can't be parsed, and
    // leading frameless TUs (metadata/padding) yield nothing. Skip those so
    // junk-leading inputs still reach `split_input`'s scan, which reports
    // the real diagnostics.
    let mut sh_run = None;
    let mut dpb = Dpb::default();
    let mut scan = None;
    let mut last_frame_err = String::new();
    'tus: for (i, tu) in frames.iter().enumerate() {
        let Ok(obus) = parse_obus(&tu.1) else {
            continue;
        };
        for obu in &obus {
            match obu.obu_type {
                ObuType::SequenceHeader => {
                    if let Ok(p) = seq_header::parse_sequence_header(&obu.payload) {
                        sh_run = Some(p);
                    }
                }
                ObuType::Frame | ObuType::FrameHeader => {
                    let Some(sh) = &sh_run else { break };
                    match crate::frame_header::parse_frame_header_info(&obu.payload, sh) {
                        Ok(info) if info.show_existing_frame => {}
                        Ok(info) => {
                            match uheader::scan_uncompressed_header(&obu.payload, sh, &mut dpb) {
                                Ok(s) => {
                                    // Prefer an intra-coded frame for
                                    // geometry — it carries its own size.
                                    // An inter scanned against an empty DPB
                                    // can read zero-sized ref slots.
                                    let intra = matches!(
                                        info.frame_type,
                                        Some(
                                            crate::frame_header::KEY_FRAME
                                                | crate::frame_header::INTRA_ONLY_FRAME
                                        )
                                    );
                                    if intra || scan.is_none() {
                                        scan = Some(s);
                                    }
                                    if intra {
                                        break 'tus;
                                    }
                                }
                                Err(e) => {
                                    last_frame_err =
                                        format!("TU {i}: bad uncompressed header: {e:#}")
                                }
                            }
                        }
                        Err(e) => {
                            last_frame_err = format!("TU {i}: frame header unreadable: {e:#}")
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    let scan = scan.with_context(|| {
        if last_frame_err.is_empty() {
            "no temporal unit carries a decodable coded frame".to_string()
        } else {
            format!("no decodable coded frame found ({last_frame_err})")
        }
    })?;
    let width = u16::try_from(scan.upscaled_width).context("frame width exceeds IVF limit")?;
    let height = u16::try_from(scan.frame_height).context("frame height exceeds IVF limit")?;

    Ok(Input {
        ivf: IvfFile {
            width,
            height,
            timebase_den: fps,
            timebase_num: 1,
            frames,
        },
        format,
    })
}

/// Split a section-5 low-overhead OBU stream into temporal units.
/// A TemporalDelimiter starts a new TU; streams without any TD are grouped
/// by coded frame (a FRAME/FRAME_HEADER OBU after a group that already has
/// one starts a new TU). All OBUs must carry size fields except possibly the
/// stream's last.
fn split_obu_stream(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut tus: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_has_frame = false;

    let mut i = 0usize;
    while i < data.len() {
        let start = i;
        let header = data[i];
        i += 1;
        anyhow::ensure!(header & 0x80 == 0, "obu_forbidden_bit set");
        let obu_type = ObuType::from_u8((header >> 3) & 0x0f)?;
        if header & 0x04 != 0 {
            anyhow::ensure!(i < data.len(), "truncated OBU extension");
            i += 1; // extension byte
        }
        let has_size = header & 0x02 != 0;
        let size = if has_size {
            let (v, n) = BitReader::leb128(&data[i..]).context("bad leb128 OBU size")?;
            i += n;
            v as usize
        } else {
            // Per spec only the last OBU of the stream may omit the size.
            data.len() - i
        };
        anyhow::ensure!(i + size <= data.len(), "OBU payload overruns stream");
        i += size;
        let bytes = &data[start..i];

        match obu_type {
            ObuType::TemporalDelimiter => {
                if !cur.is_empty() {
                    tus.push(std::mem::take(&mut cur));
                }
                cur_has_frame = false;
            }
            ObuType::Frame | ObuType::FrameHeader => {
                if cur_has_frame {
                    tus.push(std::mem::take(&mut cur));
                }
                cur_has_frame = true;
            }
            _ => {}
        }
        cur.extend_from_slice(bytes);
    }
    if !cur.is_empty() {
        tus.push(cur);
    }

    // A TU group with no coded frame (e.g. a standalone seq header before
    // the first TD) belongs to the following TU; a trailing frameless group
    // attaches to the last real TU.
    let mut merged: Vec<Vec<Obu>> = Vec::new();
    let mut pending: Vec<Obu> = Vec::new();
    for tu in tus {
        let obus = parse_obus(&tu)?;
        let has_frame = obus
            .iter()
            .any(|o| matches!(o.obu_type, ObuType::Frame | ObuType::FrameHeader));
        if has_frame {
            merged.push(concat_tu(std::mem::take(&mut pending), obus));
        } else {
            pending.extend(obus);
        }
    }
    if !pending.is_empty() {
        match merged.last_mut() {
            Some(last) => *last = concat_tu(std::mem::take(last), pending),
            None => merged.push(pending),
        }
    }
    Ok(merged
        .iter()
        .map(|obus| {
            let mut tu = Vec::new();
            for obu in obus {
                obu.write(&mut tu);
            }
            tu
        })
        .collect())
}

/// Join two adjacent TU groups into a single temporal unit. A
/// TemporalDelimiter marks a TU boundary and must be the first OBU of its
/// TU, so at most one survives: a leading TD of `front` wins, otherwise a
/// leading TD of `back` is promoted to the front. Any other TD is dropped.
/// All remaining OBUs keep their order, so a sequence header still precedes
/// the frame it governs.
fn concat_tu(front: Vec<Obu>, back: Vec<Obu>) -> Vec<Obu> {
    let mut td = None;
    let mut body = Vec::with_capacity(front.len() + back.len());
    for (i, obu) in front.iter().chain(back.iter()).enumerate() {
        if obu.obu_type != ObuType::TemporalDelimiter {
            body.push(obu.clone());
        } else if (i == 0 || i == front.len()) && td.is_none() {
            td = Some(obu.clone());
        }
    }
    if let Some(td) = td {
        body.insert(0, td);
    }
    body
}

/// A TemporalDelimiter marks a temporal-unit boundary and is meaningful
/// solely as a TU's first OBU — a TD at any deeper position is a stray.
fn is_stray_td(position: usize, obu: &Obu) -> bool {
    obu.obu_type == ObuType::TemporalDelimiter && position > 0
}

/// Canonicalize one temporal unit's OBU byte string by dropping stray
/// TemporalDelimiters. Re-serializes only when a stray TD is present;
/// input that fails OBU framing is returned unchanged — the downstream
/// scan reports the real error.
fn canonicalize_tu(tu: &[u8]) -> Vec<u8> {
    let Ok(obus) = parse_obus(tu) else {
        return tu.to_vec();
    };
    if !obus.iter().enumerate().any(|(j, o)| is_stray_td(j, o)) {
        return tu.to_vec();
    }
    let mut out = Vec::with_capacity(tu.len());
    for (j, obu) in obus.iter().enumerate() {
        if is_stray_td(j, obu) {
            continue;
        }
        obu.write(&mut out);
    }
    out
}

/// Split an Annex-B byte stream into temporal units.
/// Layout: `[leb128 temporal_unit_size] [frame_unit ...]` where each
/// frame_unit is `[leb128 frame_unit_size] [leb128 obu_size + obu ...]`.
fn split_annexb(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut tus = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let (tu_size, n) =
            BitReader::leb128(&data[i..]).context("bad Annex-B temporal_unit_size")?;
        i += n;
        let tu_size = tu_size as usize;
        anyhow::ensure!(i + tu_size <= data.len(), "truncated Annex-B temporal unit");
        let tu_end = i + tu_size;

        let mut tu = Vec::new();
        while i < tu_end {
            let (fu_size, n) =
                BitReader::leb128(&data[i..tu_end]).context("bad Annex-B frame_unit_size")?;
            i += n;
            let fu_end = i + fu_size as usize;
            anyhow::ensure!(
                fu_end <= tu_end,
                "Annex-B frame unit overruns temporal unit"
            );
            while i < fu_end {
                let (obu_size, n) =
                    BitReader::leb128(&data[i..fu_end]).context("bad Annex-B OBU size")?;
                i += n;
                let end = i + obu_size as usize;
                anyhow::ensure!(end <= fu_end, "Annex-B OBU overruns frame unit");
                // The OBU's own header stays; annexb drops the size field
                // from the OBU, so re-frame it when re-serializing.
                let raw = &data[i..end];
                let header = raw.first().context("empty Annex-B OBU")?;
                anyhow::ensure!(header & 0x80 == 0, "obu_forbidden_bit set");
                let has_ext = header & 0x04 != 0;
                let skip = 1 + usize::from(has_ext);
                anyhow::ensure!(raw.len() >= skip, "truncated Annex-B OBU header");
                Obu {
                    obu_type: ObuType::from_u8((header >> 3) & 0x0f)?,
                    extension: if has_ext { Some(raw[1]) } else { None },
                    payload: raw[skip..].to_vec(),
                }
                .write(&mut tu);
                i = end;
            }
        }
        tus.push(tu);
        i = tu_end;
    }
    anyhow::ensure!(!tus.is_empty(), "empty Annex-B stream");
    Ok(tus)
}

/// Serialize a TU list as a low-overhead OBU stream (test helper and a
/// convenience for tools that want to emit it).
pub fn write_obu_stream(tus: &[Vec<u8>]) -> Vec<u8> {
    tus.concat()
}

/// Serialize a TU list (as raw OBU byte strings) in Annex-B format.
pub fn write_annexb(tus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for tu in tus {
        // One frame unit per TU.
        let mut fu = Vec::new();
        for obu in parse_obus(tu).expect("write_annexb: invalid TU") {
            // OBU header without the size field, followed by its payload —
            // the leb128 length prefix lives in the annexb framing instead.
            let mut h = (obu.obu_type as u8) << 3;
            if obu.extension.is_some() {
                h |= 1 << 2;
            }
            let mut raw = vec![h];
            if let Some(e) = obu.extension {
                raw.push(e);
            }
            raw.extend_from_slice(&obu.payload);
            fu.extend_from_slice(&leb128_encode(raw.len() as u64));
            fu.extend_from_slice(&raw);
        }
        out.extend_from_slice(&leb128_encode(
            (leb128_encode(fu.len() as u64).len() + fu.len()) as u64,
        ));
        out.extend_from_slice(&leb128_encode(fu.len() as u64));
        out.extend_from_slice(&fu);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble;

    const SRC_IVF: &[u8] = include_bytes!("../tests/fixtures/src.ivf");

    fn ivf_packets() -> Vec<Vec<u8>> {
        ivf::read(SRC_IVF)
            .unwrap()
            .frames
            .into_iter()
            .map(|(_, tu)| tu)
            .collect()
    }

    #[test]
    fn obu_stream_roundtrips() {
        let packets = ivf_packets();
        let stream = write_obu_stream(&packets);
        let input = read(&stream).unwrap();
        assert_eq!(input.format, Format::Obu);
        assert_eq!(input.ivf.frames.len(), packets.len());
        // libaom emits size fields on every OBU, so normalization is a
        // byte-exact no-op here.
        for (i, pkt) in packets.iter().enumerate() {
            assert_eq!(&input.ivf.frames[i].1, pkt);
        }
        assert_eq!((input.ivf.width, input.ivf.height), (640, 640));
    }

    #[test]
    fn annexb_roundtrips() {
        let packets = ivf_packets();
        let stream = write_annexb(&packets);
        let input = read(&stream).unwrap();
        assert_eq!(input.format, Format::AnnexB);
        assert_eq!(input.ivf.frames.len(), packets.len());
        for (i, pkt) in packets.iter().enumerate() {
            assert_eq!(&input.ivf.frames[i].1, pkt);
        }
    }

    fn td_count(tu: &[u8]) -> usize {
        parse_obus(tu)
            .unwrap()
            .iter()
            .filter(|o| o.obu_type == ObuType::TemporalDelimiter)
            .count()
    }

    #[test]
    fn obu_stream_merge_keeps_single_leading_td() {
        // Layout encoders can emit: a frameless [TD, seq] TU followed by
        // [TD, KF] [TD, f] ... — merging the frameless group must not leave
        // the next TU's TemporalDelimiter stranded mid-TU.
        let packets = ivf_packets();
        let first = parse_obus(&packets[0]).unwrap();
        assert_eq!(first[0].obu_type, ObuType::TemporalDelimiter);
        assert_eq!(first[1].obu_type, ObuType::SequenceHeader);

        let mut stream = Vec::new();
        first[0].write(&mut stream); // TD
        first[1].write(&mut stream); // seq header alone in its own TU
        Obu::temporal_delimiter().write(&mut stream); // fresh TD opens the frame TU
        for obu in &first[2..] {
            obu.write(&mut stream);
        }
        for pkt in &packets[1..] {
            stream.extend_from_slice(pkt);
        }

        // The merge itself must produce a spec-valid TU, not just rely on
        // finish() dropping the stray delimiter.
        let tus = split_obu_stream(&stream).unwrap();
        assert_eq!(tus.len(), packets.len());
        assert_eq!(td_count(&tus[0]), 1);

        let input = read(&stream).unwrap();
        assert_eq!(input.ivf.frames.len(), packets.len());
        let obus0 = parse_obus(&input.ivf.frames[0].1).unwrap();
        assert_eq!(td_count(&input.ivf.frames[0].1), 1);
        // TD is first; the sequence header still precedes the frame it governs.
        assert_eq!(obus0[0].obu_type, ObuType::TemporalDelimiter);
        assert_eq!(obus0[1].obu_type, ObuType::SequenceHeader);
        assert!(obus0.iter().any(|o| o.obu_type == ObuType::Frame));
    }

    #[test]
    fn obu_stream_trailing_frameless_tu_keeps_single_leading_td() {
        // A trailing [TD, metadata] group merges into the last frame TU;
        // its delimiter must not end up mid-TU either.
        let packets = ivf_packets();
        let mut stream = write_obu_stream(&packets);
        Obu::temporal_delimiter().write(&mut stream);
        Obu {
            obu_type: ObuType::Metadata,
            extension: None,
            payload: vec![0xde, 0xad, 0xbe, 0xef],
        }
        .write(&mut stream);

        let tus = split_obu_stream(&stream).unwrap();
        assert_eq!(tus.len(), packets.len());
        assert_eq!(td_count(tus.last().unwrap()), 1);

        let input = read(&stream).unwrap();
        assert_eq!(input.ivf.frames.len(), packets.len());
        let last = parse_obus(&input.ivf.frames.last().unwrap().1).unwrap();
        assert_eq!(td_count(&input.ivf.frames.last().unwrap().1), 1);
        assert_eq!(last.first().unwrap().obu_type, ObuType::TemporalDelimiter);
        assert_eq!(last.last().unwrap().obu_type, ObuType::Metadata);
    }

    #[test]
    fn ivf_packet_with_mid_tu_td_is_normalized() {
        // IVF input returns before finish(), so canonicalization runs in
        // read(): splice a stray TD into packet 0 and confirm it is dropped.
        let mut ivf = ivf::read(SRC_IVF).unwrap();
        let (ts, tu) = ivf.frames[0].clone();
        let mut mutated = Vec::new();
        for obu in parse_obus(&tu).unwrap() {
            obu.write(&mut mutated);
            if obu.obu_type == ObuType::SequenceHeader {
                Obu::temporal_delimiter().write(&mut mutated); // stray mid-TU TD
            }
        }
        ivf.frames[0] = (ts, mutated);

        let input = read(&ivf::write(&ivf)).unwrap();
        assert_eq!(input.format, Format::Ivf);
        assert_eq!(input.ivf.frames.len(), ivf.frames.len());
        assert_eq!(td_count(&input.ivf.frames[0].1), 1);
        let obus0 = parse_obus(&input.ivf.frames[0].1).unwrap();
        assert_eq!(obus0[0].obu_type, ObuType::TemporalDelimiter);
        assert_eq!(obus0[1].obu_type, ObuType::SequenceHeader);
    }

    #[test]
    fn obu_stream_without_temporal_delimiters() {
        // Strip every TD OBU; grouping must fall back to frame boundaries.
        let packets = ivf_packets();
        let mut stream = Vec::new();
        for pkt in &packets {
            for obu in parse_obus(pkt).unwrap() {
                if obu.obu_type != ObuType::TemporalDelimiter {
                    obu.write(&mut stream);
                }
            }
        }
        let input = read(&stream).unwrap();
        assert_eq!(input.ivf.frames.len(), packets.len());
        let (key, golden) = assemble::split_input(&input.ivf).unwrap();
        assert!(tu_contains(&key, ObuType::SequenceHeader));
        assert!(!golden.is_empty());
    }

    fn tu_contains(tu: &[u8], t: ObuType) -> bool {
        parse_obus(tu).unwrap().iter().any(|o| o.obu_type == t)
    }

    #[test]
    fn rejects_mp4_and_ebml_and_garbage() {
        let mut mp4ish = vec![0u8; 4];
        mp4ish.extend_from_slice(b"ftypisom");
        mp4ish.extend_from_slice(&[0u8; 32]);
        let err = read(&mp4ish).err().expect("mp4 accepted");
        assert!(err.to_string().contains("MP4"));

        let ebml = [0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0];
        let err = read(&ebml).err().expect("ebml accepted");
        assert!(err.to_string().contains("WebM"));

        assert!(read(b"definitely not a bitstream").is_err());
    }
}
