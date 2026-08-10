//! **The filter index's lifecycle, on real data.** One bundle walked from a fresh build through
//! flushes, a coalesce and a fold, with the same filters answered — and checked against the source
//! parquet — at every stage.
//!
//! `docs/design/filter-index.md` §2.2–§2.3, §5–§5.2 and §6.2 are what this measures. Three write
//! paths had only ever been exercised by synthetic fixtures in unit tests: the per-flush extent,
//! the extent coalesce, and the fold's attribute pass. This runs them over the arXiv corpus and
//! its 25M-item replication, at every transition, with correctness first.
//!
//! # Why a probe binary rather than a bench arm
//!
//! `docs/design/measurement.md` §7 declares a `filter` arm that has never been built, and the
//! matrix harness is the right shape for a *cell* — one configuration, repeated, compared against
//! its neighbours. A lifecycle walk is neither: it is stateful and strictly sequential, every
//! stage's input is the previous stage's published output, and the interesting quantities are
//! differences *between* stages of one bundle rather than between independent runs. Expressed as a
//! matrix arm each stage would rebuild the world, and the coalesce and the fold — whose whole
//! subject is what they do to an accumulated state — would have nothing to act on. The arm remains
//! owed for the steady-state question it was declared for: what a filter costs per candidate shape
//! and coverage, which is a cell.
//!
//! # What is measured against what
//!
//! **The oracle is the points file**, decoded independently (`oracle.rs`). The alternative — asking
//! the index what it holds and checking the answer is consistent — passes for any writer that is
//! wrong the same way twice.
//!
//! **The mask is the engine's.** A filter answer is compared against `oracle ∩ candidate`, where
//! the candidate is the composed entity-space set a real session produces. This campaign is about
//! the filter artefact, not about `M_auth`; taking the mask from the engine keeps the comparison
//! to the thing under test. Where the subject is the artefact rather than a served answer — the
//! coalesce's content-preservation, the fold's blanking — the candidate is instead the full entity
//! range, because a deleted entity leaves the composed candidate and its *value* is exactly what
//! has to be observed.

mod oracle;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use croaring::Bitmap;
use tessera_engine::filter::{
    candidate as compose_candidate, Endpoint, FilterColumns, FilterExpr, FilterOperand, Scalar,
};
use tessera_engine::{CategoryQuery, Engine, EngineConfig};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::{AttrLocalId, EntityId, IdentityKey};

const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
/// The scaled corpus stores Morton codes, so the build's Morton branch requires the identity
/// extent — any other would silently re-quantise (`scripts/bench_build_fixtures.sh`).
const EXTENT: Bounds = Bounds {
    x_min: 0.0,
    x_max: 65536.0,
    y_min: 0.0,
    y_max: 65536.0,
};
const COLUMNS: [&str; 5] = [
    "archive",
    "primary_category",
    "secondary_category",
    "first_author",
    "submitted_at",
];

// =================================================================================================
// Configuration
// =================================================================================================

struct Args {
    data: PathBuf,
    work: PathBuf,
    out: PathBuf,
    scale: String,
    limit: u64,
    flushes: usize,
    batch: usize,
    deletes: usize,
    terms: Vec<String>,
    repeats: usize,
    keep: bool,
    /// Open each declared column on its own and report what it resides, rather than walking the
    /// lifecycle. Answers a question the whole-`FilterColumns` open cannot: §8's residency table was
    /// measured over `u32` columns, and a `utf8` one may not behave like them.
    residency: bool,
}

fn args() -> Args {
    let mut a = Args {
        data: PathBuf::from("/home/joe/code/tessera/data/filter-lifecycle"),
        work: PathBuf::from("/home/joe/code/tessera/data/filter-lifecycle/work"),
        out: PathBuf::from("."),
        scale: "2422486".into(),
        limit: 2_422_486,
        flushes: 64,
        batch: 2_000,
        deletes: 20_000,
        terms: vec!["4".into(), "27".into(), "19".into()],
        repeats: 3,
        keep: false,
        residency: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let take = |i: &mut usize| -> String {
            *i += 1;
            argv.get(*i).cloned().unwrap_or_else(|| panic!("missing value"))
        };
        match argv[i].as_str() {
            "--data" => a.data = PathBuf::from(take(&mut i)),
            "--work" => a.work = PathBuf::from(take(&mut i)),
            "--out" => a.out = PathBuf::from(take(&mut i)),
            "--scale" => a.scale = take(&mut i),
            "--limit" => a.limit = take(&mut i).parse().unwrap(),
            "--flushes" => a.flushes = take(&mut i).parse().unwrap(),
            "--batch" => a.batch = take(&mut i).parse().unwrap(),
            "--deletes" => a.deletes = take(&mut i).parse().unwrap(),
            "--terms" => a.terms = take(&mut i).split(',').map(str::to_string).collect(),
            "--repeats" => a.repeats = take(&mut i).parse().unwrap(),
            "--keep" => a.keep = true,
            "--residency" => a.residency = true,
            other => panic!("unknown flag {other}"),
        }
        i += 1;
    }
    a
}

fn config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
        k_min: 2,
        k_max_marks: 200,
        theta_target_marks: 1 << 30,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads: tessera_engine::default_compute_threads(),
        // Long, so every flush in this run is one the campaign asked for: a tick firing on its own
        // would put an unplanned extent in the middle of a measured stage.
        flush_max_age_secs: 3600,
        max_merged_segment_bytes: None,
        compaction: tessera_engine::CompactionSchedule::off(),
    }
}

// =================================================================================================
// Reporting
// =================================================================================================

struct Csv {
    file: std::fs::File,
}

impl Csv {
    fn new(path: &Path, header: &str) -> Csv {
        let mut file = std::fs::File::create(path).expect("csv");
        writeln!(file, "{header}").unwrap();
        Csv { file }
    }
    fn row(&mut self, line: String) {
        writeln!(self.file, "{line}").unwrap();
        self.file.flush().unwrap();
    }
}

fn log(m: impl AsRef<str>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    println!("[{}] {}", now % 100_000, m.as_ref());
    std::io::stdout().flush().unwrap();
}

/// This process's resident set, in bytes. Mapped value columns count here only for the pages
/// actually touched, which is the quantity `filter-index.md` §8's residency table is about.
fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

