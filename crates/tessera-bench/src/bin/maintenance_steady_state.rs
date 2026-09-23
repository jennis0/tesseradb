//! What the row-space merge and the entity-space coalesce leave behind over a long steady ingest.
//!
//! One view takes `--rows` new items per tick for `--ticks` ticks, each with an external id, a
//! keyword held in the record blob, an indexed keyword and an indexed text column, and one novel
//! descriptor so every flush promotes a dictionary extent. Merge and coalesce run at their default
//! policies; no fold runs.
//!
//! `--hold-merge` ticks run first with the merge unable to select anything, then the engine
//! reopens with the default policy, which is the state a restart after a backlog leaves.
//!
//! After every tick, once nothing under the prefix and no maintenance counter has changed for
//! `QUIET`, it prints one row: segments listed for the view, the entries in each entity-space
//! list, files and bytes under the live prefix, bytes of the view's row-space files and of every
//! external-id run and locator, the merge and coalesce output directories on disc, and the
//! executor's published and failed counts. Each attempt writes its own directory, and one
//! that neither published nor failed was discarded at its rebase, so `abandoned` is directories
//! less published less failed.
//!
//! Every side-manifest still on disc after a tick is checked once against the ordering rules the
//! readers rely on: the base run is `external_id_runs[0]`; the other runs are the runs the locator
//! extents name, in the same order; each locator span is well formed (spans may overlap, since a
//! lookup asks every extent covering an entity); each view's segments ascend without overlap in
//! entity order. At the end every ingested binding is looked up both ways through a sidecar opened
//! from the bundle on disc.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin maintenance_steady_state -- \
//!     --ticks 200 --rows 2000 [--hold-merge 9] [--scratch DIR]
//! ```

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use clap::Parser;
use parquet::arrow::ArrowWriter;
use rand::{Rng, SeedableRng};

use tessera_build::{build, BuildArgs};
use tessera_engine::{AttributeRequest, Engine, EngineConfig};
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::UnallocatedRow;
use tessera_store::manifest::SegmentsManifest;
use tessera_store::ExternalIdSidecar;
use tessera_types::layer::LayerScope;
use tessera_types::EntityId;

