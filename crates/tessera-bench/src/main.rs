//! `tessera-bench` — the measurement harness.
//!
//! **Why this is not criterion.** Criterion stays as the CI regression gate at one fixed scale
//! (`crates/tessera-engine/benches/viewport.rs`), which is what it is good at. It has no
//! representation for a non-latency metric, and this suite's central methodological constraint —
//! `probes/optimisations.md` §0, *"any benchmark that varies cardinality while holding container
//! structure constant will mislead"* — means every latency must be reported next to the container
//! count that explains it. That, plus a parameterised matrix and structured JSONL output, is what
//! this binary exists for.
//!
//! **Nothing depends on this crate.** It reaches across authz, store, spatial, engine, build and
//! server together, which no shipped crate is allowed to do; `scripts/check-layers.sh` asserts the
//! edge stays one-way.

mod arms;
mod corpus;
mod fixture;
mod metrics;
mod postings;
mod report;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tessera-bench",
    about = "Tessera performance measurement harness"
)]
struct Cli {
    /// Where fixtures live (built by `scripts/bench_build_fixtures.sh`).
    #[arg(long, default_value = fixture::DEFAULT_FIXTURES, global = true)]
    fixtures: PathBuf,

    /// Where results and the resume ledger go.
    #[arg(long, default_value = "/tmp/tessera-bench/runs/current", global = true)]
    run_dir: PathBuf,

    /// Repetitions per cell. Phase 0's convention is 3 generally, 5 at the largest scale; the
    /// minimum is the headline and the spread is retained.
    #[arg(long, default_value_t = 3, global = true)]
    repeat: u32,

    /// Restrict to these scales (comma-separated). Omit for every fixture found.
    #[arg(long, global = true, value_delimiter = ',')]
    scale: Vec<u64>,

    /// Restrict to these label sets (comma-separated). Omit for every fixture found.
    #[arg(long, global = true, value_delimiter = ',')]
    label_set: Vec<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a declared campaign of arms, resumably.
    Matrix {
        #[arg(long, default_value = "bench/matrix.toml")]
        matrix: PathBuf,
        /// Run only this arm.
        #[arg(long)]
        only: Option<String>,
        /// Print the campaign's fixture selection and stop, without measuring anything.
        #[arg(long, default_value_t = false)]
        plan: bool,
    },

    /// List discovered fixtures and their sizes.
    Fixtures,

    /// Clock overhead and other harness self-measurements, so perturbation is subtracted rather
    /// than assumed away.
    Calibrate,

    /// Mask construction cost: `build_fragment` against real postings.
    ///
    /// Swept over grant width and grant *shape*, not only over point count — the measured driver
    /// is `w` and posting shape, and `probes/results.md` §4.3 found dictionary scale is free on
    /// this path (117M terms costs the same as 10M). A point-count-only sweep draws a flat,
    /// misleading line.
    Authorise {
        /// Grant widths to sweep.
        #[arg(long, value_delimiter = ',', default_values_t = [1usize, 10, 100, 1000, 10000])]
        w: Vec<usize>,
        /// Grant shapes: random | head | tail | clustered.
        #[arg(long, value_delimiter = ',', default_values_t = ["random".to_string(), "clustered".to_string(), "head".to_string()])]
        shape: Vec<String>,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },

