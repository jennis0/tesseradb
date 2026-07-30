//! **Ask 4: true wire calls with N simultaneous users.**
//!
//! An HTTP load generator driving `POST /v1/viewport` against a real `tessera serve`. Orchestration
//! — booting the server, authorising the sessions, sampling the server's RSS and CPU — lives in
//! `scripts/bench_concurrency.py`, which reuses the proven `reference/oracle/harness.py` machinery
//! rather than reimplementing config writing and boot polling here. This file is the hot loop.
//!
//! # Why the generator is Rust
//!
//! At 1000 concurrent connections pulling multi-megabyte Arrow bodies, a Python client is the
//! bottleneck and the measurement becomes a benchmark of the client. That is not a hypothesis to
//! be assumed away: `--target healthz` runs the identical concurrency ladder against a trivial
//! endpoint, establishing the generator's own ceiling, and any viewport cell within 3x of it is
//! stamped `generator_bound` so a reader can discard it.
//!
//! # Two load modes, both reported
//!
//! * **closed** (`--concurrency N`) — N workers, each issuing the next request as soon as its
//!   previous one returns. This is the literal reading of "N simultaneous users", and it is what
//!   was asked for.
//! * **open** (`--rate R`) — requests launched on a fixed schedule regardless of whether earlier
//!   ones have returned. Closed-loop hides overload: when the server slows, a closed-loop client
//!   politely slows with it and the latency distribution never shows the queue. Open-loop is the
//!   honest throughput number, so both are available and the report says which was used.
//!
//! # Two session arms
//!
//! The `--tokens` file decides which. `scripts/bench_concurrency.py` writes either N *distinct*
//! tokens (Arm A: N principals, N fragments, N row projections — the memory question) or a
//! handful reused across N workers (Arm B: shared fragments, isolating request-path contention).
//! The generator does not need to know which; it round-robins whatever it is given, and the
//! record carries `distinct_tokens` so the two are never confused after the fact.
//!
//! # Measured, 2026-07-30, 2.42M `categories-subclass`, w=10, k=30, zoom 8, 8 s per cell
//!
//! Generator ceiling against `/healthz`: 28.4k / 59.3k / 123.6k / 110.5k rps at c=5/10/100/1000.
//! It *falls* at c=1000 with a 22 ms p99 — the generator is itself saturating there, so treat the
//! c=1000 row as a floor on what the server could do, not a measurement of what it did.
//!
//! | arm | conc | rps | p50 | p99 | server p99 | server CPU |
//! |---|---:|---:|---:|---:|---:|---:|
//! | B shared | 5 | 15,277 | 0.25 ms | 1.09 ms | 0.80 ms | 315% |
//! | B shared | 100 | 48,588 | 1.82 | 5.41 | 1.26 | 818% |
//! | B shared | 1000 | 49,475 | 17.63 | 60.20 | 1.61 | 865% |
//! | A distinct | 100 | 39,782 | 2.14 | 6.53 | 1.45 | 712% |
//! | A distinct | 1000 | **18,599** | 20.53 | **1042.31** | **10.49** | **426%** |
//!
//! **Arm B saturates cleanly.** Throughput plateaus at ~49k rps from c=100 onward and server-side
//! p99 stays at 1.6 ms while end-to-end p99 reaches 60 ms — 58 ms of that is pure queueing, not
//! server work. The server is CPU-bound at ~8.6 of 12 cores, with 4 more going to the generator.
//!
//! **F4 — Arm A at c=1000 is lock-bound, and the mechanism is `RowProjection::new` running under
//! the `row_projection_cache` mutex** (`tessera-engine/src/viewport.rs`, the `None` arm of the
//! cache lookup). Every distinct session's *first* viewport builds its entity→row projection while
//! holding one global lock, so 1000 distinct principals serialise there. The signature is
//! unmistakable: throughput halves (39.8k → 18.6k), end-to-end p99 reaches **1.04 s**, server-side
//! p99 goes to 10.5 ms — and **server CPU drops from 712% to 426%**. A CPU-bound system does not
//! get slower while using less CPU; those threads are blocked.
//!
//! This is separable from the memory question the arm was designed around, and is the more urgent
//! half: it bites at session *churn*, not at session count, so a deployment that rotates tokens
//! hourly meets it on every rotation. Building the projection outside the lock (or keying an
//! in-progress marker so concurrent builders wait per-key rather than globally) would fix it.
//!
//! **Memory: ~248 KiB per distinct session** at this scale (243 MiB of RSS delta over 1000
//! sessions; the smaller-N rows are noise at 1–4 MiB). That is the fragment plus its row
//! projection. The mask scales with the corpus, so the slope extrapolates to ~2.5 MB/session at
//! 25M and ~100 MB/session at 10⁹ — where 1000 live sessions would be ~100 GB, corroborating
//! design §13.1's own "a thousand live auth inputs is 125 GB". **The ceiling was not reached
//! here** (0.39 GiB peak); this is the slope, and it should be read as such.
//!
//! **Every Arm B cell is flagged `generator_bound`**, and honestly so: at 2.42M a viewport is
//! cheap enough (server p99 1.6 ms) that throughput lands within 3x of the generator's own
//! ceiling. Clean Arm B numbers need either a larger bundle — more server work per request — or
//! the generator on a different box. The flag exists to stop those numbers being quoted as
//! server measurements, and it should be believed.
//!
//! # Task 9 re-measurement, 2026-07-30, same fixture/params, post D-A..D-G (Tasks 1-8)
//!
//! Full numbers, per-criterion verdicts, and the shed/pan-storm/cold-build cells are in
//! `.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/task-9-report.md`; this is the headline only.
//!
//! | arm | conc | rps | p50 | p99 | server p99 | server CPU | shed% |
//! |---|---:|---:|---:|---:|---:|---:|---:|
//! | A distinct | 100 | 12,990 | 3.42 ms | 10.17 ms | 6.03 ms | 629% | 79.6% |
//! | A distinct | 1000 | 10,081 | 17.50 ms | **59.24 ms** | 8.61 ms | **610%** | 83.8% |
//!
//! **F4's lock-contention signature is gone.** Arm A c=1000 end-to-end p99 falls **1042 ms →
//! 59 ms** and server CPU no longer collapses with concurrency (537% → 646% → 629% → 610% across
//! c=5/10/100/1000 — a mild plateau from gate-shedding at high c, never the 712%→426% *collapse*
//! that was F4's smoking gun). `RowProjection::new` no longer runs under a global mutex (Tasks 1
//! and 2, D-G): concurrent first-touch races on the same key now shed with 429 instead of
//! queueing behind one lock.
//!
//! **Raw throughput does not clear the ≥35k rps bar** (10.1k measured) — not a regression, a
//! structural change the ≥35k figure predates: Task 4's admission gate (D-B) now caps
//! *sustained, running* compute at `compute_admission` (default = available cores, 12 here) with
//! a bounded queue on top, so successful-request throughput is deliberately ceilinged well below
//! what raw, ungated compute could push, in exchange for the availability guarantee the whole
//! workstream exists to provide. See the report for the full argument and the honest FAIL this
//! produces against the criterion's literal wording.
//!
//! **A new, non-F4 shed pattern appears at low concurrency** (c=5: 150 sheds, c=10: 245 sheds,
//! Arm B): confirmed via `/control/status`'s gate-only `shed_total` staying at 0 across the cell
//! (the D-B gate cannot trip this far under its 36-slot capacity) that these are D-G's
//! single-flight building-shed on Arm B's shared 4-token pool — a handful of workers racing the
//! *same* token's first viewport concurrently, exactly as designed, not a new defect.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::arms::{Context, Result};
use crate::report::Work;

