//! The session row projection's four routes, timed against each other over a built bundle.
//!
//! A session's first viewport begins with a row projection build. `RowProjection::new` chooses
//! between the whole-domain answer, the walk, the split over the bundle's term images and the
//! complement, from the principal's own grant and before any route runs. This probe runs the
//! chosen route and each forced route over the same fragment and reports what each cost.
//!
//! **The shipped code, not a transcription of it.** The bundle is opened with `open_bundle`, the
//! call `tessera serve` makes, so the base permutation's `dense_rows` state and the view's mapped
//! images are the ones a server has. The fragment is built through `FragmentCache::get_or_build`,
//! the engine's own path. Each arm is `RowProjection::new(&inputs, &row_space)` with
//! `ProjectionInputs::force` set, which is the one constructor the request path uses. A probe that
//! re-implemented a route would measure the transcription.
//!
//! # Per principal
//!
//! Four arms — `force = None` (the chooser), `Some(Walk)`, `Some(Split)`, `Some(Complement)` — and
//! each arm run twice:
//!
//! - **cold**, after `posix_fadvise(POSIX_FADV_DONTNEED)` over `permutation.bin`, the view's
//!   `.timg` file and `postings.arrow`. The eviction is best effort and does not reach a page this
//!   process holds mapped, so every arm records `read_bytes` and the major-fault delta beside its
//!   wall and the reader judges whether it took.
//! - **warm**, the same arm run again with the page cache the cold run left.
//!
//! Each run records the route taken, wall, CPU, minor and major faults, `read_bytes`, the peak
//! resident size with `VmHWM` reset for the run, the sampled anonymous peak, and the projection's
//! cardinality and `Portable` size. **Every arm's rows are checked equal to the walk's**; a
//! mismatch is recorded with both cardinalities and the first differing row, and the run
//! continues so the whole table is reported rather than the first failure.
//!
//! `ChooserInputs` is recorded per principal, with the offline `choose` verdict and each route's
//! modelled cost, so the constants can be re-derived from a results file without re-running.
//!
//! # Principals
//!
//! `--principal NAME=TERM,...` and `--terms-file` name them explicitly. `--year-uniform N` and
//! `--species-weighted N` draw them from the bundle's own dictionary: years uniformly over the
//! terms prefixed `y:`, species weighted by posting rows over those prefixed `s:`, both without
//! replacement from `--seed`. The draw reads the dictionary rather than a list, so the same
//! invocation runs unchanged at any rung of the corpus.
//!
//! ```text
//! route_probe --bundle <bundle> --view geo --seed 20260917 \
//!     --principal p1=AI,MC,MX,SM,XZ --terms-file country-terms.txt \
//!     --year-uniform 300 --species-weighted 1000 --species-weighted 10000 --out results.json
//! ```

// `serde_json::json!` expands one nesting level per key, and a run's record has more keys than
// the default 128 allows.
#![recursion_limit = "256"]

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use croaring::bitmap::Statistics;
use croaring::{Bitmap, Portable};
use serde_json::{json, Value};

use tessera_authz::{Dict, FragmentCache, FrozenFragment, PostingsReader};
use tessera_engine::{ProjectionInputs, ProjectionRoute, RowProjection};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;
use tessera_store::term_images::{
    choose, chooser_inputs, ChooserInputs, Route, TermImages, ROUTE_COSTS,
};
use tessera_store::RowSpace;
use tessera_types::process::{self, resident_bytes};
use tessera_types::TermId;

