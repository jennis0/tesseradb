//! **What does a commit window cost as the ingest buffer grows, and a deny window as the overlay
//! grows?**
//!
//! Five costs on the write executor's hot path, each priced at the scale it is suspected to bend
//! at. Nothing here changes what the executor does: every figure is a delta of counters the
//! executor already publishes (`ExecutorStats::stage_nanos`, `apply_nanos_total`, `wal_fsyncs`),
//! taken around one submission, or a direct timing of a shipped data structure this binary fills
//! itself.
//!
//! - **A** — `Executor::apply_window` deep-copies the whole ingest buffer once per commit window
//!   (`(*generation.buffer).clone()`), so a window's cost carries everything buffered since the
//!   last flush. `WriteStage::ApplyBufferClone` already times it; this sweeps the buffer from
//!   empty to a million rows and reads the stage off a fixed 100-row probe window at each depth,
//!   so the per-window term and the per-row term are separated rather than added together.
//!   The same sweep answers whether the **four per-row laps** inside `ApplyRows`
//!   (`RowEstablished`, `RowEstablishedInv`, `RowBufferInsert`, `RowWalPos`) are a material share
//!   of the per-row cost: their sum against `ApplyRows` is printed here, and the residual against
//!   a build with those four laps removed is a second run of this same binary.
//! - **B** — `Executor::apply_changes` deep-copies the whole [`Overlay`] once per deny window, and
//!   the overlay shrinks only at a fold. Priced at four depths, at a window of one change and a
//!   window of a thousand, beside a direct clone of an overlay of the same depth.
//! - **C** — `Executor::derive_records` builds a `code → key` map of a predicate layer's entire
//!   vocabulary once per window per layer, though a window needs only the codes its own rows
//!   carry. Priced by declaring the vocabulary and the layer through the engine's own write API
//!   and differencing the window, and beside it by building the same map directly.
//! - **D** — `Executor::tick_behind_flush` walks the whole buffer to count flushable rows whenever
//!   a tick lands while a flush is in flight. Priced directly over an [`IngestBuffer`] this binary
//!   fills, at A's depths.
//! - **E** — what a deny-only run leaves on disc: every deny publishes a side-manifest and every
//!   side-manifest restates the whole deny state, so the bundle grows with the integral of the
//!   overlay's depth. One cell per `target:window` pair, each over a fresh copy of the fixture.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin write_cost -- \
//!     --fixture target/tmp/arxiv/bundle [--repeat 5] [--only a]
//! ```
//!
//! The fixture is **copied** before it is opened: this binary writes into the copy and never into
//! the bundle it was pointed at.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tessera_engine::{ExecutorStats, WriteStage};
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::{ChangeOp, WalRow, WalScalar};
use tessera_lifecycle::{IngestBuffer, Overlay, UnallocatedRow};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;
use tessera_types::{EntityId, TermId};

/// Descriptors per row. The density every other ingest arm in this crate writes at, so a figure
/// here is comparable with one there.
const TERM_DENSITY: usize = 3;

/// The probe window's size. Small and fixed, so what moves between depths is the per-window term.
const PROBE_ROWS: usize = 100;

/// The fill batch. One `accept_ingest` blocks on its receipt, so one call is one commit window.
const FILL_ROWS: usize = 1_000;

/// `DENY_WINDOW_MAX_ENTRIES`: the most entries one deny window holds.
const DENY_WINDOW: usize = 1_000;

/// The gap between consecutive suppressed entity ids. See [`experiment_b`]: Roaring collapses a
/// contiguous run to a run container, and a revocation is not contiguous.
const DENY_STRIDE: u64 = 17;

// =================================================================================================
// The fixture and the engine
// =================================================================================================

struct Fixture {
    /// The bundle this binary was pointed at. Read, never written.
    source: PathBuf,
    /// The copy every engine here opens.
    root: PathBuf,
    view: String,
    extent: [f64; 4],
    /// Descriptors the dictionary names, and the terms they resolve to.
    descriptors: Vec<Vec<u8>>,
    /// How many entity ids the bundle already issued.
    high_water: u64,
    /// `(index in declared_scalars, a key of its vocabulary)` for the widest category column the
    /// bundle declares, or `None` where it declares none.
    category: Option<(usize, String)>,
    declared: usize,
}

