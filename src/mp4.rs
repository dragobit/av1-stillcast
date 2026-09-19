//! Minimal ISOBMFF (MP4) writer for the static-video use case.
//!
//! Layout: ftyp | moov | mdat — "faststart": the sample tables sit before
//! the media data so players can start and seek without reaching the end of
//! the file. moov size is stco-independent, so we build it once to measure,
//! then rebuild with real chunk offsets — still fully deterministic.
//! Each track lives in one chunk
//! (samples are cheap show_existing TUs; interleaving buys nothing).
//! No B-frames ever exist in our streams, so decode order == presentation
//! order and no ctts is needed.

use anyhow::Result;

use crate::seq_header::SequenceHeader;

// ---------- box helpers ----------

fn bx(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(fourcc);
    v.extend_from_slice(payload);
    v
}

fn full_box(fourcc: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p.extend_from_slice(payload);
    bx(fourcc, &p)
}

fn u16be(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}
fn u32be(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

// ---------- public data ----------

/// One media sample (video: a temporal unit; audio: a raw AAC access unit).
pub struct Sample(pub Vec<u8>);

pub struct VideoTrack {
    pub samples: Vec<Sample>,
    /// 1-based sample numbers that are keyframes (goes into stss).
    pub sync_samples: Vec<u32>,
    pub width: u16,
    pub height: u16,
    /// Media timescale (ticks/sec) and constant per-sample delta.
    pub timescale: u32,
    pub sample_delta: u32,
    /// av1C box contents (already serialized).
    pub av1c: Vec<u8>,
}

pub struct AudioTrack {
    pub samples: Vec<Sample>,
    /// AAC AudioSpecificConfig (2 bytes for LC).
    pub audio_specific_config: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Constant per-sample delta in sample-rate units (1024 for AAC-LC).
    pub sample_delta: u32,
    pub avg_bitrate: u32,
    pub max_bitrate: u32,
    /// ISO-639-2 3-letter language code (e.g. "eng"); `und` when None.
    pub language: Option<String>,
}

/// Optional user-visible metadata copied from the input (iTunes-style ilst).
/// Everything absent stays absent — output remains deterministic.
#[derive(Default)]
pub struct Meta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// ISO date string, goes into ©day.
    pub date: Option<String>,
    /// Cover art bytes + ilst type: 13 = JPEG, 14 = PNG.
    pub cover: Option<(Vec<u8>, u8)>,
}

/// Pack a 3-letter ISO-639-2 code into mdhd's 15-bit field.
fn lang_bits(code: &str) -> u16 {
    let b = code.as_bytes();
    if b.len() != 3 || !b.iter().all(|c| c.is_ascii_lowercase()) {
        return 0x55c4; // und
    }
    (u16::from(b[0] - 0x60) << 10) | (u16::from(b[1] - 0x60) << 5) | u16::from(b[2] - 0x60)
}

// ---------- av1C ----------

/// Build the av1C box payload from a parsed sequence header, embedding the
/// sequence header OBU itself for decoder robustness.
pub fn build_av1c(sh: &SequenceHeader, seq_header_obu_full: &[u8]) -> Vec<u8> {
    let (high_bitdepth, twelve_bit) = match sh.bit_depth {
        8 => (0u8, 0u8),
        10 => (1, 0),
        _ => (1, 1),
    };
    let mut v = Vec::with_capacity(4 + seq_header_obu_full.len());
    v.push(0x81); // marker=1, version=1
    v.push((sh.seq_profile << 5) | (sh.seq_level_idx & 0x1f));
    v.push(
        (sh.seq_tier << 7)
            | (high_bitdepth << 6)
            | (twelve_bit << 5)
            | (u8::from(sh.mono_chrome) << 4)
            | (u8::from(sh.subsample_x) << 3)
            | (u8::from(sh.subsample_y) << 2)
            | (sh.chroma_sample_position & 0x3),
    );
    v.push(0); // initial_presentation_delay = 0, reserved
    v.extend_from_slice(seq_header_obu_full);
    v
}

