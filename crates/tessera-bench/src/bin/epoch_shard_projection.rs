//! What one epoch shard's row projection costs, and what N of them per token cost.
//!
//! Splitting a corpus into N epoch shards inside one process gives each session N row projections,
//! one per shard, instead of one. The saving depends on `Permutation::project` being linear in the
//! permutation's size, so that a shard at 2³⁰ rows costs about 1/N of the corpus figure. The cost
//! depends on how much more N small projections per token take than one large one, in time and in
//! memory. Three measurements, synthetic and in memory, with no bundle:
//!
//! - `linearity`: `Permutation::project` over scattered permutations at 10⁶ to 4×10⁸ rows and masks
//!   at 25 %, 10 % and 1 % of entity space. A line fitted over the small sizes is checked against
//!   the recorded 1 277 ms at 10⁹ over a 25 % grant (`probes/2026-08-14-project-decomposition/`).
//!   The 25 % arm matches the recorded figure's condition; 10 % and 1 % are the design's. A
//!   `contiguous` mask, one run of entities, shows what a signature-sorted fragment's contiguity
//!   saves.
//! - `shards`: one mask over 10⁸ rows projected through one permutation, and the same mask split
//!   across eight permutations of 1.25×10⁷. Reports the time and the heap the results occupy.
//! - `tokens`: 10⁴ tokens' leaf projections over a 10⁶-row universe at 1 %, held all at once, in
//!   the one-permutation and eight-shard shapes. The resident set is read from `/proc/self/status`
//!   before and after, so the allocator's overhead on many small bitmaps is measured.
//!
//! Every figure is single-threaded. The harness spawns no threads and croaring runs on the caller's.
//! One JSON line per configuration goes to stdout; the readable form goes to stderr.
//!
//! How a shard is emulated: the public `Permutation` has no entity offset, so a shard's permutation
//! here covers local entity ids `[0, shard_rows)`. The session's global mask is restricted to the
//! shard's entity range and shifted down (`and` with a range, then `add_offset`) before projecting.
//! A shard-aware permutation would seek into the mask as `SegmentExtent::project` does and pay none
//! of that, so the split is timed separately and reported beside the projection.
//!
//! `--scratch` projects through `project_with` with one reused `ProjectScratch` instead of
//! `project`, which allocates and zeroes a 512 KB stamp and one vector per bucket on every call.
//! The session path uses `project`; the artifact pass uses `project_with`.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin epoch_shard_projection -- \
//!     --part linearity --rows 100000000 --mask scattered,contiguous
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use croaring::{Bitmap, Portable};
use serde_json::{json, Value};

use tessera_store::permutation::ProjectScratch;
use tessera_store::write::PermutationWriter;
use tessera_store::Permutation;
use tessera_types::EntityId;

#[derive(Parser)]
#[command(about = "Row projection per epoch shard: linearity in rows, and N leaves per token")]
struct Args {
    /// `linearity`, `shards`, `tokens`, or `all`.
    #[arg(long, default_value = "all")]
    part: String,
    /// Permutation sizes for `linearity`, in rows. One size per process is the recommended shape;
    /// two processes at one size differed by half on a shared box.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1000000,10000000,100000000,400000000"
    )]
    rows: Vec<u64>,
    /// Mask coverages for `linearity` and `shards`, as fractions of entity space.
    #[arg(long, value_delimiter = ',', default_value = "0.25,0.10,0.01")]
    coverage: Vec<f64>,
    /// Mask shapes for `linearity`: `scattered` (each entity in with probability `coverage`,
    /// independently) and/or `contiguous` (one run of `coverage × rows` entities).
    #[arg(long, value_delimiter = ',', default_value = "scattered")]
    mask: Vec<String>,
    /// Timed repetitions per configuration after one untimed warm-up; the median is reported.
    #[arg(long, default_value_t = 5)]
    reps: usize,
    /// Shard count for `shards` and `tokens`.
    #[arg(long, default_value_t = 8)]
    shards: u64,
    /// Universe for `shards`, in rows, split evenly across `--shards`.
    #[arg(long, default_value_t = 100_000_000)]
    shard_rows: u64,
    /// Tokens held at once in `tokens`.
    #[arg(long, default_value_t = 10_000)]
    tokens: usize,
    /// Universe for `tokens`, in rows, split evenly across `--shards` in the sharded shape.
    #[arg(long, default_value_t = 1_000_000)]
    token_rows: u64,
    /// Mask coverage for `tokens`.
    #[arg(long, default_value_t = 0.01)]
    token_coverage: f64,
    /// Which shapes `tokens` measures: `one`, `sharded`, or `both`. One shape per process keeps
    /// the second measurement clear of the first's freed heap.
    #[arg(long, default_value = "both")]
    token_shape: String,
    /// In `shards` and `tokens`, project through `project_with` with one reused scratch instead
    /// of `project`.
    #[arg(long)]
    scratch: bool,
    /// Where the permutation files go; the process's temp dir by default. Files are deleted as
    /// each step finishes.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Refuse to start with less than this available, in GB. The box is shared.
    #[arg(long, default_value_t = 16.0)]
    min_available_gb: f64,
}

