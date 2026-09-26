//! **What does reading the ingest buffer cost, and what does holding one cost?**
//!
//! `write_cost`'s experiment A prices the *write* side of [`IngestBuffer`]: a commit window copies
//! the whole hash table, and that term grows with everything buffered since the last flush. This
//! binary prices the other side, so a change to the map underneath the buffer can be judged on
//! both. Every figure here is over a buffer this binary fills itself, except the last section,
//! which opens an engine over a real bundle.
//!
//! - **R1** — `iter()`, the entity-space walk `compose` takes once per request and
//!   `filter::candidate` once per filtered request.
//! - **R2** — `rows()` narrowed to one view, the walk `plan_flush` takes per tick and the two
//!   view-scoped commands take per publication.
//! - **R3** — `get`, `contains` and `contains_in_view`, hit and miss, probed in random order:
//!   the join rule's admission check and the item drill-down.
//! - **R4** — `fill_of` and `scoped_fill_of`, one per row of an accepted values batch.
//! - **R5** — bytes held, per buffered row, for one buffer and for N retained generations that
//!   each differ from the last by one 100-row window. Counted by a wrapping global allocator
//!   (live bytes = allocated − freed), not by RSS, so the figure is the structure's and not the
//!   allocator's high-water.
//! - **W2** — `insert_row_with_terms` + `set_wal_pos` per row, `remove` per call, and a whole
//!   buffer clone, which is what a commit window pays.
//! - **N** — the noise control: an `Overlay` clone at 100k entries, which no change to the buffer
//!   can reach.
//! - **E1** — viewport latency, a session's composed mask and one filtered viewport, over a real
//!   bundle with rows buffered and unflushed.
//! - **E2** — deriving the per-view buffered-row lists, which is what a geometry publication pays
//!   for the lists E1's requests then read.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin buffer_read_cost -- [--only r] \
//!     [--fixture target/tmp/arxiv/bundle]
//! ```
//!
//! The fixture is **copied** before it is opened, exactly as `write_cost` copies it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::{ChangeOp, WalRow, WalScalar};
use tessera_lifecycle::{Fill, IngestBuffer, Overlay, ScopedFill, UnallocatedRow};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;
use tessera_types::{EntityId, TermId};

/// Live heap bytes: every allocation this process makes, less every free. Read either side of a
/// structure's construction, the difference is what that structure holds.
static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);

struct Counting;

// SAFETY: every call forwards to `System`, which is a sound allocator; the counter is incremented
// only after a successful allocation and decremented only on a real free.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live_bytes() -> isize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// Descriptors per row, matching every other ingest arm in this crate.
const TERM_DENSITY: usize = 3;

/// The view every synthetic row here names.
const VIEW: &str = "v";

/// A second view, so `rows()` has something to narrow past.
const OTHER_VIEW: &str = "w";

/// The window a retained generation differs from the one before it by.
const WINDOW_ROWS: usize = 100;

/// How the entity ids of a filled buffer are laid out. A hash map's walk is sensitive to how its
/// keys land in the table, and a real buffer's ids are ascending from the allocator's high water
/// — but a replay after deletions, or a corpus ingested in several streams, scatters them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ids {
    Dense,
    Scattered,
}

impl Ids {
    fn name(self) -> &'static str {
        match self {
            Ids::Dense => "dense",
            Ids::Scattered => "scattered",
        }
    }

    /// The `i`th entity id under this layout.
    fn id(self, i: u64) -> EntityId {
        match self {
            Ids::Dense => EntityId::new(i),
            // Fibonacci scatter, so consecutive rows land in unrelated buckets.
            Ids::Scattered => EntityId::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20),
        }
    }
}

fn wal_row(entity: EntityId, view: &str, join: bool) -> WalRow {
    WalRow {
        external_id: Some(format!("buffer-read-{}", entity.raw()).into_bytes()),
        entity_id: entity,
        view: view.to_string(),
        join,
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: Vec::new(),
        scoped: Vec::new(),
    }
}

