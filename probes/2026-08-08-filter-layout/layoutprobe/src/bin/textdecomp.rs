//! Where text `contains`' 96 ns per scattered candidate entity actually goes.
//!
//! Arm 6 measured the cell — 9.7 ns contiguous, 96 ns scattered at 10⁸ — and stopped. Before any
//! accelerator is worth generating, the cost has to be split between three candidates for the
//! bottleneck: the **offsets indirection** (two `i64`s per value, a random cache line on a
//! scattered candidate), the **random access into the byte array** (a second random line), and the
//! **substring algorithm itself**. An option that only speeds the comparison is worthless if the
//! misses dominate, and one that only cuts a line is worthless if the comparison does — the
//! campaign's most expensive mistakes were all optimising the term that turned out not to bind.
//!
//! The split mirrors what `ceiling` did for the unselective case: the same traversal with the
//! predicate replaced by successively more of the real work.
//!
//! - `traverse` — walk the candidate's runs and fold the entity id; touches neither array. The
//!   traversal floor.
//! - `offsets-len` — read the value's two offsets and test its length against the needle's;
//!   touches the offsets array only. Adds the first random line.
//! - `first-byte` — offsets plus one byte load at the value's start; adds the second random line
//!   without any scanning.
//! - `fb-scan` — offsets plus a scan of the whole value for the needle's first byte, counting
//!   occurrences but never verifying; the streaming cost of the value's bytes without the
//!   comparison.
//! - `full` — the shipped `byte_contains` transcribed, hits collected and built into a bitmap.
//! - `shipped` — `ValueColumn::scan_text_contains`, the code the design quotes. `full` minus
//!   `shipped` is the reimplementation gap, which arm 4 measured at 23% for the fixed-width scan
//!   and which is why no conclusion here is drawn from an uncalibrated loop.
//!
//! `full`'s bitmap is asserted equal to `shipped`'s before any timing is printed. Checksums are
//! `black_box`ed so no arm is dead code. Values, candidates and the `-000` needle are `textscan`'s
//! exactly, so the cells line up with arm 6's published table.
//!
//! At 10⁹ the shipped column and the probe's flat arrays would together exceed RAM (22 GB each),
//! so the shipped arm runs first and the column is dropped before the flat arrays are built; the
//! equality assertion keeps only the (small) result bitmaps across that boundary.

use std::hint::black_box;
use std::time::Instant;

use croaring::Bitmap;
use tessera_filter::{Codes, ValueColumn};

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Identical to `textscan`'s generator: a small stem vocabulary with a numeric tail, so values
/// share long prefixes — the adversarial case for a byte comparison and what a real string column
/// looks like.
fn value_for(e: u64) -> String {
    const STEMS: [&str; 8] = [
        "anderson", "andrews", "andrade", "bergstrom", "bergman", "castellano", "castleton",
        "delacroix",
    ];
    let h = splitmix(e);
    format!("{}-{:05}", STEMS[(h % 8) as usize], h % 100_000)
}

fn candidates(n: u64) -> Vec<(&'static str, Bitmap)> {
    let mut contiguous = Bitmap::new();
    contiguous.add_range((n / 3) as u32..(n / 3 + n / 100) as u32);
    contiguous.run_optimize();

    let mut broad = Bitmap::new();
    broad.add_range(0u32..(n / 4) as u32);
    broad.run_optimize();

    let mut scattered = Bitmap::new();
    let mut v: Vec<u32> = Vec::new();
    for e in 0..n {
        if splitmix(e ^ 0x5EED) % 100 == 0 {
            v.push(e as u32);
        }
    }
    scattered.add_many(&v);
    scattered.run_optimize();

    vec![
        ("sparse-contiguous-1pct", contiguous),
        ("broad-25pct", broad),
        ("sparse-scattered-1pct", scattered),
    ]
}

/// The candidate's set values as inclusive runs, bulk-read through the cursor — the same shape the
/// shipped traversal uses. Universal presence, so entity id == slot.
fn for_each_run(bitmap: &Bitmap, mut f: impl FnMut(u32, u32)) {
    let mut cursor = bitmap.cursor();
    let mut buf = [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; 64];
    loop {
        let n = cursor.read_many_ranges(&mut buf);
        if n == 0 {
            return;
        }
        for r in &buf[..n] {
            f(r.start, r.last);
        }
    }
}