// ---------------------------------------------------------------------------------------------
// The machine, read from /proc.
// ---------------------------------------------------------------------------------------------

fn kb_field(text: &str, field: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn meminfo_kb(field: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/meminfo").expect("this probe requires /proc");
    kb_field(&text, field).unwrap_or_else(|| panic!("/proc/meminfo has no {field}"))
}

fn status_kb(field: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/self/status").expect("this probe requires /proc");
    kb_field(&text, field).unwrap_or_else(|| panic!("/proc/self/status has no {field}"))
}

/// Bytes the allocator currently has out on loan, chunk overhead included: the main arena's
/// in-use bytes plus the chunks it served by `mmap`. The allocator's own view of what the held
/// bitmaps cost; the resident set is the kernel's.
fn malloc_in_use_bytes() -> u64 {
    // SAFETY: reads the allocator's counters; no pointer is passed or returned.
    let info = unsafe { libc::mallinfo2() };
    info.uordblks as u64 + info.hblkhd as u64
}

fn malloc_trim() {
    // SAFETY: asks the allocator to return free pages to the kernel; no pointer is involved.
    unsafe { libc::malloc_trim(0) };
}

fn host() -> Value {
    let kernel = std::fs::read_to_string("/proc/version").unwrap_or_default();
    json!({
        "part": "host",
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "mem_total_gb": meminfo_kb("MemTotal") as f64 / (1024.0 * 1024.0),
        "mem_available_gb": meminfo_kb("MemAvailable") as f64 / (1024.0 * 1024.0),
        "kernel": kernel.split_whitespace().nth(2).unwrap_or(""),
        "threads": 1,
    })
}

// ---------------------------------------------------------------------------------------------
// Fixture: a scattered bijection on [0, n), without materialising a shuffle.
// ---------------------------------------------------------------------------------------------

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A balanced Feistel permutation on `[0, 4^half_bits)`, cycle-walked down to `[0, n)`.
///
/// The construction `crates/tessera-store/examples/project_decomposition.rs` used for the 1 277 ms
/// figure, with a seed so that shards differ from one another. It stands in for the
/// `(morton, tessera_id)` order a build produces: `project`'s cost depends only on the map being
/// uncorrelated with entity order. A bijection on the larger domain restricted by walking is a
/// bijection on `[0, n)`; `Permutation::validate_rows` checks that below.
struct Shuffle {
    half_bits: u32,
    mask: u32,
    n: u64,
    seed: u64,
}

impl Shuffle {
    fn new(n: u64, seed: u64) -> Self {
        let mut half_bits = 1u32;
        while (1u64 << (2 * half_bits)) < n {
            half_bits += 1;
        }
        Shuffle {
            half_bits,
            mask: (1u32 << half_bits) - 1,
            n,
            seed,
        }
    }

    fn round(&self, i: u64, r: u32) -> u32 {
        let x = r as u64 ^ self.seed ^ 0x5DEE_CE66_D000_0000u64.wrapping_mul(i + 1);
        (splitmix64(x) >> 40) as u32 & self.mask
    }

    fn feistel(&self, x: u64) -> u64 {
        let mut l = ((x >> self.half_bits) as u32) & self.mask;
        let mut r = (x as u32) & self.mask;
        for i in 0..4u64 {
            let nl = r;
            r = l ^ self.round(i, r);
            l = nl;
        }
        ((l as u64) << self.half_bits) | (r as u64)
    }

    fn at(&self, x: u64) -> u64 {
        let mut v = self.feistel(x);
        while v >= self.n {
            v = self.feistel(v);
        }
        v
    }
}

/// Write a scattered permutation over `rows` entities and open it. Slot `e` gets the shuffle's
/// image of `e`, written in ascending `e`, so the write is a sequential stream through the
/// mapping. `validate_rows` then confirms the file is a bijection: a half-written fixture has the
/// same length as a complete one and would otherwise be measured as one. Not timed.
fn write_scattered(path: &Path, rows: u64, seed: u64) -> Permutation {
    let shuffle = Shuffle::new(rows, seed);
    let mut writer = PermutationWriter::create(path, rows).expect("permutation file");
    for entity in 0..rows {
        writer
            .set(EntityId::new(entity), shuffle.at(entity) as u32)
            .expect("every image is in bound and distinct");
    }
    writer.finish().expect("flush");
    let permutation = Permutation::load(path).expect("permutation loads");
    permutation
        .validate_rows(u32::try_from(rows).expect("rows fit u32"))
        .expect("the fixture is a bijection onto its rows");
    permutation
}

// ---------------------------------------------------------------------------------------------
// Masks.
// ---------------------------------------------------------------------------------------------

/// A deterministic 64-bit stream, so two runs of this probe draw the same masks.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let state = self.0;
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        splitmix64(state)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// Uniform on `(0, 1]`, so its logarithm is finite.
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
}

