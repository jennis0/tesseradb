//! **What does `POST /v1/artifacts/browse` cost, and what does its one whole-view scan cost?**
//!
//! `highlight-and-hierarchy.md` §7 leaves two figures ⊘. This measures both.
//!
//! - **A browse page** is one pass over the layer's artifacts — the gate, and one masked
//!   `and_cardinality` each — plus one more per row under `filters`. Measured against the two real
//!   corpora, which is what bounds it: a layer's artifact count, never the corpus's row count.
//! - **A `filters` whose leaves route row space** is the one whole-view pass the design adds
//!   (§9 (d), owner ruling): a leaf over a render-only column is answered by the viewport only
//!   inside its tiles, so browse runs the row route's own predicate over every row of the view.
//!
//! # Why the scan needs a fixture of its own, and what that costs the figure
//!
//! **Neither ladder corpus declares a `render = true, index = false` column.** arXiv's `archive`,
//! `primary_category` and `submitted_at` are all `render` **and** `index`; MedCPT's `published` is
//! too. A both-routes column takes the entity route here — browse passes `prefer_row = false`,
//! having no tile ranges to make the scan cheaper — so the scan is unreachable on them, and no
//! amount of asking either corpus produces the number. That is a fact about the corpora and is
//! stated rather than worked around.
//!
//! So the scan is measured on a **synthetic** bundle whose one attribute is declared render-only,
//! at row counts matching the two corpora. What that costs the figure is the corpus's *shape*: the
//! synthetic rows carry one `i32` in a single build segment, where a real view's rows are spread
//! over a segment ladder. The scan reads a fixed-width slot per row and compares, which is the
//! same work either way; what a real corpus adds is segment boundaries, and there are a handful.
//! **The row counts are real and the column is real; the corpus is not.** Nothing here reports a
//! ladder-corpus scan figure as measured.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin browse_cost -- \
//!     --bundle data/ladder/arxiv/bundle --view knn --layer clusters/kmeans
//! cargo run --release -p tessera-bench --bin browse_cost -- --scan-rows 2422486
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Float64Array, Int32Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_engine::browse::{BrowseForm, BrowseRequest};
use tessera_engine::filter::{Endpoint, FilterExpr, FilterOperand, Scalar};
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_store::read::open_bundle;

fn main() {
    let mut bundle: Option<PathBuf> = None;
    let mut view = "knn".to_string();
    let mut layer: Option<String> = None;
    let mut scan_rows: Option<u64> = None;
    let mut repeat = 5usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bundle" => bundle = args.next().map(PathBuf::from),
            "--view" => view = args.next().expect("--view takes a value"),
            "--layer" => layer = args.next(),
            "--scan-rows" => scan_rows = args.next().and_then(|n| n.replace('_', "").parse().ok()),
            "--repeat" => repeat = args.next().and_then(|n| n.parse().ok()).unwrap_or(5),
            other => panic!("unknown argument {other}"),
        }
    }
    if let Some(rows) = scan_rows {
        measure_scan(rows, repeat);
    }
    if let Some(root) = bundle {
        measure_pages(&root, &view, layer.as_deref(), repeat);
    }
}

fn engine_at(root: &Path, cache: &Path, wal: &Path, publishing: bool) -> Engine {
    let mut engine = Engine::open(
        root,
        cache,
        wal,
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            // No point is served here — the browse pass reads none — so the mark budget is set to
            // its smallest legal value rather than to a deployment's.
            max_k: 1,
            k_min: 1,
            k_max_marks: 1,
            theta_target_marks: 16,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: 0,
            flush_max_age_secs: 90,
            // The shipped row trigger, four commit windows (`DEFAULT_FLUSH_MAX_ITEMS`):
            // what bounds the window close's O(buffered) copy. Nothing here reaches it.
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            tier_width: None,
            segment_floor_bytes: None,
            coalesce_width: None,
            compaction: tessera_engine::CompactionSchedule::off(),
        },
    )
    .expect("the bundle opens");
    if publishing {
        engine
            .start_write_executor(8)
            .expect("the executor starts once");
    }
    engine
}