fn inspect(source: &Path, root: &Path, prefix: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let bundle = open_bundle(root)?;
    // The view and the frame it quantises against, taken from one declaration: a row is refused
    // outside its own view's extent, and the two views of a corpus need not share a frame.
    let declaration = bundle
        .manifest
        .views
        .first()
        .expect("a built bundle declares a view");
    let q = declaration.quantisation;
    let view = declaration.id.clone();
    let declared = bundle.manifest.declared_scalars.len();
    // The widest category column, since its vocabulary is the largest one a predicate layer over
    // this bundle could read.
    let mut category: Option<(usize, String, usize)> = None;
    for (index, scalar) in bundle.manifest.declared_scalars.iter().enumerate() {
        let Some(name) = &scalar.vocabulary else {
            continue;
        };
        let Some(vocabulary) = bundle.manifest.vocabularies.iter().find(|v| &v.name == name) else {
            continue;
        };
        let Some(value) = vocabulary.values.first() else {
            continue;
        };
        let size = vocabulary.values.len();
        if category.as_ref().is_none_or(|(_, _, held)| size > *held) {
            category = Some((index, value.key.clone(), size));
        }
    }
    Ok(Fixture {
        source: source.to_path_buf(),
        root: root.to_path_buf(),
        view,
        extent: [q.x_min, q.y_min, q.x_max, q.y_max],
        descriptors: dictionary_terms(root, prefix)?
            .into_iter()
            .take(TERM_DENSITY)
            .map(String::into_bytes)
            .collect(),
        high_water: bundle.manifest.entity_id_high_water,
        category: category.map(|(index, key, _)| (index, key)),
        declared,
    })
}

/// Every descriptor the bundle's dictionary names, in file order.
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

/// Put a fresh copy of the bundle at `root`. **Every engine here opens the copy**: a window's
/// close writes nothing into a bundle, but a deny drain publishes a side manifest and a flush
/// would write a segment, and a binary that can only be trusted while one configuration holds is
/// not one to point at a corpus.
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

/// An engine over the fixture whose **flush never fires**: the period is a day and the row trigger
/// is off, so the buffer only grows and every window's clone is over everything written so far.
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
            max_k: 5000,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
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

fn synth_rows(
    fx: &Fixture,
    count: usize,
    start: u64,
    terms: &[TermId],
    scalars: &[WalScalar],
) -> Vec<UnallocatedRow> {
    let [x_min, y_min, x_max, y_max] = fx.extent;
    (0..count)
        .map(|i| {
            let n = start + i as u64;
            UnallocatedRow {
                external_id: Some(format!("write-cost-{n}").into_bytes()),
                view: fx.view.clone(),
                join: None,
                descriptors: fx.descriptors.clone(),
                x: x_min + ((n % 4096) as f64 / 4096.0) * (x_max - x_min),
                y: y_min + ((n % 997) as f64 / 997.0) * (y_max - y_min),
                scalars: scalars.to_vec(),
                terms: terms.to_vec(),
                scoped: Vec::new(),
            }
        })
        .collect()
}

// =================================================================================================
// Reading the stages
// =================================================================================================

/// One window, as the difference of two [`ExecutorStats`] snapshots either side of one submission.
#[derive(Clone, Copy, Default)]
struct Window {
    stage: [u64; WriteStage::COUNT],
    apply_nanos: u64,
    fsyncs: u64,
    wall: u64,
}

impl Window {
    fn between(before: &ExecutorStats, after: &ExecutorStats, wall: u64) -> Window {
        let stage = std::array::from_fn(|i| {
            after.stage_nanos[i].saturating_sub(before.stage_nanos[i])
        });
        Window {
            stage,
            apply_nanos: after.apply_nanos_total.saturating_sub(before.apply_nanos_total),
            fsyncs: after.wal_fsyncs.saturating_sub(before.wal_fsyncs),
            wall,
        }
    }

    /// `ExecutorStats::stage_nanos` is indexed by the variant's own discriminant, which is not
    /// the order [`WriteStage::ALL`] lists them in.
    fn at(&self, stage: WriteStage) -> u64 {
        self.stage[stage as usize]
    }

    /// The stages that partition the executor thread's wall clock over this window. `SubmitToReceipt`
    /// is excluded: it overlaps every other stage rather than sitting beside them, and the four
    /// per-row stages are excluded because they sit **inside** `ApplyRows`.
    fn executor_nanos(&self) -> u64 {
        [
            WriteStage::AdmitWindow,
            WriteStage::Allocate,
            WriteStage::WalAppend,
            WriteStage::WalFsync,
            WriteStage::ApplyBufferClone,
            WriteStage::ApplyRows,
            WriteStage::ApplySwap,
            WriteStage::RecordBatch,
        ]
        .iter()
        .map(|s| self.at(*s))
        .sum()
    }