// ---------- sample entry / track serialization ----------

fn av01_sample_entry(t: &VideoTrack) -> Vec<u8> {
    let mut e = Vec::with_capacity(86);
    e.extend_from_slice(&[0u8; 6]); // reserved
    e.extend_from_slice(&u16be(1)); // data_reference_index
    e.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    e.extend_from_slice(&u16be(t.width));
    e.extend_from_slice(&u16be(t.height));
    e.extend_from_slice(&u32be(0x0048_0000)); // horizresolution 72dpi
    e.extend_from_slice(&u32be(0x0048_0000)); // vertresolution 72dpi
    e.extend_from_slice(&u32be(0)); // reserved
    e.extend_from_slice(&u16be(1)); // frame_count
    let mut name = [0u8; 32];
    name[..5].copy_from_slice(b"av01\0");
    e.extend_from_slice(&name); // compressorname
    e.extend_from_slice(&u16be(0x0018)); // depth
    e.extend_from_slice(&u16be(0xffff)); // pre_defined
    e.extend_from_slice(&bx(b"av1C", &t.av1c));
    bx(b"av01", &e)
}

fn mp4a_sample_entry(t: &AudioTrack) -> Vec<u8> {
    let mut e = Vec::with_capacity(36);
    e.extend_from_slice(&[0u8; 6]);
    e.extend_from_slice(&u16be(1));
    e.extend_from_slice(&[0u8; 8]); // reserved
    e.extend_from_slice(&u16be(t.channels));
    e.extend_from_slice(&u16be(16)); // sample_size
    e.extend_from_slice(&u16be(0)); // pre_defined
    e.extend_from_slice(&u16be(0)); // reserved
    e.extend_from_slice(&u32be(t.sample_rate << 16)); // samplerate 16.16
    e.extend_from_slice(&esds_box(t));
    bx(b"mp4a", &e)
}

fn esds_box(t: &AudioTrack) -> Vec<u8> {
    // DecSpecificInfo descriptor (tag 0x05)
    let mut dsi = vec![0x05, t.audio_specific_config.len() as u8];
    dsi.extend_from_slice(&t.audio_specific_config);
    // DecoderConfigDescr (tag 0x04)
    let mut dcd = vec![0x04, (13 + dsi.len()) as u8];
    dcd.push(0x40); // objectTypeIndication: AAC
    dcd.push(0x15); // streamType=audio(5)<<5 | upStream<<1 | reserved=1 -> 0x15
    dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
    dcd.extend_from_slice(&u32be(t.max_bitrate));
    dcd.extend_from_slice(&u32be(t.avg_bitrate));
    dcd.extend_from_slice(&dsi);
    // ES_Descriptor (tag 0x03)
    let mut esd = vec![0x03, (3 + dcd.len()) as u8];
    esd.extend_from_slice(&u16be(1)); // ES_ID
    esd.push(0); // flags
    esd.extend_from_slice(&dcd);
    full_box(b"esds", 0, 0, &esd)
}

struct TrackTables {
    stts: Vec<u8>,
    stsc: Vec<u8>,
    stsz: Vec<u8>,
    stss: Option<Vec<u8>>,
}

fn tables(samples: &[Sample], sample_delta: u32, sync: Option<&[u32]>) -> TrackTables {
    let n = samples.len() as u32;

    let mut stts = Vec::new();
    stts.extend_from_slice(&u32be(1)); // entry_count
    stts.extend_from_slice(&u32be(n));
    stts.extend_from_slice(&u32be(sample_delta));

    let mut stsc = Vec::new();
    stsc.extend_from_slice(&u32be(1));
    stsc.extend_from_slice(&u32be(1)); // first_chunk
    stsc.extend_from_slice(&u32be(n)); // samples_per_chunk (one chunk)
    stsc.extend_from_slice(&u32be(1)); // sample_description_index

    let mut stsz = Vec::with_capacity(8 + 4 * samples.len());
    stsz.extend_from_slice(&u32be(0)); // sample_size (variable)
    stsz.extend_from_slice(&u32be(n));
    for s in samples {
        stsz.extend_from_slice(&u32be(s.0.len() as u32));
    }

    let stss = sync.map(|idx| {
        let mut v = Vec::with_capacity(4 + 4 * idx.len());
        v.extend_from_slice(&u32be(idx.len() as u32));
        for i in idx {
            v.extend_from_slice(&u32be(*i));
        }
        v
    });

    TrackTables {
        stts,
        stsc,
        stsz,
        stss,
    }
}

