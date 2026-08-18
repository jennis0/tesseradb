//! **The `/control/changes` write path** — deletes, suppressions and unsuppressions.
//!
//! # Why this path gets its own arm
//!
//! It is the one write path where a latency regression is a **security** regression. Lifecycle
//! §1.3 bounds deny visibility latency by *(queue-front + fsync) and nothing else*, and SA §4.2
//! records that `/control/changes` carrying a deny disposition is **never refused for capacity**,
//! because refusing a security operation for load is fail-open. Both statements are claims about
//! latency, and neither has been measured.
//!
//! It is also the only write path whose effect is *observable*. Ingested rows are
//! durable but invisible — a buffered entity has no row in the segment's permutation, so
//! `compose` skips it (see `arms::ingest`'s scope note). A change targets an entity that already
//! **has** a row, so `perm.row_of` resolves, the overlay entry lands in the diff, and the masked
//! count genuinely moves. That difference is what makes this arm complementary rather than
//! duplicative:
//!
//! * `ingest` measures composition over buffer entries that are all **rejected** — F2's per-item
//!   **lower** bound.
//! * `changes` measures composition over overlay entries that all **resolve** and build diff
//!   bitmaps — F2's realistic **upper** bound.
//!
//! # The correctness check is the point, not a bonus
//!
//! At zoom 0 there is exactly one tile spanning every row, so `sigma_visible` *is* the mask
//! cardinality. Suppressing `K` entities that the principal could see must therefore drop
//! `sigma_visible` by **exactly** `K`. That is an exact arithmetic relationship, and this arm
//! reports whether it held. A benchmark of a suppression path that never checks the suppression
//! took effect would happily report throughput for a no-op — and fail-open is precisely the
//! failure mode this path exists to prevent (write-path §5.4: deny handling is fail-closed with
//! two distinct retirement rules — conflating them is fail-open, caught in review twice).
//!
//! # Measured, 2026-07-30, 2.42M `categories-subclass`, 5% coverage, zoom 0
//!
//! | op | overlay | ack min | ack p99 | ack max | read | compose | ns/entry | compose % |
//! |---|---:|---:|---:|---:|---:|---:|---:|---:|
//! | suppress | 100 | 2584 us | 6613 us | 6613 us | 94 us | 5.0 us | 50.1 | 5.3% |
//! | suppress | 1,000 | 2534 | 5474 | 8022 | 121 | 23.6 | 23.6 | 19.6% |
//! | suppress | 5,000 | 2485 | 4013 | 6011 | 206 | 97.8 | 19.6 | 47.7% |
//! | suppress | 20,000 | 2514 | 3988 | 9379 | 513 | 373.3 | 18.7 | 73.0% |
//! | delete | 20,000 | 2528 | 3981 | 9079 | 512 | 367.8 | 18.4 | 73.2% |
//! | predicate | 20,000 | 2509 | 7057 | **172627** | 509 | 373.6 | 18.7 | 73.6% |
//!
//! The `predicate` row is kept because it was measured; **the op no longer exists** (decisions
//! 0047 and 0048) and this arm cannot produce that row again.
//!
//! **Visibility arithmetic held in all twelve cells** — every deny denied, exactly once per
//! entity.
//!
//! **The deny floor is one fsync, ~2.5 ms, and it does not move with overlay depth.** That is
//! lifecycle §1.3's bound behaving as specified: queue-front plus fsync and nothing else. The
//! *tail* is another matter — p99 runs 4–7 ms and one predicate cell reached **172 ms**, a ~70x
//! excursion over the floor on a path where the latency bound is a security property. Nothing
//! architectural sits in that path, so the excursion is the fsync itself; it is worth a look
//! before anyone quotes the floor as the deny latency.
//!
//! **The ops measured were indistinguishable in cost** at equal overlay depth (18.4–18.7
//! ns/entry, ~2.5 ms ack). They differ in retirement rule, not in price.
//!
//! **F2, upper bound: ~18.5 ns per overlay entry**, converging from above as fixed overhead is
//! amortised (50.1 → 23.6 → 19.6 → 18.7). Against `arms::ingest`'s ~10 ns per *buffer* entry, an
//! overlay entry costs **~1.9x** a buffer entry — the difference being that overlay entries
//! resolve through `perm.row_of` and build diff bitmaps, where buffer entries are rejected. So
//! F2's real per-item cost sits in a measured 10–19 ns band depending on which structure grows.
//!
//! Extrapolating 18.5 ns/entry to SA §7's intended `overlay_soft_limit` of 500,000 gives
//! **~9.3 ms of `compose` per viewport**. And `compose` runs once per request *before* the tile
//! loop, so this is **zoom-independent**: a flat tax on every request regardless of what the
//! viewer is looking at, not something a cheap viewport escapes.

