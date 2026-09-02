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
//!   value against the mapped view;
//! - **the set route** (`--sets`, which runs alone) — §6.3's second route: the sweep that builds a
//!   session's visible-value set at 10⁴, 10⁶ and 10⁷ candidate entities, the per-keystroke cost of
//!   walking it, and the keystroke at which a session that paid for the sweep is ahead of one that
//!   kept probing;
//! - **the decomposition** (`--decomp`, which runs alone) — one probe split into the record search,
//!   the view over the record's bytes and the existential test, over the codes a budgeted walk
//!   actually probes; the walk's cost with the gate answered `false`; minor page faults; and the
//!   container keys a per-record sidecar would have to store. `probes/2026-09-02-value-suggestion/`
//!   arm 4 is what it was written for.
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
//!     [--values 1000000] [--entities 100000000] [--repeats 200] [--decomp]
//! ```

use std::io;
use std::time::Instant;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_authz::PostingRef;
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

/// Field 10 of `/proc/self/stat` — minor faults since the process started.
fn minor_faults() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("procfs");
    // The comm field may hold spaces and parentheses; fields are counted after the last `)`.
    let tail = &stat[stat.rfind(')').expect("a comm field") + 1..];
    tail.split_whitespace()
        .nth(7)
        .expect("field 10")
        .parse()
        .expect("a number")
}

/// The cost of the two `Instant::now()` calls that bracket one stage, so the stage figures can be
/// read net of the instrument.
fn timer_overhead_ns() -> f64 {
    let n = 200_000;
    let started = Instant::now();
    for _ in 0..n {
        std::hint::black_box(Instant::now());
    }
    started.elapsed().as_secs_f64() * 1e9 / n as f64
}