fn terms() -> Vec<TermId> {
    (0..TERM_DENSITY as u32).map(TermId::new).collect()
}

/// A buffer of `depth` rows under `layout`. Every eighth entity also holds a join in a second
/// view, so `rows()` yields more than `iter()` and the narrowing in R2 is not a no-op.
fn filled(depth: usize, layout: Ids) -> IngestBuffer {
    let mut buffer = IngestBuffer::new();
    let mut placed = 0usize;
    let mut i = 0u64;
    while placed < depth {
        let entity = layout.id(i);
        buffer.insert_row_with_terms(&wal_row(entity, VIEW, false), terms());
        placed += 1;
        if placed < depth && i.is_multiple_of(8) {
            buffer.insert_row_with_terms(&wal_row(entity, OTHER_VIEW, true), Vec::new());
            placed += 1;
        }
        i += 1;
    }
    buffer
}

/// Give every entity in `buffer` an entity-scoped and a group-scoped fill, as an accepted values
/// batch over the whole buffer would.
fn with_fills(buffer: &mut IngestBuffer, depth: usize, layout: Ids) {
    let absent = |value: &WalScalar| matches!(value, WalScalar::Null);
    for i in 0..depth as u64 {
        let entity = layout.id(i);
        buffer.fill(
            entity,
            Fill {
                view: VIEW.to_string(),
                scalars: vec![WalScalar::Null, WalScalar::I64(i as i64)],
                wal_pos: Some(i),
            },
            absent,
        );
        buffer.fill_scoped(
            entity,
            VIEW.to_string(),
            ScopedFill {
                view: VIEW.to_string(),
                scoped: vec![WalScalar::I64(i as i64)],
                wal_pos: Some(i),
            },
            absent,
        );
    }
}

/// The minimum of `runs` timings of `body`, which is the statistic a constant wants on a machine
/// somebody else may also be using.
fn best<T>(runs: usize, mut body: impl FnMut() -> T) -> u64 {
    let mut held = u64::MAX;
    for _ in 0..runs {
        let at = Instant::now();
        let out = body();
        held = held.min(at.elapsed().as_nanos() as u64);
        std::hint::black_box(&out);
    }
    held
}

/// [`best`] with the state each run mutates rebuilt outside the clock: `setup` produces it,
/// `body` is what is timed. A mutator has to be timed this way — subtracting one minimum from
/// another takes the two from different runs, and at a million rows that difference is noise.
fn best_on<S, T>(
    runs: usize,
    mut setup: impl FnMut() -> S,
    mut body: impl FnMut(&mut S) -> T,
) -> u64 {
    let mut held = u64::MAX;
    for _ in 0..runs {
        let mut state = setup();
        let at = Instant::now();
        let out = body(&mut state);
        held = held.min(at.elapsed().as_nanos() as u64);
        std::hint::black_box(&out);
        std::hint::black_box(&state);
    }
    held
}

