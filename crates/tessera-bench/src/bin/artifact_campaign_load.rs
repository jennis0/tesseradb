//! The concurrency sweep's load generator, in Rust, because the Python one was the bottleneck.
//!
//! **Why this exists.** The campaign's first sweep was driven by a single Python process with one
//! thread per session. At eight and thirty-two sessions that process sat at 1.4–1.5 cores — the
//! GIL's ceiling — while the server sat at 1.4, and the throughput plateau it reported was the
//! harness's rather than the engine's. A measurement that edits what it measures has stopped being
//! one. This driver issues every request from a tokio runtime over one connection pool, reads each
//! body to completion and drops it without decoding, and samples the server's own `/proc` counters
//! so the report can say what fraction of the machine the server was using while the latencies
//! were being recorded.
//!
//! **What it does not do.** It never decodes an artifacts frame — the correctness arms in
//! `probes/2026-08-22-artifact-serving-e2e/` do that, over HTTP, against the generator's census.
//! A load generator that also checked answers would be paying for the check in the number it
//! reports.
//!
//! Sessions do not share masks: each is authorised on its own grant, which is the target scenario
//! the campaign was set (one shared bundle, many principals, each with its own `M_auth`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use clap::Parser;
use rand::{Rng, SeedableRng};

/// The generator's quantisation extent, which every viewport below is expressed in.
const GRID: f64 = 65536.0;

/// The seven zooms of the design's §7 grid, as `(name, fraction of the map, zoom)`.
const LADDER: [(&str, f64, u8); 7] = [
    ("100%", 1.0, 0),
    ("75%", 0.75, 1),
    ("50%", 0.5, 1),
    ("25%", 0.25, 2),
    ("6.25%", 0.0625, 3),
    ("0.39%", 0.003_906_2, 5),
    ("0.024%", 0.000_244_14, 7),
];

#[derive(Parser)]
#[command(about = "The artifact campaign's concurrent panning load")]
struct Args {
    #[arg(long)]
    viewer: String,
    #[arg(long)]
    session: String,
    #[arg(long)]
    session_credential: String,
    /// The `fixture.json` `artifact_campaign_fixture` wrote — the principal ladder's grants.
    #[arg(long)]
    fixture_json: std::path::PathBuf,
    /// The layer to ask for.
    #[arg(long)]
    layer: String,
    /// The breadth mix, `rung=count` repeated — e.g. `--mix 0.9375=2 --mix 0.25=6 --mix 0.031=24`.
    #[arg(long, value_parser = parse_mix)]
    mix: Vec<(String, usize)>,
    #[arg(long, default_value_t = 45.0)]
    seconds: f64,
    /// The server's pid, so its resident size and CPU can be sampled from `/proc` while the load
    /// runs — the two numbers that say whether a plateau is the machine or a queue.
    #[arg(long)]
    server_pid: Option<u32>,
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// Write every request as `start_seconds,latency_seconds,rung` — the per-request record the
    /// fold and ingest arms window against events of their own. Offsets are from this process's
    /// own start, which it prints as `load_started_unix` in the report so a caller can align.
    #[arg(long)]
    samples_out: Option<std::path::PathBuf>,
}

fn parse_mix(raw: &str) -> Result<(String, usize), String> {
    let (rung, count) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected `rung=count`, got '{raw}'"))?;
    Ok((
        rung.to_string(),
        count.parse().map_err(|e| format!("'{count}': {e}"))?,
    ))
}

