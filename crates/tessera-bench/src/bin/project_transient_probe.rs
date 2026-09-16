//! The session row projection's transient memory, per principal: walk wall clock, the anonymous
//! peak sampled every millisecond on a second thread, the result's size, and a digest of the
//! result that a second build can be checked against.
//!
//! Principals are country terms from `terms/postings.arrow` (`--country NAME=TERM,...` or
//! `NAME=@PATH`) or draws over a value column (`--dim NAME=K:weighted` or `NAME=K:uniform`), drawn
//! the way `term_images_probe` draws them, from `--seed`. The same seed and the same bundle give the
//! same fragments, so two builds of this probe run the same walks.
//!
//! The anonymous sampler and the heap trim are copied from `term_images_probe.rs`.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use croaring::{Bitmap, Portable};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};

use tessera_authz::{build_fragment, Dict, PostingsReader};
use tessera_filter::{Access, CodeSet, Codes, Scalar, ValueColumn};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long, default_value = "geo")]
    view: String,
    /// `NAME=TERM,TERM,...` or `NAME=@PATH` over the authorisation terms.
    #[arg(long = "country")]
    countries: Vec<String>,
    /// `DIMENSION=K:weighted` or `DIMENSION=K:uniform`.
    #[arg(long = "dim")]
    dims: Vec<String>,
    #[arg(long, default_value_t = 20260916)]
    seed: u64,
    /// Untraced walks per principal.
    #[arg(long, default_value_t = 2)]
    reps: usize,
    /// Also run one walk with `TESSERA_WALK_TRACE` set, where the build carries the trace.
    #[arg(long)]
    trace: bool,
    #[arg(long)]
    commit: Option<String>,
    #[arg(long)]
    out: PathBuf,
}

fn rss_anon() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("RssAnon:") {
            return r.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) * 1024;
        }
    }
    0
}

fn trim_heap() {
    // SAFETY: `malloc_trim` takes a padding size and touches only the allocator's own state.
    unsafe {
        libc::malloc_trim(0);
    }
}

struct AnonSampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    peak: std::sync::Arc<std::sync::atomic::AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
    base: u64,
}

impl AnonSampler {
    fn start() -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;
        let base = rss_anon();
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(base));
        let handle = {
            let stop = stop.clone();
            let peak = peak.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    peak.fetch_max(rss_anon(), Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            })
        };
        AnonSampler {
            stop,
            peak,
            handle: Some(handle),
            base,
        }
    }

    fn finish(mut self) -> Value {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let end = rss_anon();
        let peak = self.peak.load(Ordering::Relaxed).max(end);
        json!({
            "base_bytes": self.base,
            "peak_bytes": peak,
            "end_bytes": end,
            "peak_above_base_bytes": peak.saturating_sub(self.base),
            "end_above_base_bytes": end.saturating_sub(self.base),
        })
    }
}

fn scan_terms(column: &ValueColumn, candidate: &Bitmap, terms: &[u32], domain: u32) -> Bitmap {
    match column.codes() {
        Codes::U32(_) => {
            let mut set = CodeSet::over_domain(domain.saturating_sub(1));
            for t in terms {
                set.insert(*t);
            }
            column.scan_ordinal_set(candidate, &set)
        }
        _ => {
            let needles: Vec<Scalar> = terms.iter().map(|t| Scalar::Int(i128::from(*t))).collect();
            column.scan_num_in(candidate, &needles)
        }
    }
}

fn count_terms<T: Copy + Into<u32>>(values: &[T]) -> Vec<u64> {
    let mut counts: Vec<u64> = Vec::new();
    for v in values {
        let at = Into::<u32>::into(*v) as usize;
        if at >= counts.len() {
            counts.resize(at + 1, 0);
        }
        counts[at] += 1;
    }
    counts
}

