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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::arms::{Context, Result};
use crate::report::Work;

/// One request's outcome.
struct Sample {
    wall_ns: u64,
    server_us: u64,
    bytes: u64,
    status: u16,
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
    ))?;

    if samples.is_empty() {
        return Err("no samples collected — did the server accept any request?".into());
    }

    let ok: Vec<&Sample> = samples.iter().filter(|s| s.status == 200).collect();
    let errors = samples.len() - ok.len();
    let wall: Vec<u64> = ok.iter().map(|s| s.wall_ns).collect();
    let server: Vec<u64> = ok.iter().map(|s| s.server_us * 1000).collect();
    let bytes: u64 = ok.iter().map(|s| s.bytes).sum();

    let throughput = ok.len() as f64 / duration_s;
    let mode = if open_rate.is_some() {
        "open"
    } else {
        "closed"
    };

    let mut flags = Vec::new();
    if errors > 0 {
        flags.push(format!("errors={errors}"));
    }
    if target_healthz {
        flags.push("generator_ceiling_calibration".to_string());
    }

    let cell_id = format!(
        "load/{}/{}/{}/{}/c{}",
        scale,
        label_set,
        if target_healthz {
            "healthz"
        } else {
            "viewport"
        },
        mode,
        concurrency
    );

    let work = Work {
        points_gathered: ok.len() as u64,
        bytes_touched: bytes,
        ..Default::default()
    };

    let server_timing = crate::report::Timing::from_samples(server);
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
            "requests_ok": ok.len(),
            "requests_error": errors,
            "throughput_rps": throughput,
            "mean_bytes": bytes as f64 / ok.len().max(1) as f64,
            "server_us_p50": server_timing.median_ns / 1000,
            "server_us_p99": server_timing.p99_ns / 1000,
            "server_us_max": server_timing.max_ns / 1000,
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

    let mut handles = Vec::with_capacity(concurrency);
    for worker in 0..concurrency {
        let client = client.clone();
        let url = if target_healthz {
            format!("{viewer_url}/healthz")
        } else {
            format!("{viewer_url}/v1/viewport")
        };
        let token = tokens[worker % tokens.len()].clone();
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
                let response = if target_healthz {
                    client.get(&url).send().await
                } else {
                    client
                        .post(&url)
                        .bearer_auth(&token)
                        .json(&serde_json::json!({
                            "slice": "s0", "zoom": zoom, "bbox": bbox, "k": k
                        }))
                        .send()
                        .await
                };

                match response {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let server_us = resp
                            .headers()
                            .get("x-tessera-server-us")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        let body = resp.bytes().await.map(|b| b.len() as u64).unwrap_or(0);
                        local.push(Sample {
                            wall_ns: start.elapsed().as_nanos() as u64,
                            server_us,
                            bytes: body,
                            status,
                        });
                    }
                    Err(_) => local.push(Sample {
                        wall_ns: start.elapsed().as_nanos() as u64,
                        server_us: 0,
                        bytes: 0,
                        status: 0,
                    }),
                }
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
