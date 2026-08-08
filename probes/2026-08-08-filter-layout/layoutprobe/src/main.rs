//! What does it cost to know which entities a filter column covers?
//!
//! The value array is identical across every layout under consideration, so the whole question is
//! the *addressing* structure beside it. Four candidates, measured for bytes and for masked-scan
//! latency across three presence shapes and three candidate-mask shapes:
//!
//! - **Bare** — no addressing structure at all. Only legal when every entity carries a value;
//!   the entity id *is* the array index.
//! - **Pairs** — an explicit `entity_id: u32` column beside the values, ascending.
//! - **Roaring** — a `croaring` presence bitmap; values stored compactly, rank gives the slot.
//! - **Runs** — a `(start, len, base_rank)` table, which is what a presence bitmap degenerates to
//!   when entity ranges arrive in contiguous per-slice blocks.
//!
//! Everything is in RAM: the probe reports serialised byte counts rather than writing files, both
//! because the disk this ran on had no room and because the question is bytes-and-throughput
//! rather than any property of a particular file format.
//!
//! Single-threaded throughout. A real scan would be parallel, but the comparison between layouts
//! is what is being measured and threading all four equally would only add variance.

use std::time::Instant;

use croaring::{Bitmap, Portable};

/// Distinct values in the synthetic column. 1,000 is category-shaped — the family that stresses
/// this choice hardest, because its values are narrow and its postings alternative is cheapest.
const DOMAIN: u32 = 1_000;
/// The predicate: `value == NEEDLE`, so ~0.1% of present entities match. Selectivity is
/// deliberately *not* a variable here — it moves the size of the result, not the cost of finding
/// it, which is the thing under test.
const NEEDLE: u32 = 42;

/// A cheap deterministic hash. Not for quality — for reproducibility without a dependency.
#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// Every entity carries a value. The common case for a column declared over the whole corpus.
    All,
    /// Ten slices, interleaved in blocks of `SLICE_RUN` entities. Models concurrent multi-slice
    /// ingest: a commit window is per slice, but windows interleave, so an entity range is
    /// ascending-with-holes rather than contiguous (write-path §4.2).
    Slices,
    /// One entity in ten, scattered. The worst case for any run-structured encoding.
    Scattered,
}

/// Entities per slice-block. A commit window's worth of ingest at the owner's stated rates.
const SLICE_RUN: u64 = 100_000;
const SLICE_COUNT: u64 = 10;

impl Presence {
    fn name(self) -> &'static str {
        match self {
            Presence::All => "all",
            Presence::Slices => "slices-10",
            Presence::Scattered => "scattered-10pct",
        }
    }

    #[inline]
    fn holds(self, e: u64) -> bool {
        match self {
            Presence::All => true,
            // Slice 0's blocks: every SLICE_COUNT-th run of SLICE_RUN entities.
            Presence::Slices => (e / SLICE_RUN) % SLICE_COUNT == 0,
            Presence::Scattered => splitmix(e ^ 0xA5A5) % 10 == 0,
        }
    }
}

#[derive(Clone, Copy)]
enum Candidate {
    /// A sparse principal whose visible set is contiguous — what signature-sorted entity
    /// assignment is *supposed* to produce, and the regime the repo's cost model assumes.
    SparseContiguous,
    /// A privileged principal: a quarter of the corpus.
    Broad,
    /// A sparse principal whose grants do not correlate with the sort order. The measured
    /// `surnames` policy shape, where signature-sorting bought ~1.0×.
    SparseScattered,
}

impl Candidate {
    fn name(self) -> &'static str {
        match self {
            Candidate::SparseContiguous => "sparse-contiguous-1pct",
            Candidate::Broad => "broad-25pct",
            Candidate::SparseScattered => "sparse-scattered-1pct",
        }
    }

    fn build(self, n: u64) -> Bitmap {
        let mut b = Bitmap::new();
        match self {
            // One contiguous run holding 1% of the corpus, placed a third of the way in.
            Candidate::SparseContiguous => {
                let lo = n / 3;
                b.add_range(lo as u32..(lo + n / 100) as u32);
            }
            // 25% as a contiguous quarter: a head principal under signature sorting.
            Candidate::Broad => {
                b.add_range(0u32..(n / 4) as u32);
            }
            Candidate::SparseScattered => {
                let mut v: Vec<u32> = Vec::with_capacity((n / 100) as usize);
                let mut e = 0u64;
                while e < n {
                    if splitmix(e ^ 0x5EED) % 100 == 0 {
                        v.push(e as u32);
                    }
                    e += 1;
                }
                b.add_many(&v);
            }
        }
        b.run_optimize();
        b
    }
}