fn ms(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

// =================================================================================================
// R1, R2: the two walks
// =================================================================================================

fn walks(depths: &[usize], runs: usize) {
    println!("== R1/R2: the two walks, per entry (min of {runs}) ==");
    println!(
        "{:>10} {:>10} {:>12} {:>11} {:>12} {:>11} {:>10}",
        "buffered", "layout", "iter ms", "iter ns/e", "rows ms", "rows ns/e", "rows kept"
    );
    for depth in depths {
        for layout in [Ids::Dense, Ids::Scattered] {
            let buffer = filled(*depth, layout);
            let entities = buffer.iter().count().max(1);
            let iter = best(runs, || {
                let mut n = 0usize;
                for (_, item) in buffer.iter() {
                    n += item.terms.len();
                }
                n
            });
            let mut kept = 0usize;
            let rows = best(runs, || {
                let mut n = 0usize;
                for (_, item) in buffer.rows() {
                    if item.view == VIEW {
                        n += 1;
                    }
                }
                kept = n;
                n
            });
            println!(
                "{:>10} {:>10} {:>12.4} {:>11.2} {:>12.4} {:>11.2} {:>10}",
                depth,
                layout.name(),
                ms(iter),
                iter as f64 / entities as f64,
                ms(rows),
                rows as f64 / (*depth).max(1) as f64,
                kept,
            );
        }
    }
}

// =================================================================================================
// R3: the point lookups
// =================================================================================================

fn lookups(depths: &[usize], runs: usize, probes: usize) {
    println!("\n== R3: point lookups, random order, ns per call (min of {runs}) ==");
    println!(
        "{:>10} {:>10} {:>9} {:>9} {:>11} {:>11} {:>12} {:>12}",
        "buffered", "layout", "get hit", "get miss", "cont. hit", "cont. miss", "in-view hit", "in-view miss"
    );
    for depth in depths {
        for layout in [Ids::Dense, Ids::Scattered] {
            let buffer = filled(*depth, layout);
            let mut rng = StdRng::seed_from_u64(7);
            let count = probes.min(*depth).max(1);
            let mut hits: Vec<EntityId> = (0..*depth as u64).map(|i| layout.id(i)).collect();
            hits.shuffle(&mut rng);
            hits.truncate(count);
            // Ids nothing buffered names: above every id either layout hands out.
            let misses: Vec<EntityId> = (0..count)
                .map(|i| EntityId::new(u64::MAX / 2 + i as u64))
                .collect();

            let per = |nanos: u64| nanos as f64 / count as f64;
            let get_hit = best(runs, || hits.iter().filter(|e| buffer.get(**e).is_some()).count());
            let get_miss = best(runs, || {
                misses.iter().filter(|e| buffer.get(**e).is_some()).count()
            });
            let has_hit = best(runs, || hits.iter().filter(|e| buffer.contains(**e)).count());
            let has_miss = best(runs, || misses.iter().filter(|e| buffer.contains(**e)).count());
            let view_hit = best(runs, || {
                hits.iter()
                    .filter(|e| buffer.contains_in_view(**e, VIEW))
                    .count()
            });
            let view_miss = best(runs, || {
                misses
                    .iter()
                    .filter(|e| buffer.contains_in_view(**e, VIEW))
                    .count()
            });
            println!(
                "{:>10} {:>10} {:>9.1} {:>9.1} {:>11.1} {:>11.1} {:>12.1} {:>12.1}",
                depth,
                layout.name(),
                per(get_hit),
                per(get_miss),
                per(has_hit),
                per(has_miss),
                per(view_hit),
                per(view_miss),
            );
        }
    }
}

// =================================================================================================
// R4: the fill lookups
// =================================================================================================

fn fill_lookups(depths: &[usize], runs: usize, probes: usize) {
    println!("\n== R4: fill lookups with every entity filled, ns per call (min of {runs}) ==");
    println!(
        "{:>10} {:>10} {:>11} {:>11} {:>13} {:>13}",
        "buffered", "layout", "fill hit", "fill miss", "scoped hit", "scoped miss"
    );
    for depth in depths {
        for layout in [Ids::Dense, Ids::Scattered] {
            let mut buffer = filled(*depth, layout);
            with_fills(&mut buffer, *depth, layout);
            let mut rng = StdRng::seed_from_u64(11);
            let count = probes.min(*depth).max(1);
            let mut hits: Vec<EntityId> = (0..*depth as u64).map(|i| layout.id(i)).collect();
            hits.shuffle(&mut rng);
            hits.truncate(count);
            let misses: Vec<EntityId> = (0..count)
                .map(|i| EntityId::new(u64::MAX / 2 + i as u64))
                .collect();

            let per = |nanos: u64| nanos as f64 / count as f64;
            let hit = best(runs, || {
                hits.iter().filter(|e| buffer.fill_of(**e).is_some()).count()
            });
            let miss = best(runs, || {
                misses
                    .iter()
                    .filter(|e| buffer.fill_of(**e).is_some())
                    .count()
            });
            let scoped_hit = best(runs, || {
                hits.iter()
                    .filter(|e| buffer.scoped_fill_of(**e, VIEW).is_some())
                    .count()
            });
            let scoped_miss = best(runs, || {
                misses
                    .iter()
                    .filter(|e| buffer.scoped_fill_of(**e, VIEW).is_some())
                    .count()
            });
            println!(
                "{:>10} {:>10} {:>11.1} {:>11.1} {:>13.1} {:>13.1}",
                depth,
                layout.name(),
                per(hit),
                per(miss),
                per(scoped_hit),
                per(scoped_miss),
            );
        }
    }
}

// =================================================================================================
// R5: what a buffer holds, and what N generations of one hold
// =================================================================================================

fn memory(depths: &[usize], generations: &[usize]) {
    println!("\n== R5: live heap bytes (allocated − freed), counted by a wrapping allocator ==");
    println!(
        "{:>10} {:>10} {:>14} {:>12}",
        "buffered", "layout", "bytes", "bytes/row"
    );
    for depth in depths {
        for layout in [Ids::Dense, Ids::Scattered] {
            let before = live_bytes();
            let buffer = filled(*depth, layout);
            let held = live_bytes() - before;
            println!(
                "{:>10} {:>10} {:>14} {:>12.1}",
                depth,
                layout.name(),
                held,
                held as f64 / (*depth).max(1) as f64,
            );
            drop(buffer);
        }
    }

    println!(
        "\n-- N retained generations, each one {WINDOW_ROWS}-row window past the last (dense) --"
    );
    println!(
        "{:>10} {:>6} {:>14} {:>16} {:>14}",
        "buffered", "gens", "bytes", "bytes/extra gen", "bytes/row"
    );
    for depth in depths {
        for gens in generations {
            let before = live_bytes();
            let mut held: Vec<IngestBuffer> = Vec::with_capacity(*gens);
            let mut live = filled(*depth, Ids::Dense);
            held.push(live.clone());
            let mut next = *depth as u64;
            for _ in 1..*gens {
                for _ in 0..WINDOW_ROWS {
                    let entity = Ids::Dense.id(next);
                    live.insert_row_with_terms(&wal_row(entity, VIEW, false), terms());
                    live.set_wal_pos(entity, VIEW, next);
                    next += 1;
                }
                held.push(live.clone());
            }
            let bytes = live_bytes() - before;
            let base = (*depth).max(1) as f64;
            println!(
                "{:>10} {:>6} {:>14} {:>16.0} {:>14.1}",
                depth,
                gens,
                bytes,
                if *gens > 1 {
                    (bytes as f64) / (*gens as f64)
                } else {
                    bytes as f64
                },
                bytes as f64 / base,
            );
            drop(held);
            drop(live);
        }
    }
    println!(
        "`KEEP_SUPERSEDED_GENERATIONS` is 1, and it bounds the row-projection cache rather than \
         the generations themselves: what holds an old `Generation` is the `ArcSwap` (one) plus \
         every request that loaded one and has not finished. Two is the resting number; the \
         higher rows are what a burst of in-flight requests across a window close costs."
    );
}

// =================================================================================================
// W2: the mutators, and the clone a window pays
// =================================================================================================

fn writes(depths: &[usize], runs: usize) {
    println!("\n== W2: the mutators and the whole-buffer clone (min of {runs}) ==");
    println!(
        "{:>10} {:>10} {:>14} {:>14} {:>14}",
        "buffered", "layout", "insert ns/row", "remove ns/call", "clone us"
    );
    for depth in depths {
        for layout in [Ids::Dense, Ids::Scattered] {
            let buffer = filled(*depth, layout);
            let insert = best_on(
                runs,
                || buffer.clone(),
                |copy| {
                    for i in 0..WINDOW_ROWS as u64 {
                        let entity = EntityId::new(u64::MAX / 4 + i);
                        copy.insert_row_with_terms(&wal_row(entity, VIEW, false), terms());
                        copy.set_wal_pos(entity, VIEW, i);
                    }
                },
            );
            let remove = best_on(
                runs,
                || buffer.clone(),
                |copy| {
                    for i in 0..WINDOW_ROWS as u64 {
                        copy.remove(layout.id(i));
                    }
                },
            );
            let clone = best(runs, || buffer.clone());
            println!(
                "{:>10} {:>10} {:>14.1} {:>14.1} {:>14.3}",
                depth,
                layout.name(),
                insert as f64 / WINDOW_ROWS as f64,
                remove as f64 / WINDOW_ROWS as f64,
                clone as f64 / 1e3,
            );
        }
    }
    println!(
        "`insert` and `remove` run over a fresh clone, because a mutator on a buffer a second \
         generation still shares is the one the executor runs. The clone is built outside the \
         clock and is timed on its own beside them."
    );
}

// =================================================================================================
// N: the noise control
// =================================================================================================

fn noise(runs: usize) {
    println!("\n== N: the noise control — an `Overlay` clone at 100k entries ==");
    let mut overlay = Overlay::new();
    for id in 0..100_000u64 {
        overlay.apply(EntityId::new(id * 17), ChangeOp::Suppress);
    }
    let mut samples: Vec<u64> = Vec::new();
    for _ in 0..runs.max(5) {
        let at = Instant::now();
        let copy = overlay.clone();
        samples.push(at.elapsed().as_nanos() as u64);
        std::hint::black_box(&copy);
    }
    samples.sort_unstable();
    println!(
        "min {:.3} us, median {:.3} us, max {:.3} us — spread {:.1}% of the min",
        samples[0] as f64 / 1e3,
        samples[samples.len() / 2] as f64 / 1e3,
        samples[samples.len() - 1] as f64 / 1e3,
        100.0 * (samples[samples.len() - 1] - samples[0]) as f64 / samples[0].max(1) as f64,
    );
}

// =================================================================================================
// E1: a real bundle, with rows buffered and unflushed
// =================================================================================================

struct Fixture {
    root: PathBuf,
    view: String,
    extent: [f64; 4],
    descriptors: Vec<Vec<u8>>,
    high_water: u64,
    /// A declared scalar a filter leaf can name, if the bundle declares one.
    filter_column: Option<String>,
}

fn copy_bundle(source: &Path, root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_dir_all(root);
    let copied = std::process::Command::new("cp")
        .arg("-a")
        .arg(source)
        .arg(root)
        .status()?;
    if !copied.success() {
        return Err(format!("copying {} into {} failed", source.display(), root.display()).into());
    }
    Ok(())
}

fn current_prefix(root: &Path) -> std::io::Result<String> {
    let raw = std::fs::read(root.join("CURRENT"))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)?;
    Ok(value
        .get("prefix")
        .and_then(|p| p.as_str())
        .unwrap_or("v00000")
        .to_string())
}