fn mvhd(duration_ms: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(100);
    p.extend_from_slice(&u32be(0)); // creation/modification epoch (deterministic)
    p.extend_from_slice(&u32be(0));
    p.extend_from_slice(&u32be(1000)); // timescale
    p.extend_from_slice(&u32be(duration_ms));
    p.extend_from_slice(&u32be(0x0001_0000)); // rate 1.0
    p.extend_from_slice(&u16be(0x0100)); // volume 1.0
    p.extend_from_slice(&[0u8; 10]); // reserved
                                     // identity matrix
    for (i, v) in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000]
        .iter()
        .enumerate()
    {
        let _ = i;
        p.extend_from_slice(&u32be(*v));
    }
    p.extend_from_slice(&[0u8; 24]); // pre_defined
    p.extend_from_slice(&u32be(3)); // next_track_id
    full_box(b"mvhd", 0, 0, &p)
}

fn tkhd(track_id: u32, duration_ms: u32, w: u32, h: u32, volume: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(84);
    p.extend_from_slice(&u32be(0)); // creation/modification
    p.extend_from_slice(&u32be(0));
    p.extend_from_slice(&u32be(track_id));
    p.extend_from_slice(&u32be(0)); // reserved
    p.extend_from_slice(&u32be(duration_ms));
    p.extend_from_slice(&[0u8; 8]); // reserved
    p.extend_from_slice(&u16be(0)); // layer
    p.extend_from_slice(&u16be(0)); // alternate_group
    p.extend_from_slice(&u16be(volume)); // volume (0 for video, 1.0 for audio)
    p.extend_from_slice(&u16be(0)); // reserved
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        p.extend_from_slice(&u32be(v));
    }
    p.extend_from_slice(&u32be(w << 16));
    p.extend_from_slice(&u32be(h << 16));
    full_box(b"tkhd", 0, 3, &p) // enabled | in_movie
}

fn mdhd(timescale: u32, duration: u64, language: Option<&str>) -> Vec<u8> {
    let mut p = Vec::with_capacity(24);
    p.extend_from_slice(&u32be(0));
    p.extend_from_slice(&u32be(0));
    p.extend_from_slice(&u32be(timescale));
    p.extend_from_slice(&u32be(duration as u32));
    p.extend_from_slice(&u16be(language.map(lang_bits).unwrap_or(0x55c4)));
    p.extend_from_slice(&u16be(0));
    full_box(b"mdhd", 0, 0, &p)
}

fn hdlr(handler: &[u8; 4], name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&u32be(0)); // pre_defined
    p.extend_from_slice(handler);
    p.extend_from_slice(&[0u8; 12]); // reserved
    p.extend_from_slice(name.as_bytes());
    p.push(0);
    full_box(b"hdlr", 0, 0, &p)
}

fn dref() -> Vec<u8> {
    let url = full_box(b"url ", 0, 1, &[]); // self-contained flag
    let mut p = Vec::new();
    p.extend_from_slice(&u32be(1));
    p.extend_from_slice(&url);
    full_box(b"dref", 0, 0, &p)
}

fn dinf() -> Vec<u8> {
    bx(b"dinf", &dref())
}