#[derive(Parser)]
#[command(about = "Time the four session projection routes against each other over one bundle")]
struct Args {
    /// Bundle root (the directory holding `CURRENT`).
    #[arg(long)]
    bundle: PathBuf,
    /// The view whose row space is projected into.
    #[arg(long)]
    view: String,
    /// `NAME=TERM,TERM,...`, repeatable. One principal each, run in the order given.
    #[arg(long = "principal")]
    principals: Vec<String>,
    /// A file of term strings, separated by commas or newlines; defines a principal named `all`.
    #[arg(long = "terms-file")]
    terms_file: Option<PathBuf>,
    /// `country-ranks.json` beside the bundle, which `--country-ladder` composes from.
    #[arg(long = "ranks-file")]
    ranks_file: Option<PathBuf>,
    /// Compose a compartment principal covering about this fraction of the corpus, by the rule
    /// `serve_battery.py` uses. Repeatable; needs `--ranks-file`.
    #[arg(long = "country-ladder")]
    country_ladder: Vec<f64>,
    /// Draw this many `y:` terms uniformly without replacement. Repeatable.
    #[arg(long = "year-uniform")]
    year_uniform: Vec<usize>,
    /// Draw this many `s:` terms weighted by posting rows, without replacement. Repeatable.
    #[arg(long = "species-weighted")]
    species_weighted: Vec<usize>,
    /// The seed both draws use. Printed and recorded.
    #[arg(long, default_value_t = 20_260_917)]
    seed: u64,
    /// The commit to record.
    #[arg(long)]
    commit: String,
    /// Where the results go.
    #[arg(long)]
    out: PathBuf,
}

// ------------------------------------------------------------------------------------------
// Process counters
//
// The shape `projection_probe.rs` and `identity_bands_probe.rs` both use: wall from a monotonic
// clock, CPU from `CLOCK_PROCESS_CPUTIME_ID` because a step under 10 ms reads as zero in the
// scheduler's 100 Hz ticks, faults and bytes from `/proc/self`.
// ------------------------------------------------------------------------------------------

fn process_cpu_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes into a `timespec` this scope owns and reads nothing else.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) } != 0 {
        return 0.0;
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// `(minflt, majflt)` from `/proc/self/stat`.
fn proc_faults() -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    // `comm` may hold spaces and parentheses; everything after the LAST ')' is field 3 onward.
    let Some(tail) = text.rfind(')').map(|i| &text[i + 1..]) else {
        return (0, 0);
    };
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let at = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f.parse::<u64>().ok())
            .unwrap_or(0)
    };
    // Field 10 is `minflt` and 12 `majflt`; `fields[i]` is field `i + 3`.
    (at(7), at(9))
}

fn io_read_bytes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/io") else {
        return 0;
    };
    text.lines()
        .find_map(|line| line.strip_prefix("read_bytes:"))
        .and_then(|rest| rest.trim().parse().ok())
        .unwrap_or(0)
}

