//! **What does the *built* row-space filter route cost per viewport row, and what does it cost
//! when the viewport is the corpus?**
//!
//! `records-and-search.md` §6.2 quotes 0.48–0.73 ns per viewport row and a 7–1,269× advantage over
//! the entity route, and marks both as describing *the approach, not shipped code* (review N4).
//! The probe that produced them wrote its own row-space scan over a single segment of a single
//! slice; the built route pays a segment boundary and a `ScalarSlice` match on top, and — the
//! thing no probe figure can carry — runs `scan_rows` under rayon, split over the same domain the
//! tile sweep splits. §11 item 6 owes both residuals: the built route's constants against the
//! probe's, and the coarse-zoom whole-slice scan **under the sweep's real parallelism**.
//!
//! # How the constant is obtained without touching the engine
//!
//! `evaluate_row_route` is private, and a measurement task that edits what it measures has stopped
//! being one. It does not need to be reached directly: the stage timings already separate it.
//! `filter_eval_ns` ends at `evaluate_routed`, which for a row-routed tree only builds the routed
//! tree; the scan itself lands in `filter_cross_ns`, and `rows_in_ranges` is the domain it walked.
//! So for a **pure row leaf** — one category predicate, no entity-space sub-tree, hence no
//! crossing to confound it — `filter_cross_ns / rows_in_ranges` is the built route's per-row cost
//! and nothing else.
//!
//! That is why every filter here is a single leaf. A tree with an entity verdict in it would put
//! the crossing in the same counter and the division would silently measure two things.
//!
//! # The three cells
//!
//! - **`viewport`** — the per-row constant across viewport sizes, both code widths (`archive` is
//!   `u8`, `primary_category` is `u16`) and two selectivities. This is what §6.2's 0.48–0.73 ns is
//!   to be read against.
//! - **`coarse`** — zoom 0 over the whole extent: the domain *is* the slice, which is §6.2's
//!   "coarse-zoom cell" and the residual's subject. Run at one thread and at every core, so the
//!   parallel speedup is measured rather than assumed.
//! - **`routes`** — the same request answered both ways, on a fixture whose categories carry
//!   `index = true` as well, so 0068's rule has something to choose between. A render-only
//!   category has exactly one placement and cannot be timed against the entity route at all.
//!
//! **Scale is the honest limit here and it is stated rather than worked around.** The largest
//! bundle that can carry a rendered category is the real arXiv corpus, 2,422,486 items:
//! `data/scaled/attrs/points.parquet` is the only attributed points file that exists, and
//! `probes/build_attributes.py` refuses to fabricate a tail above it. The probe's constants were
//! measured at 10⁸. A per-row constant is the quantity least disturbed by that gap — the probe's
//! own finding is that R-dense is invariant in corpus size — but the *coarse* cell is a whole-slice
//! scan, so its milliseconds are this scale's and any 10⁹ figure derived from them is modelled.
//! Nothing here reports one as measured.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin row_route_cost -- \
//!     --fixture /tmp/tessera-bench/fixtures/2422486/attrs-subclass \
//!     [--both /tmp/tessera-bench/fixtures/2422486/attrs-both] [--repeat 5]
//! ```

use std::path::{Path, PathBuf};

use tessera_engine::filter::{FilterExpr, FilterOperand};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;
use tessera_types::AttrLocalId;

/// The server's own defaults, so a cell measures a deployment somebody would run.
const K_MAX_MARKS: usize = 500;
const THETA_TARGET: u64 = 16;
const MAX_TILES_PER_REQUEST: usize = 262_144;

struct Fixture {
    slice: String,
    extent: [f64; 4],
    /// `(column, key, code)` per category column, in manifest order.
    categories: Vec<(String, String, u32)>,
    items: u64,
}

