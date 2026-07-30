//! **Asks 5 and 6: blank-database ingest, and update ingest (batch and continuous).**
//!
//! Three modes, measuring three different things that all get called "ingest":
//!
//! * `build` — a bundle from nothing, decomposed into the pipeline's own eleven stages. The
//!   question is not "how long" (the shell already knows) but *which stage bends with scale*.
//! * `batch` — `Engine::accept_ingest` at varying batch sizes against a live engine. The ack path
//!   is WAL append → fsync → buffer swap → generation swap, all under one mutex, so this measures
//!   the cost a `/control/ingest` caller actually waits on.
//! * `continuous` — single-item writes, continuously, while a reader draws viewports. This is the
//!   arm that answers what sustained small writes do to *read* latency, and it is where findings
//!   F2 and F3 should show up as curves rather than as code reading.
//!
//! # What F2 and F3 predict
//!
//! **F2** — `compose` (`tessera-engine/src/compose.rs:183` and `:200`) iterates the *entire*
//! overlay and buffer on every viewport, with a `perm.row_of` per entry. Viewport latency should
//! therefore be **linear in overlay size**, against a configured `overlay_soft_limit` of 500,000.
//! The `compose_ns` stage timing isolates it exactly.
//!
//! **F3** — `accept_ingest`/`accept_change` clone the buffer/overlay per acceptance
//! (`session.rs:658`: `let mut buffer = (*generation.buffer).clone()`), which makes M single-item
//! writes O(M²). The clone is certain from reading; the *cost* is the open question.
//!
//! # What was measured, 2026-07-30, 2.42M `categories-subclass`
//!
//! **F2: confirmed, and linear.** `compose_ns` against buffered items gave 10.28, 10.26, 10.37,
//! 9.52, 9.72 ns per buffered item across 501 → 25,001 — flat over 50x of depth, while `count_ns`
//! and `select_ns` stayed constant. Extrapolating ~10 ns/item to SA §7's intended
//! `overlay_soft_limit` of 500,000 puts `compose` at **~5 ms per viewport**, against a whole
//! request of ~600 us at these fixtures — roughly 8x the entire current request cost.
//!
//! Two caveats on that extrapolation, both load-bearing:
//!
//! * `overlay_soft_limit`, `flush_max_items` and `flush_max_age` are **design-document values, not
//!   implemented** — they appear nowhere in `tessera-server`'s config or in the engine. There is
//!   no limit in code today, so the buffer grows without bound and the extrapolation has no
//!   ceiling rather than a 500,000 one.
//! * The measured ~10 ns/item is a **lower bound**. None of the buffered rows are visible (see
//!   below), so `compose` iterates the buffer and *rejects* every entry at `perm.row_of`. Rows
//!   that resolved would additionally push into `pass_rows`/`fail_rows` and build the diff
//!   bitmaps. Real per-item cost is at least this and probably more.
//!
//! **F3: NOT confirmed by measurement — do not claim it is.** A single-item ack costs ~3.2 ms and
//! is entirely fsync-dominated: a batch of 1 and a batch of 1000 cost the same. Any O(buffer)
//! clone term is buried under that floor at reachable depths. The implied marginal ns-per-buffered-
//! item *decreases* with depth (175, 131, 41, 45), which is what attributing a fixed offset to a
//! growing denominator looks like — not what a linear clone looks like. Compare F2's flat series
//! above; that is what a real linear law reads like in this data.
//!
//! To actually measure F3 you need to separate the clone from the fsync, which means either
//! buffer depths where clone >> 3 ms, or a timer on the write path. `StageTimings` covers the
//! viewport path only; there is no write-path equivalent yet.
//!
//! **Ingest throughput is an fsync amortisation story.** 314 items/s at batch=1 rising to
//! ~1.37M items/s at batch=10,000 — ~4,400x — with the knee around batch=1000, where one fsync is
//! already spread thin enough that per-item work starts to matter.
//!
//! # Scope: what "writes to an existing database" does and does not cover here
//!
//! `batch` and `continuous` both open a real prebuilt bundle and write into it, so they are
//! genuinely update-path measurements, not first-write ones. But Phase 1's update path stops
//! earlier than the word "ingest" suggests, and the arms can only measure what exists:
//!
//! * **Durability — covered.** WAL append → fsync → buffer insert → generation swap, which is the
//!   whole of what `/control/ingest` promises before it acks.
//! * **Visibility — NOT reached.** `sigma_visible` is *identical* at every buffer depth measured
//!   (16,108 across 501 → 25,001 buffered). A buffered entity has no row in the segment's
//!   permutation, so `compose` skips it — `compose.rs`'s rule-4 loop says so outright: "No row in
//!   this segment: Phase 1 has no cross-segment geometry." Ingested rows are durable and
//!   invisible.
//! * **Absorption — does not exist.** No flush, no posting-delta fold, no merge, no compaction.
//!   `EngineError::MultiSegmentSlice` fails closed above one segment per slice. So the steady-state
//!   cost of a database that has been *running* and absorbing writes for a while is unmeasurable
//!   here, and design §16's "how many live segments before per-tile fan-out is noticeable" stays
//!   open — see this module's note rather than assuming the continuous arm answered it.
//! * **`accept_change` — not benchmarked at all.** Deletes, suppressions, unsuppressions and
//!   predicate changes are the *other* write path, and the one with a security-relevant latency
//!   bound (lifecycle §1.3: deny visibility is bounded by queue-front + fsync, and
//!   `/control/changes` is never refused for capacity because refusing a security operation for
//!   load is fail-open). It deserves its own arm.