    /// Column-gather cost by access pattern — the probe `probes/optimisations.md` §3.5 records
    /// as owed. Priority-independent by construction: it supplies its own row selection and never
    /// goes through the sampler, so whichever sampler ships cannot move these numbers.
    Gather {
        #[arg(long, value_delimiter = ',', default_values_t = [
            "contiguous".to_string(), "strided".to_string(), "scattered".to_string(),
            "scattered-unsorted".to_string(), "full-scan".to_string()])]
        pattern: Vec<String>,
        #[arg(long, value_delimiter = ',', default_values_t = [30usize, 200, 1000, 10000])]
        k: Vec<usize>,
        #[arg(long, value_delimiter = ',', default_values_t = [0.001f64, 0.01, 0.05, 0.25])]
        coverage: Vec<f64>,
        /// Rows the tile range spans — the "points in cell" axis.
        #[arg(long, value_delimiter = ',', default_values_t = [1000u32, 10_000, 100_000, 1_000_000])]
        range_rows: Vec<u32>,
        #[arg(long, value_delimiter = ',', default_values_t = ["xy+id".to_string()])]
        columns: Vec<String>,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },

    /// Whole-viewport latency with per-stage attribution.
    ///
    /// Modes: `battery` (density deciles, the workload-variance-free stall detector),
    /// `pan`, `zoom`, `random` (the existing harnesses' recipe, reported bucketed),
    /// `coverage-sweep` (which doubles as the C4 timing-channel quantification).
    Viewport {
        #[arg(long, value_delimiter = ',', default_values_t = ["battery".to_string(), "pan".to_string(), "zoom".to_string()])]
        mode: Vec<String>,
        #[arg(long, value_delimiter = ',', default_values_t = [30usize, 200])]
        k: Vec<usize>,
        #[arg(long, value_delimiter = ',', default_values_t = [0.01f64, 0.05, 0.25])]
        coverage: Vec<f64>,
        /// Zoom for the modes that hold it fixed (battery, pan, coverage-sweep).
        #[arg(long, default_value_t = 8)]
        zoom: u8,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// Design §7.2's cap clause. Selection cost depends on this, not only on `k`: it bounds the
        /// heap and the gather, and it decides which tiles the serve-all branch can claim. More than
        /// one value costs an `Engine::open` per value, so the default is the server's own.
        #[arg(long = "k-max-marks", value_delimiter = ',', default_values_t = [arms::viewport::DEFAULT_K_MAX_MARKS])]
        k_max_marks: Vec<usize>,
        /// Design §7.2's θ anchor target. Decides how many tiles skip the counting pass entirely,
        /// so it moves selection cost independently of `k`. One `Engine::open` per value.
        #[arg(long = "theta-target", value_delimiter = ',', default_values_t = [arms::viewport::DEFAULT_THETA_TARGET])]
        theta_target: Vec<u64>,
    },

    /// The `/control/changes` write path: deletes, suppressions, predicate changes.
    ///
    /// The one write path where a latency regression is a security regression (lifecycle §1.3
    /// bounds deny visibility by queue-front + fsync). Also checks that a deny actually denies —
    /// at zoom 0 the masked count must drop by exactly one per suppressed entity.
    Changes {
        #[arg(long, value_delimiter = ',', default_values_t = ["suppress".to_string(), "delete".to_string(), "predicate".to_string()])]
        op: Vec<String>,
        /// Overlay-depth checkpoints.
        #[arg(long, value_delimiter = ',', default_values_t = [100u64, 1_000, 5_000, 20_000])]
        checkpoint: Vec<u64>,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },

    /// Blank-database ingest: `tessera build` decomposed into its eleven pipeline stages.
    IngestBuild {
        #[arg(long, value_delimiter = ',', default_values_t = [250_000u64, 2_422_486, 25_000_000])]
        scale: Vec<u64>,
        #[arg(long, value_delimiter = ',', default_values_t = ["categories-subclass".to_string()])]
        label_set: Vec<String>,
        /// Repository root holding `data/scaled/`.
        #[arg(long, default_value = ".")]
        data_root: PathBuf,
    },

    /// Update ingest, batched: ack latency as a function of batch size.
    IngestBatch {
        #[arg(long, value_delimiter = ',', default_values_t = [1usize, 10, 100, 1000, 10000])]
        batch: Vec<usize>,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },

    /// Update ingest, continuous single writes, with a reader drawing a fixed viewport as the
    /// overlay grows. Reports the write-ack curve (F3) and `compose_ns` against buffer depth (F2).
    IngestContinuous {
        /// Buffered-item checkpoints.
        ///
        /// Deliberately far below `overlay_soft_limit = 500_000`: every single-item write costs
        /// one fsync (~3.2 ms measured), so reaching 500k would take ~27 minutes of pure fsync
        /// before F3's superlinear clone cost is even counted. The curve is established here and
        /// extrapolated; raise this when you want to pay for the endpoint directly.
        #[arg(long, value_delimiter = ',', default_values_t = [500u64, 2_000, 5_000, 10_000, 25_000])]
        checkpoint: Vec<u64>,
        #[arg(long, default_value_t = 30)]
        k: usize,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },

    /// HTTP load generator. Orchestration lives in `scripts/bench_concurrency.py`; this is the
    /// hot loop, in Rust because a Python client is the bottleneck at 1000 connections pulling
    /// multi-MB Arrow bodies.
    Load {
        #[arg(long)]
        viewer_url: String,
        /// One bearer token per line. N distinct tokens = Arm A (N fragments); a few reused
        /// across N workers = Arm B (shared fragments).
        #[arg(long)]
        tokens: PathBuf,
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
        #[arg(long, default_value_t = 10.0)]
        duration_s: f64,
        /// Generator worker threads. Default 4 of 12 cores, leaving the rest for the server.
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// Open-loop arrival rate (req/s). Omit for closed loop.
        #[arg(long)]
        rate: Option<f64>,
        /// Drive `/healthz` instead of `/v1/viewport` to establish the generator's own ceiling.
        #[arg(long, default_value_t = false)]
        healthz: bool,
        #[arg(long, default_value_t = 0)]
        bundle_scale: u64,
        #[arg(long, default_value = "unknown")]
        bundle_label_set: String,
        #[arg(long, default_value_t = 30)]
        k: usize,
        #[arg(long, default_value_t = 8)]
        zoom: u8,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },

    /// Morton tile enumeration and masked counting, swept over zoom.
    ///
    /// Covers zoom 0..=3 as well, which every existing script omits: counting is nearly free at
    /// depth 0 (Phase 0 measured a billion rows in 191 us) but nobody has measured the gather
    /// there, and that is the drawn-mark-budget regime.
    Tiles {
        #[arg(long, value_delimiter = ',', default_values_t = [0u8, 2, 4, 6, 8, 10, 12])]
        zoom: Vec<u8>,
        /// Coverage targets for the principal whose mask is counted.
        #[arg(long, value_delimiter = ',', default_values_t = [0.01f64, 0.05, 0.25])]
        coverage: Vec<f64>,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
}

