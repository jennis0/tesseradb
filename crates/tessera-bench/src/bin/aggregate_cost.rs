//! **What does `POST /v1/aggregate` cost?**
//!
//! Times `Engine::aggregate_stream`, every page of every table read to the end, for a table with
//! no level, value groupings, density surfaces, value by cell and an artifact grouping, each with
//! no filter and under a polygon, beside the viewport request over the same set at zoom 4 and the
//! whole extent. The principal holds every term in the bundle's dictionary. The engine's pool is
//! `--threads` wide.
//!
//! Every figure is the median of `--repeat` runs, with the fastest and slowest beside it, after one
//! run that is not counted. Each case also reports its last run's time by stage: composing the
//! sets, counting groups, counting cells (of which walking the rows is `pass`), and building the
//! pages' batches.
//!
//! ```text
//! cargo build --release -p tessera-bench --bin aggregate_cost
//! systemd-run --user --scope --collect -p MemoryMax=16G -p MemorySwapMax=2G -- \
//!     target/release/aggregate_cost \
//!     --bundle data/ladder/geonames/bundle-final --view world --threads 12 \
//!     --polygon '0.4722,0.3927;0.5833,0.3927;0.6111,0.2904;0.4722,0.2904' \
//!     --field feature_class --field country --field admin1 \
//!     --layer admin/hierarchy:0
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::record_batch::RecordBatch;
use clap::Parser;
use serde_json::{json, Value};

use tessera_engine::filter::{FilterExpr, RegionLeaf};
use tessera_engine::shapes::{Bounds, ShapeF64, Space};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{
    AggregateCaps, AggregateHead, AggregateRequest, AggregateSink, By, Engine, EngineConfig,
    Grouping, PageEnd, Pick, RecordsLimits, Session, SinkResult, TableHead,
};

type BoxError = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(about = "Time aggregate responses over a built bundle")]
struct Args {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    view: String,
    /// `x,y;x,y;...` in the view's unit square.
    #[arg(long)]
    polygon: String,
    /// A category field to group by; repeatable.
    #[arg(long)]
    field: Vec<String>,
    /// `layer:level` to group by; repeatable.
    #[arg(long)]
    layer: Vec<String>,
    /// The field grouped by cell at depth 16.
    #[arg(long, default_value = "country")]
    by_cell: String,
    #[arg(long, default_value_t = 12)]
    threads: usize,
    #[arg(long, default_value_t = 5)]
    repeat: usize,
    /// Only the cases whose name contains one of these; every case where none is given.
    #[arg(long)]
    case: Vec<String>,
}

/// Counts what a response carries and keeps none of it.
#[derive(Default)]
struct Tally {
    rows: u64,
    pages: u64,
    /// Summed over every response of the read.
    compose_ns: u64,
    count_ns: u64,
    cells_ns: u64,
    pass_ns: u64,
    batch_ns: u64,
    methods: Vec<(u32, &'static str)>,
}

impl AggregateSink for Tally {
    fn head(&mut self, _: &AggregateHead) -> SinkResult {
        Ok(())
    }

    fn table(&mut self, _: &TableHead) -> SinkResult {
        Ok(())
    }

    fn page(&mut self, _: u32, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.rows += batch.num_rows() as u64;
        self.pages += 1;
        Ok(())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// The median, fastest and slowest wall time of `repeat` runs of `f` after one uncounted run, and
/// its last answer.
fn timed<T>(repeat: usize, mut f: impl FnMut() -> T) -> (T, Duration, Duration, Duration) {
    let _ = f();
    let mut samples = Vec::with_capacity(repeat);
    let mut last = None;
    for _ in 0..repeat.max(1) {
        let started = Instant::now();
        last = Some(f());
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    (
        last.expect("at least one run"),
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1],
    )
}

/// Every descriptor in the bundle's dictionary, in its order.
fn dictionary_terms(root: &Path) -> Result<Vec<String>, BoxError> {
    let current: Value = serde_json::from_slice(&std::fs::read(root.join("CURRENT"))?)?;
    let prefix = current["prefix"].as_str().ok_or("CURRENT has no prefix")?;
    let data = std::fs::read(root.join(prefix).join("dictionary/terms-0.dict"))?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 4;
        if offset + len > data.len() {
            break;
        }
        out.push(String::from_utf8_lossy(&data[offset..offset + len]).into_owned());
        offset += len;
    }
    Ok(out)
}

/// Every table of one request read to its end, following the cursor: rows and pages.
fn read(
    engine: &Engine,
    session: &Session,
    view: &str,
    filter: Option<FilterExpr>,
    groupings: &[Grouping],
) -> Result<Tally, BoxError> {
    let mut tally = Tally::default();
    let mut cursor: Option<String> = None;
    loop {
        let trailer = engine.aggregate_stream(
            session,
            AggregateRequest {
                view,
                filter: filter.clone(),
                reference: None,
                groupings,
                page_rows: None,
                pages: None,
                cursor: cursor.as_deref(),
                limits: RecordsLimits {
                    max_page_rows: 1 << 20,
                    max_page_bytes: 64 << 20,
                    response_bytes: 1 << 30,
                    response_time: Duration::from_secs(600),
                },
                caps: AggregateCaps {
                    groupings: 8,
                    top: 1000,
                    named: 1000,
                    bins: 1000,
                    cells: u64::MAX,
                },
                cancel: None,
            },
            &mut tally,
        )?;
        tally.compose_ns += trailer.timings.compose_ns;
        tally.count_ns += trailer.timings.count_ns;
        tally.cells_ns += trailer.timings.cells_ns;
        tally.pass_ns += trailer.timings.pass_ns;
        tally.batch_ns += trailer.timings.batch_ns;
        for method in trailer.timings.methods {
            if !tally.methods.contains(&method) {
                tally.methods.push(method);
            }
        }
        match trailer.next {
            None => return Ok(tally),
            Some(next) => cursor = Some(next),
        }
    }
}

fn main() -> Result<(), BoxError> {
    let args = Args::parse();
    if cfg!(debug_assertions) {
        eprintln!(
            "WARNING: debug build, so every figure below is meaningless. Build with --release."
        );
    }
    let tmp = std::env::temp_dir().join(format!("tessera-aggregate-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let engine = Engine::open(
        &args.bundle,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 1_000,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: args.threads,
            flush_max_age_secs: 90,
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )?;
    let meta = engine.meta();
    let view = meta
        .views
        .iter()
        .find(|v| v.id == args.view)
        .ok_or_else(|| format!("no view '{}'", args.view))?;
    let q = view.quantisation;
    let whole = [q.x_min, q.y_min, q.x_max, q.y_max];
    let bounds = Bounds {
        x_min: q.x_min,
        x_max: q.x_max,
        y_min: q.y_min,
        y_max: q.y_max,
    };
    let terms = dictionary_terms(&args.bundle)?;
    let session = engine.authorise(json!({ "terms": terms }).to_string().as_bytes())?;
    engine.viewport(&session, ViewportRequest::new(&args.view, 0, whole, 1))?;
    std::thread::sleep(Duration::from_millis(500));

    let mut ring = Vec::new();
    for vertex in args.polygon.split(';').filter(|v| !v.trim().is_empty()) {
        let (x, y) = vertex.split_once(',').ok_or("a vertex is x,y")?;
        ring.push((x.trim().parse::<f64>()?, y.trim().parse::<f64>()?));
    }
    let (shape, _) = ShapeF64::Polygon(vec![vec![ring]]).canonical(Space::View, &bounds)?;
    let region = FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape)));

