//! **Hidden versus absent: how long a `match` takes for a word nobody visible carries.**
//!
//! Decision 0067 accepted a timing channel here and Appendix C row C25 registers it, with no
//! figures. This is what fills them.
//!
//! The channel is structural and reading the code is enough to see it exists: `text_match` resolves
//! each token in the dictionary, and on a hit reads that term's posting and intersects it with the
//! candidate, where on a miss it takes an empty bitmap and stops. Both answers are the same — no
//! entity — so what separates them is only the work. **What is not knowable without measuring is
//! whether that difference is large enough to read across a network**, and at what vocabulary size,
//! which is what §11 item 1 asks for at ≥250k terms.
//!
//! # The arms
//!
//! Three, per stratum, against the same candidate so the intersection's own cost is held fixed:
//!
//! - **absent** — a token that is in no dictionary. One failed binary search.
//! - **hidden** — a token that *is* in the dictionary and whose every carrier is outside the
//!   candidate. A successful search, a posting decode, and an intersection that yields nothing.
//! - **visible** — the same token against a candidate that does contain its carriers. The control:
//!   it says how much of the hidden arm's cost is the posting rather than the answer.
//!
//! Stratified by posting size, because the hidden arm's cost is the posting's and a singleton term
//! is a different measurement from a head term. A principal who can see nothing of a term's
//! carriers pays for all of them either way.
//!
//! # Why the candidate is `everything \ carriers`
//!
//! It is the largest candidate for which the term is hidden, so it is the arm's worst case, and
//! using the same set for the absent arm keeps the comparison like for like — a smaller candidate
//! would make the absent arm cheaper for a reason that has nothing to do with the channel.
//!
//! Usage (from the repository root, so `.cargo/config.toml`'s alignment pin applies):
//!     cargo run --release --manifest-path \
//!         probes/2026-08-14-hidden-vs-absent/hiddentiming/Cargo.toml -- \
//!         --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Instant;

use croaring::Bitmap;
use tessera_engine::filter::{FilterColumns, FilterOperand};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::DeclaredScalar;

const COLUMN: &str = "abstract";
const PARTITION: &str = "default";

/// Repetitions per arm. Large enough that the per-call figure is not the timer's resolution: the
/// absent arm is a binary search over a mapped file and lands in the hundreds of nanoseconds.
const REPS: u32 = 2_000;

/// Independent runs per arm. The medians and minima below are over these.
const TRIALS: usize = 9;