/// Each entity of `[0, rows)` in with probability `coverage`, independently. Drawn by geometric
/// skips, which has the same distribution as a Bernoulli trial per entity and costs one draw per
/// member instead of one per entity.
fn scattered_mask(rows: u64, coverage: f64, rng: &mut Rng) -> Bitmap {
    let log_miss = (1.0 - coverage).ln();
    let gap = |rng: &mut Rng| (rng.unit().ln() / log_miss).floor() as u64;
    let mut out = Bitmap::new();
    let mut buffer: Vec<u32> = Vec::with_capacity(1 << 20);
    let mut entity = gap(rng);
    while entity < rows {
        buffer.push(entity as u32);
        if buffer.len() == buffer.capacity() {
            out.add_many(&buffer);
            buffer.clear();
        }
        entity += 1 + gap(rng);
    }
    out.add_many(&buffer);
    out
}

/// One run of `coverage × rows` entities at a random position, run-optimised: the shape of a
/// fragment whose signatures are contiguous in entity space.
fn contiguous_mask(rows: u64, coverage: f64, rng: &mut Rng) -> Bitmap {
    let len = ((rows as f64) * coverage).round() as u64;
    let start = rng.below(rows - len + 1);
    let mut out = Bitmap::from_range(start as u32..(start + len) as u32);
    out.run_optimize();
    out
}

/// A mask's seed from its configuration, so the same configuration draws the same mask in any
/// process and any order.
fn mask_seed(rows: u64, coverage: f64, shape: &str) -> u64 {
    let shape_bits = shape
        .bytes()
        .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
    splitmix64(rows ^ coverage.to_bits().rotate_left(17) ^ shape_bits)
}

/// The global mask restricted to each shard's entity range and shifted to that shard's local
/// ids. The module doc says why this exists and why it is timed apart from the projection.
fn split(mask: &Bitmap, shards: u64, per_shard: u64) -> Vec<Bitmap> {
    (0..shards)
        .map(|i| {
            let lo = i * per_shard;
            let hi = lo + per_shard;
            mask.and(&Bitmap::from_range(lo as u32..hi as u32))
                .add_offset(-(lo as i64))
        })
        .collect()
}

/// `project`, or `project_with` over the caller's scratch when there is one.
fn project(perm: &Permutation, mask: &Bitmap, scratch: Option<&mut ProjectScratch>) -> Bitmap {
    match scratch {
        Some(scratch) => perm.project_with(mask, scratch),
        None => perm.project(mask),
    }
}

// ---------------------------------------------------------------------------------------------
// What a result occupies.
// ---------------------------------------------------------------------------------------------

/// What result bitmaps occupy, two ways. `portable` is croaring's portable serialised size: the
/// container payloads plus a four-byte key and cardinality per container and the header, the
/// figure the corpus quotes for projection sizes. `containers` is what `Bitmap::statistics`
/// reports the containers as holding on the heap. Neither includes the per-container index (a
/// pointer, a key and a typecode each, at whatever capacity croaring grew the arrays to) or the
/// `Bitmap` struct itself; `struct_bytes` counts the latter. The resident-set delta in `tokens`
/// is the figure to budget from; these explain it.
#[derive(Default, Clone, Copy)]
struct Footprint {
    bitmaps: u64,
    portable: u64,
    containers: u64,
    container_count: u64,
    cardinality: u64,
}

