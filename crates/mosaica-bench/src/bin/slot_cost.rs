//! **What does a viewer's first colouring of a layer cost, and how often do neighbours clash?**
//!
//! Opens a bundle with an empty cache, authorises the broadest principal, and builds the layer's
//! cluster slots ([`mosaica_engine::SlotStats`]) for palettes of 8, 10, 20 and 22 colours:
//!
//! - **first**: the first build in a fresh process, which also walks each level's masked counts;
//! - **cold**: a build for a palette size not built yet, the counts already held;
//! - **warm**: the same palette size asked again, answered from the cache.
//!
//! Each line also gives the clusters coloured, the drawn neighbour pairs, the pairs sharing a
//! slot, and how many items the centres were taken from.
//!
//! ```text
//! cargo run --release -p mosaica-bench --bin slot_cost -- \
//!     --bundle data/ladder/arxiv/bundle --view knn --layer clusters/hdbscan
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use mosaica_engine::{Engine, EngineConfig};

fn main() {
    let mut bundle: Option<PathBuf> = None;
    let mut view: Option<String> = None;
    let mut layer: Option<String> = None;
    let mut terms: Option<Vec<String>> = None;
    let mut scratch: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bundle" => bundle = args.next().map(PathBuf::from),
            "--view" => view = args.next(),
            "--layer" => layer = args.next(),
            "--terms" => {
                terms = args
                    .next()
                    .map(|t| t.split(',').map(str::to_string).collect())
            }
            "--scratch" => scratch = args.next().map(PathBuf::from),
            other => panic!(
                "unknown argument {other}; pass --bundle, --view and --layer, and optionally \
                 --terms a,b and --scratch <dir>"
            ),
        }
    }
    let (Some(root), Some(view), Some(layer)) = (bundle, view, layer) else {
        panic!("pass --bundle, --view and --layer");
    };
    let tmp = match scratch {
        Some(dir) => tempfile::TempDir::new_in(dir).expect("a scratch dir"),
        None => tempfile::TempDir::new().expect("a scratch dir"),
    };
    let opening = Instant::now();
    let engine = open(&root, &tmp.path().join("cache"), &tmp.path().join("wal.log"));
    println!(
        "opened in {:.1} s, peak resident {} MiB",
        opening.elapsed().as_secs_f64(),
        peak_mib()
    );
    let terms = terms.unwrap_or_else(|| all_terms(&root));
    let session = engine
        .authorise(&credential(&terms))
        .expect("the credential authorises");
    println!(
        "bundle {}  view {view}  layer {layer}  terms {}",
        root.display(),
        terms.len()
    );
    let mut first = true;
    for palette in [8u8, 10, 20, 22] {
        for pass in ["cold", "warm"] {
            let started = Instant::now();
            let stats = engine
                .cluster_slot_stats(&session, &view, &layer, palette)
                .expect("the layer is coloured");
            let ms = started.elapsed().as_secs_f64() * 1e3;
            let pass = if first { "first" } else { pass };
            first = false;
            println!(
                "  N={palette:<2} {pass:<5} {ms:>9.1} ms  clusters {:>8}  edges {:>9}  \
                 clashes {:>7} ({:.2}%)  sampled {:>9}  from figures {:>7}  beside ancestor {:>6}  \
                 no centre {}  peak {} MiB",
                stats.clusters,
                stats.edges,
                stats.clashes,
                100.0 * stats.clashes as f64 / stats.edges.max(1) as f64,
                stats.sampled_items,
                stats.from_figures,
                stats.beside_ancestor,
                stats.without_centre,
                peak_mib(),
            );
        }
    }
}

/// The process's peak resident memory so far, from `/proc/self/status`.
fn peak_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
            line.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .map_or(0, |kb| kb / 1024)
}

fn open(root: &Path, cache: &Path, wal: &Path) -> Engine {
    Engine::open(
        root,
        cache,
        wal,
        EngineConfig {
            token_max_lifetime_secs: 3600,
            // No point is served here, so the mark budget is the smallest legal one.
            max_k: 1,
            k_min: 1,
            k_max_marks: 1,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: 0,
            flush_max_age_secs: 90,
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: mosaica_engine::CompactionSchedule::off(),
        },
    )
    .expect("the bundle opens")
}

fn credential(terms: &[String]) -> Vec<u8> {
    serde_json::json!({ "terms": terms }).to_string().into_bytes()
}

/// Every term the bundle's dictionary holds: the broadest principal it admits.
fn all_terms(root: &Path) -> Vec<String> {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT")).unwrap();
    let prefix = current["prefix"].as_str().expect("a prefix");
    let Ok(data) = std::fs::read(root.join(prefix).join("dictionary/terms-0.dict")) else {
        return vec!["public".to_string()];
    };
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes")) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        if let Ok(term) = std::str::from_utf8(&data[at..at + len]) {
            out.push(term.to_string());
        }
        at += len;
    }
    out
}
