//! `mosaica-bench replay`: sends a log written by `[serve] request_log` back at a server, as one
//! viewer or as many, and reports each route's latency beside the latency the log recorded.
//!
//! A recorded session is a `token_id` within a run: each server process numbers its sessions from
//! 0 and marks its lines with a run of its own. Runs are replayed one after another, without the
//! time the server was down between them. Each copy of the log mints its own session for each one by
//! sending the recorded `/session/authorise` body again, and every request of that session carries
//! the new token. A session whose authorisation falls before the window is authorised when its
//! copy starts, before the copy's clock, and those requests are reported apart as `setup`.
//!
//! At recorded pace a request starts at its recorded offset whether or not the ones before it
//! have finished, as a browser's do. At `asap` each session's requests run one after another. A
//! request the log records as cancelled by its client is cancelled at the same elapsed time, so
//! the server sees the same cancellations.
//!
//! Not replayed: CORS preflights, viewer requests that carried no known session, sessions whose
//! authorisation is not in the log at all, and bodies the log did not keep. Each is counted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::arms::{Context, Result};
use crate::report::Timing;

/// When each request is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Pace {
    /// At its recorded offset from the start of the window, divided by `speed`.
    Recorded,
    /// As soon as the session's previous request has finished.
    Asap,
}

pub struct Options {
    pub log: PathBuf,
    pub viewer_url: String,
    pub session_url: String,
    /// The session plane's credential, the operator credential or an API key holding
    /// `authorise-as`, read from the environment by the caller.
    pub credential: String,
    pub viewers: usize,
    pub stagger: Duration,
    pub pace: Pace,
    pub speed: f64,
    pub timeout: Duration,
    /// Seconds from the log's first line.
    pub since_s: Option<f64>,
    pub until_s: Option<f64>,
}

/// One line of the request log.
#[derive(Debug, Clone, Deserialize)]
struct Logged {
    run: String,
    start_us: u64,
    plane: String,
    method: String,
    path: String,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    body_bytes: u64,
    token_id: Option<u64>,
    status: Option<u16>,
    headers_us: Option<u64>,
    end_us: u64,
    #[serde(default)]
    bytes: u64,
    outcome: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Authorise,
    Revoke,
    Viewer,
}

struct Planned {
    line: Logged,
    /// Microseconds from the first request in the window.
    offset_us: u64,
    kind: Kind,
    /// The recorded session whose fresh session this request needs or, for an authorise, mints.
    session: Option<SessionKey>,
    route: String,
}

/// A recorded session: the run's index in the log, in order of its first line, and the `token_id`.
type SessionKey = (usize, u64);

pub(crate) struct Plan {
    requests: Vec<Planned>,
    /// Each recorded session's `/session/authorise` body, from the whole log.
    authorise_bodies: HashMap<SessionKey, Value>,
    /// Sessions the window uses but does not authorise.
    setup: Vec<SessionKey>,
    /// Lines not replayed, by reason.
    skipped: BTreeMap<&'static str, usize>,
}

/// The route a path is reported under: the method and the path with its parameters named. The
/// patterns follow the routes `mosaica_server::viewer::router` declares, and change with them.
fn route_template(method: &str, path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let template = match segments.as_slice() {
        ["v1", "categories", _] => "/v1/categories/{column}",
        ["v1", "categories", _, "suggest"] => "/v1/categories/{column}/suggest",
        ["v1", "items", _] => "/v1/items/{mosaica_id}",
        ["v1", "artifacts", "browse"] => "/v1/artifacts/browse",
        ["v1", "artifacts", _] => "/v1/artifacts/{mosaica_id}",
        _ => path,
    };
    format!("{method} {template}")
}

