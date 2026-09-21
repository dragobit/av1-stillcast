use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// AV1 bitstream assembler for static-image videos.
///
/// Takes a short real encode (keyframe + one inter frame, e.g. 2 frames from
/// libaom) and expands it into a long stream where the static image is
/// re-displayed via show_existing_frame temporal units.
#[derive(Parser)]
#[command(name = "stillcast", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot: still image + audio -> video (mp4 or ivf).
    ///
    /// Drives ffmpeg for the 2-frame libaom encode and (when needed) the
    /// audio -> ADTS conversion, then assembles. Requires ffmpeg on PATH.
    Make {
        /// Jacket/thumbnail image (png, jpg, ...). Mutually exclusive with
        /// --playlist.
        #[arg(short, long, conflicts_with = "playlist")]
        image: Option<PathBuf>,
        /// Playlist file: one `image_path [duration]` per line (# comments,
        /// last entry may omit duration to fill --duration/audio).
        /// Duration forms: seconds | Ns | Nf (frames) | MM:SS[.mmm] | HH:MM:SS[.mmm].
        #[arg(long)]
        playlist: Option<PathBuf>,
        /// Audio track: .aac/.adts is used as-is, anything else is
        /// transcoded to AAC via ffmpeg. Ignored for .ivf output.
        #[arg(short, long)]
        audio: Option<PathBuf>,
        /// Output path (.mp4 or .ivf).
        #[arg(short, long)]
        output: PathBuf,
        /// Output frame rate.
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// Total output duration in seconds (defaults to audio duration).
        #[arg(long)]
        duration: Option<f64>,
        /// GOP size in frames: distance between keyframes = seek granularity.
        #[arg(long, default_value_t = 300, conflicts_with = "target_seek")]
        gop: u64,
        /// Alternative to --gop: pick gop = target_seek_seconds * fps.
        #[arg(long)]
        target_seek: Option<f64>,
        /// libaom constant-quality level for the real frames.
        #[arg(long, default_value_t = 32)]
        crf: u32,
        /// Max output size (e.g. 5MB, 500KB, bytes). Raises crf until it fits
        /// (keeps gop/seek granularity).
        #[arg(long)]
        max_size: Option<String>,
        /// Declare decoder_model_info() in the AV1 sequence header
        /// (rewrites the real frames' headers; costs ~1 bit/frame).
        #[arg(long)]
        decoder_model: bool,
        /// AAC bitrate when transcoding non-ADTS audio.
        #[arg(long, default_value = "96k")]
        audio_bitrate: String,
        /// Keep the intermediate src.ivf / audio.aac next to the output.
        #[arg(long)]
        keep_work: bool,
    },
    /// Expand a 2-frame encode into a long static-video AV1 bitstream.
    ///
    /// Pure coded-frames-in → elementary-stream-out transform; `-i -` reads
    /// the input (IVF / OBU / Annex-B) from stdin and `-o -` writes IVF to
    /// stdout, so it composes with ffmpeg in a shell pipeline.
    #[command(name = "expand", visible_alias = "assemble")]
    Expand {
        /// Input: IVF / OBU stream / Annex-B. Leading temporal units are
        /// scanned for a keyframe-anchor TU (seq header + shown KEY_FRAME)
        /// and the golden TU after it. `-` reads stdin. Mutually exclusive
        /// with --playlist.
        #[arg(short, long, conflicts_with = "playlist")]
        input: Option<PathBuf>,
        /// Playlist file: one `input [duration]` per line (# comments,
        /// last entry may omit duration to fill --duration/--frames).
        /// Each entry may be IVF, OBU, or Annex-B.
        /// Duration forms: seconds | Ns | Nf (frames) | MM:SS[.mmm] | HH:MM:SS[.mmm].
        #[arg(long)]
        playlist: Option<PathBuf>,
        /// Output path: `.ivf` / `.mp4`, or `-` for stdout (always IVF).
        #[arg(short, long)]
        output: PathBuf,
        /// Output frame rate (overrides input timebase).
        #[arg(long)]
        fps: Option<u32>,
        /// Total output duration in seconds (mutually exclusive with --frames).
        #[arg(long, conflicts_with = "frames")]
        duration: Option<f64>,
        /// Total output frame count.
        #[arg(long)]
        frames: Option<u64>,
        /// GOP size in frames: distance between keyframes = seek granularity.
        #[arg(long, default_value_t = 300, conflicts_with = "target_seek")]
        gop: u64,
        /// Alternative to --gop: pick gop = target_seek_seconds * fps.
        #[arg(long)]
        target_seek: Option<f64>,
        /// Optional audio to mux into mp4 output (.aac/.adts used as-is,
        /// anything else transcoded to AAC via ffmpeg).
        #[arg(long)]
        audio: Option<PathBuf>,
        /// Max output size (e.g. 5MB, 500KB, bytes). Raises gop until it
        /// fits (degrades seek granularity; video only gets smaller).
        #[arg(long)]
        max_size: Option<String>,
        /// Declare decoder_model_info() in the AV1 sequence header
        /// (rewrites the real frames' headers; costs ~1 bit/frame).
        #[arg(long)]
        decoder_model: bool,
    },
    /// Encode a still image to a short IVF (keyframe + golden inter frames)
    /// by driving libaom via ffmpeg. `-i -` reads the image from stdin,
    /// `-o -` writes IVF to stdout. Requires ffmpeg on PATH.
    Encode {
        /// Input image (png, jpg, ...), or `-` for stdin.
        #[arg(short, long)]
        input: PathBuf,
        /// Output IVF path, or `-` for stdout.
        #[arg(short, long)]
        output: PathBuf,
        /// Output frame rate written into the IVF timebase and the encode.
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// libaom constant-quality level.
        #[arg(long, default_value_t = 32)]
        crf: u32,
    },
    /// Size/seek frontier: project stream size and worst seek latency per gop.
    Plan {
        /// Input (same contract as expand: IVF / OBU / Annex-B), or `-` for stdin.
        #[arg(short, long)]
        input: PathBuf,
        /// Output frame rate (overrides input timebase).
        #[arg(long)]
        fps: Option<u32>,
        /// Duration to project over, in seconds.
        #[arg(long, default_value_t = 3600.0)]
        duration: f64,
    },
    /// Inspect an IVF stream. Without flags, validates an encoder-source
    /// IVF (keyframe + golden). With --verbose / --check it analyzes an
    /// assembled stillcast stream TU by TU.
    Info {
        /// Input path (.ivf/.obu/.av1b natively; .mp4 etc. via ffmpeg), or `-` for stdin.
        #[arg(short, long)]
        input: PathBuf,
        /// Per-TU dump: frame type, show_existing target, refreshed slots,
        /// sizes, plus a structure summary.
        #[arg(long)]
        verbose: bool,
        /// Assert the stillcast invariants (shown-KF start, no re-shown
        /// keyframes, every TU shown, golden is INTER+showable). Exits
        /// nonzero on violation.
        #[arg(long)]
        check: bool,
    },
}

