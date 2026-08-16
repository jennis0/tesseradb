//! **What does artifact membership cost resident, against the 794 MB it costs on disk?**
//!
//! `annotation-representation.md` §2 sizes membership at **794 MB** for 10⁷ artifacts over 10⁹
//! rows, and §11.3 marks the residency unmeasured in the same breath: *"794 MB is serialised bytes;
//! 10⁷ separately allocated bitmaps carry per-object overhead the campaign never measured, and the
//! figure multiplies by slices, by levels, and by two during a replace."* `artifact-delivery.md` §7
//! makes it the item that could refute the shape — if resident cost is materially worse than
//! serialised, the fine-level case stops being servable and the deleted assignment column (rep
//! §2.7) comes back for that regime.
//!
//! This measures it. It is a **residency** probe and deliberately not a throughput one: nothing
//! here is timed.
//!
//! # What is measured
//!
//! **Resident set size, with every bitmap live** — not `size_of`, not a sum of container capacities,
//! and not croaring's own accounting. The question is what the process costs the machine, so the
//! measurement is `VmHWM`-style RSS around a population that is all still reachable: allocator
//! per-chunk headers, size-class rounding and fragmentation across 10⁷ small allocations are the
//! overhead being asked about, and every structural estimate misses exactly those.
//!
//! Reported per arm and per scale:
//!
//! - **serialised** — `Portable` bytes, the quantity §2's 794 MB is in.
//! - **resident** — RSS delta from a baseline taken after warm-up, everything live.
//! - **ratio** — resident ÷ serialised. **This is the number the design owes.**
//! - **containers** and **bytes/container** — croaring's own structural accounting, so a bad ratio
//!   can be attributed to container overhead (croaring's problem, fixable by packing) rather than
//!   to allocation overhead (ours, fixable only by not allocating — i.e. by mapping).
//!
//! # Two arms, because membership shape drives everything
//!
//! §2 measures 0.61–1.03 B/member on the pessimistic synthetic arm and **0.006–0.073 on real
//! membership** — a 14–170× spread, because where the noise sits drives run count and the
//! generator places it adversarially. The 794 MB figure is on the pessimistic arm, so that arm is
//! the one to compare against; the realistic arm is here because the *ratio* may not be the same in
//! both, and the design would be sized from the wrong one.
//!
//! | arm | membership | why |
//! |---|---|---|
//! | `runs` | a few contiguous runs | what a spatial cluster is in row space after the signature sort — array containers become runs, and the per-bitmap floor dominates |
//! | `scattered` | uniform over the row space | §2's pessimistic arm, and the case where container count tracks member count |
//!
//! **Neither is a real clustering**, and this probe does not claim to be one. What it measures is
//! the *overhead* term, which is a property of how many bitmaps there are rather than of what is in
//! them — and that term is the one §11.3 says is unmeasured.
//!
//! # Usage
//!
//! ```text
//! cargo run --release --bin membership_residency -- [--max-artifacts N] [--members M]
//! ```
//!
//! Sweeps artifact counts by decade up to `--max-artifacts` (default 10⁶; the design's point is
//! 10⁷ and is reachable on a machine with the headroom — it is not the default because this probe
//! must not be the thing that fills a laptop). `--members` is the mean membership, defaulting to
//! **100**, which is §2's own `rows/artifacts` ratio at the 10⁹/10⁷ point the 794 MB comes from.

use croaring::{Bitmap, Portable};

/// The row space members are drawn from — §2's 10⁹ point. The space matters to container structure:
/// 100 members scattered over 10⁹ rows land in ~100 distinct containers, where the same 100 over a
/// 10⁶ space would share them.
const ROW_SPACE: u64 = 1_000_000_000;

/// §2's `rows/artifacts` at the 10⁹ rows / 10⁷ artifacts point the 794 MB figure is quoted from.
const DEFAULT_MEMBERS: u32 = 100;

const DEFAULT_MAX_ARTIFACTS: u64 = 1_000_000;

/// This process's resident set, in bytes, read from `/proc/self/statm`.
///
/// Field 2 is resident pages. Multiplied by the page size rather than an assumed 4 KiB — the
/// assumption is right on this platform and wrong on others, and a probe that silently reports
/// quarter-values on a 16 KiB-page host is worse than one that fails to build there.
fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("this probe requires /proc");
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .expect("/proc/self/statm field 2 is resident pages");
    // SAFETY: `sysconf` is a pure lookup with no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    pages * page
}

/// A deterministic 64-bit stream. Not for cryptography and not for statistics — for generating the
/// same population on every run, so two runs of this probe are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// A few contiguous runs — a spatial cluster's shape in row space.
    Runs,
    /// Uniform over the row space — §2's pessimistic arm.
    Scattered,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Runs => "runs",
            Arm::Scattered => "scattered",
        }
    }
}