/// A credential the passthrough plugin reads as "every term this corpus has" — the broadest
/// principal, which is the one whose gate pass is the most expensive.
fn credential(terms: &[String]) -> Vec<u8> {
    let terms: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

/// Every term the bundle's dictionary holds — the widest principal it admits, and the one whose
/// gate pass over a layer is the most expensive.
fn all_terms(root: &Path) -> Vec<String> {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT")).unwrap();
    let prefix = current["prefix"].as_str().expect("a prefix");
    let Ok(data) = std::fs::read(root.join(prefix).join("dictionary/terms-0.dict")) else {
        return vec!["public".to_string()];
    };
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes")) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        if let Ok(term) = std::str::from_utf8(&data[at..at + len]) {
            out.push(term.to_string());
        }
        at += len;
    }
    out
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// The three forms against a real corpus, with and without an entity-routed filter.
fn measure_pages(root: &Path, view: &str, layer: Option<&str>, repeat: usize) {
    let tmp = tempfile::TempDir::new().expect("a scratch dir");
    let engine = engine_at(root, &tmp.path().join("cache"), &tmp.path().join("wal.log"), false);
    let layer = layer.map(str::to_string).expect("--layer names the layer to browse");
    let session = engine
        .authorise(&credential(&all_terms(root)))
        .expect("the passthrough plugin authorises");
    println!("bundle {}  view {view}  layer {layer}", root.display());

    let ask = |what: &str, form: BrowseForm, filter: Option<FilterExpr>| {
        let mut rows = 0usize;
        let mut samples = Vec::new();
        for _ in 0..repeat {
            let started = Instant::now();
            let out = engine
                .browse(
                    &session,
                    BrowseRequest {
                        view,
                        layer: &layer,
                        level: None,
                        form: form.clone(),
                        filter: filter.clone(),
                        limit: 200,
                        cursor: None,
                    },
                )
                .expect("a browse answers");
            samples.push(started.elapsed().as_secs_f64() * 1e3);
            rows = out.artifacts.len();
        }
        println!("  {what:<28} {:>9.1} ms   {rows} row(s)", median(samples));
        rows
    };
    ask("roots", BrowseForm::Roots, None);
    ask("search 'a'", BrowseForm::Search("a".into()), None);

    // **The first page a session asks for, which is a different number** (2026-09-02). On a
    // row-major level the masked counts the order is taken in are one walk of the composed mask
    // reading every visible row's labels — `RowColumn::histogram_over`, decision 0093's one named
    // exception — and it is cached per `(session, layer, level, mask)`. So every page after the
    // first is the figure above, and the first pays the walk: 2.7 s single-threaded over MedCPT's
    // 3.6 × 10⁷ rows × ~46 labels, 0.52 s once the walk is split across the pool.
    //
    // A fresh session per sample rather than a fresh engine: the artifact projections and the
    // level's row form are per deployment and built once at first use, and what is measured here
    // is what the *second* viewer of a warm process waits for.
    let mut samples = Vec::new();
    for _ in 0..repeat {
        let cold = engine
            .authorise(&credential(&all_terms(root)))
            .expect("the passthrough plugin authorises");
        let started = Instant::now();
        engine
            .browse(
                &cold,
                BrowseRequest {
                    view,
                    layer: &layer,
                    level: None,
                    form: BrowseForm::Roots,
                    filter: None,
                    limit: 200,
                    cursor: None,
                },
            )
            .expect("a browse answers");
        samples.push(started.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "  {:<28} {:>9.1} ms",
        "roots, first of a session",
        median(samples)
    );
}

/// The whole-view scan: a browse `filters` over a **render-only** column, on a synthetic bundle of
/// `rows` rows — see this module's doc for why it cannot be the ladder corpora's.
fn measure_scan(rows: u64, repeat: usize) {
    let tmp = tempfile::TempDir::new().expect("a scratch dir");
    let started = Instant::now();
    let bundle = build_scan_fixture(tmp.path(), rows);
    println!("synthetic bundle of {rows} rows built in {:?}", started.elapsed());
    let engine = engine_at(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        true,
    );
    // One artifact over the whole corpus: the scan is the cost under test, and a level of many
    // artifacts would put the gate's own pass beside it.
    let map = source_to_entity(&bundle, rows);
    engine
        .register_layer(scan_layer())
        .expect("the layer registers");
    let members: Vec<tessera_types::EntityId> = (0..rows)
        .map(|s| tessera_types::EntityId::new(map[s as usize]))
        .collect();
    engine
        .publish_artifacts(
            "bench/all".into(),
            0,
            vec![tessera_lifecycle::IncomingArtifact::from_entities(
                Some("all".into()),
                members,
            )],
        )
        .expect("the artifact publishes");
    let session = engine
        .authorise(&credential(&all_terms(&bundle)))
        .expect("the passthrough plugin authorises");
    let scan = FilterExpr::Leaf {
        column: "score".into(),
        operand: FilterOperand::Range {
            lo: None,
            hi: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: false,
            }),
        },
    };
    let ask = |what: &str, filter: Option<FilterExpr>| {
        let mut samples = Vec::new();
        let mut count = None;
        for _ in 0..repeat {
            let started = Instant::now();
            let out = engine
                .browse(
                    &session,
                    BrowseRequest {
                        view: "s0",
                        layer: "bench/all",
                        level: None,
                        form: BrowseForm::Roots,
                        filter: filter.clone(),
                        limit: 10,
                        cursor: None,
                    },
                )
                .expect("a browse answers");
            samples.push(started.elapsed().as_secs_f64() * 1e3);
            count = out.artifacts.first().and_then(|r| r.matched_count);
        }
        let ms = median(samples);
        println!(
            "  {what:<28} {ms:>9.1} ms   {:>6.1} ns/row   matched {count:?}",
            ms * 1e6 / rows as f64
        );
    };
    ask("no filter", None);
    ask("render-only leaf (scan)", Some(scan));
}

