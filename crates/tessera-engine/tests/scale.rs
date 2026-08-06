//! **The scale test**: a meaningful base build, several rounds of ingest over millions of rows,
//! and the proof that every one of them becomes correctly queryable — through flush, through both
//! maintenance passes, and across a restart.
//!
//! **Figures live in `docs/evidence/memos/2026-08-05-write-path-at-scale.md`**, with the raw
//! per-round output in `probes/2026-08-05-write-path-at-scale/`. What is here is why the test has
//! the shape it has; what a run *measured* is evidence, and evidence does not belong in a module
//! doc that would then have to be corrected every time the machine changes.
//!
//! ## Why this exists, when `soak.rs` already runs sustained ingest
//!
//! `soak.rs` asserts the *shape* of the steady state: forty flushes of one row each over a
//! sixty-four item corpus, and the four axes bounded at the end of it. That is the right test for
//! "does maintenance run", and the wrong one for "does the data survive it". Its corpus fits in a
//! single Roaring container, its flushes produce single-row extents whose merge is the **identity
//! permutation** (the defect `tests/merge.rs` was rebuilt to remove), and every count it checks is
//! small enough to be right by accident.
//!
//! `crates/tessera-bench/src/arms/ingest.rs` names the gap in its own words: what the ingest arms
//! cannot show is "the steady state of a database that has been *running* and absorbing writes for
//! a while". This is that test — the only one in the tree where the merge consumes segments of
//! hundreds of thousands of rows, where entity space spans many Roaring containers, and where a
//! sparse principal's masked count is a number no reviewer could verify by inspection.
//!
//! ## Ignored by default, and release by default
//!
//! `#[ignore]`, because the gate is a 94 s budget (`.github/workflows/ci.yml`) and this spends more
//! than that on ingest alone. **Run it in release** — a debug build spends its time in Roaring and
//! Arrow rather than in anything this test is about:
//!
//! ```text
//! cargo test -p tessera-engine --release --test scale -- --ignored --nocapture
//! ```
//!
//! | variable | default | what it is |
//! |---|---|---|
//! | `TESSERA_SCALE_BASE` | 1,000,000 | items in the base build — the bundle ingest writes *into* |
//! | `TESSERA_SCALE_ROUNDS` | 16 | ingest rounds, each ending in a flush |
//! | `TESSERA_SCALE_BATCH` | 250,000 | items per round |
//!
//! **Sixteen rounds is a threshold, not a round number.** The row-space merge selects every four
//! extents and the entity-space coalesce every eight delta tiers, so a shorter run exercises the
//! merge alone and leaves the entity-space axes growing untouched — which is the configuration an
//! earlier draft of this test shipped with, and it reported `coalesces=0` while asserting nothing
//! about it. Sixteen is the smallest count at which **both** passes run more than once.
//!
//! ## What is asserted, and what each assertion would catch
//!
//! 1. **Every ingested row is queryable, after every round.** The masked total equals base plus
//!    everything ingested so far. A flush that dropped a batch, or a merge that lost rows, moves
//!    this number and nothing else in the tree would notice at this size.
//!
//!    **Queryable *when* is part of the property, and it is not "immediately".** A flush publishes
//!    without touching a live session's request thread (decision 0044's D1), so an established
//!    session keeps serving from its existing row projection until the background refresh replaces
//!    it — a count taken between the publication and the refresh is short by exactly the round's
//!    batch. Measured, not supposed: the first draft asserted straight after the flush and read
//!    1,000,000 where 1,250,000 had been ingested, while a session authorised *after* the flush
//!    read 1,250,000 immediately. So [`ingest_round`] waits for the refresh, and what this test
//!    pins is the honest contract rather than an instantaneous one.
//! 2. **Identity is preserved in both directions**, sampled across every round, including rows
//!    whose segments have since been merged and whose external-id runs have since been coalesced.
//! 3. **Position is byte-exact.** Each round plants probe rows whose `(tessera_id, code)` pairs are
//!    captured at the round's own flush and re-checked at the end and after the restart. A merge
//!    that dequantised and requantised, rather than carrying the Morton code through, fails here
//!    and passes every count.
//! 4. **Masking holds at scale (I2).** A sparse principal's masked total is checked against ground
//!    truth computed independently from the fixture's own rule, and its served set is a subset of
//!    full coverage. This is the assertion that a delta tier resolved against the wrong row space,
//!    or double-counted after a coalesce, breaks.
//! 5. **Both maintenance passes ran**, with no failures, and the segment axis came down — the
//!    `soak.rs` property at a size where the merge is doing real work.
//! 6. **A restart opens all of it**, and 1–4 still hold against what the node reopens.
//! 7. **A deny still denies the right entity** after all of the above, and one *outside* the sparse
//!    principal's set, so the two totals must move differently.
//!
//! ## What it reports rather than asserts
//!
//! Per round: the three visibility intervals ([`RoundTimings`]), bundle bytes on disc against the
//! bytes the manifest still names ([`bundle_bytes`]), and read latency across [`ZOOM_SWEEP`]. None
//! of these is asserted — a latency or size bound in a test that runs on developer machines is a
//! flake generator, and the useful form is a number a reader compares against the memo. They are
//! printed because the alternative is a separate harness that would drift from this one.
mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, Session, ViewportRequest, WriteStage};
use tessera_lifecycle::{ChangeOp, UnallocatedRow};
use tessera_types::{EntityId, TesseraId};

