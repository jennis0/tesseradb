//! **P1 and P2** — the two measurements decision 0044 requires before its refresh mechanism is
//! coded, run over a synthetic row space rather than a built bundle.
//!
//! 0044's D1 budget is *zero update-induced request-path work in the steady state*, with a bounded
//! 429 residual for same-key racers inside a merge's refresh window. Two numbers size that window
//! and decide what may run inside it:
//!
//! - **P1 — the projection ladder.** Four costs per cached entry, at one scale: the bitmap
//!   **clone** a flush's patch pays; the **union** it adds over the new extent; the **span rebase**
//!   a row-space merge needs (clear the merged span's row range, re-project the span only); and the
//!   full **rebuild** everything falls back to. What the ladder decides is which rungs may be paid
//!   per publication and which must be background work with a 429 in front of them.
//! - **P2 — the fragment build against tier count.** `build_fragment_with_deltas` per credential,
//!   over 1/8/64/512 tiers, splitting the base union (fixed) from the per-tier term probes (the
//!   term the entity-space coalesce bounds). What it decides is how urgent §11.2's incremental
//!   fragment form is — plan Task 12's remainder.
//!
//! **Synthetic, and that is a stated limitation rather than a convenience.** These build a
//! `permutation.bin` and a postings file directly, so the numbers are the *primitives'* costs at
//! the shape a wide grant produces — not an end-to-end request. Nothing here measures a bundle's
//! IO, its cache residency or its contention. The earlier 10.7 s / 125.12 MB figures came from a
//! real 10⁹ bundle (`probes/2026-07-30-1e9-rebuild/`) and remain the reference; this probe's job
//! is the *ratios between the rungs*, which a synthetic row space gives faithfully because every
//! rung walks the same slot array and the same bitmap.
//!
//! Run:
//! ```text
//! cargo run --release --example refresh_probe -p tessera-engine -- [--entities N] [--grant F] [--dir PATH]
//! ```
//! At the default 10⁹ entities the permutation file is **4 GB on disk and mapped**; the process
//! peaks around 6 GB. `--entities 100000000` is the 10⁸ rehearsal.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use croaring::Bitmap;

use tessera_authz::{build_fragment_with_deltas, write_delta_tier, DeltaTier, PostingsReader};
use tessera_store::permutation::SegmentExtent;
use tessera_store::write::write_permutation_iter;
use tessera_store::{Permutation, RowSpace};
use tessera_types::{EntityId, TermId, ROW_ABSENT};

/// How many entities the synthetic row space covers. 10⁹ is the scale every quoted figure in the
/// corpus is at.
const DEFAULT_ENTITIES: u64 = 1_000_000_000;
/// The fraction of entities a wide grant satisfies. 25% is the coverage the row-projection cache
/// is sized against (`crate::cache`'s 125.12 MB-per-entry note).
const DEFAULT_GRANT: f64 = 0.25;
/// One flush's entities — what a patch unions in, and the unit a merge's span is built from.
const FLUSH_ROWS: u64 = 100_000;
/// How many flush extents a row-space merge collapses. `MergePolicy::tier_width`.
const MERGE_WIDTH: u64 = 4;
const REPS: usize = 5;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let entities: u64 = arg(&args, "--entities")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ENTITIES);
    let grant: f64 = arg(&args, "--grant")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_GRANT);
    let dir = PathBuf::from(
        arg(&args, "--dir").unwrap_or_else(|| "/tmp/tessera-refresh-probe".to_string()),
    );
    std::fs::create_dir_all(&dir).expect("the probe directory");

    println!("# refresh_probe — P1/P2 for decision 0044");
    println!("entities={entities} grant={grant} flush_rows={FLUSH_ROWS} merge_width={MERGE_WIDTH}");
    println!("threads={}", rayon::current_num_threads());

    p1(&dir, entities, grant);
    p2(&dir, entities, grant);
}

