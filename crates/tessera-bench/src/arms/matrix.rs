//! The matrix orchestrator: run a declared grid of arms in one command, resumably.
//!
//! Each arm already skips cells the ledger records as done, so resume works at cell granularity
//! whether an arm is invoked directly or through here. What this adds is the *campaign* — one
//! declarative file saying which arms run at which scales against which label sets, so a run is
//! reproducible from a committed artifact rather than from shell history.
//!
//! # What is deliberately not here
//!
//! * **`load`** needs a booted `tessera serve`, N authorised sessions and server-side RSS/CPU
//!   sampling. That orchestration lives in `scripts/bench_concurrency.py`, which reuses
//!   `reference/oracle/harness.py`'s proven boot machinery; duplicating it in Rust would buy
//!   nothing and would have to be kept in step.
//! * **`ingest-build`** is minutes per cell and writes a full bundle to temp before deleting it.
//!   It is supported here, but it is not in the shipped `bench/matrix.toml` — putting a
//!   two-minute cell next to a two-microsecond one in the same default run makes the default
//!   run useless.

use std::path::Path;

use serde::Deserialize;

use crate::arms::{Context, Result};
use crate::fixture::Fixture;

/// The campaign file. Every field has a default so an arm entry can be one line.
#[derive(Debug, Deserialize)]
pub struct MatrixSpec {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default, rename = "arm")]
    arms: Vec<ArmSpec>,
}

#[derive(Debug, Deserialize)]
struct Defaults {
    #[serde(default = "default_repeat")]
    repeat: u32,
    #[serde(default)]
    seed: u64,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            repeat: default_repeat(),
            seed: 0,
        }
    }
}

fn default_repeat() -> u32 {
    3
}

#[derive(Debug, Deserialize)]
struct ArmSpec {
    name: String,
    /// Scales to run. Empty means every discovered scale.
    #[serde(default)]
    scales: Vec<u64>,
    /// Label sets to run. Empty means every discovered set.
    #[serde(default)]
    label_sets: Vec<String>,
    #[serde(default)]
    repeat: Option<u32>,
    #[serde(default)]
    seed: Option<u64>,

    // Arm-specific axes, all optional with the arm's own default applied when absent.
    #[serde(default)]
    w: Vec<usize>,
    #[serde(default)]
    shape: Vec<String>,
    #[serde(default)]
    zoom: Vec<u8>,
    #[serde(default)]
    coverage: Vec<f64>,
    #[serde(default)]
    k: Vec<usize>,
    #[serde(default)]
    pattern: Vec<String>,
    #[serde(default)]
    range_rows: Vec<u32>,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    mode: Vec<String>,
    /// Design §7.2's cap clause. Selection cost depends on it and not only on `k`. Costs one
    /// `Engine::open` per value, so it defaults to the single server value rather than a sweep.
    #[serde(default)]
    k_max_marks: Vec<usize>,
    /// Design §7.2's θ anchor target: decides how many tiles skip the counting pass. One
    /// `Engine::open` per value.
    #[serde(default)]
    theta_target: Vec<u64>,
    #[serde(default)]
    batch: Vec<usize>,
    /// `ingest-rate`: descriptors per row, `B/W`, and concurrent callers.
    #[serde(default)]
    density: Vec<usize>,
    #[serde(default)]
    bw: Vec<usize>,
    #[serde(default)]
    submitters: Vec<usize>,
    /// `ingest-rate`: `ingest.commit_window_max_items`, and the floor on rows measured per cell.
    #[serde(default)]
    window: Option<usize>,
    #[serde(default)]
    min_steady_rows: Option<usize>,
    #[serde(default)]
    checkpoint: Vec<u64>,
    #[serde(default)]
    op: Vec<String>,
    #[serde(default)]
    fixed_zoom: Option<u8>,
    #[serde(default)]
    data_root: Option<String>,
}

