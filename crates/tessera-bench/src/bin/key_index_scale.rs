//! **What a key index run costs at scale**: write time and file size for sorted `u64` entries,
//! the cost of opening the run, single lookups with the entry pages out of the page cache and in
//! it, batched lookups, the full verification, and optionally the spill sort of the same number
//! of entries arriving in random and in sequential key order.
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
//! ```text
//! cargo run --release -p tessera-bench --bin key_index_scale -- --dir <scratch> [--entries N] [--spill]
//! ```

use std::fs::File;
use std::num::NonZeroU64;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_store::key_index::{verify_run, KeyRun, KeyRunWriter, KeySpill};

#[derive(Parser)]
#[command(about = "key index run write, open, lookup and spill costs at scale")]
struct Args {
    /// Directory to write the run under; a subdirectory is created and removed.
    #[arg(long)]
    dir: PathBuf,
    #[arg(long, default_value_t = 100_000_000)]
    entries: u64,
    /// Single lookups timed in each of the cold and warm passes.
    #[arg(long, default_value_t = 2_000)]
    lookups: usize,
    /// Also time the spill sort over the same number of entries.
    #[arg(long)]
    spill: bool,
    /// The spill's memory budget in MiB.
    #[arg(long, default_value_t = 1024)]
    budget_mib: usize,
    /// Run one spill arm, `random` or `sequential`, and nothing else.
    #[arg(long, hide = true)]
    spill_arm: Option<String>,
}

/// The `i`th key: ascending, in `[16i, 16i + 8)`, so `key_of(i) + 8` is never a key.
fn key_of(i: u64) -> u64 {
    i * 16 + (mix(i) % 8)
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
    let file = File::open(path).expect("open run");
    let len = file.metadata().expect("metadata").len() as usize;
    // SAFETY: read-only mapping of a file nothing modifies during the call.
    let map = unsafe { memmap2::Mmap::map(&file) }.expect("map");
    let page = 4096usize;
    let mut vec = vec![0u8; len.div_ceil(page)];
    // SAFETY: `vec` has one byte per page of the mapping.
    let code = unsafe { libc::mincore(map.as_ptr() as *mut libc::c_void, len, vec.as_mut_ptr()) };
    assert_eq!(code, 0, "mincore failed");
    vec.iter().filter(|&&b| b & 1 == 1).count() as u64 * page as u64
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
    let sequential = match arm {
        "random" => false,
        "sequential" => true,
        other => panic!("--spill-arm is random or sequential, not {other}"),
    };
    let scratch = dir.join(format!("scratch-{arm}"));
    let out = dir.join(format!("spilled-{arm}"));
    std::fs::create_dir_all(&scratch).expect("scratch");
    std::fs::create_dir_all(&out).expect("out");
    let t = Instant::now();
    let mut spill = KeySpill::<u64>::create(&scratch, args.budget_mib << 20).expect("spill");
    for i in 0..n {
        let key = if sequential { i } else { mix(i) };
        spill.push(key, i as u32).expect("push");
    }
    let pushed = t.elapsed();
    let anon_after_push = status_bytes("RssAnon:");
    let mut duplicates = 0u64;
    let runs = spill
        .finish(
            &out,
            "run",
            NonZeroU64::new(250_000_000).expect("non-zero"),
            |_| duplicates += 1,
        )
        .expect("finish");
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
    println!("entries: {n}");

    let started = Instant::now();
    let mut writer =
        KeyRunWriter::<u64>::create(&dir, "run", NonZeroU64::new(u64::MAX).expect("non-zero"));
    for i in 0..n {
        writer
            .push(key_of(i), mix(i ^ 0xABCD) as u32)
            .expect("push");
    }
    let runs = writer.finish().expect("finish");
    let write = started.elapsed();
    let path = runs[0].path.clone();
    let started = Instant::now();
    tessera_store::fsync_written(std::slice::from_ref(&path)).expect("fsync");
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
    let probes: Vec<u64> = (0..args.lookups)
        .map(|_| key_of(rng.gen_range(0..n)))
        .collect();
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
    let absent: Vec<u64> = probes.iter().map(|&k| k + 8).collect();
    summary(
        "  single lookup, absent key between two present, warm",
        time_each(&absent, false),
    );
    drop(run);

    for batch in [1_000usize, 100_000] {
        let mut keys: Vec<u64> = (0..batch).map(|_| key_of(rng.gen_range(0..n))).collect();
        keys.sort_unstable();
        println!("batched lookup of {batch} sorted keys:");
        let (run, _) = cold_open(&path);
        let t = Instant::now();
        let hits = run.lookup_sorted(&keys).expect("lookup");
        let cold = t.elapsed();
        // The fastest of five, since a warm batch is short enough for other load to move it.
        let warm = (0..5)
            .map(|_| {
                let t = Instant::now();
                run.lookup_sorted(&keys).expect("lookup");
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
        for arm in ["random", "sequential"] {
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
