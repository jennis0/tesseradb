//! The **shipped** masked scan, timed — `tessera_filter::ValueColumn`, not a reimplementation.
//!
//! The other binaries in this campaign write their own scan loop, which is correct for comparing
//! storage *layouts* against each other but means their constants describe the approach rather than
//! the code that runs. This one calls the real reader, so an optimisation to it is measured against
//! itself rather than against a model that may have drifted.
//!
//! Same shapes as `main.rs` so the numbers are directly comparable: a contiguous candidate at 1%
//! and 25%, and a scattered one at 1%.

use std::time::Instant;

use croaring::Bitmap;
use tessera_filter::{Codes, ValueColumn};
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scales: Vec<u64> = if args.len() > 1 {
        args[1..].iter().map(|s| s.parse().expect("scale")).collect()
    } else {
        vec![100_000_000]
    };

    println!("n,presence,candidate,candidate_entities,ms,ns_per_candidate,hits");

    for &n in &scales {
        let values: Vec<u32> = (0..n).map(|e| (splitmix(e) % DOMAIN as u64) as u32).collect();
        let universal = ValueColumn::universal(Codes::U32(values.into()));

        // A **slice-blocked** partial column: ten slices interleaved in 10⁵-entity runs, which is
        // the shape concurrent multi-slice ingest produces (write-path §4.2). This exercises the
        // presence path, which the universal column never touches — and which arm 1 measured as the
        // campaign's worst cell.
        let (partial, present_count) = {
            let mut present = Bitmap::new();
            let mut held: Vec<u32> = Vec::new();
            let mut batch: Vec<u32> = Vec::with_capacity(1 << 16);
            for e in 0..n {
                if (e / 100_000) % 10 == 0 {
                    batch.push(e as u32);
                    held.push((splitmix(e) % DOMAIN as u64) as u32);
                    if batch.len() == batch.capacity() {
                        present.add_many(&batch);
                        batch.clear();
                    }
                }
            }
            present.add_many(&batch);
            present.run_optimize();
            let count = present.cardinality();
            (
                ValueColumn::partial(Codes::U32(held.into()), present).expect("counts agree"),
                count,
            )
        };
        eprintln!("n={n}: partial column holds {present_count} values");

        let mut sparse = Bitmap::new();
        sparse.add_range((n / 3) as u32..(n / 3 + n / 100) as u32);
        sparse.run_optimize();

        let mut broad = Bitmap::new();
        broad.add_range(0u32..(n / 4) as u32);
        broad.run_optimize();

        let mut scattered = Bitmap::new();
        {
            let mut v: Vec<u32> = Vec::new();
            for e in 0..n {
                if splitmix(e ^ 0x5EED) % 100 == 0 {
                    v.push(e as u32);
                }
            }
            scattered.add_many(&v);
        }
        scattered.run_optimize();

        for (shape, column) in [("universal", &universal), ("slice-blocked", &partial)] {
            for (name, cand) in [
                ("sparse-contiguous-1pct", &sparse),
                ("broad-25pct", &broad),
                ("sparse-scattered-1pct", &scattered),
            ] {
                let needle = AttrLocalId::new(NEEDLE);
                // One untimed pass so the measurement is warm.
                let _ = column.scan_eq(cand, needle);
                let t = Instant::now();
                let hits = column.scan_eq(cand, needle);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let candidates = cand.cardinality();
                println!(
                    "{n},{shape},{name},{candidates},{ms:.2},{:.2},{}",
                    ms * 1e6 / candidates as f64,
                    hits.cardinality()
                );
            }
        }
    }
}