fn resolve_gop(gop: u64, target_seek: Option<f64>, fps: f64) -> u64 {
    match target_seek {
        Some(s) => (s * fps).round().max(2.0) as u64,
        None => gop,
    }
}

fn is_stdio(p: &Path) -> bool {
    p == Path::new("-")
}

fn read_input(path: &Path) -> Result<Vec<u8>> {
    if is_stdio(path) {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .context("reading stdin")?;
        Ok(buf)
    } else {
        std::fs::read(path).with_context(|| format!("reading {}", path.display()))
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd
        .output()
        .with_context(|| format!("running {what} (is ffmpeg on PATH?)"))?;
    if !out.status.success() {
        anyhow::bail!("{what} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Encode the still image to a short ivf (keyframe + golden inter frames).
fn encode_still(image: &Path, fps: u32, crf: u32, dest: &Path) -> Result<()> {
    run(
        Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-loop",
                "1",
                "-i",
            ])
            .arg(image)
            .args(["-vf", "format=yuv420p", "-c:v", "libaom-av1", "-crf"])
            .arg(crf.to_string())
            .args(["-b:v", "0", "-cpu-used", "8", "-r"])
            .arg(fps.to_string())
            .args(["-frames:v", "4"])
            .arg(dest),
        "libaom encode",
    )
}

fn ffprobe_duration(path: &Path) -> Result<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .context("running ffprobe (is ffmpeg on PATH?)")?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .context("parsing ffprobe duration")
}

fn is_adts(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("aac") | Some("adts")
    )
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KB", b as f64 / (1 << 10) as f64)
    } else {
        format!("{b} B")
    }
}

fn is_mp4_path(path: &Path) -> bool {
    path.extension().map(|e| e == "mp4").unwrap_or(false)
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("MB") {
        (n, 1 << 20)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1 << 10)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1 << 20)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1 << 10)
    } else {
        (s, 1)
    };
    Ok((num
        .trim()
        .parse::<f64>()
        .with_context(|| format!("bad size {s}"))?
        * mult as f64) as u64)
}

/// Per-segment duration on a playlist line. `Seconds` covers ffmpeg-style
/// spellings (`12.3`, `12.3s`, `MM:SS.mmm`, `HH:MM:SS.mmm`); `Frames` is
/// the `Nf` form — exact and fps-independent.
#[derive(Debug, Clone, Copy)]
enum SegmentDur {
    Seconds(f64),
    Frames(u64),
}

