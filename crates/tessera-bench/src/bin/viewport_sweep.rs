//! Task 1 of the viewport/underlay plan: **what does one large viewport request cost?**
//!
//! The MVP client is tile-addressed — one `POST /v1/viewport` per deck.gl tile — and at 10⁹ that
//! shed 12 of 23 requests from a single tab. The proposed fix is one *viewport*-addressed request
//! per view, at a depth chosen to hit a global mark budget. Design §7.2's annotation gives the
//! arithmetic:
//!
//! ```text
//! marks(d) ≈ m_target · f · 4^d          f = fraction of the slice in view
//! tiles    = f · 4^d = B / m_target      INDEPENDENT of zoom and of f
//! ```
//!
//! So a 5 × 10⁴ budget at `m_target = 16` wants 3,125 tiles in *every* request. Nothing measured
//! before this exceeds one tile per request, so the design rests on an unknown number. This
//! harness produces it, plus the two other rulings the plan needs: whether cost tracks tile count
//! or visible count, and whether the `marks` prediction holds against reality.
//!
//! Measured against `Engine` directly rather than over HTTP — the question is about engine work,
//! and the transport would only add noise.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin viewport_sweep -- \
//!     --fixture data/bench-fixtures/1e8 --terms 0..200
//! ```

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;

/// Server defaults, so this measures a deployment somebody runs (see `arms::viewport`).
const K_MAX_MARKS: usize = 500;
const THETA_TARGET: u64 = 16;
const MAX_TILES_PER_REQUEST: usize = 262_144;

#[derive(Parser)]
#[command(about = "Cost of one viewport-addressed request, across depth, viewport fraction and scale")]
struct Args {
    /// Bundle root (the directory containing `CURRENT`).
    #[arg(long)]
    fixture: PathBuf,
    /// Candidate term range, `LO..HI`, matching the bench fixtures' numeric dictionary.
    #[arg(long, default_value = "0..200")]
    terms: String,
    /// Samples per cell; the median is reported.
    #[arg(long, default_value_t = 3)]
    samples: usize,
    /// Deepest depth to attempt. Cells exceeding `max_tiles_per_request` are recorded as refused.
    #[arg(long, default_value_t = 9)]
    max_depth: u8,
    /// Run the **concurrency** mode instead of the depth sweep: N simultaneous sessions issuing
    /// the same request, reporting per-request latency and aggregate throughput.
    ///
    /// This exists because the depth sweep measures a single client on an idle machine, and the
    /// problem the whole workstream addresses is a *throughput* gate (429 backpressure). Wall-clock
    /// falling with depth while CPU rises is only a win while there are spare cores; this mode is
    /// what says whether it is still a win when there are not.
    #[arg(long, value_delimiter = ',')]
    concurrency: Vec<usize>,
    /// Depths to compare in concurrency mode.
    #[arg(long, value_delimiter = ',', default_value = "0,4,6")]
    concurrency_depths: Vec<u8>,
    /// Requests per thread in concurrency mode.
    #[arg(long, default_value_t = 4)]
    iterations: usize,
}

fn parse_terms(spec: &str) -> Vec<String> {
    if let Some((lo, hi)) = spec.split_once("..") {
        let lo: u64 = lo.parse().expect("--terms LO..HI");
        let hi: u64 = hi.parse().expect("--terms LO..HI");
        (lo..=hi).map(|t| t.to_string()).collect()
    } else {
        spec.split(',').map(|s| s.trim().to_string()).collect()
    }
}

fn auth_json(terms: &[String]) -> String {
    let list = terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"terms\":[{list}]}}")
}

