//! Does the category posting actually close the gap the masked scan leaves at broad coverage?
//!
//! The layout probe measured the scan at ~2.9 ns per candidate entity, so a 25% principal at 10⁹
//! costs ~730 ms against a 158–191 ms selection-path operating point. An accelerator is therefore
//! required wherever coverage can be broad — but *whether the built keyed postings are that
//! accelerator* is a separate question, and the corpus already warns that it might not be: the
//! same coverage costs 21.7 ms or 2,885 ms depending only on posting shape, a 13× per-container
//! constant between the array and bitmap regimes.
//!
//! Three evaluation forms over the same relation, all answering `value == NEEDLE ∧ candidate`:
//!
//! - **scan** — the flat column baseline, O(candidate entities).
//! - **intersect** — `posting(v) ∧ candidate`, what the built reader does today. O(min containers),
//!   and *value-dependent*: an absent code returns instantly, which is the residual channel D4
//!   registers.
//! - **probe** — candidate-driven: walk the candidate's containers and probe the posting per
//!   container. Equalises the probe *count* at Θ(candidate containers) whatever the value. The
//!   question here is what that costs, since D4 recommends it for `per_viewer` categories.
//!
//! Two posting shapes bracket the deployment-dependent spread the category-membership probe
//! measured at 0.31× (correlated with the label set) and 1.01× (orthogonal control).

use std::time::Instant;

use croaring::{Bitmap, Portable};

/// `0.0` is the value that exists in the vocabulary but has **no members** — the case D4's
/// channel is about. Timing it beside a broad hidden value is what says whether the residual is
/// small enough to register or large enough to refuse.
const NEEDLE_SHARES: [f64; 4] = [0.0, 0.001, 0.01, 0.25];

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// The value's members are one contiguous block — an attribute perfectly correlated with the
    /// signature sort. The cheap end of the measured 0.31–1.01× spread, and the array-container
    /// regime at 114 ns.
    Correlated,
    /// The value's members are scattered uniformly — an attribute orthogonal to the label set.
    /// The 1.01× end, and where the 1.54 µs bitmap-container constant bites.
    Scattered,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Shape::Correlated => "correlated",
            Shape::Scattered => "scattered",
        }
    }
}

/// Entities carrying `NEEDLE`, under one shape and share.
fn needle_members(n: u64, shape: Shape, share: f64) -> Bitmap {
    let mut b = Bitmap::new();
    match shape {
        Shape::Correlated if share == 0.0 => {}
        Shape::Scattered if share == 0.0 => {}
        Shape::Correlated => {
            // Placed at n/8 so the block straddles the 25% candidate's upper boundary rather
            // than nesting inside it — nesting would flatter the intersection.
            let lo = n / 8;
            let len = (n as f64 * share) as u64;
            b.add_range(lo as u32..(lo + len).min(n) as u32);
        }
        Shape::Scattered => {
            let stride = (1.0 / share) as u64;
            let mut batch: Vec<u32> = Vec::with_capacity(1 << 16);
            for e in 0..n {
                if splitmix(e ^ 0xC0FF) % stride == 0 {
                    batch.push(e as u32);
                    if batch.len() == batch.capacity() {
                        b.add_many(&batch);
                        batch.clear();
                    }
                }
            }
            b.add_many(&batch);
        }
    }
    b.run_optimize();
    b
}