/// Parse one duration token: `150f` (frames) | `12.3`/`12.3s` |
/// `MM:SS[.mmm]` | `HH:MM:SS[.mmm]`.
fn parse_segment_dur(s: &str) -> Option<SegmentDur> {
    if let Some(f) = s.strip_suffix('f') {
        return f
            .parse::<u64>()
            .ok()
            .filter(|&n| n > 0)
            .map(SegmentDur::Frames);
    }
    if let Some(t) = s.strip_suffix('s') {
        return t
            .parse::<f64>()
            .ok()
            .filter(|&v| v > 0.0)
            .map(SegmentDur::Seconds);
    }
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if !(2..=3).contains(&parts.len()) {
            return None;
        }
        let mut secs = 0.0f64;
        for p in &parts[..parts.len() - 1] {
            secs = secs * 60.0 + p.parse::<u64>().ok()? as f64;
        }
        secs += parts.last()?.parse::<f64>().ok()?;
        return (secs > 0.0).then_some(SegmentDur::Seconds(secs));
    }
    s.parse::<f64>()
        .ok()
        .filter(|&v| v > 0.0)
        .map(SegmentDur::Seconds)
}

/// One line of a playlist file: a source (image for `make`, ivf for
/// `assemble`) plus an optional duration.
struct PlaylistEntry {
    path: PathBuf,
    dur: Option<SegmentDur>,
}

/// Parse `path [duration]` lines; `#` comments and blanks skipped. Relative
/// paths resolve against the playlist's directory. Duration forms:
/// `seconds` | `Ns` | `Nf` (frames) | `MM:SS[.mmm]` | `HH:MM:SS[.mmm]`.
fn read_playlist(path: &Path) -> Result<Vec<PlaylistEntry>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading playlist {}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (p, dur) = match line.rsplit_once(char::is_whitespace) {
            Some((head, tail)) if parse_segment_dur(tail).is_some() => {
                (head.trim(), parse_segment_dur(tail))
            }
            _ => (line, None),
        };
        let mut pb = PathBuf::from(p.trim_matches('"'));
        if pb.is_relative() {
            pb = dir.join(pb);
        }
        out.push(PlaylistEntry { path: pb, dur });
    }
    anyhow::ensure!(!out.is_empty(), "empty playlist {}", path.display());
    Ok(out)
}

/// Convert per-segment durations to frame counts. Seconds entries are
/// rounded to the nearest output frame; `Nf` entries are exact. Every
/// entry needs a duration except the last, which fills the remainder of
/// `total_frames` (from --frames or total_secs*fps).
fn segment_frames(
    entries: &[PlaylistEntry],
    total_secs: Option<f64>,
    fps: f64,
) -> Result<Vec<u64>> {
    let total_frames = total_secs.map(|t| (t * fps).round() as u64);
    let mut frames = Vec::with_capacity(entries.len());
    let mut used = 0u64;
    for (i, e) in entries.iter().enumerate() {
        match e.dur {
            Some(SegmentDur::Frames(n)) => {
                frames.push(n.max(1));
                used += n.max(1);
            }
            Some(SegmentDur::Seconds(s)) => {
                let n = (s * fps).round().max(1.0) as u64;
                frames.push(n);
                used += n;
            }
            None if i == entries.len() - 1 => {
                let total = total_frames.context(
                    "last playlist entry has no duration; pass --duration/--frames or --audio",
                )?;
                anyhow::ensure!(
                    total > used,
                    "total {total} frames is shorter than the sum of earlier segments ({used}f)",
                );
                frames.push(total - used);
            }
            None => anyhow::bail!("entry {} needs a duration (only the last may omit)", i + 1),
        }
    }
    Ok(frames)
}