/// Every file under a prefix that belongs to the attribute index, wherever it sits.
///
/// **Not one directory**, because a coalesce's output does not live beside the base: it lands under
/// `coalesced/<id>/attrs/<column>/` (§5.2), on the existing precedent for entity-space output
/// belonging to no segment. Counting `partitions/<p>/attrs` alone would show a coalesce producing
/// nothing, and — since the consumed extents are unlinked by a reclamation sweep that is **not
/// built** — would also hide the orphans the pass leaves behind.
fn attrs_footprint(prefix_dir: &Path) -> (u64, u64) {
    let mut bytes = 0;
    let mut files = 0;
    let mut stack = vec![(prefix_dir.to_path_buf(), false)];
    while let Some((d, inside)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let is_attrs = inside || name == std::ffi::OsStr::new("attrs");
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push((e.path(), is_attrs));
            } else if inside {
                bytes += meta.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

/// The manifest's own account of the layered artefact: which prefix is live, how many extents each
/// column composes, and the bytes those extents name.
///
/// Taken from `attr_extents` rather than from the filesystem, because that list is what a reader
/// composes at open — the file-count axis §5.1 identifies as the binding one is the length of this
/// list, not the number of files a failed pass left lying around.
struct Census {
    prefix: String,
    extents: usize,
    extent_bytes: u64,
    per_column: BTreeMap<String, usize>,
}

fn census(bundle: &Path, phash: &str) -> Census {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    let opened = open_bundle(bundle).expect("open");
    let extents = &opened.partitions[phash].manifest.attr_extents;
    let mut per_column = BTreeMap::new();
    let mut extent_bytes = 0;
    for e in extents {
        *per_column.entry(e.column.clone()).or_insert(0usize) += 1;
        for rel in [&e.values, &e.presence] {
            extent_bytes += std::fs::metadata(bundle.join(&prefix).join(rel))
                .map(|m| m.len())
                .unwrap_or(0);
        }
    }
    Census {
        prefix,
        extents: extents.len(),
        extent_bytes,
        per_column,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

// =================================================================================================
// The query set: one per route the design distinguishes
// =================================================================================================

/// A predicate expressed twice — once as the operand the index is asked, once as a closure over the
/// oracle's own values. The two are written side by side deliberately: a query whose reference is
/// derived from its operand would check the plumbing and not the answer.
struct Query {
    name: &'static str,
    column: &'static str,
    operand: FilterOperand,
    oracle: Box<dyn Fn(&oracle::Oracle, usize) -> bool>,
}

fn queries(oc: &oracle::Oracle, codes: &Codebook) -> Vec<Query> {
    // The most common values in this corpus, chosen from the data rather than named: an operand
    // over a value nothing carries measures nothing.
    let arch = codes.archive_common;
    let arch_set = codes.archive_set.clone();
    let prim = codes.primary_common;
    let sec = codes.secondary_common;
    let author = codes.author_common.clone();
    let author2 = author.clone();
    let prefix = codes.author_prefix.clone();
    let prefix2 = prefix.clone();
    let needle = "ang".to_string();
    let needle2 = needle.clone();
    let (lo, hi) = codes.ts_window;
    let _ = oc;

    let mut out = vec![
        Query {
            name: "archive_eq",
            column: "archive",
            operand: FilterOperand::Equals(AttrLocalId::new(arch as u32)),
            oracle: Box::new(move |o, s| o.archive[s] == arch),
        },
        Query {
            name: "archive_in3",
            column: "archive",
            operand: FilterOperand::In(
                arch_set
                    .iter()
                    .map(|c| AttrLocalId::new(*c as u32))
                    .collect(),
            ),
            oracle: Box::new({
                let set = codes.archive_set.clone();
                move |o, s| set.contains(&o.archive[s])
            }),
        },
        Query {
            name: "primary_eq",
            column: "primary_category",
            operand: FilterOperand::Equals(AttrLocalId::new(prim as u32)),
            oracle: Box::new(move |o, s| o.primary[s] == prim),
        },
        Query {
            name: "secondary_eq",
            column: "secondary_category",
            operand: FilterOperand::Equals(AttrLocalId::new(sec as u32)),
            oracle: Box::new(move |o, s| o.secondary[s] == sec),
        },
        Query {
            name: "author_eq",
            column: "first_author",
            operand: FilterOperand::TextEquals(author.clone()),
            oracle: Box::new(move |o, s| o.author_at(s) == Some(author2.as_str())),
        },
        Query {
            name: "author_prefix",
            column: "first_author",
            operand: FilterOperand::TextPrefix(prefix.clone()),
            oracle: Box::new(move |o, s| {
                o.author_at(s).is_some_and(|a| a.starts_with(prefix2.as_str()))
            }),
        },
        Query {
            name: "author_contains",
            column: "first_author",
            operand: FilterOperand::TextContains(needle.clone()),
            oracle: Box::new(move |o, s| {
                o.author_at(s).is_some_and(|a| a.contains(needle2.as_str()))
            }),
        },
        Query {
            name: "ts_range_closed",
            column: "submitted_at",
            operand: FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Int(lo as i128),
                    inclusive: true,
                }),
                hi: Some(Endpoint {
                    value: Scalar::Int(hi as i128),
                    inclusive: false,
                }),
            },
            oracle: Box::new(move |o, s| o.submitted_at[s] >= lo && o.submitted_at[s] < hi),
        },
        Query {
            name: "ts_range_open",
            column: "submitted_at",
            operand: FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Int(hi as i128),
                    inclusive: true,
                }),
                hi: None,
            },
            oracle: Box::new(move |o, s| o.submitted_at[s] >= hi),
        },
    ];
    out.shrink_to_fit();
    out
}