fn scan_layer() -> tessera_types::layer::LayerDeclaration {
    use tessera_types::layer::*;
    LayerDeclaration {
        scope: Default::default(),
        name: "bench/all".into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

const SCAN_SCHEMA: &str = r#"
[[attribute]]
name   = "score"
type   = "i32"
render = true
index  = false
"#;

fn build_scan_fixture(dir: &Path, rows: u64) -> PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, rows);
    write_pairs(&pairs, rows);
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, SCAN_SCHEMA).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the schema parses")
        .schema;
    let bundle = dir.join("bundle");
    tessera_build::build(&tessera_build::BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: tessera_spatial::Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: bundle.clone(),
        limit: None,
        identity_key: tessera_types::IdentityKey::from_hex(
            "07070707070707070707070707070707",
        )
        .expect("a well-formed key"),
        identity_key_hex: "07070707070707070707070707070707".to_string(),
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
    .expect("the fixture builds");
    bundle
}

fn write_points(path: &Path, rows: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", DataType::Int32, false),
    ]));
    let mut writer = ArrowWriter::try_new(
        std::fs::File::create(path).unwrap(),
        Arc::clone(&schema),
        None,
    )
    .unwrap();
    let chunk = 1_000_000u64;
    let mut at = 0u64;
    while at < rows {
        let n = chunk.min(rows - at);
        let ids: Vec<u64> = (at..at + n).collect();
        let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
        let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
        let scores: Vec<i32> = ids.iter().map(|&e| (e as i32).wrapping_mul(7) % 101 - 50).collect();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(UInt64Array::from(ids)),
                Arc::new(Float64Array::from(xs)),
                Arc::new(Float64Array::from(ys)),
                Arc::new(Int32Array::from(scores)),
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        at += n;
    }
    writer.close().unwrap();
}

fn write_pairs(path: &Path, rows: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut writer = ArrowWriter::try_new(
        std::fs::File::create(path).unwrap(),
        Arc::clone(&schema),
        None,
    )
    .unwrap();
    let chunk = 1_000_000u64;
    let mut at = 0u64;
    while at < rows {
        let n = chunk.min(rows - at);
        let ids: Vec<u64> = (at..at + n).collect();
        let terms: Vec<u32> = vec![0u32; n as usize];
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(UInt64Array::from(ids)),
                Arc::new(UInt32Array::from(terms)),
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        at += n;
    }
    writer.close().unwrap();
}

/// Source id → entity id, from the build's own external-id extent — the same read the engine's
/// own fixtures take, so the bench addresses artifacts the way every other caller does.
fn source_to_entity(bundle: &Path, rows: u64) -> Vec<u64> {
    let opened = open_bundle(bundle).expect("the bundle opens");
    let part = &opened.partitions["default"];
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().expect("a prefix");
    let path = bundle.join(prefix).join(&part.manifest.external_id_runs[0]);
    let reader =
        arrow::ipc::reader::FileReader::try_new(std::fs::File::open(&path).unwrap(), None).unwrap();
    let mut map = vec![0u64; rows as usize];
    for batch in reader {
        let batch = batch.unwrap();
        let ext = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::BinaryArray>()
            .unwrap();
        let ent = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
            map[source as usize] = ent.value(i) as u64;
        }
    }
    map
}