fn median(mut xs: Vec<u64>) -> u64 {
    xs.sort_unstable();
    xs[xs.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let bundle = open_bundle(&args.fixture)?;
    let q = bundle.manifest.quantisation;
    let slice_id = bundle
        .partitions
        .values()
        .next()
        .and_then(|p| p.slices.keys().next().cloned())
        .unwrap_or_else(|| "s0".to_string());
    drop(bundle);

    let tmp = std::env::temp_dir().join(format!("tessera-viewport-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    let engine = Engine::open(
        &args.fixture,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 5000,
            k_min: 2,
            k_max_marks: K_MAX_MARKS,
            theta_target_marks: THETA_TARGET,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: MAX_TILES_PER_REQUEST,
            compute_threads: tessera_engine::default_compute_threads(),
            pin_ttl_secs: 300,
            pins_per_session_max: 4,
        },
    )?;

    // ---- Pick principals by measured visible-set size, exactly as the client's preset script
    // does: a zoom-0 full-extent request's `visible` IS the principal's visible-set cardinality.
    let candidates = parse_terms(&args.terms);
    let full = [q.x_min, q.y_min, q.x_max, q.y_max];
    let mut measured: Vec<(String, u64)> = Vec::new();
    for term in &candidates {
        let terms = vec![term.clone()];
        let Ok(session) = engine.authorise(auth_json(&terms).as_bytes()) else {
            continue;
        };
        let out = engine.viewport(&session, ViewportRequest::new(&slice_id, 0, full, 1))?;
        let visible: u64 = out.tiles.iter().map(|t| t.visible).sum();
        if visible > 0 {
            measured.push((term.clone(), visible));
        }
    }
    measured.sort_by_key(|(_, v)| *v);
    if measured.is_empty() {
        return Err("no candidate term is visible to anyone".into());
    }
    let pick = |frac: f64| measured[((measured.len() as f64 * frac) as usize).min(measured.len() - 1)].clone();
    let principals: Vec<(String, Vec<String>)> = vec![
        ("narrow".to_string(), vec![pick(0.05).0]),
        ("medium".to_string(), vec![pick(0.5).0]),
        ("broad".to_string(), vec![measured[measured.len() - 1].0.clone()]),
        (
            "everything".to_string(),
            measured.iter().map(|(t, _)| t.clone()).collect(),
        ),
    ];

    // ---- Concurrency mode: does depth choice still win when the cores are busy?
    if !args.concurrency.is_empty() {
        let (_name, terms) = principals
            .iter()
            .find(|(n, _)| n == "everything")
            .expect("the everything principal");
        println!("depth,threads,requests,p50_ms,p95_ms,wall_s,requests_per_s,marks_per_s");
        for &depth in &args.concurrency_depths {
            for &threads in &args.concurrency {
                // One session per thread: distinct sessions are the realistic shape, and they
                // also keep the row-projection cache honest (each is warmed before timing).
                let sessions: Vec<_> = (0..threads)
                    .map(|_| {
                        let s = engine.authorise(auth_json(terms).as_bytes()).expect("authorise");
                        let _ = engine
                            .viewport(&s, ViewportRequest::new(&slice_id, 0, full, K_MAX_MARKS))
                            .expect("warm-up");
                        s
                    })
                    .collect();

                let started = Instant::now();
                let results: Vec<(Vec<u64>, u64)> = std::thread::scope(|scope| {
                    let handles: Vec<_> = sessions
                        .iter()
                        .map(|session| {
                            let engine = &engine;
                            let slice_id = &slice_id;
                            scope.spawn(move || {
                                let mut lat = Vec::with_capacity(args.iterations);
                                let mut marks = 0u64;
                                for _ in 0..args.iterations {
                                    let t0 = Instant::now();
                                    let out = engine
                                        .viewport(
                                            session,
                                            ViewportRequest::new(&slice_id, depth, full, K_MAX_MARKS),
                                        )
                                        .expect("viewport");
                                    lat.push(t0.elapsed().as_micros() as u64);
                                    marks += out.points.len() as u64;
                                }
                                (lat, marks)
                            })
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().expect("thread")).collect()
                });
                let wall = started.elapsed().as_secs_f64();

                let mut lat: Vec<u64> = results.iter().flat_map(|(l, _)| l.iter().copied()).collect();
                lat.sort_unstable();
                let marks: u64 = results.iter().map(|(_, m)| m).sum();
                let n = lat.len();
                println!(
                    "{depth},{threads},{n},{:.1},{:.1},{wall:.2},{:.1},{:.0}",
                    lat[n / 2] as f64 / 1000.0,
                    lat[(n * 95 / 100).min(n - 1)] as f64 / 1000.0,
                    n as f64 / wall,
                    marks as f64 / wall,
                );
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(());
    }

    println!(
        "principal,terms,visible_total,fraction,depth,tiles_resolved,tiles_nonempty,\
         sigma_visible,points_gathered,server_us,count_us,select_us,gather_us,refused"
    );

    for (name, terms) in &principals {
        let session = engine.authorise(auth_json(terms).as_bytes())?;

        // The row-projection cache fill must never land in a sample: the first viewport of a
        // session crosses entity space into row space over the whole fragment, and at 10⁹ that is
        // seconds. Every other harness in this crate excludes it; so does this one.
        let warm = engine.viewport(&session, ViewportRequest::new(&slice_id, 0, full, K_MAX_MARKS))?;
        let visible_total: u64 = warm.tiles.iter().map(|t| t.visible).sum();
        eprintln!(
            "{name}: {} terms, visible {visible_total}, warm-up {:.1} ms (projection_built={})",
            terms.len(),
            warm.timings.total_ns as f64 / 1e6,
            warm.timings.row_projection_built
        );

        // Three viewport fractions, so the "tiles = B / m_target, independent of f" claim is
        // checked rather than assumed. Each is a centred sub-extent covering `frac` of each axis,
        // i.e. `frac^2` of the area.
        for (label, frac) in [("1", 1.0_f64), ("1/4", 0.5), ("1/16", 0.25)] {
            let cx = (q.x_min + q.x_max) / 2.0;
            let cy = (q.y_min + q.y_max) / 2.0;
            let hw = (q.x_max - q.x_min) * frac / 2.0;
            let hh = (q.y_max - q.y_min) * frac / 2.0;
            let bbox = [cx - hw, cy - hh, cx + hw, cy + hh];

            for depth in 0..=args.max_depth {
                // Refuse rather than measure a request the server would reject.
                let tiles_predicted = (frac * frac * 4f64.powi(depth as i32)).ceil() as usize;
                if tiles_predicted > MAX_TILES_PER_REQUEST {
                    println!("{name},{},{visible_total},{label},{depth},,,,,,,,,refused-max-tiles", terms.len());
                    continue;
                }

                let mut totals = Vec::with_capacity(args.samples);
                let mut last = None;
                for _ in 0..args.samples {
                    let start = Instant::now();
                    let out = engine.viewport(
                        &session,
                        ViewportRequest::new(&slice_id, depth, bbox, K_MAX_MARKS),
                    )?;
                    totals.push(start.elapsed().as_micros() as u64);
                    last = Some(out);
                }
                let out = last.expect("at least one sample");
                let t = &out.timings;
                println!(
                    "{name},{},{visible_total},{label},{depth},{},{},{},{},{},{},{},{},",
                    terms.len(),
                    t.tiles_resolved,
                    t.tiles_nonempty,
                    t.sigma_visible,
                    t.points_gathered,
                    median(totals),
                    t.count_ns / 1000,
                    t.select_ns / 1000,
                    t.gather_ns / 1000,
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}
