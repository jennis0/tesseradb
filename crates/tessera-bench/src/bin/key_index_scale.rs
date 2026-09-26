//! **What a key index run costs at scale**: write time and file size for sorted `u64` entries,
//! the cost of opening the run, single lookups with the entry pages out of the page cache and in
//! it, a batched lookup, the full verification, and optionally the spill sort of the same number
//! of entries arriving in random and in sequential key order.
//!
//! "Cold" means the run file's pages were dropped from this machine's page cache with
//! `posix_fadvise(POSIX_FADV_DONTNEED)` before the open, which needs no privilege. Under WSL2 or
//! a VM the host may still hold the bytes, so a cold read there is a read from the host's cache,
//! not necessarily from the disk. The binary prints how much of the file was resident after the
//! drop so the figure can be judged.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin key_index_scale -- --dir <scratch> [--entries N] [--spill]
//! ```

use std::fs::File;
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

fn main() {
    let args = Args::parse();
    let dir = args
        .dir
        .join(format!("key-index-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let n = args.entries;
    println!("entries: {n}");

    let started = Instant::now();
    let mut writer = KeyRunWriter::<u64>::create(&dir, "run", u64::MAX);
    for i in 0..n {
        writer
            .push(key_of(i), mix(i ^ 0xABCD) as u32)
            .expect("push");
    }
    let runs = writer.finish().expect("finish");
    let write = started.elapsed();
    let path = runs[0].path.clone();
    let size = std::fs::metadata(&path).expect("metadata").len();
    println!(
        "write: {write:.2?} ({:.1} M entries/s), file {size} bytes ({:.2} bytes/entry)",
        n as f64 / write.as_secs_f64() / 1e6,
        size as f64 / n as f64
    );

    drop_cache(&path);
    println!("resident after drop: {} bytes of {size}", resident(&path));
    let started = Instant::now();
    let run = KeyRun::<u64>::open(&path).expect("open");
    println!("open (cold): {:.2?}", started.elapsed());

    let mut rng = StdRng::seed_from_u64(42);
    let probes: Vec<u64> = (0..args.lookups)
        .map(|_| key_of(rng.gen_range(0..n)))
        .collect();
    let mut cold = Vec::with_capacity(probes.len());
    for &key in &probes {
        let t = Instant::now();
        let found = run.get(key).expect("get");
        cold.push(t.elapsed());
        assert_eq!(found.len(), 1);
    }
    summary("single lookup, entry page cold", cold);
    let mut warm = Vec::with_capacity(probes.len());
    for &key in &probes {
        let t = Instant::now();
        let found = run.get(key).expect("get");
        warm.push(t.elapsed());
        assert_eq!(found.len(), 1);
    }
    summary("single lookup, warm", warm);
    let absent: Vec<Duration> = probes
        .iter()
        .map(|&key| {
            let t = Instant::now();
            let found = run.get(key + 8).expect("get");
            let e = t.elapsed();
            assert!(found.is_empty());
            e
        })
        .collect();
    summary(
        "single lookup, absent key between two present, warm",
        absent,
    );

    for batch in [1_000usize, 100_000] {
        let mut keys: Vec<u64> = (0..batch).map(|_| key_of(rng.gen_range(0..n))).collect();
        keys.sort_unstable();
        drop_cache(&path);
        let t = Instant::now();
        let hits = run.lookup_sorted(&keys).expect("lookup");
        let cold = t.elapsed();
        let t = Instant::now();
        run.lookup_sorted(&keys).expect("lookup");
        let warm = t.elapsed();
        assert_eq!(hits.len(), batch);
        println!("batched lookup of {batch} sorted keys: cold {cold:.2?}, warm {warm:.2?}");
    }

    drop_cache(&path);
    let t = Instant::now();
    let check = verify_run(&path).expect("verify");
    println!(
        "verify (cold): {:.2?} over {} pages",
        t.elapsed(),
        check.pages
    );
    drop(run);

    if args.spill {
        for (label, sequential) in [("random", false), ("sequential", true)] {
            let scratch = dir.join(format!("scratch-{label}"));
            let out = dir.join(format!("spilled-{label}"));
            std::fs::create_dir_all(&scratch).expect("scratch");
            std::fs::create_dir_all(&out).expect("out");
            let t = Instant::now();
            let mut spill =
                KeySpill::<u64>::create(&scratch, args.budget_mib << 20).expect("spill");
            for i in 0..n {
                let key = if sequential { i } else { mix(i) };
                spill.push(key, i as u32).expect("push");
            }
            let pushed = t.elapsed();
            let mut duplicates = 0u64;
            let runs = spill
                .finish(&out, "run", 250_000_000, |_, _| duplicates += 1)
                .expect("finish");
            println!(
                "spill of {n} {label} keys under {} MiB: push {pushed:.2?}, total {:.2?}, {} runs, {duplicates} duplicate keys",
                args.budget_mib,
                t.elapsed(),
                runs.len()
            );
            std::fs::remove_dir_all(&out).expect("remove");
        }
    }

    std::fs::remove_dir_all(&dir).expect("remove dir");
}