/// Rows per `accept_ingest` call. Each acceptance is one WAL append and one fsync, so this is the
/// batch size a real `/control/ingest` caller would choose; the measured knee is around 1,000
/// (`arms::ingest`) and 10,000 is comfortably past it.
const SUB_BATCH: usize = 10_000;

/// Probe rows planted per round for the position assertion (item 3 in this module's doc).
const PROBES_PER_ROUND: usize = 4;

/// Zoom the probe queries use, and the half-width of the box they use at it.
///
/// **Probes are placed on half-integer coordinates and read back at the deepest zoom the API
/// accepts, and both halves of that are load-bearing at scale.** `common`'s fixture puts every
/// base item at `((e·37) mod 1000, (e·53) mod 1000)` — both axes are functions of `e mod 1000`, so
/// the base occupies **exactly 1000 distinct positions whatever `n` is**. At a 250M base that is
/// 250,000 items sharing each position, and a probe sharing one of them is invisible to this
/// lookup: its tile holds a quarter-million rows, §7.2's cap serves the first `k`, and the probe is
/// not among them. Measured — a 250M run failed here at round 5 with the counts all exact.
///
/// Offsetting by half a unit puts each probe on a position **no other row in the corpus occupies**
/// (every other row is on an integer), and zoom 16 makes a tile ~0.015 units wide, so a box of
/// ±[`PROBE_HALF_WIDTH`] around it contains the probe and nothing else. The lookup is then
/// independent of corpus size, which is what a scale test needs it to be.
const PROBE_ZOOM: u8 = 16;
const PROBE_HALF_WIDTH: f64 = 0.1;

/// Zooms the per-round read-latency sweep samples.
///
/// A tile resolves to one contiguous row range **per live segment** (arch §11.3), so a viewport
/// pays a binary search and a `range_cardinality` per segment per tile — the product this sweep
/// watches. Zoom 0 is one tile (the whole extent) and zoom 8 is 65,536, so the pair brackets the
/// per-tile term against the per-segment one, and the segment count moves under both as the merge
/// runs.
const ZOOM_SWEEP: [u8; 5] = [0, 2, 4, 6, 8];

/// `k` for the latency sweep. A realistic client value rather than the uncapped one the
/// correctness assertions use — the sweep is about what a viewer waits for, and serving 100,000
/// marks per tile is not that.
const SWEEP_K: usize = 500;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(600);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A config whose selection rules are switched off, so that every assertion here is about the
/// write path rather than about θ or the mark cap.
///
/// `theta_target_marks` is raised above the total visible count for the reason `common::config`
/// gives: it saturates θ at every depth, which reduces selection to "serve every visible row up to
/// the cap" and keeps a masking bug distinguishable from a density-arithmetic one. `max_k` is
/// raised for the narrow-bbox probe queries, which must not be capped mid-tile.
fn scale_config(total: u64) -> EngineConfig {
    EngineConfig {
        max_k: 100_000,
        k_max_marks: 100_000,
        theta_target_marks: total.saturating_mul(2),
        flush_max_age_secs: 3600,
        max_merged_segment_bytes: None,
        ..config_uncapped()
    }
}

