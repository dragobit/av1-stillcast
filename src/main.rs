use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
        /// Jacket/thumbnail image (png, jpg, ...).
        #[arg(short, long)]
        image: PathBuf,
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
        /// AAC bitrate when transcoding non-ADTS audio.
        #[arg(long, default_value = "96k")]
        audio_bitrate: String,
        /// Keep the intermediate src.ivf / audio.aac next to the output.
        #[arg(long)]
        keep_work: bool,
    },
    /// Assemble a long static-video AV1 bitstream from a 2-frame IVF encode.
    Assemble {
        /// Input IVF: >=2 frames; frame 0 = keyframe, frame 1 = golden (inter).
        #[arg(short, long)]
        input: PathBuf,
        /// Output IVF path.
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
    },
    /// Size/seek frontier: project stream size and worst seek latency per gop.
    Plan {
        /// Input IVF (same contract as assemble).
        #[arg(short, long)]
        input: PathBuf,
        /// Output frame rate (overrides input timebase).
        #[arg(long)]
        fps: Option<u32>,
        /// Duration to project over, in seconds.
        #[arg(long, default_value_t = 3600.0)]
        duration: f64,
    },
    /// Inspect IVF input and report key/golden TUs and the golden slot.
    Info {
        #[arg(short, long)]
        input: PathBuf,
    },
}

fn resolve_gop(gop: u64, target_seek: Option<f64>, fps: u32) -> u64 {
    match target_seek {
        Some(s) => (s * f64::from(fps)).round().max(2.0) as u64,
        None => gop,
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

/// Build the output file bytes in memory (ivf or mp4) without writing.
/// Returns (bytes, n_samples, golden_slot, eff_fps, total_frames).
fn build_output(
    input: &Path,
    is_mp4: bool,
    fps: Option<u32>,
    duration: Option<f64>,
    frames: Option<u64>,
    gop: u64,
    audio: Option<&Path>,
) -> Result<(Vec<u8>, usize, u8, u32, u64)> {
    let data = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let ivf = stillcast::ivf::read(&data).context("parsing input IVF")?;
    let (key_tu, golden_tu) = stillcast::assemble::split_input(&ivf).context("splitting input")?;

    let eff_fps = fps.unwrap_or_else(|| {
        ivf.timebase_den
            .checked_div(ivf.timebase_num.max(1))
            .unwrap_or(30)
            .max(1)
    });
    let total = if let Some(f) = frames {
        f
    } else if let Some(d) = duration {
        (d * f64::from(eff_fps)).round() as u64
    } else {
        anyhow::bail!("specify --duration or --frames");
    };

    let params = stillcast::assemble::AssembleParams {
        fps: eff_fps,
        total_frames: total,
        gop_size: gop,
    };
    let out = stillcast::assemble::assemble(&key_tu, &golden_tu, &params)?;
    let n_samples = out.tus.len();
    let golden_slot = out.golden_slot;

    if audio.is_some() && !is_mp4 {
        anyhow::bail!("--audio requires mp4 output (-o out.mp4)");
    }

    let bytes = if is_mp4 {
        let vtrack = stillcast::mp4::VideoTrack {
            samples: out.tus.into_iter().map(stillcast::mp4::Sample).collect(),
            sync_samples: out.key_samples,
            width: ivf.width,
            height: ivf.height,
            timescale: eff_fps,
            sample_delta: 1,
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
                })
            }
            None => None,
        };
        stillcast::mp4::write(&vtrack, atrack.as_ref())?
    } else {
        let out_ivf = stillcast::assemble::to_ivf(&ivf, out.tus, fps);
        stillcast::ivf::write(&out_ivf)
    };
    Ok((bytes, n_samples, golden_slot, eff_fps, total))
}