use std::sync::Mutex;
use std::time::Duration;

use tessera_build::{BuildArgs, BuildObserver, BuildStage};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::WalRow;
use tessera_lifecycle::PendingItem;
use tessera_plugin::Passthrough;
use tessera_spatial::Extent;
use tessera_store::read::open_bundle;
use tessera_types::{IdentityKey, TermId};

use crate::arms::{Context, Result};
use crate::corpus::{build_grant_to_coverage, gen_viewports, Dictionary, GrantShape, TermStats};
use crate::report::{Stages, Work};

/// The same fixed test key `scripts/bench_build_fixtures.sh` uses, so a bundle built here is
/// byte-comparable with the prebuilt fixtures.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// Collects stage timings from a build.
#[derive(Default)]
struct StageCollector {
    stages: Mutex<Vec<(BuildStage, Duration, u64, u64)>>,
}

impl BuildObserver for StageCollector {
    fn stage_end(&self, stage: BuildStage, elapsed: Duration, rows: u64, peak_rss_kib: u64) {
        self.stages
            .lock()
            .unwrap()
            .push((stage, elapsed, rows, peak_rss_kib));
    }
}

// ---------------------------------------------------------------------------------------------
// Mode: build
// ---------------------------------------------------------------------------------------------

/// Blank-database ingest, decomposed.
///
/// # Measured, 2026-07-30, `categories-subclass`, seconds (share of build)
///
/// | stage | 250k | 2.42M | 25M |
/// |---|---|---|---|
/// | `source_ids` | 11.8 (69%) † | 5.3 (29%) | 6.5 (5%) |
/// | `dictionary` | 0.1 | 0.2 (1%) | 2.5 (2%) |
/// | `pairs_pack` | 0.1 | 0.3 (2%) | 3.4 (3%) |
/// | **`signature_sort`** | 0.1 | 2.8 (15%) | **53.4 (45%)** |
/// | `assignment` | 2.4 (14%) | 2.7 (15%) | 3.6 (3%) |
/// | `postings_write` | 0.0 | 0.4 (2%) | 4.9 (4%) |
/// | **`external_ids`** | 0.2 (1%) | 2.3 (12%) | **24.0 (20%)** |
/// | `geometry_scan` | 2.5 (14%) | 3.8 (21%) | 15.1 (13%) |
/// | `tiler_sort` | 0.0 | 0.1 | 2.9 (2%) |
/// | `segment_write` | 0.0 | 0.2 (1%) | 2.0 (2%) |
/// | `manifests` | 0.0 | 0.1 | 0.6 |
/// | **total** | **17.1** | **18.2** | **118.9** |
///
/// † The 250k row ran first, against a cold page cache, so its `source_ids` includes a cold read
/// of the 6.6 GB `geometry.parquet`. Later builds hit the cache. Compare 2.42M against 25M; treat
/// the 250k column as contaminated.
///
/// **`signature_sort` is the stage that bends.** 2.8 s → 53.4 s for 10x the items — 19x, clearly
/// superlinear — and 45% of the 25M build. It is also the one stage that cannot be skipped or
/// deferred: assignment is permanent under I9.
///
/// **`external_ids` is the second cost, and it is avoidable.** 2.3 s → 24.0 s, near-perfectly
/// linear, 20% of the build. The same sidecar is 42% of bundle *bytes*, and is synthesised for
/// every row from the source entity id even though the corpus supplies no external ids
/// (`pipeline.rs`, stage 7). A build flag to skip it would take ~20% off build time and ~42% off
/// disk — but it backs `/v1/items` drill-down and the C-5 constant-time property, so it is a
/// deliberate trade, not free.
///
/// **Bundle size is 47.0 B/item, dead flat across 100x of scale** (47.1 / 47.0 / 47.0).
///
/// A prediction that was in this comment and is now falsified: the pairs file is sorted by
/// `(term_id, entity_id)` and so cannot be row-group-pruned by an `entity_id < limit` filter, from
/// which I expected `dictionary`/`pairs_pack`/`postings_write` to be roughly flat across scales.
/// They are not — they scale with `--limit` (0.1/0.1/0.0 → 2.5/3.4/4.9). The per-row processing,
/// not the file read, dominates those passes; the three reads are also cache-warm after the first.
pub fn run_build(
    ctx: &Context,
    scales: &[u64],
    label_sets: &[String],
    data_root: &std::path::Path,
) -> Result<()> {
    let mut run = ctx.open("ingest_build")?;

    let geometry = data_root.join("data/scaled/geometry.parquet");
    if !geometry.exists() {
        return Err(format!("missing {}", geometry.display()).into());
    }

    for label_set in label_sets {
        let pairs = data_root.join(format!("data/scaled/pairs/{label_set}.pairs.parquet"));
        if !pairs.exists() {
            eprintln!("ingest_build: no pairs file for {label_set}, skipping");
            continue;
        }
        let pairs_bytes = std::fs::metadata(&pairs).map(|m| m.len()).unwrap_or(0);

        for &scale in scales {
            let cell_id = format!("ingest_build/{scale}/{label_set}");
            if run.ledger.is_done(&cell_id) {
                run.skipped += 1;
                continue;
            }

            let out = std::env::temp_dir().join(format!("tessera-bench-build-{scale}-{label_set}"));
            let _ = std::fs::remove_dir_all(&out);

            let collector = StageCollector::default();
            let args = BuildArgs {
                points: geometry.clone(),
                pairs: pairs.clone(),
                out: out.clone(),
                extent: Extent {
                    x_min: 0.0,
                    x_max: 65536.0,
                    y_min: 0.0,
                    y_max: 65536.0,
                },
                slice_id: "s0".to_string(),
                limit: Some(scale),
                identity_key: IdentityKey::from_hex(TEST_KEY_HEX)?,
                identity_key_hex: TEST_KEY_HEX.to_string(),
                identity_epoch: 1,
                shard_id: 0,
                mint_external_ids: true,
                emit_oracle_pairs: true,
            };

            eprintln!("ingest_build: scale={scale} set={label_set} (one repetition — a build is minutes, not microseconds)");
            let start = std::time::Instant::now();
            let report = tessera_build::build_observed(&args, &collector)?;
            let total_ns = start.elapsed().as_nanos() as u64;

            let stages = collector.stages.lock().unwrap().clone();
            let per_stage: serde_json::Map<String, serde_json::Value> = stages
                .iter()
                .map(|(stage, elapsed, rows, rss)| {
                    (
                        stage.name().to_string(),
                        serde_json::json!({
                            "ns": elapsed.as_nanos() as u64,
                            "pct": 100.0 * elapsed.as_nanos() as f64 / total_ns.max(1) as f64,
                            "rows": rows,
                            "peak_rss_kib": rss,
                        }),
                    )
                })
                .collect();

            let work = Work {
                mask_cardinality: report.items,
                rows_in_ranges: report.pairs,
                bytes_touched: report.bundle_bytes,
                ..Default::default()
            };

            // A build is minutes; min-of-N is unaffordable and would not help — the variance
            // being absorbed elsewhere in this suite is scheduler noise on microsecond kernels,
            // not on a multi-minute IO-bound pipeline. One repetition, stated as such.
            run.emit(
                cell_id,
                &crate::fixture::Fixture {
                    scale,
                    label_set: label_set.clone(),
                    root: out.clone(),
                    prefix: report.prefix.clone(),
                    bytes: report.bundle_bytes,
                },
                serde_json::json!({
                    "items": report.items,
                    "terms": report.terms,
                    "pairs": report.pairs,
                    "bundle_bytes": report.bundle_bytes,
                    "pairs_file_bytes": pairs_bytes,
                    "bytes_per_item": report.bundle_bytes as f64 / report.items.max(1) as f64,
                    "stages": per_stage,
                }),
                work,
                vec![total_ns],
                None,
                vec!["single_repetition".to_string()],
            )?;

            let _ = std::fs::remove_dir_all(&out);
        }
    }

    run.finish();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Modes: batch and continuous
// ---------------------------------------------------------------------------------------------

/// Synthetic rows to ingest, with entity IDs allocated the way the control plane allocates them.
///
/// Terms are real dictionary terms so the buffered items are genuinely *visible* to the reading
/// principal — ingesting items the reader cannot see would exercise none of the composition path
/// and would make the F2 measurement meaningless.
///
/// **Entity IDs come from `Engine::allocate_sorted`, not from a counter.** Assignment is
/// signature-sorted and permanent under I9; `/control/ingest` goes through the allocator
/// (`control.rs:329-354`) and so must this, or the buffered items would carry IDs inconsistent
/// with the allocator's high-water mark and the measurement would describe a state the system
/// cannot reach.
fn synth_rows(
    engine: &Engine,
    count: usize,
    start: u64,
    terms: &[TermId],
) -> Result<(Vec<WalRow>, Vec<Vec<TermId>>)> {
    let mut pending: Vec<PendingItem> = (0..count)
        .map(|i| PendingItem {
            external_id: Some(format!("bench-{}", start + i as u64).into_bytes()),
            terms: terms.to_vec(),
            entity_id: None,
        })
        .collect();
    engine.allocate_sorted(&mut pending)?;

    let mut rows = Vec::with_capacity(count);
    let mut row_terms = Vec::with_capacity(count);
    for (i, item) in pending.into_iter().enumerate() {
        let n = start + i as u64;
        rows.push(WalRow {
            external_id: item.external_id,
            entity_id: item.entity_id.expect("allocate_sorted assigns every item"),
            descriptors: Vec::new(),
            x: ((n * 37) % 65536) as f32,
            y: ((n * 53) % 65536) as f32,
            scalars: Vec::new(),
        });
        row_terms.push(terms.to_vec());
    }
    Ok((rows, row_terms))
}

/// Batch ingest: ack latency as a function of batch size.
pub fn run_batch(ctx: &Context, batch_sizes: &[usize], seed: u64) -> Result<()> {
    let mut run = ctx.open("ingest_batch")?;

    for fixture in &ctx.fixtures {
        let postings = tessera_authz::PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let (grant, _) = build_grant_to_coverage(
            &stats,
            &postings,
            GrantShape::Random,
            0.05,
            fixture.scale,
            seed,
        )?;
        if grant.terms.is_empty() {
            continue;
        }

        for &batch in batch_sizes {
            let cell_id = format!(
                "ingest_batch/{}/{}/b{}",
                fixture.scale, fixture.label_set, batch
            );
            if run.ledger.is_done(&cell_id) {
                run.skipped += 1;
                continue;
            }

            // A fresh engine per cell: an accumulated buffer would make later batches slower
            // for a reason that has nothing to do with batch size, which is exactly the
            // confound the `continuous` mode measures deliberately.
            let tmp = std::env::temp_dir().join(format!(
                "tessera-bench-ingest-{}-{}",
                std::process::id(),
                batch
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            let engine = Engine::open(
                &fixture.root,
                &tmp.join("cache"),
                &tmp.join("wal.log"),
                Passthrough::new(),
                EngineConfig {
                    token_max_lifetime_secs: 3600,
                    max_k: 200,
                    // §7.2's selection constants, at the server's own defaults — a bench measuring
                    // anything else measures a configuration nobody runs. Keep in step with
                    // `tessera-server`'s DEFAULT_* constants.
                    k_min: 2,
                    k_max_marks: 500,
                    theta_target_marks: 16,
                    max_underlay_offset: 4,
                    max_underlay_cells: 8192,
                    max_tiles_per_request: 262_144,
                },
            )?;

            let mut next_id = 0u64;
            let mut samples = Vec::new();
            for rep in 0..ctx.repeat {
                let (rows, terms) = synth_rows(&engine, batch, next_id, &grant.terms)?;
                next_id += batch as u64;
                let batch_id = format!("bench-{batch}-{rep}");
                let start = std::time::Instant::now();
                engine.accept_ingest(rows, terms, batch_id, [rep as u8; 32])?;
                samples.push(start.elapsed().as_nanos() as u64);
            }

            let min = *samples.iter().min().unwrap();
            let work = Work {
                points_gathered: batch as u64,
                ..Default::default()
            };

            run.emit(
                cell_id,
                fixture,
                serde_json::json!({
                    "batch_size": batch,
                    "ns_per_item": min as f64 / batch as f64,
                    "items_per_sec": 1e9 * batch as f64 / min as f64,
                    // Each repetition leaves its rows in the buffer, so a rising sample sequence
                    // within one cell is F3's O(buffer) clone showing up directly.
                    "samples_in_order_ns": samples.clone(),
                }),
                work,
                samples,
                None,
                Vec::new(),
            )?;

            drop(engine);
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    run.finish();
    Ok(())
}

/// Continuous small writes, with a reader drawing viewports as the overlay grows.
///
/// Reports two curves against buffered-item count: **write-ack latency** (F3) and **viewport
/// latency decomposed by stage** (F2, via `compose_ns`). Checkpoints bracket
/// `overlay_soft_limit = 500_000` so the configured limit can be compared against the point where
/// read latency actually degrades.
pub fn run_continuous(ctx: &Context, checkpoints: &[u64], k: usize, seed: u64) -> Result<()> {
    let mut run = ctx.open("ingest_continuous")?;

    for fixture in &ctx.fixtures {
        let postings = tessera_authz::PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let dictionary = Dictionary::open(&fixture.root, &fixture.prefix)?;
        let (grant, coverage) = build_grant_to_coverage(
            &stats,
            &postings,
            GrantShape::Random,
            0.05,
            fixture.scale,
            seed,
        )?;
        if grant.terms.is_empty() {
            continue;
        }

        let bundle = open_bundle(&fixture.root)?;
        let extent_span = bundle.manifest.quantisation.x_max - bundle.manifest.quantisation.x_min;
        let slice_id = bundle
            .partitions
            .values()
            .next()
            .and_then(|p| p.slices.keys().next().cloned())
            .unwrap_or_else(|| "s0".to_string());
        drop(bundle);

        let tmp = std::env::temp_dir().join(format!(
            "tessera-bench-continuous-{}-{}",
            std::process::id(),
            fixture.scale
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let engine = Engine::open(
            &fixture.root,
            &tmp.join("cache"),
            &tmp.join("wal.log"),
            Passthrough::new(),
            EngineConfig {
                token_max_lifetime_secs: 3600,
                max_k: k.max(200),
                // §7.2's selection constants, at the server's own defaults — a bench measuring
                // anything else measures a configuration nobody runs. Keep in step with
                // `tessera-server`'s DEFAULT_* constants.
                k_min: 2,
                k_max_marks: 500,
                theta_target_marks: 16,
                max_underlay_offset: 4,
                max_underlay_cells: 8192,
                max_tiles_per_request: 262_144,
            },
        )?;
        let session = engine.authorise(grant.auth_json(&dictionary).as_bytes())?;

        // One fixed viewport for the whole run: the reader's geometry must not vary, or a change
        // in read latency cannot be attributed to the growing overlay. Chosen as the densest of a
        // probe pool so there is real work to slow down.
        let probe = gen_viewports(200, extent_span, seed, &[8]);
        let mut best = (0u64, probe[0]);
        for (z, bbox) in &probe {
            let out = engine.viewport(&session, ViewportRequest::new(&slice_id, *z, *bbox, 0))?;
            if out.timings.sigma_visible > best.0 {
                best = (out.timings.sigma_visible, (*z, *bbox));
            }
        }
        let (rz, rbbox) = best.1;
        // Warm the row-projection cache before any sample (the first viewport of a session pays
        // for the whole entity->row crossing).
        engine.viewport(&session, ViewportRequest::new(&slice_id, rz, rbbox, k))?;

        let mut buffered = 0u64;
        let mut next_id = 0u64;
        for &checkpoint in checkpoints {
            while buffered < checkpoint {
                let (rows, terms) = synth_rows(&engine, 1, next_id, &grant.terms)?;
                next_id += 1;
                let start = std::time::Instant::now();
                engine.accept_ingest(rows, terms, format!("c-{next_id}"), [0u8; 32])?;
                let ack_ns = start.elapsed().as_nanos() as u64;
                buffered += 1;
                std::hint::black_box(ack_ns);
            }

            let cell_id = format!(
                "ingest_continuous/{}/{}/buffered{}",
                fixture.scale, fixture.label_set, checkpoint
            );
            if run.ledger.is_done(&cell_id) {
                run.skipped += 1;
                continue;
            }

            // Read latency at this buffer depth, same viewport every time.
            let mut last = None;
            let read_samples = crate::metrics::repeat(ctx.repeat, || {
                let out = engine
                    .viewport(&session, ViewportRequest::new(&slice_id, rz, rbbox, k))
                    .expect("viewport");
                last = Some(out.timings);
                out
            });
            let Some(t) = last else { continue };

            // The ack cost right now, measured cleanly rather than sampled mid-stream.
            let ack_now = {
                let (rows, terms) = synth_rows(&engine, 1, next_id, &grant.terms)?;
                next_id += 1;
                buffered += 1;
                let start = std::time::Instant::now();
                engine.accept_ingest(rows, terms, format!("c-probe-{next_id}"), [0u8; 32])?;
                start.elapsed().as_nanos() as u64
            };

            let work = Work {
                coverage,
                tiles_resolved: t.tiles_resolved,
                tiles_nonempty: t.tiles_nonempty,
                sigma_visible: t.sigma_visible,
                rows_in_ranges: t.rows_in_ranges,
                rows_materialised: t.select_rows_visited,
                underlay_cells_evaluated: t.underlay_cells_evaluated,
                points_gathered: t.points_gathered,
                ..Default::default()
            };

            run.emit(
                cell_id,
                fixture,
                serde_json::json!({
                    "buffered_items": buffered,
                    "overlay_soft_limit": 500_000,
                    "ack_ns_at_this_depth": ack_now,
                    "k": k,
                    "zoom": rz,
                    // F2's signal: compose is linear in overlay+buffer size, so this share should
                    // climb with `buffered_items` while every other stage stays flat.
                    "compose_ns": t.compose_ns,
                    "compose_pct": 100.0 * t.compose_ns as f64 / t.total_ns.max(1) as f64,
                    "seed": seed,
                }),
                work,
                read_samples,
                Some(Stages::from_engine(&t, run.clock_lap_ns)),
                Vec::new(),
            )?;
        }

        drop(engine);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    run.finish();
    Ok(())
}