/// Build the output file bytes in memory (ivf or mp4) without writing.
/// `donor` supplies geometry/timebase (segment 0's IVF); `pairs` are the
/// per-segment (key TU, golden TU) pairs; `seg_frames` are segment lengths.
/// Returns (bytes, n_samples, golden_slot, total_frames).
#[allow(clippy::too_many_arguments)]
fn build_output(
    donor: &stillcast::ivf::IvfFile,
    pairs: &[(
        stillcast::assemble::TemporalUnit,
        stillcast::assemble::TemporalUnit,
    )],
    seg_frames: &[u64],
    is_mp4: bool,
    fps: Option<u32>,
    gop: u64,
    audio: Option<&Path>,
    audio_lang: Option<&str>,
    meta: Option<&stillcast::mp4::Meta>,
    decoder_model: bool,
) -> Result<(Vec<u8>, usize, u8, u64)> {
    let ivf = donor;
    // Exact rational rate (num/den fps): --fps N means (N, 1); otherwise
    // the input's timebase is kept — 30000/1001 stays 30000/1001.
    let (rate_num, rate_den) = fps
        .filter(|&f| f > 0)
        .map(|f| (f, 1))
        .unwrap_or_else(|| ivf.rate());
    let total: u64 = seg_frames.iter().sum();
    anyhow::ensure!(total >= 2, "need at least 2 output frames");

    let segs: Vec<stillcast::assemble::Segment> = pairs
        .iter()
        .zip(seg_frames)
        .map(|((k, g), &f)| stillcast::assemble::Segment {
            key_tu: k,
            golden_tu: g,
            frames: f,
        })
        .collect();
    let params = stillcast::assemble::AssembleParams {
        fps: rate_num,
        fps_den: rate_den,
        total_frames: total,
        gop_size: gop,
        decoder_model,
    };
    let out = stillcast::assemble::assemble_multi(&segs, &params)?;
    let n_samples = out.tus.len();
    let golden_slot = out.golden_slot;

    if audio.is_some() && !is_mp4 {
        anyhow::bail!("--audio requires mp4 output (-o out.mp4)");
    }

    let bytes = if is_mp4 {
        // mp4 keeps the exact rate too: timescale/sample_delta = 30000/1001
        // for NTSC sources instead of a truncated 29/1.
        let vtrack = stillcast::mp4::VideoTrack {
            samples: out.tus.into_iter().map(stillcast::mp4::Sample).collect(),
            sync_samples: out.key_samples,
            width: ivf.width,
            height: ivf.height,
            timescale: rate_num,
            sample_delta: rate_den,
            av1c: stillcast::mp4::build_av1c(&out.seq_header, &out.seq_header_obu),
        };
        let atrack = match audio {
            Some(path) => {
                let data =
                    std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                let adts = stillcast::adts::parse(&data)
                    .with_context(|| format!("parsing {}", path.display()))?;
                let total_bits: u64 = adts.frames.iter().map(|f| (f.len() + 7) as u64 * 8).sum();
                let dur_secs =
                    (adts.frames.len() as u64 * 1024) as f64 / f64::from(adts.sample_rate);
                let avg = (total_bits as f64 / dur_secs.max(1e-6)) as u32;
                Some(stillcast::mp4::AudioTrack {
                    samples: adts
                        .frames
                        .into_iter()
                        .map(stillcast::mp4::Sample)
                        .collect(),
                    audio_specific_config: adts.audio_specific_config.to_vec(),
                    sample_rate: adts.sample_rate,
                    channels: adts.channels,
                    sample_delta: 1024,
                    avg_bitrate: avg,
                    max_bitrate: avg,
                    language: audio_lang.map(str::to_string),
                })
            }
            None => None,
        };
        stillcast::mp4::write(&vtrack, atrack.as_ref(), meta)?
    } else {
        let out_ivf = stillcast::assemble::to_ivf(ivf, out.tus, fps);
        stillcast::ivf::write(&out_ivf)
    };
    Ok((bytes, n_samples, golden_slot, total))
}

fn write_output(
    output: &Path,
    bytes: &[u8],
    n_samples: usize,
    golden_slot: u8,
    gop: u64,
) -> Result<()> {
    if is_stdio(output) {
        std::io::stdout()
            .lock()
            .write_all(bytes)
            .context("writing stdout")?;
        eprintln!(
            "wrote stdout: {} frames, {} B, golden slot {}, gop {}",
            n_samples,
            bytes.len(),
            golden_slot,
            gop
        );
        return Ok(());
    }
    std::fs::write(output, bytes).with_context(|| format!("writing {}", output.display()))?;
    println!(
        "wrote {}: {} frames, {} B, golden slot {}, gop {}",
        output.display(),
        n_samples,
        bytes.len(),
        golden_slot,
        gop
    );
    Ok(())
}

fn ffprobe_out(path: &Path, entries: &str, extra: &[&str]) -> Option<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(extra)
        .args(["-show_entries", entries, "-of", "default=nw=1"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn audio_codec(path: &Path) -> Option<String> {
    ffprobe_out(path, "stream=codec_name", &["-select_streams", "a:0"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Remux/transcode audio to ADTS into `work_dir`, or return the original
/// path when it is already ADTS. AAC inputs are stream-copied — the
/// bitstream is reused verbatim, never re-encoded.
fn ensure_adts(audio: &Path, work_dir: &Path, bitrate: &str) -> Result<PathBuf> {
    if is_adts(audio) {
        return Ok(audio.to_path_buf());
    }
    let out = work_dir.join("audio.aac");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(audio);
    if audio_codec(audio).as_deref() == Some("aac") {
        cmd.args(["-c:a", "copy", "-f", "adts"]);
    } else {
        cmd.args(["-c:a", "aac", "-b:a", bitrate, "-f", "adts"]);
    }
    cmd.arg(&out);
    run(&mut cmd, "audio to adts")?;
    Ok(out)
}

/// Language tag of the first audio stream (ISO-639-2, e.g. "eng").
fn audio_language(path: &Path) -> Option<String> {
    ffprobe_out(path, "stream_tags=language", &["-select_streams", "a:0"])
        .map(|s| s.trim().trim_start_matches("TAG:language=").to_string())
        .filter(|s| !s.is_empty() && s != "und")
}

/// Format-level tags of the source container (title/artist/album/date).
fn probe_format_tags(path: &Path) -> Vec<(String, String)> {
    let Some(txt) = ffprobe_out(path, "format_tags", &[]) else {
        return Vec::new();
    };
    txt.lines()
        .filter_map(|l| l.trim().strip_prefix("TAG:"))
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_lowercase(), v.to_string()))
        .collect()
}

/// ilst cover-art type from magic bytes: 13 = JPEG, 14 = PNG.
fn image_dtype(bytes: &[u8]) -> Option<u8> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(13)
    } else if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        Some(14)
    } else {
        None
    }
}