const VIEW: &str = "s0";
const BASE_ITEMS: u64 = 10_000;
const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const WAIT: Duration = Duration::from_secs(120);
const QUIET: Duration = Duration::from_millis(500);

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 200)]
    ticks: usize,
    #[arg(long, default_value_t = 2000)]
    rows: usize,
    /// Ticks run first with the merge unable to select.
    #[arg(long, default_value_t = 0)]
    hold_merge: usize,
    /// Idle ticks after the last ingest, before the final row.
    #[arg(long, default_value_t = 6)]
    idle: usize,
    #[arg(long)]
    scratch: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let tmp = match &args.scratch {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            tempfile::TempDir::new_in(dir)?
        }
        None => tempfile::TempDir::new()?,
    };
    let root = tmp.path().join("bundle");
    build_fixture(tmp.path(), &root)?;

    let mut engine = open(tmp.path(), &root, args.hold_merge > 0)?;
    for (name, ty, index) in [("note", "keyword", false), ("tag", "keyword", true), ("prose", "text", true)] {
        engine.declare_attribute(AttributeRequest {
            name: name.to_string(),
            title: None,
            ty: ty.to_string(),
            vocabulary: None,
            analyser: None,
            index,
            render: false,
            scope: LayerScope::Entity,
        })?;
    }

    let base_run = live_manifest(&engine).external_id_runs.first().cloned();
    let mut probe = Probe::new(&root, base_run);
    let mut bindings: Vec<(EntityId, Vec<u8>)> = Vec::new();
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let common = engine.resolve_terms(&[b"0".to_vec()]);

    Row::header();
    let mut rows_out: Vec<Row> = Vec::new();
    for tick in 0..args.ticks + args.idle {
        if args.hold_merge > 0 && tick == args.hold_merge {
            probe.carry(&engine.write_executor_stats());
            drop(engine);
            engine = open(tmp.path(), &root, false)?;
        }
        if tick < args.ticks {
            let novel = format!("novel-{tick}").into_bytes();
            let novel_terms = engine.resolve_terms(std::slice::from_ref(&novel));
            let batch: Vec<UnallocatedRow> = (0..args.rows)
                .map(|i| {
                    let key = format!("ext-{tick:05}-{i:06}").into_bytes();
                    let (descriptors, terms) = if i == 0 {
                        (vec![novel.clone()], novel_terms.clone())
                    } else {
                        (vec![b"0".to_vec()], common.clone())
                    };
                    UnallocatedRow {
                        external_id: Some(key),
                        view: VIEW.to_string(),
                        join: None,
                        descriptors,
                        x: rng.gen_range(0.0..1000.0),
                        y: rng.gen_range(0.0..1000.0),
                        scalars: vec![
                            WalScalar::Utf8(format!("note {i}")),
                            WalScalar::Utf8(format!("tag-{}", i % 17)),
                            WalScalar::Utf8(format!("word{} word{}", i % 31, tick % 13)),
                        ],
                        scoped: Vec::new(),
                        terms,
                    }
                })
                .collect();
            let keys: Vec<Vec<u8>> = batch.iter().map(|r| r.external_id.clone().unwrap()).collect();
            let mut digest = [0u8; 32];
            digest[..8].copy_from_slice(&(tick as u64).to_le_bytes());
            let entities = engine.accept_ingest(batch, format!("tick-{tick}"), digest)?;
            bindings.extend(entities.into_iter().zip(keys));
        }
        let before = engine.write_executor_stats();
        engine.request_flush();
        let deadline = Instant::now() + WAIT;
        loop {
            let now = engine.write_executor_stats();
            let done = if tick < args.ticks {
                now.flushes > before.flushes
            } else {
                now.ticks > before.ticks
            };
            if done {
                break;
            }
            if Instant::now() > deadline {
                return Err(format!("tick {tick} did not complete within {WAIT:?}").into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        settle(&root, &engine)?;
        let row = probe.sample(tick, &engine)?;
        row.print();
        rows_out.push(row);
    }

    let last = rows_out.last().expect("at least one tick");
    println!();
    println!("summary (max over ticks / final)");
    for (name, get) in Row::columns() {
        let max = rows_out.iter().map(get).max().unwrap_or(0);
        println!("  {name:<18} {max:>10} {:>10}", get(last));
    }
    println!("  manifests checked  {:>10}", probe.checked.len());
    println!("  ordering breaches  {:>10}", probe.breaches.len());
    for breach in probe.breaches.iter().take(20) {
        println!("    {breach}");
    }

    let high_water = live_manifest(&engine).entity_id_high_water;
    drop(engine);
    let (checked, wrong) = check_bindings(&root, &bindings, high_water)?;
    println!("  bindings checked   {checked:>10}");
    println!("  bindings wrong     {wrong:>10}");
    Ok(())
}

fn open(tmp: &Path, root: &Path, merge_held: bool) -> Result<Engine, Box<dyn std::error::Error>> {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
            k_min: 2,
            k_max_marks: 200,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
            // Every tick is one this binary asks for.
            flush_max_age_secs: 86_400,
            flush_max_items: usize::MAX,
            // One byte: no window of segments fits, so the merge selects nothing.
            max_merged_segment_bytes: merge_held.then_some(1),
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )?;
    engine.start_write_executor(4096)?;
    Ok(engine)
}

fn build_fixture(tmp: &Path, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    let ids: Vec<u64> = (0..BASE_ITEMS).collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(ids.iter().map(|e| ((e * 37) % 1000) as f64).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(ids.iter().map(|e| ((e * 53) % 1000) as f64).collect::<Vec<_>>())),
        ],
    )?;
    let mut w = ArrowWriter::try_new(File::create(&points)?, schema, None)?;
    w.write(&batch)?;
    w.close()?;

    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(UInt32Array::from(vec![0u32; ids.len()])),
        ],
    )?;
    let mut w = ArrowWriter::try_new(File::create(&pairs)?, schema, None)?;
    w.write(&batch)?;
    w.close()?;

    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: VIEW.to_string(),
            projection: tessera_spatial::Projection::None,
            extent: tessera_spatial::Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points,
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: tessera_types::IdentityKey::from_hex(KEY_HEX).expect("key"),
        identity_key_hex: KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })?;
    Ok(())
}

fn live_manifest(engine: &Engine) -> SegmentsManifest {
    let generation = engine.generation();
    let (_, partition) = generation.bundle.partitions.iter().next().expect("one partition");
    partition.manifest.clone()
}

#[derive(Clone, Copy, Default)]
struct Row {
    tick: u64,
    segments: u64,
    deltas: u64,
    runs: u64,
    locators: u64,
    dicts: u64,
    attrs: u64,
    records: u64,
    texts: u64,
    terms: u64,
    files: u64,
    mib: u64,
    geometry_mib: u64,
    runs_mib: u64,
    merge_dirs: u64,
    merges: u64,
    merge_failures: u64,
    merge_abandoned: u64,
    coalesce_dirs: u64,
    coalesces: u64,
    coalesce_failures: u64,
    coalesce_abandoned: u64,
}