/// One field of `/proc/self/status`, in bytes.
fn status_bytes(name: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    text.lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| {
            rest.trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Reset `VmHWM` to the current resident size, so a run's peak is its own.
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Samples `RssAnon` every millisecond on its own thread, so a run's peak anonymous memory is
/// recorded rather than only its value at the end.
struct AnonSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
    base: u64,
}

impl AnonSampler {
    fn start() -> Self {
        let base = resident_bytes().anon;
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(base));
        let (s, p) = (Arc::clone(&stop), Arc::clone(&peak));
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let now = resident_bytes().anon;
                p.fetch_max(now, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        AnonSampler {
            stop,
            peak,
            handle: Some(handle),
            base,
        }
    }

    /// `(base, peak, rise)` in bytes.
    fn finish(mut self) -> (u64, u64, u64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let peak = self.peak.load(Ordering::Relaxed).max(resident_bytes().anon);
        (self.base, peak, peak.saturating_sub(self.base))
    }
}

/// Wall, CPU, faults and bytes read across one run.
struct Counters {
    started: Instant,
    cpu_s: f64,
    minflt: u64,
    majflt: u64,
    read_bytes: u64,
}

impl Counters {
    fn now() -> Self {
        let (minflt, majflt) = proc_faults();
        Counters {
            started: Instant::now(),
            cpu_s: process_cpu_s(),
            minflt,
            majflt,
            read_bytes: io_read_bytes(),
        }
    }

    fn since(&self) -> Value {
        let wall_s = self.started.elapsed().as_secs_f64();
        let cpu_s = process_cpu_s() - self.cpu_s;
        let (minflt, majflt) = proc_faults();
        json!({
            "wall_s": wall_s,
            "cpu_s": cpu_s,
            "minflt": minflt.saturating_sub(self.minflt),
            "majflt": majflt.saturating_sub(self.majflt),
            "read_bytes": io_read_bytes().saturating_sub(self.read_bytes),
        })
    }
}

// ------------------------------------------------------------------------------------------
// Page cache
// ------------------------------------------------------------------------------------------

/// `posix_fadvise(POSIX_FADV_DONTNEED)` over each path, the eviction available without root on
/// this box.
///
/// **It does not reach a page this process holds mapped**, and the permutation and the images are
/// mapped for the whole run, so a cold arm is cold only in the part of the file the kernel was
/// willing to drop. Every run records `read_bytes` and the major-fault delta so the reader can
/// judge whether it took rather than take the label on trust.
fn evict(paths: &[PathBuf]) -> Value {
    let mut advised = Vec::new();
    for path in paths {
        let Ok(file) = File::open(path) else {
            continue;
        };
        // SAFETY: a read-only advisory call on a descriptor this scope owns.
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        advised.push(json!({
            "path": path.display().to_string(),
            "bytes": std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            "rc": rc,
        }));
    }
    json!(advised)
}

// ------------------------------------------------------------------------------------------
// Bitmap descriptions
// ------------------------------------------------------------------------------------------

fn stats_json(s: &Statistics) -> Value {
    json!({
        "n_containers": s.n_containers,
        "n_array_containers": s.n_array_containers,
        "n_run_containers": s.n_run_containers,
        "n_bitset_containers": s.n_bitset_containers,
        "cardinality": s.cardinality,
    })
}

fn portable_bytes(b: &Bitmap) -> u64 {
    b.get_serialized_size_in_bytes::<Portable>() as u64
}

/// Whether `other` is the same set as `walk`, with both cardinalities and the first differing row
/// when it is not.
fn equality(walk: &Bitmap, other: &Bitmap) -> Value {
    let diff = walk.xor(other);
    json!({
        "equal": diff.is_empty(),
        "walk_cardinality": walk.cardinality(),
        "other_cardinality": other.cardinality(),
        "first_differing_row": diff.minimum(),
    })
}

// ------------------------------------------------------------------------------------------
// Inputs
// ------------------------------------------------------------------------------------------

struct Principal {
    name: String,
    terms: Vec<String>,
}

fn parse_principal(spec: &str) -> Result<Principal, String> {
    let (name, terms) = spec
        .split_once('=')
        .ok_or_else(|| format!("--principal '{spec}' is not NAME=TERM,TERM,..."))?;
    let terms: Vec<String> = terms
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if name.is_empty() || terms.is_empty() {
        return Err(format!(
            "--principal '{spec}' names no principal or no term"
        ));
    }
    Ok(Principal {
        name: name.to_string(),
        terms,
    })
}

fn read_terms_file(path: &Path) -> Result<Vec<String>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let terms: Vec<String> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if terms.is_empty() {
        return Err(format!("{} holds no term", path.display()));
    }
    Ok(terms)
}

/// One entry of a rung's `country-ranks.json`: a term and the (item, term) pairs carrying it.
#[derive(serde::Deserialize)]
struct Rank {
    term: String,
    pairs: u64,
}