/// `(utime + stime)` in seconds and resident bytes, for one pid.
fn proc_sample(pid: u32) -> Option<(f64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let ticks = 100.0; // `sysconf(_SC_CLK_TCK)` is 100 on every Linux this campaign runs on
    let cpu = (fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?) / ticks;
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some((cpu, resident * 4096))
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let index = ((q * sorted.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&args.fixture_json).expect("fixture.json"))
            .expect("fixture.json parses");
    let grants: std::collections::HashMap<String, String> = fixture["grants"]
        .as_array()
        .expect("the ladder")
        .iter()
        .map(|g| {
            (
                g["target"].as_str().map(str::to_string).unwrap_or_else(|| g["target"].to_string()),
                g["grant"].as_str().expect("a grant string").to_string(),
            )
        })
        .collect();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .pool_max_idle_per_host(512)
        .build()
        .expect("an http client");

    // ---- establish every session, and time it ------------------------------------------------
    let mut tokens: Vec<(String, String)> = Vec::new();
    let authorise_began = Instant::now();
    for (rung, count) in &args.mix {
        let grant = grants
            .get(rung)
            .unwrap_or_else(|| panic!("the ladder has no rung '{rung}'"));
        let terms: Vec<&str> = grant.split(',').collect();
        for _ in 0..*count {
            let payload = base64::engine::general_purpose::STANDARD
                .encode(serde_json::json!({ "terms": terms }).to_string());
            let response = client
                .post(format!("{}/session/authorise", args.session))
                .bearer_auth(&args.session_credential)
                .json(&serde_json::json!({ "auth_data": payload }))
                .send()
                .await
                .expect("authorise");
            assert_eq!(response.status().as_u16(), 200, "authorise refused");
            let body: serde_json::Value = response.json().await.expect("authorise body");
            tokens.push((
                rung.clone(),
                body["token"].as_str().expect("a token").to_string(),
            ));
        }
    }
    let authorise_seconds = authorise_began.elapsed().as_secs_f64();

    // ---- the panning load ---------------------------------------------------------------------
    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicU64::new(0));
    let before = args.server_pid.and_then(proc_sample);
    let started_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs_f64();
    let peak_rss = Arc::new(AtomicU64::new(before.map(|(_, r)| r).unwrap_or(0)));
    let began = Instant::now();

    let mut handles = Vec::new();
    for (index, (rung, token)) in tokens.into_iter().enumerate() {
        let client = client.clone();
        let viewer = args.viewer.clone();
        let layer = args.layer.clone();
        let stop = stop.clone();
        let errors = errors.clone();
        handles.push(tokio::spawn(async move {
            let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE + index as u64);
            // One session's pan route, drawn once: eight boxes at a mix of zooms, jittered across
            // the grid, so the walk is a pan rather than one warm entry answered over and over.
            let route: Vec<(f64, f64, f64, f64, u8)> = (0..8)
                .map(|_| {
                    let (_, fraction, zoom) = LADDER[rng.gen_range(0..LADDER.len())];
                    let side = GRID * fraction.sqrt();
                    let x = rng.gen_range(0.0..=(GRID - side).max(0.0));
                    let y = rng.gen_range(0.0..=(GRID - side).max(0.0));
                    (x, y, x + side, y + side, zoom)
                })
                .collect();
            let mut latencies: Vec<f64> = Vec::new();
            let mut starts: Vec<f64> = Vec::new();
            let mut bytes = 0u64;
            let mut step = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let (x0, y0, x1, y1, zoom) = route[step % route.len()];
                step += 1;
                let at = Instant::now();
                let offset = at.duration_since(began).as_secs_f64();
                let response = client
                    .post(format!("{viewer}/v1/viewport"))
                    .bearer_auth(&token)
                    .json(&serde_json::json!({
                        "view": "s0", "zoom": zoom, "bbox": [x0, y0, x1, y1],
                        "k": 0, "layers": [layer],
                    }))
                    .send()
                    .await;
                match response {
                    Ok(r) if r.status().as_u16() == 200 => match r.bytes().await {
                        Ok(body) => {
                            latencies.push(at.elapsed().as_secs_f64());
                            starts.push(offset);
                            bytes += body.len() as u64;
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    _ => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            (rung, latencies, starts, bytes)
        }));
    }

    // Sample the server while the load runs, rather than only at its ends.
    let sampler = {
        let stop = stop.clone();
        let peak_rss = peak_rss.clone();
        let pid = args.server_pid;
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if let Some(pid) = pid {
                    if let Some((_, rss)) = proc_sample(pid) {
                        peak_rss.fetch_max(rss, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    };

    // **SIGTERM ends the run cleanly rather than killing it**, so a caller whose own experiment
    // finishes early — the fold arm, which cannot know in advance how long a fold under load will
    // take — can stop the load and still get its per-request record. Without this the choice is
    // between a `--seconds` guessed too long (and a run that waits out the guess) and one guessed
    // too short (and windows with no requests in them, which is what the first fold run produced).
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler");
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs_f64(args.seconds)) => {}
        _ = terminate.recv() => {}
    }
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.await;

    let mut per_rung: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
    let mut all: Vec<f64> = Vec::new();
    let mut samples: Vec<(f64, f64, String)> = Vec::new();
    let mut total_bytes = 0u64;
    for handle in handles {
        let (rung, latencies, starts, bytes) = handle.await.expect("a session task");
        all.extend(latencies.iter().copied());
        for (start, latency) in starts.iter().zip(latencies.iter()) {
            samples.push((*start, *latency, rung.clone()));
        }
        per_rung.entry(rung).or_default().extend(latencies);
        total_bytes += bytes;
    }
    if let Some(path) = &args.samples_out {
        samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut text = String::from("start_s,latency_s,rung\n");
        for (start, latency, rung) in &samples {
            text.push_str(&format!("{start:.6},{latency:.6},{rung}\n"));
        }
        std::fs::write(path, text).expect("writing the sample record");
    }
    let elapsed = began.elapsed().as_secs_f64();
    let after = args.server_pid.and_then(proc_sample);
    let cpu = match (before, after) {
        (Some((b, _)), Some((a, _))) => a - b,
        _ => f64::NAN,
    };

    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let report = serde_json::json!({
        "layer": args.layer,
        "load_started_unix": started_unix,
        "sessions": args.mix.iter().map(|(_, c)| c).sum::<usize>(),
        "mix": args.mix.iter().map(|(r, c)| (r.clone(), *c)).collect::<std::collections::BTreeMap<_, _>>(),
        "seconds": elapsed,
        "authorise_seconds": authorise_seconds,
        "requests": all.len(),
        "throughput_rps": all.len() as f64 / elapsed,
        "bytes_per_second": total_bytes as f64 / elapsed,
        "p50_ms": percentile(&all, 0.5) * 1000.0,
        "p99_ms": percentile(&all, 0.99) * 1000.0,
        "max_ms": all.last().copied().unwrap_or(f64::NAN) * 1000.0,
        "server_cpu_seconds": cpu,
        "server_cores_busy": cpu / elapsed,
        "server_cpu_per_request": cpu / all.len().max(1) as f64,
        "server_peak_rss_bytes": peak_rss.load(Ordering::Relaxed),
        "errors": errors.load(Ordering::Relaxed),
        "per_breadth": per_rung.iter().map(|(rung, samples)| {
            let mut sorted = samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (rung.clone(), serde_json::json!({
                "requests": sorted.len(),
                "p50_ms": percentile(&sorted, 0.5) * 1000.0,
                "p99_ms": percentile(&sorted, 0.99) * 1000.0,
            }))
        }).collect::<std::collections::BTreeMap<_, _>>(),
    });
    let text = serde_json::to_string_pretty(&report).expect("the report serialises");
    if let Some(path) = &args.out {
        std::fs::write(path, &text).expect("writing the report");
    }
    println!("{text}");
}
