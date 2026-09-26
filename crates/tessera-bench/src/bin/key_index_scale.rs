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
//! `--rewrite <dir>` instead reads every format 1 run (the fixed-width format, twelve bytes an
//! entry) of `u64` keys in `<dir>` once, front to back with `O_DIRECT` so that the read neither
//! fills nor empties the page cache, and writes each into a run of the current format under
//! `--dir`. It prints the old and new sizes, the size a width per block of 64 gaps would give
//! instead of one width per page, and cold and warm single and batched lookups over the new runs
//! as the base runs of one index; then it removes them.
//!
//! Each spill arm runs in a child process of its own, so its peak resident set (`VmHWM`) and its
//! anonymous memory (`RssAnon`) are the spill's and not the lookups' mappings.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin key_index_scale -- --dir <scratch> [--entries N] [--keys dense|random] [--spill]
//! cargo run --release -p tessera-bench --bin key_index_scale -- --dir <scratch> --rewrite <dir of format 1 runs>
//! ```

use std::fs::File;
use std::io::Read;
use std::num::NonZeroU64;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_store::key_index::{verify_run, KeyIndexView, KeyRun, KeyRunWriter, KeySpill};

#[derive(Parser)]
#[command(about = "key index run write, open, lookup and spill costs at scale")]
struct Args {
    /// Directory to write the run under; a subdirectory is created and removed.
    #[arg(long)]
    dir: PathBuf,
    #[arg(long, default_value_t = 100_000_000)]
    entries: u64,
    /// The keys: `dense` or `random` (see the module doc).
    #[arg(long, default_value = "dense")]
    keys: String,
    /// Single lookups timed in each of the cold and warm passes.
    #[arg(long, default_value_t = 2_000)]
    lookups: usize,
    /// Also time the spill sort over the same number of entries.
    #[arg(long)]
    spill: bool,
    /// The spill's memory budget in MiB.
    #[arg(long, default_value_t = 1024)]
    budget_mib: usize,
    /// Rewrite the format 1 `u64` runs in this directory into the current format and time
    /// lookups over them, instead of the synthetic run.
    #[arg(long)]
    rewrite: Option<PathBuf>,
    /// Run one spill arm, `random` or `sequential`, and nothing else.
    #[arg(long, hide = true)]
    spill_arm: Option<String>,
}

/// The keys of a run of `n` entries: the `i`th key, and a key between it and the next that is
/// never a key.
#[derive(Clone, Copy)]
enum Keys {
    Dense,
    Random { stride: u64 },
}

