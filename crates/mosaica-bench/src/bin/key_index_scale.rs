//! **What a key index run costs at scale**: write time and file size for sorted `u64` entries,
//! the cost of opening the run, single lookups with the entry pages out of the page cache and in
//! it, batched lookups, the full verification, and optionally the spill sort of the same number
//! of entries arriving in random and in sequential key order.
//!
//! `--keys dense` puts the `i`th key at `2i` or `2i + 1`, half the integers in the range, as a
//! sequence of record numbers with holes is. `--keys random` puts it uniformly in the lower half
//! of the `i`th of `n` equal parts of the `u64` range, so gaps between keys are about as wide as
//! between `n` sorted uniform keys. Entities are random in key order in both.
//!
//! "Cold" means the run was closed and its file's pages dropped from this machine's page cache
//! with `posix_fadvise(POSIX_FADV_DONTNEED)` before the run was opened again, which needs no
//! privilege. The binary prints how much of the file was resident after every drop. Under WSL2
//! or a VM the host may still hold the bytes, so a cold read there is a read from the host's
//! cache, not necessarily from the disk.
//!
//! Each spill arm runs in a child process of its own, so its peak resident set (`VmHWM`) and its
//! anonymous memory (`RssAnon`) are the spill's and not the lookups' mappings.
//!
//! `--filters` replaces all of that with a sweep over key filter shapes: the run is written once
//! per shape, `0` for none or `bits:hashes`, and for each the bench reports the filter's bytes,
//! the share of absent keys it passes, the cold open, single lookups of a present key, of an
//! absent key the filter refuses and of one it passes, and a batch of absent keys. A single lookup
//! is a one-key `lookup_sorted`, the call a unique index makes. Cold here means the run is open
//! and its entry pages and filter blocks were dropped before each lookup, from this process's
//! mapping with `madvise(MADV_PAGEOUT)` and from the page cache with `posix_fadvise`; the header
//! and page index stay resident. "Pages cold" drops the entry pages alone, so the filter is
//! resident. The cold open is timed twice: as the run opens, and with the filter's pages read
//! ahead first. Each shape prints one JSON line starting `filter-shape`.
//!
//! ```text
//! cargo run --release -p mosaica-bench --bin key_index_scale -- --dir <scratch> [--entries N] [--keys dense|random] [--spill]
//! cargo run --release -p mosaica-bench --bin key_index_scale -- --dir <scratch> --entries N --filters 0,6:4,10:7
//! ```

use std::fs::File;
use std::io::Read;
use std::num::NonZeroU64;
use std::ops::Range;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use mosaica_store::key_index::{verify_run, KeyRun, KeyRunWriter, KeySpill};

#[derive(Parser)]
#[command(about = "key index run write, open, lookup and spill costs at scale")]
struct Args {
    /// Directory to write the run under; a subdirectory is created and removed.
    #[arg(long)]
    dir: PathBuf,
    #[arg(long, default_value_t = 100_000_000)]
    entries: u64,
    /// The keys (see the module doc).
    #[arg(long, value_enum, default_value_t = KeyPattern::Dense)]
    keys: KeyPattern,
    /// Single lookups timed in each of the cold and warm passes.
    #[arg(long, default_value_t = 2_000)]
    lookups: usize,
    /// Also time the spill sort over the same number of entries.
    #[arg(long)]
    spill: bool,
    /// The spill's memory budget in MiB.
    #[arg(long, default_value_t = 1024)]
    budget_mib: usize,
    /// Run one spill arm, `random`, `sequential` or `dense`, and nothing else.
    #[arg(long, hide = true)]
    spill_arm: Option<String>,
    /// Sweep these key filter shapes instead (see the module doc): `0` or `bits:hashes`, comma
    /// separated.
    #[arg(long, value_delimiter = ',')]
    filters: Vec<String>,
    /// Absent keys the sweep counts the filter's answer for.
    #[arg(long, default_value_t = 20_000_000)]
    fpr_keys: u64,
    /// Single lookups timed cold in each case of the sweep; `--lookups` sets the warm ones.
    #[arg(long, default_value_t = 300)]
    cold_lookups: usize,
    /// Absent keys in the sweep's batched lookup.
    #[arg(long, default_value_t = 10_000)]
    batch: usize,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum KeyPattern {
    Dense,
    Random,
}

/// The keys of a run of `n` entries: the `i`th key, and a key between it and the next that is
/// never a key.
#[derive(Clone, Copy)]
enum Keys {
    Dense,
    Random { stride: u64 },
}

impl Keys {
    fn new(pattern: KeyPattern, n: u64) -> Self {
        match pattern {
            KeyPattern::Dense => Keys::Dense,
            KeyPattern::Random => Keys::Random {
                stride: u64::MAX / n.max(1),
            },
        }
    }

