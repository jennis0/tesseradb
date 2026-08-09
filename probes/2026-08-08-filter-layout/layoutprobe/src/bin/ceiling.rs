//! **How much of an unselective filter is the scan, and how much is building the answer?**
//!
//! Arm 7 established that a predicate matching a large fraction of a big candidate costs seconds,
//! and that nearly all of it is downstream of the comparison. This bounds what any fix could buy,
//! by timing the same traversal three ways over the same data:
//!
//! - **count** — the predicate runs and matches are counted, nothing is built. The floor: no result
//!   construction can be cheaper than not constructing one.
//! - **collect** — matches are appended to a `Vec<u32>` and nothing more. Isolates the buffer from
//!   the bitmap.
//! - **bitmap** — `Vec` then one bulk `add_many`, which is what the accumulator does for scattered
//!   matches.
//!
//! The gap between *count* and *bitmap* is the whole prize available to a better result
//! representation; the gap between *collect* and *bitmap* is croaring's share of it.

use std::time::Instant;

use croaring::Bitmap;

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(1_000_000_000);

    let values: Vec<u8> = (0..n as u64).map(|e| (splitmix(e) % 100) as u8).collect();

    println!("n,selectivity_pct,stage,ms,hits");

    for pct in [1u8, 2, 5, 10, 20, 30, 50, 70, 90, 99, 100] {
        // count — the traversal and the comparison alone.
        let t = Instant::now();
        let mut count = 0u64;
        for v in &values {
            if *v < pct {
                count += 1;
            }
        }
        std::hint::black_box(count);
        println!("{n},{pct},count,{:.1},{count}", t.elapsed().as_secs_f64() * 1000.0);

        // collect — the same, appending each match to a flat buffer.
        let t = Instant::now();
        let mut buf: Vec<u32> = Vec::new();
        for (i, v) in values.iter().enumerate() {
            if *v < pct {
                buf.push(i as u32);
            }
        }
        let collect_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("{n},{pct},collect,{collect_ms:.1},{}", buf.len());

        // collect-branchless — the same result, written without a data-dependent branch: the entity
        // is stored unconditionally and the cursor advances by the predicate. At middling
        // selectivity the branch above is a coin flip and costs a misprediction per element, which
        // is why `count` is four times dearer at 25% than at 100%.
        // Chunked, so the comparison is against the accumulator's actual shape: a reusable 64 Ki
        // buffer rather than one allocation the size of the corpus.
        const CH: usize = 1 << 16;
        let t = Instant::now();
        let mut scratch: Vec<u32> = vec![0; CH];
        let mut total = 0usize;
        let mut sink: Vec<u32> = Vec::new();
        for (base, block) in values.chunks(CH).enumerate() {
            let mut k = 0usize;
            for (i, v) in block.iter().enumerate() {
                scratch[k] = (base * CH + i) as u32;
                k += usize::from(*v < pct);
            }
            total += k;
            // Consume the chunk so the work is not optimised away, at the cost the accumulator
            // pays: one bulk hand-off per chunk.
            if sink.len() < CH {
                sink.extend_from_slice(&scratch[..k.min(CH - sink.len())]);
            }
        }
        std::hint::black_box(&sink);
        println!(
            "{n},{pct},collect_branchless,{:.1},{total}",
            t.elapsed().as_secs_f64() * 1000.0
        );

        // bitmap — one bulk add of the collected matches.
        let t = Instant::now();
        let mut out = Bitmap::new();
        out.add_many(&buf);
        let add_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("{n},{pct},add_many,{add_ms:.1},{}", out.cardinality());

        // bitmap, run-optimised — what a dense result costs to compress once built.
        let t = Instant::now();
        out.run_optimize();
        println!(
            "{n},{pct},run_optimize,{:.1},{}",
            t.elapsed().as_secs_f64() * 1000.0,
            out.cardinality()
        );
        drop(buf);
    }
}
