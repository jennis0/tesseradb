//! **What does the fold's artifact pass cost, and which of the two constructions is cheaper?**
//!
//! `annotation-representation.md` §5.0.3 poses the pass as a choice between *riding pass 1* with the
//! inverted entity→artifacts multimap resident, and *per-artifact translation* through a table, and
//! marks the comparison unmeasured. `artifact-delivery.md` §6 makes it the largest unpriced item
//! left and §7 says it decides whether `plan_fold`'s ~9–10 GB anonymous peak moves. This is that
//! measurement.
//!
//! # The posing is corrected here, and the correction is the first result
//!
//! §5.0.3's per-artifact arm translates `old_row → new_row` through a table it says must be
//! scatter-built — a second 4 GB scatter beside `permutation.bin`'s. **There is no such table and no
//! such scatter**, because membership never lives in row space on disk: the durable form is
//! entity-canonical (rep §2.4, write-cycle §4.1) and the row form is derived from it by
//! `ArtifactRows::build`. So the per-artifact arm is `Permutation::project` of an entity-space
//! bitmap through the **new** `permutation.bin` — the file pass 1 writes anyway, entity→new_row,
//! mapped — and old rows never enter the question. That is what this probe measures, and it is why
//! the arm is called `project` rather than `translate`.
//!
//! The ride-pass-1 arm is unaffected by the correction: it still needs the inverted relation
//! resident, because it consumes rows in new-row order and must find the artifacts holding each
//! entity as it goes.
//!
//! # What is measured
//!
//! Per arm, per scale: **wall time for the whole pass** (every artifact's row form rebuilt, in the
//! shape the fold would have to produce them) and **RSS at the peak**, with a note on where those
//! bytes live — the ride arm's are anonymous, the project arm's are the page cache behind a mapping
//! `plan_fold` is already budgeting for.
//!
//! Both arms are checked to produce **identical** bitmaps under `--verify`. Two constructions of one
//! quantity that disagree would make the timing meaningless, and the check is cheap at small scale.
//!
//! # The two shapes of entity space, because they bracket the answer
//!
//! The project arm's cost is dominated by where its reads land in the mapped permutation, which is
//! decided by how an artifact's members sit in **entity** space:
//!
//! | `--entity-order` | what it models | effect on the project arm |
//! |---|---|---|
//! | `morton` | the shipped `(signature, morton)` allocation, one signature group: entity rank tracks Morton rank, so a spatial cluster is a few runs in entity space *and* its slots are a neighbourhood of the permutation | sequential-ish reads |
//! | `issue` | entity ids issued in arrival order, uncorrelated with Morton rank — `permutation.rs`'s own stated assumption | one cold 4-byte read per member |
//!
//! Neither is a claim about a real deployment; they bracket it. A corpus spread over many signature
//! groups sits between them.
//!
//! # Usage
//!
//! ```text
//! cargo run --release --bin fold_artifact_pass -- [--rows N] [--artifacts N] [--members M]
//!                                                 [--runs R] [--deleted-fraction F]
//!                                                 [--entity-order morton|issue] [--verify]
//! ```
//!
//! Sweeps the scales below `--rows` by decade. One configuration per process, for the reason
//! `membership_residency` records: an RSS delta is only meaningful against an allocator that has not
//! already been handed the space.

use std::path::Path;
use std::time::Instant;

use croaring::Bitmap;

use tessera_store::permutation::Permutation;
use tessera_store::write::write_permutation_iter;
use tessera_types::EntityId;

/// Rows per artifact — §2's `rows/artifacts` at the 10⁹/10⁷ point the design's sizing comes from.
const DEFAULT_MEMBERS: u32 = 100;

/// Runs per artifact in entity space. Four is `membership_residency`'s realistic arm: a spatially
/// coherent cluster is a handful of contiguous stretches, not a scatter.
const DEFAULT_RUNS: u32 = 4;

