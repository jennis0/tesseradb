//! **Arm 3 — a multi-valued column: three addressings, three operators.**
//!
//! `filter-index.md` §2.1's value column maps **one presence bit to one slot**, and the affine-rank
//! traversal, the fold's blanking and the coalesce's merge are all built on that. A list column
//! breaks it: an entity carries *k* values, so "the *k*-th set bit's value is at slot *k*" no longer
//! addresses anything. This arm prices the three shapes that could replace it.
//!
//! - **CSR** — `offsets[e]..offsets[e+1]` into a flat values array, entity-addressed. The value
//!   column generalised: still one array of values in entity order, with a second array saying where
//!   each entity's run starts. Costs 4 B/entity for the offsets on top of the values.
//! - **Postings** — one Roaring bitmap per value, which is what memo §6.2's keyword case proposed
//!   and what a category already derives as an accelerator. Every operator is then bitmap algebra
//!   and costs O(containers touched) rather than O(candidate).
//! - **Pairs** — a sorted `(entity, value)` array. The obvious shape, and the one the single-valued
//!   campaign found *never optimal*; measured here because the reason it lost there — 4 B/entity of
//!   pure addressing overhead against a bare array — does not transfer to a column that has no bare
//!   array to lose to.
//!
//! Three operators, because they do not agree on which layout wins: `any_of` (a union),
//! `all_of` (an intersection, which is where a per-entity walk has to count rather than short-
//! circuit) and `none_of` — decision 0066's positive predicate: *carries a value in this column, and
//! none of these matches it*.
//!
//! **Values are uniform over the domain, and skew is not measured.** A skewed vocabulary makes one
//! posting enormous and the rest tiny, which moves the postings layout's numbers and nothing else's;
//! the design should not quote these cells for a heavy-tailed vocabulary without a second run.

use std::time::Instant;

use croaring::{Bitmap, Portable};
use placement::{mask, median, splitmix, MaskShape};

const ROUNDS: usize = 3;

/// A CSR list column: values in entity order, `offsets[e]..offsets[e+1]` delimiting entity `e`.
struct Csr {
    offsets: Vec<u32>,
    values: Vec<u32>,
}

impl Csr {
    /// **An entity's values are a set, and the vocabulary is skewed.** Both matter: a duplicate
    /// value would make `all_of` mean something different in each layout, and a *uniform* vocabulary
    /// is the one shape that flatters the postings layout — decision 0039's measured corpus has
    /// 404,104 surnames of which **138,861 are singletons**, so the tail is where a posting-per-value
    /// has to justify itself. The skew here is quadratic (`D·u²`), which is a stand-in for that tail
    /// rather than a fit to it.
    fn build(n: usize, mean: u64, domain: u64) -> Csr {
        let mut offsets = Vec::with_capacity(n + 1);
        let mut values = Vec::with_capacity(n * mean as usize);
        let mut scratch: Vec<u32> = Vec::with_capacity(16);
        offsets.push(0);
        for e in 0..n as u64 {
            let k = 1 + splitmix(e ^ 0xA5A5) % (2 * mean - 1);
            scratch.clear();
            for i in 0..k {
                let u = splitmix(e.wrapping_mul(31).wrapping_add(i)) % domain;
                scratch.push(((u * u) / domain) as u32);
            }
            scratch.sort_unstable();
            scratch.dedup();
            values.extend_from_slice(&scratch);
            offsets.push(values.len() as u32);
        }
        Csr { offsets, values }
    }

    fn bytes(&self) -> usize {
        4 * self.offsets.len() + 4 * self.values.len()
    }

    /// Walk the candidate's runs, visiting each entity's value run in turn.
    ///
    /// The traversal is the candidate's, exactly as `for_each_slot_run` makes it for the
    /// single-valued column: what varies per entity is how many values it holds, which is a length
    /// read from the offsets rather than a search.
    fn scan(&self, candidate: &Bitmap, keep: impl Fn(&[u32]) -> bool) -> Bitmap {
        let mut out = Bitmap::new();
        let mut buf: Vec<u32> = Vec::with_capacity(1024);
        for (lo, hi) in runs(candidate) {
            for e in lo..=hi {
                let (s, t) = (self.offsets[e as usize] as usize, self.offsets[e as usize + 1] as usize);
                if keep(&self.values[s..t]) {
                    buf.push(e);
                    if buf.len() == 1024 {
                        out.add_many(&buf);
                        buf.clear();
                    }
                }
            }
        }
        out.add_many(&buf);
        out
    }
}