fn stbl(sample_entry: &[u8], t: &TrackTables, stco_value: u32) -> Vec<u8> {
    let mut stbl = Vec::new();
    stbl.extend_from_slice(&full_box(b"stsd", 0, 0, &{
        let mut p = u32be(1).to_vec();
        p.extend_from_slice(sample_entry);
        p
    }));
    stbl.extend_from_slice(&full_box(b"stts", 0, 0, &t.stts));
    stbl.extend_from_slice(&full_box(b"stsc", 0, 0, &t.stsc));
    stbl.extend_from_slice(&full_box(b"stsz", 0, 0, &t.stsz));
    let mut stco = Vec::new();
    stco.extend_from_slice(&u32be(1));
    stco.extend_from_slice(&u32be(stco_value));
    stbl.extend_from_slice(&full_box(b"stco", 0, 0, &stco));
    if let Some(stss) = &t.stss {
        stbl.extend_from_slice(&full_box(b"stss", 0, 0, stss));
    }
    bx(b"stbl", &stbl)
}

fn minf_video(stbl: &[u8]) -> Vec<u8> {
    // vmhd: graphicsmode=0, opcolor={0,0,0}
    let mut p = Vec::new();
    p.extend_from_slice(&u16be(0));
    p.extend_from_slice(&[0u8; 6]);
    let vmhd = full_box(b"vmhd", 0, 1, &p);
    let mut m = Vec::new();
    m.extend_from_slice(&vmhd);
    m.extend_from_slice(&dinf());
    m.extend_from_slice(stbl);
    bx(b"minf", &m)
}

fn minf_audio(stbl: &[u8]) -> Vec<u8> {
    let smhd = full_box(b"smhd", 0, 0, u16be(0).as_ref());
    let mut m = Vec::new();
    m.extend_from_slice(&smhd);
    m.extend_from_slice(&dinf());
    m.extend_from_slice(stbl);
    bx(b"minf", &m)
}

fn trak(track_id: u32, duration_ms: u32, tkhd_wh: (u32, u32), volume: u16, mdia: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&tkhd(track_id, duration_ms, tkhd_wh.0, tkhd_wh.1, volume));
    t.extend_from_slice(mdia);
    bx(b"trak", &t)
}

fn mdia(
    timescale: u32,
    media_duration: u64,
    language: Option<&str>,
    hdlr_b: Vec<u8>,
    minf: Vec<u8>,
) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&mdhd(timescale, media_duration, language));
    m.extend_from_slice(&hdlr_b);
    m.extend_from_slice(&minf);
    bx(b"mdia", &m)
}

// ---------- metadata (udta/meta/ilst) ----------

fn data_atom(dtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + payload.len());
    p.extend_from_slice(&u32be(u32::from(dtype))); // version=0, flags=type
    p.extend_from_slice(&u32be(0)); // locale
    p.extend_from_slice(payload);
    bx(b"data", &p)
}

fn ilst_item(fourcc: &[u8; 4], dtype: u8, payload: &[u8]) -> Vec<u8> {
    bx(fourcc, &data_atom(dtype, payload))
}

/// moov-level udta carrying an iTunes-style meta/ilst. Only built when at
/// least one field is present.
fn udta(meta: &Meta) -> Option<Vec<u8>> {
    let mut items = Vec::new();
    if let Some(t) = &meta.title {
        items.extend_from_slice(&ilst_item(b"\xa9nam", 1, t.as_bytes()));
    }
    if let Some(a) = &meta.artist {
        items.extend_from_slice(&ilst_item(b"\xa9ART", 1, a.as_bytes()));
    }
    if let Some(a) = &meta.album {
        items.extend_from_slice(&ilst_item(b"\xa9alb", 1, a.as_bytes()));
    }
    if let Some(d) = &meta.date {
        items.extend_from_slice(&ilst_item(b"\xa9day", 1, d.as_bytes()));
    }
    if let Some((img, dtype)) = &meta.cover {
        items.extend_from_slice(&ilst_item(b"covr", *dtype, img));
    }
    if items.is_empty() {
        return None;
    }
    let ilst = bx(b"ilst", &items);
    let mut m = Vec::new();
    m.extend_from_slice(&hdlr(b"mdir", "appl"));
    m.extend_from_slice(&ilst);
    Some(bx(b"udta", &full_box(b"meta", 0, 0, &m)))
}