/// What a fold typically retires, as a fraction of rows. The pass's cost is insensitive to it — it
/// is here because a pass measured against a permutation with no holes is measuring a copy.
const DEFAULT_DELETED: f64 = 0.02;

/// This process's resident set high-water mark, in bytes, from `/proc/self/status`.
///
/// The **peak**, not the current reading: the ride arm frees its multimap before the last builder is
/// serialised, and a probe reporting the trough would report the wrong number for a budget.
fn peak_resident_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("this probe requires /proc");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .expect("VmHWM is a kB count");
            return kb * 1024;
        }
    }
    panic!("/proc/self/status has no VmHWM");
}

/// A deterministic 64-bit stream — splitmix64, so two runs of this probe are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
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
enum EntityOrder {
    /// Entity rank tracks Morton rank — the shipped `(signature, morton)` allocation.
    Morton,
    /// Entity ids uncorrelated with Morton rank.
    Issue,
}

/// The population: entity-space memberships, and the new row order pass 1 emits.
struct Population {
    /// One entity-space bitmap per artifact, exactly the durable form.
    members: Vec<Bitmap>,
    /// Surviving entities in **new-row order** — what pass 1 streams, and what
    /// `permutation.bin` is written from.
    row_order: Vec<EntityId>,
}

/// Build the population. **Not timed** — this is the fold's input, not its work.
fn population(
    rows: u64,
    artifacts: u64,
    members: u32,
    runs: u32,
    deleted: f64,
    order: EntityOrder,
) -> Population {
    let mut rng = Rng(0x5EED);

    // Memberships in entity space: `runs` contiguous stretches each.
    let per_run = (members / runs.max(1)).max(1);
    let mut sets = Vec::with_capacity(artifacts as usize);
    for _ in 0..artifacts {
        let mut bitmap = Bitmap::new();
        for _ in 0..runs.max(1) {
            let start = rng.below(rows - per_run as u64) as u32;
            bitmap.add_range(start..start.saturating_add(per_run));
        }
        bitmap.run_optimize();
        sets.push(bitmap);
    }

    // Survivors, in the order pass 1 emits them.
    let mut row_order: Vec<EntityId> = Vec::with_capacity(rows as usize);
    let keep_below = ((1.0 - deleted) * u64::MAX as f64) as u64;
    for entity in 0..rows {
        if rng.next() < keep_below {
            row_order.push(EntityId::new(entity));
        }
    }
    if order == EntityOrder::Issue {
        // Fisher–Yates: entity rank no longer tracks row rank, so the project arm's reads scatter.
        for i in (1..row_order.len()).rev() {
            let j = rng.below(i as u64 + 1) as usize;
            row_order.swap(i, j);
        }
    }

    Population {
        members: sets,
        row_order,
    }
}