/// A greedy term set covering about `target` of `total_rows`, by the rule
/// `test_corpora/common/serve_battery.py`'s `compose_ladder` uses, so a ladder rung here is the
/// same principal the battery would compose for the same corpus.
///
/// Fill descending by size while the running sum stays under the budget, then consider the one
/// smallest unused term that would overshoot and take whichever sum is closer to the budget **in
/// ratio**. Ratio rather than absolute difference, because a rung's job is to sit at an order of
/// magnitude.
///
/// Composed from the corpus rather than written down, so the same invocation runs at a rung whose
/// compartment vocabulary is a prefix of another's. The rung 6 sets of the evidence memo are what
/// this produces at rung 6.
fn compose_ladder(
    ranks: &[Rank],
    total_rows: u64,
    target: f64,
) -> (Vec<String>, u64, &'static str) {
    if target >= 1.0 {
        let mut terms: Vec<String> = ranks.iter().map(|r| r.term.clone()).collect();
        terms.sort();
        let total = ranks.iter().map(|r| r.pairs).sum();
        return (terms, total, "all");
    }
    let budget = target * total_rows as f64;
    let mut descending: Vec<&Rank> = ranks.iter().collect();
    descending.sort_by(|a, b| b.pairs.cmp(&a.pairs).then(a.term.cmp(&b.term)));
    let mut chosen: Vec<String> = Vec::new();
    let mut total = 0u64;
    for entry in &descending {
        if (total + entry.pairs) as f64 <= budget {
            chosen.push(entry.term.clone());
            total += entry.pairs;
        }
    }
    let mut ascending: Vec<&Rank> = ranks.iter().collect();
    ascending.sort_by(|a, b| a.pairs.cmp(&b.pairs).then(a.term.cmp(&b.term)));
    let mut rule = "fill";
    if let Some(smallest) = ascending.iter().find(|r| !chosen.contains(&r.term)) {
        let over = (total + smallest.pairs) as f64;
        let under = total.max(1) as f64;
        if (over / budget).ln().abs() < (under / budget).ln().abs() {
            chosen.push(smallest.term.clone());
            total += smallest.pairs;
            rule = "fill+smallest-overshoot";
        }
    }
    chosen.sort();
    (chosen, total, rule)
}

/// SplitMix64, so a draw is reproducible from the seed alone and does not depend on which `rand`
/// version the workspace resolves to.
struct SplitMix(u64);

impl SplitMix {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform in `(0, 1)`; never zero, so a logarithm of it is finite.
    fn next_unit(&mut self) -> f64 {
        let bits = self.next_u64() >> 11;
        (bits as f64 + 0.5) / (1u64 << 53) as f64
    }
}

/// `count` of `candidates` without replacement, each drawn with probability proportional to its
/// weight — the exponential race: take the `count` smallest values of `-ln(u) / w`.
///
/// One pass and a `count`-sized heap, which is what makes it usable over the 1.4×10⁶ species
/// terms of the whole corpus. A weight of zero is never drawn. Uniform selection is this with
/// every weight one, so both draws run the same code and the same seed stream.
fn weighted_draw(candidates: &[(u32, f64)], count: usize, rng: &mut SplitMix) -> Vec<u32> {
    use std::cmp::Ordering as CmpOrdering;
    use std::collections::BinaryHeap;

    /// A key ordered by `key` alone, largest first, so the heap's top is the one to drop.
    struct Keyed(f64, u32);
    impl PartialEq for Keyed {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }
    impl Eq for Keyed {}
    impl PartialOrd for Keyed {
        fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Keyed {
        fn cmp(&self, other: &Self) -> CmpOrdering {
            self.0
                .partial_cmp(&other.0)
                .unwrap_or(CmpOrdering::Equal)
                .then(self.1.cmp(&other.1))
        }
    }

    let mut heap: BinaryHeap<Keyed> = BinaryHeap::with_capacity(count + 1);
    for &(id, weight) in candidates {
        if weight <= 0.0 {
            continue;
        }
        let key = -rng.next_unit().ln() / weight;
        heap.push(Keyed(key, id));
        if heap.len() > count {
            heap.pop();
        }
    }
    let mut drawn: Vec<u32> = heap.into_iter().map(|k| k.1).collect();
    drawn.sort_unstable();
    drawn
}

fn box_json() -> Value {
    json!({
        "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).ok(),
        "cores": std::thread::available_parallelism().map(|n| n.get()).ok(),
    })
}

// ------------------------------------------------------------------------------------------
// The run
// ------------------------------------------------------------------------------------------

/// What every arm over one view reads, beside the principal's own fragment: the postings, the
/// view's images and row space, and the files a cold arm evicts.
struct Subject<'a> {
    postings: &'a PostingsReader,
    images: Option<&'a TermImages>,
    row_space: &'a RowSpace,
    evictable: &'a [PathBuf],
}