/// The four layouts, each holding the same logical `(entity → value)` relation.
struct Built {
    /// Values for present entities only, ascending by entity.
    values: Vec<u32>,
    /// Layout-specific addressing structure.
    addressing: Addressing,
}

enum Addressing {
    Bare,
    Pairs(Vec<u32>),
    Roaring(Bitmap),
    Runs(Vec<(u32, u32, u32)>),
}

impl Addressing {
    fn name(&self) -> &'static str {
        match self {
            Addressing::Bare => "bare",
            Addressing::Pairs(_) => "pairs",
            Addressing::Roaring(_) => "roaring",
            Addressing::Runs(_) => "runs",
        }
    }

    /// Bytes the addressing structure costs. Roaring is its *serialised* size — what a bundle
    /// would store — rather than its in-memory footprint, which carries allocator slack.
    fn bytes(&self) -> u64 {
        match self {
            Addressing::Bare => 0,
            Addressing::Pairs(v) => (v.len() * 4) as u64,
            Addressing::Roaring(b) => b.get_serialized_size_in_bytes::<Portable>() as u64,
            Addressing::Runs(r) => (r.len() * 12) as u64,
        }
    }
}

fn build(n: u64, presence: Presence, layout: &str) -> Built {
    let mut values: Vec<u32> = Vec::new();
    let mut ids: Vec<u32> = Vec::new();
    let mut present = Bitmap::new();
    let mut runs: Vec<(u32, u32, u32)> = Vec::new();

    let mut run_start: Option<u64> = None;
    let mut rank: u64 = 0;
    let mut id_batch: Vec<u32> = Vec::with_capacity(1 << 16);

    for e in 0..n {
        let held = presence.holds(e);
        if held {
            values.push((splitmix(e) % DOMAIN as u64) as u32);
            match layout {
                "pairs" => ids.push(e as u32),
                "roaring" => {
                    id_batch.push(e as u32);
                    if id_batch.len() == id_batch.capacity() {
                        present.add_many(&id_batch);
                        id_batch.clear();
                    }
                }
                _ => {}
            }
            if run_start.is_none() {
                run_start = Some(e);
            }
        } else if let Some(s) = run_start.take() {
            runs.push((s as u32, (e - s) as u32, rank as u32));
            rank += e - s;
        }
    }
    if let Some(s) = run_start.take() {
        runs.push((s as u32, (n - s) as u32, rank as u32));
    }
    if !id_batch.is_empty() {
        present.add_many(&id_batch);
    }
    if layout == "roaring" {
        present.run_optimize();
    }

    let addressing = match layout {
        "bare" => Addressing::Bare,
        "pairs" => Addressing::Pairs(ids),
        "roaring" => Addressing::Roaring(present),
        "runs" => Addressing::Runs(runs),
        other => panic!("unknown layout {other}"),
    };
    Built { values, addressing }
}

