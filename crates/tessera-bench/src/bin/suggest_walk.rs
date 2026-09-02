//! **What does a keystroke cost?** — the shipped suggestion walk, priced at scale
//! (`docs/design/value-suggestion.md` §6).
//!
//! The design's §6.2 figures were measured on the *probe's* routes: a `Bitmap::intersect` against a
//! mapped view, and a `SortedDict` prefix range, timed separately. What ships is
//! `tessera_engine::suggest::walk` over `SuggestIndex` and `ColumnPostings::intersects`, and this
//! measures that — the same function `Engine::suggest` calls, with the gate supplied as a closure
//! rather than composed from a session. So a regression in the walk shows here; a regression in the
//! gate does not.
//!
//! **What it measures**
//!
//! - **the build** — deriving and sorting the entry list, and writing the five files;
//! - **the extents sweep** — the loop `FilterColumns::category_membership` runs, over a synthetic
//!   post-build extent, per candidate;
//! - **the walk** — fold, two binary searches, and one boolean posting probe per value examined, at
//!   budgets 10³/10⁴/10⁵, for viewers at 0.01%, 1% and 10% of the corpus, in a contiguous and a
//!   scattered shape;
//! - **counts** — `?counts=true` for a page of twenty, which is one `and_cardinality` per served
//!   value against the mapped view.
//!
//! **What it does not measure, and why.** ⊘ **Candidate composition.** `filter::candidate` takes a
//! session's frozen fragment, its satisfied term set, the overlay and the ingest buffer, none of
//! which exists without a built bundle and an authorised principal — so pricing it here would mean
//! building a 10⁸-entity bundle, which is a campaign rather than a bench. It is an addition to
//! every figure below, it is per-request work this design does not add, and
//! `probes/2026-08-07-category-membership/` is where it is measured. ⊘ **Device-cold and concurrent
//! figures**: one thread, page cache warm, as the design's own table is.
//!
//! The vocabulary is synthetic and its *shape* is the thing that decides the walk's cost, so it is
//! stated rather than left to be inferred: keys are `<tag>.<n>` over 676 two-letter tags, titles are
//! three words drawn from a small list, and membership is Zipf — value *i* has `members / (i + 1)`
//! members scattered over the corpus. A one-character prefix therefore covers a predictable slice of
//! the index, and the values under it span the whole membership range.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin suggest_walk -- \
//!     [--values 1000000] [--entities 100000000] [--repeats 200]
//! ```

use std::io;
use std::time::Instant;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_engine::suggest::{walk, SuggestIndex, SuggestValue, VocabularySuggest, WalkBudget};
use tessera_filter::{Access, ColumnPostings, ValueColumn};
use tessera_types::AttrLocalId;

/// The two-letter tags keys are drawn from, so a one-character prefix covers ~1/26 of the index.
fn tag(i: usize) -> String {
    let a = (b'a' + (i / 26 % 26) as u8) as char;
    let b = (b'a' + (i % 26) as u8) as char;
    format!("{a}{b}")
}

const WORDS: &[&str] = &[
    "machine", "learning", "systems", "coastal", "atlantic", "northern", "harbour", "station",
    "research", "quantum", "mineral", "district", "provincial", "junction", "reservoir", "summit",
];

fn word(i: usize) -> &'static str {
    WORDS[i % WORDS.len()]
}