fn engine_at(tmp: &std::path::Path, root: &std::path::Path, total: u64) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        scale_config(total),
    )
    .expect("engine opens");
    engine
        .start_write_executor(256)
        .expect("the executor starts once");
    engine
}

/// A whole-extent viewport, retried past the bounded `ProjectionBuilding` a merge's refresh window
/// answers with (decision 0044's permitted residual).
fn viewport_k(
    engine: &Engine,
    session: &Session,
    bbox: [f64; 4],
    zoom: u8,
    k: usize,
) -> tessera_engine::viewport::ViewportOut {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        match engine.viewport(session, ViewportRequest::new("s0", zoom, bbox, k)) {
            Ok(out) => return out,
            Err(e) => {
                assert!(Instant::now() < deadline, "timed out retrying a viewport: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// [`viewport_k`] at the uncapped `k` the correctness assertions need.
fn viewport(
    engine: &Engine,
    session: &Session,
    bbox: [f64; 4],
    zoom: u8,
) -> tessera_engine::viewport::ViewportOut {
    viewport_k(engine, session, bbox, zoom, 100_000)
}

/// The exact masked total — the sum of per-tile visible counts, which §7.1 makes exact at any
/// zoom and which is independent of `k`. **Not** the served point count, which the cap bounds.
fn masked_total(engine: &Engine, session: &Session) -> u64 {
    viewport(engine, session, [0.0, 0.0, 1000.0, 1000.0], 2)
        .tiles
        .iter()
        .map(|t| t.visible)
        .sum()
}

/// Total bytes on disc under `root`, orphans included.
///
/// **Not the same number as [`live_bytes`], and the gap is the point.** A merge's consumed
/// segments and a coalesce's consumed tiers stay on disc: every side-manifest below the current
/// `n` still names them, and a step-down serves one of those (contracts §2.3). Reclaiming them is
/// compaction's, and **the compaction fold does not exist** — so this number only ever grows, and
/// the ratio between the two is what a deployment would actually have to provision for today.
fn bundle_bytes(root: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), total);
            } else {
                *total += meta.len();
            }
        }
    }
    let mut total = 0;
    walk(root, &mut total);
    total
}

/// Bytes the live manifests still name — the working set a reader touches.
///
/// **Two maps, and taking only one of them is a mistake that reads as a catastrophe.** The build's
/// artefacts are digested in the bundle-level `MANIFEST.json`; a flush, merge or coalesce writes
/// into the partition's `SEGMENTS-<n>.json`. `plan_coalesce` keeps them apart on purpose — it takes
/// `build_files` separately so it can refuse to consume a build artefact — so the side-manifest's
/// `files` covers *only* what the write path produced.
///
/// Summing the side-manifest alone reported 9.1 MiB live against a 9.4 GiB bundle at round 0 of a
/// 250M run, and an "orphan ratio" of 1065×. The number was a missing addend, not a leak.
fn live_bytes(engine: &Engine) -> u64 {
    let generation = engine.generation();
    let build: u64 = generation.bundle.manifest.files.values().map(|d| d.size).sum();
    let written: u64 = generation.bundle.partitions["default"]
        .manifest
        .files
        .values()
        .map(|d| d.size)
        .sum();
    build + written
}

/// Whole-extent viewport latency at each of [`ZOOM_SWEEP`]'s depths.
fn zoom_sweep(engine: &Engine, session: &Session) -> Vec<(u8, Duration, usize)> {
    ZOOM_SWEEP
        .iter()
        .map(|&zoom| {
            // Warm the path once, so the figure is the steady-state cost rather than whatever the
            // first touch of a freshly published segment's mmap pages costs.
            viewport_k(engine, session, [0.0, 0.0, 1000.0, 1000.0], zoom, SWEEP_K);
            let t = Instant::now();
            let out = viewport_k(engine, session, [0.0, 0.0, 1000.0, 1000.0], zoom, SWEEP_K);
            (zoom, t.elapsed(), out.tiles.len())
        })
        .collect()
}

