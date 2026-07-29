//! `tessera` — one binary for the build pipeline and (later) the server (SA §3: Rust is the
//! implementation language for engine, build and serving alike).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tessera_spatial::Extent;

#[derive(Parser)]
#[command(name = "tessera", about = "Tessera: a permission-masked point service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a bundle from a points file and a `(entity_id, term_id)` pairs file.
    Build {
        /// Parquet points file: `entity_id` plus either `x`/`y` or `morton`.
        #[arg(long)]
        points: PathBuf,
        /// Parquet pairs file: `(entity_id, term_id)`.
        #[arg(long)]
        pairs: PathBuf,
        /// Bundle root to create.
        #[arg(long)]
        out: PathBuf,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent)]
        extent: Extent,
        /// Slice identifier for this build's segment.
        #[arg(long = "slice")]
        slice_id: String,
        /// Keep only source rows with `entity_id < LIMIT` (a prefix of entity space).
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Verify a bundle: the read protocol (digests, manifests) plus permutation bijectivity.
    Verify {
        /// Bundle root (the directory containing `CURRENT`).
        bundle: PathBuf,
    },
    /// Serve a bundle: the three HTTP planes (viewer/session/control), per `tessera.toml`.
    Serve {
        /// Path to `tessera.toml` (SA §7).
        #[arg(short = 'c', long = "config")]
        config: PathBuf,
    },
}

fn parse_extent(raw: &str) -> Result<Extent, String> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "expected four comma-separated numbers 'x_min,x_max,y_min,y_max', got '{raw}'"
        ));
    }
    let mut values = [0f64; 4];
    for (slot, text) in values.iter_mut().zip(parts) {
        *slot = text
            .parse::<f64>()
            .map_err(|e| format!("'{text}' is not a number: {e}"))?;
    }
    let extent = Extent {
        x_min: values[0],
        x_max: values[1],
        y_min: values[2],
        y_max: values[3],
    };
    extent.validate()?;
    Ok(extent)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            points,
            pairs,
            out,
            extent,
            slice_id,
            limit,
        } => {
            let args = tessera_build::BuildArgs {
                points,
                pairs,
                out: out.clone(),
                extent,
                slice_id,
                limit,
            };
            match tessera_build::build(&args) {
                Ok(report) => {
                    println!(
                        "built {} ({}): {} items, {} terms, {} pairs, {} bytes on disk",
                        out.display(),
                        report.prefix,
                        report.items,
                        report.terms,
                        report.pairs,
                        report.bundle_bytes
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("build FAILED: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Verify { bundle } => match tessera_build::verify(&bundle) {
            Ok(report) => {
                println!(
                    "OK {} ({}): {} partition(s), {} slice(s), {} segment(s), {} rows, \
                     entity_id_high_water {}",
                    bundle.display(),
                    report.prefix,
                    report.partitions,
                    report.slices,
                    report.segments,
                    report.rows,
                    report.entity_id_high_water
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("FAILED {}: {e}", bundle.display());
                ExitCode::FAILURE
            }
        },
        Command::Serve { config } => {
            tracing_subscriber::fmt::init();
            let prepared = match tessera_server::prepare(&config) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("tessera serve: refused to start: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("tessera serve: could not start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match runtime.block_on(tessera_server::run(prepared)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tessera serve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
