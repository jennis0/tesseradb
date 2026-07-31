//! **The `/control/changes` write path** — deletes, suppressions, unsuppressions and predicate
//! changes.
//!
//! # Why this path gets its own arm
//!
//! It is the one write path where a latency regression is a **security** regression. Lifecycle
//! §1.3 bounds deny visibility latency by *(queue-front + fsync) and nothing else*, and SA §4.2
//! records that `/control/changes` carrying a deny disposition is **never refused for capacity**,
//! because refusing a security operation for load is fail-open. Both statements are claims about
//! latency, and neither has been measured.
//!
//! It is also the only write path in Phase 1 whose effect is *observable*. Ingested rows are
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
//! failure mode this path exists to prevent (design: "deny handling is fail-closed with three
//! distinct retirement rules ... conflating them is fail-open — caught in review twice").
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
//! **The three ops are indistinguishable in cost** at equal overlay depth (18.4–18.7 ns/entry,
//! ~2.5 ms ack). They differ in retirement rule, not in price.
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
    /// Retires by the epoch ledger.
    Delete,
    /// Retires at its compaction fold.
    Predicate,
}

impl Op {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "suppress" => Some(Op::Suppress),
            "delete" => Some(Op::Delete),
            "predicate" => Some(Op::Predicate),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Op::Suppress => "suppress",
            Op::Delete => "delete",
            Op::Predicate => "predicate",
        }
    }
    fn change_op(&self) -> ChangeOp {
        match self {
            Op::Suppress => ChangeOp::Suppress,
            Op::Delete => ChangeOp::Delete,
            Op::Predicate => ChangeOp::Predicate,
        }
    }
    /// Whether this op, **as this arm invokes it**, should reduce the masked count by one per
    /// entity.
    ///
    /// True for all three, but for different reasons, and the distinction matters. `Suppress` and
    /// `Delete` deny outright. `Predicate` re-evaluates the item's terms — and this arm supplies
    /// an *empty* descriptor set, so the item ends up satisfying nothing and drops out of every
    /// principal's mask. A predicate change carrying real descriptors would move the count by an
    /// amount that depends on those descriptors, which is why this is a property of how the arm
    /// calls the op rather than of the op alone.
    ///
    /// The three ops still differ in their **retirement rules** (lifecycle §3: deletes retire by
    /// the epoch ledger, suppressions only on unsuppress, predicate changes at their compaction
    /// fold). Conflating those is fail-open and was caught twice in review — but it is a
    /// correctness property for the conformance suite, not something this arm measures.
    fn removes_from_mask(&self) -> bool {
        matches!(self, Op::Suppress | Op::Delete | Op::Predicate)
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
        let slice_id = bundle
            .partitions
            .values()
            .next()
            .and_then(|p| p.slices.keys().next().cloned())
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
                    compute_threads: tessera_engine::default_compute_threads(),
                    pin_ttl_secs: 300,
                    pins_per_session_max: 4,
                },
            )?;
            let session = engine.authorise(grant.auth_json(&dictionary).as_bytes())?;

            // Zoom 0: one tile spanning every row, so `sigma_visible` is the mask cardinality and
            // the arithmetic below is exact. Also warms the row-projection cache, which must not
            // land in any sample.
            let baseline = engine
                .viewport(&session, ViewportRequest::new(&slice_id, 0, full_bbox, 0))?
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
                    let external_id = format!("bench-change-{}", entity.raw()).into_bytes();
                    let descriptors = if op == Op::Predicate {
                        // Re-evaluate against an empty descriptor set: the item satisfies nothing,
                        // so a predicate change is observable rather than a no-op.
                        Some(Vec::new())
                    } else {
                        None
                    };
                    let start = std::time::Instant::now();
                    engine.accept_change(external_id, entity, op.change_op(), descriptors)?;
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
                        .viewport(&session, ViewportRequest::new(&slice_id, 0, full_bbox, 0))
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