fn draw(counts: &[u64], k: usize, weighted: bool, rng: &mut StdRng) -> Vec<u32> {
    let terms: Vec<u32> = (0..counts.len() as u32)
        .filter(|v| counts[*v as usize] > 0)
        .collect();
    let k = k.min(terms.len());
    let mut chosen: Vec<u32> = if weighted {
        let mut keyed: Vec<(f64, u32)> = terms
            .iter()
            .map(|v| {
                let u: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
                (u.ln() / counts[*v as usize] as f64, *v)
            })
            .collect();
        keyed.select_nth_unstable_by(k - 1, |a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        keyed[..k].iter().map(|(_, v)| *v).collect()
    } else {
        terms.choose_multiple(rng, k).copied().collect()
    };
    chosen.sort_unstable();
    chosen
}

fn digest(b: &Bitmap) -> String {
    use std::hash::{Hash, Hasher};
    let mut c = b.clone();
    c.run_optimize();
    let bytes = c.serialize::<Portable>();
    // SipHash with std's fixed keys: stable across two builds of this binary.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}-{}", h.finish(), bytes.len())
}

fn main() {
    let args = Args::parse();
    let mut rng = StdRng::seed_from_u64(args.seed);
    let started = Instant::now();
    let bundle = open_bundle(&args.bundle).expect("opening the bundle");
    let open_s = started.elapsed().as_secs_f64();
    let current: CurrentPointer =
        serde_json::from_slice(&std::fs::read(args.bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix_dir = args.bundle.join(&current.prefix);
    let (phash, partition) = bundle.partitions.iter().next().expect("a partition");
    let row_space = &partition.views.get(&args.view).expect("the view").row_space;
    assert_eq!(row_space.extent_count(), 0, "a bundle with extents");
    let bound = row_space.base().bound();
    let total_rows = row_space.total_rows();
    let partition_dir = prefix_dir.join("partitions").join(phash);
    eprintln!("bound {bound}, total_rows {total_rows}, opened in {open_s:.1} s");

    let mut principals: Vec<(String, Bitmap)> = Vec::new();
    if !args.countries.is_empty() {
        let dicts: Vec<PathBuf> = partition
            .manifest
            .dict_extents
            .iter()
            .map(|e| prefix_dir.join(&e.path))
            .collect();
        let dict = Dict::load(&dicts).expect("the dictionary");
        let postings =
            PostingsReader::open(&partition_dir.join("terms").join("postings.arrow"), true)
                .expect("the postings");
        for spec in &args.countries {
            let (name, terms) = spec.split_once('=').expect("NAME=TERMS");
            let terms: Vec<String> = match terms.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)
                    .unwrap()
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect(),
                None => terms.split(',').map(str::to_string).collect(),
            };
            let ids: Vec<_> = terms
                .iter()
                .map(|t| dict.lookup(t.as_bytes()).expect("a known term"))
                .collect();
            principals.push((
                format!("countries-{name}"),
                build_fragment(&ids, &postings).unwrap(),
            ));
        }
    }
    for spec in &args.dims {
        let (name, rest) = spec.split_once('=').expect("DIM=K:kind");
        let (k, kind) = rest.split_once(':').expect("K:kind");
        let k: usize = k.parse().unwrap();
        let column = ValueColumn::open_dir(&partition_dir.join("attrs").join(name), Access::MappedSequential)
            .expect("the value column");
        let presence = column.present();
        let counts = match column.codes() {
            Codes::U32(v) => count_terms::<u32>(v.as_ref()),
            Codes::U16(v) => count_terms::<u16>(v.as_ref()),
            _ => panic!("u16 or u32 codes"),
        };
        let chosen = draw(&counts, k, kind == "weighted", &mut rng);
        let fragment = scan_terms(&column, &presence, &chosen, counts.len() as u32);
        principals.push((format!("{name}-{kind}-{k}"), fragment));
    }

    let mut records = Vec::new();
    for (name, fragment) in &principals {
        let hi = u32::try_from(bound.saturating_sub(1)).unwrap_or(u32::MAX);
        let share = fragment.range_cardinality(0..=hi) as f64 / bound as f64;
        let mut runs = Vec::new();
        let mut shape = Value::Null;
        for rep in 0..args.reps {
            trim_heap();
            let sampler = AnonSampler::start();
            let t = Instant::now();
            let rows = row_space.project(fragment);
            let wall = t.elapsed().as_secs_f64();
            let anon = sampler.finish();
            if rep == 0 {
                let st = rows.statistics();
                shape = json!({
                    "rows": rows.cardinality(),
                    "coverage": rows.cardinality() as f64 / total_rows as f64,
                    "containers": st.n_containers,
                    "array": st.n_array_containers,
                    "bitset": st.n_bitset_containers,
                    "run": st.n_run_containers,
                    "portable_bytes": rows.get_serialized_size_in_bytes::<Portable>(),
                    "digest_optimised": digest(&rows),
                });
            }
            drop(rows);
            trim_heap();
            let after = rss_anon();
            eprintln!(
                "{name} rep {rep}: {wall:.3} s, anon peak +{} B, end +{} B",
                anon["peak_above_base_bytes"], anon["end_above_base_bytes"]
            );
            runs.push(json!({"wall_s": wall, "anon": anon, "anon_after_drop_and_trim": after}));
        }
        if args.trace {
            trim_heap();
            std::env::set_var("TESSERA_WALK_TRACE", "1");
            eprintln!("TRACE-BEGIN {name}");
            let rows = row_space.project(fragment);
            eprintln!("TRACE-END {name}");
            std::env::remove_var("TESSERA_WALK_TRACE");
            drop(rows);
        }
        eprintln!("{name}: entity share {share:.4}, {shape}");
        records.push(json!({"name": name, "entity_share": share, "result": shape, "runs": runs}));
        let out = json!({
            "bundle": args.bundle.display().to_string(),
            "commit": args.commit,
            "seed": args.seed,
            "bound": bound,
            "total_rows": total_rows,
            "open_s": open_s,
            "principals": records,
        });
        std::fs::write(&args.out, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
}
