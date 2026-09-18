use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
        #[arg(long, default_value_t = 300)]
        gop: u64,
        /// Optional ADTS (.aac) audio to mux into mp4 output.
        #[arg(long)]
        audio: Option<PathBuf>,
    },
    /// Inspect IVF input and report key/golden TUs and the golden slot.
    Info {
        #[arg(short, long)]
        input: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Assemble {
            input,
            output,
            fps,
            duration,
            frames,
            gop,
            audio,
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

            let is_mp4 = output.extension().map(|e| e == "mp4").unwrap_or(false);
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
                        let data = std::fs::read(&path)
                            .with_context(|| format!("reading {}", path.display()))?;
                        let adts = stillcast::adts::parse(&data)
                            .with_context(|| format!("parsing {}", path.display()))?;
                        let total_bits: u64 =
                            adts.frames.iter().map(|f| (f.len() + 7) as u64 * 8).sum();
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
            std::fs::write(&output, &bytes)
                .with_context(|| format!("writing {}", output.display()))?;
            println!(
                "wrote {}: {} frames, {} B, golden slot {}, gop {}",
                output.display(),
                n_samples,
                bytes.len(),
                out.golden_slot,
                gop
            );
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
