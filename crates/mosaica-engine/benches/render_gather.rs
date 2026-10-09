//! The viewport's render-column gather, over a column every item holds and one half the items
//! lack.
//!
//! Three bundles of 10⁶ items, each with four rendered `f64` columns: in `whole` every item
//! carries a value, in `rare` one item in a thousand carries none, and in `sparse` every second
//! item carries none. A request at zoom 6 over 63 × 63 tiles
//! serves about 6.3 × 10⁴ points under the production density settings, so the four columns'
//! gather is a large share of the request. The tile count is below the parallel sweep's threshold,
//! so the whole request runs on the calling thread. The bundles are built in a temporary
//! directory, so the bench needs no fixture on disk.
//!
//! Before criterion times the whole request, each case prints the gather stage's time and the
//! request's CPU time on the calling thread, as the minimum and median of 200 requests. On a
//! machine shared with other work, the minima are the figures to compare.
//!
//! ```text
//! cargo bench -p mosaica-engine --bench render_gather
//! ```

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use criterion::{criterion_group, criterion_main, Criterion};
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use mosaica_build::config::{AttributeSource, Config};
use mosaica_build::{build, BuildArgs, ViewArgs};
use mosaica_engine::viewport::ViewportRequest;
use mosaica_engine::{Engine, EngineConfig};
use mosaica_spatial::{Bounds, Projection};
use mosaica_types::IdentityKey;

const N: u64 = 1_000_000;
const COLUMNS: [&str; 4] = ["a", "b", "c", "d"];
const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

fn write(path: &Path, columns: Vec<(Field, ArrayRef)>) {
    let (fields, arrays): (Vec<Field>, Vec<ArrayRef>) = columns.into_iter().unzip();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// A bundle whose four render columns hold no value for one item in `absent_every`, or for none
/// where it is `None`.
fn bundle(dir: &Path, absent_every: Option<u64>) -> Engine {
    let ids: Vec<u64> = (0..N).collect();
    let spread = |e: u64, salt: u64| {
        (e.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(salt as u32) % 65_536) as f64 + 0.5
    };
    let mut columns: Vec<(Field, ArrayRef)> = vec![
        (
            Field::new("entity_id", DataType::UInt64, false),
            Arc::new(UInt64Array::from(ids.clone())),
        ),
        (
            Field::new("x", DataType::Float64, false),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|&e| spread(e, 7)))),
        ),
        (
            Field::new("y", DataType::Float64, false),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|&e| spread(e, 29)))),
        ),
    ];
    for (n, name) in COLUMNS.iter().enumerate() {
        let values: Float64Array = ids
            .iter()
            .map(|&e| {
                let absent = absent_every.is_some_and(|every| e % every == 0);
                (!absent).then_some((e + n as u64) as f64)
            })
            .collect();
        columns.push((Field::new(*name, DataType::Float64, true), Arc::new(values)));
    }
    let points = dir.join("points.parquet");
    write(&points, columns);
    let pairs = dir.join("pairs.parquet");
    write(
        &pairs,
        vec![
            (
                Field::new("entity_id", DataType::UInt64, false),
                Arc::new(UInt64Array::from(ids)),
            ),
            (
                Field::new("term_id", DataType::UInt32, false),
                Arc::new(UInt32Array::from(vec![0u32; N as usize])),
            ),
        ],
    );
    let toml: String = COLUMNS
        .iter()
        .map(|name| format!("[[attribute]]\nname = \"{name}\"\ntype = \"f64\"\nrender = true\n\n"))
        .collect();
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, toml).unwrap();
    let schema = Config::parse(&schema_path, &Default::default()).unwrap().schema;
    let out = dir.join("bundle");
    build(&BuildArgs {
        views: vec![ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: AttributeSource::over(points, &schema),
        out: out.clone(),
        limit: None,
        strict: false,
        identity_key: IdentityKey::from_hex(KEY_HEX).unwrap(),
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
    Engine::open(
        &out,
        &dir.join("cache"),
        &dir.join("wal.log"),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 500,
            k_min: 2,
            k_max_marks: 500,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: mosaica_engine::default_compute_threads(),
            flush_max_age_secs: 90,
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: mosaica_engine::CompactionSchedule::off(),
        },
    )
    .unwrap()
}

/// CPU time consumed by the calling thread, in nanoseconds.
fn thread_cpu_ns() -> u64 {
    let mut at = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `at` is a valid, writable timespec for the call's duration.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut at) };
    at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64
}

fn bench_render_gather(c: &mut Criterion) {
    let mut group = c.benchmark_group("render_gather");
    for (name, absent_every) in [("whole", None), ("rare", Some(1000)), ("sparse", Some(2))] {
        let tmp = TempDir::new().unwrap();
        let engine = bundle(tmp.path(), absent_every);
        let session = engine.authorise(br#"{"terms": ["0"]}"#).unwrap();
        let request = || ViewportRequest::new("s0", 6, [0.0, 0.0, 64512.0, 64512.0], 500);
        let mut gather = Vec::new();
        let mut cpu = Vec::new();
        for _ in 0..200 {
            let before = thread_cpu_ns();
            let out = engine.viewport(&session, request()).unwrap();
            cpu.push(thread_cpu_ns() - before);
            gather.push(out.timings.gather_ns);
        }
        gather.sort_unstable();
        cpu.sort_unstable();
        let served = engine.viewport(&session, request()).unwrap().points.len();
        eprintln!(
            "render_gather/{name}: {served} points a request; gather min {} us, median {} us; \
             request cpu min {} us, median {} us",
            gather[0] / 1000,
            gather[100] / 1000,
            cpu[0] / 1000,
            cpu[100] / 1000,
        );
        group.bench_function(name, |b| {
            b.iter(|| engine.viewport(&session, request()).unwrap());
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30);
    targets = bench_render_gather
}
criterion_main!(benches);