impl Footprint {
    fn of(bitmap: &Bitmap) -> Self {
        let s = bitmap.statistics();
        Footprint {
            bitmaps: 1,
            portable: bitmap.get_serialized_size_in_bytes::<Portable>() as u64,
            containers: s.n_bytes_array_containers as u64
                + s.n_bytes_run_containers as u64
                + s.n_bytes_bitset_containers as u64,
            container_count: s.n_containers as u64,
            cardinality: s.cardinality,
        }
    }

    fn sum<'a>(bitmaps: impl IntoIterator<Item = &'a Bitmap>) -> Self {
        bitmaps
            .into_iter()
            .fold(Footprint::default(), |mut acc, b| {
                let f = Footprint::of(b);
                acc.bitmaps += f.bitmaps;
                acc.portable += f.portable;
                acc.containers += f.containers;
                acc.container_count += f.container_count;
                acc.cardinality += f.cardinality;
                acc
            })
    }

    fn json(&self) -> Value {
        json!({
            "bitmaps": self.bitmaps,
            "struct_bytes": self.bitmaps * std::mem::size_of::<Bitmap>() as u64,
            "portable_bytes": self.portable,
            "container_bytes": self.containers,
            "containers": self.container_count,
            "cardinality": self.cardinality,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Timing.
// ---------------------------------------------------------------------------------------------

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    sorted[sorted.len() / 2]
}

fn min(samples: &[f64]) -> f64 {
    samples.iter().copied().fold(f64::INFINITY, f64::min)
}

/// This process's CPU time, page faults and context switches so far, and four system-wide
/// counters from `/proc/vmstat`. Read around every timed call because the box is shared. Wall
/// clock well above CPU time is the scheduler's doing; a major fault during a call is the
/// fixture's pages being re-read after the page cache evicted them; a voluntary switch is the
/// process sleeping on something, a non-voluntary one is preemption. The vmstat counters name
/// the kernel-side stalls a large transient allocation can hit: direct compaction stalls,
/// transparent-huge-page faults and fallbacks, and swap traffic. They are system-wide, so a
/// co-tenant contributes to them; a jump that coincides with a slow sample is evidence only.
#[derive(Clone, Copy)]
struct Usage {
    cpu_s: f64,
    majflt: i64,
    minflt: i64,
    voluntary_switches: i64,
    nonvoluntary_switches: i64,
    compact_stall: i64,
    thp_fault_alloc: i64,
    thp_fault_fallback: i64,
    swap_pages: i64,
}

fn vmstat_field(text: &str, field: &str) -> i64 {
    text.lines()
        .find_map(|line| {
            let rest = line.strip_prefix(field)?.strip_prefix(' ')?;
            rest.trim().parse().ok()
        })
        .unwrap_or(0)
}

fn rusage() -> libc::rusage {
    // SAFETY: `ru` is a valid, writable `rusage`, and `RUSAGE_SELF` reads this process's own
    // counters into it; nothing else is touched.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    }
}

/// The reading taken before a timed call. `getrusage` is read last, after the `/proc` reads, so
/// their cost falls outside the CPU-time window; [`usage_after`] reads it first for the same
/// reason. With one order for both, the `/proc` reads of one side (about 70 µs) land inside
/// every window, which at the per-token scale is a third of the call.
fn usage_before() -> Usage {
    let (status, vmstat) = proc_counters();
    usage_from(rusage(), &status, &vmstat)
}

/// The reading taken after a timed call; see [`usage_before`].
fn usage_after() -> Usage {
    let ru = rusage();
    let (status, vmstat) = proc_counters();
    usage_from(ru, &status, &vmstat)
}

fn proc_counters() -> (String, String) {
    (
        std::fs::read_to_string("/proc/self/status").expect("this probe requires /proc"),
        std::fs::read_to_string("/proc/vmstat").unwrap_or_default(),
    )
}

fn usage_from(ru: libc::rusage, status: &str, vmstat: &str) -> Usage {
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
    let switches = |field: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(field)?.strip_prefix(':')?.trim().parse().ok())
            .unwrap_or(0)
    };
    Usage {
        cpu_s: seconds(ru.ru_utime) + seconds(ru.ru_stime),
        majflt: ru.ru_majflt,
        minflt: ru.ru_minflt,
        voluntary_switches: switches("voluntary_ctxt_switches"),
        nonvoluntary_switches: switches("nonvoluntary_ctxt_switches"),
        compact_stall: vmstat_field(vmstat, "compact_stall"),
        thp_fault_alloc: vmstat_field(vmstat, "thp_fault_alloc"),
        thp_fault_fallback: vmstat_field(vmstat, "thp_fault_fallback"),
        swap_pages: vmstat_field(vmstat, "pswpin") + vmstat_field(vmstat, "pswpout"),
    }
}

