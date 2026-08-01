//! The measurement arms, and the context they share.

pub mod authorise;
pub mod changes;
pub mod gather;
pub mod ingest;
pub mod load;
pub mod matrix;
pub mod tiles;
pub mod viewport;

use std::path::PathBuf;

use crate::fixture::{Fixture, Ledger};
use crate::report::{Env, Record, Stages, Timing, Work, Writer, SCHEMA_VERSION};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Everything every arm needs: which bundles to measure, how many repetitions, where output goes.
pub struct Context {
    pub run_dir: PathBuf,
    pub repeat: u32,
    pub fixtures: Vec<Fixture>,
}

impl Context {
    /// A writer, ledger and env snapshot for one arm.
    pub fn open(&self, arm: &str) -> Result<ArmRun> {
        let writer = Writer::open(&self.run_dir.join(format!("{arm}.jsonl")))?;
        let ledger = Ledger::open(&self.run_dir)?;
        Ok(ArmRun {
            arm: arm.to_string(),
            run_id: run_id(),
            writer,
            ledger,
            env: capture_env(),
            clock_lap_ns: crate::metrics::clock_lap_ns(),
            written: 0,
            skipped: 0,
        })
    }
}

/// One arm's output channel, carrying the per-run constants so no cell has to re-derive them.
pub struct ArmRun {
    pub arm: String,
    pub run_id: String,
    pub writer: Writer,
    pub ledger: Ledger,
    pub env: Env,
    pub clock_lap_ns: u64,
    pub written: usize,
    pub skipped: usize,
}

impl ArmRun {
    /// Emit one cell — result file first, then the ledger line (see `Ledger`'s doc for why the
    /// order matters).
    #[allow(clippy::too_many_arguments)]
    pub fn emit(
        &mut self,
        cell_id: String,
        fixture: &Fixture,
        params: serde_json::Value,
        work: Work,
        samples: Vec<u64>,
        stages: Option<Stages>,
        mut flags: Vec<String>,
    ) -> Result<()> {
        let timing = Timing::from_samples(samples);
        let normalised = crate::report::Normalised::derive(timing.min_ns, &work);

        // A cell whose mask covers everything measures the absence of masking. The probes
        // flagged exactly this ("the 13.8 s row is degenerate — that principal sees 100% of the
        // corpus, so there is nothing to mask; compare equal-coverage rows"). Mark it so a
        // collator cannot quietly average it in with real principals.
        if work.degenerate && !flags.iter().any(|f| f == "degenerate") {
            flags.push("degenerate".to_string());
        }

        // A Roaring container holds 2^16 entities, so a corpus of N spans at most ceil(N/65536)
        // of them: 4 at 250k, 37 at 2.42M, 382 at 25M. Below ~32 there are too few denominators
        // for `ns_per_container` to mean anything — the cost is dominated by work *within*
        // containers, which is the regime the probes' model explicitly does not describe (its
        // headline, 114 ns/container, was fitted across 399 -> 1.5M containers). Flag it rather
        // than let a collator gate on a ratio built from four samples.
        const MIN_CONTAINERS_FOR_NORMALISATION: u64 = 32;
        if work.containers > 0 && work.containers < MIN_CONTAINERS_FOR_NORMALISATION {
            flags.push("low_container_resolution".to_string());
        }

        // Instrumentation perturbation and a debug build are both reasons a number is not what
        // it appears to be; say so in the record instead of in a README nobody reads.
        if cfg!(debug_assertions) {
            flags.push("debug_build".to_string());
        }

        let record = Record {
            schema: SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            cell_id: cell_id.clone(),
            arm: self.arm.clone(),
            scale: fixture.scale,
            label_set: fixture.label_set.clone(),
            params,
            work,
            timing,
            stages,
            normalised,
            env: self.env.clone(),
            status: "ok".to_string(),
            flags,
        };
        self.writer.write(&record)?;
        self.ledger.mark(&cell_id, "ok")?;
        self.written += 1;
        Ok(())
    }

    pub fn finish(&self) {
        println!(
            "{}: {} cells written, {} skipped (already in ledger)",
            self.arm, self.written, self.skipped
        );
    }
}

fn run_id() -> String {
    // No wall clock dependency beyond process start: the run id only has to be unique within a
    // campaign, and the ledger — not the id — is what makes a run resumable.
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

/// What this process is running on, captured once.
fn capture_env() -> Env {
    let (total_gib, avail_gib) = crate::metrics::memory_gib();
    let (minor, major) = crate::metrics::faults();
    Env {
        git_sha: git("rev-parse --short HEAD"),
        dirty: !git("status --porcelain").is_empty(),
        profile: if cfg!(debug_assertions) {
            "debug".to_string()
        } else {
            "release".to_string()
        },
        features: if cfg!(feature = "bench-timing") {
            vec!["bench-timing".to_string()]
        } else {
            vec![]
        },
        cores: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        mem_gib: total_gib,
        free_ram_gib_before: avail_gib,
        bundle_fits_in_ram: true,
        minor_faults_delta: minor,
        major_faults_delta: major,
        rss_peak_kib: crate::metrics::rss_peak_kib(),
        load_avg: crate::metrics::load_avg(),
    }
}

fn git(args: &str) -> String {
    std::process::Command::new("git")
        .args(args.split_whitespace())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn list_fixtures(ctx: &Context) -> Result<()> {
    if ctx.fixtures.is_empty() {
        println!("no fixtures found — run scripts/bench_build_fixtures.sh");
        return Ok(());
    }
    println!("{:>12}  {:<20}  {:>10}  root", "scale", "label set", "size");
    for f in &ctx.fixtures {
        println!(
            "{:>12}  {:<20}  {:>9.1}M  {}",
            f.scale,
            f.label_set,
            f.bytes as f64 / (1024.0 * 1024.0),
            f.root.display()
        );
    }
    Ok(())
}

/// Harness self-measurement, reported before any number that depends on it is believed.
pub fn calibrate(_ctx: &Context) -> Result<()> {
    let lap = crate::metrics::clock_lap_ns();
    let (total, avail) = crate::metrics::memory_gib();
    println!("clock_lap_ns          {lap}");
    println!(
        "  a ~300-tile viewport takes ~4 laps/tile, so instrumentation costs ~{} us/request",
        lap * 4 * 300 / 1000
    );
    println!(
        "cores                 {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    println!("mem_total_gib         {total}");
    println!("mem_available_gib     {avail}");
    println!("load_avg              {:.2}", crate::metrics::load_avg());
    println!(
        "bench_timing          {}",
        if cfg!(feature = "bench-timing") {
            "on"
        } else {
            "OFF — stage breakdowns will be all zeros"
        }
    );
    println!(
        "profile               {}",
        if cfg!(debug_assertions) {
            "debug — NUMBERS ARE NOT MEANINGFUL, build with --release"
        } else {
            "release"
        }
    );
    Ok(())
}