    fn key(self, i: u64) -> u64 {
        match self {
            Keys::Dense => 2 * i + (mix(i) & 1),
            Keys::Random { stride } => i * stride + mix(i) % (stride / 2),
        }
    }

    fn absent(self, i: u64) -> u64 {
        match self {
            Keys::Dense => self.key(i) ^ 1,
            Keys::Random { stride } => i * stride + stride / 2,
        }
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn drop_cache(path: &Path) {
    let file = File::open(path).expect("open run");
    // SAFETY: advisory call on this function's own descriptor.
    let code = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(code, 0, "posix_fadvise failed");
}

/// Resident bytes of `path`, from `mincore` over a fresh mapping.
fn resident(path: &Path) -> u64 {
    let len = std::fs::metadata(path).expect("metadata").len();
    resident_in(path, 0..len)
}

/// Resident bytes of the pages of `path` that `range` touches.
fn resident_in(path: &Path, range: Range<u64>) -> u64 {
    let page = 4096u64;
    let (lo, hi) = (range.start / page * page, range.end.div_ceil(page) * page);
    if lo >= hi {
        return 0;
    }
    let file = File::open(path).expect("open run");
    let len = file.metadata().expect("metadata").len();
    // SAFETY: read-only mapping of a file nothing modifies during the call.
    let map = unsafe {
        memmap2::MmapOptions::new()
            .offset(lo)
            .len((hi.min(len) - lo) as usize)
            .map(&file)
    }
    .expect("map");
    let mut vec = vec![0u8; map.len().div_ceil(page as usize)];
    // SAFETY: `vec` has one byte per page of the mapping.
    let code = unsafe {
        libc::mincore(
            map.as_ptr() as *mut libc::c_void,
            map.len(),
            vec.as_mut_ptr(),
        )
    };
    assert_eq!(code, 0, "mincore failed");
    vec.iter().filter(|&&b| b & 1 == 1).count() as u64 * page
}

/// The byte ranges of a run file's entry pages and of its key filter's blocks, from its header
/// page (the layout is in the module doc of `mosaica_store::key_index`).
fn regions(path: &Path) -> (Range<u64>, Range<u64>) {
    let mut header = [0u8; 84];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .expect("read header");
    let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().expect("8 bytes"));
    let (pages, offset, blocks) = (u64_at(24), u64_at(64), u64_at(72));
    (4096..4096 * (1 + pages), offset..offset + blocks * 64)
}

/// Drop `ranges` of the file at `path` from this process's mappings of it, with
/// `madvise(MADV_PAGEOUT)`, and from the page cache.
fn evict(path: &Path, ranges: &[Range<u64>]) {
    let page = 4096u64;
    let target = path.canonicalize().expect("canonical path");
    let maps = std::fs::read_to_string("/proc/self/maps").expect("this bench requires /proc");
    let hex = |s: &str| u64::from_str_radix(s, 16).expect("hex in /proc/self/maps");
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || Path::new(fields[5]) != target {
            continue;
        }
        let (start, end) = fields[0].split_once('-').expect("an address range");
        let (start, end, offset) = (hex(start), hex(end), hex(fields[2]));
        for range in ranges {
            let lo = (range.start / page * page).max(offset);
            let hi = (range.end.div_ceil(page) * page).min(offset + (end - start));
            if lo >= hi {
                continue;
            }
            // SAFETY: a hint over pages of a read-only file mapping this process holds; their
            // contents are read back from the file on the next touch.
            let code = unsafe {
                libc::madvise(
                    (start + lo - offset) as *mut libc::c_void,
                    (hi - lo) as usize,
                    libc::MADV_PAGEOUT,
                )
            };
            assert_eq!(code, 0, "madvise(MADV_PAGEOUT) failed");
        }
    }
    let file = File::open(path).expect("open run");
    for range in ranges.iter().filter(|r| r.start < r.end) {
        // SAFETY: advisory call on this function's own descriptor.
        let code = unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                range.start as i64,
                (range.end - range.start) as i64,
                libc::POSIX_FADV_DONTNEED,
            )
        };
        assert_eq!(code, 0, "posix_fadvise failed");
    }
}