fn main() -> std::process::ExitCode {
    // Restore the default SIGPIPE disposition. Rust ignores SIGPIPE and turns the failed write
    // into an error, which `println!` then panics on — so `tessera-bench fixtures | head` prints
    // a panic backtrace that reads like a real failure. Every listing subcommand here is meant to
    // be piped, so exit quietly instead, as every other CLI does.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    let fixtures = match fixture::discover(&cli.fixtures) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot read fixtures at {}: {e}", cli.fixtures.display());
            return std::process::ExitCode::FAILURE;
        }
    };
    let selected: Vec<fixture::Fixture> = fixtures
        .into_iter()
        .filter(|f| cli.scale.is_empty() || cli.scale.contains(&f.scale))
        .filter(|f| cli.label_set.is_empty() || cli.label_set.contains(&f.label_set))
        .collect();

    if selected.is_empty() && !matches!(cli.command, Command::Fixtures) {
        // A typo in --label-set silently selecting nothing is worse than selecting everything:
        // the run "succeeds" with no cells and a reader sees an empty file, not an error.
        eprintln!(
            "no fixture matches --scale {:?} --label-set {:?} under {}. \
             Run `tessera-bench fixtures` to see what exists.",
            cli.scale,
            cli.label_set,
            cli.fixtures.display()
        );
        return std::process::ExitCode::FAILURE;
    }

    let ctx = arms::Context {
        run_dir: cli.run_dir.clone(),
        repeat: cli.repeat,
        fixtures: selected,
    };

    let result = match cli.command {
        Command::Matrix { matrix, only, plan } => {
            arms::matrix::run(&ctx, &matrix, only.as_deref(), plan)
        }
        Command::Fixtures => arms::list_fixtures(&ctx),
        Command::Calibrate => arms::calibrate(&ctx),
        Command::Authorise { w, shape, seed } => arms::authorise::run(&ctx, &w, &shape, seed),
        Command::Gather {
            pattern,
            k,
            coverage,
            range_rows,
            columns,
            seed,
        } => arms::gather::run(&ctx, &pattern, &k, &coverage, &range_rows, &columns, seed),
        Command::Viewport {
            mode,
            k,
            coverage,
            zoom,
            seed,
            k_max_marks,
            theta_target,
        } => arms::viewport::run(
            &ctx,
            &mode,
            &k,
            &coverage,
            zoom,
            seed,
            &arms::viewport::SelectionSweep {
                k_max_marks: &k_max_marks,
                theta_targets: &theta_target,
            },
        ),
        Command::Load {
            viewer_url,
            tokens,
            concurrency,
            duration_s,
            threads,
            rate,
            healthz,
            bundle_scale,
            bundle_label_set,
            k,
            zoom,
            seed,
        } => arms::load::run(
            &ctx,
            &viewer_url,
            &tokens,
            concurrency,
            duration_s,
            threads,
            rate,
            healthz,
            bundle_scale,
            &bundle_label_set,
            k,
            zoom,
            seed,
        ),
        Command::Changes {
            op,
            checkpoint,
            seed,
        } => arms::changes::run(&ctx, &op, &checkpoint, seed),
        Command::IngestBuild {
            scale,
            label_set,
            data_root,
        } => arms::ingest::run_build(&ctx, &scale, &label_set, &data_root),
        Command::IngestBatch { batch, seed } => arms::ingest::run_batch(&ctx, &batch, seed),
        Command::IngestContinuous {
            checkpoint,
            k,
            seed,
        } => arms::ingest::run_continuous(&ctx, &checkpoint, k, seed),
        Command::Tiles {
            zoom,
            coverage,
            seed,
        } => arms::tiles::run(&ctx, &zoom, &coverage, seed),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