/// Pull the attached picture (cover art) out of an audio file, if any.
fn extract_cover(audio: &Path, work_dir: &Path) -> Option<(Vec<u8>, u8)> {
    let out = work_dir.join("cover.bin");
    let ok = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(audio)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-c",
            "copy",
            "-f",
            "image2",
        ])
        .arg(&out)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&out).ok()?;
    image_dtype(&bytes).map(|t| (bytes, t))
}

/// Metadata to carry into the mp4: source tags plus cover art. Cover falls
/// back to `cover_fallback` (typically the -i still image) when the audio
/// has no attached picture.
fn probe_meta(
    audio: &Path,
    work_dir: &Path,
    cover_fallback: Option<&Path>,
) -> stillcast::mp4::Meta {
    let mut meta = stillcast::mp4::Meta::default();
    for (k, v) in probe_format_tags(audio) {
        match k.as_str() {
            "title" => meta.title = Some(v),
            "artist" => meta.artist = Some(v),
            "album" => meta.album = Some(v),
            "date" | "creation_time" if meta.date.is_none() => meta.date = Some(v),
            _ => {}
        }
    }
    meta.cover = extract_cover(audio, work_dir).or_else(|| {
        let bytes = std::fs::read(cover_fallback?).ok()?;
        image_dtype(&bytes).map(|t| (bytes, t))
    });
    meta
}

/// Load every input (IVF / OBU / Annex-B) and extract (key TU, golden TU)
/// pairs. Returns the geometry-donor input (segment 0), its detected
/// format, and the pairs.
#[allow(clippy::type_complexity)]
fn load_pairs(
    paths: &[PathBuf],
) -> Result<(
    stillcast::ivf::IvfFile,
    stillcast::container::Format,
    Vec<(
        stillcast::assemble::TemporalUnit,
        stillcast::assemble::TemporalUnit,
    )>,
)> {
    let mut donor = None;
    let mut donor_format = stillcast::container::Format::Ivf;
    let mut pairs = Vec::new();
    for p in paths {
        let data = read_input(p)?;
        let input = stillcast::container::read(&data)
            .with_context(|| format!("parsing {}", p.display()))?;
        let pair = stillcast::assemble::split_input(&input.ivf)
            .with_context(|| format!("splitting {}", p.display()))?;
        if donor.is_none() {
            donor = Some(input.ivf);
            donor_format = input.format;
        }
        pairs.push(pair);
    }
    Ok((donor.context("no inputs")?, donor_format, pairs))
}