/// This process's major page faults so far: reads that waited on the disc.
fn major_faults() -> u64 {
    // SAFETY: `getrusage` fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let code = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(code, 0, "getrusage failed");
    usage.ru_majflt as u64
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// Timings in nanoseconds, or null where there were none.
fn stats(mut times: Vec<Duration>) -> serde_json::Value {
    if times.is_empty() {
        return serde_json::Value::Null;
    }
    times.sort_unstable();
    let ns = |d: Duration| d.as_nanos() as u64;
    let pick = |q: f64| ns(times[((times.len() - 1) as f64 * q) as usize]);
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    serde_json::json!({
        "n": times.len(),
        "mean_ns": ns(mean),
        "p50_ns": pick(0.5),
        "p90_ns": pick(0.9),
        "p99_ns": pick(0.99),
    })
}

/// Write the run with the key filter `spec` names, `0` for none or `bits:hashes`, and measure
/// it (see the module doc).
fn filter_shape(args: &Args, dir: &Path, keys: Keys, spec: &str) {
    let n = args.entries;
    let shape = match spec {
        "0" => None,
        s => {
            let (bits, hashes) = s
                .split_once(':')
                .unwrap_or_else(|| panic!("--filters takes 0 or bits:hashes, not {s}"));
            Some((
                bits.parse::<u64>().expect("bits a key"),
                hashes.parse::<u32>().expect("hashes"),
            ))
        }
    };
    let load_before = load_average();
    let started = Instant::now();
    let writer =
        KeyRunWriter::<u64>::create(dir, "run", NonZeroU64::new(u64::MAX).expect("non-zero"));
    let mut writer = match shape {
        Some((bits, hashes)) => writer.with_key_filter_of(bits, hashes),
        None => writer,
    };
    for i in 0..n {
        writer
            .push(keys.key(i), mix(i ^ 0xABCD) as u32)
            .expect("push");
    }
    let runs = writer.finish().expect("finish");
    let write = started.elapsed();
    let path = runs[0].path.clone();
    // Written back, so the page cache can drop the file's pages.
    mosaica_store::fsync_written(std::slice::from_ref(&path)).expect("fsync");
    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    let (entry_pages, filter) = regions(&path);
    let filter_bytes = filter.end - filter.start;
    let rest_bytes = if filter_bytes == 0 {
        file_bytes
    } else {
        filter.start
    };

    let faults = major_faults();
    let (run, open_cold) = cold_open(&path);
    let open_faults = major_faults() - faults;

    // The share of absent keys, spread evenly over the run, that the filter passes.
    let tested = args.fpr_keys.min(n);
    let stride = n / tested;
    let t = Instant::now();
    let passed = (0..tested)
        .filter(|&j| run.may_hold(keys.absent(j * stride)))
        .count() as u64;
    let probe_scan = t.elapsed();
    let rate = passed as f64 / tested as f64;
    println!(
        "{spec}: filter {filter_bytes} bytes of {file_bytes} ({:.1}% of the rest), passes {passed} \
         of {tested} absent keys ({:.4}%)",
        100.0 * filter_bytes as f64 / rest_bytes as f64,
        100.0 * rate
    );

    // Present keys, absent keys the filter refuses, and absent keys it passes.
    let mut rng = StdRng::seed_from_u64(42);
    let present: Vec<u64> = (0..args.lookups)
        .map(|_| keys.key(rng.gen_range(0..n)))
        .collect();
    let (mut refused, mut maybe) = (Vec::new(), Vec::new());
    let mut tries = 0u64;
    while (maybe.len() < args.lookups || (shape.is_some() && refused.len() < args.lookups))
        && tries < 100_000_000
    {
        let key = keys.absent(rng.gen_range(0..n));
        tries += 1;
        let set = if run.may_hold(key) {
            &mut maybe
        } else {
            &mut refused
        };
        if set.len() < args.lookups {
            set.push(key);
        }
    }

    let one = |key: u64, hits: usize| -> Duration {
        let t = Instant::now();
        let found = run
            .lookup_sorted(std::slice::from_ref(&key))
            .expect("lookup");
        let e = t.elapsed();
        assert_eq!(found.len(), hits);
        e
    };
    let both = [entry_pages.clone(), filter.clone()];
    let pages_only = [entry_pages.clone()];
    evict(&path, &both);
    let left = resident_in(&path, entry_pages.clone()) + resident_in(&path, filter.clone());
    let cold = |set: &[u64], hits: usize, dropped: &[Range<u64>]| -> Vec<Duration> {
        set.iter()
            .take(args.cold_lookups)
            .map(|&key| {
                evict(&path, dropped);
                one(key, hits)
            })
            .collect()
    };
    let warm = |set: &[u64], hits: usize| -> Vec<Duration> {
        for &key in set {
            one(key, hits);
        }
        set.iter().map(|&key| one(key, hits)).collect()
    };
    let (present_cold, refused_cold, maybe_cold) = (
        cold(&present, 1, &both),
        cold(&refused, 0, &both),
        cold(&maybe, 0, &both),
    );
    read_into_cache(&path, filter.clone());
    let (present_pages_cold, refused_pages_cold, maybe_pages_cold) = (
        cold(&present, 1, &pages_only),
        cold(&refused, 0, &pages_only),
        cold(&maybe, 0, &pages_only),
    );
    let (present_warm, refused_warm, maybe_warm) =
        (warm(&present, 1), warm(&refused, 0), warm(&maybe, 0));

    // A batch of absent keys, as a write resolving new values against a run that holds none.
    let mut batch: Vec<u64> = (0..args.batch)
        .map(|_| keys.absent(rng.gen_range(0..n)))
        .collect();
    batch.sort_unstable();
    batch.dedup();
    let batch_passed = batch.iter().filter(|&&k| run.may_hold(k)).count();
    let time_batch = || {
        let t = Instant::now();
        assert!(run.lookup_sorted(&batch).expect("lookup").is_empty());
        t.elapsed()
    };
    let faults = major_faults();
    let batch_cold: Vec<Duration> = (0..3)
        .map(|_| {
            evict(&path, &both);
            time_batch()
        })
        .collect();
    let batch_cold_faults = major_faults() - faults;
    read_into_cache(&path, filter.clone());
    let batch_pages_cold: Vec<Duration> = (0..3)
        .map(|_| {
            evict(&path, &pages_only);
            time_batch()
        })
        .collect();
    time_batch();
    let batch_warm = (0..5).map(|_| time_batch()).min().expect("five runs");
    drop(run);

    // The open again, with the filter's pages read ahead before it, as an open advising the
    // kernel that it will need them would.
    drop_cache(&path);
    if filter_bytes > 0 {
        let file = File::open(&path).expect("open run");
        // SAFETY: advisory call on this function's own descriptor.
        let code = unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                filter.start as i64,
                filter_bytes as i64,
                libc::POSIX_FADV_WILLNEED,
            )
        };
        assert_eq!(code, 0, "posix_fadvise failed");
    }
    let t = Instant::now();
    drop(KeyRun::<u64>::open(&path).expect("open"));
    let open_cold_readahead = t.elapsed();

    println!(
        "filter-shape {}",
        serde_json::json!({
            "entries": n,
            "keys": format!("{:?}", args.keys),
            "spec": spec,
            "bits": shape.map_or(0, |s| s.0),
            "hashes": shape.map_or(0, |s| s.1),
            "write_s": write.as_secs_f64(),
            "file_bytes": file_bytes,
            "filter_bytes": filter_bytes,
            "rest_bytes": rest_bytes,
            "open_cold_ns": open_cold.as_nanos() as u64,
            "open_cold_major_faults": open_faults,
            "open_cold_readahead_ns": open_cold_readahead.as_nanos() as u64,
            "fpr_tested": tested,
            "fpr_passed": passed,
            "fpr": rate,
            "probe_scan_mean_ns": probe_scan.as_nanos() as f64 / tested as f64,
            "resident_after_evict": left,
            "present_cold": stats(present_cold),
            "refused_cold": stats(refused_cold),
            "maybe_cold": stats(maybe_cold),
            "present_warm": stats(present_warm),
            "refused_warm": stats(refused_warm),
            "maybe_warm": stats(maybe_warm),
            "present_pages_cold": stats(present_pages_cold),
            "refused_pages_cold": stats(refused_pages_cold),
            "maybe_pages_cold": stats(maybe_pages_cold),
            "batch_keys": batch.len(),
            "batch_passed": batch_passed,
            "batch_cold": stats(batch_cold),
            "batch_cold_major_faults": batch_cold_faults,
            "batch_pages_cold": stats(batch_pages_cold),
            "batch_warm_ns": batch_warm.as_nanos() as u64,
            "load_before": load_before,
            "load_after": load_average(),
        })
    );
    std::fs::remove_file(&path).expect("remove run");
}