/// **P1 — the projection ladder.**
fn p1(dir: &std::path::Path, entities: u64, grant: f64) {
    println!("\n## P1 — projection ladder");

    // The base permutation: every entity below the flush region occupies its own row, so the row
    // space is dense and `project` walks the same slot array a real one does. Written through the
    // one writer that knows the format, never hand-rolled here.
    let base_bound = entities;
    let perm_path = dir.join("permutation.bin");
    if !perm_path.exists() {
        let t = Instant::now();
        write_permutation_iter(&perm_path, (0..base_bound).map(EntityId::new), base_bound)
            .expect("the synthetic permutation writes");
        println!(
            "fixture: permutation.bin {:.1} GB in {:.1}s",
            (base_bound as f64 * 4.0) / 1e9,
            t.elapsed().as_secs_f64()
        );
    }
    let base = Arc::new(Permutation::load(&perm_path).expect("it loads"));
    let base_rows = u32::try_from(base_bound).expect("bundle_format 1 is u32 rows");

    // The grant: every `stride`-th entity, which is the *worst* shape for a Roaring mask — a
    // uniformly scattered 25% is bitmap-container dominated end to end, which is exactly the
    // arrangement the 125.12 MB per-entry figure describes.
    let stride = (1.0 / grant).round().max(1.0) as u64;
    let mut fragment = Bitmap::new();
    for e in (0..base_bound).step_by(stride as usize) {
        fragment.add(e as u32);
    }
    fragment.run_optimize();
    println!(
        "grant: {} entities, {:.2} MB serialised",
        fragment.cardinality(),
        fragment.get_serialized_size_in_bytes::<croaring::Portable>() as f64 / 1e6
    );

    // `MERGE_WIDTH` flush extents above the base — the state a merge selects over.
    let mut space = RowSpace::new(Arc::clone(&base), base_rows);
    let mut row_base = base_rows;
    for i in 0..MERGE_WIDTH {
        let lo = base_bound + i * FLUSH_ROWS;
        let extent = SegmentExtent {
            entity_lo: lo,
            entity_hi: lo + FLUSH_ROWS - 1,
            seg_id: format!("flush-{i}"),
            row_base,
            rows: (0..FLUSH_ROWS as u32).collect(),
        };
        for e in lo..lo + FLUSH_ROWS {
            if e % stride == 0 {
                fragment.add(e as u32);
            }
        }
        space = space.with_extent(extent).expect("it continues row space");
        row_base += FLUSH_ROWS as u32;
    }

    let t = Instant::now();
    let full = space.project(&fragment);
    let rebuild = t.elapsed();
    println!(
        "rebuild:      {:>9.1} ms   ({} rows, {:.2} MB)",
        rebuild.as_secs_f64() * 1e3,
        full.cardinality(),
        full.get_serialized_size_in_bytes::<croaring::Portable>() as f64 / 1e6
    );

    let mut clone_ns = Vec::new();
    let mut union_ns = Vec::new();
    let mut span_ns = Vec::new();
    for _ in 0..REPS {
        let t = Instant::now();
        let mut patched = full.clone();
        clone_ns.push(t.elapsed().as_nanos() as u64);

        let t = Instant::now();
        patched.or_inplace(&space.project_extents_from(&fragment, space.extent_count() - 1));
        union_ns.push(t.elapsed().as_nanos() as u64);

        // The span rebase: clear the merged span's row range and re-project that span only. This
        // is the operation decision 0044's D1 puts inside the background refresh window, and the
        // whole reason it is not a full rebuild is that everything outside the span is exact.
        let span_lo = space.extents()[0].row_base;
        let span_hi = row_base;
        let t = Instant::now();
        let mut rebased = full.clone();
        rebased.remove_range(span_lo..span_hi);
        rebased.or_inplace(&space.project_extents_from(&fragment, 0));
        span_ns.push(t.elapsed().as_nanos() as u64);
        assert_eq!(rebased.cardinality(), full.cardinality(), "span rebase is exact");
    }
    report("clone (patch's fixed cost)", &clone_ns);
    report("union over one new extent", &union_ns);
    report("span rebase over a merge", &span_ns);
    println!(
        "ratio: rebuild / span_rebase = {:.0}x",
        rebuild.as_secs_f64() * 1e9 / median(&span_ns) as f64
    );
}

/// **P2 — `build_fragment_with_deltas` against tier count.**
fn p2(dir: &std::path::Path, entities: u64, grant: f64) {
    println!("\n## P2 — fragment build vs tier count");

    // One credential's satisfied terms. Eight is the shape a realistic grant has: a handful of
    // compartment terms whose postings union to the wide grant.
    const TERMS: usize = 8;
    let stride = (1.0 / grant).round().max(1.0) as u64;
    let postings_path = dir.join("postings.arrow");
    if !postings_path.exists() {
        let mut per_term: Vec<Vec<u32>> = vec![Vec::new(); TERMS];
        for e in (0..entities).step_by(stride as usize) {
            per_term[(e as usize / stride as usize) % TERMS].push(e as u32);
        }
        let t = Instant::now();
        tessera_authz::write_postings(&postings_path, &per_term, 32).expect("postings write");
        println!("fixture: postings.arrow in {:.1}s", t.elapsed().as_secs_f64());
    }
    let postings = PostingsReader::open(&postings_path, true).expect("postings open");
    let terms: Vec<TermId> = (0..TERMS as u32).map(TermId::new).collect();

    // Tiers carry one tick's arrivals: a handful of entities per term.
    let tier_dir = dir.join("tiers");
    std::fs::create_dir_all(&tier_dir).expect("tier dir");
    let mut tiers: Vec<Arc<DeltaTier>> = Vec::new();
    for count in [0usize, 1, 8, 64, 512] {
        while tiers.len() < count {
            let i = tiers.len();
            let path = tier_dir.join(format!("delta-{i}.arrow"));
            if !path.exists() {
                let entries: Vec<(TermId, Vec<u32>)> = (0..TERMS as u32)
                    .map(|t| {
                        (
                            TermId::new(t),
                            (0..16).map(|k| (entities as u32) + (i as u32 * 64) + k).collect(),
                        )
                    })
                    .collect();
                write_delta_tier(&path, &entries, 32).expect("tier write");
            }
            tiers.push(Arc::new(DeltaTier::open(&path).expect("tier open")));
        }
        let mut ns = Vec::new();
        for _ in 0..REPS {
            let t = Instant::now();
            let fragment =
                build_fragment_with_deltas(&terms, &postings, &tiers).expect("fragment builds");
            ns.push(t.elapsed().as_nanos() as u64);
            std::hint::black_box(fragment);
        }
        report(&format!("fragment build, {count:>3} tiers"), &ns);
    }
}

fn median(ns: &[u64]) -> u64 {
    let mut v = ns.to_vec();
    v.sort_unstable();
    v[v.len() / 2]
}

fn report(label: &str, ns: &[u64]) {
    let med = median(ns);
    let max = ns.iter().copied().max().unwrap_or(0);
    println!(
        "{label:<30} median {:>9.3} ms   max {:>9.3} ms",
        med as f64 / 1e6,
        max as f64 / 1e6
    );
}

/// Referenced so the sentinel's meaning is checked rather than assumed by the fixture above.
#[allow(dead_code)]
const _ROW_ABSENT_IS_THE_SENTINEL: u32 = ROW_ABSENT;
