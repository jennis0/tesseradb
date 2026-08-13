//! What is recoverable from keyword `contains`, measured against the shipped routes.
//!
//! `2026-08-13-utf8-retirement-fence` measured the keyword family's `contains` at 1.5–71× slower
//! than the flat `utf8` column it replaced, and reported two things about *where* the cost sits
//! that this campaign follows up:
//!
//! - result 6, "the dictionary walk with the substring search costs 10.7–25.1 ns per key … varying
//!   with the **needle's length** rather than with what it matches", against the dictionary
//!   campaign's 11.0–18.8 ns for the decode alone. A cost that scales with the needle and not with
//!   the corpus is the signature of a searcher *constructed per key*: `str::contains(&str)` builds
//!   a two-way searcher on every call, and the shipped broad route calls it once per dictionary
//!   key. Arm `finder` hoists the construction out of the walk.
//! - the narrow route at 0.07–0.15 µs per candidate entity — about `restart_interval / 2` block
//!   decodes, one per entity, with no account taken of entities that share an ordinal or of
//!   ordinals that share a block. Arm `sorted_distinct` takes both.
//!
//! Every arm is asserted to produce the shipped route's exact answer before it is timed.
//!
//! Usage (from the repository root, so `.cargo/config.toml`'s alignment pin applies):
//!     cargo run --release --manifest-path probes/2026-08-13-contains-recovery/recover/Cargo.toml \
//!         -- --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json

use std::collections::HashMap;
use std::hint::black_box;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use memchr::memmem;
use tessera_filter::{SortedDict, SortedDictWriter, DEFAULT_RESTART_INTERVAL};

/// Needle lengths swept. The fence's claim is that the shipped walk's cost rises with this number
/// and the decode's does not, so it is the axis that separates the two.
const NEEDLE_LENS: [usize; 5] = [3, 4, 6, 8, 16];

