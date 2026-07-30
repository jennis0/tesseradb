//! The measured basis for `select.rs`'s `RUN_DECODE_MIN_DENSITY_PCT` — re-runnable evidence, in
//! the same spirit as `route_saving.rs`.
//!
//! Selection's decode tier is chosen per tile from `(visible, range.len())`. This example times
//! the **general branch's** work (threshold count + peek-reject heap) under five decode
//! mechanisms across mask densities:
//!
//! - `per-value` — the retired pre-B9 mechanism: materialise `rows_in_range`, iterate it.
//! - `run-decode` — tier 1 (`for_each_visible_run`).
//! - `value-batch` — tier 2's mechanism as a local reference loop: `next_many` into a stack
//!   buffer, tight scan. Beats the retired mechanism by ~30–40% at every density.
//! - `cursor-value` — a rejected tier-2 candidate: `next()` per value, no buffer. Loses to the
//!   batch loop at every density; kept so the rejection stays re-runnable rather than folklore.
//! - `mask-batch` — tier 2 **exactly as shipped**: the same batch loop driven over
//!   `EffectiveMask::decode_source`. Must match `value-batch`; a gap means an abstraction has
//!   crept back into the hot loop. (The first draft fed batches through a closure-taking mask
//!   method and measured ~2× slower than the local loop — which is why `decode_source` exists.)
//!
//! The run/batch crossover locates `RUN_DECODE_MIN_DENSITY_PCT`; the constant is rounded
//! **upwards** (towards the batch decode) because the run mechanism's failure mode — scattered
//! mask, mean run ≈ 1-3 rows — was a measured +7.8% end-to-end regression at 2.4M, while the
//! batch decode's cost is flat in run length.
//!
//! Run with: `cargo run --release --example decode_tiers -p tessera-engine`

use std::collections::BinaryHeap;
use std::hint::black_box;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::compose::{compose, EffectiveMask, RowProjection};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::write::write_permutation;
use tessera_store::Permutation;
use tessera_types::{EntityId, TermId};

const ROWS: u32 = 1 << 20;
const TILE_SPAN: u32 = 1 << 14; // 64 tiles of 16k rows each
/// The deployment operating point (`cap = min(k, k_max_marks = 500)`, and k defaults to the
/// cap — owner directive 2026-07-30). The threshold constant is chosen from THIS sweep; the
/// secondary cap exists only to show the crossover's (in)sensitivity to cap.
const PRIMARY_CAP: usize = 500;
const SECONDARY_CAP: usize = 50;
const TRIALS: u32 = 5;
const REPS: u32 = 3;

fn main() {
    let mut rng = StdRng::seed_from_u64(0x71E5);
    let ids: Vec<u64> = (0..ROWS).map(|_| rng.gen()).collect();
    // A cut admitting about half the ids — both sides of the threshold branch stay live.
    let cut: u64 = u64::MAX / 2;

    for cap in [PRIMARY_CAP, SECONDARY_CAP] {
        println!(
            "cap = {cap}{}",
            if cap == PRIMARY_CAP {
                " (operating point — the threshold constant is read off this table)"
            } else {
                " (secondary, shape check only)"
            }
        );
        sweep(&mut rng, &ids, cut, cap);
        println!();
    }
}