/// One ingested row's identity and intended position.
#[derive(Clone)]
struct Planted {
    entity: EntityId,
    external_id: String,
    x: f32,
    y: f32,
    /// Whether this row carries `SUBSET_TERM` — see [`carries_subset`]. Kept so the deny case can
    /// choose a row *outside* the sparse principal's set deliberately rather than by index
    /// arithmetic, and assert that suppressing it moves that principal's total by nothing.
    subset: bool,
}

/// Where round `round`'s item `i` sits, and whether it carries `SUBSET_TERM`.
///
/// Rounds are spread across the extent rather than stacked, so that the merge's inputs interleave
/// in Morton order instead of concatenating — the property `tests/merge.rs` had to be rebuilt to
/// obtain, and which a layout of "round r occupies band r" would quietly lose.
fn position_of(round: usize, i: usize) -> (f32, f32) {
    let x = ((i * 7 + round * 3) % 1000) as f32;
    let y = ((i * 13 + round * 101) % 1000) as f32;
    (x, y)
}

fn carries_subset(i: usize) -> bool {
    i.is_multiple_of(4)
}

/// What one round cost, split at the two points that decide *when* a row becomes visible.
///
/// See [`ingest_round`] and property 1 in this module's doc for why the split is the interesting
/// part rather than the total.
#[derive(Debug, Clone, Copy)]
struct RoundTimings {
    /// `accept_ingest` calls: WAL append + fsync + buffer swap, per sub-batch. The row is
    /// **durable and invisible** at the end of this.
    ack: Duration,
    /// The flush: buffered rows become a segment and a generation is published. The row is now
    /// visible to a session authorised *after* this point.
    publish: Duration,
    /// The background refresh: every live session's row projection is brought forward. The row is
    /// now visible to sessions that already existed.
    refresh: Duration,
}