/// Grow gop geometrically until the produced file fits `max_bytes`.
/// Returns (bytes, n_samples, golden_slot, chosen gop, fits_budget).
#[allow(clippy::too_many_arguments)]
fn fit_gop_for_size(
    donor: &stillcast::ivf::IvfFile,
    pairs: &[(
        stillcast::assemble::TemporalUnit,
        stillcast::assemble::TemporalUnit,
    )],
    seg_frames: &[u64],
    is_mp4: bool,
    fps: Option<u32>,
    gop: u64,
    audio: Option<&Path>,
    audio_lang: Option<&str>,
    meta: Option<&stillcast::mp4::Meta>,
    max_bytes: u64,
    decoder_model: bool,
) -> Result<(Vec<u8>, usize, u8, u64, bool)> {
    let mut g = gop.max(2);
    loop {
        let (bytes, n, slot, total) = build_output(
            donor,
            pairs,
            seg_frames,
            is_mp4,
            fps,
            g,
            audio,
            audio_lang,
            meta,
            decoder_model,
        )?;
        let fits = bytes.len() as u64 <= max_bytes;
        if fits || g >= total {
            return Ok((bytes, n, slot, g, fits));
        }
        g = (g.saturating_mul(2)).min(total);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Make {
            image,
            playlist,
            audio,
            output,
            fps,
            duration,
            gop,
            target_seek,
            crf,
            max_size,
            decoder_model,
            audio_bitrate,
            keep_work,
        } => {
            let entries = match (&image, &playlist) {
                (Some(img), None) => vec![PlaylistEntry {
                    path: img.clone(),
                    dur: duration.map(SegmentDur::Seconds),
                }],
                (None, Some(pl)) => read_playlist(pl)?,
                _ => anyhow::bail!("need -i <image> or --playlist <file>"),
            };

            let work = if keep_work {
                output
                    .parent()
                    .map(|p| p.join("stillcast-work"))
                    .unwrap_or_else(|| PathBuf::from("stillcast-work"))
            } else {
                std::env::temp_dir().join(format!("stillcast-{}", std::process::id()))
            };
            std::fs::create_dir_all(&work)?;

            let is_mp4 = is_mp4_path(&output);
            let aac = match &audio {
                Some(a) if is_mp4 => Some(ensure_adts(a, &work, &audio_bitrate)?),
                _ => None,
            };
            // Carry source metadata (language, tags, cover) into the mp4.
            // Cover falls back to the first still image when the audio has
            // no attached picture.
            let meta = match &audio {
                Some(a) if is_mp4 => Some(probe_meta(
                    a,
                    &work,
                    entries.first().map(|e| e.path.as_path()),
                )),
                _ => None,
            };
            let alang = match &audio {
                Some(a) if is_mp4 => audio_language(a),
                _ => None,
            };

            let total_secs = match (duration, &audio) {
                (Some(d), _) => Some(d),
                (None, Some(a)) => Some(
                    ffprobe_duration(a)
                        .context("could not probe audio duration; pass --duration")?,
                ),
                (None, None) => None,
            };
            let seg_frames = segment_frames(&entries, total_secs, f64::from(fps))?;
            let gop = resolve_gop(gop, target_seek, f64::from(fps));

            // CRF ladder when a size budget is given: first fit wins.
            let mut crfs = vec![crf];
            if max_size.is_some() {
                for c in [40u32, 48, 56, 63] {
                    if c > crf {
                        crfs.push(c);
                    }
                }
            }
            let budget: Option<u64> = max_size.map(|s| parse_size(&s)).transpose()?;
            let mut last_err: Option<anyhow::Error> = None;
            let mut chosen: Option<(Vec<u8>, usize, u8, u32)> = None; // bytes,n,slot,crf_used
            for c in &crfs {
                let mut srcs = Vec::with_capacity(entries.len());
                for (i, e) in entries.iter().enumerate() {
                    let p = work.join(format!("src{i}.ivf"));
                    encode_still(&e.path, fps, *c, &p)?;
                    srcs.push(p);
                }
                let (donor, _fmt, pairs) = load_pairs(&srcs)?;
                match build_output(
                    &donor,
                    &pairs,
                    &seg_frames,
                    is_mp4,
                    Some(fps),
                    gop,
                    aac.as_deref(),
                    alang.as_deref(),
                    meta.as_ref(),
                    decoder_model,
                ) {
                    Ok((bytes, n, slot, _)) => {
                        let fits = budget.map(|b| bytes.len() as u64 <= b).unwrap_or(true);
                        chosen = Some((bytes, n, slot, *c));
                        if fits {
                            break;
                        }
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            let (bytes, n, slot, crf_used) = match (chosen, last_err) {
                (Some(c), _) => c,
                (None, Some(e)) => return Err(e),
                (None, None) => unreachable!(),
            };
            if let (Some(b), _) = (budget, &bytes) {
                if bytes.len() as u64 > b {
                    eprintln!(
                        "warning: could not fit {}: best is {} at crf {}",
                        fmt_bytes(b),
                        fmt_bytes(bytes.len() as u64),
                        crf_used
                    );
                } else if crf_used != crf {
                    eprintln!("note: raised crf to {} to fit {}", crf_used, fmt_bytes(b));
                }
            }
            write_output(&output, &bytes, n, slot, gop)?;

            if !keep_work {
                let _ = std::fs::remove_dir_all(&work);
            }
        }
        Cmd::Expand {
            input,
            playlist,
            output,
            fps,
            duration,
            frames,
            gop,
            target_seek,
            audio,
            max_size,
            decoder_model,
        } => {
            let entries = match (&input, &playlist) {
                (Some(i), None) => vec![PlaylistEntry {
                    path: i.clone(),
                    dur: if frames.is_some() {
                        None
                    } else {
                        duration.map(SegmentDur::Seconds)
                    },
                }],
                (None, Some(pl)) => read_playlist(pl)?,
                _ => anyhow::bail!("need -i <input> or --playlist <file>"),
            };
            let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
            let (donor, donor_format, pairs) = load_pairs(&paths)?;
            // Exact rational rate (num/den fps): --fps N means (N, 1);
            // otherwise the input's timebase, e.g. 30000/1001 for NTSC.
            let (rate_num, rate_den) = fps
                .filter(|&f| f > 0)
                .map(|f| (f, 1))
                .unwrap_or_else(|| donor.rate());
            let eff_fps = f64::from(rate_num) / f64::from(rate_den);
            if donor_format != stillcast::container::Format::Ivf && fps.filter(|&f| f > 0).is_none()
            {
                eprintln!(
                    "note: {:?} input has no container timebase; using {}/{} fps \
                     (from the sequence header's timing_info, else 30/1) — \
                     pass --fps to override",
                    donor_format, rate_num, rate_den
                );
            }
            let gop = resolve_gop(gop, target_seek, eff_fps);
            let is_mp4 = is_mp4_path(&output);
            anyhow::ensure!(
                !is_stdio(&output) || audio.is_none(),
                "--audio requires mp4 output; stdout is IVF only"
            );

            // --frames counts total output; otherwise --duration (or the sum
            // of per-segment playlist durations) gives the length. A trailing
            // playlist entry without a duration fills the remainder.
            let total_secs = match (frames, duration) {
                (Some(f), _) => Some(f as f64 / eff_fps),
                (None, d) => d,
            };
            let seg_frames = segment_frames(&entries, total_secs, eff_fps)?;

            // Non-ADTS audio gets transcoded via ffmpeg into a temp dir.
            let work = std::env::temp_dir().join(format!("stillcast-{}", std::process::id()));
            let aac = match &audio {
                Some(a) if is_mp4 => {
                    std::fs::create_dir_all(&work)?;
                    Some(ensure_adts(a, &work, "96k")?)
                }
                _ => audio.clone(),
            };
            let meta = match &audio {
                Some(a) if is_mp4 => Some(probe_meta(a, &work, None)),
                _ => None,
            };
            let alang = match &audio {
                Some(a) if is_mp4 => audio_language(a),
                _ => None,
            };

            match max_size.map(|s| parse_size(&s)).transpose()? {
                Some(budget) => {
                    let (bytes, n, slot, g, fits) = fit_gop_for_size(
                        &donor,
                        &pairs,
                        &seg_frames,
                        is_mp4,
                        fps,
                        gop,
                        aac.as_deref(),
                        alang.as_deref(),
                        meta.as_ref(),
                        budget,
                        decoder_model,
                    )?;
                    if !fits {
                        eprintln!(
                            "warning: could not fit {}: smallest is {} at gop {}",
                            fmt_bytes(budget),
                            fmt_bytes(bytes.len() as u64),
                            g
                        );
                    } else if g != gop {
                        eprintln!("note: raised gop to {} to fit {}", g, fmt_bytes(budget));
                    }
                    write_output(&output, &bytes, n, slot, g)?;
                }
                None => {
                    let (bytes, n, slot, _) = build_output(
                        &donor,
                        &pairs,
                        &seg_frames,
                        is_mp4,
                        fps,
                        gop,
                        aac.as_deref(),
                        alang.as_deref(),
                        meta.as_ref(),
                        decoder_model,
                    )?;
                    write_output(&output, &bytes, n, slot, gop)?;
                }
            }
            let _ = std::fs::remove_dir_all(&work);
        }
        Cmd::Encode {
            input,
            output,
            fps,
            crf,
        } => {
            let work = std::env::temp_dir().join(format!("stillcast-enc-{}", std::process::id()));
            std::fs::create_dir_all(&work)?;
            let src = if is_stdio(&input) {
                let data = read_input(&input)?;
                let p = work.join("stdin.img");
                std::fs::write(&p, &data).context("staging stdin image")?;
                p
            } else {
                input.clone()
            };
            let (dest, staged) = if is_stdio(&output) {
                (work.join("out.ivf"), true)
            } else {
                (output.clone(), false)
            };
            encode_still(&src, fps, crf, &dest)?;
            if staged {
                let bytes =
                    std::fs::read(&dest).with_context(|| format!("reading {}", dest.display()))?;
                std::io::stdout()
                    .lock()
                    .write_all(&bytes)
                    .context("writing stdout")?;
                eprintln!("wrote stdout: {} B", bytes.len());
            } else {
                let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                println!("wrote {}: {} B", dest.display(), size);
            }
            let _ = std::fs::remove_dir_all(&work);
        }
        Cmd::Plan {
            input,
            fps,
            duration,
        } => {
            let data = read_input(&input)?;
            let input_fmt =
                stillcast::container::read(&data).context("parsing input (IVF / OBU / Annex-B)")?;
            let ivf = input_fmt.ivf;
            let (key_tu, golden_tu) =
                stillcast::assemble::split_input(&ivf).context("splitting input")?;
            let (rate_num, rate_den) = fps
                .filter(|&f| f > 0)
                .map(|f| (f, 1))
                .unwrap_or_else(|| ivf.rate());
            let eff_fps = f64::from(rate_num) / f64::from(rate_den);
            // Measure one show_existing TU to get the per-frame repeat cost.
            let probe = stillcast::assemble::assemble(
                &key_tu,
                &golden_tu,
                &stillcast::assemble::AssembleParams {
                    fps: rate_num,
                    fps_den: rate_den,
                    total_frames: 300,
                    gop_size: 300,
                    decoder_model: false,
                },
            )?;
            let se_size = probe.tus.get(2).map(|t| t.len()).unwrap_or(6) as u64;
            let kf_size = key_tu.len() as u64;
            let g_size = golden_tu.len() as u64;
            let total = (duration * eff_fps).round() as u64;

            println!("input: key TU {kf_size} B, golden TU {g_size} B, repeat TU {se_size} B");
            println!("projection: {duration}s @ {rate_num}/{rate_den} fps = {total} frames");
            println!(
                "{:>8} {:>10} {:>10} {:>12}",
                "gop", "video", "kbps", "worst seek"
            );
            let mut seen = std::collections::BTreeSet::new();
            for g in [60u64, 150, 300, 600, 1200, 3600, total] {
                let g = g.clamp(2, total);
                if !seen.insert(g) {
                    continue;
                }
                let n_gops = total.div_ceil(g);
                let bytes =
                    n_gops * (kf_size + g_size) + total.saturating_sub(2 * n_gops) * se_size;
                let kbps = bytes as f64 * 8.0 / duration / 1000.0;
                println!(
                    "{:>8} {:>10} {:>10.1} {:>11.1}s",
                    g,
                    fmt_bytes(bytes),
                    kbps,
                    g as f64 / eff_fps
                );
            }
            println!("(elementary stream; container adds ~12 B/frame ivf, ~4 B/frame mp4)");
            println!("(--target-seek N picks gop = N*fps directly in make/assemble)");
        }
        Cmd::Info {
            input,
            verbose,
            check,
        } => {
            let data = read_input(&input)?;
            // Native containers (IVF / OBU / Annex-B) parse directly;
            // anything else (mp4, mkv, ...) is demuxed to IVF via ffmpeg.
            let ivf = match stillcast::container::read(&data) {
                Ok(inp) => inp.ivf,
                Err(native_err) => {
                    // stdin input has no path for ffmpeg, so stage it.
                    let staged;
                    let src = if is_stdio(&input) {
                        staged = std::env::temp_dir().join("stillcast-info-src.mp4");
                        std::fs::write(&staged, &data).context("staging stdin")?;
                        &staged
                    } else {
                        &input
                    };
                    let tmp = std::env::temp_dir().join("stillcast-info.ivf");
                    run(
                        Command::new("ffmpeg")
                            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                            .arg(src)
                            .args(["-map", "0:v:0", "-c:v", "copy", "-f", "ivf"])
                            .arg(&tmp),
                        "ffmpeg demux to ivf",
                    )
                    .with_context(|| format!("{native_err:#}"))?;
                    let demuxed = std::fs::read(&tmp).context("reading demuxed ivf")?;
                    stillcast::ivf::read(&demuxed).context("parsing demuxed IVF")?
                }
            };
            if verbose || check {
                let rep = stillcast::inspect::analyze(&ivf).context("analyzing stream")?;
                let sh = &rep.seq_header;
                println!(
                    "seq: {}x{} decoder_model={} equal_interval={} order_hint_bits={}",
                    ivf.width,
                    ivf.height,
                    sh.decoder_model_info_present,
                    sh.equal_picture_interval,
                    sh.order_hint_bits
                );
                if verbose {
                    for tu in &rep.tus {
                        println!(
                            "tu {:>6} ts={:>6} {:>6}B {}{}",
                            tu.index,
                            tu.timestamp,
                            tu.bytes,
                            if tu.has_seq_header { "seq+" } else { "" },
                            stillcast::inspect::describe_kind(&tu.kind)
                        );
                    }
                }
                println!(
                    "summary: {} TUs | {} shown keyframes (seek points) at {:?} | {} coded non-key | {} show_existing",
                    rep.tus.len(),
                    rep.key_tus.len(),
                    rep.key_tus,
                    rep.golden_tus.len(),
                    rep.show_existing_tus.len()
                );
                if check {
                    let mut failed = 0usize;
                    for (name, ok, detail) in stillcast::inspect::checks(&rep) {
                        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
                        if !ok {
                            failed += 1;
                        }
                    }
                    if failed > 0 {
                        anyhow::bail!("{failed} invariant(s) violated");
                    }
                }
                return Ok(());
            }
            let (key_tu, golden_tu) =
                stillcast::assemble::split_input(&ivf).context("splitting input")?;
            let params = stillcast::assemble::AssembleParams {
                fps: 30,
                fps_den: 1,
                total_frames: 2,
                gop_size: 2,
                decoder_model: false,
            };
            match stillcast::assemble::assemble(&key_tu, &golden_tu, &params) {
                Ok(o) => println!(
                    "input OK: {} frames, golden slot {}",
                    ivf.frames.len(),
                    o.golden_slot
                ),
                Err(e) => println!("input not usable: {e:#}"),
            }
        }
    }
    Ok(())
}