use tessera_authz::PostingsReader;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::ChangeOp;
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;
use tessera_types::EntityId;

use crate::arms::{Context, Result};
use crate::corpus::{build_grant_to_coverage, Dictionary, GrantShape, TermStats};
use crate::report::{Stages, Work};

/// The disposition under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// The security-critical one: retires *only* on unsuppress, never touches postings.
    Suppress,
    /// Retires at the compaction fold that executes it.
    Delete,
}

impl Op {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "suppress" => Some(Op::Suppress),
            "delete" => Some(Op::Delete),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Op::Suppress => "suppress",
            Op::Delete => "delete",
        }
    }
    fn change_op(&self) -> ChangeOp {
        match self {
            Op::Suppress => ChangeOp::Suppress,
            Op::Delete => ChangeOp::Delete,
        }
    }
    /// Whether this op, **as this arm invokes it**, should reduce the masked count by one per
    /// entity.
    ///
    /// True for both: `Suppress` and `Delete` deny outright.
    ///
    /// The two still differ in their **retirement rules** (write-path §5.4: a suppression retires
    /// only on unsuppress — Rule S; a deletion only at the compaction fold that executes it —
    /// Rule F). Conflating those is fail-open and was caught twice in review — but it is a
    /// correctness property for the conformance suite, not something this arm measures.
    fn removes_from_mask(&self) -> bool {
        matches!(self, Op::Suppress | Op::Delete)
    }
}