/// Task 9's client-driven extensions to the plain closed/open-loop generator: the pan-storm mode
/// (abandon-and-reissue, D-C's cancellation payoff) and the cold-build storm (D-G's non-blocking
/// single-flight at a scale where a *blocking* waiter would have shed the world). Grouped into one
/// struct rather than four more positional arguments to `run`/`drive`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StormOptions {
    /// Each worker races its request against `abort_after_ms`: if the deadline fires first, the
    /// in-flight request is dropped (client-side cancel — `reqwest` tears down the connection,
    /// which is what flips the server's `CancelToken`, D-C) and the worker immediately issues its
    /// next request. A request that finishes before the deadline is recorded normally; nothing is
    /// aborted needlessly just because the mode is on.
    pub pan_storm: bool,
    pub abort_after_ms: u64,
    /// Independent of `pan_storm`: a hard client-side deadline used to positively detect a hang
    /// rather than rely on the 120 s connection-level timeout below, which is far too generous to
    /// serve as criterion 2's watchdog. Ignored when `pan_storm` is set (that mode's own deadline
    /// already bounds every request).
    pub hang_timeout_ms: Option<u64>,
    /// The cold-build storm's key split: the first `cold_workers` of `concurrency` reuse
    /// `tokens[0]` (one shared, never-yet-queried session) on every request; the rest round-robin
    /// over `tokens[1..]` (pre-warmed sessions, D-G's "other keys stay served" half of the claim).
    /// `0` disables the split and every worker round-robins the whole token list, as before this
    /// task.
    pub cold_workers: usize,
}

