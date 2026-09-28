//! **What does it cost to cross a region's rows to their entities?**
//!
//! `RowSpace::entities_of_rows` reads a row set as runs, each run as one slice of the view's
//! `row-entity.u32` or of the extent inverse, in parallel chunks of 2^20 rows. This times it over
//! the rows of a principal's visible set under a polygon, on a pool of one thread and on a pool
//! of the engine's width, beside the row-at-a-time walk it replaces (`RowSpace::entity_of` per
//! row, then a sort), and checks that both give the same entities.
//!
//! Every figure is the median of `--repeat` runs, printed with the fastest and slowest.
//!
//! ```text
//! cargo build --release -p tessera-bench --bin row_entity_crossing
//! systemd-run --user --scope --collect -p MemoryMax=16G -p MemorySwapMax=2G -- \
//!     target/release/row_entity_crossing \
//!     --bundle data/ladder/geonames/bundle-final --view world \
//!     --polygon '0.4722,0.3927;0.5833,0.3927;0.6111,0.2904;0.4722,0.2904'
//! ```
//!
//! The principal holds every term in the bundle's dictionary. The polygon is in the view's own
//! unit square.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use croaring::Bitmap;
use serde_json::{json, Value};

use tessera_engine::compose::MaskedSet;
use tessera_engine::region::RegionDecomposition;
use tessera_engine::shapes::{Bounds, ShapeF64, Space};
use tessera_engine::viewport::{segments_with_row_bases, ViewportRequest};
use tessera_engine::{Engine, EngineConfig, DEFAULT_MAX_REGION_CELLS};
use tessera_plugin::Passthrough;
use tessera_types::RowId;

type BoxError = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(about = "Time crossing a region's rows to their entities")]
struct Args {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    view: String,
    /// `x,y;x,y;...` in the view's unit square.
    #[arg(long)]
    polygon: String,
    #[arg(long, default_value_t = 5)]
    repeat: usize,
}

/// The median, fastest and slowest of `repeat` runs of `f`, and its last answer.
fn timed<T>(repeat: usize, mut f: impl FnMut() -> T) -> (T, Duration, Duration, Duration) {
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

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
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

fn main() -> Result<(), BoxError> {
    let args = Args::parse();
    if cfg!(debug_assertions) {
        eprintln!(
            "WARNING: debug build, so every figure below is meaningless. Build with --release."
        );
    }
    let tmp = std::env::temp_dir().join(format!("tessera-crossing-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let threads = tessera_engine::default_compute_threads();
    let engine = Engine::open(
        &args.bundle,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 1_000,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: threads,
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
    let bounds = Bounds {
        x_min: q.x_min,
        x_max: q.x_max,
        y_min: q.y_min,
        y_max: q.y_max,
    };
    let terms = dictionary_terms(&args.bundle)?;
    let session = engine.authorise(json!({ "terms": terms }).to_string().as_bytes())?;
    engine.viewport(
        &session,
        ViewportRequest::new(&args.view, 0, [q.x_min, q.y_min, q.x_max, q.y_max], 1),
    )?;
    std::thread::sleep(Duration::from_millis(500));

    let (generation, mask) = engine.composed_mask(&session, &args.view)?;
    let view_data = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(&args.view))
        .ok_or("the view has no data in any partition")?;
    let segments = segments_with_row_bases(&args.view, view_data)?;
    let row_space = &view_data.row_space;

    let mut ring = Vec::new();
    for vertex in args.polygon.split(';').filter(|v| !v.trim().is_empty()) {
        let (x, y) = vertex.split_once(',').ok_or("a vertex is x,y")?;
        ring.push((x.trim().parse::<f64>()?, y.trim().parse::<f64>()?));
    }
    let (shape, _) = ShapeF64::Polygon(vec![vec![ring]]).canonical(Space::View, &bounds)?;
    let decomposition =
        RegionDecomposition::build(Arc::new(shape), DEFAULT_MAX_REGION_CELLS, &segments);
    let rows = mask.visible_rows(&decomposition.rows_under(&mask, &segments));

    let serial = || {
        let mut entities: Vec<u32> = rows
            .iter()
            .map(|row| {
                row_space
                    .entity_of(RowId::new(row))
                    .expect("every row has an entity")
                    .raw() as u32
            })
            .collect();
        entities.sort_unstable();
        Bitmap::of(&entities)
    };
    let (want, walk, walk_lo, walk_hi) = timed(args.repeat, serial);

    let mut report = vec![json!({
        "rows": rows.cardinality(),
        "entities": want.cardinality(),
        "row_at_a_time_ms": {"median": ms(walk), "fastest": ms(walk_lo), "slowest": ms(walk_hi)},
    })];
    for width in [1, threads] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(width).build()?;
        let (got, median, lo, hi) = timed(args.repeat, || {
            pool.install(|| row_space.entities_of_rows(&rows))
        });
        let got = got.ok_or("the view has no row-entity.u32, so the crossing has no answer")?;
        report.push(json!({
            "threads": width,
            "entities_of_rows_ms": {"median": ms(median), "fastest": ms(lo), "slowest": ms(hi)},
            "agrees": got == want,
        }));
    }
    for line in report {
        println!("{line}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}