impl Usage {
    fn since(self, earlier: Usage) -> Usage {
        Usage {
            cpu_s: self.cpu_s - earlier.cpu_s,
            majflt: self.majflt - earlier.majflt,
            minflt: self.minflt - earlier.minflt,
            voluntary_switches: self.voluntary_switches - earlier.voluntary_switches,
            nonvoluntary_switches: self.nonvoluntary_switches - earlier.nonvoluntary_switches,
            compact_stall: self.compact_stall - earlier.compact_stall,
            thp_fault_alloc: self.thp_fault_alloc - earlier.thp_fault_alloc,
            thp_fault_fallback: self.thp_fault_fallback - earlier.thp_fault_fallback,
            swap_pages: self.swap_pages - earlier.swap_pages,
        }
    }
}

/// The timed repetitions of one configuration: wall clock, and what [`Usage`] saw during each.
#[derive(Default)]
struct Samples {
    wall_ms: Vec<f64>,
    cpu_ms: Vec<f64>,
    majflt: Vec<i64>,
    minflt: Vec<i64>,
    voluntary_switches: Vec<i64>,
    nonvoluntary_switches: Vec<i64>,
    compact_stall: Vec<i64>,
    thp_fault_alloc: Vec<i64>,
    thp_fault_fallback: Vec<i64>,
    swap_pages: Vec<i64>,
}

impl Samples {
    fn push(&mut self, wall_ms: f64, used: Usage) {
        self.wall_ms.push(wall_ms);
        self.cpu_ms.push(used.cpu_s * 1e3);
        self.majflt.push(used.majflt);
        self.minflt.push(used.minflt);
        self.voluntary_switches.push(used.voluntary_switches);
        self.nonvoluntary_switches.push(used.nonvoluntary_switches);
        self.compact_stall.push(used.compact_stall);
        self.thp_fault_alloc.push(used.thp_fault_alloc);
        self.thp_fault_fallback.push(used.thp_fault_fallback);
        self.swap_pages.push(used.swap_pages);
    }

    fn median_ms(&self) -> f64 {
        median(&self.wall_ms)
    }

    fn min_ms(&self) -> f64 {
        min(&self.wall_ms)
    }

    fn median_cpu_ms(&self) -> f64 {
        median(&self.cpu_ms)
    }

    fn json(&self) -> Value {
        json!({
            "samples_ms": self.wall_ms,
            "cpu_ms": self.cpu_ms,
            "majflt": self.majflt,
            "minflt": self.minflt,
            "voluntary_switches": self.voluntary_switches,
            "nonvoluntary_switches": self.nonvoluntary_switches,
            "compact_stall": self.compact_stall,
            "thp_fault_alloc": self.thp_fault_alloc,
            "thp_fault_fallback": self.thp_fault_fallback,
            "swap_pages": self.swap_pages,
            "median_ms": self.median_ms(),
            "min_ms": self.min_ms(),
            "median_cpu_ms": self.median_cpu_ms(),
        })
    }
}

/// `reps` timed calls of `f` after one untimed warm-up, with the last result.
fn timed<T>(reps: usize, mut f: impl FnMut() -> T) -> (Samples, T) {
    let mut last = f();
    let mut samples = Samples::default();
    for _ in 0..reps {
        let before = usage_before();
        let started = Instant::now();
        last = f();
        let wall_ms = started.elapsed().as_secs_f64() * 1e3;
        samples.push(wall_ms, usage_after().since(before));
    }
    (samples, last)
}

fn emit(line: &Value) {
    println!("{line}");
}

/// Fold `extra`'s fields into `line`; both are objects.
fn merge(line: &mut Value, extra: Value) {
    let (Value::Object(target), Value::Object(source)) = (line, extra) else {
        panic!("merge takes two JSON objects");
    };
    target.extend(source);
}

// ---------------------------------------------------------------------------------------------
// (a) Linearity in rows.
// ---------------------------------------------------------------------------------------------

