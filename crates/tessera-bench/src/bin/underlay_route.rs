//! **How does the density underlay scale toward full resolution?**
//!
//! The density underlay is capped at `max_underlay_offset = 4` — 16 × 16 sub-cells per tile, i.e.
//! 32-pixel blocks under individually-placed marks. Client-interaction §9's annotation records the
//! owner's verdict ("waaaay too low resolution") and the two things that break before full
//! resolution (offset 9, 512 × 512): the per-sub-cell evaluation, and the sparse
//! `(cell, count)` wire encoding.
//!
//! This harness measures the **first** of those, and it measures it rather than modelling it,
//! because the extrapolation in the annotation (~0.5 s/tile at offset 9) was arithmetic on a
//! single data point.
//!
//! Both limits it would otherwise hit — `max_underlay_offset` and `max_underlay_cells` — are
//! plain `EngineConfig` fields, so the existing route can be driven to full resolution through the
//! public API with no engine change. That is deliberate: it answers "is today's algorithm
//! affordable at full resolution?" before anyone writes a second one.
//!
//! What it reports per cell:
//!   * `underlay_us` — the §3.3 loop's own time, separated from the rest of the request.
//!   * `cells_evaluated` vs `cells_emitted` — the gap is work spent discovering emptiness, which
//!     on a clustered corpus is most of it, and is exactly what a single-pass route would avoid.
//!   * `us_per_cell_evaluated` — the figure that says whether the cost is per-cell or per-item.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin underlay_route -- \
//!     --fixture data/bench-fixtures/1e8 --terms 0..200
//! ```

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;

const K_MAX_MARKS: usize = 500;
const THETA_TARGET: u64 = 16;

#[derive(Parser)]
#[command(about = "Underlay cost scaling toward full resolution")]
struct Args {
    #[arg(long)]
    fixture: PathBuf,
    #[arg(long, default_value = "0..200")]
    terms: String,
    #[arg(long, default_value_t = 3)]
    samples: usize,
    /// Deepest sub-cell offset to attempt. `zoom + offset` may not exceed the depth-16 grid.
    #[arg(long, default_value_t = 9)]
    max_offset: u8,
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
    // The frame is the view's (decision 0040); a built bundle declares one, and this harness
    // measures against it.
    let q = bundle
        .manifest
        .views
        .first()
        .expect("a built bundle declares a view")
        .quantisation;
    let view_id = bundle
        .partitions
        .values()
        .next()
        .and_then(|p| p.views.keys().next().cloned())
        .unwrap_or_else(|| "s0".to_string());
    drop(bundle);