fn sweep(rng: &mut StdRng, ids: &[u64], cut: u64, cap: usize) {
    println!(
        "{:>8} {:>14} {:>14} {:>14} {:>14} {:>14}   (ns/visible row, best of {TRIALS}x{REPS})",
        "density", "per-value", "run-decode", "value-batch", "cursor-value", "mask-batch"
    );

    for density in [0.30, 0.50, 0.70, 0.80, 0.85, 0.90, 0.95, 0.99, 1.00] {
        let visible: Vec<u32> = (0..ROWS).filter(|_| rng.gen_bool(density)).collect();
        let (_t, mask) = mask_over(&visible, ROWS);
        let ranges: Vec<Range<u32>> = (0..ROWS / TILE_SPAN)
            .map(|i| i * TILE_SPAN..(i + 1) * TILE_SPAN)
            .collect();
        // Per-range visible counts, precomputed as the real caller has them (the count stage
        // runs before selection); the shipped shape uses them to bound its decode reads.
        let range_vis: Vec<(Range<u32>, u64)> = ranges
            .iter()
            .map(|r| (r.clone(), mask.count_range(r.clone())))
            .collect();
        let total_visible: u64 = range_vis.iter().map(|(_, v)| v).sum();

        let per_value = best_of(|| {
            let mut acc = 0u64;
            for r in &ranges {
                acc ^= general_per_value(&mask, ids, r.clone(), cut, cap);
            }
            acc
        });
        let runs = best_of(|| {
            let mut acc = 0u64;
            for r in &ranges {
                acc ^= general_runs(&mask, ids, r.clone(), cut, cap);
            }
            acc
        });
        // The batch mechanism reads the raw visible bitmap — identical to the mask's base, since
        // this example's mask has empty diffs.
        let vis_bitmap = croaring::Bitmap::of(&visible);
        let batches = best_of(|| {
            let mut acc = 0u64;
            for r in &ranges {
                acc ^= general_batches(&vis_bitmap, ids, r.clone(), cut, cap);
            }
            acc
        });
        let cursor_vals = best_of(|| {
            let mut acc = 0u64;
            for r in &ranges {
                acc ^= general_cursor_values(&mask, ids, r.clone(), cut, cap);
            }
            acc
        });
        let mask_batches = best_of(|| {
            let mut acc = 0u64;
            for (r, vis) in &range_vis {
                acc ^= general_mask_batches(&mask, ids, r.clone(), cut, cap, *vis);
            }
            acc
        });

        println!(
            "{density:>8.2} {:>14.3} {:>14.3} {:>14.3} {:>14.3} {:>14.3}",
            per_value as f64 / total_visible as f64,
            runs as f64 / total_visible as f64,
            batches as f64 / total_visible as f64,
            cursor_vals as f64 / total_visible as f64,
            mask_batches as f64 / total_visible as f64,
        );
    }
}

/// Best-of timing of `f`, in nanoseconds per invocation. Each closure result is folded into a
/// checksum and black-boxed, and the three mechanisms' checksums are computed identically — the
/// compiler can elide none of them and any divergence would surface in the number itself.
fn best_of(mut f: impl FnMut() -> u64) -> u128 {
    black_box(f()); // warm
    (0..TRIALS)
        .map(|_| {
            let t0 = Instant::now();
            for _ in 0..REPS {
                black_box(f());
            }
            t0.elapsed().as_nanos() / REPS as u128
        })
        .min()
        .expect("TRIALS is non-zero")
}

/// The general branch's per-tile work, checksummed: `c_theta`, folded with the heap's survivors.
fn fold(c_theta: u64, heap: BinaryHeap<(u64, u32)>) -> u64 {
    heap.into_iter()
        .fold(c_theta, |acc, (id, row)| acc ^ id ^ row as u64)
}

/// The retired mechanism: per-value iteration over the materialised range bitmap.
fn general_per_value(mask: &EffectiveMask, ids: &[u64], r: Range<u32>, cut: u64, cap: usize) -> u64 {
    let mut c_theta = 0u64;
    let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(cap + 1);
    for row in mask.rows_in_range(r).iter() {
        let id = ids[row as usize];
        if id < cut {
            c_theta += 1;
        }
        if heap.len() == cap {
            if id >= heap.peek().expect("non-empty").0 {
                continue;
            }
            heap.pop();
        }
        heap.push((id, row));
    }
    fold(c_theta, heap)
}

/// Tier 1: contiguous-slice scan per run.
fn general_runs(mask: &EffectiveMask, ids: &[u64], r: Range<u32>, cut: u64, cap: usize) -> u64 {
    let mut c_theta = 0u64;
    let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(cap + 1);
    mask.for_each_visible_run(r, |run| {
        let slice = &ids[run.start as usize..run.end as usize];
        c_theta += slice.iter().filter(|&&id| id < cut).count() as u64;
        for (i, &id) in slice.iter().enumerate() {
            if heap.len() == cap {
                if id >= heap.peek().expect("non-empty").0 {
                    continue;
                }
                heap.pop();
            }
            heap.push((id, run.start + i as u32));
        }
    });
    fold(c_theta, heap)
}