/// Read `range` of `path` into the page cache.
fn read_into_cache(path: &Path, range: Range<u64>) {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path).expect("open run");
    file.seek(SeekFrom::Start(range.start)).expect("seek");
    let mut buf = vec![0u8; (range.end - range.start) as usize];
    file.read_exact(&mut buf).expect("read");
}

fn summary(label: &str, mut times: Vec<Duration>) {
    times.sort_unstable();
    let pick = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    println!(
        "{label}: n={} mean={:.1?} p50={:.1?} p90={:.1?} p99={:.1?} max={:.1?}",
        times.len(),
        mean,
        pick(0.5),
        pick(0.9),
        pick(0.99),
        times[times.len() - 1]
    );
}

/// A line of `/proc/self/status`, in bytes.
fn status_bytes(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("this bench requires /proc");
    let line = status
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .unwrap_or_else(|| panic!("/proc/self/status has no {field}"));
    let kb: u64 = line
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .expect("a kB count");
    kb * 1024
}

/// The run opened after its file's pages were dropped from the page cache, and how long the open
/// took.
fn cold_open(path: &Path) -> (KeyRun<u64>, Duration) {
    drop_cache(path);
    println!("  resident after drop: {} bytes", resident(path));
    let t = Instant::now();
    let run = KeyRun::<u64>::open(path).expect("open");
    (run, t.elapsed())
}