    let tmp = std::env::temp_dir().join(format!("tessera-underlay-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    // The two guards this measurement exists to look past. Raised here and ONLY here: the point is
    // to learn what the current route costs beyond where a deployment would allow it, so the
    // decision to move them is taken on numbers.
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
            max_underlay_offset: 16,
            max_underlay_cells: usize::MAX,
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

    let candidates = parse_terms(&args.terms);
    let full = [q.x_min, q.y_min, q.x_max, q.y_max];
    let mut measured: Vec<(String, u64)> = Vec::new();
    for term in &candidates {
        let terms = vec![term.clone()];
        let Ok(session) = engine.authorise(auth_json(&terms).as_bytes()) else {
            continue;
        };
        let out = engine.viewport(&session, ViewportRequest::new(&view_id, 0, full, 1))?;
        let visible: u64 = out.tiles.iter().map(|t| t.visible).sum();
        if visible > 0 {
            measured.push((term.clone(), visible));
        }
    }
    measured.sort_by_key(|(_, v)| *v);
    if measured.is_empty() {
        return Err("no candidate term is visible to anyone".into());
    }
    let mid = measured[measured.len() / 2].0.clone();
    let principals: Vec<(String, Vec<String>)> = vec![
        ("medium".to_string(), vec![mid]),
        (
            "everything".to_string(),
            measured.iter().map(|(t, _)| t.clone()).collect(),
        ),
    ];

    println!(
        "principal,visible_total,zoom,tiles,offset,cells_per_tile,cells_evaluated,cells_emitted,\
         underlay_us,total_us,us_per_cell_evaluated"
    );

    for (name, terms) in &principals {
        let session = engine.authorise(auth_json(terms).as_bytes())?;
        // Warm the row projection outside every sample (see `viewport_sweep`).
        let warm = engine.viewport(
            &session,
            ViewportRequest::new(&view_id, 0, full, K_MAX_MARKS),
        )?;
        let visible_total: u64 = warm.tiles.iter().map(|t| t.visible).sum();
        eprintln!(
            "{name}: visible {visible_total}, warm-up {:.1} ms",
            warm.timings.total_ns as f64 / 1e6
        );

        // A single tile at a few depths, so cost-per-tile is the unit and the tile count does not
        // confound the offset scaling. Depth 6 is the depth the mark-budget work wants.
        for zoom in [2u8, 4, 6] {
            let span = (q.x_max - q.x_min) / f64::from(1u32 << zoom);
            // A centred tile, biased to where the data actually is.
            let tx = (1u32 << zoom) / 2;
            let bbox = [
                q.x_min + f64::from(tx) * span + span * 0.01,
                q.y_min + f64::from(tx) * span + span * 0.01,
                q.x_min + f64::from(tx) * span + span * 0.99,
                q.y_min + f64::from(tx) * span + span * 0.99,
            ];

            for offset in 0..=args.max_offset {
                if zoom + offset > 16 {
                    continue;
                }
                let cells_per_tile = 1u64 << (2 * u32::from(offset));

                let mut totals = Vec::with_capacity(args.samples);
                let mut last = None;
                for _ in 0..args.samples {
                    let start = Instant::now();
                    let out = engine.viewport(
                        &session,
                        ViewportRequest::new(&view_id, zoom, bbox, K_MAX_MARKS)
                            .underlay_offset(if offset == 0 { None } else { Some(offset) }),
                    )?;
                    totals.push(start.elapsed().as_micros() as u64);
                    last = Some(out);
                }
                let out = last.expect("at least one sample");
                let t = &out.timings;
                let evaluated = t.underlay_cells_evaluated;
                let underlay_us = t.underlay_ns / 1000;
                let per_cell = if evaluated > 0 {
                    format!("{:.4}", underlay_us as f64 / evaluated as f64)
                } else {
                    String::new()
                };
                println!(
                    "{name},{visible_total},{zoom},{},{offset},{cells_per_tile},{evaluated},{},{underlay_us},{},{per_cell}",
                    t.tiles_resolved,
                    out.sub_cells.len(),
                    median(totals),
                );
            }
        }
    }

    // ---- The operating point the mark-budget work actually creates.
    //
    // The per-tile sweep above measures one tile in isolation, which is the wrong unit for the
    // decision: "full resolution" is a property of the VIEWPORT, not of a tile. A view showing
    // `T` tiles over a ~10⁶-pixel screen needs ~10⁶/T sub-cells per tile, so the offset that
    // matters falls as the tile count rises. This section measures the whole-viewport request at
    // the depths the budget selects, which is the number a resolution increase turns on.
    println!();
    println!(
        "principal,visible_total,depth,tiles,offset,cells_per_tile,total_cells_evaluated,\
         total_cells_emitted,underlay_us,total_us"
    );
    for (name, terms) in &principals {
        let session = engine.authorise(auth_json(terms).as_bytes())?;
        let _warm = engine.viewport(
            &session,
            ViewportRequest::new(&view_id, 0, full, K_MAX_MARKS),
        )?;

        for depth in [5u8, 6, 7] {
            for offset in [0u8, 2, 3, 4, 5] {
                if depth + offset > 16 {
                    continue;
                }
                let mut totals = Vec::with_capacity(args.samples);
                let mut last = None;
                for _ in 0..args.samples {
                    let start = Instant::now();
                    let out = engine.viewport(
                        &session,
                        ViewportRequest::new(&view_id, depth, full, K_MAX_MARKS)
                            .underlay_offset(if offset == 0 { None } else { Some(offset) }),
                    )?;
                    totals.push(start.elapsed().as_micros() as u64);
                    last = Some(out);
                }
                let out = last.expect("at least one sample");
                let t = &out.timings;
                println!(
                    "{name},{},{depth},{},{offset},{},{},{},{},{}",
                    visible_for(&out),
                    t.tiles_resolved,
                    1u64 << (2 * u32::from(offset)),
                    t.underlay_cells_evaluated,
                    out.sub_cells.len(),
                    t.underlay_ns / 1000,
                    median(totals),
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// Summed masked visible over a response's tiles.
fn visible_for(out: &tessera_engine::viewport::ViewportOut) -> u64 {
    out.tiles.iter().map(|t| t.visible).sum()
}