    fn per_row_laps(&self) -> u64 {
        [
            WriteStage::RowEstablished,
            WriteStage::RowEstablishedInv,
            WriteStage::RowBufferInsert,
            WriteStage::RowWalPos,
        ]
        .iter()
        .map(|s| self.at(*s))
        .sum()
    }

    /// Element-wise minimum, which is the statistic a constant wants on a shared machine.
    fn min_with(self, other: Window) -> Window {
        let stage = std::array::from_fn(|i| self.stage[i].min(other.stage[i]));
        Window {
            stage,
            apply_nanos: self.apply_nanos.min(other.apply_nanos),
            fsyncs: self.fsyncs.min(other.fsyncs),
            wall: self.wall.min(other.wall),
        }
    }
}

fn ms(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

// =================================================================================================
// A: the commit window against buffer depth
// =================================================================================================

fn experiment_a(
    fx: &Fixture,
    scratch: &Path,
    depths: &[usize],
    rounds: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("== A: one {PROBE_ROWS}-row commit window, against buffer depth ==");
    println!("(min of {rounds} rounds; a fresh engine per round, filled in {FILL_ROWS}-row windows)");

    let mut best: Vec<Option<Window>> = vec![None; depths.len()];
    let mut buffered_at_end = 0usize;
    for round in 0..rounds {
        let engine = open_engine(fx, scratch, &format!("a{round}"))?;
        let terms = engine.resolve_terms(&fx.descriptors);
        let mut next = fx.high_water + (round as u64 + 1) * 10_000_000;
        let mut buffered = 0usize;
        for (cell, depth) in depths.iter().enumerate() {
            while buffered < *depth {
                let count = FILL_ROWS.min(depth - buffered);
                let rows = synth_rows(fx, count, next, &terms, &[]);
                next += count as u64;
                engine.accept_ingest(rows, format!("fill-{round}-{next}"), [round as u8; 32])?;
                buffered += count;
            }
            let rows = synth_rows(fx, PROBE_ROWS, next, &terms, &[]);
            next += PROBE_ROWS as u64;
            let before = engine.write_executor_stats();
            let at = Instant::now();
            engine.accept_ingest(rows, format!("probe-{round}-{next}"), [round as u8; 32])?;
            let wall = at.elapsed().as_nanos() as u64;
            let after = engine.write_executor_stats();
            buffered += PROBE_ROWS;
            let window = Window::between(&before, &after, wall);
            best[cell] = Some(match best[cell] {
                None => window,
                Some(held) => held.min_with(window),
            });
        }
        // The guard: the executor's own occupancy figure against what was submitted.
        let reported = engine.write_executor_stats().buffered_items;
        if reported != buffered {
            return Err(format!(
                "round {round}: the engine reports {reported} buffered rows, {buffered} were submitted"
            )
            .into());
        }
        buffered_at_end = buffered;
        drop(engine);
    }
    println!("correctness guard: the engine reported {buffered_at_end} buffered rows, as submitted");

    println!(
        "\n{:>10} {:>11} {:>11} {:>11} {:>8} {:>12} {:>11} {:>10}",
        "buffered", "clone ms", "rows ms", "window ms", "clone %", "rows/s", "wall ms", "fsyncs"
    );
    for (cell, depth) in depths.iter().enumerate() {
        let w = best[cell].expect("a measured cell");
        let window = w.executor_nanos();
        println!(
            "{:>10} {:>11.4} {:>11.4} {:>11.4} {:>7.1}% {:>12.0} {:>11.4} {:>10}",
            depth,
            ms(w.at(WriteStage::ApplyBufferClone)),
            ms(w.at(WriteStage::ApplyRows)),
            ms(window),
            100.0 * w.at(WriteStage::ApplyBufferClone) as f64 / window.max(1) as f64,
            1e9 * PROBE_ROWS as f64 / window.max(1) as f64,
            ms(w.wall),
            w.fsyncs,
        );
    }

    println!("\n-- the same windows, stage by stage (ns for the whole {PROBE_ROWS}-row window) --");
    print!("{:>10}", "buffered");
    for stage in WriteStage::ALL {
        print!(" {:>14}", stage.name());
    }
    println!();
    for (cell, depth) in depths.iter().enumerate() {
        let w = best[cell].expect("a measured cell");
        print!("{depth:>10}");
        for stage in WriteStage::ALL {
            print!(" {:>14}", w.at(stage));
        }
        println!();
    }

    println!(
        "\n-- suspect 3: the four per-row laps against the loop they are inside (ns per row) --\n\
         {:>10} {:>12} {:>12} {:>9} {:>11} {:>11} {:>12} {:>10}",
        "buffered", "apply_rows", "4 laps", "share", ".est_fwd", ".est_inv", ".buf_insert", ".wal_pos"
    );
    for (cell, depth) in depths.iter().enumerate() {
        let w = best[cell].expect("a measured cell");
        let rows = PROBE_ROWS as f64;
        let laps = w.per_row_laps();
        println!(
            "{:>10} {:>12.1} {:>12.1} {:>8.1}% {:>11.1} {:>11.1} {:>12.1} {:>10.1}",
            depth,
            w.at(WriteStage::ApplyRows) as f64 / rows,
            laps as f64 / rows,
            100.0 * laps as f64 / w.at(WriteStage::ApplyRows).max(1) as f64,
            w.at(WriteStage::RowEstablished) as f64 / rows,
            w.at(WriteStage::RowEstablishedInv) as f64 / rows,
            w.at(WriteStage::RowBufferInsert) as f64 / rows,
            w.at(WriteStage::RowWalPos) as f64 / rows,
        );
    }
    println!(
        "\nThe four laps are timed by the same clock they charge, so their sum is **their own cost \
         plus the work**; what the sum over `apply_rows` bounds is how much of the loop the \
         instrumentation could possibly be. The residual is a second run of this binary built with \
         those four `lap` calls removed, compared at the same depths."
    );
    Ok(())
}

// =================================================================================================
// B: the deny window against overlay depth
// =================================================================================================

fn experiment_b(
    fx: &Fixture,
    scratch: &Path,
    depths: &[usize],
    clone_depths: &[usize],
    rounds: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== B: one deny window, against overlay depth ==");
    println!("(min of {rounds} rounds; suppressions, which never retire, so the depth only grows)");
    if let Some(deepest) = depths.last() {
        if *deepest as u64 > fx.high_water {
            println!(
                "note: the fixture issued {} entity ids, so every suppression past that names no \
                 point. The overlay takes it — it is a bitmap of entity ids and checks nothing — \
                 and the row-space deny mask it derives is then smaller than a real one's, so the \
                 cells above {} are a **lower** bound on the window and an exact one on the clone.",
                fx.high_water, fx.high_water
            );
        }
    }

    let mut one: Vec<Option<Window>> = vec![None; depths.len()];
    let mut many: Vec<Option<Window>> = vec![None; depths.len()];
    for round in 0..rounds {
        // A fresh copy per round. The deny lane marks the overlay behind-live at every window and
        // `Executor::run` publishes a side manifest after every drain, each carrying the whole
        // suppression list, so one round's backlog is gigabytes and would otherwise be carried
        // into the next round's bundle.
        copy_bundle(&fx.source, &fx.root)?;
        let engine = open_engine(fx, scratch, &format!("b{round}"))?;
        let mut depth = 0usize;
        // The nth id suppressed is `n * DENY_STRIDE`. A contiguous run is the one shape Roaring
        // compresses to nothing, so an ascending-by-one ramp would price a run container rather
        // than a revocation: a suppression set is scattered over entity space, and the clone's
        // cost is a property of that scatter.
        let mut next = 0u64;
        for (cell, target) in depths.iter().enumerate() {
            while depth < *target {
                let count = DENY_WINDOW.min(target - depth);
                let pending: Vec<_> = (0..count)
                    .map(|i| {
                        engine.submit_change(
                            EntityId::new((next + i as u64) * DENY_STRIDE),
                            ChangeOp::Suppress,
                        )
                    })
                    .collect::<Result<_, _>>()?;
                next += count as u64;
                depth += count;
                for p in pending {
                    p.wait()?;
                }
            }
            for (size, held) in [(1usize, &mut one), (DENY_WINDOW, &mut many)] {
                let before = engine.write_executor_stats();
                let at = Instant::now();
                let pending: Vec<_> = (0..size)
                    .map(|i| {
                        engine.submit_change(
                            EntityId::new((next + i as u64) * DENY_STRIDE),
                            ChangeOp::Suppress,
                        )
                    })
                    .collect::<Result<_, _>>()?;
                for p in pending {
                    p.wait()?;
                }
                let wall = at.elapsed().as_nanos() as u64;
                next += size as u64;
                depth += size;
                let window = Window::between(&before, &engine.write_executor_stats(), wall);
                held[cell] = Some(match held[cell] {
                    None => window,
                    Some(prior) => prior.min_with(window),
                });
            }
        }
        drop(engine);
    }

    println!(
        "\n{:>10} {:>7} {:>13} {:>13} {:>13} {:>13}",
        "depth", "changes", "apply ms", "wall ms", "ns/change", "windows"
    );
    for (cell, depth) in depths.iter().enumerate() {
        for (size, held) in [(1usize, &one), (DENY_WINDOW, &many)] {
            let w = held[cell].expect("a measured cell");
            println!(
                "{:>10} {:>7} {:>13.4} {:>13.4} {:>13.1} {:>13}",
                depth,
                size,
                ms(w.apply_nanos),
                ms(w.wall),
                w.apply_nanos as f64 / size as f64,
                w.fsyncs,
            );
        }
    }
    println!(
        "`apply ms` is `apply_nanos_total`'s delta, which on this lane is `apply_changes` and \
         nothing else: the overlay clone, the applies, the deny mask and the swap. `windows` is \
         the fsync count, so a row reading more than one did not get the window it asked for."
    );

    println!(
        "\n-- the clone alone: `(*generation.overlay).clone()`, scattered (stride {DENY_STRIDE}) \
         and contiguous --"
    );
    println!(
        "{:>10} {:>15} {:>13} {:>15} {:>13}",
        "depth", "scattered us", "ns/entry", "contiguous us", "ns/entry"
    );
    for depth in clone_depths {
        let mut cells = [0f64; 2];
        for (cell, stride) in [DENY_STRIDE, 1].into_iter().enumerate() {
            let mut overlay = Overlay::new();
            for id in 0..*depth as u64 {
                overlay.apply(EntityId::new(id * stride), ChangeOp::Suppress);
            }
            let mut best = u64::MAX;
            for _ in 0..rounds.max(5) {
                let at = Instant::now();
                let copy = overlay.clone();
                best = best.min(at.elapsed().as_nanos() as u64);
                std::hint::black_box(&copy);
            }
            cells[cell] = best as f64;
        }
        println!(
            "{:>10} {:>15.3} {:>13.3} {:>15.3} {:>13.3}",
            depth,
            cells[0] / 1e3,
            cells[0] / (*depth).max(1) as f64,
            cells[1] / 1e3,
            cells[1] / (*depth).max(1) as f64,
        );
    }
    Ok(())
}

// =================================================================================================
// C: `derive_records` against a predicate layer's vocabulary size
// =================================================================================================

/// The map `derive_records` builds per window per predicate layer, built here over `size` bindings
/// so the term is priced without the engine having to hold a vocabulary that large.
fn derive_map_cost(size: usize, rounds: usize) -> u64 {
    let bindings: Vec<(String, u32)> = (0..size).map(|i| (format!("key-{i:08}"), i as u32 + 1)).collect();
    let mut best = u64::MAX;
    for _ in 0..rounds {
        let at = Instant::now();
        let mut key_of_code: std::collections::BTreeMap<u32, String> = Default::default();
        for (key, code) in &bindings {
            key_of_code.insert(*code, key.to_string());
        }
        best = best.min(at.elapsed().as_nanos() as u64);
        std::hint::black_box(&key_of_code);
    }
    best
}

fn experiment_c(
    fx: &Fixture,
    scratch: &Path,
    rounds: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== C: `derive_records`'s `code → key` map, per window per predicate layer ==");

    println!("\n-- the map alone, over a vocabulary of this many keys --");
    println!("{:>12} {:>13} {:>13}", "keys", "build ms", "ns/key");
    for size in [1_000usize, 10_000, 100_000, 1_000_000] {
        let best = derive_map_cost(size, rounds.clamp(1, 3));
        println!(
            "{:>12} {:>13.4} {:>13.1} ",
            size,
            ms(best),
            best as f64 / size as f64
        );
    }

    let Some((index, key)) = fx.category.clone() else {
        println!("\n(the fixture declares no category column, so no predicate layer is registered)");
        return Ok(());
    };
    let column = column_name(fx, index);

    // The engine cells: the same 100-row window, with no predicate layer, with one over a column
    // the fixture already carries, and with one over a column this binary declares at a running
    // service against a vocabulary of the stated size. `derive_records` runs between the
    // `Allocate` lap and the append, so its cost lands in `WalAppend`.
    println!("\n-- through the engine: one {PROBE_ROWS}-row window, by predicate layer --");
    let cells: Vec<(String, Option<usize>)> = vec![
        ("no layer".to_string(), None),
        (format!("layer on {column}"), Some(0)),
        ("layer on 10k keys".to_string(), Some(10_000)),
        ("layer on 1M keys".to_string(), Some(1_000_000)),
    ];
    let mut best: Vec<Option<Window>> = vec![None; cells.len()];
    for round in 0..rounds {
        for (cell, (_, declare)) in cells.iter().enumerate() {
            let engine = open_engine(fx, scratch, &format!("c{round}-{cell}"))?;
            // Every declaration this cell makes carries the cell in its name, and the layer is
            // dropped at the end of it: the bundle copy is shared by every cell here, so a name
            // reused across cells is refused and a layer left registered would run its own
            // derive pass inside the next cell's window.
            let tag = format!("{round}-{cell}");
            let mut layer = None;
            let (probe_index, probe_key) = match declare {
                None => (index, key.clone()),
                Some(0) => {
                    layer = Some(register_predicate(&engine, &fx.view, &column, &tag)?);
                    (index, key.clone())
                }
                Some(size) => {
                    let declared = declare_vocabulary_column(&engine, *size, &tag)?;
                    layer = Some(register_predicate(&engine, &fx.view, &declared, &tag)?);
                    let index = engine
                        .generation()
                        .bundle
                        .manifest
                        .declared_scalars
                        .iter()
                        .position(|scalar| scalar.name == declared)
                        .expect("the column this call just declared");
                    (index, "key-00000001".to_string())
                }
            };
            let terms = engine.resolve_terms(&fx.descriptors);
            let mut scalars = vec![WalScalar::Null; probe_index + 1];
            scalars[probe_index] = WalScalar::Utf8(probe_key);
            let base = fx.high_water + (round as u64 + 40) * 10_000_000 + cell as u64 * 1_000_000;
            let rows = synth_rows(fx, PROBE_ROWS, base, &terms, &scalars);
            let before = engine.write_executor_stats();
            let at = Instant::now();
            engine.accept_ingest(rows, format!("c-{round}-{cell}"), [round as u8; 32])?;
            let wall = at.elapsed().as_nanos() as u64;
            let window = Window::between(&before, &engine.write_executor_stats(), wall);
            best[cell] = Some(match best[cell] {
                None => window,
                Some(prior) => prior.min_with(window),
            });
            if let Some(layer) = layer {
                engine.drop_layer(layer)?;
            }
            drop(engine);
        }
    }
    println!("{:>26} {:>14} {:>14}", "cell", "wal_append ms", "window ms");
    for (cell, (label, _)) in cells.iter().enumerate() {
        let w = best[cell].expect("a measured cell");
        println!(
            "{:>26} {:>14.4} {:>14.4}",
            label,
            ms(w.at(WriteStage::WalAppend)),
            ms(w.executor_nanos())
        );
    }
    println!(
        "`derive_records` has no lap of its own: it runs between the `Allocate` lap and the \
         append, so it is charged to `wal_append`. The difference between a row and the first is \
         that pass over that layer's vocabulary — **and, on the two declared cells, the \
         `Vocabularies` clone `mint_window_codes` takes in the same interval**, which is O(the \
         same vocabulary). The map alone above is what separates the two."
    );
    Ok(())
}

fn column_name(fx: &Fixture, index: usize) -> String {
    let bundle = open_bundle(&fx.root).expect("the fixture opened once already");
    bundle.manifest.declared_scalars[index].name.clone()
}

/// Declare a vocabulary of `size` keys and a category column over it at the running service, and
/// answer the column's name. Values are minted in pages, as a client would send them.
fn declare_vocabulary_column(
    engine: &Engine,
    size: usize,
    tag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    const PAGE: usize = 50_000;
    let name = format!("write-cost-vocabulary-{tag}");
    let value = |i: usize| tessera_lifecycle::DeclaredValue {
        key: format!("key-{i:08}"),
        title: None,
    };
    engine.declare_vocabulary(tessera_lifecycle::VocabularyRequest {
        name: name.clone(),
        title: None,
        kind: tessera_types::vocabulary::VocabularyKind::Declared,
        visibility: tessera_types::vocabulary::Visibility::Derived,
        width: "u32".to_string(),
        values: (0..size.min(PAGE)).map(value).collect(),
        reserved: Vec::new(),
    })?;
    let mut minted = size.min(PAGE);
    while minted < size {
        let end = (minted + PAGE).min(size);
        engine.mint_vocabulary_values(name.clone(), (minted..end).map(value).collect())?;
        minted = end;
    }
    let column = format!("write_cost_category_{}", tag.replace('-', "_"));
    engine.declare_attribute(tessera_lifecycle::AttributeRequest {
        name: column.clone(),
        title: None,
        ty: "category".to_string(),
        vocabulary: Some(name),
        analyser: None,
        index: false,
        render: false,
        scope: Default::default(),
    })?;
    Ok(column)
}

/// Register a predicate layer over `field` — `membership = {{ attribute = f }}` — and answer its
/// name. It declares no content: a predicate layer's artifacts are derived from the rule, and
/// computed content is refused there.
fn register_predicate(
    engine: &Engine,
    view: &str,
    field: &str,
    tag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let name = format!("write-cost/predicate-{tag}");
    engine.register_layer(predicate_layer(&name, view, field))?;
    Ok(name)
}

fn predicate_layer(name: &str, view: &str, field: &str) -> tessera_types::layer::LayerDeclaration {
    tessera_types::layer::LayerDeclaration {
        scope: Default::default(),
        name: name.to_string(),
        title: None,
        views: vec![view.to_string()],
        membership: tessera_types::layer::MembershipSource::Attribute(field.to_string()),
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: tessera_types::layer::Hierarchy {
            kind: tessera_types::layer::HierarchyKind::Flat,
            prune_children: false,
        },
        content: Default::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

// =================================================================================================
// D: the flushable count a tick behind a flush takes
// =================================================================================================

fn experiment_d(depths: &[usize], rounds: usize) {
    println!("\n== D: `tick_behind_flush`'s walk of the whole buffer ==");
    println!(
        "{:>10} {:>13} {:>13} {:>13}",
        "buffered", "walk ms", "ns/row", "maintained ns"
    );
    let overlay = Overlay::new();
    for depth in depths {
        let mut buffer = IngestBuffer::new();
        for id in 0..*depth as u64 {
            let row = WalRow {
                external_id: Some(format!("d-{id}").into_bytes()),
                entity_id: EntityId::new(id),
                view: "v".to_string(),
                join: false,
                descriptors: Vec::new(),
                x: 0.0,
                y: 0.0,
                scalars: Vec::new(),
                scoped: Vec::new(),
            };
            buffer.insert_row_with_terms(&row, vec![TermId::new(1), TermId::new(2), TermId::new(3)]);
        }
        let mut best = u64::MAX;
        for _ in 0..rounds.max(5) {
            let at = Instant::now();
            let flushable = buffer
                .iter()
                .filter(|(entity, _)| !overlay.is_deleted(**entity))
                .count();
            best = best.min(at.elapsed().as_nanos() as u64);
            std::hint::black_box(flushable);
        }
        // What the tick asks for now: the maintained row count, read rather than walked.
        let mut maintained = u64::MAX;
        for _ in 0..rounds.max(5) {
            let at = Instant::now();
            let flushable = buffer.len();
            maintained = maintained.min(at.elapsed().as_nanos() as u64);
            std::hint::black_box(flushable);
        }
        println!(
            "{:>10} {:>13.4} {:>13.2} {:>13}",
            depth,
            ms(best),
            best as f64 / (*depth).max(1) as f64,
            maintained,
        );
    }
    println!(
        "One tick, not one window: it is paid only when a tick lands while a flush is in flight. \
         `walk` is the filtered walk with its Roaring `contains` per buffered entity; \
         `maintained` is the row counter the buffer keeps, which is what the tick reads."
    );
}

// =================================================================================================

// =================================================================================================
// E: what a deny-only run leaves on disc
// =================================================================================================

/// Every `SEGMENTS-<n>.json` under the bundle copy: how many, and the size of the newest.
fn side_manifests(root: &Path) -> (usize, u64, u64) {
    let mut count = 0usize;
    let mut newest = (0u64, 0u64);
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(n) = name
                .strip_prefix("SEGMENTS-")
                .and_then(|r| r.strip_suffix(".json"))
                .and_then(|r| r.parse::<u64>().ok())
            else {
                continue;
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            count += 1;
            total += size;
            if n >= newest.0 {
                newest = (n, size);
            }
        }
    }
    (count, newest.1, total)
}

/// Every byte under `dir`, files only.
fn bytes_under(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// **What a deny-only run leaves behind.** Every deny is published, and every publication restates
/// the whole deny state, so the bundle grows with the *integral* of the overlay's depth rather than
/// with its depth. One cell is one run to `target` suppressions in windows of `window`, over a
/// fresh copy of the fixture, reporting the bytes the copy holds at the end, how many
/// side-manifests are there, the size of the newest, and the wall clock per deny.
fn experiment_e(
    fx: &Fixture,
    scratch: &Path,
    cells: &[(usize, usize)],
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n== E: what a deny-only run leaves on disc ==");
    println!(
        "{:>10} {:>8} {:>14} {:>14} {:>11} {:>14} {:>12}",
        "denies", "window", "bundle MB", "manifest MB", "manifests", "newest KB", "us/deny"
    );
    for (target, window) in cells {
        copy_bundle(&fx.source, &fx.root)?;
        let empty = bytes_under(&fx.root);
        let engine = open_engine(fx, scratch, &format!("e{target}-{window}"))?;
        let at = Instant::now();
        let mut done = 0usize;
        while done < *target {
            let count = (*window).min(target - done);
            let pending: Vec<_> = (0..count)
                .map(|i| {
                    engine.submit_change(
                        EntityId::new((done + i) as u64 * DENY_STRIDE),
                        ChangeOp::Suppress,
                    )
                })
                .collect::<Result<_, _>>()?;
            for p in pending {
                p.wait()?;
            }
            done += count;
        }
        // The last window's publication happens after its receipt, so the run is quiet before the
        // disc is read.
        let publications = engine.write_executor_stats().overlay_publications;
        let deadline = Instant::now() + std::time::Duration::from_secs(120);
        while engine.write_executor_stats().overlay_publications == publications
            && Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let wall = at.elapsed();
        drop(engine);
        let (count, newest, manifest_bytes) = side_manifests(&fx.root);
        println!(
            "{:>10} {:>8} {:>14.1} {:>14.1} {:>11} {:>14.1} {:>12.1}",
            target,
            window,
            (bytes_under(&fx.root) - empty) as f64 / 1e6,
            manifest_bytes as f64 / 1e6,
            count,
            newest as f64 / 1e3,
            wall.as_micros() as f64 / *target as f64,
        );
    }
    println!(
        "`bundle MB` is what the run added to the copy, `manifest MB` how much of that is \
         side-manifests, and `us/deny` is wall clock over the whole run: at a window of 1 it is \
         the per-publication cost, at 1,000 it is that cost spread over the window."
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut fixture: Option<PathBuf> = None;
    let mut rounds = 5usize;
    let mut only: Option<String> = None;
    let mut max_buffer = 1_000_000usize;
    let mut max_overlay = 5_000_000usize;
    let mut e_cells: Vec<(usize, usize)> = vec![(100_000, 1), (100_000, 1_000), (1_000_000, 1_000)];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixture" => fixture = args.next().map(PathBuf::from),
            "--repeat" => rounds = args.next().and_then(|v| v.parse().ok()).unwrap_or(rounds),
            "--only" => only = args.next(),
            "--max-buffer" => {
                max_buffer = args.next().and_then(|v| v.parse().ok()).unwrap_or(max_buffer)
            }
            "--max-overlay" => {
                max_overlay = args.next().and_then(|v| v.parse().ok()).unwrap_or(max_overlay)
            }
            // `--e-cells 20000:1,300000:1000` — one `target:window` pair per cell. The default
            // set writes tens of gigabytes on a build that keeps every side-manifest.
            "--e-cells" => {
                if let Some(raw) = args.next() {
                    e_cells = raw
                        .split(',')
                        .map(|cell| {
                            let (target, window) = cell.split_once(':').expect("target:window");
                            (
                                target.parse().expect("a target"),
                                window.parse().expect("a window"),
                            )
                        })
                        .collect();
                }
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    let Some(source) = fixture else {
        return Err("--fixture <bundle root> is required".into());
    };
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — every number below is meaningless. Use --release.");
    }
    if !cfg!(feature = "bench-timing") {
        return Err("built without `bench-timing`: every `WriteStage` figure would be zero".into());
    }

    let scratch = std::env::temp_dir().join(format!("tessera-write-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    let root = scratch.join("bundle");
    copy_bundle(&source, &root)?;

    let prefix = current_prefix(&root)?;
    let fx = inspect(&source, &root, &prefix)?;
    println!(
        "fixture {} (copied to {}) — {} items, view {}, {} descriptors/row, {} declared scalar(s)\n",
        source.display(),
        root.display(),
        fx.high_water,
        fx.view,
        fx.descriptors.len(),
        fx.declared,
    );

    let buffer_depths: Vec<usize> = [0usize, 50_000, 200_000, 500_000, 1_000_000]
        .into_iter()
        .filter(|d| *d <= max_buffer)
        .collect();
    let clone_depths = [0usize, 100_000, 1_000_000, 5_000_000];
    let overlay_depths: Vec<usize> = clone_depths
        .into_iter()
        .filter(|d| *d <= max_overlay)
        .collect();

    let wanted = |letter: &str| only.as_deref().is_none_or(|o| o.contains(letter));
    if wanted("a") {
        experiment_a(&fx, &scratch, &buffer_depths, rounds)?;
    }
    if wanted("b") {
        experiment_b(&fx, &scratch, &overlay_depths, &clone_depths, rounds)?;
    }
    if wanted("c") {
        experiment_c(&fx, &scratch, rounds)?;
    }
    if wanted("d") {
        experiment_d(&buffer_depths, rounds);
    }
    if wanted("e") {
        experiment_e(&fx, &scratch, &e_cells)?;
    }

    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}