pub fn run(ctx: &Context, ops: &[String], checkpoints: &[u64], seed: u64) -> Result<()> {
    let mut run = ctx.open("changes")?;

    let ops: Vec<Op> = ops
        .iter()
        .map(|o| Op::parse(o).ok_or_else(|| format!("unknown op {o:?}")))
        .collect::<std::result::Result<_, _>>()?;

    for fixture in &ctx.fixtures {
        let postings = PostingsReader::open(&fixture.postings_path(), true)?;
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
        let q = bundle.manifest.quantisation;
        let full_bbox = [q.x_min, q.y_min, q.x_max, q.y_max];
        let view_id = bundle
            .partitions
            .values()
            .next()
            .and_then(|p| p.views.keys().next().cloned())
            .unwrap_or_else(|| "s0".to_string());
        drop(bundle);

        // The entities to act on: drawn from the principal's own visible set, so a suppression
        // has something to suppress. Acting on entities the principal cannot see would leave the
        // masked count unchanged and the correctness check vacuously true.
        let visible_entities = crate::postings::union(&postings, &grant.terms)?;
        let max_needed = checkpoints.iter().copied().max().unwrap_or(0) as usize;
        let targets: Vec<EntityId> = visible_entities
            .iter()
            .take(max_needed)
            .map(|e| EntityId::new(e as u64))
            .collect();
        if targets.len() < max_needed {
            eprintln!(
                "changes: {}/{} has only {} visible entities, capping checkpoints",
                fixture.scale,
                fixture.label_set,
                targets.len()
            );
        }

        for &op in &ops {
            let tmp = std::env::temp_dir().join(format!(
                "tessera-bench-changes-{}-{}-{}",
                std::process::id(),
                fixture.scale,
                op.name()
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
                    tier_width: None,
                    segment_floor_bytes: None,
                    coalesce_width: None,
                    // Compaction §9's trigger is off unless a deployment configures one.
                    compaction: tessera_engine::CompactionSchedule::off(),
                },
            )?;
            // The WAL lives on a dedicated executor thread, so an engine that writes must start
            // one. Bound is generous — this harness never means to measure queue-full
            // backpressure, only ack latency.
            engine.start_write_executor(1024)?;
            let session = engine.authorise(grant.auth_json(&dictionary).as_bytes())?;

            // Zoom 0: one tile spanning every row, so `sigma_visible` is the mask cardinality and
            // the arithmetic below is exact. Also warms the row-projection cache, which must not
            // land in any sample.
            let baseline = engine
                .viewport(&session, ViewportRequest::new(&view_id, 0, full_bbox, 0))?
                .timings
                .sigma_visible;

            let mut applied = 0u64;
            for &checkpoint in checkpoints {
                let limit = checkpoint.min(targets.len() as u64);
                if limit <= applied {
                    continue;
                }

                // Apply up to the checkpoint, timing every ack. Unlike ingest — where a 3.2 ms
                // fsync floor buried everything — these are recorded individually so the
                // distribution, not just a mean, is available: a *deny* path's tail is the
                // number that matters, since it bounds how long a revoked viewer keeps seeing.
                let mut acks = Vec::new();
                while applied < limit {
                    let entity = targets[applied as usize];
                    let start = std::time::Instant::now();
                    engine.accept_change(entity, op.change_op())?;
                    acks.push(start.elapsed().as_nanos() as u64);
                    applied += 1;
                }

                let cell_id = format!(
                    "changes/{}/{}/{}/n{}",
                    fixture.scale,
                    fixture.label_set,
                    op.name(),
                    limit
                );
                if run.ledger.is_done(&cell_id) {
                    run.skipped += 1;
                    continue;
                }

                // Read cost at this overlay depth, same whole-extent viewport every time.
                let mut last = None;
                let read_samples = crate::metrics::repeat(ctx.repeat, || {
                    let out = engine
                        .viewport(&session, ViewportRequest::new(&view_id, 0, full_bbox, 0))
                        .expect("viewport");
                    last = Some(out.timings);
                    out
                });
                let Some(t) = last else { continue };

                // The exact check: a deny disposition must remove exactly one item per entity.
                let expected_drop = if op.removes_from_mask() { limit } else { 0 };
                let actual_drop = baseline.saturating_sub(t.sigma_visible);
                let holds = actual_drop == expected_drop;

                let mut flags = Vec::new();
                if !holds {
                    // Loud, because the alternative reading is that a suppression did not
                    // suppress — which is the fail-open this path exists to prevent.
                    flags.push(format!(
                        "VISIBILITY_ARITHMETIC_FAILED expected_drop={expected_drop} actual={actual_drop}"
                    ));
                    eprintln!(
                        "changes: {cell_id}: expected sigma_visible to drop by {expected_drop}, \
                         it dropped by {actual_drop} — a deny that does not deny is fail-open"
                    );
                }

                let ack_min = acks.iter().copied().min().unwrap_or(0);
                let ack_max = acks.iter().copied().max().unwrap_or(0);
                let mut sorted = acks.clone();
                sorted.sort_unstable();
                let ack_p99 = sorted
                    .get(
                        ((sorted.len() as f64 * 0.99) as usize).min(sorted.len().saturating_sub(1)),
                    )
                    .copied()
                    .unwrap_or(0);

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
                        "op": op.name(),
                        "overlay_entries": limit,
                        "ack_min_ns": ack_min,
                        "ack_p99_ns": ack_p99,
                        "ack_max_ns": ack_max,
                        // F2's realistic upper bound: these overlay entries all resolve to a row
                        // and build diff bitmaps, unlike ingest's rejected buffer entries.
                        "compose_ns": t.compose_ns,
                        "compose_ns_per_overlay_entry": t.compose_ns as f64 / limit.max(1) as f64,
                        "compose_pct": 100.0 * t.compose_ns as f64 / t.total_ns.max(1) as f64,
                        "baseline_sigma_visible": baseline,
                        "expected_drop": expected_drop,
                        "actual_drop": actual_drop,
                        "visibility_arithmetic_holds": holds,
                        "seed": seed,
                    }),
                    work,
                    read_samples,
                    Some(Stages::from_engine(&t, run.clock_lap_ns)),
                    flags,
                )?;
            }

            drop(engine);
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    run.finish();
    Ok(())
}