/// The composed expression, kept apart from the leaf set because it is answered by `evaluate`
/// rather than `resolve` and its reference is the conjunction of two leaves'.
fn composed(codes: &Codebook) -> (FilterExpr, Box<dyn Fn(&oracle::Oracle, usize) -> bool>) {
    let arch = codes.archive_common;
    let (lo, hi) = codes.ts_window;
    (
        FilterExpr::AllOf(vec![
            FilterExpr::Leaf {
                column: "archive".into(),
                operand: FilterOperand::Equals(AttrLocalId::new(arch as u32)),
            },
            FilterExpr::Leaf {
                column: "submitted_at".into(),
                operand: FilterOperand::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Int(lo as i128),
                        inclusive: true,
                    }),
                    hi: Some(Endpoint {
                        value: Scalar::Int(hi as i128),
                        inclusive: false,
                    }),
                },
            },
        ]),
        Box::new(move |o: &oracle::Oracle, s: usize| {
            o.archive[s] == arch && o.submitted_at[s] >= lo && o.submitted_at[s] < hi
        }),
    )
}

/// The values the query set names, chosen from the corpus's own distribution.
#[derive(Clone)]
struct Codebook {
    archive_common: u8,
    archive_set: Vec<u8>,
    primary_common: u16,
    secondary_common: u16,
    author_common: String,
    author_prefix: String,
    ts_window: (i64, i64),
}

fn codebook(oc: &oracle::Oracle, base: usize) -> Codebook {
    let mut arch_hist = BTreeMap::new();
    let mut prim_hist = BTreeMap::new();
    let mut sec_hist = BTreeMap::new();
    let mut author_hist: BTreeMap<&str, usize> = BTreeMap::new();
    for s in 0..base {
        *arch_hist.entry(oc.archive[s]).or_insert(0usize) += 1;
        *prim_hist.entry(oc.primary[s]).or_insert(0usize) += 1;
        if oc.secondary[s] != 0 {
            *sec_hist.entry(oc.secondary[s]).or_insert(0usize) += 1;
        }
        if let Some(a) = oc.author_at(s) {
            *author_hist.entry(a).or_insert(0) += 1;
        }
    }
    let top = |h: &BTreeMap<u8, usize>| *h.iter().max_by_key(|(_, n)| **n).unwrap().0;
    let top16 = |h: &BTreeMap<u16, usize>| *h.iter().max_by_key(|(_, n)| **n).unwrap().0;
    let mut arch_sorted: Vec<(u8, usize)> = arch_hist.iter().map(|(k, v)| (*k, *v)).collect();
    arch_sorted.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let archive_set: Vec<u8> = arch_sorted.iter().take(3).map(|(k, _)| *k).collect();
    let author_common = author_hist
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(k, _)| k.to_string())
        .unwrap();
    let mut ts: Vec<i64> = (0..base).step_by(97).map(|s| oc.submitted_at[s]).collect();
    ts.sort_unstable();
    // A quarter of the corpus by submission date, and the open range above it — the two range
    // shapes §3 distinguishes, at a selectivity a viewer would actually ask for.
    let ts_window = (ts[ts.len() / 2], ts[ts.len() * 3 / 4]);
    Codebook {
        archive_common: top(&arch_hist),
        archive_set,
        primary_common: top16(&prim_hist),
        secondary_common: top16(&sec_hist),
        author_prefix: author_common.chars().take(2).collect(),
        author_common,
        ts_window,
    }
}

// =================================================================================================
// The walk
// =================================================================================================

/// `entity → source id`, grown as the campaign ingests. The build's half is the external-id
/// extent's decode; the flushed half is what `accept_ingest` returned, recorded row by row.
struct EntityMap {
    source_of: Vec<u32>,
}

impl EntityMap {
    fn from_build(bundle: &Path, prefix: &str, n: usize) -> EntityMap {
        use arrow::array::{Array, BinaryArray, UInt32Array};
        let opened = open_bundle(bundle).expect("bundle opens");
        let part = opened.partitions.values().next().expect("one partition");
        let mut source_of = vec![u32::MAX; n];
        for rel in &part.manifest.external_id_runs {
            let file = std::fs::File::open(bundle.join(prefix).join(rel)).expect("ext run");
            let reader = arrow::ipc::reader::FileReader::try_new(file, None).expect("ipc");
            for batch in reader {
                let batch = batch.expect("batch");
                let ext = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .expect("external ids are binary");
                let ent = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .expect("entities are u32");
                for i in 0..batch.num_rows() {
                    let source = u64::from_le_bytes(ext.value(i).try_into().unwrap()) as u32;
                    source_of[ent.value(i) as usize] = source;
                }
            }
        }
        assert!(
            source_of.iter().all(|s| *s != u32::MAX),
            "the external-id extent did not name every built entity"
        );
        EntityMap { source_of }
    }

    fn set(&mut self, entity: u32, source: u32) {
        if self.source_of.len() <= entity as usize {
            self.source_of.resize(entity as usize + 1, u32::MAX);
        }
        self.source_of[entity as usize] = source;
    }

    fn source(&self, entity: u32) -> Option<u32> {
        self.source_of
            .get(entity as usize)
            .copied()
            .filter(|s| *s != u32::MAX)
    }

    fn bound(&self) -> u32 {
        self.source_of.len() as u32
    }
}