pub(crate) fn plan(text: &str, since_s: Option<f64>, until_s: Option<f64>) -> Result<Plan> {
    let mut lines = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let line: Logged = serde_json::from_str(raw)
            .map_err(|e| format!("line {} of the log is not a request log line: {e}", n + 1))?;
        lines.push(line);
    }
    lines.sort_by_key(|l| l.start_us);
    let Some(first_us) = lines.first().map(|l| l.start_us) else {
        return Err("the log holds no requests".into());
    };

    let mut runs: HashMap<String, usize> = HashMap::new();
    for line in &lines {
        let next = runs.len();
        runs.entry(line.run.clone()).or_insert(next);
    }
    let key = |line: &Logged| line.token_id.map(|id| (runs[&line.run], id));

    let mut authorise_bodies = HashMap::new();
    for line in &lines {
        if line.plane == "session" && line.path.starts_with("/session/authorise") {
            if let (Some(session), Some(body)) = (key(line), &line.body) {
                authorise_bodies.insert(session, body.clone());
            }
        }
    }

    let since_us = since_s.map_or(0, |s| (s * 1e6) as u64);
    let until_us = until_s.map_or(u64::MAX, |s| (s * 1e6) as u64);
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut requests = Vec::new();
    for line in lines {
        let at = line.start_us - first_us;
        if at < since_us || at >= until_us {
            continue;
        }
        if line.method == "OPTIONS" {
            *skipped.entry("cors preflight").or_default() += 1;
            continue;
        }
        if line.body.is_none() && line.body_bytes > 0 {
            *skipped.entry("body not in the log").or_default() += 1;
            continue;
        }
        let kind = match (line.plane.as_str(), line.path.split('?').next()) {
            ("session", Some("/session/authorise")) => Kind::Authorise,
            ("session", Some("/session/revoke")) => Kind::Revoke,
            ("viewer", _) => Kind::Viewer,
            _ => {
                *skipped.entry("not a viewer or session route").or_default() += 1;
                continue;
            }
        };
        let session = key(&line);
        if kind != Kind::Authorise {
            match session {
                None => {
                    *skipped.entry("no session").or_default() += 1;
                    continue;
                }
                Some(id) if !authorise_bodies.contains_key(&id) => {
                    *skipped
                        .entry("session authorised before the log")
                        .or_default() += 1;
                    continue;
                }
                Some(_) => {}
            }
        }
        requests.push(Planned {
            offset_us: 0,
            route: route_template(&line.method, &line.path),
            kind,
            session,
            line,
        });
    }

    // Offsets from the window's first request, with each run starting where the one before it
    // stopped: the end of its last-ending request.
    let mut base_us = 0u64;
    let mut current: Option<(&str, u64, u64)> = None; // run, its first start, its last end
    let mut offsets = Vec::with_capacity(requests.len());
    for p in &requests {
        let (start, end) = (p.line.start_us, p.line.start_us + p.line.end_us);
        match &mut current {
            Some((run, _, last)) if *run == p.line.run => *last = (*last).max(end),
            _ => {
                if let Some((_, first, last)) = current {
                    base_us += last - first;
                }
                current = Some((&p.line.run, start, end));
            }
        }
        let (_, first, _) = current.expect("set above");
        offsets.push(base_us + (start - first));
    }
    for (p, offset_us) in requests.iter_mut().zip(offsets) {
        p.offset_us = offset_us;
    }

    let authorised: HashSet<SessionKey> = requests
        .iter()
        .filter(|p| p.kind == Kind::Authorise)
        .filter_map(|p| p.session)
        .collect();
    let mut setup: Vec<SessionKey> = requests
        .iter()
        .filter_map(|p| p.session)
        .filter(|id| !authorised.contains(id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    setup.sort_unstable();

    Ok(Plan {
        requests,
        authorise_bodies,
        setup,
        skipped,
    })
}

/// A session minted for one copy.
#[derive(Clone)]
struct Fresh {
    token: String,
    token_id: u64,
}

/// Pending, failed, or minted.
type SessionSlot = watch::Sender<Option<Option<Fresh>>>;

struct Copy {
    copy: usize,
    sessions: HashMap<SessionKey, SessionSlot>,
}

/// What one replayed request did.
struct Sent {
    status: Option<u16>,
    headers_us: Option<u64>,
    end_us: u64,
    bytes: u64,
    outcome: &'static str,
    body: Vec<u8>,
    pin: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn send(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    bearer: &str,
    body: Option<&Value>,
    keep_body: bool,
    cancel_after: Option<Duration>,
) -> Sent {
    let started = Instant::now();
    let mut sent = Sent {
        status: None,
        headers_us: None,
        end_us: 0,
        bytes: 0,
        outcome: "ok",
        body: Vec::new(),
        pin: None,
    };
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut request = client.request(method, url).bearer_auth(bearer);
    if let Some(body) = body {
        request = request.json(body);
    }
    let exchange = async {
        let mut resp = request.send().await?;
        sent.status = Some(resp.status().as_u16());
        sent.headers_us = Some(started.elapsed().as_micros() as u64);
        sent.pin = resp
            .headers()
            .get("x-mosaica-pin")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        while let Some(chunk) = resp.chunk().await? {
            sent.bytes += chunk.len() as u64;
            if keep_body {
                sent.body.extend_from_slice(&chunk);
            }
        }
        Ok::<(), reqwest::Error>(())
    };
    let finished = match cancel_after {
        Some(after) => tokio::time::timeout(after, exchange).await.ok(),
        None => Some(exchange.await),
    };
    sent.end_us = started.elapsed().as_micros() as u64;
    sent.outcome = match finished {
        None => "cancelled",
        Some(Err(e)) if e.is_timeout() => "timeout",
        Some(Err(_)) => "transport_error",
        Some(Ok(())) => match sent.status {
            Some(s) if (200..300).contains(&s) => "ok",
            _ => "error_status",
        },
    };
    sent
}

/// Sends `/session/authorise` with `body` and records the session it mints, or its failure, in
/// `slot`.
async fn authorise(
    client: &reqwest::Client,
    opts: &Options,
    body: &Value,
    slot: Option<&SessionSlot>,
    cancel_after: Option<Duration>,
) -> Sent {
    let url = format!("{}/session/authorise", opts.session_url);
    let sent = send(
        client,
        "POST",
        &url,
        &opts.credential,
        Some(body),
        true,
        cancel_after,
    )
    .await;
    if let Some(slot) = slot {
        let fresh = (sent.outcome == "ok")
            .then(|| serde_json::from_slice::<Value>(&sent.body).ok())
            .flatten()
            .and_then(|v| {
                Some(Fresh {
                    token: v["token"].as_str()?.to_string(),
                    token_id: v["token_id"].as_u64()?,
                })
            });
        slot.send_replace(Some(fresh));
    }
    sent
}

/// One output line. Never holds a token.
fn line(copy: usize, p: Option<&Planned>, route: &str, sent: &Sent, wait_us: u64) -> Value {
    json!({
        "copy": copy,
        "run": p.map(|p| p.line.run.as_str()),
        "session": p.and_then(|p| p.session).map(|(_, token_id)| token_id),
        "route": route,
        "path": p.map(|p| p.line.path.as_str()),
        "offset_us": p.map(|p| p.offset_us),
        "recorded": p.map(|p| json!({
            "status": p.line.status,
            "headers_us": p.line.headers_us,
            "end_us": p.line.end_us,
            "bytes": p.line.bytes,
            "outcome": p.line.outcome,
        })),
        "status": sent.status,
        "headers_us": sent.headers_us,
        "end_us": sent.end_us,
        "bytes": sent.bytes,
        "outcome": sent.outcome,
        "session_wait_us": wait_us,
        "pin": sent.pin,
    })
}

/// Replays one planned request in `copy`.
async fn replay_one(client: &reqwest::Client, opts: &Options, copy: &Copy, p: &Planned) -> Value {
    let scale = match opts.pace {
        Pace::Recorded => opts.speed,
        Pace::Asap => 1.0,
    };
    let cancel_after = (p.line.outcome == "cancelled")
        .then(|| Duration::from_secs_f64(p.line.end_us as f64 / 1e6 / scale));

    if p.kind == Kind::Authorise {
        let body = p.line.body.clone().unwrap_or(Value::Null);
        let slot = p.session.and_then(|id| copy.sessions.get(&id));
        let sent = authorise(client, opts, &body, slot, cancel_after).await;
        return line(copy.copy, Some(p), &p.route, &sent, 0);
    }

    let waited = Instant::now();
    let fresh = match p.session.and_then(|id| copy.sessions.get(&id)) {
        Some(slot) => {
            let mut rx = slot.subscribe();
            let waited = tokio::time::timeout(opts.timeout, async {
                rx.wait_for(Option::is_some)
                    .await
                    .ok()
                    .and_then(|value| value.clone().flatten())
            })
            .await;
            waited.ok().flatten()
        }
        None => None,
    };
    let wait_us = waited.elapsed().as_micros() as u64;
    let Some(fresh) = fresh else {
        let sent = Sent {
            status: None,
            headers_us: None,
            end_us: 0,
            bytes: 0,
            outcome: "no_session",
            body: Vec::new(),
            pin: None,
        };
        return line(copy.copy, Some(p), &p.route, &sent, wait_us);
    };

    let sent = match p.kind {
        Kind::Revoke => {
            let mut body = p.line.body.clone().unwrap_or_else(|| json!({}));
            body["token_id"] = json!(fresh.token_id);
            let url = format!("{}{}", opts.session_url, p.line.path);
            send(
                client,
                "POST",
                &url,
                &opts.credential,
                Some(&body),
                false,
                cancel_after,
            )
            .await
        }
        _ => {
            let url = format!("{}{}", opts.viewer_url, p.line.path);
            send(
                client,
                &p.line.method,
                &url,
                &fresh.token,
                p.line.body.as_ref(),
                false,
                cancel_after,
            )
            .await
        }
    };
    line(copy.copy, Some(p), &p.route, &sent, wait_us)
}

/// Replays every copy of `plan` and returns one line per request sent.
pub(crate) async fn replay(plan: Arc<Plan>, opts: Arc<Options>) -> Result<Vec<Value>> {
    let users = plan.requests.len().max(1) * opts.viewers.max(1);
    let client = crate::arms::load::http_client(users, opts.timeout)?;
    let started = Instant::now();
    let mut copies = tokio::task::JoinSet::new();
    for copy in 0..opts.viewers.max(1) {
        let (plan, opts, client) = (Arc::clone(&plan), Arc::clone(&opts), client.clone());
        let at = started + opts.stagger.mul_f64(copy as f64);
        copies.spawn(async move { run_copy(copy, plan, opts, client, at).await });
    }
    let mut out = Vec::new();
    while let Some(lines) = copies.join_next().await {
        out.extend(lines?);
    }
    Ok(out)
}

async fn run_copy(
    copy: usize,
    plan: Arc<Plan>,
    opts: Arc<Options>,
    client: reqwest::Client,
    at: Instant,
) -> Vec<Value> {
    tokio::time::sleep_until(at.into()).await;
    let sessions: HashMap<SessionKey, SessionSlot> = plan
        .requests
        .iter()
        .filter_map(|p| p.session)
        .map(|id| (id, watch::channel(None).0))
        .collect();
    let copy_state = Arc::new(Copy { copy, sessions });
    let mut out = Vec::new();

    // Sessions authorised before the window, before this copy's clock starts.
    let mut setup = tokio::task::JoinSet::new();
    for &id in &plan.setup {
        let (client, opts, copy_state) =
            (client.clone(), Arc::clone(&opts), Arc::clone(&copy_state));
        let body = plan.authorise_bodies[&id].clone();
        setup.spawn(async move {
            let sent = authorise(&client, &opts, &body, copy_state.sessions.get(&id), None).await;
            let mut line = line(copy, None, "POST /session/authorise", &sent, 0);
            line["session"] = json!(id.1);
            line["setup"] = json!(true);
            line
        });
    }
    while let Some(line) = setup.join_next().await {
        if let Ok(line) = line {
            out.push(line);
        }
    }

    let base = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    match opts.pace {
        Pace::Recorded => {
            for index in 0..plan.requests.len() {
                let (client, opts, plan, copy_state) = (
                    client.clone(),
                    Arc::clone(&opts),
                    Arc::clone(&plan),
                    Arc::clone(&copy_state),
                );
                tasks.spawn(async move {
                    let p = &plan.requests[index];
                    let due = Duration::from_secs_f64(p.offset_us as f64 / 1e6 / opts.speed);
                    tokio::time::sleep_until((base + due).into()).await;
                    vec![replay_one(&client, &opts, &copy_state, p).await]
                });
            }
        }
        Pace::Asap => {
            let mut by_session: BTreeMap<Option<SessionKey>, Vec<usize>> = BTreeMap::new();
            for (index, p) in plan.requests.iter().enumerate() {
                by_session.entry(p.session).or_default().push(index);
            }
            for (_, indices) in by_session {
                let (client, opts, plan, copy_state) = (
                    client.clone(),
                    Arc::clone(&opts),
                    Arc::clone(&plan),
                    Arc::clone(&copy_state),
                );
                tasks.spawn(async move {
                    let mut lines = Vec::with_capacity(indices.len());
                    for index in indices {
                        lines.push(
                            replay_one(&client, &opts, &copy_state, &plan.requests[index]).await,
                        );
                    }
                    lines
                });
            }
        }
    }
    while let Some(lines) = tasks.join_next().await {
        if let Ok(lines) = lines {
            out.extend(lines);
        }
    }
    out
}

/// One route's row of the summary.
struct RouteSummary {
    route: String,
    count: usize,
    ok: usize,
    errors: usize,
    timeouts: usize,
    cancelled: usize,
    /// To the end of the body as the replaying client saw it, network included, over replayed
    /// requests whose body ended.
    timing: Option<Timing>,
    /// The log's own medians over the requests it records as completed: measured by the server,
    /// from the request reaching it to its headers and to the end of its body.
    recorded_headers_p50_us: Option<u64>,
    recorded_p50_us: Option<u64>,
}

fn summarise(plan: &Plan, lines: &[Value]) -> Vec<RouteSummary> {
    let mut by_route: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for line in lines {
        let mut route = line["route"].as_str().unwrap_or("?").to_string();
        if line["setup"] == true {
            route.push_str(" (setup)");
        }
        by_route.entry(route).or_default().push(line);
    }
    let mut recorded: BTreeMap<&str, (Vec<u64>, Vec<u64>)> = BTreeMap::new();
    for p in &plan.requests {
        if p.line.outcome == "completed" {
            let (headers, ends) = recorded.entry(&p.route).or_default();
            headers.extend(p.line.headers_us.map(|us| us * 1000));
            ends.push(p.line.end_us * 1000);
        }
    }
    let median_us =
        |ns: &Vec<u64>| (!ns.is_empty()).then(|| Timing::from_samples(ns.clone()).median_ns / 1000);
    by_route
        .into_iter()
        .map(|(route, lines)| {
            let outcome = |o: &str| lines.iter().filter(|l| l["outcome"] == o).count();
            let ended: Vec<u64> = lines
                .iter()
                .filter(|l| l["outcome"] == "ok" || l["outcome"] == "error_status")
                .filter_map(|l| l["end_us"].as_u64())
                .map(|us| us * 1000)
                .collect();
            RouteSummary {
                count: lines.len(),
                ok: outcome("ok"),
                errors: outcome("error_status")
                    + outcome("transport_error")
                    + outcome("no_session"),
                timeouts: outcome("timeout"),
                cancelled: outcome("cancelled"),
                timing: (!ended.is_empty()).then(|| Timing::from_samples(ended)),
                recorded_headers_p50_us: recorded
                    .get(route.as_str())
                    .and_then(|(headers, _)| median_us(headers)),
                recorded_p50_us: recorded
                    .get(route.as_str())
                    .and_then(|(_, ends)| median_us(ends)),
                route,
            }
        })
        .collect()
}

/// The table on stdout. The replayed columns are the client's times to the end of the body,
/// network included; the recorded columns are the server's own, from the log.
fn print_summary(rows: &[RouteSummary]) {
    let ms = |ns: u64| format!("{:.1}", ns as f64 / 1e6);
    let us = |us: Option<u64>| us.map_or("-".to_string(), |us| format!("{:.1}", us as f64 / 1e3));
    println!(
        "{:<44} {:>6} {:>6} {:>5} {:>5} {:>5} | {:^39} | {:^21}",
        "", "", "", "", "", "", "replayed, client end of body (ms)", "recorded, server (ms)"
    );
    println!(
        "{:<44} {:>6} {:>6} {:>5} {:>5} {:>5} | {:>9} {:>9} {:>9} {:>9} | {:>10} {:>10}",
        "route", "n", "ok", "err", "t/o", "cxl", "p50", "p95", "p99", "max", "hdr p50", "end p50"
    );
    for row in rows {
        let (p50, p95, p99, max) = match &row.timing {
            Some(t) => (ms(t.median_ns), ms(t.p95_ns), ms(t.p99_ns), ms(t.max_ns)),
            None => ("-".into(), "-".into(), "-".into(), "-".into()),
        };
        println!(
            "{:<44} {:>6} {:>6} {:>5} {:>5} {:>5} | {:>9} {:>9} {:>9} {:>9} | {:>10} {:>10}",
            row.route,
            row.count,
            row.ok,
            row.errors,
            row.timeouts,
            row.cancelled,
            p50,
            p95,
            p99,
            max,
            us(row.recorded_headers_p50_us),
            us(row.recorded_p50_us),
        );
    }
}

pub fn run(ctx: &Context, opts: Options) -> Result<()> {
    if !(opts.speed > 0.0 && opts.speed.is_finite()) {
        return Err(format!(
            "--speed {} cannot pace a replay; give a factor above 0",
            opts.speed
        )
        .into());
    }
    if opts.viewers == 0 {
        return Err("--viewers 0 replays nothing; give 1 or more".into());
    }
    let text = std::fs::read_to_string(&opts.log)
        .map_err(|e| format!("cannot read the log {}: {e}", opts.log.display()))?;
    let plan = Arc::new(plan(&text, opts.since_s, opts.until_s)?);
    if plan.requests.is_empty() {
        return Err("no request in the window can be replayed".into());
    }
    let run_id = crate::arms::run_id();
    let env = crate::arms::capture_env();
    std::fs::create_dir_all(&ctx.run_dir)?;
    let lines_path = ctx.run_dir.join(format!("replay-{run_id}.jsonl"));
    let summary_path = ctx.run_dir.join(format!("replay-{run_id}.summary.json"));

    let args = json!({
        "log": opts.log,
        "viewer_url": opts.viewer_url,
        "session_url": opts.session_url,
        "viewers": opts.viewers,
        "stagger_s": opts.stagger.as_secs_f64(),
        "pace": format!("{:?}", opts.pace).to_lowercase(),
        "speed": opts.speed,
        "timeout_s": opts.timeout.as_secs_f64(),
        "since_s": opts.since_s,
        "until_s": opts.until_s,
    });
    eprintln!(
        "replaying {} requests from {} sessions, {} cop{}; skipped {:?}",
        plan.requests.len(),
        plan.authorise_bodies.len(),
        opts.viewers,
        if opts.viewers == 1 { "y" } else { "ies" },
        plan.skipped
    );

    let opts = Arc::new(opts);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let wall = Instant::now();
    let lines = runtime.block_on(replay(Arc::clone(&plan), Arc::clone(&opts)))?;
    let wall_s = wall.elapsed().as_secs_f64();

    let mut text = String::new();
    for line in &lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    std::fs::write(&lines_path, text)?;

    let rows = summarise(&plan, &lines);
    let pin = lines.iter().find_map(|l| l["pin"].as_str());
    let summary = json!({
        "run_id": run_id,
        "git_sha": env.git_sha,
        "dirty": env.dirty,
        "profile": env.profile,
        "args": args,
        "pin": pin,
        "wall_s": wall_s,
        "requests_planned": plan.requests.len(),
        "requests_sent": lines.len(),
        "skipped": plan.skipped,
        "routes": rows.iter().map(|r| json!({
            "route": r.route,
            "count": r.count,
            "ok": r.ok,
            "errors": r.errors,
            "timeouts": r.timeouts,
            "cancelled": r.cancelled,
            "end_us_p50": r.timing.as_ref().map(|t| t.median_ns / 1000),
            "end_us_p95": r.timing.as_ref().map(|t| t.p95_ns / 1000),
            "end_us_p99": r.timing.as_ref().map(|t| t.p99_ns / 1000),
            "end_us_max": r.timing.as_ref().map(|t| t.max_ns / 1000),
            "recorded_headers_us_p50": r.recorded_headers_p50_us,
            "recorded_end_us_p50": r.recorded_p50_us,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;

    print_summary(&rows);
    println!(
        "{} requests in {wall_s:.1} s; lines in {}, summary in {}",
        lines.len(),
        lines_path.display(),
        summary_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use arrow::array::{Float64Array, UInt32Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use serde_json::{json, Value};

    use super::{plan, replay, Options, Pace};

    const CREDENTIAL: &str = "replay-operator-secret";
    const ROWS: u64 = 200;

    fn write_parquet(path: &Path, columns: Vec<(&str, DataType, arrow::array::ArrayRef)>) {
        let schema = Arc::new(Schema::new(
            columns
                .iter()
                .map(|(name, t, _)| Field::new(*name, t.clone(), false))
                .collect::<Vec<_>>(),
        ));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            columns.into_iter().map(|c| c.2).collect(),
        )
        .unwrap();
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    /// A deployment over `ROWS` points all carrying term 0, logging requests to `requests.jsonl`.
    /// One request computes at a time and a queued one waits a minute, so the test can hold the
    /// gate.
    fn deployment(dir: &Path) -> std::path::PathBuf {
        let ids: Vec<u64> = (0..ROWS).collect();
        let points = dir.join("points.parquet");
        let pairs = dir.join("pairs.parquet");
        write_parquet(
            &points,
            vec![
                (
                    "entity_id",
                    DataType::UInt64,
                    Arc::new(UInt64Array::from(ids.clone())),
                ),
                (
                    "x",
                    DataType::Float64,
                    Arc::new(Float64Array::from_iter_values(
                        ids.iter().map(|e| (e * 37 % 1000) as f64),
                    )),
                ),
                (
                    "y",
                    DataType::Float64,
                    Arc::new(Float64Array::from_iter_values(
                        ids.iter().map(|e| (e * 53 % 1000) as f64),
                    )),
                ),
            ],
        );
        write_parquet(
            &pairs,
            vec![
                (
                    "entity_id",
                    DataType::UInt64,
                    Arc::new(UInt64Array::from(ids)),
                ),
                (
                    "term_id",
                    DataType::UInt32,
                    Arc::new(UInt32Array::from(vec![0u32; ROWS as usize])),
                ),
            ],
        );
        let schema_path = dir.join("schema.toml");
        // Items are named by a unique `id` read from `entity_id`, which the pairs file names them by.
        std::fs::write(
            &schema_path,
            "[[attribute]]\nname = \"id\"\ntype = \"u64\"\nunique = true\nfield = \"entity_id\"\n",
        )
        .unwrap();
        let schema = mosaica_build::config::Config::parse(&schema_path, &Default::default())
            .unwrap()
            .schema;
        let bundle = dir.join("bundle");
        mosaica_build::build(&mosaica_build::BuildArgs {
            views: vec![mosaica_build::ViewArgs {
                visibility: None,
                view_id: "s0".to_string(),
                projection: mosaica_spatial::Projection::None,
                extent: mosaica_spatial::Bounds {
                    x_min: 0.0,
                    x_max: 1000.0,
                    y_min: 0.0,
                    y_max: 1000.0,
                },
                points: points.clone(),
                point_fields: Default::default(),
                select: None,
                access: mosaica_build::config::AccessInput::relation(pairs),
            }],
            anchor: 0,
            groups: Vec::new(),
            scoped_attributes: Vec::new(),
            attribute_sources: mosaica_build::config::AttributeSource::over(points, &schema),
            out: bundle.clone(),
            limit: None,
            strict: false,
            identity_key: mosaica_types::IdentityKey::from_hex("07070707070707070707070707070707")
                .unwrap(),
            shard_id: 0,
            layers: Vec::new(),
            layer_inputs: Vec::new(),
            scoped_layers: Default::default(),
            emit_oracle_pairs: false,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema,
        })
        .unwrap();
        std::fs::write(dir.join("operator.cred"), CREDENTIAL).unwrap();
        let toml = dir.join("mosaica.toml");
        std::fs::write(
            &toml,
            r#"
            [bundle]
            path = "bundle"
            cache = "cache"
            wal = "wal.log"
            [disclosure]
            token_max_lifetime = 3600
            [serve]
            viewer = "127.0.0.1:0"
            session = "127.0.0.1:0"
            control = "127.0.0.1:0"
            operator_credential_file = "operator.cred"
            request_log = "requests.jsonl"
            compute_admission = 1
            compute_queue = 4
            admission_timeout_ms = 60000
            [catalogue]
            dir = "catalogue"
            "#,
        )
        .unwrap();
        toml
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: not within a minute"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn log_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn options(viewer: &str, session: &str, viewers: usize, pace: Pace) -> Arc<Options> {
        Arc::new(Options {
            log: Default::default(),
            viewer_url: viewer.to_string(),
            session_url: session.to_string(),
            credential: CREDENTIAL.to_string(),
            viewers,
            stagger: Duration::ZERO,
            pace,
            speed: 1.0,
            timeout: Duration::from_secs(30),
            since_s: None,
            until_s: None,
        })
    }

    /// A server prepared from `deployment` as `mosaica serve` prepares one, its viewer and session
    /// routers served on tasks the test can stop.
    struct Running {
        state: Arc<mosaica_server::state::AppState>,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        viewer: String,
        session: String,
    }

    async fn start(deployment: &Path) -> Running {
        let state = mosaica_server::prepare(deployment).unwrap().state;
        let mut urls = Vec::new();
        let mut tasks = Vec::new();
        for router in [
            mosaica_server::viewer::router(Arc::clone(&state)),
            mosaica_server::session::router(Arc::clone(&state)),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            urls.push(format!("http://{}", listener.local_addr().unwrap()));
            tasks.push(tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            }));
        }
        Running {
            state,
            tasks,
            viewer: urls[0].clone(),
            session: urls[1].clone(),
        }
    }

    impl Running {
        /// Stops serving and waits for the state, and with it the log's writer, to be dropped.
        /// Callers drop their clients first, so no kept-alive connection holds the state.
        async fn stop(self) {
            for task in &self.tasks {
                task.abort();
            }
            for task in self.tasks {
                let _ = task.await;
            }
            let state = self.state;
            wait_until("the state to be released", || Arc::strong_count(&state) == 1).await;
        }
    }

    async fn authorise(client: &reqwest::Client, session: &str, term: &str) -> Value {
        client
            .post(format!("{session}/session/authorise"))
            .bearer_auth(CREDENTIAL)
            .json(&json!({ "terms": [term] }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Two server runs both number their sessions from 0. Each run's requests replay on the
    /// session minted for that run's principal, and the second run follows the first without the
    /// time the server was down.
    #[tokio::test(flavor = "multi_thread")]
    async fn each_run_in_a_log_replays_on_sessions_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = deployment(tmp.path());
        let log = tmp.path().join("requests.jsonl");
        let viewport = json!({"view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200});
        // Term 0 sees every point and term 1 none, so the two principals' viewports differ.
        for term in ["0", "1"] {
            let server = start(&toml).await;
            let client = reqwest::Client::new();
            let auth = authorise(&client, &server.session, term).await;
            assert_eq!(auth["token_id"], 0, "each run numbers its sessions from 0");
            client
                .post(format!("{}/v1/viewport", server.viewer))
                .bearer_auth(auth["token"].as_str().unwrap())
                .json(&viewport)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            drop(client);
            server.stop().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let recorded = log_lines(&log);
        assert_eq!(recorded.len(), 4);
        assert_ne!(recorded[0]["run"], recorded[2]["run"]);
        // A response's trailer varies by a few bytes from one request to the next; the two
        // principals' responses differ by far more.
        let bytes = |line: &Value| line["bytes"].as_u64().unwrap() as i64;
        const SLACK: i64 = 32;
        assert!(
            (bytes(&recorded[1]) - bytes(&recorded[3])).abs() > 4 * SLACK,
            "the two principals must be told apart by what they are served: {} and {}",
            bytes(&recorded[1]),
            bytes(&recorded[3])
        );

        let server = start(&toml).await;
        let planned = Arc::new(plan(&std::fs::read_to_string(&log).unwrap(), None, None).unwrap());
        let lines = replay(
            planned,
            options(&server.viewer, &server.session, 1, Pace::Recorded),
        )
        .await
        .unwrap();
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(line["outcome"], "ok", "{line}");
            assert!(
                (bytes(line) - bytes(&line["recorded"])).abs() <= SLACK,
                "served as the recorded principal was: {line}"
            );
        }
        let second = lines
            .iter()
            .find(|l| l["route"] == "POST /session/authorise" && l["run"] == recorded[2]["run"])
            .unwrap();
        let downtime_us =
            recorded[2]["start_us"].as_u64().unwrap() - recorded[1]["start_us"].as_u64().unwrap();
        assert!(
            second["offset_us"].as_u64().unwrap() + 2_000_000 <= downtime_us,
            "the second run starts where the first stopped, not after the downtime: {second}"
        );
        server.stop().await;
    }

    /// A session recorded against a server replays against it as two viewers, each on sessions of
    /// its own; and a request the log records as cancelled is cancelled at the recorded time, so
    /// the server logs it cancelled again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recorded_session_replays_with_fresh_sessions_and_its_cancellations() {
        let tmp = tempfile::tempdir().unwrap();
        let server = start(&deployment(tmp.path())).await;
        let state = &server.state;
        let (viewer, session) = (server.viewer.clone(), server.session.clone());
        let log = tmp.path().join("requests.jsonl");

        // Record: authorise, meta, a viewport, a category the bundle lacks, and a revoke.
        let client = reqwest::Client::new();
        let auth = authorise(&client, &session, "0").await;
        let token = auth["token"].as_str().unwrap();
        let viewport = json!({"view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5});
        for request in [
            client.get(format!("{viewer}/v1/meta")),
            client.post(format!("{viewer}/v1/viewport")).json(&viewport),
            client.get(format!("{viewer}/v1/categories/absent")),
        ] {
            request
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
        }
        let revoked = client
            .post(format!("{session}/session/revoke"))
            .bearer_auth(CREDENTIAL)
            .json(&json!({ "token_id": auth["token_id"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status(), 204);
        wait_until("the recorded lines", || log_lines(&log).len() == 5).await;
        let recorded = std::fs::read_to_string(&log).unwrap();

        // Two viewers through the subcommand's own entry point, which builds its own runtime.
        let run_dir = tmp.path().join("runs");
        let ctx = crate::arms::Context {
            run_dir: run_dir.clone(),
            repeat: 1,
            fixtures: Vec::new(),
        };
        let mut two = Arc::try_unwrap(options(&viewer, &session, 2, Pace::Asap))
            .ok()
            .unwrap();
        two.log = log.clone();
        std::thread::spawn(move || super::run(&ctx, two).map_err(|e| e.to_string()))
            .join()
            .unwrap()
            .unwrap();
        let written = |suffix: &str| {
            let entry = std::fs::read_dir(&run_dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.to_string_lossy().ends_with(suffix))
                .unwrap();
            std::fs::read_to_string(entry).unwrap()
        };
        let lines: Vec<Value> = written(".jsonl")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 10);
        for line in &lines {
            // The recorded token was revoked before the replay, so a viewer request answers as it
            // did only on a session the replay minted.
            assert_eq!(line["status"], line["recorded"]["status"], "{line}");
        }
        assert!(!written(".jsonl").contains(token));
        assert!(
            state.sessions.lock().list(|_| true).is_empty(),
            "each copy's revoke names the session that copy minted"
        );
        let summary: Value = serde_json::from_str(&written(".summary.json")).unwrap();
        let routes: Vec<String> = summary["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{} {} {}",
                    r["route"].as_str().unwrap(),
                    r["count"],
                    r["errors"]
                )
            })
            .collect();
        assert_eq!(
            routes,
            [
                "GET /v1/categories/{column} 2 2",
                "GET /v1/meta 2 0",
                "POST /session/authorise 2 0",
                "POST /session/revoke 2 0",
                "POST /v1/viewport 2 0",
            ]
        );
        assert!(
            summary["pin"].is_string(),
            "the viewport's generation is stamped"
        );

        // The authorise line, then a viewport three seconds later that its client gave up on
        // after 300 ms.
        let mut abandoned: Value = serde_json::from_str(recorded.lines().nth(2).unwrap()).unwrap();
        let authorised: Value = serde_json::from_str(recorded.lines().next().unwrap()).unwrap();
        abandoned["start_us"] = json!(authorised["start_us"].as_u64().unwrap() + 3_000_000);
        abandoned["outcome"] = json!("cancelled");
        abandoned["end_us"] = json!(300_000);
        abandoned["status"] = Value::Null;
        let window = format!("{authorised}\n{abandoned}\n");
        let before = log_lines(&log).len();

        let window_plan = Arc::new(plan(&window, None, None).unwrap());
        let window_options = options(&viewer, &session, 1, Pace::Recorded);
        let replaying = tokio::spawn(async move {
            replay(window_plan, window_options)
                .await
                .map_err(|e| e.to_string())
        });
        wait_until("the replayed session", || {
            state.sessions.lock().list(|_| true).len() == 1
        })
        .await;
        let held = state.compute_gate.admit().await.unwrap();
        let lines = replaying.await.unwrap().unwrap();
        drop(held);
        let viewport_line = lines
            .iter()
            .find(|l| l["route"] == "POST /v1/viewport")
            .unwrap();
        assert_eq!(viewport_line["outcome"], "cancelled", "{viewport_line}");
        wait_until("the server logging the cancellation", || {
            log_lines(&log)[before..]
                .iter()
                .any(|l| l["path"] == "/v1/viewport" && l["outcome"] == "cancelled")
        })
        .await;
    }
}