/// The shipped `byte_contains`, transcribed from `tessera-filter/src/values.rs` so the `full` arm
/// runs the same algorithm the shipped scan runs.
#[inline]
fn byte_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    haystack[..=last_start]
        .iter()
        .enumerate()
        .any(|(i, &b)| b == first && &haystack[i..i + needle.len()] == needle)
}

fn time_arm(n: u64, arm: &str, cand: (&str, &Bitmap), mut f: impl FnMut() -> u64) {
    let _ = black_box(f()); // warm pass
    let t = Instant::now();
    let checksum = black_box(f());
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let c = cand.1.cardinality();
    println!(
        "{n},{arm},{},{c},{ms:.3},{:.2},{checksum}",
        cand.0,
        ms * 1e6 / c as f64
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(100_000_000);
    let needle = b"-000";

    println!("n,arm,candidate,candidate_entities,ms,ns_per_candidate,checksum");

    let cands = candidates(n);

    // Shipped arm first, then drop the column, so the peak is one column's worth of memory.
    let mut shipped_results: Vec<Bitmap> = Vec::new();
    {
        let text = ValueColumn::universal(Codes::text((0..n).map(value_for)));
        for c in &cands {
            let cd = (c.0, &c.1);
            let needle_str = std::str::from_utf8(needle).unwrap();
            time_arm(n, "shipped", cd, || {
                let r = text.scan_text_contains(&c.1, needle_str);
                let card = r.cardinality();
                shipped_results.push(r);
                card
            });
            // Two invocations ran (warm + timed); keep one result per candidate.
            shipped_results.pop();
        }
    }

    // The probe's own flat column: the same values, the same layout.
    let mut bytes: Vec<u8> = Vec::new();
    let mut offsets: Vec<i64> = Vec::with_capacity(n as usize + 1);
    offsets.push(0);
    for e in 0..n {
        bytes.extend_from_slice(value_for(e).as_bytes());
        offsets.push(bytes.len() as i64);
    }

    for (ci, c) in cands.iter().enumerate() {
        let cd = (c.0, &c.1);

        time_arm(n, "traverse", cd, || {
            let mut acc = 0u64;
            for_each_run(&c.1, |s, l| {
                for e in s..=l {
                    acc = acc.wrapping_add(e as u64);
                }
            });
            acc
        });

        time_arm(n, "offsets-len", cd, || {
            let mut count = 0u64;
            for_each_run(&c.1, |s, l| {
                for e in s..=l {
                    let lo = offsets[e as usize];
                    let hi = offsets[e as usize + 1];
                    if (hi - lo) as usize >= needle.len() {
                        count += 1;
                    }
                }
            });
            count
        });

        time_arm(n, "first-byte", cd, || {
            let mut count = 0u64;
            for_each_run(&c.1, |s, l| {
                for e in s..=l {
                    let lo = offsets[e as usize] as usize;
                    let hi = offsets[e as usize + 1] as usize;
                    if hi > lo && bytes[lo] == needle[0] {
                        count += 1;
                    }
                }
            });
            count
        });

        time_arm(n, "fb-scan", cd, || {
            let mut count = 0u64;
            for_each_run(&c.1, |s, l| {
                for e in s..=l {
                    let lo = offsets[e as usize] as usize;
                    let hi = offsets[e as usize + 1] as usize;
                    count += bytes[lo..hi].iter().filter(|&&b| b == needle[0]).count() as u64;
                }
            });
            count
        });

        time_arm(n, "full", cd, || {
            let mut hits: Vec<u32> = Vec::new();
            for_each_run(&c.1, |s, l| {
                for e in s..=l {
                    let lo = offsets[e as usize] as usize;
                    let hi = offsets[e as usize + 1] as usize;
                    if byte_contains(&bytes[lo..hi], needle) {
                        hits.push(e);
                    }
                }
            });
            let mut bm = Bitmap::new();
            bm.add_many(&hits);
            assert_eq!(
                bm, shipped_results[ci],
                "full arm disagrees with the shipped scan"
            );
            bm.cardinality()
        });
    }
}