/// **Arm `project`** — per artifact, project its entity-space membership through the new mapped
/// permutation. This is `ArtifactRows::build`'s own construction, run inside the fold.
///
/// Resident cost is one level's output plus whatever of the mapping the reads touch; the mapping
/// itself is page cache the fold already pays for, because pass 1 has just written it.
/// **The arm parallelises and the ride arm cannot**, which is why `--threads` lives here and has no
/// counterpart: every artifact is an independent read of a shared read-only mapping, where riding
/// pass 1 is one sequential consumer of one stream. Chunked rather than work-stolen because the
/// artifacts are near-uniform in size and this probe must not measure a scheduler.
fn arm_project(permutation: &Path, members: &[Bitmap], threads: usize) -> (f64, Vec<Bitmap>) {
    let perm = Permutation::load(permutation).expect("the permutation this fold just wrote");
    let start = Instant::now();
    if threads <= 1 {
        let mut out = Vec::with_capacity(members.len());
        for set in members {
            let mut rows = perm.project(set);
            rows.run_optimize();
            out.push(rows);
        }
        return (start.elapsed().as_secs_f64(), out);
    }
    let chunk = members.len().div_ceil(threads);
    let perm = &perm;
    let parts: Vec<Vec<Bitmap>> = std::thread::scope(|scope| {
        let handles: Vec<_> = members
            .chunks(chunk.max(1))
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|set| {
                            let mut rows = perm.project(set);
                            rows.run_optimize();
                            rows
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let seconds = start.elapsed().as_secs_f64();
    (seconds, parts.into_iter().flatten().collect())
}

/// **Arm `ride`** — hold the inverted entity→artifacts relation resident, then consume pass 1's
/// `(entity, new_row)` stream in new-row order, appending to every builder as the rows go past.
///
/// The inversion is CSR — offsets over entity space plus one `u32` per (entity, artifact) pair —
/// which is the cheapest shape the multimap has and the one §5.0.3's ~4 GB at 10⁹ is quoted from.
/// **Its construction is timed**, because the fold would have to build it: nothing else in the fold
/// holds this relation.
fn arm_ride(rows: u64, row_order: &[EntityId], members: &[Bitmap]) -> (f64, Vec<Bitmap>) {
    let start = Instant::now();

    // CSR by counting sort. `entity` is bounded by `rows`, so the offset array is one `u32` per
    // entity slot — the same 4 GB shape at 10⁹ as `permutation.bin`, but anonymous.
    let mut counts = vec![0u32; rows as usize + 1];
    for set in members {
        for entity in set.iter() {
            counts[entity as usize + 1] += 1;
        }
    }
    for i in 1..counts.len() {
        counts[i] += counts[i - 1];
    }
    let pairs = counts[counts.len() - 1] as usize;
    let mut values = vec![0u32; pairs];
    let mut cursor = counts.clone();
    for (ordinal, set) in members.iter().enumerate() {
        for entity in set.iter() {
            let slot = &mut cursor[entity as usize];
            values[*slot as usize] = ordinal as u32;
            *slot += 1;
        }
    }
    drop(cursor);

    // Every builder in the level open at once, which is the arm's other resident term.
    let mut builders: Vec<Vec<u32>> = vec![Vec::new(); members.len()];
    for (new_row, &entity) in row_order.iter().enumerate() {
        let slot = entity.raw() as usize;
        let lo = counts[slot] as usize;
        let hi = counts[slot + 1] as usize;
        for &ordinal in &values[lo..hi] {
            builders[ordinal as usize].push(new_row as u32);
        }
    }
    drop(counts);
    drop(values);

    // The rows arrived ascending, so each builder is already sorted — which is the arm's claim.
    let out: Vec<Bitmap> = builders
        .into_iter()
        .map(|rows| {
            let mut bitmap = Bitmap::of(&rows);
            bitmap.run_optimize();
            bitmap
        })
        .collect();
    (start.elapsed().as_secs_f64(), out)
}

/// Measure one configuration and print its row. **One configuration per process** — see [`main`].
#[allow(clippy::too_many_arguments)]
fn measure_one(
    arm: &str,
    rows: u64,
    artifacts: u64,
    members: u32,
    runs: u32,
    deleted: f64,
    order: EntityOrder,
    threads: usize,
    verify: bool,
) {
    let pop = population(rows, artifacts, members, runs, deleted, order);

    let dir = std::env::temp_dir().join(format!("fold-artifact-pass-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let permutation = dir.join("permutation.bin");
    write_permutation_iter(&permutation, pop.row_order.iter().copied(), rows)
        .expect("writing the permutation this fold would have written");

    let before = peak_resident_bytes();
    let (seconds, out) = match arm {
        "project" => arm_project(&permutation, &pop.members, threads),
        "ride" => arm_ride(rows, &pop.row_order, &pop.members),
        other => panic!("--arm takes project|ride, got {other:?}"),
    };
    let peak = peak_resident_bytes();

    if verify {
        let (_, other) = if arm == "project" {
            arm_ride(rows, &pop.row_order, &pop.members)
        } else {
            arm_project(&permutation, &pop.members, 1)
        };
        assert_eq!(out.len(), other.len(), "the arms disagree on artifact count");
        for (i, (a, b)) in out.iter().zip(other.iter()).enumerate() {
            assert_eq!(a, b, "the two constructions disagree at artifact {i}");
        }
        eprintln!("# verified: the two constructions agree on every artifact");
    }

    let cardinality: u64 = out.iter().map(|b| b.cardinality()).sum();
    let gb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    println!(
        "{:<8} {:<7} {:>7} {:>12} {:>11} {:>9.2} {:>12.2} {:>12.2} {:>14}",
        arm,
        if arm == "project" { threads.to_string() } else { "—".to_string() },
        match order {
            EntityOrder::Morton => "morton",
            EntityOrder::Issue => "issue",
        },
        rows,
        artifacts,
        seconds,
        gb(before),
        gb(peak),
        cardinality,
    );

    std::fs::remove_file(&permutation).ok();
    std::fs::remove_dir(&dir).ok();
    std::hint::black_box(&out);
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
    let text = |name: &str, default: &'static str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };
    let members = flag("--members", DEFAULT_MEMBERS as u64) as u32;
    let runs = flag("--runs", DEFAULT_RUNS as u64) as u32;
    let deleted = args
        .iter()
        .position(|a| a == "--deleted-fraction")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DELETED);
    let order = match text("--entity-order", "morton").as_str() {
        "morton" => EntityOrder::Morton,
        "issue" => EntityOrder::Issue,
        other => panic!("--entity-order takes morton|issue, got {other:?}"),
    };
    let verify = args.iter().any(|a| a == "--verify");
    let threads = flag("--threads", 1) as usize;

    // The child form: one configuration, one row, then exit.
    if let Some(i) = args.iter().position(|a| a == "--arm") {
        let rows = flag("--rows", 10_000_000);
        measure_one(
            args[i + 1].as_str(),
            rows,
            flag("--artifacts", rows / members as u64),
            members,
            runs,
            deleted,
            order,
            threads,
            verify,
        );
        return;
    }

    let max_rows = flag("--rows", 10_000_000);

    println!("# the fold's artifact pass — two constructions");
    println!(
        "# mean members {members} in {runs} entity-space runs, {:.0}% of rows retired by the fold",
        deleted * 100.0
    );
    println!(
        "# project: `Permutation::project` per artifact through the new mapped permutation \
         (`ArtifactRows::build`'s construction)"
    );
    println!("# ride:    the inverted entity→artifacts CSR held resident across pass 1's stream");
    println!("# peak RSS is VmHWM — the ride arm's bytes are anonymous, the project arm's are page cache");
    println!();
    println!(
        "{:<8} {:<7} {:>7} {:>12} {:>11} {:>9} {:>12} {:>12} {:>14}",
        "arm", "eorder", "threads", "rows", "artifacts", "seconds", "RSS before", "RSS peak",
        "rows written"
    );

    let exe = std::env::current_exe().expect("this probe re-runs itself per configuration");
    for arm in ["project", "ride"] {
        let mut rows = 1_000_000u64;
        while rows <= max_rows {
            let mut child = std::process::Command::new(&exe);
            child.args([
                "--arm",
                arm,
                "--rows",
                &rows.to_string(),
                "--artifacts",
                &(rows / members as u64).to_string(),
                "--members",
                &members.to_string(),
                "--runs",
                &runs.to_string(),
                "--deleted-fraction",
                &deleted.to_string(),
                "--threads",
                &threads.to_string(),
                "--entity-order",
                match order {
                    EntityOrder::Morton => "morton",
                    EntityOrder::Issue => "issue",
                },
            ]);
            if verify && rows <= 1_000_000 {
                child.arg("--verify");
            }
            let status = child.status().expect("child measurement");
            if !status.success() {
                println!("{arm:<8} {:>12} — killed (out of memory)", rows);
                break;
            }
            rows *= 10;
        }
    }
}