// =================================================================================================
// Mode: deny_ack — the security-critical latency, against the two things that actually move it
// =================================================================================================

/// **Deny-ack latency against buffered-item depth, quiescent and contended, plus never-shed.**
///
/// # What the existing `run` above does not measure, and why that mattered
///
/// `run` grows the **overlay** and measures ack against it. It never ingests, so its buffer is
/// empty in every cell. That leaves the quantity the deny-ack floor is actually sized from —
/// `ExecutorHealth::apply_nanos_*`, whose doc calls the clone "O(total buffered items)" — entirely
/// unexercised, and it leaves lifecycle §1.3's real bound ("a deny's wait is bounded by the work
/// item currently executing") untested, because nothing is ever executing.
///
/// This mode measures three things `run` cannot:
///
/// 1. **Quiescent deny-ack against buffered depth.** Ingest to depth `N`, then time denies on an
///    otherwise idle executor, reading `write_executor_stats()` immediately either side of each
///    one so the delta in `apply_nanos_total` *is* that deny's own apply step, exactly.
/// 2. **Contended deny-ack.** Background threads submit large ingest batches continuously while
///    denies are issued, so a deny lands behind an in-flight `apply_ingest` that is cloning an
///    `N`-entry buffer. This is the head-of-line term, and it is the one §1.3 bounds.
/// 3. **Never-shed** (`tessera-engine/src/write.rs`'s two-lane submit): the work lane is a bounded
///    `SyncSender` (`QueueFull`), the deny lane an unbounded `Sender`. With the queue genuinely
///    saturated — proven by counting `QueueFull` on the ingest lane rather than assumed — no deny
///    may be refused. **Scope limit: this exercises the engine's lane split only.** The HTTP-level
///    asymmetry is a different claim and is not made here; `changes_never_429s` in
///    `tessera-server/tests/http_write.rs` is what asserts it.
// Eight parameters, one over clippy's default. A bench arm's signature IS its knob surface --
// ops, buffered depths, submitters, repeats and seed are each independently swept from the CLI,
// and folding them into a params struct would put a second name on every one of them for no
// reader's benefit. The sibling arms in this crate take the same shape.
#[allow(clippy::too_many_arguments)]
pub fn run_deny_ack(
    ctx: &Context,
    ops: &[String],
    buffered: &[u64],
    denies: usize,
    flood_workers: usize,
    ingest_batch: usize,
    queue_bound: usize,
    seed: u64,
) -> Result<()> {
    let mut run = ctx.open("deny_ack")?;

    let ops: Vec<Op> = ops
        .iter()
        .map(|o| Op::parse(o).ok_or_else(|| format!("unknown op {o:?}")))
        .collect::<std::result::Result<_, _>>()?;

    for fixture in &ctx.fixtures {
        let postings = PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
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
        let visible_entities = crate::postings::union(&postings, &grant.terms)?;
        // The buffer fill below is a *background* condition, not this arm's subject, so it is
        // built at the same stated term density every other ingest arm writes at — see
        // `arms::ingest`'s `grant_pool`. A density that varied here would move the deny-ack floor
        // for a reason that has nothing to do with denies.
        let dictionary = Dictionary::open(&fixture.root, &fixture.prefix)?;
        let (fill_terms, fill_descriptors) =
            crate::arms::ingest::ingest_density(&grant, &dictionary);

        for &op in &ops {
            for &depth in buffered {
                let cell_id = format!(
                    "deny_ack/{}/{}/{}/buffered{}",
                    fixture.scale,
                    fixture.label_set,
                    op.name(),
                    depth
                );
                if run.ledger.is_done(&cell_id) {
                    run.skipped += 1;
                    continue;
                }

                // A fresh engine per cell: the overlay must start empty, or the deny's own
                // `apply_change` clone (which is O(overlay), not O(buffer) — see the finding in
                // the memo) would carry the previous cell's depth into this one's numbers.
                let tmp = std::env::temp_dir().join(format!(
                    "tessera-bench-denyack-{}-{}-{}-{}",
                    std::process::id(),
                    fixture.scale,
                    op.name(),
                    depth
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
                        tier_width: None,
                        segment_floor_bytes: None,
                        coalesce_width: None,
                        // Compaction §9's trigger is off unless a deployment configures one.
                        compaction: tessera_engine::CompactionSchedule::off(),
                    },
                )?;
                // Small on purpose, unlike the other arms' generous 1024: the never-shed phase
                // needs the work lane to actually saturate, and a deep queue would measure
                // patience rather than the lane split.
                engine.start_write_executor(queue_bound)?;

                // ---- fill the buffer to `depth` -----------------------------------------------
                let fill_started = std::time::Instant::now();
                let mut next_id = 0u64;
                let mut filled = 0u64;
                while filled < depth {
                    let n = ingest_batch.min((depth - filled) as usize);
                    let rows =
                        crate::arms::ingest::synth_rows(n, next_id, &fill_terms, &fill_descriptors);
                    next_id += n as u64;
                    engine.accept_ingest(rows, format!("fill-{next_id}"), [0u8; 32])?;
                    filled += n as u64;
                }
                let fill_ns = fill_started.elapsed().as_nanos() as u64;

                // ---- phase 1: quiescent deny-ack ----------------------------------------------
                let mut targets = visible_entities.iter().map(|e| EntityId::new(e as u64));
                let mut quiet_ack = Vec::with_capacity(denies);
                let mut quiet_apply = Vec::with_capacity(denies);
                for _ in 0..denies {
                    let Some(entity) = targets.next() else { break };
                    let before = engine.write_executor_stats();
                    let start = std::time::Instant::now();
                    engine.accept_change(entity, op.change_op())?;
                    let ack = start.elapsed().as_nanos() as u64;
                    let after = engine.write_executor_stats();
                    quiet_ack.push(ack);
                    // The executor is idle, so exactly one apply happened between the two reads:
                    // this delta IS this deny's own apply step.
                    quiet_apply.push(after.apply_nanos_total - before.apply_nanos_total);
                }

                // ---- phase 2: contended deny-ack, and never-shed ------------------------------
                let stop = std::sync::atomic::AtomicBool::new(false);
                let queue_full = std::sync::atomic::AtomicU64::new(0);
                let ingest_ok = std::sync::atomic::AtomicU64::new(0);
                let deny_refused = std::sync::atomic::AtomicU64::new(0);
                let mut busy_ack: Vec<u64> = Vec::with_capacity(denies);
                let apply_max_before = engine.write_executor_stats().apply_nanos_max;

                std::thread::scope(|scope| -> Result<()> {
                    let engine = &engine;
                    let (stop, queue_full, ingest_ok) = (&stop, &queue_full, &ingest_ok);
                    let (terms, descriptors) = (&fill_terms, &fill_descriptors);
                    let mut floods = Vec::new();
                    for w in 0..flood_workers {
                        floods.push(scope.spawn(move || {
                            let mut id = 1_000_000_000u64 + (w as u64) * 100_000_000;
                            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                                let rows = crate::arms::ingest::synth_rows(
                                    ingest_batch,
                                    id,
                                    terms,
                                    descriptors,
                                );
                                id += ingest_batch as u64;
                                match engine.accept_ingest(
                                    rows,
                                    format!("flood-{w}-{id}"),
                                    [0u8; 32],
                                ) {
                                    Ok(_) => {
                                        ingest_ok
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    Err(tessera_engine::AcceptError::Submit(_)) => {
                                        // The bounded work lane refusing under load: the
                                        // *expected* half of the asymmetry, and the proof that
                                        // the queue is genuinely saturated for the deny phase.
                                        queue_full
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    Err(_) => {}
                                }
                            }
                        }));
                    }

                    // Let the flood saturate the queue before any deny is timed.
                    std::thread::sleep(std::time::Duration::from_millis(200));

                    for _ in 0..denies {
                        let Some(entity) = targets.next() else { break };
                        let start = std::time::Instant::now();
                        let r = engine.accept_change(entity, op.change_op());
                        busy_ack.push(start.elapsed().as_nanos() as u64);
                        if matches!(r, Err(tessera_engine::AcceptError::Submit(_))) {
                            // The failure this whole arm exists to detect: a security operation
                            // refused for load. Never acceptable (SA §4.2, contracts §3.1).
                            deny_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }

                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    for f in floods {
                        let _ = f.join();
                    }
                    Ok(())
                })?;

                let final_stats = engine.write_executor_stats();
                let refused = deny_refused.load(std::sync::atomic::Ordering::Relaxed);
                let saturated = queue_full.load(std::sync::atomic::Ordering::Relaxed);

                let mut flags = Vec::new();
                if refused > 0 {
                    flags.push(format!("DENY_REFUSED_FOR_LOAD n={refused}"));
                    eprintln!(
                        "deny_ack: {cell_id}: {refused} denies refused for load — refusing a \
                         security operation for capacity is fail-open (SA §4.2)"
                    );
                }
                if saturated == 0 {
                    // Not a failure of the system; a failure of the *experiment*. Recorded so a
                    // reader never reads "no deny was refused" as evidence when the queue was
                    // never full in the first place.
                    flags.push("WORK_QUEUE_NEVER_SATURATED".to_string());
                }

                let pct = |v: &mut Vec<u64>, p: f64| -> u64 {
                    if v.is_empty() {
                        return 0;
                    }
                    v.sort_unstable();
                    v[(((v.len() as f64) * p) as usize).min(v.len() - 1)]
                };
                let mut q = quiet_ack.clone();
                let mut b = busy_ack.clone();
                let mut qa = quiet_apply.clone();

                run.emit(
                    cell_id,
                    fixture,
                    serde_json::json!({
                        "op": op.name(),
                        "buffered_items": depth,
                        "ingest_batch": ingest_batch,
                        "queue_bound": queue_bound,
                        "flood_workers": flood_workers,
                        "fill_ns": fill_ns,
                        // Phase 1 — quiescent.
                        "quiet_ack_p50_ns": pct(&mut q, 0.50),
                        "quiet_ack_p99_ns": pct(&mut q, 0.99),
                        "quiet_ack_min_ns": quiet_ack.iter().copied().min().unwrap_or(0),
                        "quiet_ack_max_ns": quiet_ack.iter().copied().max().unwrap_or(0),
                        // The deny's OWN apply step, exactly (idle executor, delta of one apply).
                        "quiet_apply_p50_ns": pct(&mut qa, 0.50),
                        "quiet_apply_max_ns": quiet_apply.iter().copied().max().unwrap_or(0),
                        "quiet_apply_share_of_ack": quiet_apply.iter().sum::<u64>() as f64
                            / quiet_ack.iter().sum::<u64>().max(1) as f64,
                        // Phase 2 — contended.
                        "busy_ack_p50_ns": pct(&mut b, 0.50),
                        "busy_ack_p99_ns": pct(&mut b, 0.99),
                        "busy_ack_max_ns": busy_ack.iter().copied().max().unwrap_or(0),
                        // The counter the deny-ack floor is sized from. Its max over the
                        // contended phase is the ingest lane's apply, i.e. the head-of-line term
                        // a deny can queue behind.
                        "apply_nanos_max_before_flood": apply_max_before,
                        "apply_nanos_max_after_flood": final_stats.apply_nanos_max,
                        "apply_nanos_total": final_stats.apply_nanos_total,
                        "wal_appends": final_stats.wal_appends,
                        "wal_fsyncs": final_stats.wal_fsyncs,
                        "work_submitted": final_stats.work_submitted,
                        "deny_submitted": final_stats.deny_submitted,
                        // Never-shed.
                        "ingest_queue_full_count": saturated,
                        "ingest_accepted_count": ingest_ok.load(std::sync::atomic::Ordering::Relaxed),
                        "denies_refused_for_load": refused,
                        "never_shed_holds": refused == 0,
                        "never_shed_exercised": saturated > 0,
                        "seed": seed,
                    }),
                    Work {
                        coverage,
                        ..Default::default()
                    },
                    quiet_ack.clone(),
                    None,
                    flags,
                )?;

                drop(engine);
                let _ = std::fs::remove_dir_all(&tmp);
            }
        }
    }

    run.finish();
    Ok(())
}