fn write_output(
    output: &Path,
    bytes: &[u8],
    n_samples: usize,
    golden_slot: u8,
    gop: u64,
) -> Result<()> {
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

/// Transcode audio to ADTS via ffmpeg into `work_dir`, or return the
/// original path when it is already ADTS.
fn ensure_adts(audio: &Path, work_dir: &Path, bitrate: &str) -> Result<PathBuf> {
    if is_adts(audio) {
        return Ok(audio.to_path_buf());
    }
    let out = work_dir.join("audio.aac");
    run(
        Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(audio)
            .args(["-c:a", "aac", "-b:a", bitrate, "-f", "adts"])
            .arg(&out),
        "audio transcode",
    )?;
    Ok(out)
}

/// Grow gop geometrically until the produced file fits `max_bytes`.
/// Returns (bytes, n_samples, golden_slot, chosen gop, fits_budget).
#[allow(clippy::too_many_arguments)]
fn fit_gop_for_size(
    input: &Path,
    is_mp4: bool,
    fps: Option<u32>,
    duration: Option<f64>,
    frames: Option<u64>,
    gop: u64,
    audio: Option<&Path>,
    max_bytes: u64,
) -> Result<(Vec<u8>, usize, u8, u64, bool)> {
    let mut g = gop.max(2);
    loop {
        let (bytes, n, slot, _fps, total) =
            build_output(input, is_mp4, fps, duration, frames, g, audio)?;
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
            audio,
            output,
            fps,
            duration,
            gop,
            target_seek,
            crf,
            max_size,
            audio_bitrate,
            keep_work,
        } => {
            let work = if keep_work {
                output
                    .parent()
                    .map(|p| p.join("stillcast-work"))
                    .unwrap_or_else(|| PathBuf::from("stillcast-work"))
            } else {
                std::env::temp_dir().join(format!("stillcast-{}", std::process::id()))
            };
            std::fs::create_dir_all(&work)?;
            let src_ivf = work.join("src.ivf");

            let is_mp4 = is_mp4_path(&output);
            let aac = match &audio {
                Some(a) if is_mp4 => Some(ensure_adts(a, &work, &audio_bitrate)?),
                _ => None,
            };

            let duration = match (duration, &audio) {
                (Some(d), _) => Some(d),
                (None, Some(a)) => Some(
                    ffprobe_duration(a)
                        .context("could not probe audio duration; pass --duration")?,
                ),
                (None, None) => anyhow::bail!("no --audio: pass --duration or --frames"),
            };
            let gop = resolve_gop(gop, target_seek, fps);

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
                encode_still(&image, fps, *c, &src_ivf)?;
                match build_output(
                    &src_ivf,
                    is_mp4,
                    Some(fps),
                    duration,
                    None,
                    gop,
                    aac.as_deref(),
                ) {
                    Ok((bytes, n, slot, _, _)) => {
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
        Cmd::Assemble {
            input,
            output,
            fps,
            duration,
            frames,
            gop,
            target_seek,
            audio,
            max_size,
        } => {
            let eff_fps = fps.unwrap_or(30);
            let gop = resolve_gop(gop, target_seek, eff_fps);
            let is_mp4 = is_mp4_path(&output);

            // Non-ADTS audio gets transcoded via ffmpeg into a temp dir.
            let work = std::env::temp_dir().join(format!("stillcast-{}", std::process::id()));
            let aac = match &audio {
                Some(a) if is_mp4 => {
                    std::fs::create_dir_all(&work)?;
                    Some(ensure_adts(a, &work, "96k")?)
                }
                _ => audio.clone(),
            };

            match max_size.map(|s| parse_size(&s)).transpose()? {
                Some(budget) => {
                    let (bytes, n, slot, g, fits) = fit_gop_for_size(
                        &input,
                        is_mp4,
                        fps,
                        duration,
                        frames,
                        gop,
                        aac.as_deref(),
                        budget,
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
                    let (bytes, n, slot, _, _) =
                        build_output(&input, is_mp4, fps, duration, frames, gop, aac.as_deref())?;
                    write_output(&output, &bytes, n, slot, gop)?;
                }
            }
            let _ = std::fs::remove_dir_all(&work);
        }
        Cmd::Plan {
            input,
            fps,
            duration,
        } => {
            let data =
                std::fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
            let ivf = stillcast::ivf::read(&data).context("parsing input IVF")?;
            let (key_tu, golden_tu) =
                stillcast::assemble::split_input(&ivf).context("splitting input")?;
            let eff_fps = fps.unwrap_or_else(|| {
                ivf.timebase_den
                    .checked_div(ivf.timebase_num.max(1))
                    .unwrap_or(30)
                    .max(1)
            });
            // Measure one show_existing TU to get the per-frame repeat cost.
            let probe = stillcast::assemble::assemble(
                &key_tu,
                &golden_tu,
                &stillcast::assemble::AssembleParams {
                    fps: eff_fps,
                    total_frames: 300,
                    gop_size: 300,
                },
            )?;
            let se_size = probe.tus.get(2).map(|t| t.len()).unwrap_or(6) as u64;
            let kf_size = key_tu.len() as u64;
            let g_size = golden_tu.len() as u64;
            let total = (duration * f64::from(eff_fps)).round() as u64;

            println!("input: key TU {kf_size} B, golden TU {g_size} B, repeat TU {se_size} B");
            println!("projection: {duration}s @ {eff_fps}fps = {total} frames");
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
                    g as f64 / f64::from(eff_fps)
                );
            }
            println!("(elementary stream; container adds ~12 B/frame ivf, ~4 B/frame mp4)");
            println!("(--target-seek N picks gop = N*fps directly in make/assemble)");
        }
        Cmd::Info { input } => {
            let data =
                std::fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
            let ivf = stillcast::ivf::read(&data).context("parsing input IVF")?;
            let (key_tu, golden_tu) =
                stillcast::assemble::split_input(&ivf).context("splitting input")?;
            let params = stillcast::assemble::AssembleParams {
                fps: 30,
                total_frames: 2,
                gop_size: 2,
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