fn spill_arm(args: &Args, dir: &Path, arm: &str) {
    let n = args.entries;
    // `dense` is the integer ids of a corpus in an order unrelated to them: n distinct keys below
    // the next power of two, visited by an odd multiplier.
    let span = n.next_power_of_two();
    let key_of = |i: u64| match arm {
        "random" => mix(i),
        "sequential" => i,
        "dense" => i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(7) & (span - 1),
        other => panic!("--spill-arm is random, sequential or dense, not {other}"),
    };
    let scratch = dir.join(format!("scratch-{arm}"));
    let out = dir.join(format!("spilled-{arm}"));
    std::fs::create_dir_all(&scratch).expect("scratch");
    std::fs::create_dir_all(&out).expect("out");
    let t = Instant::now();
    let mut spill = KeySpill::<u64>::create(&scratch, args.budget_mib << 20).expect("spill");
    for i in 0..n {
        spill.push(key_of(i), i as u32).expect("push");
    }
    let pushed = t.elapsed();
    let anon_after_push = status_bytes("RssAnon:");
    let mut duplicates = 0u64;
    let writer = KeyRunWriter::create(&out, "run", NonZeroU64::new(250_000_000).expect("non-zero"))
        .with_key_filter();
    let runs = spill.finish(writer, |_| duplicates += 1).expect("finish");
    println!(
        "spill of {n} {arm} keys under {} MiB: push {pushed:.2?}, total {:.2?}, {} runs, \
         {duplicates} duplicate keys; peak RSS {} MiB, anonymous after push {} MiB",
        args.budget_mib,
        t.elapsed(),
        runs.len(),
        status_bytes("VmHWM:") >> 20,
        anon_after_push >> 20,
    );
    std::fs::remove_dir_all(&out).expect("remove");
}