type Column = (&'static str, fn(&Row) -> u64);

impl Row {
    fn columns() -> Vec<Column> {
        vec![
            ("segments", |r| r.segments),
            ("deltas", |r| r.deltas),
            ("runs", |r| r.runs),
            ("locators", |r| r.locators),
            ("dicts", |r| r.dicts),
            ("attrs", |r| r.attrs),
            ("records", |r| r.records),
            ("texts", |r| r.texts),
            ("entity_terms", |r| r.terms),
            ("files", |r| r.files),
            ("MiB", |r| r.mib),
            ("geometry_MiB", |r| r.geometry_mib),
            ("runs_MiB", |r| r.runs_mib),
            ("merge_dirs", |r| r.merge_dirs),
            ("merges", |r| r.merges),
            ("merge_failed", |r| r.merge_failures),
            ("merge_abandoned", |r| r.merge_abandoned),
            ("coalesce_dirs", |r| r.coalesce_dirs),
            ("coalesces", |r| r.coalesces),
            ("coalesce_failed", |r| r.coalesce_failures),
            ("coalesce_abandoned", |r| r.coalesce_abandoned),
        ]
    }

    fn header() {
        let names: Vec<&str> = std::iter::once("tick").chain(Self::columns().iter().map(|c| c.0)).collect();
        println!("{}", names.join("\t"));
    }

    fn print(&self) {
        let values: Vec<String> = std::iter::once(self.tick)
            .chain(Self::columns().iter().map(|c| (c.1)(self)))
            .map(|v| v.to_string())
            .collect();
        println!("{}", values.join("\t"));
    }
}

struct Probe {
    root: PathBuf,
    /// Published and failed counts of engines already closed: merges, merge failures, coalesces,
    /// coalesce failures.
    carried: [u64; 4],
    base_run: Option<String>,
    checked: BTreeSet<(String, u64)>,
    breaches: Vec<String>,
}

impl Probe {
    fn new(root: &Path, base_run: Option<String>) -> Self {
        Probe {
            root: root.to_path_buf(),
            carried: [0; 4],
            base_run,
            checked: BTreeSet::new(),
            breaches: Vec::new(),
        }
    }

    fn carry(&mut self, stats: &tessera_engine::ExecutorStats) {
        self.carried[0] += stats.merges;
        self.carried[1] += stats.merge_failures;
        self.carried[2] += stats.coalesces;
        self.carried[3] += stats.coalesce_failures;
    }

    fn sample(&mut self, tick: usize, engine: &Engine) -> Result<Row, Box<dyn std::error::Error>> {
        let generation = engine.generation();
        let prefix_dir = self.root.join(&generation.prefix);
        let (partition, data) = generation.bundle.partitions.iter().next().expect("one partition");
        let m = &data.manifest;
        let stats = engine.write_executor_stats();
        let [merges, merge_failures, coalesces, coalesce_failures] = self.carried;
        let merges = merges + stats.merges;
        let merge_failures = merge_failures + stats.merge_failures;
        let coalesces = coalesces + stats.coalesces;
        let coalesce_failures = coalesce_failures + stats.coalesce_failures;

        let (files, bytes) = walk(&prefix_dir, |_| true);
        let segments_dir = tessera_store::view_path(&prefix_dir.join("partitions").join(partition), VIEW).join("segments");
        let merge_dirs = count_dirs(&segments_dir, "merge-");
        let coalesce_dirs = count_dirs(&prefix_dir.join("partitions").join(partition).join("coalesced"), "coalesce-");

        self.check_side_manifests(&prefix_dir, partition, &generation.prefix)?;

        Ok(Row {
            tick: tick as u64,
            segments: m.segments.iter().filter(|s| s.view == VIEW).count() as u64,
            deltas: m.deltas.len() as u64,
            runs: m.external_id_runs.len() as u64,
            locators: m.locator_extents.len() as u64,
            dicts: m.dict_extents.len() as u64,
            attrs: m.attr_extents.len() as u64,
            records: m.record_extents.len() as u64,
            texts: m.text_extents.len() as u64,
            terms: m.entity_terms_extents.len() as u64,
            files,
            mib: bytes >> 20,
            geometry_mib: walk(&segments_dir, |name| !is_run(name)).1 >> 20,
            runs_mib: walk(&prefix_dir, is_run).1 >> 20,
            merge_dirs,
            merges,
            merge_failures,
            merge_abandoned: merge_dirs.saturating_sub(merges + merge_failures),
            coalesce_dirs,
            coalesces,
            coalesce_failures,
            coalesce_abandoned: coalesce_dirs.saturating_sub(coalesces + coalesce_failures),
        })
    }

    fn check_side_manifests(&mut self, prefix_dir: &Path, partition: &str, prefix: &str) -> Result<(), Box<dyn std::error::Error>> {
        let dir = prefix_dir.join("partitions").join(partition);
        for entry in std::fs::read_dir(&dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(n) = name.strip_prefix("SEGMENTS-").and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            let n: u64 = n.parse()?;
            if !self.checked.insert((prefix.to_string(), n)) {
                continue;
            }
            // A manifest pruned between listing and reading was superseded; skip it.
            let Ok(raw) = std::fs::read(dir.join(&name)) else {
                continue;
            };
            let manifest: SegmentsManifest = serde_json::from_slice(&raw)?;
            for breach in ordering_breaches(&manifest, self.base_run.as_deref()) {
                self.breaches.push(format!("SEGMENTS-{n}: {breach}"));
            }
        }
        Ok(())
    }
}

fn ordering_breaches(m: &SegmentsManifest, base_run: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(base) = base_run {
        if m.external_id_runs.first().map(String::as_str) != Some(base) {
            out.push(format!("external_id_runs[0] is {:?}, not the base run", m.external_id_runs.first()));
        }
    }
    let named: Vec<&str> = m.locator_extents.iter().map(|e| e.external_id_run.as_str()).collect();
    let listed: Vec<&str> = m.external_id_runs.iter().skip(1).map(String::as_str).collect();
    if named != listed {
        out.push(format!("runs after the base {listed:?} are not the locator extents' runs {named:?} in order"));
    }
    for run in &named {
        if !m.external_id_runs.iter().any(|r| r == run) {
            out.push(format!("locator extent names unlisted run {run}"));
        }
    }
    for extent in &m.locator_extents {
        if extent.entity_lo > extent.entity_hi {
            out.push(format!("locator span {}..={} is empty", extent.entity_lo, extent.entity_hi));
        }
    }
    let views: BTreeSet<&str> = m.segments.iter().map(|s| s.view.as_str()).collect();
    for view in views {
        let segs: Vec<_> = m.segments.iter().filter(|s| s.view == view).collect();
        for pair in segs.windows(2) {
            if pair[0].entity_hi >= pair[1].entity_lo {
                out.push(format!(
                    "view {view}: segments {} ({}..={}) then {} ({}..={}) out of entity order",
                    pair[0].seg_id, pair[0].entity_lo, pair[0].entity_hi, pair[1].seg_id, pair[1].entity_lo, pair[1].entity_hi
                ));
            }
        }
    }
    out
}

fn check_bindings(root: &Path, bindings: &[(EntityId, Vec<u8>)], high_water: u64) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let bundle = tessera_store::open_bundle(root)?;
    let prefix = std::fs::read(root.join("CURRENT"))?;
    let prefix: serde_json::Value = serde_json::from_slice(&prefix)?;
    let prefix_dir = root.join(prefix["prefix"].as_str().ok_or("CURRENT names no prefix")?);
    let (_, partition) = bundle.partitions.iter().next().ok_or("no partition")?;
    let sidecar = ExternalIdSidecar::deferred_from_manifest(&bundle.manifest, &partition.manifest, &prefix_dir)?;
    let mut wrong = 0;
    for chunk in bindings.chunks(50_000) {
        let keys: Vec<Vec<u8>> = chunk.iter().map(|(_, key)| key.clone()).collect();
        let forward = sidecar.resolve_many(&keys)?;
        for ((entity, key), forward) in chunk.iter().zip(forward) {
            let reverse = sidecar.external_id_of_checked(*entity, high_water)?;
            if forward != Some(*entity) || reverse.as_deref() != Some(key.as_slice()) {
                wrong += 1;
            }
        }
    }
    Ok((bindings.len(), wrong))
}

/// Wait until the bytes under the bundle and the maintenance counters hold still for `QUIET`.
fn settle(root: &Path, engine: &Engine) -> Result<(), Box<dyn std::error::Error>> {
    let state = || {
        let s = engine.write_executor_stats();
        (walk(root, |_| true), s.merges, s.merge_failures, s.coalesces, s.coalesce_failures)
    };
    let deadline = Instant::now() + WAIT;
    let mut last = state();
    let mut since = Instant::now();
    while since.elapsed() < QUIET {
        if Instant::now() > deadline {
            return Err("maintenance did not settle".into());
        }
        std::thread::sleep(Duration::from_millis(50));
        let now = state();
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    Ok(())
}

fn is_run(name: &str) -> bool {
    name == "external-ids.arrow" || name == "ext-locator.u32"
}

/// Files and bytes under `dir`, counting the files whose names `keep` accepts.
fn walk(dir: &Path, keep: impl Fn(&str) -> bool) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if keep(&entry.file_name().to_string_lossy()) {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

fn count_dirs(dir: &Path, prefix: &str) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
                .count() as u64
        })
        .unwrap_or(0)
}