/// One request's outcome.
struct Sample {
    wall_ns: u64,
    server_us: u64,
    bytes: u64,
    /// `0` for a sample that never got an HTTP status at all — a transport error, a client-side
    /// abort, or a watchdog-detected hang; which of those is recorded in `aborted`/`hung` below.
    status: u16,
    /// This worker's request was itself cancelled by the pan-storm deadline before any response
    /// arrived — a deliberate, expected outcome of that mode, never counted as an error.
    aborted: bool,
    /// The hang-watchdog's hard deadline elapsed before a response arrived. Criterion 2 requires
    /// this to be `false` for every sample; a `true` here is exactly the violation it looks for.
    hung: bool,
    /// Only meaningful when `status == 429`: whether the response carried both `Retry-After: 1`
    /// and body `retry_after_s: 1` (D-E, fixed values). `None` for every other status.
    retry_after_ok: Option<bool>,
    /// This worker was one of the cold-build storm's `cold_workers` (see [`StormOptions`]).
    cold: bool,
    /// New bench metric: this 200 response's summed `served` column (`0` for any other status).
    points_served: u64,
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &Context,
    viewer_url: &str,
    tokens_path: &std::path::Path,
    concurrency: usize,
    duration_s: f64,
    threads: usize,
    open_rate: Option<f64>,
    target_healthz: bool,
    scale: u64,
    label_set: &str,
    k: usize,
    zoom: u8,
    seed: u64,
    storm: StormOptions,
) -> Result<()> {
    let mut run = ctx.open("load")?;

    let tokens: Vec<String> = std::fs::read_to_string(tokens_path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return Err(format!("no tokens in {}", tokens_path.display()).into());
    }
    if storm.cold_workers > 0 && tokens.len() < 2 {
        // The cold-build split needs `tokens[0]` (the cold key) AND at least one entry in
        // `tokens[1..]` (the warm pool) — a single-token file cannot exercise "warm traffic on
        // other keys keeps being served" at all.
        return Err(format!(
            "--cold-workers {} needs at least 2 tokens (1 cold + >=1 warm), got {} in {}",
            storm.cold_workers,
            tokens.len(),
            tokens_path.display()
        )
        .into());
    }
    if storm.cold_workers > concurrency {
        return Err(format!(
            "--cold-workers {} exceeds --concurrency {concurrency}",
            storm.cold_workers
        )
        .into());
    }
    let distinct_tokens = {
        let set: std::collections::HashSet<&String> = tokens.iter().collect();
        set.len()
    };

    // Viewports are drawn once and shared by every worker, so all N users are doing comparable
    // work. Varying geometry per worker would fold the corpus's density distribution into the
    // concurrency measurement — the exact confound the tail-attribution memo warned about.
    let plan = crate::corpus::gen_viewports(256, 65536.0, seed, &[zoom]);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;

    let samples = runtime.block_on(drive(
        viewer_url.to_string(),
        tokens.clone(),
        plan,
        concurrency,
        Duration::from_secs_f64(duration_s),
        open_rate,
        target_healthz,
        k,
        storm,
    ))?;

    if samples.is_empty() {
        return Err("no samples collected — did the server accept any request?".into());
    }

    // Per-status accounting (deliverable 1). `429` is a SHED, never an error — Task 4's gate and
    // Tasks 1-2's single-flight caches both produce it by design, and the ledger note on Arm B's
    // opening burst is explicit that counting it as an error would misreport a working mechanism
    // as a fault. `aborted` (pan-storm's own deadline) is likewise expected, not an error, and is
    // disjoint from `hung` by construction (only one of the two deadline kinds is ever armed).
    let ok: Vec<&Sample> = samples.iter().filter(|s| s.status == 200).collect();
    let shed: Vec<&Sample> = samples.iter().filter(|s| s.status == 429).collect();
    let aborted: Vec<&Sample> = samples.iter().filter(|s| s.aborted).collect();
    let hung: Vec<&Sample> = samples.iter().filter(|s| s.hung).collect();
    let other: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.status != 0 && s.status != 200 && s.status != 429)
        .collect();
    let network_errors: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.status == 0 && !s.aborted && !s.hung)
        .collect();
    // Genuinely unexpected outcomes only — see the accounting note above.
    let unexpected_errors = other.len() + network_errors.len() + hung.len();

    let wall: Vec<u64> = if !ok.is_empty() {
        ok.iter().map(|s| s.wall_ns).collect()
    } else {
        // A cell whose admission bound is forced low enough (criterion 3) can legitimately have
        // zero 200s in a short window; emitting on an empty Vec would panic `Timing::from_samples`
        // (a pre-existing latent bug this task's shed cell would otherwise have hit first).
        samples.iter().map(|s| s.wall_ns).collect()
    };
    let server: Vec<u64> = ok.iter().map(|s| s.server_us * 1000).collect();
    let bytes: u64 = ok.iter().map(|s| s.bytes).sum();

    // Every sample that reached a real HTTP verdict (200 or 429) has a meaningful wall time;
    // aborted/hung samples are capped at their own deadline and are not informative about how
    // long the server actually took, so they are excluded from this bound rather than silently
    // dominating it.
    let max_wall_ms = samples
        .iter()
        .filter(|s| s.status == 200 || s.status == 429)
        .map(|s| s.wall_ns)
        .max()
        .unwrap_or(0) as f64
        / 1e6;

    let retry_after_violations = shed
        .iter()
        .filter(|s| s.retry_after_ok == Some(false))
        .count();
    let shed_rate = shed.len() as f64 / samples.len() as f64;

    let throughput = ok.len() as f64 / duration_s;
    let mode = if open_rate.is_some() {
        "open"
    } else {
        "closed"
    };

    // New bench metrics (user-requested): points delivered, not just requests served. A 429 or a
    // hang delivers zero points by definition, so these are sums over `ok` only — same population
    // `throughput`/`server_timing` already use, for the same reason.
    let total_points_served: u64 = ok.iter().map(|s| s.points_served).sum();
    // Same denominator as `throughput` above (the cell's own measured window), so the two divide
    // consistently: `points_per_second / throughput` recovers mean points-per-response exactly.
    let points_per_second = total_points_served as f64 / duration_s;
    // `concurrency` is "how many simultaneous users", matching this arm's own module doc — not
    // `distinct_tokens`, which can be smaller (Arm B) and would inflate the per-user figure for a
    // shared-fragment cell that is not actually serving that many distinct principals faster.
    let points_per_user_per_second = points_per_second / concurrency.max(1) as f64;
    let points_per_request = if ok.is_empty() {
        0.0
    } else {
        total_points_served as f64 / ok.len() as f64
    };

    let mut flags = Vec::new();
    if unexpected_errors > 0 {
        flags.push(format!("errors={unexpected_errors}"));
    }
    if !hung.is_empty() {
        flags.push(format!("hangs={}", hung.len()));
    }
    if retry_after_violations > 0 {
        flags.push(format!("retry_after_violations={retry_after_violations}"));
    }
    if ok.is_empty() {
        flags.push("no_successful_requests".to_string());
    }
    if target_healthz {
        flags.push("generator_ceiling_calibration".to_string());
    }
    if storm.pan_storm {
        flags.push("pan_storm".to_string());
    }
    if storm.cold_workers > 0 {
        flags.push("cold_build_storm".to_string());
    }

    let cell_id = format!(
        "load/{}/{}/{}/{}/c{}{}{}",
        scale,
        label_set,
        if target_healthz {
            "healthz"
        } else {
            "viewport"
        },
        mode,
        concurrency,
        if storm.pan_storm { "/panstorm" } else { "" },
        if storm.cold_workers > 0 {
            "/coldbuild"
        } else {
            ""
        },
    );

    let work = Work {
        points_gathered: ok.len() as u64,
        bytes_touched: bytes,
        ..Default::default()
    };

    // `server` (and therefore `server_timing`) is built only from 200s, which can legitimately be
    // empty in the same forced-low-admission cell noted above — guarded rather than assumed
    // non-empty, for the same reason `wall` is.
    let server_timing = if server.is_empty() {
        None
    } else {
        Some(crate::report::Timing::from_samples(server))
    };

    // The cold-build storm's own split: cold-key samples (the single shared, first-touch token)
    // reported apart from warm-key samples (the pre-warmed pool), so a reader can see directly
    // that the storm on one key did not touch the other keys' service — criterion 6's claim.
    let cold_samples: Vec<&Sample> = samples.iter().filter(|s| s.cold).collect();
    let warm_samples: Vec<&Sample> = samples.iter().filter(|s| !s.cold).collect();
    let cold_block = if storm.cold_workers > 0 {
        let cold_ok = cold_samples.iter().filter(|s| s.status == 200).count();
        let cold_shed = cold_samples.iter().filter(|s| s.status == 429).count();
        let cold_max_wall_ms = cold_samples
            .iter()
            .filter(|s| s.status == 200 || s.status == 429)
            .map(|s| s.wall_ns)
            .max()
            .unwrap_or(0) as f64
            / 1e6;
        let warm_ok: Vec<u64> = warm_samples
            .iter()
            .filter(|s| s.status == 200)
            .map(|s| s.wall_ns)
            .collect();
        let warm_ok_count = warm_ok.len();
        let warm_timing = if warm_ok.is_empty() {
            None
        } else {
            Some(crate::report::Timing::from_samples(warm_ok))
        };
        let warm_shed = warm_samples.iter().filter(|s| s.status == 429).count();
        serde_json::json!({
            "cold_requests": cold_samples.len(),
            "cold_requests_ok": cold_ok,
            "cold_requests_shed": cold_shed,
            "cold_max_wall_ms": cold_max_wall_ms,
            "warm_requests": warm_samples.len(),
            "warm_requests_ok": warm_ok_count,
            "warm_requests_shed": warm_shed,
            "warm_p50_ms": warm_timing.as_ref().map(|t| t.median_ns as f64 / 1e6),
            "warm_p99_ms": warm_timing.as_ref().map(|t| t.p99_ns as f64 / 1e6),
        })
    } else {
        serde_json::Value::Null
    };

    run.emit(
        cell_id,
        &crate::fixture::Fixture {
            scale,
            label_set: label_set.to_string(),
            root: std::path::PathBuf::from(viewer_url),
            prefix: String::new(),
            bytes: 0,
        },
        serde_json::json!({
            "mode": mode,
            "concurrency": concurrency,
            "open_rate": open_rate,
            "generator_threads": threads,
            "duration_s": duration_s,
            "target": if target_healthz { "healthz" } else { "viewport" },
            "tokens_supplied": tokens.len(),
            // Arm A vs Arm B, recorded rather than inferred: N distinct tokens means N distinct
            // fragments and row projections; a handful reused means shared ones.
            "distinct_tokens": distinct_tokens,
            "k": k,
            "zoom": zoom,
            "requests_total": samples.len(),
            "requests_ok": ok.len(),
            "requests_shed_429": shed.len(),
            "requests_other_status": other.len(),
            "requests_network_error": network_errors.len(),
            "requests_hung": hung.len(),
            "requests_aborted": aborted.len(),
            "requests_error": unexpected_errors,
            "shed_rate": shed_rate,
            "retry_after_violations": retry_after_violations,
            "max_wall_ms": max_wall_ms,
            "throughput_rps": throughput,
            "mean_bytes": bytes as f64 / ok.len().max(1) as f64,
            // New bench metrics (user-requested): delivered work, not just request counts.
            "points_served_total": total_points_served,
            "points_per_second": points_per_second,
            "points_per_user_per_second": points_per_user_per_second,
            "points_per_request": points_per_request,
            "server_us_p50": server_timing.as_ref().map(|t| t.median_ns / 1000).unwrap_or(0),
            "server_us_p99": server_timing.as_ref().map(|t| t.p99_ns / 1000).unwrap_or(0),
            "server_us_max": server_timing.as_ref().map(|t| t.max_ns / 1000).unwrap_or(0),
            "pan_storm": storm.pan_storm,
            "abort_after_ms": if storm.pan_storm { Some(storm.abort_after_ms) } else { None },
            "hang_timeout_ms": storm.hang_timeout_ms,
            "cold_workers": storm.cold_workers,
            "cold_build": cold_block,
            "seed": seed,
        }),
        work,
        wall,
        None,
        flags,
    )?;

    run.finish();
    Ok(())
}