fn main() {
    let mut snapshot = String::new();
    let mut limit = 2_400_000usize;
    let mut reps = 3usize;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--snapshot" => {
                snapshot = args[i + 1].clone();
                i += 2;
            }
            "--limit" => {
                limit = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--reps" => {
                reps = args[i + 1].parse().unwrap();
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

    // Snapshot order = submission order = entity order (`probes/dataset.md` §5 rule 1), so a scale
    // is a prefix, exactly as every campaign over this corpus has taken it.
    let file = std::fs::File::open(&path).expect("the arXiv snapshot");
    let mut columns: Vec<(&str, Vec<String>)> = vec![
        ("id", Vec::with_capacity(limit)),
        ("submitter", Vec::with_capacity(limit)),
        ("doi", Vec::with_capacity(limit)),
    ];
    for line in BufReader::with_capacity(1 << 20, file).lines() {
        if columns[0].1.len() >= limit {
            break;
        }
        let line = line.expect("a readable line");
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        for (name, values) in columns.iter_mut() {
            values.push(
                record
                    .get(*name)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    let rows = columns[0].1.len();
    println!("# contains recovery — {rows} rows, restart interval {DEFAULT_RESTART_INTERVAL}, median of {reps}\n");

    for (name, values) in &columns {
        let column = Column::build(name, values);
        println!(
            "## {name} — {} present of {rows} rows, {} distinct keys, {} blocks",
            column.present.len(),
            column.dict.len(),
            column.dict.block_count()
        );
        broad(&column, reps);
        narrow(&column, reps);
        println!();
    }
}

/// One real column: its dictionary, and every present row's ordinal in entity order.
struct Column {
    dict: SortedDict,
    /// `(entity, ordinal)` for every row carrying a value, in entity order — the layer's rank
    /// addressing flattened, which is what the narrow route walks.
    present: Vec<(u32, u32)>,
}

impl Column {
    fn build(name: &str, values: &[String]) -> Self {
        let mut keys: Vec<&str> = values.iter().map(String::as_str).filter(|v| !v.is_empty()).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut bytes = Vec::new();
        let mut writer = SortedDictWriter::new(&mut bytes).expect("a dictionary writer");
        let mut ordinal_of: HashMap<&str, u32> = HashMap::with_capacity(keys.len());
        for key in &keys {
            let ordinal = writer.push(key).expect("an ascending key");
            ordinal_of.insert(key, ordinal);
        }
        writer.finish().expect("a finished dictionary");
        let dict = SortedDict::from_vec(bytes).expect("a readable dictionary");
        dict.self_check().unwrap_or_else(|e| panic!("{name}: {e}"));
        let present = values
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_empty())
            .map(|(row, v)| (row as u32, ordinal_of[v.as_str()]))
            .collect();
        Column { dict, present }
    }

    /// A needle of `len` bytes taken from the middle of the dictionary, so it matches at least the
    /// key it came from and its length is the controlled variable. `None` when no key is long
    /// enough, which is honest for a short column rather than silently shortening the needle.
    fn needle(&self, len: usize) -> Option<String> {
        let mut scratch = Vec::new();
        let key = self.dict.key_of(self.dict.len() / 2, &mut scratch).ok()?;
        let start = 2.min(key.len());
        key.get(start..start + len).map(str::to_owned)
    }
}

/// The broad route's walk: decode alone, the shipped per-key `str::contains`, and the same search
/// with the searcher hoisted out of the loop.
fn broad(column: &Column, reps: usize) {
    println!("\n### broad walk, ns per dictionary key");
    println!("| needle bytes | matching keys | decode only | shipped `str::contains` | hoisted `memmem::Finder` | recovered |");
    println!("|---|---|---|---|---|---|");
    let keys = f64::from(column.dict.len());

    let decode = median(reps, || {
        let t = Instant::now();
        column.dict.walk(|_, key| { black_box(key); }).expect("a walkable dictionary");
        t.elapsed()
    });

    for len in NEEDLE_LENS {
        let Some(needle) = column.needle(len) else {
            println!("| {len} | — | — | *no key is long enough* | — | — |");
            continue;
        };

        // The two searches must agree on the answer before either is timed.
        let mut shipped_hits = Vec::new();
        column.dict.walk(|o, key| if key.contains(needle.as_str()) { shipped_hits.push(o) }).unwrap();
        let finder = memmem::Finder::new(needle.as_bytes());
        let mut finder_hits = Vec::new();
        column.dict.walk(|o, key| if finder.find(key.as_bytes()).is_some() { finder_hits.push(o) }).unwrap();
        assert_eq!(shipped_hits, finder_hits, "the hoisted searcher answers differently");

        let shipped = median(reps, || {
            let t = Instant::now();
            let mut n = 0u64;
            column.dict.walk(|_, key| if key.contains(needle.as_str()) { n += 1 }).unwrap();
            black_box(n);
            t.elapsed()
        });
        let hoisted = median(reps, || {
            let finder = memmem::Finder::new(needle.as_bytes());
            let t = Instant::now();
            let mut n = 0u64;
            column.dict.walk(|_, key| if finder.find(key.as_bytes()).is_some() { n += 1 }).unwrap();
            black_box(n);
            t.elapsed()
        });
        println!(
            "| {len} | {} | {:.2} | {:.2} | {:.2} | **{:.2}×** |",
            shipped_hits.len(),
            decode.as_nanos() as f64 / keys,
            shipped.as_nanos() as f64 / keys,
            hoisted.as_nanos() as f64 / keys,
            shipped.as_nanos() as f64 / hoisted.as_nanos() as f64,
        );
    }
}

/// The narrow route: one probe per candidate entity as shipped, against one probe per *distinct*
/// ordinal the candidate carries, taken in ascending order.
///
/// The deduplicated arm answers "which ordinals match", not "which entities match", so it owes a
/// scan of the ordinal column afterwards — the same second stage the broad route pays, measured by
/// the fence as result 7 and not repeated here. What is compared is the dictionary work alone,
/// which is where the narrow route's 0.07–0.15 µs sits.
fn narrow(column: &Column, reps: usize) {
    println!("\n### narrow probe, ns per candidate entity (dictionary work only)");
    println!("| candidate | entities | distinct ordinals | blocks touched | shipped probe | hoisted probe | probe per distinct | block walk | recovered |");
    println!("|---|---|---|---|---|---|---|---|---|");
    let Some(needle) = column.needle(3) else { return };
    let finder = memmem::Finder::new(needle.as_bytes());
    let interval = column.dict.restart_interval();

    let quarter = column.present.len() / 4;
    let shapes: [(&str, Vec<(u32, u32)>); 2] = [
        ("25% contiguous", column.present[..quarter].to_vec()),
        ("25% stride-4", column.present.iter().copied().step_by(4).collect()),
    ];

    for (shape, candidate) in shapes {
        let mut distinct: Vec<u32> = candidate.iter().map(|(_, o)| *o).collect();
        distinct.sort_unstable();
        distinct.dedup();
        let mut blocks: Vec<u32> = distinct.iter().map(|o| o / interval).collect();
        blocks.dedup();

        // Both arms must name the same matching ordinals.
        let mut scratch = Vec::new();
        let mut shipped_hits: Vec<u32> = candidate
            .iter()
            .filter(|(_, o)| finder.find(column.dict.key_of(*o, &mut scratch).unwrap().as_bytes()).is_some())
            .map(|(_, o)| *o)
            .collect();
        shipped_hits.sort_unstable();
        shipped_hits.dedup();
        let distinct_hits: Vec<u32> = distinct
            .iter()
            .copied()
            .filter(|o| finder.find(column.dict.key_of(*o, &mut scratch).unwrap().as_bytes()).is_some())
            .collect();
        assert_eq!(shipped_hits, distinct_hits, "the deduplicated probe answers differently");
        let mut walk_hits: Vec<u32> = Vec::new();
        column
            .dict
            .walk_ordinals(&distinct, |ordinal, key| {
                if finder.find(key.as_bytes()).is_some() {
                    walk_hits.push(ordinal);
                }
            })
            .unwrap();
        assert_eq!(shipped_hits, walk_hits, "the block walk answers differently");

        let n = candidate.len() as f64;
        // **The route as it actually shipped**: `key_of` per candidate entity *and*
        // `str::contains`, which builds a two-way searcher per entity. Writing this arm with a
        // hoisted `Finder` — as the retirement fence's `narrow_contains` did, and as the first
        // run of this campaign did — measures a route that never existed and understates what the
        // replacement recovers.
        let shipped = median(reps, || {
            let mut scratch = Vec::new();
            let t = Instant::now();
            let mut hits = 0u64;
            for (_, ordinal) in &candidate {
                let key = column.dict.key_of(*ordinal, &mut scratch).unwrap();
                if key.contains(needle.as_str()) {
                    hits += 1;
                }
            }
            black_box(hits);
            t.elapsed()
        });
        // The same loop with the searcher hoisted — the fence's arm, kept so the two are separable.
        let hoisted_probe = median(reps, || {
            let mut scratch = Vec::new();
            let t = Instant::now();
            let mut hits = 0u64;
            for (_, ordinal) in &candidate {
                let key = column.dict.key_of(*ordinal, &mut scratch).unwrap();
                if finder.find(key.as_bytes()).is_some() {
                    hits += 1;
                }
            }
            black_box(hits);
            t.elapsed()
        });
        let deduped = median(reps, || {
            let mut scratch = Vec::new();
            let t = Instant::now();
            let mut ordinals: Vec<u32> = candidate.iter().map(|(_, o)| *o).collect();
            ordinals.sort_unstable();
            ordinals.dedup();
            let mut hits = 0u64;
            for ordinal in &ordinals {
                let key = column.dict.key_of(*ordinal, &mut scratch).unwrap();
                if finder.find(key.as_bytes()).is_some() {
                    hits += 1;
                }
            }
            black_box(hits);
            t.elapsed()
        });
        // The landed route: deduplicate, then decode each block holding a wanted ordinal once.
        let walked = median(reps, || {
            let t = Instant::now();
            let mut ordinals: Vec<u32> = candidate.iter().map(|(_, o)| *o).collect();
            ordinals.sort_unstable();
            ordinals.dedup();
            let mut hits = 0u64;
            column
                .dict
                .walk_ordinals(&ordinals, |_, key| {
                    if finder.find(key.as_bytes()).is_some() {
                        hits += 1;
                    }
                })
                .unwrap();
            black_box(hits);
            t.elapsed()
        });
        println!(
            "| {shape} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | **{:.2}×** |",
            candidate.len(),
            distinct.len(),
            blocks.len(),
            shipped.as_nanos() as f64 / n,
            hoisted_probe.as_nanos() as f64 / n,
            deduped.as_nanos() as f64 / n,
            walked.as_nanos() as f64 / n,
            shipped.as_nanos() as f64 / walked.as_nanos() as f64,
        );
    }
}

fn median(reps: usize, mut run: impl FnMut() -> Duration) -> Duration {
    let mut times: Vec<Duration> = (0..reps).map(|_| run()).collect();
    times.sort_unstable();
    times[times.len() / 2]
}