fn main() {
    let mut snapshot = String::new();
    let mut scales = vec![400_000usize, 1_000_000];
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--snapshot" => {
                snapshot = args[i + 1].clone();
                i += 2;
            }
            "--scales" => {
                scales = args[i + 1].split(',').map(|s| s.parse().unwrap()).collect();
                i += 2;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(!snapshot.is_empty(), "--snapshot is required");
    let path = if let Some(rest) = snapshot.strip_prefix("~/") {
        format!("{}/{rest}", std::env::var("HOME").unwrap())
    } else {
        snapshot
    };
    let limit = *scales.iter().max().unwrap();

    eprintln!("reading {limit} abstracts…");
    let file = std::fs::File::open(&path).expect("the arXiv snapshot");
    let mut abstracts: Vec<String> = Vec::with_capacity(limit);
    for line in BufReader::with_capacity(1 << 20, file).lines() {
        if abstracts.len() >= limit {
            break;
        }
        let line = line.expect("a readable line");
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        abstracts.push(
            record
                .get("abstract")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    assert_eq!(abstracts.len(), limit, "the snapshot is shorter than --scales");

    println!(
        "| entities | terms | stratum | posting(s) | absent med/min ns | hidden med/min ns | \
         visible ns | hidden/absent | separation |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for scale in scales {
        run(&abstracts[..scale]);
    }
}

fn run(abstracts: &[String]) {
    let analyser = tessera_analyse::Analyser::new();

    // The index, through the shipped writers — the same two calls the batch build makes.
    let mut terms: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (entity, prose) in abstracts.iter().enumerate() {
        let entity = entity as u32;
        for token in analyser.tokens(prose) {
            let postings = terms.entry(token).or_default();
            if postings.last() != Some(&entity) {
                postings.push(entity);
            }
        }
    }
    let term_count = terms.len();

    let tmp = tempfile::tempdir().expect("a scratch directory");
    let column_dir = tmp
        .path()
        .join("partitions")
        .join(PARTITION)
        .join("attrs")
        .join(COLUMN);
    std::fs::create_dir_all(&column_dir).expect("the column directory");
    tessera_filter::write_sorted_dict(
        &column_dir.join(tessera_filter::DICT_FILE),
        terms.keys().map(String::as_str),
    )
    .expect("the token dictionary writes");
    let per_term: Vec<Vec<u32>> = terms.values().cloned().collect();
    tessera_authz::postings::write_postings(
        &column_dir.join("postings.arrow"),
        &per_term,
        tessera_types::SMALL_TERM_THRESHOLD_DEFAULT,
    )
    .expect("the token postings write");

    // **An empty record blob beside it, because text is blob-resident and the reader derives that
    // from the declaration rather than probing for the file.** The measurement never reads it —
    // `match` answers from postings alone — but opening without it would be opening a shape no
    // deployment has, and the point of this harness is that the timed call is the shipped one.
    let record_dir = tmp.path().join("partitions").join(PARTITION).join("attrs/record");
    std::fs::create_dir_all(&record_dir).expect("the record directory");
    tessera_filter_write::RecordBlobWriter::create(
        &record_dir.join(tessera_filter::RECORD_BLOCKS_FILE),
        &record_dir.join(tessera_filter::RECORD_HASROW_FILE),
        &record_dir.join(tessera_filter::RECORD_DIRECTORY_FILE),
        tessera_filter::RECORD_BLOCK_TARGET,
    )
    .expect("the record blob writer")
    .finish()
    .expect("an empty record blob writes");

    // **The shipped reader, opened as a session opens it** — mapped, from a declaration, with the
    // analyser identity the manifest would carry.
    let declared = vec![DeclaredScalar {
        name: COLUMN.to_string(),
        arrow_type: ScalarType::Text,
        vocabulary: None,
        analyser: Some(analyser.identity().to_string()),
        index: true,
        render: false,
    }];
    let columns = FilterColumns::open(tmp.path(), PARTITION, &declared, &[], &[], &[], &[], true)
        .expect("the text column opens");

    // Strata by posting size: one term per decade, taken from the middle of each band so the
    // choice is not the extreme.
    let mut by_size: Vec<(&String, usize)> = terms.iter().map(|(k, v)| (k, v.len())).collect();
    by_size.sort_by_key(|(_, n)| *n);
    let strata: [(&str, usize, usize); 4] = [
        ("singleton", 1, 1),
        ("small", 10, 100),
        ("mid", 1_000, 10_000),
        ("head", 100_000, usize::MAX),
    ];

    multiword(&columns, &terms, &by_size, abstracts.len(), term_count);

    for (label, lo, hi) in strata {
        let Some((term, size)) = pick(&by_size, lo, hi) else {
            println!(
                "| {} | {term_count} | {label} | — | — | — | — | — | no term in this band |",
                abstracts.len()
            );
            continue;
        };
        let carriers: Bitmap = terms[term].iter().copied().collect();

        // Everything except this term's carriers: the largest candidate for which it is hidden.
        let mut hidden_candidate = Bitmap::new();
        hidden_candidate.add_range(0..=(abstracts.len() as u32 - 1));
        hidden_candidate.andnot_inplace(&carriers);
        // The control's candidate is the whole corpus, so the two differ by exactly the carriers.
        let mut all = Bitmap::new();
        all.add_range(0..=(abstracts.len() as u32 - 1));

        // A token no dictionary holds. Checked rather than assumed — a nonce that happened to be a
        // real term would silently make the absent arm a second hidden one.
        let nonce = "zzqxvnonce";
        assert!(!terms.contains_key(nonce), "the absent arm's token is in the corpus");

        // **Interleaved rather than one arm after the other**: a machine that drifts warmer over
        // the campaign's minutes would otherwise put the drift entirely into whichever arm ran
        // last, and the whole result is a difference between arms.
        let (absent, absent_min) = time(&columns, nonce, &hidden_candidate, 0);
        let (hidden, hidden_min) = time(&columns, term, &hidden_candidate, 0);
        let (visible, _) = time(&columns, term, &all, size as u64);
        let (absent2, absent_min2) = time(&columns, nonce, &hidden_candidate, 0);
        let absent = absent.min(absent2);
        let absent_min = absent_min.min(absent_min2);

        println!(
            "| {} | {term_count} | {label} (`{term}`) | {size} | {absent:.0} / {absent_min:.0} | \
             {hidden:.0} / {hidden_min:.0} | {visible:.0} | {:.2}× | {:+.0} ns |",
            abstracts.len(),
            hidden / absent,
            hidden_min - absent_min
        );
    }
}

/// **A multi-word conjunction's cost as the token count grows** — the arm the accumulator rewrite
/// was made for.
///
/// Two things were stacked before it. Every token's answer was held at once, so a query of *n*
/// words held *n* bitmaps; and each token's **corpus-wide** posting was materialised as an owned
/// bitmap before being narrowed to the candidate, so the transient peak was the posting's size
/// rather than the answer's. The rewrite carries one running set, narrowed token by token, and
/// intersects against the mapped posting *view* so the corpus-wide set is never assembled at all.
///
/// What this measures is the wall clock, which is the half a harness can see from outside: the
/// resident half needs a peak-RSS reading the shipped process does not take. The prediction is
/// that time falls too — a bitmap operation costs O(containers touched), so every step after the
/// first works against an already-narrowed set — and a measurement that showed it flat would mean
/// the container arithmetic is not where the cost is.
fn multiword(
    columns: &FilterColumns,
    terms: &BTreeMap<String, Vec<u32>>,
    by_size: &[(&String, usize)],
    entities: usize,
    term_count: usize,
) {
    // Head words, which is where a conjunction is expensive: each posting is large, so a route
    // that materialised them all held the most and a route that narrows as it goes saves the most.
    let heads: Vec<&str> = by_size
        .iter()
        .rev()
        .take(8)
        .map(|(k, _)| k.as_str())
        .collect();
    let mut all = Bitmap::new();
    all.add_range(0..=(entities as u32 - 1));

    println!("CONJ | entities | terms | words | postings read | ns med/min | answer |");
    for n in [1usize, 2, 4, 8] {
        let query = heads[..n].join(" ");
        // The postings this query reads, summed — the quantity the old route's peak tracked.
        let posting_sum: usize = heads[..n].iter().map(|t| terms[*t].len()).sum();
        let (median, min) = time_conjunction(columns, &query, &all);
        let hits = columns
            .resolve(
                COLUMN,
                &FilterOperand::Match {
                    query: query.clone(),
                    minimum: None,
                },
                &all,
            )
            .expect("resolve")
            .cardinality();
        println!(
            "| {entities} | {term_count} | {n}-word conjunction | {posting_sum} |              {median:.0} / {min:.0} | {hits} |",
        );
    }
}

/// Nanoseconds per multi-word `resolve`, median and minimum over [`TRIALS`] runs.
fn time_conjunction(columns: &FilterColumns, query: &str, candidate: &Bitmap) -> (f64, f64) {
    let operand = FilterOperand::Match {
        query: query.to_string(),
        minimum: None,
    };
    for _ in 0..16 {
        let _ = columns.resolve(COLUMN, &operand, candidate).expect("resolve");
    }
    let reps = 200u32;
    let mut runs: Vec<f64> = Vec::with_capacity(TRIALS);
    for _ in 0..TRIALS {
        let start = Instant::now();
        for _ in 0..reps {
            let _ = columns.resolve(COLUMN, &operand, candidate).expect("resolve");
        }
        runs.push(start.elapsed().as_nanos() as f64 / f64::from(reps));
    }
    runs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    (runs[runs.len() / 2], runs[0])
}

/// A term whose posting size is in `[lo, hi]`, taken from the middle of the band.
fn pick<'a>(by_size: &[(&'a String, usize)], lo: usize, hi: usize) -> Option<(&'a str, usize)> {
    let band: Vec<&(&String, usize)> = by_size
        .iter()
        .filter(|(_, n)| *n >= lo && *n <= hi)
        .collect();
    band.get(band.len() / 2).map(|(k, n)| (k.as_str(), *n))
}

