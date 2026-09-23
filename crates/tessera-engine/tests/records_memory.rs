//! What a records read holds at its peak, measured by a counting allocator that CRoaring's own
//! allocations are routed through, against the page's byte ceiling.
//!
//! One test, so no other test's allocations land in the count. The engine's background work is
//! quiet: the flush tick is an hour and the refresh is off.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::{Config, Fields};
use tessera_build::{build, BuildArgs, ViewArgs};
use tessera_engine::filter::{FilterExpr, RegionLeaf};
use tessera_engine::shapes::ShapeF64;
use tessera_engine::{
    Engine, ItemsHead, ItemsLimits, ItemsPageEnd, ItemsRequest, ItemsSink, RecordsOrder,
    Session, SinkResult,
};
use tessera_spatial::shape::Space;
use tessera_spatial::Projection;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the counters are side effects on relaxed
// atomics and never influence the pointer returned.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            note_alloc(layout.size() as u64);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            note_alloc(layout.size() as u64);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            note_alloc(new_size as u64);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn note_alloc(size: u64) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed).wrapping_add(size);
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The most bytes live at once while `f` runs, over what was live when it started. A free of
/// memory allocated before is counted below the start, so the peak is of what `f` added.
fn peak_of(f: impl FnOnce()) -> u64 {
    const START: u64 = 1 << 62;
    LIVE.store(START, Ordering::Relaxed);
    PEAK.store(START, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    f();
    TRACKING.store(false, Ordering::Relaxed);
    PEAK.load(Ordering::Relaxed) - START
}

/// A sink that keeps nothing but the rows it was sent.
#[derive(Default)]
struct Count {
    rows: usize,
}

impl ItemsSink for Count {
    fn head(&mut self, _: &ItemsHead) -> SinkResult {
        Ok(())
    }

    fn page(&mut self, batch: &RecordBatch, _: &ItemsPageEnd) -> SinkResult {
        self.rows += batch.num_rows();
        Ok(())
    }
}

/// Every row of the view in one response, `page_rows` a page under a `max_page_bytes` ceiling.
fn read(
    engine: &Engine,
    session: &Session,
    order: RecordsOrder,
    fields: &[String],
    filter: Option<FilterExpr>,
    page_rows: Option<u32>,
    max_page_bytes: usize,
) -> usize {
    let req = ItemsRequest {
        view: "s0",
        fields,
        system_fields: &[],
        filter,
        keep_unmatched: false,
        count: false,
        order: Some(order),
        page_rows,
        pages: None,
        cursor: None,
        idset: None,
        limits: ItemsLimits {
            max_page_rows: 1 << 20,
            max_page_bytes,
            response_bytes: 1 << 40,
            response_time: Duration::from_secs(600),
        },
        cancel: None,
    };
    let mut sink = Count::default();
    let trailer = engine.items_stream(session, req, &mut sink).expect("a response");
    assert!(trailer.next.is_none(), "one response reads the view");
    sink.rows
}

/// A bundle of `n` items along the diagonal, so source order, item order and map order agree,
/// each with a `note` of `note_of(s)` bytes, held in the record store.
fn bundle(dir: &Path, n: u64, note_of: impl Fn(u64) -> usize) -> Engine {
    let ids: Vec<u64> = (0..n).collect();
    let at = |s: u64| (s as f64 + 0.5) * 999.0 / n as f64;
    let columns: Vec<(Field, ArrayRef)> = vec![
        (
            Field::new("entity_id", DataType::UInt64, false),
            Arc::new(UInt64Array::from(ids.clone())),
        ),
        (
            Field::new("x", DataType::Float64, false),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|&s| at(s)))),
        ),
        (
            Field::new("y", DataType::Float64, false),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|&s| at(s)))),
        ),
        (
            Field::new("note", DataType::Utf8, true),
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|&s| "n".repeat(note_of(s))),
            )),
        ),
    ];
    let (fields, arrays): (Vec<Field>, Vec<ArrayRef>) = columns.into_iter().unzip();
    let schema = Arc::new(Schema::new(fields));
    let points = dir.join("points.parquet");
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(&points).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, n);
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, "[[attribute]]\nname = \"note\"\ntype = \"keyword\"\n").unwrap();
    let schema = Config::parse(&schema_path, &std::collections::HashMap::new())
        .map(|c| c.schema)
        .expect("the schema parses");
    let out = dir.join("bundle");
    build(&BuildArgs {
        views: vec![ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Fields::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the bundle builds");
    let engine = engine_at(dir, &out, 3600);
    engine.set_background_refresh_for_test(false);
    engine
}

/// **A read holds a bounded multiple of its page ceiling however skewed its rows are, and a
/// stretch no more than the ceiling**: short notes and then notes of 100 KB, read under a 256 KiB
/// ceiling, in both orders; and a sparse filter whose stretches grow to their ceiling, for a
/// viewer who sees every item and one who sees every third. What a stretch holds a row is
/// measured from two reads whose stretches differ in size and nothing else.
#[test]
fn a_reads_peak_is_held_to_its_page_ceiling() {
    // SAFETY: called before this process allocates a bitmap.
    unsafe { croaring::configure_rust_alloc() };
    let ceiling = 256 << 10;
    let big = 100_000;
    let tmp = tempfile::TempDir::new().unwrap();
    let skewed = tmp.path().join("skewed");
    std::fs::create_dir_all(&skewed).unwrap();
    let engine = bundle(&skewed, 4000, |s| if s < 3700 { 8 } else { big });
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let fields = vec!["note".to_string()];
    for order in [RecordsOrder::Stored, RecordsOrder::Map] {
        assert_eq!(read(&engine, &session, order, &fields, None, None, ceiling), 4000);
        let peak = peak_of(|| {
            read(&engine, &session, order, &fields, None, None, ceiling);
        });
        eprintln!(
            "skewed {order:?}: peak {peak} bytes, {:.2} times the page ceiling",
            peak as f64 / ceiling as f64
        );
        assert!(
            peak < 8 * ceiling as u64,
            "{order:?}: the read held {peak} bytes under a {ceiling}-byte page ceiling"
        );
    }
    drop(engine);

    let sparse = tmp.path().join("sparse");
    std::fs::create_dir_all(&sparse).unwrap();
    let n = 400_000u64;
    let engine = bundle(&sparse, n, |_| 0);
    // A band across the diagonal a few hundred items wide, so every stretch grows to its ceiling.
    let band = ShapeF64::Bbox {
        min_x: 0.0,
        min_y: 500.0,
        max_x: 1000.0,
        max_y: 500.5,
    }
    .canonical(Space::View, &extent())
    .unwrap()
    .0;
    let band = FilterExpr::Region(RegionLeaf::Shape(Arc::new(band)));
    // A viewer who sees every item, and one who sees every third, whose stretches' rows are
    // scattered.
    let viewers = [("every", full_coverage_credential()), ("third", subset_credential())];
    for (viewer, credential) in viewers {
        let session = engine.authorise(&credential).unwrap();
        for order in [RecordsOrder::Stored, RecordsOrder::Map] {
            let mut peaks = Vec::new();
            for ceiling in [256usize << 10, 1 << 20] {
                let matched =
                    read(&engine, &session, order, &[], Some(band.clone()), Some(16), ceiling);
                assert!(matched > 50, "the band holds {matched} items");
                let peak = peak_of(|| {
                    read(&engine, &session, order, &[], Some(band.clone()), Some(16), ceiling);
                });
                eprintln!(
                    "sparse, {viewer} item, {order:?} under {ceiling}: peak {peak} bytes, {:.2} \
                     times the page ceiling",
                    peak as f64 / ceiling as f64
                );
                assert!(
                    peak < (ceiling + ceiling / 4) as u64,
                    "{viewer} {order:?}: the read held {peak} bytes under a {ceiling}-byte ceiling"
                );
                peaks.push((ceiling, peak));
            }
            let ((low, low_peak), (high, high_peak)) = (peaks[0], peaks[1]);
            eprintln!(
                "sparse, {viewer} item, {order:?}: {:.3} bytes held per byte of page ceiling",
                (high_peak as f64 - low_peak as f64) / (high - low) as f64
            );
        }
    }
}