fn dictionary_terms(root: &Path, prefix: &str) -> std::io::Result<Vec<String>> {
    let data = std::fs::read(root.join(prefix).join("dictionary/terms-0.dict"))?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().expect("4 bytes")) as usize;
        offset += 4;
        if offset + len > data.len() {
            break;
        }
        out.push(String::from_utf8_lossy(&data[offset..offset + len]).into_owned());
        offset += len;
    }
    Ok(out)
}

fn inspect(root: &Path, prefix: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let bundle = open_bundle(root)?;
    let declaration = bundle
        .manifest
        .views
        .first()
        .expect("a built bundle declares a view");
    let q = declaration.quantisation;
    Ok(Fixture {
        root: root.to_path_buf(),
        view: declaration.id.clone(),
        extent: [q.x_min, q.y_min, q.x_max, q.y_max],
        descriptors: dictionary_terms(root, prefix)?
            .into_iter()
            .take(TERM_DENSITY)
            .map(String::into_bytes)
            .collect(),
        high_water: bundle.manifest.entity_id_high_water,
        // A numeric column, so `Range { lo: None, hi: None }` — "carries a value at all" — is a
        // leaf the column can answer. A text or category column would refuse it, and a refused
        // filter measures nothing.
        filter_column: bundle
            .manifest
            .declared_scalars
            .iter()
            .find(|scalar| {
                scalar.vocabulary.is_none()
                    && !matches!(
                        scalar.arrow_type,
                        tessera_spatial::tiler::ScalarType::Text
                            | tessera_spatial::tiler::ScalarType::Keyword
                            | tessera_spatial::tiler::ScalarType::Utf8
                    )
            })
            .map(|scalar| scalar.name.clone()),
    })
}