/// Ingest one round and flush it. Returns the round's probe rows and what it cost.
fn ingest_round(engine: &Engine, round: usize, batch: usize) -> (Vec<Planted>, RoundTimings) {
    let mut probes = Vec::new();
    let mut ingested = 0usize;
    let t_ack = Instant::now();
    while ingested < batch {
        let n = SUB_BATCH.min(batch - ingested);
        let mut rows = Vec::with_capacity(n);
        let mut meta = Vec::with_capacity(n);
        for k in 0..n {
            let i = ingested + k;
            let external_id = format!("r{round}-i{i}");
            let (mut x, mut y) = position_of(round, i);
            // See `PROBE_ZOOM`: a probe goes half a unit off the integer grid every other row sits
            // on, so its own lookup can isolate it at any corpus size.
            if ingested == 0 && i < PROBES_PER_ROUND {
                x += 0.5;
                y += 0.5;
            }
            // **`ALL_TERM`, optionally `SUBSET_TERM`, and a novel per-round descriptor.**
            //
            // The novel one makes the flush *promote* — a descriptor with no ordinal becomes a
            // durable one — so the dictionary-extent axis grows rather than being vacuously
            // bounded, and promotion is exercised at a quarter-million rows rather than at one.
            //
            // It cannot be the only descriptor, and that is a visibility rule rather than a
            // preference: `satisfied` is fixed at authorise and never re-resolved (§3.3), so a row
            // carrying only a term promoted afterwards is invisible to an established session **by
            // design**. Every row therefore also carries `ALL_TERM`, and the totals below measure
            // the write path rather than that rule.
            let novel = format!("scale-term-{round}").into_bytes();
            let descriptors = if carries_subset(i) {
                vec![b"0".to_vec(), b"1".to_vec(), novel]
            } else {
                vec![b"0".to_vec(), novel]
            };
            rows.push(UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                slice: "s0".to_string(),
                x,
                y,
                scalars: Vec::new(),
                terms: engine.resolve_terms(&descriptors),
                descriptors,
            });
            meta.push((external_id, x, y, i));
        }
        let entities = engine
            .accept_ingest(rows, format!("r{round}-b{ingested}"), {
                let mut key = [0u8; 32];
                key[0] = round as u8;
                key[1..9].copy_from_slice(&(ingested as u64).to_le_bytes());
                key
            })
            .expect("ingest is accepted");
        assert_eq!(entities.len(), n, "every submitted row is allocated an entity");
        for (entity, (external_id, x, y, i)) in entities.into_iter().zip(meta) {
            // Probes are the first few of each round — enough to pin position, few enough that the
            // narrow-bbox queries stay cheap.
            if ingested == 0 && i < PROBES_PER_ROUND {
                probes.push(Planted {
                    entity,
                    external_id,
                    x,
                    y,
                    subset: carries_subset(i),
                });
            }
        }
        ingested += n;
    }

    // **Publish, then wait for the background refresh** — and the second half is the contract,
    // not impatience. Decision 0044's D1 keeps a flush off a live session's request thread: the
    // session keeps serving from its existing row projection, which a flush *appends* to rather
    // than invalidating, so it is served one generation stale until the background refresh
    // replaces the entry. A count taken between the two is short by exactly the round's batch —
    // measured, and the reason this helper exists rather than an inline `request_flush`.
    //
    // So the visibility contract this test asserts is the honest one: a flushed row is visible to
    // an *established* session once the refresh has landed, and to a session authorised after the
    // flush immediately. `soak.rs` never meets this because it counts only after a long settle
    // loop; a test that asserted straight after the flush would be asserting D1 does not exist.
    let ack = t_ack.elapsed();
    let flushes = engine.write_executor_stats().flushes;
    let refreshes = engine.refreshes();
    let t_publish = Instant::now();
    engine.request_flush();
    wait_until("the round's flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });
    let publish = t_publish.elapsed();
    let t_refresh = Instant::now();
    wait_until("the background refresh to replace the session's projection", || {
        engine.refreshes() > refreshes
    });
    (probes, RoundTimings { ack, publish, refresh: t_refresh.elapsed() })
}

/// The `(tessera_id, code)` of each probe, read from a narrow viewport around it.
///
/// Narrow rather than whole-extent because the assertion is about *this row's* code: a
/// whole-extent request at this size would serve millions of points to check four of them.
fn probe_codes(engine: &Engine, session: &Session, probes: &[Planted]) -> BTreeMap<String, (TesseraId, u64)> {
    let mut out = BTreeMap::new();
    for probe in probes {
        let tessera_id = engine
            .tessera_id_of(probe.entity)
            .expect("a planted entity has a wire identity");
        let (x, y) = (probe.x as f64, probe.y as f64);
        let bbox = [
            x - PROBE_HALF_WIDTH,
            y - PROBE_HALF_WIDTH,
            x + PROBE_HALF_WIDTH,
            y + PROBE_HALF_WIDTH,
        ];
        let served = viewport(engine, session, bbox, PROBE_ZOOM);
        let found = served
            .points
            .iter()
            .find(|p| p.tessera_id == tessera_id)
            .unwrap_or_else(|| {
                panic!(
                    "{} is not served in a viewport around its own coordinates ({}, {})",
                    probe.external_id, probe.x, probe.y
                )
            });
        out.insert(probe.external_id.clone(), (tessera_id, found.code));
    }
    out
}

/// Assert identity resolves in both directions and the entity has a row — for every sample.
fn assert_identity(engine: &Engine, samples: &[Planted]) {
    let generation = engine.generation();
    let row_space = &generation.bundle.partitions["default"].slices["s0"].row_space;
    for planted in samples {
        assert_eq!(
            engine
                .resolve_external_id(planted.external_id.as_bytes())
                .expect("resolvable"),
            Some(planted.entity),
            "{} lost its forward binding",
            planted.external_id
        );
        assert_eq!(
            engine
                .external_id_of(planted.entity)
                .expect("no inconsistency"),
            Some(planted.external_id.as_bytes().to_vec()),
            "{} lost its reverse binding",
            planted.external_id
        );
        assert!(
            row_space.row_of(planted.entity).is_some(),
            "{} has no row",
            planted.external_id
        );
    }
}