/// One arm, cold then warm, with equality against `walk` when one is given.
fn arm(
    label: &str,
    force: Option<ProjectionRoute>,
    subject: &Subject<'_>,
    fragment: &FrozenFragment,
    satisfied: &[TermId],
    walk: Option<&Bitmap>,
) -> Result<(Value, Bitmap), String> {
    let run = |cold: bool| -> Result<(Value, Bitmap), String> {
        let advised = if cold {
            evict(subject.evictable)
        } else {
            json!(null)
        };
        process::trim_heap();
        reset_peak();
        let sampler = AnonSampler::start();
        let counters = Counters::now();
        let inputs = ProjectionInputs {
            fragment,
            satisfied,
            postings: subject.postings,
            deltas: &[],
            images: subject.images,
            force,
        };
        let (projection, route) = RowProjection::new(&inputs, subject.row_space)
            .map_err(|e| format!("arm '{label}': building the projection: {e}"))?;
        let counters = counters.since();
        let (anon_base, anon_peak, anon_rise) = sampler.finish();
        let bitmap = projection.bitmap().clone();
        Ok((
            json!({
                "cold": cold,
                "route_taken": route.name(),
                "counters": counters,
                "peak_rss_bytes": status_bytes("VmHWM:"),
                "anon_base_bytes": anon_base,
                "anon_peak_bytes": anon_peak,
                "anon_rise_bytes": anon_rise,
                "cardinality": projection.cardinality(),
                "portable_bytes": portable_bytes(&bitmap),
                "statistics": stats_json(&bitmap.statistics()),
                "evicted": advised,
            }),
            bitmap,
        ))
    };

    let (cold, cold_rows) = run(true)?;
    let (warm, warm_rows) = run(false)?;
    let record = json!({
        "arm": label,
        "forced": force.map(|r| r.name()),
        "cold": cold,
        "warm": warm,
        "cold_equals_warm": equality(&cold_rows, &warm_rows),
        "equal_to_walk": walk.map(|w| equality(w, &warm_rows)),
    });
    eprintln!(
        "  {label:<11} cold {:>8.3} s  warm {:>8.3} s  route {}  {} rows",
        cold["counters"]["wall_s"].as_f64().unwrap_or(0.0),
        warm["counters"]["wall_s"].as_f64().unwrap_or(0.0),
        warm["route_taken"].as_str().unwrap_or("?"),
        warm["cardinality"].as_u64().unwrap_or(0),
    );
    Ok((record, warm_rows))
}

/// What a cold arm evicts: the permutation the walk reads, the postings the residual reads and
/// the images the split maps.
fn evictable(partition_dir: &Path, view: &str, postings_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        partition_dir
            .join("views")
            .join(view)
            .join("permutation.bin"),
        postings_path.to_path_buf(),
    ];
    if let Ok(entries) = std::fs::read_dir(partition_dir.join("term-images")) {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        found.sort();
        paths.extend(found);
    }
    paths.retain(|p| p.exists());
    paths
}

/// The compartment ladder's principals, composed from `--ranks-file`, with the record of each.
fn ladder_principals(
    args: &Args,
    base_rows: u64,
    principals: &mut Vec<Principal>,
    draws: &mut Vec<Value>,
) -> Result<(), String> {
    if args.country_ladder.is_empty() {
        return Ok(());
    }
    let path = args.ranks_file.as_ref().ok_or_else(|| {
        "--country-ladder composes from the rung's country-ranks.json; give --ranks-file"
            .to_string()
    })?;
    let ranks: Vec<Rank> = serde_json::from_slice(
        &std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("parsing {}: {e}", path.display()))?;
    for target in &args.country_ladder {
        let (terms, pairs, rule) = compose_ladder(&ranks, base_rows, *target);
        if terms.is_empty() {
            return Err(format!(
                "--country-ladder {target}: no term set composes to it over {} ranks",
                ranks.len()
            ));
        }
        let name = format!("p{}", (target * 100.0).round() as u64);
        draws.push(json!({
            "name": name,
            "kind": "country ladder",
            "target": target,
            "rule": rule,
            "target_pairs": pairs,
            "terms": terms,
        }));
        principals.push(Principal { name, terms });
    }
    Ok(())
}

