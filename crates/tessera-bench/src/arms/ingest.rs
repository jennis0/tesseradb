//! **Asks 5 and 6: blank-database ingest, and update ingest.**
//!
//! Five modes, because "ingest" names five different questions and one number cannot answer them:
//!
//! * `build` — a bundle from nothing, decomposed into the pipeline's own eleven stages. The
//!   question is not "how long" (the shell already knows) but *which stage bends with scale*.
//! * `rate` — **sustained rows per second**, swept over the three properties that actually move
//!   it: term density, `B/W`, and how many callers submit at once. This is the only arm a
//!   throughput may be quoted from.
//! * `batch` — ack latency against batch size, into a buffer nothing ever flushes. It measures a
//!   *ramp*, not a rate; see below.
//! * `concurrent` — what a commit window **collects** when submissions overlap. Integer counters
//!   (`wal_appends / wal_fsyncs`, the fragmentation tally), not timings.
//! * `continuous` — single-item writes while a reader draws viewports: what sustained small writes
//!   do to *read* latency, which is finding F2's home.
//!
//! Figures live in `docs/evidence/`; this doc carries only the argument for the arms' shape.
//! `docs/evidence/memos/2026-08-05-ingest-rate.md` is this module's, and
//! `docs/evidence/memos/2026-08-05-write-path-at-scale.md` is the write path's at size.
//!
//! # `rate`: why these axes, and not the ones `batch` sweeps
//!
//! `batch` sweeps batch size against a fixed corpus, and both of those turned out to be the wrong
//! knobs. What a commit window's close costs is governed by:
//!
//! * **Term density** — descriptors per row. It is the operand of the buffer clone, of
//!   `assign_sorted`'s signature comparison and of the WAL record, so three stages scale with it
//!   at once. Measured as the dominant factor; corpus size is not.
//! * **`B/W`** — rows buffered between flushes (`B`) over rows per commit-window close (`W`).
//!   `apply_window` copies the whole buffer per close, so a flush interval pays `B²/2W` item
//!   copies and per-row cost is proportional to `B/W`. A sweep that holds the buffer shallow
//!   cannot see the term that dominates a deployment.
//! * **Concurrency.** `accept_ingest` blocks on its receipt, so a *serial* caller gives the window
//!   a queue of one: one close and one fsync per call, and `commit_window_max_items` does nothing
//!   at all (a window also closes when the work queue is observed empty — decision 0034, no
//!   linger). Only under concurrent load does the window gather, and gathering changes `W`, which
//!   changes `B/W`, which is the term above. The two axes are coupled and the arm reports `W` as
//!   **observed**, never as configured.
//!
//! Base corpus size is fixed at one value, deliberately: it is measured as barely moving ingest
//! cost, and paying for it as an axis would buy nothing while halving the resolution of the three
//! that do.
//!
//! **`B/W` is also the number of commit windows per flush cycle**, so at `B/W = r` there are `r`
//! submissions in a cycle and a cell asking for more than `r` concurrent callers cannot keep them
//! all busy. Such cells are measured anyway — a shallow buffer under many callers is a real
//! deployment shape — and report `submitters_effective` beside the requested count.
//!
//! # `batch` has no steady state, and that is the finding
//!
//! Nothing in `batch` flushes, so every repetition in a cell lands on a deeper buffer than the
//! one before it and the samples are a **ramp**. Reading `min` off that ramp reads the depth-0
//! sample — which is how F3 sat unconfirmed for a month, with the clone's signal sitting in
//! samples the headline discarded. The cell therefore reports the *deepest half* of the ramp and
//! labels the depth-0 figure `cold`, and carries the buffer depth beside every sample so the slope
//! is recoverable without a re-run.
//!
//! # Two claims this doc used to make, both refuted by measurement
//!
//! * **Ingest throughput is not an fsync amortisation story.** `wal_fsync` measures 11–24% of a
//!   serial caller's per-row cost at a maximal batch, against `apply_window`'s 38–50%. It was the
//!   story for the batch=1 cell the old headline came from — ~1.37M items/s at batch=10,000, a
//!   4,400x curve off batch=1 — and that headline was `min` over a ramp besides. Withdrawn.
//! * **F3 — the `apply_window` buffer clone — is confirmed**, by `WriteStage` rather than by
//!   inference. It was the largest single stage until a `BufferedItem` went behind an `Arc`; the
//!   `B²/2W` law is untouched by that change, only its constant. Reopening conditions for removing
//!   the term itself are in `docs/evidence/memos/2026-08-05-ingest-buffer-snapshot.md`.
//!
//! F2 — `compose` iterating the whole overlay and buffer per viewport, with a `perm.row_of` per
//! entry — is confirmed and linear, at ~10 ns per buffered item flat over 50x of depth. That is a
//! **lower bound**: none of `continuous`'s buffered rows are visible (see below), so `compose`
//! rejects every entry at `perm.row_of` rather than building the diff bitmaps.
//!
//! # Scope: what "writes to an existing database" does and does not cover here
//!
//! Every update-path arm opens a real prebuilt bundle and writes into it, so these are genuinely
//! update-path measurements, not first-write ones. But the update path stops earlier than the word
//! "ingest" suggests, and the arms can only measure what exists:
//!
//! * **Durability — covered.** WAL append → fsync → buffer insert → generation swap, which is the
//!   whole of what `/control/ingest` promises before it acks.
//! * **Visibility — reached by `rate` alone.** `rate` drives the tick and waits for the flush, so
//!   its rows acquire geometry; every other arm leaves them durable and invisible, and reports the
//!   same `sigma_visible` at every buffer depth because a buffered entity has no row and `compose`
//!   has nowhere to put its verdict. That is a property of those arms rather than of the system —
//!   flush gives a buffered row geometry within `flush_max_age_secs` (write-path §4).
//!
//!   `rate` therefore writes into a **private copy** of the fixture bundle, one per cell: a flush
//!   publishes a segment and a side-manifest into the bundle root, so an arm that flushed a shared
//!   fixture would leave every later cell — and every other arm's — measuring a different corpus.
//! * **Absorption — partly measured, and not here.** Flush, both halves of merge and the
//!   background refresh exist; the compaction fold does not. What these arms still cannot show is
//!   the steady state of a database that has been *running* and absorbing writes for a while.
//!   `crates/tessera-engine/tests/soak.rs` covers the *shape* of it (40 flushes → 2 segments,
//!   5 delta tiers, one full projection build); **`tests/scale.rs` covers it at size** since
//!   2026-08-05, with the figures in `docs/evidence/memos/2026-08-05-write-path-at-scale.md`.
//!
//!   **What that measured, and it is not reassuring: the segment axis is not bounded.** The
//!   entity-space coalesce does bound its three axes, repeatedly. The row-space merge does not
//!   bound segments, because `MergePolicy::select`'s rule 3 refuses a window whose *total* exceeds
//!   `max_merged_segment_bytes`. At the shipped 256 MiB cap and `tier_width` 4, a 250,000-row
//!   extent is ~9.3 MiB, so tier 1 reaches ~37 MiB and tier 2 ~149 MiB — and four of *those* total
//!   595 MiB, over the cap. **Merging therefore saturates at ~149 MiB and the segment count then
//!   grows linearly, one per ~4M rows ingested** (measured: 2 → 17 segments over 200 flushes at a
//!   250M base, while merges kept firing throughout). Design §16's "how many live segments before
//!   per-tile fan-out is noticeable" is **still open**, and now has a rate attached to it rather
//!   than only a question. The cap is raisable — write-path §7 only requires it strictly below the
//!   base segment's size — but merge peak memory is a measured 4.4–4.9× the inputs' file bytes, so
//!   raising it buys segment count with pool transient.
//! * **`accept_change` — the *other* write path, and not this module's.** It lives in
//!   `arms::changes`: `changes` measures ack, tail and the visibility arithmetic against *overlay*
//!   depth, and `arms::changes::run_deny_ack` measures the deny-ack floor's own drivers — buffered
//!   depth, and the head-of-line wait behind an in-flight ingest. It is the write path where a
//!   latency regression is a security regression (lifecycle §1.3: deny visibility is bounded by
//!   queue-front + fsync, and `/control/changes` is never refused for capacity because refusing a
//!   security operation for load is fail-open).