/// **Several rounds of millions of rows, into a meaningful build, all of it queryable.**
///
/// See this module's doc for the seven properties and what each would catch.
#[test]
#[ignore = "minutes, and wants a release build — see the module doc"]
fn millions_of_ingested_rows_become_correctly_queryable() {
    let base = env_usize("TESSERA_SCALE_BASE", 1_000_000) as u64;
    let rounds = env_usize("TESSERA_SCALE_ROUNDS", 16);
    let batch = env_usize("TESSERA_SCALE_BATCH", 250_000);
    let total = base + (rounds * batch) as u64;
    eprintln!("scale: base={base} rounds={rounds} batch={batch} total={total}");

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let started = Instant::now();
    let (points, pairs) = (tmp.path().join("points.parquet"), tmp.path().join("pairs.parquet"));
    build_fixture_n(&root, &points, &pairs, base);
    // **Freed before the run, not with the tempdir.** At a 250M base the two inputs are ~5 GiB
    // beside a ~9 GiB bundle, and the build has already consumed them — holding them for the
    // lifetime of the test is 5 GiB of headroom spent on nothing. The bundle is what the rest of
    // this test reads.
    let _ = std::fs::remove_file(&points);
    let _ = std::fs::remove_file(&pairs);
    eprintln!(
        "  base build: {:?} ({:.2} GiB bundle)",
        started.elapsed(),
        bundle_bytes(&root) as f64 / (1u64 << 30) as f64
    );

    let engine = engine_at(tmp.path(), &root, total);
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let sparse = engine.authorise(&subset_credential()).unwrap();

    assert_eq!(
        masked_total(&engine, &full),
        base,
        "the base build is queryable before anything is ingested"
    );

    // ---- the rounds -------------------------------------------------------------------------
    let mut probes: Vec<Planted> = Vec::new();
    let mut codes: BTreeMap<String, (TesseraId, u64)> = BTreeMap::new();
    for round in 0..rounds {
        let (round_probes, timings) = ingest_round(&engine, round, batch);

        // **Property 1**, checked every round rather than only at the end: a round that lost its
        // batch is otherwise indistinguishable from one that never ran.
        let expected = base + ((round + 1) * batch) as u64;
        let seen = masked_total(&engine, &full);
        assert_eq!(
            seen, expected,
            "after round {round}, {seen} items are visible where {expected} were ingested"
        );

        // **Property 3**, captured at the round's own flush, before any merge has moved it.
        codes.extend(probe_codes(&engine, &full, &round_probes));
        probes.extend(round_probes);

        let stats = engine.write_executor_stats();
        let on_disc = bundle_bytes(&root);
        let live = live_bytes(&engine);
        let sweep = zoom_sweep(&engine, &full);
        eprintln!(
            "  round {round}: ack {:>7.1?} | publish {:>7.1?} | refresh {:>7.1?} | \
             visible={seen} segments={} deltas={} merges={} coalesces={}",
            timings.ack,
            timings.publish,
            timings.refresh,
            engine.generation().bundle.partitions["default"].slices["s0"]
                .segments
                .len(),
            engine.generation().bundle.partitions["default"]
                .manifest
                .deltas
                .len(),
            stats.merges,
            stats.coalesces,
        );
        // **Per (tile x segment), not per request.** A tile resolves to one contiguous row range
        // per live segment, so the request cost is the product — and the product is what stays
        // flat if the cost model holds. A raw per-request figure grows for two reasons at once
        // (more occupied tiles, more segments) and cannot tell which moved.
        let segments = engine.generation().bundle.partitions["default"].slices["s0"]
            .segments
            .len();
        // **The write-path stage split, at scale.** Zero without `bench-timing`. A microbenchmark
        // at a 1M base and this at 250M disagree about where ingest spends its time — the map
        // occupancy and the bundle's external-id run length both move — so the attribution has to
        // be taken here rather than transferred.
        let st = engine.write_executor_stats().stage_nanos;
        let per_row = |i: usize| st[i] as f64 / 1e3 / ((round + 1) * batch) as f64;
        eprintln!(
            "            stages us/row: {}",
            WriteStage::ALL
                .iter()
                .filter(|s| st[**s as usize] > 0)
                .map(|s| format!("{} {:.2}", s.name().trim(), per_row(*s as usize)))
                .collect::<Vec<_>>()
                .join("  "),
        );
        eprintln!(
            "            disc {:>7.1} MiB (live {:>7.1} MiB, {:.2}x orphan) | zoom {}",
            on_disc as f64 / (1 << 20) as f64,
            live as f64 / (1 << 20) as f64,
            on_disc as f64 / live.max(1) as f64,
            sweep
                .iter()
                .map(|(z, d, tiles)| format!(
                    "z{z}({tiles}t) {d:.1?} [{:.1}us/ts]",
                    d.as_secs_f64() * 1e6 / ((*tiles).max(1) * segments) as f64
                ))
                .collect::<Vec<_>>()
                .join("  "),
        );
    }

    // ---- let the maintenance passes drain ----------------------------------------------------
    // Each is selected on a tick and at most one of each runs at a time, so the steady state takes
    // several ticks after the last flush. Stops when a round changes nothing, rather than after a
    // fixed count — the same shape `soak.rs` uses, for the same reason.
    // **Patient enough for a pass to complete, not merely to be selected.** A coalesce over eight
    // ~1 MiB tiers runs on the pool for longer than a few 50 ms ticks, so a shorter quiet period
    // declares the node settled while the pass is still in flight — measured: an 8-round run
    // reported `coalesces=0` with the tier axis sitting at its selection width.
    let mut settled = 0;
    for _ in 0..400 {
        let before = engine.write_executor_stats();
        engine.request_flush();
        std::thread::sleep(Duration::from_millis(100));
        let after = engine.write_executor_stats();
        if after.merges == before.merges && after.coalesces == before.coalesces {
            settled += 1;
            if settled >= 5 {
                break;
            }
        } else {
            settled = 0;
        }
    }

    let generation = engine.generation();
    let partition = &generation.bundle.partitions["default"];
    let manifest = &partition.manifest;
    let stats = engine.write_executor_stats();
    eprintln!(
        "  settled: segments={}, deltas={}, runs={}, dict extents={}, locators={} \
         (merges={}, coalesces={}, refreshes={}, full builds={})",
        partition.slices["s0"].segments.len(),
        manifest.deltas.len(),
        manifest.external_id_runs.len(),
        manifest.dict_extents.len(),
        manifest.locator_extents.len(),
        stats.merges,
        stats.coalesces,
        engine.refreshes(),
        engine.full_projection_builds(),
    );

    // **Property 5.** The bound, not the count — the exact figures are a function of the policy
    // widths. What matters is that maintenance ran and did not fail its way to a low number.
    assert!(
        stats.merges > 0,
        "no merge ran, so nothing here exercised the row-space permutation at scale"
    );
    // **The entity-space pass, on its own cadence.** The tier axis reaches
    // `CoalescePolicy::width` (8) roughly every seven rounds once the merge is consuming segments
    // (the memo records the cadence a run actually saw). Asserted as a floor derived from the
    // round count rather than a constant, so turning the axes down does not fail it spuriously and
    // turning them up does not stop checking.
    let expected_coalesces = rounds / 8;
    assert!(
        stats.coalesces as usize >= expected_coalesces,
        "the entity-space coalesce ran {} times over {rounds} rounds, expected at least \
         {expected_coalesces} — the tier, run and dictionary axes are what it bounds",
        stats.coalesces
    );
    assert_eq!(
        (
            stats.merge_failures,
            stats.coalesce_failures,
            stats.flush_failures
        ),
        (0, 0, 0),
        "maintenance must not be failing its way to a low segment count: {stats:?}"
    );
    assert!(
        partition.slices["s0"].segments.len() < rounds + 1,
        "the segment axis is unbounded at scale: {} segments after {rounds} flushes",
        partition.slices["s0"].segments.len()
    );

    // ---- properties 1-4, against everything that has happened --------------------------------
    assert_eq!(
        masked_total(&engine, &full),
        total,
        "every ingested item survives both maintenance passes"
    );
    assert_identity(&engine, &probes);
    assert_eq!(
        probe_codes(&engine, &full, &probes),
        codes,
        "a merge carries the Morton code through byte-exactly — a code that changed is a \
         dequantise-and-requantise, and it moves points on a viewer's map"
    );

    // **Property 4.** Ground truth from the fixture's own rule, computed independently: every third
    // base item carries SUBSET_TERM (`common::terms_of`), and every fourth ingested one does.
    let expected_sparse = (0..base).filter(|i| i.is_multiple_of(3)).count() as u64
        + (0..batch).filter(|i| carries_subset(*i)).count() as u64 * rounds as u64;
    assert_eq!(
        masked_total(&engine, &sparse),
        expected_sparse,
        "the sparse principal's masked total must equal its ground truth — this is the number a \
         tier resolved against the wrong row space gets wrong while full coverage stays right"
    );

    // ---- property 7: a deny, after all of it -------------------------------------------------
    // **Chosen outside the sparse principal's set**, so the two totals must move differently: full
    // coverage loses the item, the sparse principal loses nothing. A deny that reached the wrong
    // entity would almost certainly move both.
    let suppressed = probes
        .iter()
        .find(|p| !p.subset)
        .expect("some planted row is outside the sparse principal's set")
        .clone();
    let before_deny = masked_total(&engine, &full);
    engine
        .accept_change(suppressed.entity, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    assert_eq!(
        masked_total(&engine, &full),
        before_deny - 1,
        "the suppression is in force"
    );
    assert_eq!(
        masked_total(&engine, &sparse),
        expected_sparse,
        "and it moves the sparse principal's total by nothing — the suppressed row was never in          its set, so a deny that reached a different entity shows up here"
    );
    let (sx, sy) = (suppressed.x as f64, suppressed.y as f64);
    let bbox = [
        sx - PROBE_HALF_WIDTH,
        sy - PROBE_HALF_WIDTH,
        sx + PROBE_HALF_WIDTH,
        sy + PROBE_HALF_WIDTH,
    ];
    let tessera_id = engine.tessera_id_of(suppressed.entity).unwrap();
    assert!(
        !viewport(&engine, &full, bbox, PROBE_ZOOM)
            .points
            .iter()
            .any(|p| p.tessera_id == tessera_id),
        "and it is the suppressed entity that is gone, not merely one item"
    );

    // ---- property 6: the restart -------------------------------------------------------------
    drop(engine);
    let t = Instant::now();
    let reopened = engine_at(tmp.path(), &root, total);
    eprintln!("  restart: {:?}", t.elapsed());
    let full = reopened.authorise(&full_coverage_credential()).unwrap();
    let sparse = reopened.authorise(&subset_credential()).unwrap();

    assert_eq!(
        masked_total(&reopened, &full),
        total - 1,
        "the reopened node serves every ingested item, less the suppressed one"
    );
    assert_eq!(
        masked_total(&reopened, &sparse),
        expected_sparse,
        "and the sparse principal's total is unchanged by the restart"
    );
    assert!(
        reopened
            .generation()
            .overlay
            .is_suppressed(suppressed.entity),
        "the suppression survives the restart"
    );
    assert_identity(&reopened, &probes);

    // Every probe but the suppressed one still carries its original code.
    let mut expected_codes = codes;
    expected_codes.remove(&suppressed.external_id);
    let surviving: Vec<Planted> = probes
        .iter()
        .filter(|p| p.external_id != suppressed.external_id)
        .cloned()
        .collect();
    assert_eq!(
        probe_codes(&reopened, &full, &surviving),
        expected_codes,
        "and every position survives the restart byte-exactly"
    );

    eprintln!("scale: {total} items, {:?} end to end", started.elapsed());
}