/// Read the slice id, the quantisation extent and every category column's vocabulary straight off
/// the manifest — a filter leaf names a *code*, and the manifest is where the binding lives.
fn inspect(root: &Path) -> Result<Fixture, Box<dyn std::error::Error>> {
    let bundle = open_bundle(root)?;
    let q = bundle.manifest.quantisation;
    let slice = bundle
        .partitions
        .values()
        .next()
        .and_then(|p| p.slices.keys().next().cloned())
        .unwrap_or_else(|| "s0".to_string());
    let mut categories = Vec::new();
    for scalar in &bundle.manifest.declared_scalars {
        let Some(vocab_name) = &scalar.vocabulary else {
            continue;
        };
        let Some(vocab) = bundle
            .manifest
            .vocabularies
            .iter()
            .find(|v| &v.name == vocab_name)
        else {
            continue;
        };
        // The most common value is not knowable from the manifest, so this takes a mid-vocabulary
        // key and reports the match rate it actually got. A cell's selectivity is measured and
        // printed, never assumed from the key's name.
        for pick in [0usize, vocab.values.len() / 2] {
            if let Some(v) = vocab.values.get(pick) {
                categories.push((scalar.name.clone(), v.key.clone(), v.code));
            }
        }
    }
    let items = bundle.manifest.entity_id_high_water;
    Ok(Fixture {
        slice,
        extent: [q.x_min, q.y_min, q.x_max, q.y_max],
        categories,
        items,
    })
}

fn open_engine(root: &Path, threads: usize, tag: &str) -> Result<Engine, Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join(format!(
        "tessera-row-route-{}-{tag}-{threads}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    Ok(Engine::open(
        root,
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
            compute_threads: threads,
            flush_max_age_secs: 90,
            max_merged_segment_bytes: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )?)
}

/// Every term in the dictionary — the widest principal the bundle admits, which is what puts
/// `rows_in_ranges <= |M_auth|` on the row route's side at zoom 0 (0068's rule).
fn all_terms(root: &Path, prefix: &str) -> std::io::Result<Vec<String>> {
    let data = std::fs::read(root.join(prefix).join("dictionary/terms-0.dict"))?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().expect("4 bytes")) as usize;
        offset += 4;
        if offset + len > data.len() {
            break;
        }
        out.push(String::from_utf8_lossy(&data[offset..offset + len]).into_owned());
        offset += len;
    }
    Ok(out)
}

fn current_prefix(root: &Path) -> std::io::Result<String> {
    let raw = std::fs::read(root.join("CURRENT"))?;
    let v: serde_json::Value = serde_json::from_slice(&raw)?;
    Ok(v.get("prefix")
        .and_then(|p| p.as_str())
        .unwrap_or("v00000")
        .to_string())
}