/// The rejected candidate: `next_many` into a stack buffer, then scan the buffer. Implemented
/// locally — the engine deliberately carries no batch decoder, and this function is the evidence
/// for why.
fn general_batches(bitmap: &croaring::Bitmap, ids: &[u64], r: Range<u32>, cut: u64, cap: usize) -> u64 {
    const BUF: usize = 1024; // 256 and 4096 were also tried; the ordering never changed
    let mut c_theta = 0u64;
    let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(cap + 1);
    let mut iter = bitmap.iter();
    iter.reset_at_or_after(r.start);
    let mut buf = [0u32; BUF];
    'outer: loop {
        let n = iter.next_many(&mut buf);
        if n == 0 {
            break;
        }
        for &row in &buf[..n] {
            if row >= r.end {
                break 'outer;
            }
            let id = ids[row as usize];
            if id < cut {
                c_theta += 1;
            }
            if heap.len() == cap {
                if id >= heap.peek().expect("non-empty").0 {
                    continue;
                }
                heap.pop();
            }
            heap.push((id, row));
        }
    }
    fold(c_theta, heap)
}

/// **Tier 2 as shipped**: the batch loop driven by the caller over
/// `EffectiveMask::decode_source` — the exact shape `Selection::of`'s value tier compiles. Must
/// match the local `value-batch` column; a gap here means an abstraction is eating the win
/// again, which is the regression this column exists to catch.
fn general_mask_batches(
    mask: &EffectiveMask,
    ids: &[u64],
    r: Range<u32>,
    cut: u64,
    cap: usize,
    vis: u64,
) -> u64 {
    let mut c_theta = 0u64;
    let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(cap + 1);
    let source = mask.decode_source(r.clone());
    let mut iter = source.bitmap().iter();
    iter.reset_at_or_after(r.start);
    let mut buf = [0u32; 1024];
    let mut remaining = vis;
    'outer: while remaining > 0 {
        let want = remaining.min(1024) as usize;
        let n = iter.next_many(&mut buf[..want]);
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        for &row in &buf[..n] {
            if row >= r.end {
                break 'outer;
            }
            let id = ids[row as usize];
            if id < cut {
                c_theta += 1;
            }
            if heap.len() == cap {
                if id >= heap.peek().expect("non-empty").0 {
                    continue;
                }
                heap.pop();
            }
            heap.push((id, row));
        }
    }
    fold(c_theta, heap)
}

/// A rejected tier-2 candidate: per-value cursor walk (`next()` per value, no buffer) over the
/// decode source. Loses to the batch loop at every density — kept so the rejection stays
/// re-runnable.
fn general_cursor_values(mask: &EffectiveMask, ids: &[u64], r: Range<u32>, cut: u64, cap: usize) -> u64 {
    let mut c_theta = 0u64;
    let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(cap + 1);
    let source = mask.decode_source(r.clone());
    let mut iter = source.bitmap().iter();
    iter.reset_at_or_after(r.start);
    for row in iter {
        if row >= r.end {
            break;
        }
        let id = ids[row as usize];
        if id < cut {
            c_theta += 1;
        }
        if heap.len() == cap {
            if id >= heap.peek().expect("non-empty").0 {
                continue;
            }
            heap.pop();
        }
        heap.push((id, row));
    }
    fold(c_theta, heap)
}

/// An [`EffectiveMask`] over exactly `visible_rows` — the identity-permutation fixture the
/// integration tests use, sized up.
fn mask_over(visible_rows: &[u32], row_count: u32) -> (TempDir, EffectiveMask) {
    let temp = TempDir::new().unwrap();
    let bound = row_count as u64;

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &[visible_rows.to_vec()], 32).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();

    let cache = FragmentCache::new(&temp.path().join("cache"), [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&[TermId::new(0)], [3u8; 32], &postings, bound)
        .unwrap();

    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, bound).unwrap();
    let perm = Permutation::load(&perm_path).unwrap();

    let base = Arc::new(RowProjection::new(&fragment, &perm));
    let satisfied: FxHashSet<TermId> = [TermId::new(0)].into_iter().collect();
    let mask = compose(
        &fragment,
        &satisfied,
        &Overlay::default(),
        &IngestBuffer::default(),
        base,
        &perm,
    );
    (temp, mask)
}