use std::sync::Mutex;
use std::time::Duration;

use tessera_build::{BuildArgs, BuildObserver, BuildStage};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::UnallocatedRow;
use tessera_plugin::Passthrough;
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::{IdentityKey, TermId};

use crate::arms::{Context, Result};
use crate::corpus::{
    build_grant_to_coverage, gen_viewports, Dictionary, Grant, GrantShape, TermStats,
};
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
                extent: Bounds {
                    x_min: 0.0,
                    x_max: 65536.0,
                    y_min: 0.0,
                    y_max: 65536.0,
                },
                slice_id: "s0".to_string(),
                limit: Some(scale),
                identity_key: IdentityKey::from_hex(TEST_KEY_HEX)?,
                identity_key_hex: TEST_KEY_HEX.to_string(),
                idset: 1,
                shard_id: 0,
                mint_external_ids: true,
                emit_oracle_pairs: true,
                batch_items: None,
                memory_budget: None,
                band_rows: None,
            };

            eprintln!("ingest_build: scale={scale} set={label_set} (one repetition — a build is minutes, not microseconds)");
            let start = std::time::Instant::now();
            let report = tessera_build::build_observed(&args, &collector)?;
            let total_ns = start.elapsed().as_nanos() as u64;

            let stages = collector.stages.lock().unwrap().clone();
            // A stage may fire once per signature batch (the batched build); aggregate by
            // name — sum time and rows, keep the max RSS and the firing count — rather than
            // letting the map's last write silently discard every batch but the final one.
            let mut aggregated: std::collections::BTreeMap<&'static str, (u64, u64, u64, u64)> =
                std::collections::BTreeMap::new();
            for (stage, elapsed, rows, rss) in &stages {
                let entry = aggregated.entry(stage.name()).or_insert((0, 0, 0, 0));
                entry.0 += elapsed.as_nanos() as u64;
                entry.1 += rows;
                entry.2 = entry.2.max(*rss);
                entry.3 += 1;
            }
            let per_stage: serde_json::Map<String, serde_json::Value> = aggregated
                .into_iter()
                .map(|(name, (ns, rows, rss, firings))| {
                    (
                        name.to_string(),
                        serde_json::json!({
                            "ns": ns,
                            "pct": 100.0 * ns as f64 / total_ns.max(1) as f64,
                            "rows": rows,
                            "peak_rss_kib": rss,
                            "firings": firings,
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

/// Synthetic rows to ingest, in the shape the control plane submits them.
///
/// Terms are real dictionary terms so the buffered items are genuinely *visible* to the reading
/// principal — ingesting items the reader cannot see would exercise none of the composition path
/// and would make the F2 measurement meaningless.
///
/// **`descriptors` and `terms` are the same set, carried twice, because that is the only state
/// `/control/ingest` can produce.** A handler resolves the request's descriptors and submits both
/// halves (`UnallocatedRow`'s own doc says why: the WAL stores raw descriptors, since a term coined
/// between builds has no durable id, while signature-sorted assignment needs the resolved ids
/// before any entity id exists). A row with terms and no descriptors is unreachable, and
/// synthesising one understates `wal_append` and the WAL record's size.
///
/// **Entity IDs come from the allocator, not from a counter** — they are simply assigned one layer
/// further in. Signature-sorted assignment lives on the write executor rather than the handler, so
/// `/control/ingest` submits `UnallocatedRow`s and the thread that owns the WAL assigns the ids
/// (`control.rs`). This helper produces the same shape, so what it synthesises is a state the
/// system can actually reach.
pub(crate) fn synth_rows(
    count: usize,
    start: u64,
    terms: &[TermId],
    descriptors: &[Vec<u8>],
) -> Vec<UnallocatedRow> {
    (0..count)
        .map(|i| {
            let n = start + i as u64;
            UnallocatedRow {
                external_id: Some(format!("bench-{n}").into_bytes()),
                slice: "s0".to_string(),
                descriptors: descriptors.to_vec(),
                x: ((n * 37) % 65536) as f32,
                y: ((n * 53) % 65536) as f32,
                scalars: Vec::new(),
                terms: terms.to_vec(),
            }
        })
        .collect()
}

/// A grant's terms paired with the bundle descriptors that resolve to them, at most `limit` of
/// them.
///
/// **Every arm here states the term density it writes at**, because density is the dominant factor
/// in a commit window's cost: it is the operand of the buffer clone, of `assign_sorted`'s signature
/// comparison and of the WAL record at once. Passing a whole coverage-shaped grant — which is what
/// `build_grant_to_coverage` returns, and can be hundreds of terms — would put an unstated and
/// wholly unrealistic density into every cell.
///
/// A term the dictionary cannot name is dropped along with its descriptor, so the two halves stay
/// aligned; the returned length is what was actually achieved.
fn grant_pool(grant: &Grant, dict: &Dictionary, limit: usize) -> (Vec<TermId>, Vec<Vec<u8>>) {
    let mut terms = Vec::new();
    let mut descriptors = Vec::new();
    for term in &grant.terms {
        if terms.len() >= limit {
            break;
        }
        if let Some(d) = dict.descriptor(*term) {
            terms.push(*term);
            descriptors.push(d.as_bytes().to_vec());
        }
    }
    (terms, descriptors)
}

/// Term density the arms other than `rate` write at.
///
/// Three descriptors per row is the density `docs/evidence/memos/2026-08-05-write-path-at-scale.md`
/// measures a deployment-shaped corpus at, and the one `rate` centres its own sweep on. These arms
/// hold it fixed because their subject is something else — ack against batch size, group commit's
/// gathering, read latency against buffer depth — and a density that varied between them would make
/// their cells incomparable with each other and with `rate`.
const DEFAULT_TERM_DENSITY: usize = 3;

/// [`grant_pool`] at [`DEFAULT_TERM_DENSITY`] — the one entry point for arms whose ingest is a
/// background condition rather than their subject, so the density they write at is the same number
/// in one place rather than three.
pub(crate) fn ingest_density(grant: &Grant, dict: &Dictionary) -> (Vec<TermId>, Vec<Vec<u8>>) {
    grant_pool(grant, dict, DEFAULT_TERM_DENSITY)
}

/// Batch ingest: ack latency as a function of batch size, against a buffer that only deepens.
///
/// # This arm measures a ramp, and reports it as one
///
/// Nothing here flushes, so repetition *n* of a cell lands on a buffer holding `n · batch` rows
/// and pays `apply_window`'s clone over all of them. The samples are therefore a rising series,
/// not repeats of one quantity, and the two summary statistics a rising series admits are its ends:
///
/// * `ns_per_item_cold` — repetition 0, at buffer depth 0. **Labelled cold**, and it is the
///   quantity the arm's superseded "~1.37M items/s at batch=10,000" headline was: `min` over a
///   ramp is the ramp's start, which is why F3's signal sat in this arm's own output for a month
///   without being read.
/// * `ns_per_item` — the median of the **deepest half** of the ramp, which is what the emitted
///   `samples` are, so `timing` and `normalised` describe a warm buffer rather than an empty one.
///
/// `samples_in_order_ns` and `buffered_before_sample` carry the whole series, so the slope — F3's
/// `B/W` law, at `W = batch` and `B` growing without bound — is recoverable without a re-run.
///
/// **There is no steady state to report**, because a buffer nothing flushes has none: per-row cost
/// grows without bound in the number of repetitions. `run_rate` is the arm that flushes, and it is
/// the one to quote a throughput from.
pub fn run_batch(ctx: &Context, batch_sizes: &[usize], seed: u64) -> Result<()> {
    let mut run = ctx.open("ingest_batch")?;

    for fixture in &ctx.fixtures {
        let postings = tessera_authz::PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let dictionary = Dictionary::open(&fixture.root, &fixture.prefix)?;
        let (grant, _) = build_grant_to_coverage(
            &stats,
            &postings,
            GrantShape::Random,
            0.05,
            fixture.scale,
            seed,
        )?;
        let (terms, descriptors) = grant_pool(&grant, &dictionary, DEFAULT_TERM_DENSITY);
        if terms.is_empty() {
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
            let mut engine = Engine::open(
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
                    compute_threads: tessera_engine::default_compute_threads(),
                    flush_max_age_secs: 90,
                    max_merged_segment_bytes: None,
                    // Compaction §9's trigger is off unless a deployment configures one.
                    compaction: tessera_engine::CompactionSchedule::off(),
                },
            )?;
            // The WAL lives on a dedicated executor thread, so an engine that writes must start
            // one. Bound is generous — this harness never means to measure queue-full
            // backpressure, only ack latency.
            engine.start_write_executor(1024)?;

            let mut next_id = 0u64;
            let mut samples = Vec::new();
            let mut depths = Vec::new();
            for rep in 0..ctx.repeat {
                let rows = synth_rows(batch, next_id, &terms, &descriptors);
                next_id += batch as u64;
                let batch_id = format!("bench-{batch}-{rep}");
                depths.push(rep as u64 * batch as u64);
                let start = std::time::Instant::now();
                engine.accept_ingest(rows, batch_id, [rep as u8; 32])?;
                samples.push(start.elapsed().as_nanos() as u64);
            }

            // The deepest half of the ramp: what `timing` and `normalised` are derived from, so
            // the gated quantities describe a warm buffer. `samples[0]` — depth 0 — is reported
            // separately and only as `cold`.
            let cold = samples[0];
            let warm: Vec<u64> = samples[samples.len() / 2..].to_vec();
            let mut sorted = warm.clone();
            sorted.sort_unstable();
            let warm_median = sorted[sorted.len() / 2];

            let work = Work {
                points_gathered: batch as u64,
                ..Default::default()
            };

            run.emit(
                cell_id,
                fixture,
                serde_json::json!({
                    "batch_size": batch,
                    "terms_per_row": terms.len(),
                    "ns_per_item": warm_median as f64 / batch as f64,
                    "items_per_sec": 1e9 * batch as f64 / warm_median as f64,
                    // Depth 0. NOT a throughput, and not comparable with `ns_per_item` above.
                    "ns_per_item_cold": cold as f64 / batch as f64,
                    // Each repetition leaves its rows in the buffer, so this rising series is
                    // F3's O(buffer) clone showing up directly; `buffered_before_sample` is its
                    // x-axis, which makes the slope recoverable without a re-run.
                    "samples_in_order_ns": samples,
                    "buffered_before_sample": depths,
                }),
                work,
                warm,
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
        let (terms, descriptors) = grant_pool(&grant, &dictionary, DEFAULT_TERM_DENSITY);
        if terms.is_empty() {
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
        let mut engine = Engine::open(
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
                compute_threads: tessera_engine::default_compute_threads(),
                flush_max_age_secs: 90,
                max_merged_segment_bytes: None,
                // Compaction §9's trigger is off unless a deployment configures one.
                compaction: tessera_engine::CompactionSchedule::off(),
            },
        )?;
        // The WAL lives on a dedicated executor thread, so an engine that writes must start one.
        // Bound is generous — this harness never means to measure queue-full backpressure, only
        // ack latency.
        engine.start_write_executor(1024)?;
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
                let rows = synth_rows(1, next_id, &terms, &descriptors);
                next_id += 1;
                let start = std::time::Instant::now();
                engine.accept_ingest(rows, format!("c-{next_id}"), [0u8; 32])?;
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
                let rows = synth_rows(1, next_id, &terms, &descriptors);
                next_id += 1;
                buffered += 1;
                let start = std::time::Instant::now();
                engine.accept_ingest(rows, format!("c-probe-{next_id}"), [0u8; 32])?;
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
                    "terms_per_row": terms.len(),
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

// ---------------------------------------------------------------------------------------------
// Mode: concurrent — what group commit actually collects when submissions overlap
// ---------------------------------------------------------------------------------------------

/// Rows whose **signatures vary**, which `synth_rows` deliberately does not do.
///
/// `synth_rows` gives every row the *same* term list, because the arms it was written for want
/// buffered items that are uniformly visible to the reading principal. That makes every row's
/// signature identical, so every term has `k = W` in every window — where the fragmentation
/// baseline `k·(W − k + 1)/W` is `1` and one contiguous run is `1`, and `run_ratio` is pinned at
/// exactly `1.0` for every window size. Measured: the first run of this arm reported `1.0` at one,
/// four and sixteen submitters. That is a property of the corpus, not of the server.
///
/// So this arm builds its own rows: a two-term signature per row, cycled over the grant's terms, so
/// a term's postings split across every signature carrying it as they do on a real corpus.
///
/// **The distribution is synthetic and uniform, and no `run_ratio` from this arm forecasts a
/// deployment.** The measured corpus is heavily skewed (probes results §3: 54,791 signatures over
/// 2.42 M items, mean group 44, top 500 groups covering 82.4%), and a uniform cycle has none of
/// that shape. What this arm's figure *is* good for is the mechanism: that the counters move with
/// the window and with concurrency, on a real engine over a real bundle.
fn varied_signature_rows(
    count: usize,
    start: u64,
    terms: &[TermId],
    descriptors: &[Vec<u8>],
) -> Vec<UnallocatedRow> {
    let mut rows = synth_rows(count, start, terms, descriptors);
    if terms.len() < 2 {
        return rows;
    }
    for (i, row) in rows.iter_mut().enumerate() {
        let a = (i * 7) % terms.len();
        let b = (a + 1) % terms.len();
        row.terms = vec![terms[a], terms[b]];
        row.descriptors = vec![descriptors[a].clone(), descriptors[b].clone()];
    }
    rows
}

/// Concurrent ingest: N submitters at once, so commit windows hold more than one entry.
///
/// # Why this arm exists, and why `batch` above cannot answer the same question
///
/// `run_batch` submits **sequentially** and blocks on each receipt, so the executor never has a
/// second entry queued when it closes a window: every window it produces holds exactly one entry,
/// `wal_appends / wal_fsyncs` is pinned at 1.0, and any group-commit figure taken from it is a null
/// result dressed as a measurement. That is a property of the harness, not of the server — a real
/// `/control/ingest` caller is one of `ingest_admission` concurrent handlers.
///
/// So this arm spawns N threads each calling `Engine::accept_ingest`, which is exactly what N
/// concurrent handlers do one layer up (`control.rs` runs `run_ingest` inside `spawn_blocking`).
///
/// # What is reported, and what is exact
///
/// **Two counter-derived figures, which need no quiet box**: `entries_per_window`
/// (`wal_appends / wal_fsyncs` — group commit's amortisation, exact) and the fragmentation block
/// (`run_ratio`, `postings_per_container`, and the raw counters — exact, contracts §3.4). Both are
/// integer counters read off `ExecutorStats`; neither is a timing.
///
/// **One timing, reported as a throughput and not as a latency budget**: wall time for the whole
/// concurrent submission. It is load-dependent by construction and must not be read as an ack
/// latency; `run_batch` is where ack latency is measured.
///
/// `run_ratio` here is **within-window sort quality against a within-window random baseline** — see
/// `tessera_lifecycle::window::FragmentationTally`. It is not comparable with the probes' §2
/// full-corpus posting compression, and it is not the row-space run ratio `crate::metrics` computes.
pub fn run_concurrent(
    ctx: &Context,
    submitters: &[usize],
    batch: usize,
    window: usize,
    seed: u64,
) -> Result<()> {
    let mut run = ctx.open("ingest_concurrent")?;

    for fixture in &ctx.fixtures {
        let postings = tessera_authz::PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let dictionary = Dictionary::open(&fixture.root, &fixture.prefix)?;
        let (grant, _) = build_grant_to_coverage(
            &stats,
            &postings,
            GrantShape::Random,
            0.05,
            fixture.scale,
            seed,
        )?;
        // The whole grant is the *pool* signatures are drawn from here, not the density: each row
        // takes two of it. `usize::MAX` is "no cap", and the cap that matters is applied per row.
        let (pool_terms, pool_descriptors) = grant_pool(&grant, &dictionary, usize::MAX);
        if pool_terms.len() < 2 {
            continue;
        }

        for &threads in submitters {
            let cell_id = format!(
                "ingest_concurrent/{}/{}/t{}b{}w{}",
                fixture.scale, fixture.label_set, threads, batch, window
            );
            if run.ledger.is_done(&cell_id) {
                run.skipped += 1;
                continue;
            }

            let tmp = std::env::temp_dir().join(format!(
                "tessera-bench-concurrent-{}-{}",
                std::process::id(),
                threads
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            let mut engine = Engine::open(
                &fixture.root,
                &tmp.join("cache"),
                &tmp.join("wal.log"),
                Passthrough::new(),
                EngineConfig {
                    token_max_lifetime_secs: 3600,
                    max_k: 200,
                    k_min: 2,
                    k_max_marks: 500,
                    theta_target_marks: 16,
                    max_underlay_offset: 4,
                    max_underlay_cells: 8192,
                    max_tiles_per_request: 262_144,
                    compute_threads: tessera_engine::default_compute_threads(),
                    flush_max_age_secs: 90,
                    max_merged_segment_bytes: None,
                    // Compaction §9's trigger is off unless a deployment configures one.
                    compaction: tessera_engine::CompactionSchedule::off(),
                },
            )?;
            // Generous, deliberately: this arm means to measure what a full window collects, never
            // queue-full backpressure.
            engine.start_write_executor(4096)?;
            engine.set_commit_window_max_rows(window);
            let engine = std::sync::Arc::new(engine);

            let start = std::time::Instant::now();
            std::thread::scope(|s| {
                for t in 0..threads {
                    let engine = std::sync::Arc::clone(&engine);
                    let terms = pool_terms.clone();
                    let descriptors = pool_descriptors.clone();
                    s.spawn(move || {
                        for rep in 0..ctx.repeat {
                            let base = (t as u64 * 1_000_000) + (rep as u64 * batch as u64);
                            let rows = varied_signature_rows(batch, base, &terms, &descriptors);
                            let batch_id = format!("conc-{t}-{rep}");
                            engine
                                .accept_ingest(rows, batch_id, [t as u8; 32])
                                .expect("the batch is accepted");
                        }
                    });
                }
            });
            let elapsed = start.elapsed();

            let stats = engine.write_executor_stats();
            let rows_total = (threads * batch * ctx.repeat as usize) as u64;
            let entries_per_window = if stats.wal_fsyncs > 0 {
                stats.wal_appends as f64 / stats.wal_fsyncs as f64
            } else {
                0.0
            };

            let work = Work {
                points_gathered: rows_total,
                ..Default::default()
            };
            run.emit(
                cell_id,
                fixture,
                serde_json::json!({
                    "submitters": threads,
                    "batch_size": batch,
                    "terms_per_row": 2,
                    "signature_pool_terms": pool_terms.len(),
                    "commit_window_max_items": window,
                    "rows_total": rows_total,
                    // Exact counters. `entries_per_window` at ~1.0 means group commit ran and
                    // collected nothing — which is what the sequential `batch` arm always reports.
                    "wal_appends": stats.wal_appends,
                    "wal_fsyncs": stats.wal_fsyncs,
                    "entries_per_window": entries_per_window,
                    "fragmentation_windows": stats.fragmentation_windows,
                    "fragmentation_postings": stats.fragmentation.postings,
                    "fragmentation_runs": stats.fragmentation.runs,
                    "fragmentation_containers": stats.fragmentation.containers,
                    "run_ratio": stats.run_ratio(),
                    "postings_per_container": stats.postings_per_container(),
                    // Load-dependent by construction. NOT an ack latency and NOT a budget.
                    "wall_ns": elapsed.as_nanos() as u64,
                    "rows_per_sec": rows_total as f64 / elapsed.as_secs_f64(),
                }),
                work,
                vec![elapsed.as_nanos() as u64],
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

// ---------------------------------------------------------------------------------------------
// Mode: rate — sustained rows per second, keyed on the shape of the write
// ---------------------------------------------------------------------------------------------

/// The three axes, plus the constants held fixed across them.
pub struct RateSweep<'a> {
    /// Descriptors per row.
    pub density: &'a [usize],
    /// `B/W` — rows buffered between flushes, in units of the batch size.
    pub ratio: &'a [usize],
    /// Concurrent `/control/ingest` callers.
    pub submitters: &'a [usize],
    /// Rows per `accept_ingest` call. `W` for a serial caller; a lower bound on it otherwise.
    pub batch: usize,
    /// `ingest.commit_window_max_items`, the ceiling a gathering window closes at.
    pub window: usize,
    /// The floor on rows measured after the cold cycle is discarded.
    pub min_steady_rows: usize,
    pub seed: u64,
}

/// Copy a bundle so a cell can flush into it without changing what every later cell measures.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<u64> {
    std::fs::create_dir_all(to)?;
    let mut bytes = 0;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            bytes += copy_tree(&src, &dst)?;
        } else {
            bytes += std::fs::copy(&src, &dst)?;
        }
    }
    Ok(bytes)
}

/// A `WriteStage` split as microseconds per ingested row, from a before/after stats pair.
///
/// **Differenced, never absolute.** `ExecutorStats::stage_nanos` counts from executor start, so a
/// cell that reported the raw figure would be reporting its own cold cycle plus every flush before
/// it. `submit→receipt` is the only entry that does not partition beside the others — it is the
/// caller's whole wait and the executor's stages happen inside it — and under `c` concurrent
/// callers it sums `c` overlapping waits, so it exceeds the reciprocal of the throughput by
/// design.
fn stage_split(
    before: &tessera_engine::ExecutorStats,
    after: &tessera_engine::ExecutorStats,
    rows: u64,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for stage in tessera_engine::WriteStage::ALL {
        let ns =
            after.stage_nanos[stage as usize].saturating_sub(before.stage_nanos[stage as usize]);
        map.insert(
            stage.name().trim().to_string(),
            serde_json::json!(ns as f64 / 1e3 / rows.max(1) as f64),
        );
    }
    serde_json::Value::Object(map)
}

/// Distinct signatures the descriptor pool can express, and the pool's size.
///
/// 1,024 is the widest column `crates/tessera-engine/tests/ingest_shape.rs`' signature-diversity
/// sweep reaches, so the two harnesses describe the same regime. Above it that sweep found no
/// further movement; below ~8 the window sort degenerates into an all-ties tie-break and measures
/// the fixture rather than the system.
const POOL_TERMS: usize = 1_024;

/// `limit` descriptors spread evenly across the bundle's dictionary, with their term ids.
///
/// Spread rather than taken from the head: a dictionary is written in term order and the head is
/// systematically the shortest and widest descriptors, so a prefix would make descriptor *length*
/// — which is what the WAL record and the external-id compare pay for — unrepresentative.
fn dictionary_pool(dict: &Dictionary, limit: usize) -> (Vec<TermId>, Vec<Vec<u8>>) {
    let n = dict.len();
    let take = limit.min(n);
    let stride = (n / take.max(1)).max(1);
    let mut terms = Vec::with_capacity(take);
    let mut descriptors = Vec::with_capacity(take);
    for i in 0..take {
        let term = TermId::new((i * stride) as u32);
        if let Some(d) = dict.descriptor(term) {
            terms.push(term);
            descriptors.push(d.as_bytes().to_vec());
        }
    }
    (terms, descriptors)
}

/// Rows at a chosen term density, drawn from a fixed descriptor pool.
///
/// **Signature count is held constant as density varies**, which is the whole reason the pool is
/// strided rather than blocked. Row `i` takes descriptors `(i·7 + j·137) mod P` for `j < density`:
/// 7 and 137 are both coprime with the pool size, so consecutive rows land on different signatures
/// and the number of distinct signatures is `P` at every density. Taking a contiguous run instead
/// would make the signature count `P/gcd(density, P)` — so `density` would move two things at once
/// and neither column of the table would mean anything. `assign_sorted` sorts a window by
/// `(signature, external_id)`, and `crates/tessera-engine/tests/ingest_shape.rs` measures that the
/// tie-break's cost is real, so this is not a hypothetical confound.
fn rate_rows(
    pool_terms: &[TermId],
    pool_descriptors: &[Vec<u8>],
    count: usize,
    start: u64,
    density: usize,
) -> Vec<UnallocatedRow> {
    let p = pool_terms.len();
    (0..count)
        .map(|i| {
            let n = start + i as u64;
            let picks: Vec<usize> = (0..density)
                .map(|j| ((n as usize).wrapping_mul(7).wrapping_add(j * 137)) % p)
                .collect();
            UnallocatedRow {
                external_id: Some(format!("rate-{n}").into_bytes()),
                slice: "s0".to_string(),
                descriptors: picks.iter().map(|&k| pool_descriptors[k].clone()).collect(),
                x: ((n * 37) % 65536) as f32,
                y: ((n * 53) % 65536) as f32,
                scalars: Vec::new(),
                terms: picks.iter().map(|&k| pool_terms[k]).collect(),
            }
        })
        .collect()
}

/// Submit every batch in `cycle` across `threads` callers, and return the wall time of the
/// submission alone.
///
/// Callers pull from one queue rather than owning a contiguous slice, so a slow batch does not
/// leave its thread's remaining work unstarted while others idle — which would report a
/// concurrency the run never had.
fn drive_cycle(
    engine: &Engine,
    cycle: Vec<(String, Vec<UnallocatedRow>)>,
    threads: usize,
    acks: &Mutex<Vec<(usize, u64)>>,
) -> u64 {
    let queue = Mutex::new(cycle.into_iter().enumerate());
    let start = std::time::Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let Some((seq, (batch_id, rows))) = queue.lock().unwrap().next() else {
                    break;
                };
                let hash = [(seq % 251) as u8; 32];
                let t = std::time::Instant::now();
                engine
                    .accept_ingest(rows, batch_id, hash)
                    .expect("the batch is accepted");
                acks.lock()
                    .unwrap()
                    .push((seq, t.elapsed().as_nanos() as u64));
            });
        }
    });
    start.elapsed().as_nanos() as u64
}

/// Sustained ingest rate, swept over term density, `B/W` and concurrency.
///
/// # What a cell does
///
/// A private copy of the base bundle, a fresh engine, then `1 + n` **flush cycles**: `B` rows
/// submitted in batches of `batch` across `c` callers, then `request_flush` and a wait for the
/// publication. Cycle 0 is discarded as cold — it pays the page-cache warm-up, the allocator's
/// first growth and the row-projection build — and the remaining `n` are the measurement, sized so
/// at least `min_steady_rows` rows are timed.
///
/// **Waiting for the flush is what pins `B`.** A cadence that only *requested* a flush would let
/// the loader run on through the publication, and the rows it added would land in the next cycle's
/// buffer — so achieved `B` would drift upward with the rate, and the axis the table is keyed on
/// would be a function of the answer. The cost of pinning it is that the wall clock has to be
/// split, and it is: `rows_per_sec` is submission alone, `rows_per_sec_with_flush` includes the
/// publication a deployment also pays. Neither is the other.
///
/// # What is reported, and what it is not
///
/// * `rows_per_sec` / `us_per_row` — steady state, the headline. Derived from wall clock over
///   complete cycles, so under `c > 1` callers it is aggregate throughput, not per-caller.
/// * the 13-stage `WriteStage` split, differenced across the steady region.
/// * `w_observed` — rows per commit-window close, `steady_rows / closes`. **`W` is measured, not
///   assumed**: a serial caller pins it at `batch` whatever `commit_window_max_items` says, and a
///   concurrent one lets the window gather. `bw_observed` is `B / w_observed`, and it is the value
///   the `B²/2W` law is actually keyed on.
/// * `cold_us_per_row` — cycle 0, labelled and never mixed into the headline.
///
/// A cell whose `B/W` gives fewer batches per cycle than it has callers cannot keep them all busy;
/// it reports `submitters_effective` and carries the `concurrency_capped` flag.
pub fn run_rate(ctx: &Context, sweep: &RateSweep) -> Result<()> {
    let mut run = ctx.open("ingest_rate")?;

    for fixture in &ctx.fixtures {
        // **The pool is the bundle's dictionary, not a grant.** Every other ingest arm draws terms
        // from a coverage-shaped principal, because its buffered rows have to be *visible* to a
        // reader; this arm never reads, so grant membership would only narrow the pool. It narrows
        // it severely: a 5% grant over `categories-subclass` is four terms, which cannot express a
        // density of eight and pins the signature count at one.
        let dictionary = Dictionary::open(&fixture.root, &fixture.prefix)?;
        let (pool_terms, pool_descriptors) = dictionary_pool(&dictionary, POOL_TERMS);
        let max_density = sweep.density.iter().copied().max().unwrap_or(1);
        if pool_terms.len() < max_density.max(2) {
            eprintln!(
                "ingest_rate: {}/{} has a {}-term dictionary, below the widest density asked for \
                 ({max_density}); skipping",
                fixture.scale,
                fixture.label_set,
                pool_terms.len()
            );
            continue;
        }

        for &density in sweep.density {
            for &ratio in sweep.ratio {
                for &threads in sweep.submitters {
                    let cell_id = format!(
                        "ingest_rate/{}/{}/d{}/r{}/c{}",
                        fixture.scale, fixture.label_set, density, ratio, threads
                    );
                    if run.ledger.is_done(&cell_id) {
                        run.skipped += 1;
                        continue;
                    }

                    let b = ratio * sweep.batch;
                    let steady_cycles = sweep.min_steady_rows.div_ceil(b).max(1);
                    let steady_rows = (b * steady_cycles) as u64;

                    let tmp = std::env::temp_dir().join(format!(
                        "tessera-bench-rate-{}-{}-{}-{}",
                        std::process::id(),
                        density,
                        ratio,
                        threads
                    ));
                    let _ = std::fs::remove_dir_all(&tmp);
                    std::fs::create_dir_all(&tmp)?;
                    let base = tmp.join("bundle");
                    copy_tree(&fixture.root, &base)?;

                    let mut engine = Engine::open(
                        &base,
                        &tmp.join("cache"),
                        &tmp.join("wal.log"),
                        Passthrough::new(),
                        EngineConfig {
                            token_max_lifetime_secs: 3600,
                            max_k: 200,
                            k_min: 2,
                            k_max_marks: 500,
                            theta_target_marks: 16,
                            max_underlay_offset: 4,
                            max_underlay_cells: 8192,
                            max_tiles_per_request: 262_144,
                            compute_threads: tessera_engine::default_compute_threads(),
                            // Only a requested flush may fire. A periodic tick would put a
                            // publication in the middle of a cycle and make `B` a function of the
                            // rate rather than an axis.
                            flush_max_age_secs: 86_400,
                            max_merged_segment_bytes: None,
                            // Compaction §9's trigger is off unless a deployment configures one.
                            compaction: tessera_engine::CompactionSchedule::off(),
                        },
                    )?;
                    // Generous: this arm never means to measure queue-full backpressure.
                    engine.start_write_executor(4096)?;
                    engine.set_commit_window_max_rows(sweep.window);

                    // **Rows are built one cycle ahead of the cycle that submits them**, which
                    // keeps the harness's own allocation outside every timed region without
                    // holding a whole cell's rows live: at `B` = 240,000 and eight descriptors
                    // that would be a gigabyte of `UnallocatedRow`, and an allocator carrying it
                    // is not the one a streaming loader runs against.
                    let batches_per_cycle = b / sweep.batch;
                    let effective = threads.min(batches_per_cycle);
                    let mut next_id = 0u64;
                    let build_cycle = |next_id: &mut u64| {
                        (0..batches_per_cycle)
                            .map(|_| {
                                let id = *next_id;
                                *next_id += sweep.batch as u64;
                                (
                                    format!("rate-{id}"),
                                    rate_rows(
                                        &pool_terms,
                                        &pool_descriptors,
                                        sweep.batch,
                                        id,
                                        density,
                                    ),
                                )
                            })
                            .collect::<Vec<_>>()
                    };

                    // Cycle 0: cold, and reported as such. It pays the page-cache warm-up, the
                    // allocator's first growth and the WAL's first extent.
                    let acks = Mutex::new(Vec::new());
                    let cold_ns = drive_cycle(&engine, build_cycle(&mut next_id), effective, &acks);
                    let cold_flush_ns = flush_and_wait(&engine)?;
                    acks.lock().unwrap().clear();

                    // **Repetitions of the whole steady region, not of one cycle**, because a cell's
                    // subject is a rate over *complete* flush cycles and a partial one is not a
                    // smaller sample of it. One region here is under a second and is scheduler
                    // noise on its own — `min` over five is stable to ~±20% run-to-run, one shot to
                    // ~±40% — and `min` over comparable regions is the probes' own estimator rather
                    // than a min over a ramp.
                    let before = engine.write_executor_stats();
                    let mut submit_samples = Vec::with_capacity(ctx.repeat as usize);
                    let mut flush_ns = 0u64;
                    for _ in 0..ctx.repeat.max(1) {
                        let mut region_ns = 0u64;
                        for _ in 0..steady_cycles {
                            region_ns +=
                                drive_cycle(&engine, build_cycle(&mut next_id), effective, &acks);
                            flush_ns += flush_and_wait(&engine)?;
                        }
                        submit_samples.push(region_ns);
                    }
                    let after = engine.write_executor_stats();
                    let submit_ns = *submit_samples.iter().min().expect("one repetition");
                    let mut ordered = submit_samples.clone();
                    ordered.sort_unstable();
                    let submit_median_ns = ordered[ordered.len() / 2];
                    let measured_rows = steady_rows * ctx.repeat.max(1) as u64;

                    // **A one-row probe, purely to read the buffer.** `ExecutorStats::buffered_items`
                    // is stamped at the last *apply*, so straight after a flush it still reports the
                    // depth the flush consumed. One more apply refreshes it, and what it then reports
                    // is the probe's own row plus whatever the flush left behind — so `1` is a drained
                    // buffer and anything larger means `B` is not what this cell claims.
                    drive_cycle(
                        &engine,
                        vec![(
                            "rate-drain-probe".to_string(),
                            rate_rows(&pool_terms, &pool_descriptors, 1, next_id, density),
                        )],
                        1,
                        &Mutex::new(Vec::new()),
                    );
                    let drained = engine.write_executor_stats().buffered_items;

                    let closes = after.wal_fsyncs.saturating_sub(before.wal_fsyncs);
                    let appends = after.wal_appends.saturating_sub(before.wal_appends);
                    let w_observed = if closes > 0 {
                        measured_rows as f64 / closes as f64
                    } else {
                        0.0
                    };
                    let mut acks = acks.into_inner().unwrap();
                    acks.sort_unstable_by_key(|(seq, _)| *seq);
                    let ack_series: Vec<u64> = acks.iter().map(|(_, ns)| *ns).collect();

                    let mut flags = Vec::new();
                    if effective < threads {
                        flags.push("concurrency_capped".to_string());
                    }
                    // A cycle must leave the buffer empty, or `B` is not what the cell says it is
                    // and the `B/W` column is fiction.
                    if drained > 1 {
                        flags.push("buffer_not_drained".to_string());
                    }
                    // A window closes on its row bound *or* an empty work queue, so a ceiling at or
                    // below the batch size means one entry fills it and group commit can collect
                    // nothing however many callers there are. `w_observed` will read `batch` at
                    // every concurrency, and that is the configuration, not the server.
                    if sweep.window <= sweep.batch {
                        flags.push("group_commit_capped".to_string());
                    }

                    let work = Work {
                        points_gathered: steady_rows,
                        ..Default::default()
                    };

                    run.emit(
                        cell_id,
                        fixture,
                        serde_json::json!({
                            "density": density,
                            "bw_requested": ratio,
                            "submitters": threads,
                            "submitters_effective": effective,
                            "batch_size": sweep.batch,
                            "commit_window_max_items": sweep.window,
                            "buffered_between_flushes": b,
                            "steady_cycles": steady_cycles,
                            "steady_rows": steady_rows,
                            // The headline. Submission alone, so `B` stays pinned; the flush a
                            // deployment also pays is the second figure.
                            "rows_per_sec": 1e9 * steady_rows as f64 / submit_ns.max(1) as f64,
                            "us_per_row": submit_ns as f64 / 1e3 / steady_rows as f64,
                            "rows_per_sec_with_flush": 1e9 * measured_rows as f64
                                / (submit_samples.iter().sum::<u64>() + flush_ns).max(1) as f64,
                            "flush_us_per_row": flush_ns as f64 / 1e3 / measured_rows as f64,
                            "us_per_row_median": submit_median_ns as f64 / 1e3 / steady_rows as f64,
                            "repetitions": ctx.repeat.max(1),
                            "measured_rows": measured_rows,
                            // Cycle 0, at an empty buffer and a cold page cache. NOT a throughput.
                            "cold_us_per_row": cold_ns as f64 / 1e3 / b as f64,
                            "cold_flush_ns": cold_flush_ns,
                            // Measured, not configured — see this function's doc.
                            "w_observed": w_observed,
                            "bw_observed": if w_observed > 0.0 { b as f64 / w_observed } else { 0.0 },
                            "wal_appends": appends,
                            "wal_closes": closes,
                            "entries_per_window": if closes > 0 {
                                appends as f64 / closes as f64
                            } else {
                                0.0
                            },
                            "signature_pool_terms": pool_terms.len(),
                            "flushes": after.flushes.saturating_sub(before.flushes),
                            "merges": after.merges.saturating_sub(before.merges),
                            "coalesces": after.coalesces.saturating_sub(before.coalesces),
                            "buffered_after_flush": drained.saturating_sub(1),
                            "stage_us_per_row": stage_split(&before, &after, measured_rows),
                            "ack_ns_in_order": ack_series,
                            "seed": sweep.seed,
                        }),
                        work,
                        // One sample per repetition, each the wall clock of a whole steady region
                        // of `steady_rows`. `normalised.ns_per_point_gathered` — the gated
                        // quantity — is then `min_ns / steady_rows`, the steady-state per-row cost.
                        submit_samples.clone(),
                        None,
                        flags,
                    )?;

                    drop(engine);
                    let _ = std::fs::remove_dir_all(&tmp);
                }
            }
        }
    }

    run.finish();
    Ok(())
}

/// Request a flush and wait for its publication, returning the wall time.
///
/// Polls `ExecutorStats::flushes`, which advances on publication rather than on the plan, so the
/// wait covers the segment write and the manifest — the whole of what makes an ingested row
/// visible to a session authorised after it (write-path §4).
fn flush_and_wait(engine: &Engine) -> Result<u64> {
    let before = engine.write_executor_stats().flushes;
    let start = std::time::Instant::now();
    engine.request_flush();
    let deadline = start + Duration::from_secs(600);
    while engine.write_executor_stats().flushes <= before {
        if std::time::Instant::now() >= deadline {
            return Err("flush did not publish within 600 s".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(start.elapsed().as_nanos() as u64)
}