/// Serialize the whole file. Video samples = temporal units in order.
/// Audio is optional; when present both tracks mux into the same mdat.
pub fn write(
    video: &VideoTrack,
    audio: Option<&AudioTrack>,
    meta: Option<&Meta>,
) -> Result<Vec<u8>> {
    anyhow::ensure!(!video.samples.is_empty(), "no video samples");

    // ftyp: isom + isom/av01/iso8/mp41 compat
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"isom");
    ftyp.extend_from_slice(&u32be(512)); // minor_version
    for b in [b"isom", b"av01", b"iso8", b"mp41"] {
        ftyp.extend_from_slice(b.as_slice());
    }
    let ftyp = bx(b"ftyp", &ftyp);

    // mdat: video samples, then audio samples
    let v_off = 0usize;
    let a_off = video.samples.iter().map(|s| s.0.len()).sum::<usize>() + v_off;
    let mut mdat_payload = Vec::with_capacity(a_off);
    for s in &video.samples {
        mdat_payload.extend_from_slice(&s.0);
    }
    if let Some(a) = audio {
        for s in &a.samples {
            mdat_payload.extend_from_slice(&s.0);
        }
    }

    // duration bookkeeping
    let v_media_dur = u64::from(video.sample_delta) * video.samples.len() as u64;
    let v_dur_ms = (v_media_dur * 1000 / u64::from(video.timescale)) as u32;
    let (a_media_dur, a_dur_ms) = match audio {
        Some(a) => {
            let d = u64::from(a.sample_delta) * a.samples.len() as u64;
            (d, (d * 1000 / u64::from(a.sample_rate)) as u32)
        }
        None => (0, 0),
    };
    let movie_dur_ms = v_dur_ms.max(a_dur_ms);

    // layout: ftyp | moov | mdat — faststart. stco doesn't depend on its own
    // values (one fixed-size entry per chunk), so build moov once to measure
    // it, then rebuild with real chunk offsets.
    let build_moov = |v_chunk_off: u32, a_chunk_off: u32| {
        let vt = tables(
            &video.samples,
            video.sample_delta,
            Some(&video.sync_samples),
        );
        let video_trak = {
            let stbl = stbl(&av01_sample_entry(video), &vt, v_chunk_off);
            let mdia = mdia(
                video.timescale,
                v_media_dur,
                None,
                hdlr(b"vide", "stillcast video"),
                minf_video(&stbl),
            );
            trak(
                1,
                movie_dur_ms,
                (video.width as u32, video.height as u32),
                0,
                &mdia,
            )
        };
        let mut moov = mvhd(movie_dur_ms);
        moov.extend_from_slice(&video_trak);
        if let Some(a) = audio {
            let at = tables(&a.samples, a.sample_delta, None);
            let stbl = stbl(&mp4a_sample_entry(a), &at, a_chunk_off);
            let mdia = mdia(
                a.sample_rate,
                a_media_dur,
                a.language.as_deref(),
                hdlr(b"soun", "stillcast audio"),
                minf_audio(&stbl),
            );
            moov.extend_from_slice(&trak(2, movie_dur_ms, (0, 0), 0x0100, &mdia));
        }
        if let Some(u) = meta.and_then(udta) {
            moov.extend_from_slice(&u);
        }
        bx(b"moov", &moov)
    };

    let moov_len = build_moov(0, 0).len();
    let mdat_data_off = ftyp.len() + moov_len + 8; // ftyp + moov + mdat header
    let moov = build_moov(
        (mdat_data_off + v_off) as u32,
        (mdat_data_off + a_off) as u32,
    );
    debug_assert_eq!(moov.len(), moov_len);

    let mut out = Vec::with_capacity(ftyp.len() + moov.len() + 8 + mdat_payload.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&(mdat_payload.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&mdat_payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_sizes() {
        let b = bx(b"free", &[1, 2, 3]);
        assert_eq!(&b[..8], &[0, 0, 0, 11, b'f', b'r', b'e', b'e']);
    }
}