fn main() {
    let args = Args::parse();
    assert!(
        args.entries <= u32::MAX as u64,
        "--entries is at most {}, since entities are u32",
        u32::MAX
    );
    if let Some(arm) = &args.spill_arm {
        spill_arm(&args, &args.dir, arm);
        return;
    }
    let dir = args
        .dir
        .join(format!("key-index-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let n = args.entries;
    let keys = Keys::new(args.keys, n);
    println!("entries: {n}, {:?} keys", args.keys);
    if !args.filters.is_empty() {
        for shape in &args.filters {
            filter_shape(&args, &dir, keys, shape);
        }
        std::fs::remove_dir_all(&dir).expect("remove dir");
        return;
    }

    let started = Instant::now();
    let mut writer =
        KeyRunWriter::<u64>::create(&dir, "run", NonZeroU64::new(u64::MAX).expect("non-zero"))
            .with_key_filter();
    for i in 0..n {
        writer
            .push(keys.key(i), mix(i ^ 0xABCD) as u32)
            .expect("push");
    }
    let runs = writer.finish().expect("finish");
    let write = started.elapsed();
    let path = runs[0].path.clone();
    let started = Instant::now();
    mosaica_store::fsync_written(std::slice::from_ref(&path)).expect("fsync");
    let fsync = started.elapsed();
    let size = std::fs::metadata(&path).expect("metadata").len();
    println!(
        "write: {write:.2?} ({:.1} M entries/s) then fsync {fsync:.2?}, file {size} bytes \
         ({:.2} bytes/entry)",
        n as f64 / write.as_secs_f64() / 1e6,
        size as f64 / n as f64
    );

    println!("open and single lookups:");
    let (run, open) = cold_open(&path);
    println!("  open (cold): {open:.2?}");
    let mut rng = StdRng::seed_from_u64(42);
    let picks: Vec<u64> = (0..args.lookups).map(|_| rng.gen_range(0..n)).collect();
    let probes: Vec<u64> = picks.iter().map(|&i| keys.key(i)).collect();
    let time_each = |keys: &[u64], present: bool| -> Vec<Duration> {
        keys.iter()
            .map(|&key| {
                let t = Instant::now();
                let found = run.get(key).expect("get");
                let e = t.elapsed();
                assert_eq!(found.len(), usize::from(present));
                e
            })
            .collect()
    };
    summary("  single lookup, entry page cold", time_each(&probes, true));
    summary("  single lookup, warm", time_each(&probes, true));
    let absent: Vec<u64> = picks.iter().map(|&i| keys.absent(i)).collect();
    summary(
        "  single lookup, absent key between two present, warm",
        time_each(&absent, false),
    );
    drop(run);

    for batch in [1_000usize, 100_000] {
        let mut batch_keys: Vec<u64> = (0..batch).map(|_| keys.key(rng.gen_range(0..n))).collect();
        batch_keys.sort_unstable();
        println!("batched lookup of {batch} sorted keys:");
        let (run, _) = cold_open(&path);
        let t = Instant::now();
        let hits = run.lookup_sorted(&batch_keys).expect("lookup");
        let cold = t.elapsed();
        // The fastest of five, since a warm batch is short enough for other load to move it.
        let warm = (0..5)
            .map(|_| {
                let t = Instant::now();
                run.lookup_sorted(&batch_keys).expect("lookup");
                t.elapsed()
            })
            .min()
            .expect("five runs");
        assert_eq!(hits.len(), batch);
        println!("  cold {cold:.2?}, warm {warm:.2?}");
    }

    println!("verify:");
    drop_cache(&path);
    println!("  resident after drop: {} bytes", resident(&path));
    let t = Instant::now();
    let check = verify_run(&path).expect("verify");
    println!("  cold {:.2?} over {} pages", t.elapsed(), check.pages);
    std::fs::remove_file(&path).expect("remove run");

    if args.spill {
        let exe = std::env::current_exe().expect("this bench re-runs itself per spill arm");
        for arm in ["random", "sequential", "dense"] {
            let status = std::process::Command::new(&exe)
                .arg("--dir")
                .arg(&dir)
                .arg("--entries")
                .arg(n.to_string())
                .arg("--budget-mib")
                .arg(args.budget_mib.to_string())
                .arg("--spill-arm")
                .arg(arm)
                .status()
                .expect("spawn spill arm");
            assert!(status.success(), "spill arm {arm} failed");
        }
    }

    std::fs::remove_dir_all(&dir).expect("remove dir");
}