/// Scan under the candidate mask, returning the matching entities.
///
/// Each layout gets the natural implementation for its shape — that is the point of the
/// comparison. What must stay equal across all four is the *answer*, which `main` asserts.
fn scan(built: &Built, candidate: &Bitmap) -> Bitmap {
    let mut hits: Vec<u32> = Vec::new();
    match &built.addressing {
        // Entity id is the index: iterate the candidate, index directly.
        Addressing::Bare => {
            for e in candidate.iter() {
                if built.values[e as usize] == NEEDLE {
                    hits.push(e);
                }
            }
        }
        // Merge-walk the id column against the candidate. Both ascending, so this is one
        // sequential pass over the ids — which is exactly the cost being interrogated: the id
        // column must be *read* to know what is in it.
        Addressing::Pairs(ids) => {
            let mut it = candidate.iter();
            let mut want = it.next();
            for (slot, &e) in ids.iter().enumerate() {
                while let Some(w) = want {
                    if w < e {
                        want = it.next();
                    } else {
                        break;
                    }
                }
                match want {
                    Some(w) if w == e => {
                        if built.values[slot] == NEEDLE {
                            hits.push(e);
                        }
                        want = it.next();
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        // Container arithmetic first — whole 65,536-entity blocks are skipped without being
        // touched — then a lockstep walk to turn entity ids into slots.
        Addressing::Roaring(present) => {
            let live = candidate.and(present);
            let mut slot: u64 = 0;
            let mut pit = present.iter();
            let mut cur: Option<u32> = pit.next();
            for e in live.iter() {
                while let Some(p) = cur {
                    if p < e {
                        slot += 1;
                        cur = pit.next();
                    } else {
                        break;
                    }
                }
                if built.values[slot as usize] == NEEDLE {
                    hits.push(e);
                }
            }
        }
        // Rank is offset arithmetic inside a run, so the candidate drives everything and the
        // addressing structure is consulted O(runs) times, not O(entities).
        Addressing::Runs(runs) => {
            for e in candidate.iter() {
                let idx = match runs.binary_search_by(|r| {
                    if r.0 > e {
                        std::cmp::Ordering::Greater
                    } else if (r.0 as u64 + r.1 as u64) <= e as u64 {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                }) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                let (start, _, base) = runs[idx];
                let slot = base as u64 + (e - start) as u64;
                if built.values[slot as usize] == NEEDLE {
                    hits.push(e);
                }
            }
        }
    }
    let mut b = Bitmap::new();
    b.add_many(&hits);
    b
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scales: Vec<u64> = if args.len() > 1 {
        args[1..].iter().map(|s| s.parse().expect("scale")).collect()
    } else {
        vec![1_000_000, 100_000_000]
    };

    println!("n,presence,layout,present_entities,addressing_bytes,values_bytes,addressing_bytes_per_present,candidate,scan_ms,hits");

    for &n in &scales {
        for presence in [Presence::All, Presence::Slices, Presence::Scattered] {
            // `bare` is only meaningful when presence is universal: with holes the array index
            // stops being the entity id, which is the whole point at issue.
            let layouts: &[&str] = if presence == Presence::All {
                &["bare", "pairs", "roaring", "runs"]
            } else {
                &["pairs", "roaring", "runs"]
            };
            let candidates = [
                Candidate::SparseContiguous,
                Candidate::Broad,
                Candidate::SparseScattered,
            ];
            let masks: Vec<(Candidate, Bitmap)> =
                candidates.iter().map(|c| (*c, c.build(n))).collect();

            let mut expected: Option<Vec<u64>> = None;
            for &layout in layouts {
                let built = build(n, presence, layout);
                let present_entities = built.values.len() as u64;
                let abytes = built.addressing.bytes();
                let vbytes = (built.values.len() * 4) as u64;
                let per_present = if present_entities == 0 {
                    0.0
                } else {
                    abytes as f64 / present_entities as f64
                };

                let mut answers = Vec::new();
                for (cand, mask) in &masks {
                    // One untimed pass so every layout is measured warm.
                    let _ = scan(&built, mask);
                    let t = Instant::now();
                    let hits = scan(&built, mask);
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    answers.push(hits.cardinality());
                    println!(
                        "{n},{},{},{present_entities},{abytes},{vbytes},{per_present:.4},{},{ms:.2},{}",
                        presence.name(),
                        built.addressing.name(),
                        cand.name(),
                        hits.cardinality()
                    );
                }
                // Every layout holds the same relation, so every layout must answer identically.
                // Without this the fast ones are only fast.
                match &expected {
                    None => expected = Some(answers),
                    Some(first) => assert_eq!(
                        first,
                        &answers,
                        "layout {} disagreed at n={n} presence={}",
                        built.addressing.name(),
                        presence.name()
                    ),
                }
            }
        }
    }
}