/// The year and species principals, drawn from the dictionary so the same invocation runs at any
/// rung, with the record of each draw.
///
/// A species' weight is its posting's row count, which the image table records for every term
/// whether or not its image was kept. A view with no image table cannot be drawn from this way,
/// and the refusal says so rather than falling back to a uniform draw reported as weighted.
fn drawn_principals(
    args: &Args,
    descriptors: &[String],
    images: Option<&TermImages>,
    principals: &mut Vec<Principal>,
    draws: &mut Vec<Value>,
) -> Result<(), String> {
    let by_prefix = |prefix: &str| -> Vec<u32> {
        descriptors
            .iter()
            .enumerate()
            .filter(|(_, d)| d.starts_with(prefix))
            .map(|(i, _)| i as u32)
            .collect()
    };
    if !args.species_weighted.is_empty() && images.is_none() {
        return Err(
            "--species-weighted needs the view's term-image table for the posting row counts it \
             weights by, and this view has none"
                .to_string(),
        );
    }
    let weight = |id: u32| {
        images
            .and_then(|t| t.entry(TermId::new(id)))
            .map_or(0.0, |e| e.rows as f64)
    };
    // Years are drawn uniformly, species by size.
    let classes = [
        (
            "year",
            "y:",
            "--year-uniform",
            "year, uniform",
            false,
            &args.year_uniform,
        ),
        (
            "species",
            "s:",
            "--species-weighted",
            "species, size-weighted",
            true,
            &args.species_weighted,
        ),
    ];
    let mut rng = SplitMix(args.seed);
    for (class, prefix, flag, kind, weighted, counts) in classes {
        for count in counts {
            let candidates: Vec<(u32, f64)> = by_prefix(prefix)
                .into_iter()
                .map(|id| (id, if weighted { weight(id) } else { 1.0 }))
                .collect();
            if candidates.is_empty() {
                return Err(format!(
                    "{flag}: the dictionary holds no term prefixed '{prefix}'"
                ));
            }
            let drawn = weighted_draw(&candidates, *count, &mut rng);
            let name = format!("{class}{count}");
            draws.push(json!({
                "name": name,
                "kind": kind,
                "asked": count,
                "candidates": candidates.len(),
                "drawn": drawn.len(),
            }));
            principals.push(Principal {
                name,
                terms: drawn
                    .iter()
                    .map(|id| descriptors[*id as usize].clone())
                    .collect(),
            });
        }
    }
    Ok(())
}