/// The expected answer: entities in `candidate` whose source value satisfies `pred`.
fn expected(
    map: &EntityMap,
    oc: &oracle::Oracle,
    candidate: &Bitmap,
    pred: &dyn Fn(&oracle::Oracle, usize) -> bool,
) -> Bitmap {
    let mut out = Bitmap::new();
    for e in candidate.iter() {
        if let Some(s) = map.source(e) {
            if pred(oc, s as usize) {
                out.add(e);
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn run_stage(
    stage: &str,
    scale: &str,
    columns: &FilterColumns,
    session_candidate: &Bitmap,
    full_candidate: &Bitmap,
    map: &EntityMap,
    oc: &oracle::Oracle,
    qs: &[Query],
    comp: &(FilterExpr, Box<dyn Fn(&oracle::Oracle, usize) -> bool>),
    repeats: usize,
    csv: &mut Csv,
    failures: &mut Vec<String>,
) -> BTreeMap<String, Bitmap> {
    let mut artefact = BTreeMap::new();
    for q in qs {
        // Timed against the session's candidate — the shape a served request has.
        let mut times = Vec::new();
        let mut got = Bitmap::new();
        for _ in 0..repeats {
            let t = Instant::now();
            got = columns
                .resolve(q.column, &q.operand, session_candidate)
                .unwrap_or_else(|e| panic!("{stage}/{}: {e:?}", q.name));
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let want = expected(map, oc, session_candidate, &q.oracle);
        if got != want {
            failures.push(format!(
                "{scale}/{stage}/{}: masked answer disagrees with the corpus — got {} entities, \
                 expected {} ({} missing, {} spurious)",
                q.name,
                got.cardinality(),
                want.cardinality(),
                want.andnot(&got).cardinality(),
                got.andnot(&want).cardinality()
            ));
        }
        csv.row(format!(
            "{scale},{stage},{},masked,{},{:.3}",
            q.name,
            got.cardinality(),
            median(times)
        ));

        // And against the full entity range: the artefact's own answer, which is what a coalesce
        // must preserve and a fold must change in exactly one way.
        let t = Instant::now();
        let whole = columns
            .resolve(q.column, &q.operand, full_candidate)
            .expect("resolve");
        let ms = t.elapsed().as_secs_f64() * 1e3;
        csv.row(format!(
            "{scale},{stage},{},artefact,{},{:.3}",
            q.name,
            whole.cardinality(),
            ms
        ));
        artefact.insert(q.name.to_string(), whole);
    }

    let mut times = Vec::new();
    let mut got = Bitmap::new();
    for _ in 0..repeats {
        let t = Instant::now();
        got = columns.evaluate(&comp.0, session_candidate).expect("evaluate");
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let want = expected(map, oc, session_candidate, &comp.1);
    if got != want {
        failures.push(format!(
            "{scale}/{stage}/composed: got {} expected {}",
            got.cardinality(),
            want.cardinality()
        ));
    }
    csv.row(format!(
        "{scale},{stage},composed,masked,{},{:.3}",
        got.cardinality(),
        median(times)
    ));
    artefact.insert(
        "composed".into(),
        columns.evaluate(&comp.0, full_candidate).expect("evaluate"),
    );
    artefact
}

fn wait_until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    let a = args();
    std::fs::create_dir_all(&a.out).expect("out dir");
    std::fs::create_dir_all(&a.work).expect("work dir");

    let archive_codes = oracle::read_vocabulary(&a.data.join("archive.parquet"));
    let primary_codes = oracle::read_vocabulary(&a.data.join("primary_category.parquet"));
    let secondary_codes = oracle::read_vocabulary(&a.data.join("secondary_category.parquet"));

    let ingest_total = a.flushes * a.batch;
    let total = a.limit as usize + ingest_total;
    log(format!(
        "scale={} limit={} ingest={} ({} flushes x {})",
        a.scale, a.limit, ingest_total, a.flushes, a.batch
    ));

    log("loading the oracle from the points file");
    let t = Instant::now();
    let oc = oracle::load(
        &a.data.join("points.parquet"),
        total,
        &archive_codes,
        &primary_codes,
        &secondary_codes,
    );
    log(format!("oracle loaded in {:.1}s", t.elapsed().as_secs_f64()));

    let codes = codebook(&oc, a.limit as usize);
    log(format!(
        "codebook: archive={} primary={} secondary={} author='{}' prefix='{}' ts=[{},{})",
        codes.archive_common,
        codes.primary_common,
        codes.secondary_common,
        codes.author_common,
        codes.author_prefix,
        codes.ts_window.0,
        codes.ts_window.1
    ));

    let mut failures: Vec<String> = Vec::new();


    // ------------------------------------------------------------------ the fresh build
    let bundle = a.work.join(format!("bundle-{}", a.scale));
    if bundle.exists() {
        std::fs::remove_dir_all(&bundle).expect("clear the bundle");
    }
    let mut values = std::collections::HashMap::new();
    values.insert("archive".to_string(), a.data.join("archive.parquet"));
    values.insert(
        "primary_category".to_string(),
        a.data.join("primary_category.parquet"),
    );
    values.insert(
        "secondary_category".to_string(),
        a.data.join("secondary_category.parquet"),
    );
    let schema = tessera_build::schema::Schema::parse(&a.data.join("schema.toml"), &values)
        .expect("the schema parses");

    log("building");
    let t = Instant::now();
    tessera_build::build(&tessera_build::BuildArgs {
        points: a.data.join("points.parquet"),
        pairs: PathBuf::from("/home/joe/code/tessera/data/scaled/pairs/categories-subclass.pairs.parquet"),
        out: bundle.clone(),
        extent: EXTENT,
        slice_id: "s0".into(),
        limit: Some(a.limit),
        identity_key: IdentityKey::from_hex(KEY_HEX).unwrap(),
        identity_key_hex: KEY_HEX.into(),
        idset: 1,
        shard_id: 0,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the build succeeds");
    let build_ms = t.elapsed().as_secs_f64() * 1e3;
    log(format!("built in {build_ms:.0} ms"));

    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    let mut map = EntityMap::from_build(&bundle, &prefix, a.limit as usize);

    let opened = open_bundle(&bundle).expect("open");
    let phash = opened.partitions.keys().next().unwrap().clone();
    let attrs_dir = bundle
        .join(&prefix)
        .join("partitions")
        .join(&phash)
        .join("attrs");
    if a.residency {
        // One column at a time, in declaration order, with the process's resident set read between
        // opens: the delta is that column's own residency, which a single `FilterColumns::open`
        // cannot separate.
        let mut csv = Csv::new(
            &a.out.join(format!("residency-{}.csv", a.scale)),
            "scale,column,file_bytes,postings_bytes,rss_values_delta,rss_postings_delta",
        );
        for column in COLUMNS {
            let dir = attrs_dir.join(column);
            let before = rss_bytes();
            let values = std::fs::metadata(dir.join("values.arrow"))
                .map(|m| m.len())
                .unwrap_or(0);
            let postings_bytes = std::fs::metadata(dir.join("postings.arrow"))
                .map(|m| m.len())
                .unwrap_or(0);
            let opened = tessera_filter::ValueColumn::open_dir(&dir, true).expect("column opens");
            let after_values = rss_bytes();
            let postings = (postings_bytes > 0)
                .then(|| tessera_filter::ColumnPostings::open_keyed(&dir.join("postings.arrow")));
            let after = rss_bytes();
            csv.row(format!(
                "{},{column},{values},{postings_bytes},{},{}",
                a.scale,
                after_values.saturating_sub(before),
                after.saturating_sub(after_values)
            ));
            log(format!(
                "{column}: values {values} bytes -> +{} rss; postings {postings_bytes} bytes -> +{} rss",
                after_values.saturating_sub(before),
                after.saturating_sub(after_values)
            ));
            std::mem::forget(opened);
            std::mem::forget(postings);
        }
        return;
    }

    let mut lifecycle = Csv::new(
        &a.out.join(format!("query-{}.csv", a.scale)),
        "scale,stage,query,candidate,cardinality,ms",
    );
    let mut write_csv = Csv::new(
        &a.out.join(format!("write-{}.csv", a.scale)),
        "scale,event,index,wall_ms,attrs_bytes,attrs_files,extents,extent_bytes,rss_bytes",
    );
    let mut stage_csv = Csv::new(
        &a.out.join(format!("stage-{}.csv", a.scale)),
        "scale,stage,open_ms,attrs_bytes,attrs_files,extent_files,rss_after_open_bytes,\
         rss_after_scan_bytes,layers_archive,layers_author",
    );
    let (ab, af) = attrs_footprint(&bundle.join(&prefix));
    let c = census(&bundle, &phash);
    write_csv.row(format!(
        "{},build,0,{:.0},{ab},{af},{},{},{}",
        a.scale,
        build_ms,
        c.extents,
        c.extent_bytes,
        rss_bytes()
    ));

    let qs = queries(&oc, &codes);
    let comp = composed(&codes);

    // ------------------------------------------------------------------ stage 0: fresh build
    let engine = Engine::open(&bundle, &a.work.join("cache"), &a.work.join("wal"), Passthrough::new(), config())
        .expect("engine opens");
    engine.set_coalesce_for_test(false);
    let credential = format!(
        "{{\"terms\": [{}]}}",
        a.terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let session = engine
        .authorise(credential.as_bytes())
        .expect("the credential resolves");
    let generation = engine.generation();
    let session_cand = compose_candidate(
        &session.fragment,
        &session.satisfied,
        &generation.overlay,
        &generation.buffer,
    );
    log(format!(
        "principal coverage at build: {} of {} entities ({:.1}%)",
        session_cand.cardinality(),
        a.limit,
        100.0 * session_cand.cardinality() as f64 / a.limit as f64
    ));
    assert!(
        session_cand.cardinality() > 0,
        "the principal sees nothing — pick different --terms"
    );
    drop(generation);
    drop(session);
    drop(engine);

    let mut full_cand = Bitmap::from_range(0..a.limit as u32);
    let base_stage = open_and_measure(
        "s0-build",
        &a.scale,
        &bundle,
        &prefix,
        &phash,
        &attrs_dir,
        &session_cand,
        &mut stage_csv,
    );
    let s0 = run_stage(
        "s0-build",
        &a.scale,
        &base_stage,
        &session_cand,
        &full_cand,
        &map,
        &oc,
        &qs,
        &comp,
        a.repeats,
        &mut lifecycle,
        &mut failures,
    );
    drop(base_stage);

    // ------------------------------------------------------------------ the flushes
    let engine = Engine::open(
        &bundle,
        &a.work.join("cache"),
        &a.work.join("wal"),
        Passthrough::new(),
        config(),
    )
    .expect("engine reopens");
    let mut engine = engine;
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_coalesce_for_test(false);

    let mut ingested: Vec<u32> = Vec::with_capacity(ingest_total);
    let mut next_source = a.limit as u32;
    for f in 0..a.flushes {
        let before = engine.write_executor_stats().flushes;
        let mut rows = Vec::with_capacity(a.batch);
        for _ in 0..a.batch {
            let s = next_source as usize;
            next_source += 1;
            // The term this row carries is one of the principal's, so the flushed half of the
            // corpus is inside the candidate and the extents are actually scanned. Geometry is not
            // the subject here: coordinates are a deterministic spread inside the extent.
            let term = a.terms[s % a.terms.len()].clone();
            rows.push(UnallocatedRow {
                external_id: Some(format!("ing-{s}").into_bytes()),
                slice: "s0".into(),
                descriptors: vec![term.clone().into_bytes()],
                x: (s % 65536) as f32,
                y: ((s / 65536) % 65536) as f32,
                scalars: vec![
                    WalScalar::Utf8(oc.archive_key(s).to_string()),
                    WalScalar::Utf8(oc.primary_key(s).to_string()),
                    if oc.secondary[s] == 0 {
                        WalScalar::U16(0)
                    } else {
                        WalScalar::Utf8(oc.secondary_key(s).to_string())
                    },
                    WalScalar::Utf8(oc.author_at(s).unwrap_or_default().to_string()),
                    WalScalar::I64(oc.submitted_at[s]),
                ],
                terms: engine.resolve_terms(&[term.into_bytes()]),
            });
        }
        let sources: Vec<u32> = ((next_source - a.batch as u32)..next_source).collect();
        let t = Instant::now();
        let allocated = engine
            .accept_ingest(rows, format!("flush-{f}"), [0u8; 32])
            .expect("ingest accepted");
        for (row, source) in allocated.iter().zip(&sources) {
            map.set(row.raw() as u32, *source);
            ingested.push(row.raw() as u32);
        }
        engine.request_flush();
        wait_until("the flush to publish", 600, || {
            engine.write_executor_stats().flushes > before
        });
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let (ab, af) = attrs_footprint(&bundle.join(&prefix));
        let c = census(&bundle, &phash);
        write_csv.row(format!(
            "{},flush,{f},{ms:.1},{ab},{af},{},{},{}",
            a.scale,
            c.extents,
            c.extent_bytes,
            rss_bytes()
        ));
        if f % 8 == 0 {
            log(format!(
                "flush {f}: {ms:.0} ms, {} extents / {} extent bytes, {af} attr files",
                c.extents, c.extent_bytes
            ));
        }
    }
    let high_water = map.bound();
    full_cand = Bitmap::from_range(0..high_water);

    let generation = engine.generation();
    let session = engine.authorise(credential.as_bytes()).expect("resolves");
    let session_cand_flushed = compose_candidate(
        &session.fragment,
        &session.satisfied,
        &generation.overlay,
        &generation.buffer,
    );
    log(format!(
        "after {} flushes: {} entities, principal sees {}",
        a.flushes,
        high_water,
        session_cand_flushed.cardinality()
    ));

    let s1 = run_stage(
        "s1-flushed",
        &a.scale,
        &generation.filter_columns,
        &session_cand_flushed,
        &full_cand,
        &map,
        &oc,
        &qs,
        &comp,
        a.repeats,
        &mut lifecycle,
        &mut failures,
    );

    // The build's own entities must answer exactly as they did before any flush.
    let below = Bitmap::from_range(0..a.limit as u32);
    for (name, before) in &s0 {
        let after = restrict(&s1[name], &below);
        if after != restrict(before, &below) {
            failures.push(format!(
                "{}/s1-flushed/{name}: a flush changed what the build's own entities answer",
                a.scale
            ));
        }
    }
    stage_from_manifest("s1-flushed", &a.scale, &bundle, &phash, &attrs_dir, &session_cand_flushed, &mut stage_csv);
    check_categories(
        "s1-flushed",
        a.limit as u32,
        &engine,
        &session,
        &oc,
        &map,
        &session_cand_flushed,
        &mut failures,
    );
    drop(generation);

    // ------------------------------------------------------------------ deletions, then coalesce
    // Deleted before the coalesce on purpose: §5.2's claim is that a coalesce retires *nothing*,
    // and a deleted-but-unfolded entity's value is the case that distinguishes a content-preserving
    // re-encode from one that quietly executed a removal.
    let mut deleted: Vec<u32> = Vec::new();
    let step = (high_water as usize / a.deletes.max(1)).max(1);
    let t = Instant::now();
    // Submitted rather than accepted one at a time: a caller that waits for each receipt leaves the
    // executor one entry to gather and pays a manifest write and an fsync per delete, which is the
    // deny lane's own documented anti-pattern (`Engine::submit_change`). Measured at 2.4M before
    // this change: ~20,000 side-manifests and ~20 minutes for 20,000 deletes.
    let mut pending = Vec::with_capacity(a.deletes);
    for e in (0..high_water as usize).step_by(step).take(a.deletes) {
        pending.push(
            engine
                .submit_change(EntityId::new(e as u64), ChangeOp::Delete)
                .expect("a delete is accepted"),
        );
        deleted.push(e as u32);
    }
    for p in pending {
        p.wait().expect("the delete commits");
    }
    log(format!(
        "deleted {} entities in {:.0} ms",
        deleted.len(),
        t.elapsed().as_secs_f64() * 1e3
    ));

    engine.set_coalesce_for_test(true);
    let mut passes = 0;
    loop {
        let before = engine.write_executor_stats().coalesces;
        let t = Instant::now();
        engine.request_flush();
        // A tick either selects a coalesce or does not; give it a bounded window to publish.
        let mut fired = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if engine.write_executor_stats().coalesces > before {
                fired = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !fired {
            break;
        }
        passes += 1;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let (ab, af) = attrs_footprint(&bundle.join(&prefix));
        let c = census(&bundle, &phash);
        write_csv.row(format!(
            "{},coalesce,{passes},{ms:.1},{ab},{af},{},{},{}",
            a.scale,
            c.extents,
            c.extent_bytes,
            rss_bytes()
        ));
        log(format!(
            "coalesce pass {passes}: {ms:.0} ms, {} extents ({:?}), {af} attr files / {ab} bytes",
            c.extents, c.per_column
        ));
        if passes > 64 {
            break;
        }
    }
    log(format!("{passes} coalesce passes"));

    let generation = engine.generation();
    let s2 = run_stage(
        "s2-coalesced",
        &a.scale,
        &generation.filter_columns,
        &session_cand_flushed,
        &full_cand,
        &map,
        &oc,
        &qs,
        &comp,
        a.repeats,
        &mut lifecycle,
        &mut failures,
    );
    for (name, before) in &s1 {
        if s2[name] != *before {
            failures.push(format!(
                "{}/s2-coalesced/{name}: a coalesce changed the artefact's answer ({} -> {})",
                a.scale,
                before.cardinality(),
                s2[name].cardinality()
            ));
        }
    }
    stage_from_manifest("s2-coalesced", &a.scale, &bundle, &phash, &attrs_dir, &session_cand_flushed, &mut stage_csv);
    drop(generation);

    // ------------------------------------------------------------------ the fold
    let folds_before = engine.write_executor_stats().folds;
    let rss_before = rss_bytes();
    // Peak RSS across the fold, sampled: `PassCost` records the resident set at the *end* of each
    // pass, which is a staircase maximum rather than a peak (`write.rs` says so at the field), and
    // a pass whose transient is freed before it returns would leave no trace in it.
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(rss_before));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let peak = std::sync::Arc::clone(&peak);
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                peak.fetch_max(rss_bytes(), std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let t = Instant::now();
    engine.request_fold();
    wait_until("the fold to publish", 7200, || {
        let s = engine.write_executor_stats();
        assert_eq!(
            s.fold_failures,
            engine.write_executor_stats().fold_failures,
            "the fold was discarded"
        );
        s.folds > folds_before
    });
    let fold_ms = t.elapsed().as_secs_f64() * 1e3;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sampler.join().ok();
    let fold_peak_rss = peak.load(std::sync::atomic::Ordering::Relaxed);
    let mut fold_csv = Csv::new(
        &a.out.join(format!("fold-{}.csv", a.scale)),
        "scale,pass,elapsed_ms,rss_bytes,anon_bytes",
    );
    for pc in engine.last_fold_passes() {
        fold_csv.row(format!(
            "{},{},{:.1},{},{}",
            a.scale,
            pc.pass,
            pc.elapsed.as_secs_f64() * 1e3,
            pc.rss,
            pc.anon
        ));
    }
    let generation = engine.generation();
    let new_prefix = generation.prefix.clone();
    let new_attrs = bundle
        .join(&new_prefix)
        .join("partitions")
        .join(&phash)
        .join("attrs");
    let (ab, af) = attrs_footprint(&bundle.join(&new_prefix));
    let c = census(&bundle, &phash);
    write_csv.row(format!(
        "{},fold,0,{fold_ms:.0},{ab},{af},{},{},{}",
        a.scale,
        c.extents,
        c.extent_bytes,
        rss_bytes()
    ));
    log(format!(
        "folded in {fold_ms:.0} ms, prefix {new_prefix}, attrs {af} files / {ab} bytes, \
         rss {} -> {} (peak {fold_peak_rss})",
        rss_before,
        rss_bytes()
    ));

    let session = engine.authorise(credential.as_bytes()).expect("resolves");
    let session_cand_folded = compose_candidate(
        &session.fragment,
        &session.satisfied,
        &generation.overlay,
        &generation.buffer,
    );
    let s3 = run_stage(
        "s3-folded",
        &a.scale,
        &generation.filter_columns,
        &session_cand_folded,
        &full_cand,
        &map,
        &oc,
        &qs,
        &comp,
        a.repeats,
        &mut lifecycle,
        &mut failures,
    );
    let deleted_set: Bitmap = deleted.iter().copied().collect();
    for (name, before) in &s2 {
        let want = before.andnot(&deleted_set);
        if s3[name] != want {
            failures.push(format!(
                "{}/s3-folded/{name}: the fold's answer is not the coalesced one minus the deleted \
                 entities ({} vs {})",
                a.scale,
                s3[name].cardinality(),
                want.cardinality()
            ));
        }
    }
    stage_from_manifest("s3-folded", &a.scale, &bundle, &phash, &new_attrs, &session_cand_folded, &mut stage_csv);

    // ------------------------------------------------------------------ byte identity
    check_byte_identity(
        &a,
        &new_attrs,
        &map,
        &oc,
        &deleted_set,
        high_water,
        &mut failures,
    );

    // ------------------------------------------------------------------ /v1/categories
    check_categories(
        "s3-folded",
        a.limit as u32,
        &engine,
        &session,
        &oc,
        &map,
        &session_cand_folded,
        &mut failures,
    );

    // ------------------------------------------------------------------ verdict
    let mut verdict = Csv::new(&a.out.join(format!("verdict-{}.csv", a.scale)), "scale,finding");
    if failures.is_empty() {
        log("ALL CHECKS PASSED");
        verdict.row(format!("{},all checks passed", a.scale));
    } else {
        for f in &failures {
            log(format!("FAILURE: {f}"));
            verdict.row(format!("{},\"{}\"", a.scale, f.replace('"', "'")));
        }
    }
    drop(generation);
    drop(engine);
    if !a.keep {
        let _ = std::fs::remove_dir_all(&bundle);
        let _ = std::fs::remove_dir_all(a.work.join("cache"));
        let _ = std::fs::remove_dir_all(a.work.join("wal"));
    }
}

fn restrict(b: &Bitmap, mask: &Bitmap) -> Bitmap {
    b.and(mask)
}

/// Open the declared columns from the bundle as a request would, timing the open and recording what
/// it resides.
#[allow(clippy::too_many_arguments)]
fn open_and_measure(
    stage: &str,
    scale: &str,
    bundle: &Path,
    prefix: &str,
    phash: &str,
    attrs_dir: &Path,
    candidate: &Bitmap,
    csv: &mut Csv,
) -> FilterColumns {
    let opened = open_bundle(bundle).expect("open");
    let extents = &opened.partitions[phash].manifest.attr_extents;
    let rss_before = rss_bytes();
    let t = Instant::now();
    let columns = FilterColumns::open(
        &bundle.join(prefix),
        phash,
        &opened.manifest.declared_scalars,
        &opened.manifest.vocabularies,
        extents,
        true,
    )
    .expect("the declared columns open");
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    let rss_open = rss_bytes();
    let _ = columns.resolve(
        "archive",
        &FilterOperand::Equals(AttrLocalId::new(1)),
        candidate,
    );
    let _ = columns.resolve(
        "first_author",
        &FilterOperand::TextPrefix("A".into()),
        candidate,
    );
    let rss_scan = rss_bytes();
    let _ = attrs_dir;
    let (ab, af) = attrs_footprint(&bundle.join(prefix));
    let per_column = |name: &str| extents.iter().filter(|e| e.column == name).count() + 1;
    csv.row(format!(
        "{scale},{stage},{open_ms:.3},{ab},{af},{},{},{},{},{}",
        extents.len(),
        rss_open.saturating_sub(rss_before),
        rss_scan.saturating_sub(rss_before),
        per_column("archive"),
        per_column("first_author"),
    ));
    columns
}

/// The same measurement, taken against whatever prefix the bundle's `CURRENT` now names.
fn stage_from_manifest(
    stage: &str,
    scale: &str,
    bundle: &Path,
    phash: &str,
    attrs_dir: &Path,
    candidate: &Bitmap,
    csv: &mut Csv,
) {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    let columns = open_and_measure(stage, scale, bundle, &prefix, phash, attrs_dir, candidate, csv);
    drop(columns);
}

/// **A folded column is the bytes a single build over the same live entities would have written**
/// (filter-index §6.2). Checked here against the *source parquet*, so it is simultaneously the
/// content oracle: the expected column is assembled from the corpus's own values and handed to the
/// one-shot writer, and the comparison is byte for byte with what the fold left on disc.
fn check_byte_identity(
    a: &Args,
    attrs: &Path,
    map: &EntityMap,
    oc: &oracle::Oracle,
    deleted: &Bitmap,
    bound: u32,
    failures: &mut Vec<String>,
) {
    use arrow::buffer::ScalarBuffer;
    use tessera_filter::{write_value_column, Codes};

    let tmp = tempfile::tempdir().expect("tempdir");
    for column in COLUMNS {
        let mut present = Bitmap::new();
        let mut u8s: Vec<u8> = Vec::new();
        let mut u16s: Vec<u16> = Vec::new();
        let mut i64s: Vec<i64> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for e in 0..bound {
            if deleted.contains(e) {
                continue;
            }
            let Some(s) = map.source(e) else { continue };
            let s = s as usize;
            match column {
                "archive" => {
                    if oc.archive[s] == 0 {
                        continue;
                    }
                    present.add(e);
                    u8s.push(oc.archive[s]);
                }
                "primary_category" => {
                    if oc.primary[s] == 0 {
                        continue;
                    }
                    present.add(e);
                    u16s.push(oc.primary[s]);
                }
                "secondary_category" => {
                    if oc.secondary[s] == 0 {
                        continue;
                    }
                    present.add(e);
                    u16s.push(oc.secondary[s]);
                }
                "first_author" => match oc.author_at(s) {
                    Some(v) => {
                        present.add(e);
                        texts.push(v.to_string());
                    }
                    None => continue,
                },
                "submitted_at" => {
                    present.add(e);
                    i64s.push(oc.submitted_at[s]);
                }
                _ => unreachable!(),
            }
        }
        let codes = match column {
            "archive" => Codes::U8(ScalarBuffer::from(u8s)),
            "primary_category" | "secondary_category" => Codes::U16(ScalarBuffer::from(u16s)),
            "first_author" => Codes::text(texts.clone()),
            "submitted_at" => Codes::I64(ScalarBuffer::from(i64s)),
            _ => unreachable!(),
        };
        // The reader's dense-from-zero convention: the presence file is omitted exactly when every
        // entity below the bound is present (filter-index §2.1, §6.2).
        let universal = present.cardinality() == bound as u64;
        let want_values = tmp.path().join(format!("{column}-values.arrow"));
        let want_presence = tmp.path().join(format!("{column}-presence.roaring"));
        write_value_column(
            &want_values,
            &want_presence,
            &codes,
            (!universal).then_some(&present),
        )
        .expect("the one-shot writer");

        let got_values = attrs.join(column).join("values.arrow");
        let got_presence = attrs.join(column).join("presence.roaring");
        let want = std::fs::read(&want_values).expect("expected column");
        let got = std::fs::read(&got_values).expect("folded column");
        if want != got {
            failures.push(format!(
                "{}/byte-identity/{column}: the folded value column is not what a build over the \
                 same live entities writes ({} vs {} bytes)",
                a.scale,
                got.len(),
                want.len()
            ));
        }
        if universal {
            if got_presence.exists() {
                failures.push(format!(
                    "{}/byte-identity/{column}: a universally-present column wrote a presence file",
                    a.scale
                ));
            }
        } else {
            let want_p = std::fs::read(&want_presence).expect("expected presence");
            match std::fs::read(&got_presence) {
                Ok(got_p) if got_p == want_p => {}
                Ok(got_p) => failures.push(format!(
                    "{}/byte-identity/{column}: the folded presence bitmap differs ({} vs {} bytes)",
                    a.scale,
                    got_p.len(),
                    want_p.len()
                )),
                Err(e) => failures.push(format!(
                    "{}/byte-identity/{column}: no folded presence bitmap ({e})",
                    a.scale
                )),
            }
        }

        // **Blanking removes the value's bytes rather than overwriting them** — the retention
        // property the fold exists for (§6.2), asserted against the file itself. Only a value no
        // *surviving* entity carries can be looked for: the bytes of a shared value are still in
        // the column on the survivors' account, which is correct and would make a naive search
        // report a false failure.
        if column == "first_author" {
            let live: std::collections::HashSet<&str> = texts.iter().map(|s| s.as_str()).collect();
            // A value that is a *substring* of a survivor's value is still legitimately in the
            // column's bytes — "Burnel" inside "Burnell" — so it proves nothing and is skipped.
            let live_substrings: std::collections::HashSet<&str> = live.iter().copied().collect();
            let mut checked = 0;
            for e in deleted.iter() {
                if checked >= 32 {
                    break;
                }
                let Some(s) = map.source(e) else { continue };
                let Some(v) = oc.author_at(s as usize) else {
                    continue;
                };
                if v.len() < 6
                    || live.contains(v)
                    || live_substrings.iter().any(|l| l.contains(v))
                {
                    continue;
                }
                checked += 1;
                if bytes_contain(&got, v) {
                    failures.push(format!(
                        "{}/blanking/{column}: a deleted entity's value '{v}' is still in the \
                         folded column's bytes",
                        a.scale
                    ));
                    break;
                }
            }
            if checked == 0 {
                failures.push(format!(
                    "{}/blanking/{column}: no deleted entity carried a value unique to it, so the \
                     bytes-are-gone check proved nothing — raise --deletes",
                    a.scale
                ));
            }
        }
    }
}

fn bytes_contain(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// **`/v1/categories` under `per_viewer` offers exactly the values the principal can see**
/// (per-point-attributes §3.3). The reference is the corpus's: the set of codes carried by an
/// entity inside the composed candidate.
fn check_categories(
    stage: &str,
    limit: u32,
    engine: &Engine,
    session: &tessera_engine::Session,
    oc: &oracle::Oracle,
    map: &EntityMap,
    candidate: &Bitmap,
    failures: &mut Vec<String>,
) {
    let mut want: std::collections::BTreeSet<u32> = Default::default();
    let mut below: std::collections::BTreeSet<u32> = Default::default();
    for e in candidate.iter() {
        if let Some(s) = map.source(e) {
            let code = oc.primary[s as usize];
            if code != 0 {
                want.insert(code as u32);
                if e < limit {
                    below.insert(code as u32);
                }
            }
        }
    }
    // How much of the claim this run actually exercises: a value the build's own entities do not
    // carry is the case §2.3's extent sweep exists for, and a run whose corpus has none of them has
    // not tested it. Reported rather than assumed either way.
    let post_build_only = want.difference(&below).count();
    let mut got: std::collections::BTreeSet<u32> = Default::default();
    let mut after: Option<String> = None;
    loop {
        let page = engine
            .categories(
                session,
                "primary_category",
                CategoryQuery::Page {
                    after: after.as_deref(),
                    limit: 500,
                },
            )
            .expect("categories answers")
            .expect("primary_category is a category column");
        for v in &page.values {
            got.insert(v.code);
        }
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    log(format!(
        "/v1/categories at {stage}: offered {} values, {} of them carried only by post-build \
         entities",
        got.len(),
        post_build_only
    ));
    if got != want {
        failures.push(format!(
            "/v1/categories per_viewer at {stage}: offered {} values, the principal can see {} \
             ({} over-offered, {} missing)",
            got.len(),
            want.len(),
            got.difference(&want).count(),
            want.difference(&got).count()
        ));
    }
}