/// Non-empty `given`, else `fallback`.
fn or<T: Clone>(given: &[T], fallback: &[T]) -> Vec<T> {
    if given.is_empty() {
        fallback.to_vec()
    } else {
        given.to_vec()
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub fn run(ctx: &Context, matrix_path: &Path, only: Option<&str>, plan_only: bool) -> Result<()> {
    let spec: MatrixSpec = toml::from_str(&std::fs::read_to_string(matrix_path)?)?;
    if spec.arms.is_empty() {
        return Err(format!("{} declares no [[arm]] entries", matrix_path.display()).into());
    }

    // Plan before running, and print it. A campaign that turns out to be the wrong shape should
    // be visible in the first second, not after forty minutes of measuring the wrong grid.
    println!("campaign from {}", matrix_path.display());
    let mut planned = Vec::new();
    let mut total_cells = 0usize;
    for arm in &spec.arms {
        if let Some(only) = only {
            if arm.name != only {
                continue;
            }
        }
        let fixtures = select(&ctx.fixtures, arm);
        if fixtures.is_empty() {
            println!(
                "  {:<20} SKIP — no fixture matches scales={:?} label_sets={:?}",
                arm.name, arm.scales, arm.label_sets
            );
            continue;
        }
        let cells = estimated_cells(arm) * fixtures.len();
        total_cells += cells;
        println!(
            "  {:<20} {:>2} fixture(s) x {:>4} = {:>6} cells   {}",
            arm.name,
            fixtures.len(),
            estimated_cells(arm),
            cells,
            fixtures
                .iter()
                .map(|f| format!("{}/{}", f.scale, f.label_set))
                .collect::<Vec<_>>()
                .join(", ")
        );
        planned.push((arm, fixtures));
    }
    println!(
        "  {:<20} {:>6} cells total (upper bound — arms skip cells that cannot fit, e.g. a \n\
                        range wider than the segment, or a w above the vocabulary)",
        "", total_cells
    );
    if planned.is_empty() {
        return Err("nothing to run — check --only, scales and label_sets".into());
    }
    if plan_only {
        println!("\n--plan: nothing run. Drop the flag to execute.");
        return Ok(());
    }
    println!();

    for (arm, fixtures) in planned {
        let sub = Context {
            run_dir: ctx.run_dir.clone(),
            repeat: arm.repeat.unwrap_or(spec.defaults.repeat),
            fixtures,
        };
        let seed = arm.seed.unwrap_or(spec.defaults.seed);
        println!("=== {} ===", arm.name);

        // A failing arm does not abort the campaign: the remaining arms are independent, and a
        // partial grid with a named failure is more useful than no grid at all. The ledger
        // records only what succeeded, so a re-run picks up exactly what is missing.
        let outcome = dispatch(&sub, arm, seed);
        if let Err(e) = outcome {
            eprintln!("  {} FAILED: {e}", arm.name);
        }
    }

    Ok(())
}

/// Cells one fixture will produce: the product of the arm's swept axes.
///
/// An upper bound, not a count — arms skip cells that cannot exist (a range wider than the
/// segment, a `w` above the vocabulary, a coverage target no grant reaches). The point is to make
/// a mis-shaped campaign visible before it runs, not to be exact.
fn estimated_cells(arm: &ArmSpec) -> usize {
    fn n<T>(given: &[T], default_len: usize) -> usize {
        if given.is_empty() {
            default_len
        } else {
            given.len()
        }
    }
    match arm.name.as_str() {
        "authorise" => n(&arm.w, 5) * n(&arm.shape, 3),
        "tiles" => n(&arm.zoom, 7) * n(&arm.coverage, 3),
        "gather" => {
            n(&arm.pattern, 5)
                * n(&arm.k, 4)
                * n(&arm.coverage, 4)
                * n(&arm.range_rows, 4)
                * n(&arm.columns, 1)
        }
        // The battery and coverage-sweep modes emit up to ten viewports each; pan emits 12 and
        // zoom 7. Ten is the representative figure.
        // The trailing 10 is the per-mode viewport count the arm generates internally. The two
        // selection axes multiply the whole cell count because each value is a separate
        // `Engine::open` — omitting them under-reported a sweep arm's cost by its own factor.
        "viewport" => {
            n(&arm.mode, 3)
                * n(&arm.k, 2)
                * n(&arm.coverage, 3)
                * n(&arm.k_max_marks, 1)
                * n(&arm.theta_target, 1)
                * 10
        }
        "changes" => n(&arm.op, 3) * n(&arm.checkpoint, 4),
        "ingest-batch" => n(&arm.batch, 4),
        "ingest-rate" => n(&arm.density, 3) * n(&arm.bw, 4) * n(&arm.submitters, 4),
        "ingest-continuous" => n(&arm.checkpoint, 5),
        "ingest-build" => 1,
        _ => 1,
    }
}

fn select(fixtures: &[Fixture], arm: &ArmSpec) -> Vec<Fixture> {
    fixtures
        .iter()
        .filter(|f| arm.scales.is_empty() || arm.scales.contains(&f.scale))
        .filter(|f| arm.label_sets.is_empty() || arm.label_sets.contains(&f.label_set))
        .cloned()
        .collect()
}

fn dispatch(ctx: &Context, arm: &ArmSpec, seed: u64) -> Result<()> {
    match arm.name.as_str() {
        "authorise" => super::authorise::run(
            ctx,
            &or(&arm.w, &[1, 10, 100, 1000, 10000]),
            &or(&arm.shape, &strings(&["random", "clustered", "head"])),
            seed,
        ),
        "tiles" => super::tiles::run(
            ctx,
            &or(&arm.zoom, &[0, 2, 4, 6, 8, 10, 12]),
            &or(&arm.coverage, &[0.01, 0.05, 0.25]),
            seed,
        ),
        "gather" => super::gather::run(
            ctx,
            &or(
                &arm.pattern,
                &strings(&[
                    "contiguous",
                    "strided",
                    "scattered",
                    "scattered-unsorted",
                    "full-scan",
                ]),
            ),
            &or(&arm.k, &[30, 200, 1000, 10000]),
            &or(&arm.coverage, &[0.001, 0.01, 0.05, 0.25]),
            &or(&arm.range_rows, &[1000, 10_000, 100_000, 1_000_000]),
            &or(&arm.columns, &strings(&["pos+id"])),
            seed,
        ),
        "viewport" => super::viewport::run(
            ctx,
            &or(&arm.mode, &strings(&["battery", "pan", "zoom"])),
            &or(&arm.k, &[30, 200]),
            &or(&arm.coverage, &[0.01, 0.05, 0.25]),
            arm.fixed_zoom.unwrap_or(8),
            seed,
            &super::viewport::SelectionSweep {
                k_max_marks: &or(&arm.k_max_marks, &[super::viewport::DEFAULT_K_MAX_MARKS]),
                theta_targets: &or(&arm.theta_target, &[super::viewport::DEFAULT_THETA_TARGET]),
            },
        ),
        "changes" => super::changes::run(
            ctx,
            &or(&arm.op, &strings(&["suppress", "delete"])),
            &or(&arm.checkpoint, &[100, 1_000, 5_000, 20_000]),
            seed,
        ),
        "ingest-batch" => super::ingest::run_batch(ctx, &or(&arm.batch, &[1, 10, 100, 1000]), seed),
        // `batch` and `window` are scalars for this arm — one rows-per-call and one window
        // ceiling, held fixed while the three axes sweep — so the shared `Vec` field is read for
        // its first value, as `ingest-continuous` reads `k`.
        "ingest-rate" => super::ingest::run_rate(
            ctx,
            &super::ingest::RateSweep {
                density: &or(&arm.density, &[1, 3, 8]),
                ratio: &or(&arm.bw, &[1, 4, 12, 24]),
                submitters: &or(&arm.submitters, &[1, 2, 4, 8]),
                batch: *or(&arm.batch, &[10_000]).first().unwrap_or(&10_000),
                window: arm.window.unwrap_or(10_000),
                min_steady_rows: arm.min_steady_rows.unwrap_or(240_000),
                seed,
            },
        ),
        "ingest-continuous" => super::ingest::run_continuous(
            ctx,
            &or(&arm.checkpoint, &[500, 2_000, 5_000, 10_000, 25_000]),
            *or(&arm.k, &[30]).first().unwrap_or(&30),
            seed,
        ),
        "ingest-build" => {
            let root = arm.data_root.as_deref().unwrap_or(".");
            let scales: Vec<u64> = if arm.scales.is_empty() {
                ctx.fixtures.iter().map(|f| f.scale).collect()
            } else {
                arm.scales.clone()
            };
            let sets: Vec<String> = if arm.label_sets.is_empty() {
                vec!["categories-subclass".to_string()]
            } else {
                arm.label_sets.clone()
            };
            super::ingest::run_build(ctx, &scales, &sets, Path::new(root))
        }
        other => Err(format!(
            "unknown arm {other:?}. Known: authorise, tiles, gather, viewport, changes, \
             ingest-batch, ingest-rate, ingest-continuous, ingest-build. `load` is driven by \
             scripts/bench_concurrency.py, which owns the server lifecycle."
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<Fixture> {
        [(250_000u64, "a"), (250_000, "b"), (2_422_486, "a")]
            .iter()
            .map(|(scale, set)| Fixture {
                scale: *scale,
                label_set: set.to_string(),
                root: std::path::PathBuf::new(),
                prefix: "v00000".to_string(),
                bytes: 0,
            })
            .collect()
    }

    fn spec_with(scales: Vec<u64>, label_sets: Vec<String>) -> ArmSpec {
        toml::from_str(&format!(
            "name = \"authorise\"\nscales = {scales:?}\nlabel_sets = {label_sets:?}\n"
        ))
        .unwrap()
    }

    #[test]
    fn empty_selectors_mean_everything() {
        let arm = spec_with(vec![], vec![]);
        assert_eq!(select(&fixtures(), &arm).len(), 3);
    }

    #[test]
    fn selectors_intersect() {
        let arm = spec_with(vec![250_000], vec!["a".to_string()]);
        let got = select(&fixtures(), &arm);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].scale, 250_000);
        assert_eq!(got[0].label_set, "a");
    }

    #[test]
    fn a_selector_matching_nothing_yields_nothing_rather_than_everything() {
        // The failure mode worth guarding: a typo in a label set silently running the whole grid
        // would be far worse than running none of it.
        let arm = spec_with(vec![], vec!["typo".to_string()]);
        assert!(select(&fixtures(), &arm).is_empty());
    }

    #[test]
    fn or_prefers_the_given_axis_and_falls_back_when_empty() {
        assert_eq!(or(&[1, 2], &[9]), vec![1, 2]);
        assert_eq!(or::<u32>(&[], &[9]), vec![9]);
    }

    #[test]
    fn the_shipped_matrix_parses() {
        // A campaign file that does not parse is discovered at the start of a long run, which is
        // the cheapest possible time — but only if something checks.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("bench/matrix.toml");
        let text = std::fs::read_to_string(&path).expect("bench/matrix.toml should exist");
        let spec: MatrixSpec = toml::from_str(&text).expect("bench/matrix.toml should parse");
        assert!(!spec.arms.is_empty());
        for arm in &spec.arms {
            assert!(
                matches!(
                    arm.name.as_str(),
                    "authorise"
                        | "tiles"
                        | "gather"
                        | "viewport"
                        | "changes"
                        | "ingest-batch"
                        | "ingest-rate"
                        | "ingest-continuous"
                        | "ingest-build"
                ),
                "unknown arm in the shipped matrix: {}",
                arm.name
            );
        }
    }
}