/// The vocabulary, in key order — which is the order a dense position indexes.
fn vocabulary(values: usize) -> Vec<SuggestValue> {
    let mut out: Vec<SuggestValue> = (0..values)
        .map(|i| SuggestValue {
            key: format!("{}.{i:07}", tag(i)),
            title: Some(format!(
                "{} {} {}",
                word(i),
                word(i / 7 + 3),
                word(i / 53 + 11)
            )),
            code: 0,
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    // Codes scattered over the `u32` width, as a real vocabulary's are (§3.4) — a keyed postings
    // base binary-searches them, so a dense assignment would measure the wrong lookup.
    for (position, value) in out.iter_mut().enumerate() {
        value.code = 1 + (position as u64 * 2_654_435_761 % (u32::MAX as u64 - 1)) as u32;
    }
    out
}

/// One posting per value, Zipf over the corpus: value *i* has `entities / 64 / (i + 1)` members,
/// at least one, scattered by a stride so no two values share a container pattern.
fn write_postings(
    path: &std::path::Path,
    values: &[SuggestValue],
    entities: u32,
) -> io::Result<Vec<u32>> {
    let mut entries: Vec<(u32, Vec<u32>)> = Vec::with_capacity(values.len());
    let mut rng = StdRng::seed_from_u64(0x5133);
    for (i, value) in values.iter().enumerate() {
        let members = ((entities as u64 / 64) / (i as u64 + 1)).max(1) as usize;
        let stride = (entities as u64 / members as u64).max(1) as u32;
        let offset = rng.gen_range(0..stride.max(1));
        let mut posting: Vec<u32> = (0..members)
            .map(|m| offset.wrapping_add(m as u32 % u32::MAX).wrapping_add(m as u32 * stride))
            .filter(|e| *e < entities)
            .collect();
        posting.sort_unstable();
        posting.dedup();
        if posting.is_empty() {
            posting.push(i as u32 % entities);
        }
        entries.push((value.code, posting));
    }
    entries.sort_by_key(|(code, _)| *code);
    entries.dedup_by_key(|(code, _)| *code);
    tessera_authz::write_delta_tier_at(path, &entries, 32)?;
    // The base tier's code array, in the order the reader binary-searches it — kept so the
    // decomposition below can price that search on its own.
    Ok(entries.into_iter().map(|(code, _)| code).collect())
}

/// A viewer's composed candidate, in the two shapes §6.2 measures: one contiguous run, and every
/// *k*-th entity.
fn candidate(entities: u32, fraction: f64, contiguous: bool) -> Bitmap {
    let n = ((entities as f64) * fraction) as u32;
    if contiguous {
        Bitmap::from_range(0..n)
    } else {
        let stride = (entities / n.max(1)).max(1);
        let mut out = Bitmap::new();
        let members: Vec<u32> = (0..n).map(|i| i.saturating_mul(stride) % entities).collect();
        out.add_many(&members);
        out
    }
}

fn percentile(samples: &mut [f64], p: f64) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[at]
}

fn main() {
    let mut values_n = 1_000_000usize;
    let mut entities = 100_000_000u32;
    let mut repeats = 200usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--values" => values_n = args.next().unwrap().parse().unwrap(),
            "--entities" => entities = args.next().unwrap().parse().unwrap(),
            "--repeats" => repeats = args.next().unwrap().parse().unwrap(),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let pool = rayon::ThreadPoolBuilder::new()
        .build()
        .expect("the pool builds");

    println!("# suggest_walk — {values_n} values over {entities} entities, {repeats} repeats");
    println!("# one thread, page cache warm; candidate composition is NOT measured (see header)");

    let started = Instant::now();
    let values = vocabulary(values_n);
    println!("vocabulary   {:>9.2} s", started.elapsed().as_secs_f64());

    let started = Instant::now();
    let index = SuggestIndex::build(dir.path(), 0, &values, &pool).expect("the index builds");
    println!(
        "index build  {:>9.2} s   {} entry strings, {} payloads",
        started.elapsed().as_secs_f64(),
        index.entry_count(),
        index.payload_count()
    );
    let bytes: u64 = std::fs::read_dir(index.dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    println!("index bytes  {:>9.1} MB", bytes as f64 / 1e6);

    let started = Instant::now();
    let postings_path = dir.path().join("postings.arrow");
    let record_codes = write_postings(&postings_path, &values, entities).expect("the postings write");
    let postings = ColumnPostings::open_keyed(&postings_path).expect("the postings open");
    println!(
        "postings     {:>9.2} s   {:.1} MB",
        started.elapsed().as_secs_f64(),
        std::fs::metadata(&postings_path).unwrap().len() as f64 / 1e6
    );

    // A post-build extent: the entities a flush has published since the build, with a code each.
    // The sweep below is `category_membership`'s own loop over it.
    let extent_len = 1_000_000u32.min(entities);
    let extent_values = dir.path().join("extent.arrow");
    let extent_presence = dir.path().join("extent-presence.arrow");
    let codes: Vec<u32> = (0..extent_len)
        .map(|e| values[e as usize % values.len()].code)
        .collect();
    tessera_filter::write_value_column(
        &extent_values,
        &extent_presence,
        &tessera_filter::Codes::U32(codes.into()),
        None,
    )
    .expect("the extent writes");
    let extent = ValueColumn::open(&extent_values, None, Access::Mapped).expect("it opens");

    let live = VocabularySuggest::new(std::sync::Arc::new(index));
    let fold = tessera_analyse::SuggestionFold::new();

    println!();
    println!("# the extents sweep — `category_membership`'s own loop, over a {extent_len}-entity extent");
    println!("{:<12} {:<12} {:>10} {:>10}", "viewer", "shape", "median ms", "p99 ms");
    for (label, fraction) in [("0.01%", 0.0001), ("1%", 0.01), ("10%", 0.1)] {
        for (shape, contiguous) in [("contiguous", true), ("scattered", false)] {
            let cand = candidate(entities, fraction, contiguous);
            let mut samples = Vec::new();
            for _ in 0..repeats.min(20) {
                let started = Instant::now();
                let mut sweep: rustc_hash::FxHashMap<u32, u64> = Default::default();
                for entity in extent.present().and(&cand).iter() {
                    if let Some(code) = extent.value_of(entity) {
                        *sweep.entry(code.raw()).or_default() += 1;
                    }
                }
                samples.push(started.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(sweep.len());
            }
            println!(
                "{label:<12} {shape:<12} {:>10.3} {:>10.3}",
                percentile(&mut samples, 0.5),
                percentile(&mut samples, 0.99)
            );
        }
    }

    println!();
    println!("# the walk — one-character prefixes, page of 20, `counts=false`");
    println!(
        "{:<12} {:<12} {:>8} {:>10} {:>10} {:>8} {:>8}",
        "viewer", "shape", "budget", "median ms", "p99 ms", "filled", "more"
    );
    let prefixes: Vec<String> = (b'a'..=b'z').map(|c| (c as char).to_string()).collect();
    for (label, fraction) in [("0.01%", 0.0001), ("1%", 0.01), ("10%", 0.1)] {
        for (shape, contiguous) in [("contiguous", true), ("scattered", false)] {
            let cand = candidate(entities, fraction, contiguous);
            for budget in [1_000u64, 10_000, 100_000] {
                let mut samples = Vec::with_capacity(repeats);
                let mut filled = 0usize;
                let mut more_count = 0usize;
                for r in 0..repeats {
                    let q = &prefixes[r % prefixes.len()];
                    let started = Instant::now();
                    let (found, more) = walk(
                        &live,
                        &fold,
                        q,
                        WalkBudget {
                            limit: 20,
                            walk_budget: budget,
                            counts: false,
                        },
                        &|code| postings.intersects(AttrLocalId::new(code), &cand),
                        &|_| Ok(0u64),
                        &|e: io::Error| e,
                    )
                    .expect("the walk");
                    samples.push(started.elapsed().as_secs_f64() * 1e3);
                    if found.len() == 20 {
                        filled += 1;
                    }
                    if more {
                        more_count += 1;
                    }
                }
                println!(
                    "{label:<12} {shape:<12} {budget:>8} {:>10.3} {:>10.3} {:>8} {:>8}",
                    percentile(&mut samples, 0.5),
                    percentile(&mut samples, 0.99),
                    format!("{filled}/{repeats}"),
                    format!("{more_count}/{repeats}")
                );
            }
        }
    }

    println!();
    println!("# probe decomposition — where the walk's constant goes, at budget 10⁵");
    println!(
        "# `search` is the keyed base's binary search over its {} code records, alone;",
        record_codes.len()
    );
    println!("# `probe` is the whole `ColumnPostings::intersects` including that search.");
    println!(
        "{:<12} {:<12} {:>12} {:>12} {:>8}",
        "viewer", "shape", "search ns", "probe ns", "search %"
    );
    // The codes a one-character prefix's walk actually probes, in the order it probes them — an
    // arbitrary order over the code array, which is the access pattern that decides the search's
    // cost. Taken from the index rather than invented, so the two columns are the same work.
    let mut probed: Vec<u32> = Vec::new();
    {
        let base = live.base();
        let entries = base.prefix_range("a").expect("a prefix range");
        for at in base.payload_range(entries).take(100_000) {
            let payload = base.payload_at(at).expect("a payload");
            probed.push(base.code_at(payload.position).expect("a code"));
        }
    }
    for (label, fraction) in [("0.01%", 0.0001), ("1%", 0.01), ("10%", 0.1)] {
        for (shape, contiguous) in [("contiguous", true), ("scattered", false)] {
            let cand = candidate(entities, fraction, contiguous);
            let started = Instant::now();
            let mut hits = 0usize;
            for code in &probed {
                if record_codes.binary_search(code).is_ok() {
                    hits += 1;
                }
            }
            let search = started.elapsed().as_secs_f64() * 1e9 / probed.len() as f64;
            std::hint::black_box(hits);

            let started = Instant::now();
            for code in &probed {
                std::hint::black_box(
                    postings
                        .intersects(AttrLocalId::new(*code), &cand)
                        .expect("a probe"),
                );
            }
            let probe = started.elapsed().as_secs_f64() * 1e9 / probed.len() as f64;
            println!(
                "{label:<12} {shape:<12} {search:>12.1} {probe:>12.1} {:>7.0}%",
                search / probe * 100.0
            );
        }
    }

    println!();
    println!("# `counts=true` — one `and_cardinality` per served value, page of 20, budget 10⁵");
    println!("{:<12} {:<12} {:>10} {:>10}", "viewer", "shape", "median ms", "p99 ms");
    for (label, fraction) in [("0.01%", 0.0001), ("1%", 0.01), ("10%", 0.1)] {
        for (shape, contiguous) in [("contiguous", true), ("scattered", false)] {
            let cand = candidate(entities, fraction, contiguous);
            let mut samples = Vec::with_capacity(repeats);
            for r in 0..repeats {
                let q = &prefixes[r % prefixes.len()];
                let started = Instant::now();
                let (found, _) = walk(
                    &live,
                    &fold,
                    q,
                    WalkBudget {
                        limit: 20,
                        walk_budget: 100_000,
                        counts: true,
                    },
                    &|code| postings.intersects(AttrLocalId::new(code), &cand),
                    &|code| postings.intersection_cardinality(AttrLocalId::new(code), &cand),
                    &|e: io::Error| e,
                )
                .expect("the walk");
                samples.push(started.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(found.len());
            }
            println!(
                "{label:<12} {shape:<12} {:>10.3} {:>10.3}",
                percentile(&mut samples, 0.5),
                percentile(&mut samples, 0.99)
            );
        }
    }
}