/// Nanoseconds per `resolve`: the **median and the minimum across [`TRIALS`] independent runs** of
/// [`REPS`] calls each.
///
/// Two figures rather than one, and the reason is the singleton stratum. The difference this
/// campaign is trying to see there is a couple of hundred nanoseconds against a call of about
/// five hundred, so a single mean cannot distinguish a real channel from scheduler noise. The
/// minimum is the cleanest run the machine gave — the figure an adversary approaches by taking
/// many samples — and the spread between it and the median is what says whether the arms are
/// separable at all at this stratum.
///
/// The answer's cardinality is asserted, so a timing that measured a short circuit — or a route
/// that stopped answering — cannot pass as a measurement.
fn time(columns: &FilterColumns, query: &str, candidate: &Bitmap, expect: u64) -> (f64, f64) {
    let operand = FilterOperand::Match {
        query: query.to_string(),
        minimum: None,
    };
    // Warm: the first call faults the mapping in, and the channel is about the steady state.
    for _ in 0..64 {
        let _ = columns.resolve(COLUMN, &operand, candidate).expect("resolve");
    }
    let mut runs: Vec<f64> = Vec::with_capacity(TRIALS);
    for _ in 0..TRIALS {
        let start = Instant::now();
        let mut last = 0u64;
        for _ in 0..REPS {
            last = columns
                .resolve(COLUMN, &operand, candidate)
                .expect("resolve")
                .cardinality();
        }
        let elapsed = start.elapsed();
        assert_eq!(
            last, expect,
            "the arm answered {last} entities where {expect} was expected — it is not measuring \
             what it says it is"
        );
        runs.push(elapsed.as_nanos() as f64 / f64::from(REPS));
    }
    runs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN from a duration"));
    (runs[runs.len() / 2], runs[0])
}