/// One request's raw wire outcome, before it becomes a [`Sample`] — the common tail shared by the
/// plain, pan-storm, and hang-watchdog paths below so the three do not triplicate the
/// status/header/body extraction.
struct RawOutcome {
    status: u16,
    server_us: u64,
    retry_after_header_is_one: bool,
    body_len: u64,
    /// Whether a 429's body carries `retry_after_s: 1`; always `false` for any other status.
    retry_after_body_is_one: bool,
    /// Sum of the tile batch's `served` column — points_gathered per §7.2's `m(T)`, contract-equal
    /// to the points-stream row count (`tessera-wire::payload`'s module doc). `0` for any status
    /// other than 200 (a 429's body is a JSON error, never an Arrow tile stream).
    points_served: u64,
}

/// Decode just the tile stream's `served` column and sum it — new bench metric (points served per
/// second/user/request). Per the wire framing (`tessera-wire::payload`'s module doc): `u32 LE`
/// byte length of the tile stream, then the tile stream itself (an Arrow IPC stream, schema
/// `tile/visible/matched/served`, all `uint64`). The points stream and any subcell stream follow
/// but are never touched — the length prefix is exactly what makes that possible without parsing
/// Arrow metadata first, and `served` alone is sufficient (equal by contract to the points-stream
/// row count, so there is nothing the points stream itself would add).
///
/// Returns `0` on any malformed input (too short for the length prefix, length prefix past the
/// body's end, or an Arrow decode failure) rather than panicking — a load generator must never
/// crash the whole run over one malformed response; the caller's accounting simply undercounts
/// that one response's points, which a near-zero rate elsewhere in the cell would already flag.
fn sum_served(body: &[u8]) -> u64 {
    if body.len() < 4 {
        return 0;
    }
    let tile_len = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let Some(tile_bytes) = body.get(4..4 + tile_len) else {
        return 0;
    };
    let Ok(reader) = arrow::ipc::reader::StreamReader::try_new(tile_bytes, None) else {
        return 0;
    };
    let mut total = 0u64;
    for batch in reader {
        let Ok(batch) = batch else { return total };
        let Some(col) = batch.column_by_name("served") else {
            continue;
        };
        let Some(arr) = col.as_any().downcast_ref::<arrow::array::UInt64Array>() else {
            continue;
        };
        total += arr.values().iter().sum::<u64>();
    }
    total
}