fn runs(b: &Bitmap) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut cursor = b.cursor();
    let Some(mut start) = cursor.current() else {
        return out;
    };
    let mut prev = start;
    cursor.move_next();
    while let Some(v) = cursor.current() {
        cursor.move_next();
        if v != prev + 1 {
            out.push((start, prev));
            start = v;
        }
        prev = v;
    }
    out.push((start, prev));
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scales: Vec<usize> = if args.len() > 1 {
        args[1..].iter().map(|s| s.parse().expect("scale")).collect()
    } else {
        vec![10_000_000]
    };

    println!(
        "n,mean_values,domain,candidate,candidates,operator,layout,ms,ns_per_candidate,hits,bytes,\
         needle_members"
    );

    for &n in &scales {
        // The two real shapes decision 0039 measured on the arXiv corpus: `categories` (176
        // distinct, mean 1.72) and `surnames` (404,104 distinct, mean 4.54). A list column's cost
        // is governed by these two numbers, so the grid is them rather than round ones.
        for &(mean, domain) in &[(2u64, 200u64), (5, 400_000)] {
            {
                eprintln!("n={n} mean={mean} domain={domain}: building");
                let csr = Csr::build(n, mean, domain);

                // Postings: one bitmap per value, built from the same data — the accelerator a
                // category already derives, generalised to a list.
                let mut postings: Vec<Vec<u32>> = vec![Vec::new(); domain as usize];
                for e in 0..n as u32 {
                    let (s, t) = (
                        csr.offsets[e as usize] as usize,
                        csr.offsets[e as usize + 1] as usize,
                    );
                    for &v in &csr.values[s..t] {
                        let list = &mut postings[v as usize];
                        if list.last() != Some(&e) {
                            list.push(e);
                        }
                    }
                }
                let postings: Vec<Bitmap> = postings
                    .into_iter()
                    .map(|v| {
                        let mut b = Bitmap::of(&v);
                        b.run_optimize();
                        b
                    })
                    .collect();
                let postings_bytes: usize = postings
                    .iter()
                    .map(|b| b.get_serialized_size_in_bytes::<Portable>())
                    .sum();
                // Every entity carries at least one value here, so presence is universal and costs
                // nothing to store. A partial column would add a Roaring presence bitmap to all
                // three layouts alike.
                let mut present = Bitmap::new();
                present.add_range(0u32..n as u32);
                present.run_optimize();

                // Pairs: sorted (entity, value), the shape a merge join wants.
                let mut pairs: Vec<u64> = Vec::with_capacity(csr.values.len());
                for e in 0..n as u32 {
                    let (s, t) = (
                        csr.offsets[e as usize] as usize,
                        csr.offsets[e as usize + 1] as usize,
                    );
                    for &v in &csr.values[s..t] {
                        pairs.push((e as u64) << 32 | v as u64);
                    }
                }
                pairs.sort_unstable();
                let pairs_bytes = 8 * pairs.len();

                let needles: Vec<u32> = vec![1, 7, 13];
                let pair_ = |op: &str| -> (&[u32], usize) {
                    match op {
                        "all_of" => (&needles[..2], 2),
                        _ => (&needles[..3], 3),
                    }
                };

                for shape in [MaskShape::Contiguous, MaskShape::Blocked, MaskShape::Scattered] {
                    for &coverage in &[0.01f64, 0.25] {
                        let candidate = mask(n, shape, coverage);
                        let candidates = candidate.cardinality();
                        let label = format!("{}-{}pct", shape.name(), (coverage * 100.0) as u32);

                        for op in ["any_of", "all_of", "none_of"] {
                            let (needles, _k) = pair_(op);

                            // ---- CSR ---------------------------------------------------
                            let mut csr_ms = Vec::new();
                            let mut answer = Bitmap::new();
                            for _ in 0..ROUNDS {
                                let t = Instant::now();
                                answer = match op {
                                    "any_of" => csr.scan(&candidate, |vs| {
                                        vs.iter().any(|v| needles.contains(v))
                                    }),
                                    "all_of" => csr.scan(&candidate, |vs| {
                                        needles.iter().all(|nd| vs.contains(nd))
                                    }),
                                    _ => csr.scan(&candidate, |vs| {
                                        !vs.is_empty() && !vs.iter().any(|v| needles.contains(v))
                                    }),
                                };
                                csr_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                            }

                            // ---- postings ----------------------------------------------
                            let mut post_ms = Vec::new();
                            let mut answer_p = Bitmap::new();
                            for _ in 0..ROUNDS {
                                let t = Instant::now();
                                answer_p = match op {
                                    "any_of" => {
                                        let refs: Vec<&Bitmap> =
                                            needles.iter().map(|&v| &postings[v as usize]).collect();
                                        let mut u = Bitmap::fast_or(&refs);
                                        u.and_inplace(&candidate);
                                        u
                                    }
                                    "all_of" => {
                                        let mut acc = postings[needles[0] as usize].clone();
                                        for &v in &needles[1..] {
                                            acc.and_inplace(&postings[v as usize]);
                                        }
                                        acc.and_inplace(&candidate);
                                        acc
                                    }
                                    _ => {
                                        let refs: Vec<&Bitmap> =
                                            needles.iter().map(|&v| &postings[v as usize]).collect();
                                        let u = Bitmap::fast_or(&refs);
                                        let mut base = present.clone();
                                        base.and_inplace(&candidate);
                                        base.andnot_inplace(&u);
                                        base
                                    }
                                };
                                post_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                            }

                            // ---- pairs -------------------------------------------------
                            let mut pair_ms = Vec::new();
                            let mut answer_x = Bitmap::new();
                            for _ in 0..ROUNDS {
                                let t = Instant::now();
                                let mut out = Bitmap::new();
                                let mut buf: Vec<u32> = Vec::with_capacity(1024);
                                for (lo, hi) in runs(&candidate) {
                                    let start = pairs.partition_point(|&p| (p >> 32) < lo as u64);
                                    let mut i = start;
                                    let mut entity = lo;
                                    while i < pairs.len() {
                                        let e = (pairs[i] >> 32) as u32;
                                        if e > hi {
                                            break;
                                        }
                                        entity = e;
                                        // Which *needles* were seen, as a bitmask — not how many
                                        // matching pairs there were. A count is wrong the moment an
                                        // entity carries a value twice, which is the defect this
                                        // arm's first run caught by disagreeing with CSR.
                                        let mut matched = 0u32;
                                        let mut count = 0usize;
                                        while i < pairs.len() && (pairs[i] >> 32) as u32 == entity {
                                            let v = (pairs[i] & 0xFFFF_FFFF) as u32;
                                            if let Some(k) = needles.iter().position(|&nd| nd == v) {
                                                matched |= 1 << k;
                                            }
                                            count += 1;
                                            i += 1;
                                        }
                                        let all = matched.count_ones() as usize;
                                        let keep = match op {
                                            "any_of" => matched != 0,
                                            "all_of" => all >= needles.len(),
                                            _ => count > 0 && matched == 0,
                                        };
                                        if keep {
                                            buf.push(entity);
                                            if buf.len() == 1024 {
                                                out.add_many(&buf);
                                                buf.clear();
                                            }
                                        }
                                    }
                                    let _ = entity;
                                }
                                out.add_many(&buf);
                                answer_x = out;
                                pair_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                            }

                            assert_eq!(answer, answer_p, "csr/postings disagree on {op}");
                            assert_eq!(answer, answer_x, "csr/pairs disagree on {op}");

                            let hits = answer.cardinality();
                            // Corpus-wide members of the needles, so a cell's cost can be read
                            // against how much it was looking for rather than against the operator
                            // name alone.
                            let needle_members: u64 = needles
                                .iter()
                                .map(|&v| postings[v as usize].cardinality())
                                .sum();
                            let emit = |layout: &str, ms: f64, bytes: usize| {
                                println!(
                                    "{n},{mean},{domain},{label},{candidates},{op},{layout},\
                                     {ms:.3},{:.2},{hits},{bytes},{needle_members}",
                                    ms * 1e6 / candidates as f64
                                );
                            };
                            emit("csr", median(csr_ms), csr.bytes());
                            emit("postings", median(post_ms), postings_bytes);
                            emit("pairs", median(pair_ms), pairs_bytes);
                        }
                    }
                }
            }
        }
    }
}