/// The containers one posting touches — the sidecar option (c) would store one `u16` per one of
/// these, per record.
fn container_count(posting: &PostingRef<'_>) -> usize {
    let mut last: i64 = -1;
    let mut n = 0usize;
    let mut note = |e: u32| {
        let chunk = (e >> 16) as i64;
        if chunk != last {
            last = chunk;
            n += 1;
        }
    };
    match posting {
        PostingRef::Roaring(view) => view.iter().for_each(&mut note),
        PostingRef::Array(bytes) => bytes
            .chunks_exact(4)
            .for_each(|c| note(u32::from_le_bytes(c.try_into().unwrap()))),
    }
    n
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
    let mut decomp = false;
    let mut sets = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--values" => values_n = args.next().unwrap().parse().unwrap(),
            "--entities" => entities = args.next().unwrap().parse().unwrap(),
            "--repeats" => repeats = args.next().unwrap().parse().unwrap(),
            // Run the stage decomposition alone, and skip the arms above it: the whole-walk
            // figures are already in the design's table and re-measuring them costs minutes.
            "--decomp" => decomp = true,
            // The set-route arm, which needs a value column over the whole entity space and so
            // runs alone for the decomposition's reason.
            "--sets" => sets = true,
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

    if decomp {
        decomposition(&live, &fold, &postings, &record_codes, entities, values_n);
        return;
    }
    if sets {
        set_route(&live, &fold, &postings, dir.path(), &values, entities);
        return;
    }

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
                        None,
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
                    None,
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

/// **Where the walk's constant goes** — `ColumnPostings::intersects` a stage at a time, over the
/// codes a real budgeted walk probes for the sparsest viewer.
///
/// The shipped probe is three things in one call: a binary search over the keyed base's code
/// array, a borrowed view over the found record's bytes (a portable Roaring deserialise, or a
/// slice for a tag-0 array), and the existential test against the candidate. The three
/// `bench_*` accessors call exactly that code, so the sum of the stages is the whole probe up to
/// the instrument, and the whole probe is timed beside them to show it.
///
/// It answers two questions the owner asked of the shipped 61–68 ms: what would a code → record
/// map at open remove (option **b** — the search column), and what would a per-record container-key
/// sidecar remove (option **c** — the view column, plus the part of the test that is a walk over
/// containers the candidate cannot meet).
fn decomposition(
    live: &VocabularySuggest,
    fold: &tessera_analyse::SuggestionFold,
    postings: &ColumnPostings,
    record_codes: &[u32],
    entities: u32,
    values_n: usize,
) {
    let overhead = timer_overhead_ns();
    println!();
    println!("# probe decomposition — one budgeted walk's probes, staged");
    println!(
        "# timer: {overhead:.1} ns per `Instant::now()`, so a bracketed stage carries ~{:.1} ns \
         of instrument",
        overhead
    );

    println!();
    println!(
        "{:<12} {:<8} {:>7} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "shape",
        "class",
        "n",
        "search50",
        "search99",
        "view50",
        "view99",
        "test50",
        "test99",
        "whole50",
        "whole99"
    );

    for (shape, contiguous) in [("contiguous", true), ("scattered", false)] {
        let cand = candidate(entities, 0.0001, contiguous);
        let stats = cand.statistics();

        // The codes a real walk probes, in the order it probes them — captured through the gate
        // closure `Engine::suggest` would pass, so this is the walk's own access pattern and not a
        // scan of the code array. Over the same 26 one-character prefixes the whole-walk arm
        // cycles, and not one of them: the fixture's key order puts the fattest postings under
        // `a`, so that prefix alone fills the page in twenty probes and measures the one keystroke
        // that is cheap.
        let probed = std::cell::RefCell::new(Vec::new());
        let mut pages = 0usize;
        let mut walks = 0usize;
        // Faults over *this* loop are the only first-touch figure in the run: it is the first
        // thing that reads the index and the postings after they are written, so every later pass
        // finds the mapping resident. Cold-*device* cost is still not measured — the files were
        // written seconds earlier and are in page cache.
        let faults_before = minor_faults();
        for c in b'a'..=b'z' {
            let (found, _more) = walk(
                live,
                fold,
                &(c as char).to_string(),
                WalkBudget {
                    limit: 20,
                    walk_budget: 100_000,
                    counts: false,
                },
                &|code| {
                    probed.borrow_mut().push(code);
                    postings.intersects(AttrLocalId::new(code), &cand)
                },
                &|_| Ok(0u64),
                &|e: io::Error| e,
                None,
            )
            .expect("the walk");
            walks += 1;
            if found.len() == 20 {
                pages += 1;
            }
        }
        let probed = probed.into_inner();
        let faults_capture = minor_faults() - faults_before;

        // What the walk costs *around* the probe: the same 26 walks with a gate that answers
        // `false` without reading a posting — the fold, the two binary searches, the payload and
        // code reads per entry, the emitted set — against the same walks with the shipped gate.
        // Neither (b) nor (c) touches this half, so it is the floor either option leaves behind.
        let mut null_ms: Vec<f64> = Vec::new();
        let mut real_ms: Vec<f64> = Vec::new();
        let mut null_probes = 0u64;
        for c in b'a'..=b'z' {
            let q = (c as char).to_string();
            let seen = std::cell::Cell::new(0u64);
            let started = Instant::now();
            walk(
                live,
                fold,
                &q,
                WalkBudget {
                    limit: 20,
                    walk_budget: 100_000,
                    counts: false,
                },
                &|_| {
                    seen.set(seen.get() + 1);
                    Ok(false)
                },
                &|_| Ok(0u64),
                &|e: io::Error| e,
                None,
            )
            .expect("the walk");
            null_ms.push(started.elapsed().as_secs_f64() * 1e3);
            null_probes += seen.get();

            let started = Instant::now();
            walk(
                live,
                fold,
                &q,
                WalkBudget {
                    limit: 20,
                    walk_budget: 100_000,
                    counts: false,
                },
                &|code| postings.intersects(AttrLocalId::new(code), &cand),
                &|_| Ok(0u64),
                &|e: io::Error| e,
                None,
            )
            .expect("the walk");
            real_ms.push(started.elapsed().as_secs_f64() * 1e3);
        }

        // Pass 1: the whole call, as the walk makes it.
        let faults_before = minor_faults();
        let mut whole: Vec<(bool, f64)> = Vec::with_capacity(probed.len());
        for code in &probed {
            let started = Instant::now();
            let hit = postings
                .intersects(AttrLocalId::new(*code), &cand)
                .expect("a probe");
            whole.push((hit, started.elapsed().as_secs_f64() * 1e9));
        }
        let faults_whole = minor_faults() - faults_before;

        // Pass 2: the same probes, staged.
        let faults_before = minor_faults();
        let mut staged: Vec<(bool, bool, f64, f64, f64)> = Vec::with_capacity(probed.len());
        for code in &probed {
            let t0 = Instant::now();
            let idx = postings.bench_base_record_index(AttrLocalId::new(*code));
            let t1 = Instant::now();
            let Some(idx) = idx else {
                continue;
            };
            let posting = postings
                .bench_base_posting_at_index(idx)
                .expect("a base record");
            let t2 = Instant::now();
            let hit = ColumnPostings::bench_hits(&posting, &cand);
            let t3 = Instant::now();
            staged.push((
                hit,
                matches!(posting, PostingRef::Roaring(_)),
                (t1 - t0).as_secs_f64() * 1e9,
                (t2 - t1).as_secs_f64() * 1e9,
                (t3 - t2).as_secs_f64() * 1e9,
            ));
        }
        let faults_staged = minor_faults() - faults_before;

        let roaring = staged.iter().filter(|r| r.1).count();
        println!(
            "# {shape}: {} candidate entities in {} containers; {} probes over {walks} prefixes \
             ({pages} pages filled), {} visible, {roaring} on a Roaring record",
            cand.cardinality(),
            stats.n_containers,
            probed.len(),
            whole.iter().filter(|(hit, _)| *hit).count()
        );
        println!(
            "# {shape}: minor faults — {faults_capture} over the first (cold-mapping) walk, \
             {faults_whole} over the whole-call pass, {faults_staged} over the staged pass = \
             {:.1}, {:.1} and {:.1} per 10^4 probes",
            faults_capture as f64 * 1e4 / probed.len().max(1) as f64,
            faults_whole as f64 * 1e4 / probed.len().max(1) as f64,
            faults_staged as f64 * 1e4 / probed.len().max(1) as f64
        );

        println!(
            "# {shape}: the walk itself, per prefix — null gate {:.2} ms median / {:.2} p99 over \
             {null_probes} entries = {:.1} ns each; shipped gate {:.2} / {:.2} ms",
            percentile(&mut null_ms.clone(), 0.5),
            percentile(&mut null_ms.clone(), 0.99),
            null_ms.iter().sum::<f64>() * 1e6 / null_probes.max(1) as f64,
            percentile(&mut real_ms.clone(), 0.5),
            percentile(&mut real_ms.clone(), 0.99),
        );

        for (class, want) in [("hidden", false), ("visible", true)] {
            let mut search: Vec<f64> = staged
                .iter()
                .filter(|r| r.0 == want)
                .map(|r| r.2)
                .collect();
            let mut view: Vec<f64> = staged.iter().filter(|r| r.0 == want).map(|r| r.3).collect();
            let mut test: Vec<f64> = staged.iter().filter(|r| r.0 == want).map(|r| r.4).collect();
            let mut all: Vec<f64> = whole
                .iter()
                .filter(|(hit, _)| *hit == want)
                .map(|(_, ns)| *ns)
                .collect();
            if search.is_empty() {
                println!("{shape:<12} {class:<8} {:>7}", 0);
                continue;
            }
            println!(
                "{shape:<12} {class:<8} {:>7} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} \
{:>9.1} {:>9.1}",
                search.len(),
                percentile(&mut search, 0.5),
                percentile(&mut search, 0.99),
                percentile(&mut view, 0.5),
                percentile(&mut view, 0.99),
                percentile(&mut test, 0.5),
                percentile(&mut test, 0.99),
                percentile(&mut all, 0.5),
                percentile(&mut all, 0.99),
            );
        }
    }
    println!();
    // What option (c) would cost to store: one `u16` per container per record, plus a `u32` run
    // offset per record. Measured over the fixture's own postings rather than modelled from the
    // Zipf law that generated them.
    let started = Instant::now();
    let mut containers = 0usize;
    let mut roaring_records = 0usize;
    for code in record_codes {
        let Some(idx) = postings.bench_base_record_index(AttrLocalId::new(*code)) else {
            continue;
        };
        let posting = postings
            .bench_base_posting_at_index(idx)
            .expect("a base record");
        if matches!(posting, PostingRef::Roaring(_)) {
            roaring_records += 1;
        }
        containers += container_count(&posting);
    }
    println!(
        "# postings: {} records, {roaring_records} of them Roaring; {containers} container keys \
         in all ({:.1} MB as u16 + {:.1} MB of u32 run offsets)  [{:.1} s to count]",
        record_codes.len(),
        containers as f64 * 2.0 / 1e6,
        (record_codes.len() + 1) as f64 * 4.0 / 1e6,
        started.elapsed().as_secs_f64()
    );

    println!("# ({values_n} values; every figure ns per probe unless marked)");
}

/// **The second route, priced** — §6.3's per-session visible-value set: what the sweep costs, what
/// a keystroke over the set costs, and how many keystrokes it takes to pay the sweep back.
///
/// The value column is over the **whole** entity space, which is what a build writes and what the
/// sweep walks: `entities` `u32` codes, one per entity, each the code of `entity % values`. So a
/// candidate of *n* entities sees about *n* distinct values while *n* is under `V`, which is the
/// shape the design's own arm 3 measured.
fn set_route(
    live: &VocabularySuggest,
    fold: &tessera_analyse::SuggestionFold,
    postings: &ColumnPostings,
    dir: &std::path::Path,
    values: &[SuggestValue],
    entities: u32,
) {
    let started = Instant::now();
    let values_path = dir.join("column.arrow");
    let presence_path = dir.join("column-presence.arrow");
    let codes: Vec<u32> = (0..entities)
        .map(|e| values[e as usize % values.len()].code)
        .collect();
    tessera_filter::write_value_column(
        &values_path,
        &presence_path,
        &tessera_filter::Codes::U32(codes.into()),
        None,
    )
    .expect("the value column writes");
    let column = ValueColumn::open(&values_path, None, Access::Mapped).expect("it opens");
    println!();
    println!(
        "# the set route (§6.3) — a {entities}-entity value column, written in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    println!(
        "{:<10} {:<12} {:>10} {:>10} {:>12} {:>12} {:>12} {:>12} {:>10}",
        "candidate",
        "shape",
        "sweep ms",
        "set KB",
        "visible",
        "probe ms",
        "set ms",
        "set p99 ms",
        "keystroke"
    );
    let prefixes: Vec<String> = (b'a'..=b'z').map(|c| (c as char).to_string()).collect();
    for (label, want) in [("1e4", 10_000u32), ("1e6", 1_000_000), ("1e7", 10_000_000)] {
        for (shape, contiguous) in [("contiguous", true), ("scattered", false)] {
            let fraction = want as f64 / entities as f64;
            let cand = candidate(entities, fraction, contiguous);

            let started = Instant::now();
            let set = tessera_engine::suggest_set::sweep(
                std::iter::once(&column),
                &cand,
                live.base().as_ref(),
            )
            .expect("a category sweeps");
            let sweep_ms = started.elapsed().as_secs_f64() * 1e3;

            let mut probe_ms = Vec::with_capacity(prefixes.len());
            let mut set_ms = Vec::with_capacity(prefixes.len());
            for q in &prefixes {
                let started = Instant::now();
                let (found, _) = walk(
                    live,
                    fold,
                    q,
                    WalkBudget {
                        limit: 20,
                        walk_budget: 100_000,
                        counts: false,
                    },
                    &|code| postings.intersects(AttrLocalId::new(code), &cand),
                    &|_| Ok(0u64),
                    &|e: io::Error| e,
                    None,
                )
                .expect("the walk");
                probe_ms.push(started.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(found.len());

                let started = Instant::now();
                let (found, _) = walk(
                    live,
                    fold,
                    q,
                    WalkBudget {
                        limit: 20,
                        walk_budget: 100_000,
                        counts: false,
                    },
                    // Unreachable on this arm — every value under a prefix has a dense position, so
                    // the set answers every gate test and the probe closure is never called.
                    &|_| Ok(false),
                    &|_| Ok(0u64),
                    &|e: io::Error| e,
                    Some(&set),
                )
                .expect("the walk");
                set_ms.push(started.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(found.len());
            }
            let probe = percentile(&mut probe_ms.clone(), 0.5);
            let on_set = percentile(&mut set_ms.clone(), 0.5);
            // Where the sweep pays for itself: the keystroke at which cumulative
            // `sweep + k × set` first falls below `k × probe`. `—` where a keystroke on the set is
            // no cheaper, which is the wide viewer the ceiling exists to keep off this route.
            let payback = if on_set < probe {
                format!("{:.0}", (sweep_ms / (probe - on_set)).ceil())
            } else {
                "—".to_string()
            };
            println!(
                "{label:<10} {shape:<12} {sweep_ms:>10.1} {:>10.1} {:>12} {probe:>12.3} \
{on_set:>12.4} {:>12.4} {payback:>10}",
                set.serialized_bytes() as f64 / 1e3,
                set.visible_values(),
                percentile(&mut set_ms, 0.99),
            );
        }
    }
}