/// One artifact's membership.
///
/// **Built through `Bitmap` exactly as the shipped path does**, including `run_optimize` on the
/// contiguous arm — the write path's serialisation runs it, so a probe that skipped it would
/// measure a representation nothing stores.
fn membership(arm: Arm, members: u32, runs: u32, rng: &mut Rng) -> Bitmap {
    let mut bitmap = Bitmap::new();
    match arm {
        Arm::Runs => {
            // `runs` contiguous stretches. **This is the parameter the whole cost turns on**, and
            // it is what the id space decides: a spatially coherent cluster is a handful of runs in
            // Morton-ranked row space, and one run per *signature group* it touches in
            // signature-ranked entity space (rep §2.1). Sweeping it is how a caller reads off the
            // cost of either form.
            let per_run = (members / runs.max(1)).max(1);
            for _ in 0..runs.max(1) {
                let start = rng.below(ROW_SPACE - per_run as u64);
                bitmap.add_range(start as u32..(start as u32).saturating_add(per_run));
            }
            bitmap.run_optimize();
        }
        Arm::Scattered => {
            for _ in 0..members {
                bitmap.add(rng.below(ROW_SPACE) as u32);
            }
        }
    }
    bitmap
}

/// Measure one `(arm, n)` and print its row. **One configuration per process** — see [`main`].
fn measure_one(arm: Arm, n: u64, members: u32, runs: u32) {
    let baseline = resident_bytes();
    let mut population: Vec<Bitmap> = Vec::with_capacity(n as usize);
    let mut rng = Rng(0x5EED);
    for _ in 0..n {
        population.push(membership(arm, members, runs, &mut rng));
    }
    let resident = resident_bytes().saturating_sub(baseline);

    // Read *after* the RSS sample, so serialisation's own scratch is not counted into it.
    let serialised: u64 = population
        .iter()
        .map(|b| b.get_serialized_size_in_bytes::<Portable>() as u64)
        .sum();
    let containers: u64 = population
        .iter()
        .map(|b| b.statistics().n_containers as u64)
        .sum();

    let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    println!(
        "{:<10} {:>6} {:>12} {:>14.1} {:>14.1} {:>8.2} {:>14} {:>10.1}",
        arm.name(),
        if arm == Arm::Runs { runs.to_string() } else { "—".to_string() },
        n,
        mb(serialised),
        mb(resident),
        resident as f64 / serialised as f64,
        containers,
        resident as f64 / containers.max(1) as f64,
    );

    // Held to here so the RSS reading above cannot have been taken over a freed set.
    std::hint::black_box(&population);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str, default: u64| -> u64 {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let members = flag("--members", DEFAULT_MEMBERS as u64) as u32;

    // The child form: one configuration, one row, then exit.
    if let Some(i) = args.iter().position(|a| a == "--arm") {
        let arm = match args.get(i + 1).map(String::as_str) {
            Some("runs") => Arm::Runs,
            Some("scattered") => Arm::Scattered,
            other => panic!("--arm takes runs|scattered, got {other:?}"),
        };
        measure_one(
            arm,
            flag("--artifacts", 10_000),
            members,
            flag("--runs", 4) as u32,
        );
        return;
    }

    let max_artifacts = flag("--max-artifacts", DEFAULT_MAX_ARTIFACTS);

    println!("# membership residency");
    println!(
        "# row space {ROW_SPACE}, mean members {members}, arms: runs (realistic), scattered \
         (rep §2's pessimistic arm)"
    );
    println!(
        "# resident is RSS delta with every bitmap live — allocator overhead and fragmentation \
         included, which is the term rep §11.3 says is unmeasured"
    );
    println!();
    println!(
        "{:<10} {:>6} {:>12} {:>14} {:>14} {:>8} {:>14} {:>10}",
        "arm", "runs", "artifacts", "serialised MB", "resident MB", "ratio", "containers", "B/cont"
    );

    // **One configuration per process, and this is a correction rather than tidiness.** A first
    // revision swept in-process behind a fixed warm-up, and the second arm's small scales read
    // *zero* resident bytes: glibc had already grown the arena for the first arm, so the population
    // fit inside memory the baseline had already counted. An RSS delta is only meaningful against
    // an allocator that has not already been handed the space, which a fresh process is the only
    // cheap way to guarantee.
    let exe = std::env::current_exe().expect("this probe re-runs itself per configuration");
    let runs = flag("--runs", 4) as u32;
    for arm in ["runs", "scattered"] {
        let mut n = 10_000u64;
        while n <= max_artifacts {
            let status = std::process::Command::new(&exe)
                .args([
                    "--arm",
                    arm,
                    "--artifacts",
                    &n.to_string(),
                    "--members",
                    &members.to_string(),
                    "--runs",
                    &runs.to_string(),
                ])
                .status()
                .expect("child measurement");
            if !status.success() {
                // A child that dies is almost always the OOM killer at the top of a sweep, which is
                // itself a result: say so rather than printing a row that is not a measurement.
                println!(
                    "{:<10} {:>6} {:>12}   — child exited {status}; at this scale the population \
                     does not fit in this machine's memory",
                    arm, runs, n
                );
                break;
            }
            n *= 10;
        }
    }
}