/// Each principal's terms as ids, ascending and deduplicated as a session's own are; a term the
/// dictionary does not hold is refused before anything is measured.
fn resolve(principals: &[Principal], dict: &Dict) -> Result<Vec<(String, Vec<TermId>)>, String> {
    principals
        .iter()
        .map(|principal| {
            let mut ids = principal
                .terms
                .iter()
                .map(|term| {
                    dict.lookup(term.as_bytes()).ok_or_else(|| {
                        format!(
                            "principal '{}': term '{term}' is not in the bundle dictionary",
                            principal.name
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            ids.sort_unstable();
            ids.dedup();
            Ok((principal.name.clone(), ids))
        })
        .collect()
}

/// One principal: what the chooser is given and makes of it — recorded so the constants can be
/// re-derived from the results without re-running — then the walk and the three other arms.
fn measure(
    name: &str,
    ids: &[TermId],
    subject: &Subject<'_>,
    cache: &FragmentCache,
) -> Result<Value, String> {
    eprintln!("principal {name}: {} terms", ids.len());
    let fragment = cache
        .get_or_build(ids, subject.postings, &[], 0)
        .map_err(|e| format!("principal '{name}': building the fragment: {e}"))?;
    let base = subject.row_space.base();
    let bound = base.bound();
    let held = match bound.checked_sub(1).and_then(|hi| u32::try_from(hi).ok()) {
        Some(hi) => fragment.view().range_cardinality(0..=hi),
        None => 0,
    };

    let complement_valid = base.dense_rows().is_some();
    let chooser = subject.images.map_or(
        ChooserInputs {
            held,
            bound,
            complement_valid,
            ..ChooserInputs::default()
        },
        |t| chooser_inputs(t, ids, held, bound, complement_valid, 0),
    );
    let prices = ROUTE_COSTS.price(&chooser);
    let chosen_offline = match choose(&chooser, &ROUTE_COSTS) {
        Route::Walk => "walk",
        Route::Split => "split",
        Route::Complement => "complement",
    };

    // The walk first, so every other arm has something to be checked against.
    let walk = Some(ProjectionRoute::Walk);
    let (walk_record, walk_rows) = arm("walk", walk, subject, &fragment, ids, None)?;
    let mut arms = vec![walk_record];
    for (label, force) in [
        ("chooser", None),
        ("split", Some(ProjectionRoute::Split)),
        ("complement", Some(ProjectionRoute::Complement)),
    ] {
        arms.push(arm(label, force, subject, &fragment, ids, Some(&walk_rows))?.0);
    }

    Ok(json!({
        "name": name,
        "terms": ids.len(),
        "term_ids": ids.iter().map(|id| id.raw()).collect::<Vec<_>>(),
        "fragment_cardinality": fragment.view().cardinality(),
        "held_below_bound": held,
        "coverage": held as f64 / bound.max(1) as f64,
        "chooser_inputs": {
            "held": chooser.held,
            "bound": chooser.bound,
            "complement_valid": chooser.complement_valid,
            "kept_arrays_and_runs": chooser.kept_arrays_and_runs,
            "kept_bitsets": chooser.kept_bitsets,
            "kept_terms": chooser.kept_terms,
            "residual_entities": chooser.residual_entities,
        },
        "chooser_priced_ns": {
            "walk": prices.walk,
            "split": prices.split,
            "complement": prices.complement,
        },
        "chooser_route_offline": chosen_offline,
        "arms": arms,
    }))
}

/// `principal/arm` for every arm whose rows differ from the walk's.
fn mismatched(principals: &[Value]) -> Vec<String> {
    principals
        .iter()
        .flat_map(|principal| {
            let arms = principal["arms"].as_array().into_iter().flatten();
            arms.filter(|a| a["equal_to_walk"]["equal"] == json!(false))
                .map(|a| {
                    format!(
                        "{}/{}",
                        principal["name"].as_str().unwrap_or("?"),
                        a["arm"].as_str().unwrap_or("?")
                    )
                })
        })
        .collect()
}

fn run(args: Args) -> Result<(), String> {
    // ---- The bundle, opened as `tessera serve` opens it.
    let opened = Counters::now();
    let bundle = open_bundle(&args.bundle).map_err(|e| format!("opening the bundle: {e}"))?;
    let opened = opened.since();

    let current: CurrentPointer = serde_json::from_slice(
        &std::fs::read(args.bundle.join("CURRENT")).map_err(|e| format!("reading CURRENT: {e}"))?,
    )
    .map_err(|e| format!("parsing CURRENT: {e}"))?;
    let prefix_dir = args.bundle.join(&current.prefix);

    let (phash, partition) = bundle
        .partitions
        .iter()
        .next()
        .ok_or_else(|| "the bundle has no partition".to_string())?;
    let view_data = partition.views.get(&args.view).ok_or_else(|| {
        let mut names: Vec<&String> = partition.views.keys().collect();
        names.sort();
        format!("no view '{}' in the bundle; it has {:?}", args.view, names)
    })?;
    let row_space = &view_data.row_space;
    let images = view_data.term_images.as_deref();

    // ---- The dictionary and the postings, as `Engine::open` loads them.
    let dict_paths: Vec<PathBuf> = partition
        .manifest
        .dict_extents
        .iter()
        .map(|e| prefix_dir.join(&e.path))
        .collect();
    let dict = Dict::load(&dict_paths).map_err(|e| format!("loading the dictionary: {e}"))?;
    let descriptors: Vec<String> = (0..dict.len())
        .map(|id| {
            let bytes = dict.descriptor(TermId::new(id)).unwrap_or_default();
            String::from_utf8_lossy(bytes).into_owned()
        })
        .collect();
    let partition_dir = prefix_dir.join("partitions").join(phash);
    let postings_path = partition_dir.join("terms").join("postings.arrow");
    let postings = PostingsReader::open(&postings_path, true)
        .map_err(|e| format!("opening {}: {e}", postings_path.display()))?;
    let evictable = evictable(&partition_dir, &args.view, &postings_path);

    // ---- The principals, every term resolved before anything is measured.
    let mut principals: Vec<Principal> = args
        .principals
        .iter()
        .map(|spec| parse_principal(spec))
        .collect::<Result<_, _>>()?;
    let mut draws = Vec::new();
    let base_rows = u64::from(row_space.base_rows());
    ladder_principals(&args, base_rows, &mut principals, &mut draws)?;
    if let Some(path) = &args.terms_file {
        principals.push(Principal {
            name: "all".to_string(),
            terms: read_terms_file(path)?,
        });
    }
    drawn_principals(&args, &descriptors, images, &mut principals, &mut draws)?;
    if principals.is_empty() {
        return Err("nothing to measure: give --principal, --terms-file or a draw".to_string());
    }
    let resolved = resolve(&principals, &dict)?;

    let bound = row_space.base().bound();
    eprintln!(
        "bundle {} view {}: bound {bound}, base_rows {base_rows}, total_rows {}, {} extents, \
         {} terms, images {}, seed {}",
        args.bundle.display(),
        args.view,
        row_space.total_rows(),
        row_space.extent_count(),
        dict.len(),
        images.map_or("none".to_string(), |t| format!("{} terms", t.dict_len())),
        args.seed,
    );

    // ---- The fragment cache the engine uses, in a directory this run cleans up.
    let cache_dir = tempfile::tempdir().map_err(|e| format!("making a cache directory: {e}"))?;
    let cache = FragmentCache::new(cache_dir.path(), [0u8; 32], [0u8; 32]);
    let subject = Subject {
        postings: &postings,
        images,
        row_space,
        evictable: &evictable,
    };
    let records = resolved
        .iter()
        .map(|(name, ids)| measure(name, ids, &subject, &cache))
        .collect::<Result<Vec<_>, _>>()?;

    let results = json!({
        "bundle": args.bundle.display().to_string(),
        "view": args.view,
        "commit": args.commit,
        "box": box_json(),
        "seed": args.seed,
        "draws": draws,
        "opened": opened,
        "bound": bound,
        "base_rows": base_rows,
        "total_rows": row_space.total_rows(),
        "extent_count": row_space.extent_count(),
        "dense_rows": row_space.base().dense_rows(),
        "dictionary_terms": dict.len(),
        "postings_terms": postings.term_count(),
        "image_file_bytes": evictable
            .iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("timg"))
            .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum::<u64>(),
        "route_costs": {
            "walk_ns_per_entity": ROUTE_COSTS.walk_ns_per_entity,
            "split_ns_per_array_or_run": ROUTE_COSTS.split_ns_per_array_or_run,
            "split_ns_per_bitset": ROUTE_COSTS.split_ns_per_bitset,
            "residual_ns_per_entity": ROUTE_COSTS.residual_ns_per_entity,
            "complement_ns_per_entity": ROUTE_COSTS.complement_ns_per_entity,
        },
        "evictable": evictable.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "principals": records,
    });

    let text = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
    std::fs::write(&args.out, text).map_err(|e| format!("writing {}: {e}", args.out.display()))?;
    eprintln!("wrote {}", args.out.display());

    // A mismatch is an invariant failure, and the exit code says so after the whole table is
    // written: the reader needs the other principals to tell a route's bug from a corpus's.
    let mismatched = mismatched(&records);
    if !mismatched.is_empty() {
        return Err(format!(
            "route(s) whose rows differ from the walk's: {mismatched:?}. The table is in {}",
            args.out.display()
        ));
    }
    Ok(())
}

fn main() {
    let args = Args::parse();
    if let Err(message) = run(args) {
        eprintln!("route_probe: {message}");
        std::process::exit(2);
    }
}