impl Keys {
    fn new(name: &str, n: u64) -> Self {
        match name {
            "dense" => Keys::Dense,
            "random" => Keys::Random {
                stride: u64::MAX / n.max(1),
            },
            other => panic!("--keys is dense or random, not {other}"),
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
    if let Some(from) = &args.rewrite {
        rewrite(&args, from);
        return;
    }
    let dir = args
        .dir
        .join(format!("key-index-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let n = args.entries;
    let keys = Keys::new(&args.keys, n);
    println!("entries: {n}, {} keys", args.keys);

    let started = Instant::now();
    let mut writer =
        KeyRunWriter::<u64>::create(&dir, "run", NonZeroU64::new(u64::MAX).expect("non-zero"));
    for i in 0..n {
        writer
            .push(keys.key(i), mix(i ^ 0xABCD) as u32)
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

/// Every `(key, entity)` of a format 1 run of `u64` keys to `visit`, reading the file once with
/// `O_DIRECT`. Returns the file's length.
fn read_format_1(path: &Path, mut visit: impl FnMut(u64, u32)) -> u64 {
    const CHUNK: usize = 8 << 20;
    const PAGE: usize = 4096;
    const PER_PAGE: u64 = 341;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .expect("open a format 1 run");
    let len = file.metadata().expect("metadata").len();
    let mut backing = vec![0u8; CHUNK + PAGE];
    let skip = backing.as_ptr().align_offset(PAGE);
    let buf = &mut backing[skip..skip + CHUNK];
    let (mut entries, mut pages) = (0u64, 0u64);
    let mut page_no = 0u64;
    let mut seen = 0u64;
    loop {
        let mut filled = 0;
        while filled < CHUNK {
            let n = file.read(&mut buf[filled..]).expect("read");
            if n == 0 {
                break;
            }
            filled += n;
        }
        for page in buf[..filled].chunks_exact(PAGE) {
            if page_no == 0 {
                assert_eq!(
                    &page[0..8],
                    b"TSKEYRUN",
                    "{}: not a key run",
                    path.display()
                );
                let version = u32::from_le_bytes(page[8..12].try_into().unwrap());
                let width = u32::from_le_bytes(page[12..16].try_into().unwrap());
                assert_eq!((version, width), (1, 8), "{}", path.display());
                entries = u64::from_le_bytes(page[16..24].try_into().unwrap());
                pages = u64::from_le_bytes(page[24..32].try_into().unwrap());
            } else if page_no <= pages {
                let in_page = (entries - (page_no - 1) * PER_PAGE).min(PER_PAGE) as usize;
                for e in page[..in_page * 12].chunks_exact(12) {
                    visit(
                        u64::from_le_bytes(e[..8].try_into().unwrap()),
                        u32::from_le_bytes(e[8..].try_into().unwrap()),
                    );
                }
                seen += in_page as u64;
            }
            page_no += 1;
        }
        if filled < CHUNK {
            break;
        }
    }
    assert_eq!(seen, entries, "{}: entries read", path.display());
    len
}

/// The size of a run packed with one gap width per block of `block` gaps, each block's width in a
/// byte before it, and otherwise as the current format packs: a page fills until the next entry
/// does not fit.
struct BlockSim {
    block: usize,
    pages: u64,
    entries: u64,
    in_page: usize,
    last: u64,
    closed: usize,
    open: usize,
    open_bits: u32,
}

impl BlockSim {
    fn new(block: usize) -> Self {
        BlockSim {
            block,
            pages: 0,
            entries: 0,
            in_page: 0,
            last: 0,
            closed: 0,
            open: 0,
            open_bits: 0,
        }
    }

    fn cost(&self, n: usize, closed: usize, open: usize, bits: u32) -> usize {
        let open_bytes = if open > 0 {
            1 + (open * bits as usize).div_ceil(8)
        } else {
            0
        };
        3 + 8 + closed + open_bytes + 4 * n
    }

    fn push(&mut self, key: u64) {
        self.entries += 1;
        if self.in_page > 0 {
            let bits = 64 - (key - self.last).leading_zeros();
            let (mut closed, mut open, mut open_bits) = (self.closed, self.open, self.open_bits);
            if open == self.block {
                closed += 1 + (open * open_bits as usize).div_ceil(8);
                open = 0;
                open_bits = 0;
            }
            open_bits = open_bits.max(bits);
            open += 1;
            if self.cost(self.in_page + 1, closed, open, open_bits) <= 4092 {
                (self.closed, self.open, self.open_bits) = (closed, open, open_bits);
                self.in_page += 1;
                self.last = key;
                return;
            }
        }
        self.pages += u64::from(self.in_page > 0);
        (self.in_page, self.closed, self.open, self.open_bits) = (1, 0, 0, 0);
        self.last = key;
    }

    fn file_bytes(&self) -> u64 {
        let pages = self.pages + u64::from(self.in_page > 0);
        (1 + pages) * 4096 + pages * 8 + 4
    }
}

fn rewrite(args: &Args, from: &Path) {
    let mut inputs: Vec<PathBuf> = std::fs::read_dir(from)
        .expect("read the runs' directory")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "keys"))
        .collect();
    inputs.sort();
    let dir = args
        .dir
        .join(format!("key-index-rewrite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let (mut old_bytes, mut new_bytes, mut entries) = (0u64, 0u64, 0u64);
    let mut sim = BlockSim::new(64);
    let mut sim_bytes = 0u64;
    let mut present = Vec::new();
    let mut absent = Vec::new();
    let mut outputs = Vec::new();
    let started = Instant::now();
    for (i, input) in inputs.iter().enumerate() {
        let mut writer = KeyRunWriter::<u64>::create(
            &dir,
            &format!("base-{i}"),
            NonZeroU64::new(u64::MAX).expect("non-zero"),
        );
        let mut pending: Option<u64> = None;
        old_bytes += read_format_1(input, |key, entity| {
            if let Some(k) = pending.take() {
                if key > k + 1 {
                    absent.push(k + 1);
                }
            }
            if entries % 16_384 == 0 {
                present.push(key);
                pending = Some(key);
            }
            entries += 1;
            sim.push(key);
            writer.push(key, entity).expect("push");
        });
        sim_bytes += sim.file_bytes();
        sim = BlockSim::new(64);
        let runs = writer.finish().expect("finish");
        for run in runs {
            tessera_store::fsync_written(std::slice::from_ref(&run.path)).expect("fsync");
            drop_cache(&run.path);
            new_bytes += std::fs::metadata(&run.path).expect("metadata").len();
            outputs.push(run);
        }
    }
    let elapsed = started.elapsed();
    let gb = |b: u64| b as f64 / 1e9;
    println!(
        "rewrote {} runs, {entries} entries, in {elapsed:.2?}: format 1 {:.2} GB ({:.3} B/entry), \
         current {:.2} GB ({:.3} B/entry); a width per 64 gaps would be {:.2} GB ({:.3} B/entry)",
        outputs.len(),
        gb(old_bytes),
        old_bytes as f64 / entries as f64,
        gb(new_bytes),
        new_bytes as f64 / entries as f64,
        gb(sim_bytes),
        sim_bytes as f64 / entries as f64,
    );

    outputs.sort_by_key(|r| r.min_key);
    let open_view = || {
        let base = outputs
            .iter()
            .map(|r| {
                drop_cache(&r.path);
                Arc::new(KeyRun::<u64>::open(&r.path).expect("open"))
            })
            .collect();
        KeyIndexView::new(vec![], base).expect("view")
    };
    let mut rng = StdRng::seed_from_u64(42);
    let pick = |rng: &mut StdRng, from: &[u64], n: usize| -> Vec<u64> {
        (0..n).map(|_| from[rng.gen_range(0..from.len())]).collect()
    };
    let view = open_view();
    let probes = pick(&mut rng, &present, args.lookups);
    let time_each = |keys: &[u64], present: bool| -> Vec<Duration> {
        keys.iter()
            .map(|&key| {
                let t = Instant::now();
                let found = view.get(key).expect("get");
                let e = t.elapsed();
                assert_eq!(found.len(), usize::from(present));
                e
            })
            .collect()
    };
    summary("  single lookup, entry page cold", time_each(&probes, true));
    summary("  single lookup, warm", time_each(&probes, true));
    let gaps = pick(&mut rng, &absent, args.lookups);
    summary(
        "  single lookup, absent key, entry page cold",
        time_each(&gaps, false),
    );
    drop(view);
    for batch in [1_000usize, 100_000] {
        let mut keys = pick(&mut rng, &present, batch);
        keys.sort_unstable();
        let view = open_view();
        let t = Instant::now();
        let hits = view.lookup_sorted(&keys).expect("lookup");
        let cold = t.elapsed();
        let warm = (0..5)
            .map(|_| {
                let t = Instant::now();
                view.lookup_sorted(&keys).expect("lookup");
                t.elapsed()
            })
            .min()
            .expect("five runs");
        assert_eq!(hits.len(), batch);
        println!("batched lookup of {batch} sorted keys: cold {cold:.2?}, warm {warm:.2?}");
    }
    std::fs::remove_dir_all(&dir).expect("remove dir");
}