fn linearity(args: &Args, dir: &Path) {
    for &rows in &args.rows {
        let path = dir.join(format!("linear-{rows}.bin"));
        let started = Instant::now();
        let permutation = write_scattered(&path, rows, 0x51);
        eprintln!(
            "# linearity: {rows} rows, fixture written and validated in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        for shape in &args.mask {
            for &coverage in &args.coverage {
                let mut rng = Rng(mask_seed(rows, coverage, shape));
                let mask = match shape.as_str() {
                    "scattered" => scattered_mask(rows, coverage, &mut rng),
                    "contiguous" => contiguous_mask(rows, coverage, &mut rng),
                    other => panic!("--mask takes scattered|contiguous, got {other:?}"),
                };
                let (samples, out) = timed(args.reps, || permutation.project(&mask));
                assert_eq!(
                    out.cardinality(),
                    mask.cardinality(),
                    "every masked entity has a row, so the projection is as large as the mask"
                );
                let med = samples.median_ms();
                let footprint = Footprint::of(&out);
                eprintln!(
                    "linearity rows={rows} mask={shape} coverage={coverage} card={} \
                     median={med:.2} ms (cpu {:.2}) min={:.2} ms majflt={:?} \
                     switches vol={:?} nonvol={:?} compact_stall={:?} thp={:?}/{:?} swap={:?}  \
                     {:.3} ns/row  {:.2} ns/projected row  result {:.2} MB portable, {} containers",
                    mask.cardinality(),
                    samples.median_cpu_ms(),
                    samples.min_ms(),
                    samples.majflt,
                    samples.voluntary_switches,
                    samples.nonvoluntary_switches,
                    samples.compact_stall,
                    samples.thp_fault_alloc,
                    samples.thp_fault_fallback,
                    samples.swap_pages,
                    med * 1e6 / rows as f64,
                    med * 1e6 / mask.cardinality() as f64,
                    footprint.portable as f64 / 1e6,
                    footprint.container_count,
                );
                let mut line = json!({
                    "part": "linearity",
                    "rows": rows,
                    "mask": shape,
                    "coverage": coverage,
                    "mask_cardinality": mask.cardinality(),
                    "mask_containers": mask.statistics().n_containers,
                    "reps": args.reps,
                    "ns_per_row": med * 1e6 / rows as f64,
                    "ns_per_projected_row": med * 1e6 / mask.cardinality() as f64,
                    "result": footprint.json(),
                });
                merge(&mut line, samples.json());
                emit(&line);
            }
        }
        drop(permutation);
        std::fs::remove_file(&path).ok();
    }
}

// ---------------------------------------------------------------------------------------------
// (b) One mask, one permutation against N.
// ---------------------------------------------------------------------------------------------

fn shards(args: &Args, dir: &Path) {
    let rows = args.shard_rows;
    let n = args.shards;
    assert_eq!(rows % n, 0, "--shard-rows must split evenly across --shards");
    let per_shard = rows / n;

    let whole_path = dir.join("shards-whole.bin");
    let whole = write_scattered(&whole_path, rows, 0x1);
    let shard_paths: Vec<PathBuf> = (0..n)
        .map(|i| dir.join(format!("shard-{i}.bin")))
        .collect();
    let parts: Vec<Permutation> = shard_paths
        .iter()
        .enumerate()
        .map(|(i, path)| write_scattered(path, per_shard, 0x100 + i as u64))
        .collect();
    eprintln!("# shards: {rows} rows as 1 and as {n} × {per_shard}, fixtures written");
    let mut scratch = args.scratch.then(ProjectScratch::default);

    for &coverage in &args.coverage {
        let mut rng = Rng(mask_seed(rows, coverage, "shards"));
        let mask = scattered_mask(rows, coverage, &mut rng);

        let (one_samples, one) = timed(args.reps, || project(&whole, &mask, scratch.as_mut()));
        let (split_samples, locals) = timed(args.reps, || split(&mask, n, per_shard));
        let (sharded_samples, outs) = timed(args.reps, || {
            parts
                .iter()
                .zip(&locals)
                .map(|(p, m)| project(p, m, scratch.as_mut()))
                .collect::<Vec<Bitmap>>()
        });

        let one_fp = Footprint::of(&one);
        let sharded_fp = Footprint::sum(&outs);
        assert_eq!(one_fp.cardinality, mask.cardinality());
        assert_eq!(sharded_fp.cardinality, mask.cardinality());

        let one_ms = one_samples.median_ms();
        let sharded_ms = sharded_samples.median_ms();
        let split_ms = split_samples.median_ms();
        eprintln!(
            "shards scratch={} coverage={coverage} card={}  one={one_ms:.2} ms (cpu {:.2})  \
             sharded={sharded_ms:.2} ms (cpu {:.2}) (+ split {split_ms:.2} ms)  ratio {:.3} \
             project-only, {:.3} with split  heap one={} B sharded={} B ratio {:.4}",
            args.scratch,
            mask.cardinality(),
            one_samples.median_cpu_ms(),
            sharded_samples.median_cpu_ms(),
            sharded_ms / one_ms,
            (sharded_ms + split_ms) / one_ms,
            one_fp.portable,
            sharded_fp.portable,
            sharded_fp.portable as f64 / one_fp.portable as f64,
        );
        let mut one_json = one_samples.json();
        merge(&mut one_json, json!({ "result": one_fp.json() }));
        let mut sharded_json = sharded_samples.json();
        merge(
            &mut sharded_json,
            json!({ "split": split_samples.json(), "result": sharded_fp.json() }),
        );
        emit(&json!({
            "part": "shards",
            "scratch": args.scratch,
            "rows": rows,
            "shards": n,
            "per_shard_rows": per_shard,
            "coverage": coverage,
            "mask_cardinality": mask.cardinality(),
            "reps": args.reps,
            "one": one_json,
            "sharded": sharded_json,
            "ratio_project_only": sharded_ms / one_ms,
            "ratio_with_split": (sharded_ms + split_ms) / one_ms,
            "ratio_portable_bytes": sharded_fp.portable as f64 / one_fp.portable as f64,
            "ratio_container_bytes": sharded_fp.containers as f64 / one_fp.containers as f64,
        }));
    }

    drop(parts);
    drop(whole);
    std::fs::remove_file(&whole_path).ok();
    for path in &shard_paths {
        std::fs::remove_file(path).ok();
    }
}