fn open_engine(fx: &Fixture, scratch: &Path, tag: &str) -> Result<Engine, Box<dyn std::error::Error>> {
    let tmp = scratch.join(tag);
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let mut engine = Engine::open(
        &fx.root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            // Server defaults, so this measures a deployment somebody runs.
            max_k: 1_000,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
            // The flush never fires, so every viewport below is answered with the rows still
            // buffered — which is the state this whole binary is about.
            flush_max_age_secs: 86_400,
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )?;
    engine.start_write_executor(4096)?;
    Ok(engine)
}

fn synth_rows(fx: &Fixture, count: usize, start: u64, terms: &[TermId]) -> Vec<UnallocatedRow> {
    let [x_min, y_min, x_max, y_max] = fx.extent;
    (0..count)
        .map(|i| {
            let n = start + i as u64;
            UnallocatedRow {
                external_id: Some(format!("buffer-read-{n}").into_bytes()),
                view: fx.view.clone(),
                join: None,
                descriptors: fx.descriptors.clone(),
                x: x_min + ((n % 4096) as f64 / 4096.0) * (x_max - x_min),
                y: y_min + ((n % 997) as f64 / 997.0) * (y_max - y_min),
                scalars: Vec::new(),
                terms: terms.to_vec(),
                scoped: Vec::new(),
            }
        })
        .collect()
}