    let field = |column: &str, cells: Option<u8>| Grouping {
        by: Some(By::Field {
            column: column.to_string(),
            pick: Pick::Top(10),
        }),
        cells,
        area: None,
    };
    let density = |depth: u8| Grouping {
        by: None,
        cells: Some(depth),
        area: None,
    };
    let mut cases: Vec<(String, Grouping, bool)> = vec![(
        "size".to_string(),
        Grouping {
            by: None,
            cells: None,
            area: None,
        },
        true,
    )];
    for column in &args.field {
        cases.push((format!("{column} top 10"), field(column, None), true));
    }
    for depth in [6u8, 16, 32] {
        cases.push((format!("density d{depth}"), density(depth), depth == 16));
    }
    cases.push((
        format!("{} top 10 x cells d16", args.by_cell),
        field(&args.by_cell, Some(16)),
        true,
    ));
    for spec in &args.layer {
        let (layer, level) = spec.split_once(':').ok_or("a layer is name:level")?;
        cases.push((
            format!("{spec} top 10"),
            Grouping {
                by: Some(By::Layer {
                    layer: layer.to_string(),
                    level: Some(level.parse()?),
                    pick: Pick::Top(10),
                }),
                cells: None,
                area: None,
            },
            true,
        ));
    }

    let viewport = |filter: &Option<FilterExpr>| -> Result<(), BoxError> {
        let mut request = ViewportRequest::new(&args.view, 4, whole, 1_000);
        if let Some(filter) = filter {
            request = request.filter(filter.clone());
        }
        engine.viewport(&session, request)?;
        Ok(())
    };
    println!(
        "{}",
        json!({"threads": args.threads, "repeat": args.repeat, "view": args.view})
    );
    for filter in [None, Some(region.clone())] {
        let (_, median, lo, hi) = timed(args.repeat, || viewport(&filter));
        println!(
            "{}",
            json!({
                "case": "viewport zoom 4",
                "region": filter.is_some(),
                "ms": {"median": ms(median), "fastest": ms(lo), "slowest": ms(hi)},
            })
        );
        for (name, grouping, with_region) in &cases {
            if filter.is_some() && !with_region {
                continue;
            }
            if !args.case.is_empty() && !args.case.iter().any(|c| name.contains(c.as_str())) {
                continue;
            }
            let groupings = std::slice::from_ref(grouping);
            let (tally, median, lo, hi) = timed(args.repeat, || {
                read(&engine, &session, &args.view, filter.clone(), groupings)
            });
            let tally = tally?;
            println!(
                "{}",
                json!({
                    "case": name,
                    "region": filter.is_some(),
                    "rows": tally.rows,
                    "pages": tally.pages,
                    "ms": {"median": ms(median), "fastest": ms(lo), "slowest": ms(hi)},
                    "last_run_ms": {
                        "compose": tally.compose_ns as f64 / 1e6,
                        "count": tally.count_ns as f64 / 1e6,
                        "cells": tally.cells_ns as f64 / 1e6,
                        "pass": tally.pass_ns as f64 / 1e6,
                        "batch": tally.batch_ns as f64 / 1e6,
                    },
                    "method": tally.methods.first().map(|(_, m)| *m),
                })
            );
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}