// ---------------------------------------------------------------------------------------------
// (b) Per token: N leaves held at once, for many tokens.
// ---------------------------------------------------------------------------------------------

fn tokens(args: &Args, dir: &Path) {
    let rows = args.token_rows;
    let n = args.shards;
    assert_eq!(rows % n, 0, "--token-rows must split evenly across --shards");
    let per_shard = rows / n;
    let shapes: Vec<&str> = match args.token_shape.as_str() {
        "one" => vec!["one"],
        "sharded" => vec!["sharded"],
        "both" => vec!["one", "sharded"],
        other => panic!("--token-shape takes one|sharded|both, got {other:?}"),
    };

    for shape in shapes {
        let mut paths = Vec::new();
        let permutations: Vec<Permutation> = if shape == "one" {
            let path = dir.join("tokens-whole.bin");
            let p = write_scattered(&path, rows, 0x2);
            paths.push(path);
            vec![p]
        } else {
            (0..n)
                .map(|i| {
                    let path = dir.join(format!("tokens-shard-{i}.bin"));
                    let p = write_scattered(&path, per_shard, 0x200 + i);
                    paths.push(path);
                    p
                })
                .collect()
        };
        let mut scratch = args.scratch.then(ProjectScratch::default);

        // One token's worth of work, discarded, so the mapping's pages and the allocator's first
        // structures are in before the reading everything is measured against.
        let mut rng = Rng(mask_seed(rows, args.token_coverage, "warm"));
        let warm = scattered_mask(rows, args.token_coverage, &mut rng);
        let warm_out: Vec<Bitmap> = if shape == "one" {
            vec![project(&permutations[0], &warm, scratch.as_mut())]
        } else {
            split(&warm, n, per_shard)
                .iter()
                .zip(&permutations)
                .map(|(m, p)| project(p, m, scratch.as_mut()))
                .collect()
        };
        drop(warm_out);
        drop(warm);
        malloc_trim();

        let rss_before = status_kb("VmRSS") * 1024;
        let hwm_before = status_kb("VmHWM") * 1024;
        let malloc_before = malloc_in_use_bytes();

        let mut held: Vec<Bitmap> = Vec::with_capacity(args.tokens * permutations.len());
        let mut project_s = 0.0f64;
        let mut project_cpu_s = 0.0f64;
        let mut split_s = 0.0f64;
        for token in 0..args.tokens {
            // A distinct mask per token, drawn and dropped inside the loop, so what stays resident
            // across the loop is the projections and nothing else.
            let mut rng = Rng(mask_seed(rows, args.token_coverage, "token") ^ token as u64);
            let mask = scattered_mask(rows, args.token_coverage, &mut rng);
            if shape == "one" {
                let before = usage_before();
                let started = Instant::now();
                let out = project(&permutations[0], &mask, scratch.as_mut());
                project_s += started.elapsed().as_secs_f64();
                project_cpu_s += usage_after().since(before).cpu_s;
                held.push(out);
            } else {
                let started = Instant::now();
                let locals = split(&mask, n, per_shard);
                split_s += started.elapsed().as_secs_f64();
                let before = usage_before();
                let started = Instant::now();
                for (p, m) in permutations.iter().zip(&locals) {
                    held.push(project(p, m, scratch.as_mut()));
                }
                project_s += started.elapsed().as_secs_f64();
                project_cpu_s += usage_after().since(before).cpu_s;
            }
        }

        let rss_after = status_kb("VmRSS") * 1024;
        let hwm_after = status_kb("VmHWM") * 1024;
        let malloc_after = malloc_in_use_bytes();
        malloc_trim();
        let rss_trimmed = status_kb("VmRSS") * 1024;
        let footprint = Footprint::sum(&held);

        let per_token = |bytes: u64| bytes as f64 / args.tokens as f64;
        eprintln!(
            "tokens shape={shape} scratch={} tokens={} bitmaps={}  project {:.1} µs/token (cpu \
             {:.1}; split {:.1} µs/token)  RSS +{:.1} MB ({:.1} KB/token; trimmed +{:.1} MB)  \
             malloc in use +{:.1} MB  portable {:.1} MB  containers {:.1} MB in {}",
            args.scratch,
            args.tokens,
            held.len(),
            project_s * 1e6 / args.tokens as f64,
            project_cpu_s * 1e6 / args.tokens as f64,
            split_s * 1e6 / args.tokens as f64,
            (rss_after - rss_before) as f64 / 1e6,
            per_token(rss_after - rss_before) / 1e3,
            (rss_trimmed - rss_before) as f64 / 1e6,
            (malloc_after - malloc_before) as f64 / 1e6,
            footprint.portable as f64 / 1e6,
            footprint.containers as f64 / 1e6,
            footprint.container_count,
        );
        emit(&json!({
            "part": "tokens",
            "shape": shape,
            "scratch": args.scratch,
            "rows": rows,
            "shards": if shape == "one" { 1 } else { n },
            "per_shard_rows": if shape == "one" { rows } else { per_shard },
            "coverage": args.token_coverage,
            "tokens": args.tokens,
            "bitmaps_held": held.len(),
            "project_total_ms": project_s * 1e3,
            "project_us_per_token": project_s * 1e6 / args.tokens as f64,
            "project_cpu_total_ms": project_cpu_s * 1e3,
            "project_cpu_us_per_token": project_cpu_s * 1e6 / args.tokens as f64,
            "split_total_ms": split_s * 1e3,
            "split_us_per_token": split_s * 1e6 / args.tokens as f64,
            "rss_before_bytes": rss_before,
            "rss_after_bytes": rss_after,
            "rss_after_trim_bytes": rss_trimmed,
            "rss_delta_bytes": rss_after - rss_before,
            "rss_delta_bytes_per_token": per_token(rss_after - rss_before),
            "vmhwm_before_bytes": hwm_before,
            "vmhwm_after_bytes": hwm_after,
            "malloc_in_use_delta_bytes": malloc_after - malloc_before,
            "malloc_in_use_delta_bytes_per_token": per_token(malloc_after - malloc_before),
            "result": footprint.json(),
        }));

        drop(held);
        drop(permutations);
        for path in &paths {
            std::fs::remove_file(path).ok();
        }
        malloc_trim();
    }
}

fn main() {
    let args = Args::parse();

    let available_gb = meminfo_kb("MemAvailable") as f64 / (1024.0 * 1024.0);
    if available_gb < args.min_available_gb {
        eprintln!(
            "refusing to start: {available_gb:.1} GB available, below the {:.1} GB floor \
             (the box is shared; pass --min-available-gb to override)",
            args.min_available_gb
        );
        std::process::exit(2);
    }

    let dir = args.dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("epoch-shard-projection-{}", std::process::id()))
    });
    std::fs::create_dir_all(&dir).expect("scratch directory");
    emit(&host());

    match args.part.as_str() {
        "linearity" => linearity(&args, &dir),
        "shards" => shards(&args, &dir),
        "tokens" => tokens(&args, &dir),
        "all" => {
            linearity(&args, &dir);
            shards(&args, &dir);
            tokens(&args, &dir);
        }
        other => panic!("--part takes linearity|shards|tokens|all, got {other:?}"),
    }
}