fn auth_json(terms: &[String]) -> String {
    let list = terms
        .iter()
        .map(|t| format!("{t:?}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"terms\":[{list}]}}")
}

/// A bbox covering `frac` of each extent axis about the centre.
fn centred(extent: [f64; 4], frac: f64) -> [f64; 4] {
    let cx = (extent[0] + extent[2]) / 2.0;
    let cy = (extent[1] + extent[3]) / 2.0;
    let hw = (extent[2] - extent[0]) * frac / 2.0;
    let hh = (extent[3] - extent[1]) * frac / 2.0;
    [cx - hw, cy - hh, cx + hw, cy + hh]
}

/// One request repeated, keeping the **minimum** — the probes' convention, and the right statistic
/// for a constant: the spread here is scheduler noise, not workload variance.
struct Cell {
    cross_ns: u64,
    eval_ns: u64,
    total_ns: u64,
    rows_in_ranges: u64,
    matched: u64,
    row_routed: bool,
}

fn measure(
    engine: &Engine,
    session: &tessera_engine::Session,
    slice: &str,
    zoom: u8,
    bbox: [f64; 4],
    expr: &FilterExpr,
    repeat: usize,
) -> Result<Cell, Box<dyn std::error::Error>> {
    // The row-projection cache fill crosses entity space into row space over the whole fragment
    // and belongs in no sample; every harness here excludes it.
    let _ = engine.viewport(
        session,
        ViewportRequest::new(slice, zoom, bbox, 200).filter(expr.clone()),
    )?;
    let before = engine.filter_row_routes();
    let mut best: Option<Cell> = None;
    for _ in 0..repeat {
        let out = engine.viewport(
            session,
            ViewportRequest::new(slice, zoom, bbox, 200).filter(expr.clone()),
        )?;
        let t = out.timings;
        let cell = Cell {
            cross_ns: t.filter_cross_ns,
            eval_ns: t.filter_eval_ns,
            total_ns: t.total_ns,
            rows_in_ranges: t.rows_in_ranges,
            matched: t.filter_matched,
            row_routed: false,
        };
        best = Some(match best {
            None => cell,
            Some(b) if cell.cross_ns < b.cross_ns => cell,
            Some(b) => b,
        });
    }
    let mut cell = best.expect("at least one repetition");
    cell.row_routed = engine.filter_row_routes() > before;
    Ok(cell)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut fixture: Option<PathBuf> = None;
    let mut both: Option<PathBuf> = None;
    let mut repeat = 5usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fixture" => fixture = args.next().map(PathBuf::from),
            "--both" => both = args.next().map(PathBuf::from),
            "--repeat" => repeat = args.next().and_then(|v| v.parse().ok()).unwrap_or(repeat),
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    let Some(fixture_root) = fixture else {
        return Err("--fixture <bundle root> is required".into());
    };
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — every number below is meaningless. Use --release.");
    }
    if !cfg!(feature = "bench-timing") {
        return Err("built without `bench-timing`: filter_cross_ns would be zero and this bin \
                    measures nothing else"
            .into());
    }

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let fx = inspect(&fixture_root)?;
    let prefix = current_prefix(&fixture_root)?;
    let terms = all_terms(&fixture_root, &prefix)?;
    println!(
        "fixture {} — {} items, slice {}, {} terms, {} cores\n",
        fixture_root.display(),
        fx.items,
        fx.slice,
        terms.len(),
        cores
    );

    // ---- Cell 1: the per-row constant across viewport sizes and code widths.
    println!("== viewport: the built route's per-row cost ==");
    println!(
        "{:<18} {:>6} {:>12} {:>10} {:>8} {:>11} {:>11} {:>10}",
        "column", "thr", "rows", "matched", "sel %", "cross ms", "ns/row", "total ms"
    );
    for threads in [1usize, cores] {
        let engine = open_engine(&fixture_root, threads, "viewport")?;
        let session = engine.authorise(auth_json(&terms).as_bytes())?;
        for (column, key, code) in &fx.categories {
            let expr = FilterExpr::Leaf {
                column: column.clone(),
                operand: FilterOperand::Equals(AttrLocalId::new(*code)),
            };
            for frac in [0.02f64, 0.1, 0.35, 1.0] {
                let bbox = centred(fx.extent, frac);
                let cell = measure(&engine, &session, &fx.slice, 6, bbox, &expr, repeat)?;
                if cell.rows_in_ranges == 0 {
                    continue;
                }
                println!(
                    "{:<18} {:>6} {:>12} {:>10} {:>8.3} {:>11.3} {:>11.3} {:>10.3}{}",
                    format!("{column}={key}"),
                    threads,
                    cell.rows_in_ranges,
                    cell.matched,
                    100.0 * cell.matched as f64 / cell.rows_in_ranges as f64,
                    cell.cross_ns as f64 / 1e6,
                    cell.cross_ns as f64 / cell.rows_in_ranges as f64,
                    cell.total_ns as f64 / 1e6,
                    if cell.row_routed { "" } else { "  [ENTITY ROUTE]" },
                );
            }
        }
    }

    // ---- Cell 2: the coarse-zoom whole-slice scan, at one thread and at every core.
    println!("\n== coarse: zoom 0, the whole extent — the domain is the slice ==");
    println!(
        "{:<18} {:>6} {:>12} {:>10} {:>11} {:>11} {:>10}",
        "column", "thr", "rows", "matched", "cross ms", "ns/row", "total ms"
    );
    for threads in [1usize, cores] {
        let engine = open_engine(&fixture_root, threads, "coarse")?;
        let session = engine.authorise(auth_json(&terms).as_bytes())?;
        for (column, key, code) in &fx.categories {
            let expr = FilterExpr::Leaf {
                column: column.clone(),
                operand: FilterOperand::Equals(AttrLocalId::new(*code)),
            };
            let cell = measure(&engine, &session, &fx.slice, 0, fx.extent, &expr, repeat)?;
            println!(
                "{:<18} {:>6} {:>12} {:>10} {:>11.3} {:>11.3} {:>10.3}{}",
                format!("{column}={key}"),
                threads,
                cell.rows_in_ranges,
                cell.matched,
                cell.cross_ns as f64 / 1e6,
                cell.cross_ns as f64 / cell.rows_in_ranges.max(1) as f64,
                cell.total_ns as f64 / 1e6,
                if cell.row_routed { "" } else { "  [ENTITY ROUTE]" },
            );
        }
    }

    // ---- Cell 3: both routes, same request, on a fixture that affords both.
    let Some(both_root) = both else {
        println!("\n(no --both fixture given; the entity/row comparison is skipped)");
        return Ok(());
    };
    let bx = inspect(&both_root)?;
    let bprefix = current_prefix(&both_root)?;
    let bterms = all_terms(&both_root, &bprefix)?;
    println!(
        "\n== routes: the same predicate answered both ways ({}) ==",
        both_root.display()
    );
    // **The two routes cannot be timed on one request, and that is a property of the built
    // system rather than of this harness.** 0068's rule is `rows_in_ranges <= |M_auth|` — it is a
    // function of exactly the two quantities that would have to be held fixed to compare, so
    // forcing the other route would mean an engine override, which is outside a measurement
    // task's business. What *can* be measured is the shape each route's cost has, over one
    // principal, as the viewport sweeps across the crossover:
    //
    // - the **entity** route scans the whole authorised set and then crosses the result, so its
    //   cost should be flat in viewport size. Whether it is flat is the thing to check, not to
    //   assume: if it is, the entity figure measured past the crossover is the same figure a
    //   smaller viewport would have paid, and the ratio at any viewport size follows.
    // - the **row** route's cost should be proportional to `rows_in_ranges`.
    //
    // The ratio column is therefore honest only insofar as the flatness holds, and the flatness
    // is printed rather than asserted.
    println!(
        "{:<18} {:>6} {:>12} {:>10} {:>10} {:>10} {:>10} {:>7}",
        "column", "thr", "rows", "matched", "eval ms", "cross ms", "total ms", "route"
    );
    for threads in [1usize, cores] {
        let engine = open_engine(&both_root, threads, "routes")?;
        // One principal, mid-width: wide enough that a small viewport falls on the row side of
        // the rule and a full-extent one does not. Its terms are the middle of the dictionary by
        // position, and its measured `rows_in_ranges` at each fraction is printed, so a reader can
        // see where the crossover actually fell rather than trusting the intent.
        let mid: Vec<String> = bterms
            .iter()
            .skip(bterms.len() / 4)
            .take(bterms.len() / 8)
            .cloned()
            .collect();
        let session = engine.authorise(auth_json(&mid).as_bytes())?;
        for (column, key, code) in &bx.categories {
            let expr = FilterExpr::Leaf {
                column: column.clone(),
                operand: FilterOperand::Equals(AttrLocalId::new(*code)),
            };
            for frac in [0.02f64, 0.1, 0.35, 1.0] {
                let bbox = centred(bx.extent, frac);
                let zoom = if frac >= 1.0 { 0 } else { 6 };
                let cell = measure(&engine, &session, &bx.slice, zoom, bbox, &expr, repeat)?;
                if cell.rows_in_ranges == 0 {
                    continue;
                }
                println!(
                    "{:<18} {:>6} {:>12} {:>10} {:>10.3} {:>10.3} {:>10.3} {:>7}",
                    format!("{column}={key}"),
                    threads,
                    cell.rows_in_ranges,
                    cell.matched,
                    cell.eval_ns as f64 / 1e6,
                    cell.cross_ns as f64 / 1e6,
                    cell.total_ns as f64 / 1e6,
                    if cell.row_routed { "row" } else { "entity" },
                );
            }
        }
    }
    println!(
        "\nns/row divides `filter_cross_ns` by `rows_in_ranges`: on a pure row leaf that stage is\n\
         `evaluate_row_route` and nothing else. In the routes block the two columns mean different\n\
         things per route — on the entity route `eval ms` is the scan and `cross ms` the crossing,\n\
         on the row route `eval ms` is only the routing and `cross ms` is the scan — so read\n\
         `eval + cross` there, and the two blocks are not one table."
    );
    Ok(())
}
