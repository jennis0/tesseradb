//! Arm 13 — what layer accumulation costs: N per-flush extents between folds.
//!
//! A column on the read path is base + one layer per live extent, scanned in turn and unioned
//! (`filter-index.md` §5; `tessera-engine`'s `FilterColumns::resolve`). Every flush adds a layer
//! and nothing removes one until the fold, so both the per-request scan and the generation open
//! walk a list that grows at flush rate. This arm turns "layers cost something" into a curve:
//!
//! - **Phase A** times the shipped `ValueColumn` resolve loop — base scan plus one scan per
//!   extent, results unioned, exactly `FilterColumns::resolve`'s shape — at N layers for the
//!   candidate shapes the campaign already uses. The per-layer increment is the number the design
//!   needs: the fixed cost of `candidate ∧ presence` plus scan setup, paid per layer whatever the
//!   layer holds.
//! - **Phase B** writes N real extent file pairs and times the open path: `open_extent` (mmap)
//!   plus the disjointness check `FilterColumns::compose` performs, and separately a full read of
//!   every file, which brackets what the open-time digest sweep pays per file before hashing.
//!
//! Extent shape modelled: contiguous entity ranges above the base high-water, `per_flush` entities
//! each — the single-slice shape. Concurrent multi-slice ingest interleaves holes (write-path
//! §4.2), which adds presence runs per extent; the per-layer floor measured here is therefore a
//! floor, not a ceiling, and the harness notes it rather than modelling every shape.

use std::time::Instant;

use croaring::Bitmap;
use tessera_filter::{write_extent, Codes, ValueColumn};
use tessera_types::AttrLocalId;

const DOMAIN: u32 = 1_000;
const NEEDLE: u32 = 42;

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base_n: u64 = args.get(1).map_or(1_000_000_000, |s| s.parse().expect("base_n"));
    let per_flush: u64 = args.get(2).map_or(25_000, |s| s.parse().expect("per_flush"));
    let max_layers: usize = args.get(3).map_or(960, |s| s.parse().expect("max_layers"));
    let counts: Vec<usize> = [0usize, 8, 64, 240, 960]
        .into_iter()
        .filter(|&k| k <= max_layers)
        .collect();

    // ---- Phase A: the resolve loop, in memory --------------------------------------------------
    let values: Vec<u32> = (0..base_n).map(|e| (splitmix(e) % DOMAIN as u64) as u32).collect();
    let base = ValueColumn::universal(Codes::U32(values.into()));

    let mut extents: Vec<ValueColumn> = Vec::with_capacity(max_layers);
    for i in 0..max_layers as u64 {
        let lo = base_n + i * per_flush;
        let vals: Vec<u32> = (0..per_flush)
            .map(|j| (splitmix(lo + j) % DOMAIN as u64) as u32)
            .collect();
        let mut present = Bitmap::new();
        present.add_range(lo as u32..(lo + per_flush) as u32);
        present.run_optimize();
        extents.push(ValueColumn::partial(Codes::U32(vals.into()), present).expect("counts agree"));
    }

    let total = base_n + max_layers as u64 * per_flush;
    let mut broad = Bitmap::new();
    broad.add_range(0u32..(total / 4) as u32);
    broad.run_optimize();
    // The tail candidate covers the extents too — every layer's AND is non-empty, which is the
    // expensive direction for the per-layer floor.
    let mut full = Bitmap::new();
    full.add_range(0u32..total as u32);
    full.run_optimize();
    let mut scattered = Bitmap::new();
    {
        let mut v: Vec<u32> = Vec::new();
        for e in 0..total {
            if splitmix(e ^ 0x5EED) % 100 == 0 {
                v.push(e as u32);
            }
        }
        scattered.add_many(&v);
    }
    scattered.run_optimize();

    println!("phase,base_n,per_flush,layers,candidate,candidate_entities,ms,hits");
    let needle = AttrLocalId::new(NEEDLE);
    for (name, cand) in [
        ("broad-25pct", &broad),
        ("full-corpus", &full),
        ("scattered-1pct", &scattered),
    ] {
        for &k in &counts {
            let resolve = || {
                let mut out = base.scan_eq(cand, needle);
                for e in &extents[..k] {
                    out |= e.scan_eq(cand, needle);
                }
                out
            };
            let _ = resolve(); // warm
            let mut times = Vec::new();
            let mut hits = 0;
            for _ in 0..3 {
                let t = Instant::now();
                let out = resolve();
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                hits = out.cardinality();
            }
            println!(
                "resolve,{base_n},{per_flush},{k},{name},{},{:.3},{hits}",
                cand.cardinality(),
                median(times)
            );
        }
    }

    // ---- Phase B: the open path, on disk -------------------------------------------------------
    // Real extent files for one column; open+compose cost scales linearly in columns, so one
    // column's curve is the per-column constant.
    let dir = std::env::temp_dir().join("layoutprobe-layers");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut paths = Vec::with_capacity(max_layers);
    for i in 0..max_layers as u64 {
        let lo = base_n + i * per_flush;
        let vals: Vec<u32> = (0..per_flush)
            .map(|j| (splitmix(lo + j) % DOMAIN as u64) as u32)
            .collect();
        let mut present = Bitmap::new();
        present.add_range(lo as u32..(lo + per_flush) as u32);
        present.run_optimize();
        let pair = write_extent(&dir, &format!("flush-{i:06}"), &Codes::U32(vals.into()), &present)
            .expect("write extent");
        paths.push(pair);
    }

    for &k in &counts {
        if k == 0 {
            continue;
        }
        // Open + compose, as `FilterColumns::open` does per extent: mmap both files, then the
        // disjointness check against the accumulated coverage.
        let t = Instant::now();
        let mut covered = Bitmap::new();
        for (values_path, presence_path) in &paths[..k] {
            let col = tessera_filter::open_extent(values_path, presence_path, true)
                .expect("open extent");
            let present = col.present();
            assert_eq!(covered.and_cardinality(&present), 0, "layers must be disjoint");
            covered |= present;
        }
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;

        // Full read of every file — the IO half of what the open-time digest sweep costs for the
        // same set (hashing adds CPU at O(bytes) on top; the per-file constant is what small
        // files are dominated by).
        let t = Instant::now();
        let mut bytes = 0u64;
        for (values_path, presence_path) in &paths[..k] {
            bytes += std::fs::read(values_path).expect("read").len() as u64;
            bytes += std::fs::read(presence_path).expect("read").len() as u64;
        }
        let sweep_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("open-compose,{base_n},{per_flush},{k},files,{},{open_ms:.3},0", 2 * k);
        println!("read-sweep,{base_n},{per_flush},{k},bytes,{bytes},{sweep_ms:.3},0");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
