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
            let (tus, slot) = stillcast::assemble::assemble(&key_tu, &golden_tu, &params)?;
            let out_ivf = stillcast::assemble::to_ivf(&ivf, tus, fps);
            std::fs::write(&output, stillcast::ivf::write(&out_ivf))
                .with_context(|| format!("writing {}", output.display()))?;
            println!(
                "wrote {}: {} frames, {} B, golden slot {}, gop {}",
                output.display(),
                out_ivf.frames.len(),
                std::fs::metadata(&output)?.len(),
                slot,
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
                Ok((_, slot)) => println!(
                    "input OK: {} frames, golden slot {}",
                    ivf.frames.len(),
                    slot
                ),
                Err(e) => println!("input not usable: {e:#}"),
            }
        }
    }
    Ok(())
}