/// The flat column: value 0 means "not the needle", 1 means it is. A `u8` rather than the layout
/// probe's `u32`, because a category code is narrow — which also makes this the *most* favourable
/// width for the scan, and so the most conservative comparison for the accelerator.
fn flat_column(n: u64, members: &Bitmap) -> Vec<u8> {
    let mut v = vec![0u8; n as usize];
    for e in members.iter() {
        v[e as usize] = 1;
    }
    v
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scales: Vec<u64> = if args.len() > 1 {
        args[1..].iter().map(|s| s.parse().expect("scale")).collect()
    } else {
        vec![100_000_000]
    };

    println!("n,shape,needle_share,coverage,form,posting_bytes,column_bytes,ms,hits");

    for &n in &scales {
        // The candidate: a broad principal. 25% is the corner the layout probe identified; 100%
        // is the ceiling, and both are contiguous, which is the regime signature sorting is meant
        // to produce.
        // "hidden" is the case D4 turns on: a 25% candidate placed **disjoint** from a
        // correlated value's members, so the principal can see none of them. Timed against
        // `share = 0` — a value with no members at all — it is exactly the pair
        // per-point-attributes §3.8 requires to be indistinguishable in work. It is only
        // constructible for the correlated shape: a uniformly scattered value has members in
        // every container, so no candidate is disjoint from it, and the row is reported n/a.
        let coverages: [(&str, f64); 3] =
            [("25pct", 0.25), ("100pct", 1.0), ("hidden-25pct", -1.0)];
        for shape in [Shape::Correlated, Shape::Scattered] {
            for share in NEEDLE_SHARES {
                let members = needle_members(n, shape, share);
                let posting_bytes = members.get_serialized_size_in_bytes::<Portable>() as u64;
                let column = flat_column(n, &members);
                let column_bytes = column.len() as u64;

                for (cov_name, cov) in coverages {
                    if cov < 0.0 && shape == Shape::Scattered {
                        continue;
                    }
                    let mut candidate = Bitmap::new();
                    if cov < 0.0 {
                        // Disjoint from the correlated block at [n/8, n/8 + share*n).
                        candidate.add_range((n as f64 * 0.40) as u32..(n as f64 * 0.65) as u32);
                    } else {
                        candidate.add_range(0u32..((n as f64 * cov) as u64).min(n) as u32);
                    }
                    candidate.run_optimize();

                    // --- scan: the baseline the accelerator must beat.
                    let mut hits = Vec::new();
                    for e in candidate.iter() {
                        if column[e as usize] == 1 {
                            hits.push(e);
                        }
                    }
                    let expected = hits.len() as u64;
                    let t = Instant::now();
                    let mut hits = Vec::new();
                    for e in candidate.iter() {
                        if column[e as usize] == 1 {
                            hits.push(e);
                        }
                    }
                    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
                    assert_eq!(hits.len() as u64, expected);

                    // --- intersect: what the built keyed reader does.
                    let _ = candidate.and(&members);
                    let t = Instant::now();
                    let got = candidate.and(&members);
                    let and_ms = t.elapsed().as_secs_f64() * 1000.0;
                    assert_eq!(got.cardinality(), expected, "intersect disagreed with scan");

                    // --- probe: candidate-driven, D4's proposed form. Container-granular, so the
                    // probe count is a function of the candidate alone. Modelled here by slicing
                    // the candidate into 2^16 blocks and intersecting each against the posting —
                    // the same number of lookups whatever the value holds.
                    let blocks = (n >> 16) + 1;
                    let t = Instant::now();
                    let mut acc = Bitmap::new();
                    for blk in 0..blocks {
                        let lo = (blk << 16) as u32;
                        let hi = lo.saturating_add(u16::MAX as u32);
                        let mut window = Bitmap::new();
                        window.add_range(lo..=hi);
                        window.and_inplace(&candidate);
                        if !window.is_empty() {
                            window.and_inplace(&members);
                            acc.or_inplace(&window);
                        }
                    }
                    let probe_ms = t.elapsed().as_secs_f64() * 1000.0;
                    assert_eq!(acc.cardinality(), expected, "probe disagreed with scan");

                    for (form, ms) in [
                        ("scan", scan_ms),
                        ("intersect", and_ms),
                        ("probe", probe_ms),
                    ] {
                        println!(
                            "{n},{},{share},{cov_name},{form},{posting_bytes},{column_bytes},{ms:.3},{expected}",
                            shape.name()
                        );
                    }
                }
            }
        }
    }
}