/// One E1 cell: the three viewport percentiles, the composed mask and the filtered viewport.
type Cell = (Duration, Duration, Duration, u64, u64);

fn percentile(sorted: &[Duration], q: f64) -> Duration {
    let idx = ((sorted.len() as f64) * q) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn experiment_e1(
    fx: &Fixture,
    scratch: &Path,
    depths: &[usize],
    samples: usize,
    rounds: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== E1: viewport latency with rows buffered and unflushed ==");
    println!("(min over {rounds} round(s) of a fresh engine; {samples} viewports per cell)");
    println!(
        "{:>10} {:>11} {:>11} {:>11} {:>14} {:>16}",
        "buffered", "p50 ms", "p90 ms", "p99 ms", "compose ms", "filtered vp ms"
    );
    let mut best_cells: Vec<Option<Cell>> = vec![None; depths.len()];
    for round in 0..rounds {
        let engine = open_engine(fx, scratch, &format!("e{round}"))?;
        let terms = engine.resolve_terms(&fx.descriptors);
        let descriptor = String::from_utf8_lossy(&fx.descriptors[0]).into_owned();
        let session = engine.authorise(format!(r#"{{"terms": ["{descriptor}"]}}"#).as_bytes())?;
        let mut next = fx.high_water + (round as u64 + 1) * 10_000_000;
        let mut buffered = 0usize;
        for (cell, depth) in depths.iter().enumerate() {
            while buffered < *depth {
                let count = 5_000.min(depth - buffered);
                let rows = synth_rows(fx, count, next, &terms);
                next += count as u64;
                engine.accept_ingest(rows, format!("fill-{round}-{next}"), [round as u8; 32])?;
                buffered += count;
            }
            let [x_min, y_min, x_max, y_max] = fx.extent;
            let mut rng = StdRng::seed_from_u64(42);
            // One warm pass outside the timing loop: a session's geometry is built once and
            // reused, exactly as a real client's is.
            engine.viewport(
                &session,
                ViewportRequest::new(&fx.view, 6, [x_min, y_min, x_max, y_max], 30),
            )?;
            let mut latencies = Vec::with_capacity(samples);
            for _ in 0..samples {
                let x0 = rng.gen_range(x_min..x_max);
                let y0 = rng.gen_range(y_min..y_max);
                let x1 = (x0 + (x_max - x_min) / 128.0).min(x_max);
                let y1 = (y0 + (y_max - y_min) / 128.0).min(y_max);
                let zoom: u8 = rng.gen_range(4..=12);
                let at = Instant::now();
                engine.viewport(&session, ViewportRequest::new(&fx.view, zoom, [x0, y0, x1, y1], 30))?;
                latencies.push(at.elapsed());
            }
            latencies.sort();
            // The session's composed mask: the `compose` walk of the buffer, on its own.
            let compose = best(5, || engine.composed_mask(&session, &fx.view));
            // One filtered viewport, which takes `filter::candidate`'s walk beside `compose`'s.
            let filtered = match &fx.filter_column {
                None => 0,
                Some(column) => {
                    let expr = tessera_engine::filter::FilterExpr::Leaf {
                        column: column.clone(),
                        operand: tessera_engine::filter::FilterOperand::Range { lo: None, hi: None },
                    };
                    let mut request =
                        ViewportRequest::new(&fx.view, 6, [x_min, y_min, x_max, y_max], 30);
                    request.filter = Some(expr);
                    // Refused (an unfilterable column) is reported as zero rather than guessed at.
                    match engine.viewport(&session, request) {
                        Ok(_) => {
                            let expr = tessera_engine::filter::FilterExpr::Leaf {
                                column: column.clone(),
                                operand: tessera_engine::filter::FilterOperand::Range {
                                    lo: None,
                                    hi: None,
                                },
                            };
                            best(3, || {
                                let mut request = ViewportRequest::new(
                                    &fx.view,
                                    6,
                                    [x_min, y_min, x_max, y_max],
                                    30,
                                );
                                request.filter = Some(expr.clone());
                                engine.viewport(&session, request)
                            })
                        }
                        Err(_) => 0,
                    }
                }
            };
            let cell_value = (
                percentile(&latencies, 0.50),
                percentile(&latencies, 0.90),
                percentile(&latencies, 0.99),
                compose,
                filtered,
            );
            best_cells[cell] = Some(match best_cells[cell] {
                None => cell_value,
                Some(held) => (
                    held.0.min(cell_value.0),
                    held.1.min(cell_value.1),
                    held.2.min(cell_value.2),
                    held.3.min(cell_value.3),
                    held.4.min(cell_value.4),
                ),
            });
        }
        drop(engine);
    }
    for (cell, depth) in depths.iter().enumerate() {
        let (p50, p90, p99, compose, filtered) = best_cells[cell].expect("a measured cell");
        println!(
            "{:>10} {:>11.4} {:>11.4} {:>11.4} {:>14.4} {:>16.4}",
            depth,
            p50.as_nanos() as f64 / 1e6,
            p90.as_nanos() as f64 / 1e6,
            p99.as_nanos() as f64 / 1e6,
            ms(compose),
            ms(filtered),
        );
    }
    println!(
        "The zero row is the noise control: an empty buffer is a state no change to the buffer's \
         map can reach, so what moves there is the machine."
    );
    Ok(())
}

/// **E2** — the fresh derivation every geometry publication pays: one `buffered_rows_of` per view
/// of the fixture's bundle, over a buffer of `depth` rows none of which has a row yet, which is the
/// state a flush publishes out of.
fn experiment_e2(root: &Path, depths: &[usize], runs: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== E2: deriving the buffered-row lists, as a publication does ==");
    println!("(min of {runs}; entity ids past the bundle's high water, so every lookup misses)");
    println!("{:>10} {:>14} {:>16}", "buffered", "per view ms", "all views ms");
    let bundle = open_bundle(root)?;
    let spaces: Vec<&tessera_store::RowSpace> = bundle
        .partitions
        .values()
        .flat_map(|partition| partition.views.values().map(|view| &view.row_space))
        .collect();
    for depth in depths {
        let mut buffer = IngestBuffer::new();
        for i in 0..*depth as u64 {
            let entity = EntityId::new(bundle.manifest.entity_id_high_water + i);
            buffer.insert_row_with_terms(&wal_row(entity, VIEW, false), terms());
        }
        let all = best(runs, || {
            spaces
                .iter()
                .map(|space| tessera_engine::buffered_rows_of(&buffer, space))
                .collect::<Vec<_>>()
        });
        println!(
            "{:>10} {:>14.4} {:>16.4}",
            depth,
            ms(all) / spaces.len() as f64,
            ms(all)
        );
    }
    Ok(())
}

// =================================================================================================

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut fixture: Option<PathBuf> = None;
    let mut runs = 5usize;
    let mut rounds = 1usize;
    let mut samples = 200usize;
    let mut probes = 20_000usize;
    let mut only: Option<String> = None;
    let mut max_depth = 1_000_000usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixture" => fixture = args.next().map(PathBuf::from),
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(runs),
            "--rounds" => rounds = args.next().and_then(|v| v.parse().ok()).unwrap_or(rounds),
            "--samples" => samples = args.next().and_then(|v| v.parse().ok()).unwrap_or(samples),
            "--probes" => probes = args.next().and_then(|v| v.parse().ok()).unwrap_or(probes),
            "--only" => only = args.next(),
            "--max-depth" => {
                max_depth = args.next().and_then(|v| v.parse().ok()).unwrap_or(max_depth)
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — every number below is meaningless. Use --release.");
    }

    let depths: Vec<usize> = [10_000usize, 40_000, 200_000, 1_000_000]
        .into_iter()
        .filter(|d| *d <= max_depth)
        .collect();
    let wanted = |letter: &str| only.as_deref().is_none_or(|o| o.contains(letter));

    if wanted("1") || wanted("r") {
        walks(&depths, runs);
    }
    if wanted("3") || wanted("r") {
        lookups(&depths, runs, probes);
    }
    if wanted("4") || wanted("r") {
        fill_lookups(&depths, runs, probes);
    }
    if wanted("5") || wanted("r") {
        memory(&depths, &[1usize, 2, 4, 8]);
    }
    if wanted("w") {
        writes(&depths, runs);
    }
    if wanted("n") {
        noise(runs);
    }
    if wanted("e") {
        let Some(source) = fixture else {
            println!("\n(E1 skipped: no --fixture <bundle root>)");
            return Ok(());
        };
        let scratch =
            std::env::temp_dir().join(format!("tessera-buffer-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let root = scratch.join("bundle");
        copy_bundle(&source, &root)?;
        let prefix = current_prefix(&root)?;
        let fx = inspect(&root, &prefix)?;
        println!(
            "\nfixture {} (copied to {}) — {} items, view {}",
            source.display(),
            root.display(),
            fx.high_water,
            fx.view,
        );
        let e1_depths: Vec<usize> = [0usize, 40_000, 200_000]
            .into_iter()
            .filter(|d| *d <= max_depth)
            .collect();
        let outcome = experiment_e1(&fx, &scratch, &e1_depths, samples, rounds)
            .and_then(|()| experiment_e2(&root, &[200_000, 1_000_000], runs));
        let _ = std::fs::remove_dir_all(&scratch);
        outcome?;
    }
    Ok(())
}
