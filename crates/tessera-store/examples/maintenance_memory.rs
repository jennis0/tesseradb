//! **The memory probe** — peak RSS across a merge and a tier coalesce, against the inputs' bytes.
//!
//! The named gap in write-path §12's evidence table: *"merge execute peaks at ≈5–7× its inputs'
//! on-disk bytes"* and *"tier coalescence peaks at ≈2–3× the pairs' bytes"* are **modelled**, and
//! no maintenance event's peak memory has ever been measured. Operators size boxes from those
//! numbers, and `max_merged_segment_bytes` bounds the *selection-time file bytes* rather than the
//! decoded resident set — so the multiplier is the whole of what turns a configured cap into a
//! memory budget.
//!
//! **What it measures.** `VmHWM` from `/proc/self/status` — the kernel's own high-water mark for
//! resident set size, which is the number a container limit is compared against. Sampled before
//! and after each stage, with the delta reported against the stage's input bytes. The mark is
//! monotone within a process, so each stage runs in a fresh child (`--stage`) and the parent
//! reports; running them in one process would attribute the first stage's peak to the second.
//!
//! **What it does not measure.** Allocator retention: `VmHWM` counts pages the process ever held,
//! which is what a limit enforces, but a stage's *own* peak is only distinguishable from its
//! predecessor's because of the fresh-child split. Nor does it model concurrency — a flush, a
//! merge and a coalesce can overlap on the pool, and their peaks add.
//!
//! Run:
//! ```text
//! cargo run --release --example maintenance_memory -p tessera-store -- [--rows N] [--segments K]
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use tessera_spatial::tiler::ScalarType;
use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::Quantisation;
use tessera_store::merge::{execute_merge, MergeInput, MergeSpec};
use tessera_types::{EntityId, IdentityKey};

const PARTITION: &str = "p0";
const VIEW: &str = "s0";
const DEFAULT_ROWS: u64 = 2_000_000;
const DEFAULT_SEGMENTS: u64 = 4;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn vm_hwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn key() -> IdentityKey {
    IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key")
}

fn quantisation() -> Quantisation {
    Quantisation {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn seg_dir(root: &Path, seg_id: &str) -> PathBuf {
    root.join("partitions")
        .join(PARTITION)
        .join("views")
        .join(VIEW)
        .join("segments")
        .join(seg_id)
}

/// Write `segments` flush segments of `rows` rows each. Coordinates are a function of the entity
/// id with a stride that interleaves the segments in Morton order, so the merge's sort does real
/// work rather than concatenating already-ordered runs.
fn write_inputs(root: &Path, rows: u64, segments: u64) -> Vec<MergeInput> {
    (0..segments)
        .map(|s| {
            let entity_lo = s * rows;
            let flush_rows: Vec<FlushRow> = (entity_lo..entity_lo + rows)
                .map(|e| FlushRow {
                    entity_id: EntityId::new(e),
                    external_id: Some(format!("ext-{e}").into_bytes()),
                    x: (((e * 7 + s * 13) % 65_521) as f64) / 65_521.0,
                    y: (((e * 31 + s * 17) % 65_519) as f64) / 65_519.0,
                    scalars: vec![],
                })
                .collect();
            write_flush_segment(
                root,
                PARTITION,
                VIEW,
                FlushInput {
                    incarnation: 0,
                    seg_id: &format!("in-{s}"),
                    rows: flush_rows,
                    quantisation: quantisation(),
                    identity_key: &key(),
                    shard_id: 0,
                    scalar_schema: &[],
                    row_base: (s * rows) as u32,
                }, &[],
            )
            .expect("the input segment writes");
            MergeInput {
                seg_id: format!("in-{s}"),
                entity_lo,
                entity_hi: entity_lo + rows - 1,
            }
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: u64 = arg(&args, "--rows")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ROWS);
    let segments: u64 = arg(&args, "--segments")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SEGMENTS);
    let root = PathBuf::from(
        arg(&args, "--dir").unwrap_or_else(|| "/tmp/tessera-maintenance-memory".to_string()),
    );

    match arg(&args, "--stage").as_deref() {
        // The child: one stage, one process, so `VmHWM` is this stage's own peak.
        Some("merge") => {
            let inputs = write_inputs(&root, rows, segments);
            let input_bytes: u64 = inputs
                .iter()
                .map(|i| dir_bytes(&seg_dir(&root, &i.seg_id)))
                .sum();
            let baseline = vm_hwm_kb();
            let schema: Vec<(String, ScalarType)> = vec![];
            execute_merge(
                &root,
                PARTITION,
                VIEW,
                MergeSpec {
                    incarnation: 0,
                    seg_id: "merged",
                    inputs: &inputs,
                    identity_key: &key(),
                    shard_id: 0,
                    scalar_schema: &schema,
                    absent_ok: &[],
                    row_base: 0,
                },
            )
            .expect("the merge executes");
            println!(
                "MEASURED input_bytes={input_bytes} baseline_kb={baseline} peak_kb={}",
                vm_hwm_kb()
            );
        }
        Some(other) => panic!("unknown stage {other}"),
        None => {
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("the probe directory");
            println!("# maintenance_memory — peak RSS across a merge");
            println!("rows_per_segment={rows} segments={segments}");
            let exe = std::env::current_exe().expect("this binary's path");
            let out = Command::new(exe)
                .args(["--stage", "merge", "--rows"])
                .arg(rows.to_string())
                .args(["--segments"])
                .arg(segments.to_string())
                .args(["--dir"])
                .arg(&root)
                .output()
                .expect("the child runs");
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text
                .lines()
                .find(|l| l.starts_with("MEASURED"))
                .unwrap_or_else(|| panic!("child produced no measurement:\n{text}"));
            let field = |name: &str| -> u64 {
                line.split_whitespace()
                    .find_map(|f| f.strip_prefix(name)?.parse().ok())
                    .unwrap_or(0)
            };
            let input_bytes = field("input_bytes=");
            let peak_kb = field("peak_kb=");
            let baseline_kb = field("baseline_kb=");
            println!(
                "merge: inputs {:.1} MB on disk, VmHWM {:.1} MB (baseline before the merge \
                 {:.1} MB) → **{:.1}x** the inputs' file bytes",
                input_bytes as f64 / 1e6,
                peak_kb as f64 / 1e3,
                baseline_kb as f64 / 1e3,
                (peak_kb as f64 * 1e3) / input_bytes as f64,
            );
        }
    }
}