async fn issue(
    client: &reqwest::Client,
    url: &str,
    target_healthz: bool,
    token: &str,
    k: usize,
    zoom: u8,
    bbox: [f64; 4],
) -> reqwest::Result<RawOutcome> {
    let resp = if target_healthz {
        client.get(url).send().await?
    } else {
        client
            .post(url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "slice": "s0", "zoom": zoom, "bbox": bbox, "k": k
            }))
            .send()
            .await?
    };
    let status = resp.status().as_u16();
    let server_us = resp
        .headers()
        .get("x-tessera-server-us")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // D-E: `Retry-After` is fixed at `1` on every `Backpressure` response — checked here rather
    // than left to trust, since a header present-but-wrong would otherwise look identical to a
    // header present-and-right in every other count this arm reports.
    let retry_after_header_is_one = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        == Some("1");
    let body = resp.bytes().await?;
    // The body is read in full either way (mean_bytes needs its length on the 200 path too), but
    // only ever handed to `serde_json::from_slice` for a 429 — a 200's multi-megabyte Arrow body
    // is never JSON-parsed, which would just fail immediately on non-JSON bytes for nothing.
    let retry_after_body_is_one = status == 429
        && serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("retry_after_s").and_then(serde_json::Value::as_u64))
            == Some(1);
    // Only a 200's body is a tile stream at all; decoding a 429's small JSON body through the
    // Arrow length-prefix path would just read garbage bytes as a "length" for nothing.
    let points_served = if status == 200 && !target_healthz {
        sum_served(&body)
    } else {
        0
    };
    Ok(RawOutcome {
        status,
        server_us,
        retry_after_header_is_one,
        body_len: body.len() as u64,
        retry_after_body_is_one,
        points_served,
    })
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    viewer_url: String,
    tokens: Vec<String>,
    plan: Vec<(u8, [f64; 4])>,
    concurrency: usize,
    duration: Duration,
    open_rate: Option<f64>,
    target_healthz: bool,
    k: usize,
    storm: StormOptions,
) -> Result<Vec<Sample>> {
    let client = reqwest::Client::builder()
        // One connection per virtual user: a shared, smaller pool would serialise users behind
        // each other inside the client and report the client's queueing as the server's latency.
        .pool_max_idle_per_host(concurrency.max(1))
        .timeout(Duration::from_secs(120))
        .build()?;

    let deadline = Instant::now() + duration;
    let issued = Arc::new(AtomicU64::new(0));
    let collected = Arc::new(tokio::sync::Mutex::new(Vec::<Sample>::new()));

    // Either the pan-storm's self-abort deadline or the hang-watchdog's hard deadline, never
    // both — `pan_storm` wins if somehow both are set, since that mode's deadline already bounds
    // every request and a second, looser one would just never fire. `is_abort` distinguishes
    // which outcome a firing deadline records.
    let watchdog: Option<(Duration, bool)> = if storm.pan_storm {
        Some((Duration::from_millis(storm.abort_after_ms), true))
    } else {
        storm
            .hang_timeout_ms
            .map(|ms| (Duration::from_millis(ms), false))
    };

    let mut handles = Vec::with_capacity(concurrency);
    for worker in 0..concurrency {
        let client = client.clone();
        let url = if target_healthz {
            format!("{viewer_url}/healthz")
        } else {
            format!("{viewer_url}/v1/viewport")
        };
        // The cold-build split (criterion 6): the first `cold_workers` workers hammer
        // `tokens[0]` (the shared cold key) on every request; the rest round-robin
        // `tokens[1..]` (the pre-warmed pool) — disjoint from the cold key by construction, so
        // "warm traffic on other keys" is a different token on every single request. With
        // `cold_workers == 0` this reduces to the original whole-list round robin.
        let (token, cold) = if storm.cold_workers > 0 {
            if worker < storm.cold_workers {
                (tokens[0].clone(), true)
            } else {
                let warm_len = tokens.len() - 1;
                (
                    tokens[1 + (worker - storm.cold_workers) % warm_len].clone(),
                    false,
                )
            }
        } else {
            (tokens[worker % tokens.len()].clone(), false)
        };
        let plan = plan.clone();
        let issued = Arc::clone(&issued);
        let collected = Arc::clone(&collected);

        handles.push(tokio::spawn(async move {
            let mut local = Vec::new();
            let mut i = worker;
            while Instant::now() < deadline {
                // Open loop: hold each worker to its share of the arrival schedule, so requests
                // launch on a clock rather than on completion. Closed loop: no pacing at all.
                if let Some(rate) = open_rate {
                    let n = issued.fetch_add(1, Ordering::Relaxed);
                    let due = Duration::from_secs_f64(n as f64 / rate);
                    let elapsed =
                        duration.saturating_sub(deadline.saturating_duration_since(Instant::now()));
                    if due > elapsed {
                        tokio::time::sleep(due - elapsed).await;
                    }
                }

                let (zoom, bbox) = plan[i % plan.len()];
                i += concurrency.max(1);

                let start = Instant::now();
                let outcome = issue(&client, &url, target_healthz, &token, k, zoom, bbox);

                let sample = match watchdog {
                    Some((dur, is_abort)) => match tokio::time::timeout(dur, outcome).await {
                        Ok(result) => from_result(result, &start, cold),
                        // The deadline fired first: dropping `outcome` here (it is not
                        // `.await`ed again) tears down the in-flight request — for pan-storm
                        // that IS the abort (D-C observes the disconnect and cancels
                        // server-side); for the hang watchdog it just stops waiting on a
                        // request that has already blown its bound.
                        Err(_) => Sample {
                            wall_ns: start.elapsed().as_nanos() as u64,
                            server_us: 0,
                            bytes: 0,
                            status: 0,
                            aborted: is_abort,
                            hung: !is_abort,
                            retry_after_ok: None,
                            cold,
                            points_served: 0,
                        },
                    },
                    None => from_result(outcome.await, &start, cold),
                };
                local.push(sample);
            }
            collected.lock().await.extend(local);
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }
    let samples = std::mem::take(&mut *collected.lock().await);
    Ok(samples)
}

/// Turn one request's `Result<RawOutcome, _>` into a [`Sample`]. Shared by the plain and
/// watchdog-wrapped paths in [`drive`] so the status/header/body-derived fields are computed in
/// exactly one place.
fn from_result(result: reqwest::Result<RawOutcome>, start: &Instant, cold: bool) -> Sample {
    let wall_ns = start.elapsed().as_nanos() as u64;
    match result {
        Ok(o) => Sample {
            wall_ns,
            server_us: o.server_us,
            bytes: o.body_len,
            status: o.status,
            aborted: false,
            hung: false,
            retry_after_ok: (o.status == 429)
                .then_some(o.retry_after_header_is_one && o.retry_after_body_is_one),
            cold,
            points_served: o.points_served,
        },
        Err(_) => Sample {
            wall_ns,
            server_us: 0,
            bytes: 0,
            status: 0,
            aborted: false,
            hung: false,
            retry_after_ok: None,
            cold,
            points_served: 0,
        },
    }
}
