//! **`render` on a group-scoped attribute, on the wire** (`views.md` §5): the value reaches the
//! hot row tail of every view of its group — and of any group sharing those views via `members` —
//! and of no other view.
//!
//! That is the rule `per-point-attributes.md` §3.9 already has for `render_in`, with the view set
//! decided by the scope instead of listed, and it is the one thing a scope buys that no filter
//! does: the *same entity* draws with a different value on two maps. Four facts carry it here, and
//! each is a way the placement could be wrong while every filter test beside it still passed:
//!
//! - **Per view, and the values are that view's.** An entity in two quarters carries each
//!   quarter's own number, so a build that permuted one view's column into another's row space
//!   would be caught by the values rather than by their absence.
//! - **Nowhere else.** The plain view's rows carry no such column at all — asserted against a
//!   second build of the same corpus with the family removed, whose segment must be
//!   byte-identical.
//! - **A sharing group renders the owner's family**, under its own view ids and its own geometry.
//! - **Absence is ordinary.** An entity with no value in a quarter takes the type's zero, exactly
//!   as an entity-scoped render column's absence does (decision 0064), and a view created while
//!   the service runs — which no batch can write a scoped column for — simply carries none.
//!
//! Since 2026-08-31 the family is also a **filter operand** on `render` alone (`views.md` §5 r26),
//! which is why the filter cases below live in this file rather than beside the indexed family's:
//! `heat` is declared `index = false`, so what answers a leaf here is the licence `render` gives
//! and nothing else. What they check is that the operand is the **entity-space column** and not
//! the lane — a pin from a view outside the group answers over rows that hold no lane at all —
//! and that the answer follows the write path, an ingested value being filterable once flushed and
//! after a fold.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::config::{Attribute, Fields};
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation, ScopedColumnFamily,
    ViewArgs,
};
use tessera_engine::EngineConfig;
use tessera_spatial::tiler::ScalarType;

const ENTITIES: u64 = 24;
/// The plain view holds the first twenty; each quarter holds its own slice, so the two quarters
/// overlap on a run of entities and each of those draws twice, with two values.
const WORLD: std::ops::Range<u64> = 0..20;
const QUARTERS: [(&str, std::ops::Range<u64>); 2] = [("2026-Q1", 0..15), ("2026-Q2", 8..24)];

/// A key the sharing group declares and the owning group does not. The owner's view of it is
/// created while the service runs, so the two views of the key sit at different incarnations —
/// the one shape in which a flush through the borrowing view can stamp an artifact with an
/// incarnation the view it names does not carry. Nothing but
/// [`a_borrowing_views_scoped_values_survive_a_restart`] reaches it.
const BORROWED_KEY: &str = "2026-Q4";
/// The rows the sharing group's view of [`BORROWED_KEY`] holds at the build: geometry only, the
/// family's columns belonging to the group that owns the key.
const BORROWED: std::ops::Range<u64> = 0..10;

/// **`heat`, per view and per entity** — the rendered family. An entity's value differs between
/// quarters, so a tail gathered from the wrong view's column is a wrong number rather than a
/// missing one; one entity in three carries none at all, which is the presence bitmap's ordinary
/// case (decision 0064) and reaches the wire as the type's zero.
fn heat(slot: usize, entity: u64) -> Option<f32> {
    if (entity + slot as u64).is_multiple_of(3) {
        return None;
    }
    Some((entity * 10 + slot as u64) as f32 / 4.0)
}

/// What a row of `heat` carries on the wire: the value, or the placeholder an absence is written
/// as (the render column is non-nullable — contracts R4 — so absence is the type's zero).
fn heat_on_the_wire(slot: usize, entity: u64) -> f32 {
    heat(slot, entity).unwrap_or(0.0)
}

fn members(slot: usize) -> std::ops::Range<u64> {
    QUARTERS[slot].1.clone()
}

/// A view's own layout: the same entity sits somewhere different in each, so a response's points
/// are that view's rows and not another's.
fn position(view: &str, e: u64) -> (f64, f64) {
    match view.split_once(':') {
        None => ((e % 5) as f64 * 100.0, (e / 5) as f64 * 100.0),
        Some(("quarter", key)) => (
            900.0 - (e % 5) as f64 * 100.0,
            (e / 5) as f64 * 70.0 + key.len() as f64,
        ),
        Some((_, _)) => ((e % 7) as f64 * 90.0, 900.0 - (e / 7) as f64 * 60.0),
    }
}

fn group_frame() -> Quantisation {
    let e = extent();
    Quantisation {
        x_min: e.x_min,
        x_max: e.x_max,
        y_min: e.y_min,
        y_max: e.y_max,
    }
}

/// A points file. A view of a group carries the `heat` column its family reads from it; the plain
/// view carries none, which is what it means for the family to be the group's.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, slot: Option<usize>) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ];
    if slot.is_some() {
        fields.push(Field::new("heat", DataType::Float32, true));
        // The two families the two-door work added: a `text` one, whose stored value cannot be
        // compared once flushed, and one carrying neither flag, which is stored and served at the
        // drill-down without being searchable or drawn.
        fields.push(Field::new("note", DataType::Utf8, true));
        fields.push(Field::new("tag", DataType::Float32, true));
    }
    let schema = Arc::new(ArrowSchema::new(fields));
    let ids: Vec<u64> = ids.collect();
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(UInt64Array::from(ids.clone())),
        Arc::new(Float64Array::from(
            ids.iter().map(|&e| position(view, e).0).collect::<Vec<_>>(),
        )),
        Arc::new(Float64Array::from(
            ids.iter().map(|&e| position(view, e).1).collect::<Vec<_>>(),
        )),
    ];
    if let Some(slot) = slot {
        columns.push(Arc::new(Float32Array::from(
            ids.iter().map(|&e| heat(slot, e)).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(arrow::array::StringArray::from(
            ids.iter()
                .map(|&e| Some(format!("built prose for {e} in {slot}")))
                .collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(Float32Array::from(
            ids.iter()
                .map(|&e| Some((e * 2 + slot as u64) as f32))
                .collect::<Vec<_>>(),
        )));
    }
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The gate on `quarter:2026-Q2`, and on nothing else: the term a principal must hold to reach
/// that one view. Access is a relation here, so the descriptors the dictionary carries are the
/// relation's own integers and `"1"` is one of them (`common::terms_of` grants it on `e % 3 == 0`).
///
/// It exists for the case a sharing group makes possible and nothing else does: a principal who
/// reaches `quarter_map:2026-Q2` and **not** `quarter:2026-Q2`, for whom the column arrives under
/// an id the family's own list does not name (`views.md` §3.3, §6).
const GATED_QUARTER: &str = "2026-Q2";
const GATE_TERM: &str = "1";

fn view_args(view: &str, points: &Path, pairs: &Path, visibility: Option<&str>) -> ViewArgs {
    ViewArgs {
        visibility: visibility.map(|label| vec![label.to_string()]),
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Fields::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

/// The group's roster. `gated` says whether this roster is the owning group's, whose
/// `2026-Q2` carries a gate — the view's own `visibility` and the roster's record of it are checked
/// equal at open, so the two say the same thing here.
fn roster(gated: bool) -> Vec<GroupViewDescriptor> {
    QUARTERS
        .iter()
        .map(|(key, _)| GroupViewDescriptor {
            key: key.to_string(),
            visibility: (gated && *key == GATED_QUARTER).then(|| vec![GATE_TERM.to_string()]),
            metadata: Default::default(),
        })
        .collect()
}

/// The sharing group's roster: the owner's two keys and [`BORROWED_KEY`], which is its own until
/// the owner's view of it is created.
fn sharing_roster() -> Vec<GroupViewDescriptor> {
    let mut views = roster(false);
    views.push(GroupViewDescriptor {
        key: BORROWED_KEY.to_string(),
        visibility: None,
        metadata: Default::default(),
    });
    views
}

/// The bundle: a plain view, a group of two quarters carrying the rendered family, and a second
/// group that is a different layout over the same two keys (`views.md` §3.3).
///
/// `declared` says whether the family is declared at all — the `false` build is the byte-equality
/// reference for a view outside every scope.
fn build_bundle(dir: &Path, declared: bool) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_points(&world_points, "world", WORLD, None);
    let mut views = vec![view_args("world", &world_points, &pairs, None)];
    let mut family_views = Vec::new();
    for (slot, (key, range)) in QUARTERS.iter().enumerate() {
        let id = format!("quarter:{key}");
        let points = dir.join(format!("quarter-{key}.parquet"));
        write_points(&points, &id, range.clone(), Some(slot));
        family_views.push(views.len());
        let gate = (*key == GATED_QUARTER).then_some(GATE_TERM);
        views.push(view_args(&id, &points, &pairs, gate));
    }
    // The sharing group's views are public throughout, including the one whose owner counterpart
    // is gated — which is what makes the mixed case reachable at all.
    for (slot, (key, range)) in QUARTERS.iter().enumerate() {
        let id = format!("quarter_map:{key}");
        let points = dir.join(format!("map-{key}.parquet"));
        write_points(&points, &id, range.clone(), Some(slot));
        views.push(view_args(&id, &points, &pairs, None));
    }
    // The sharing group's own key, which the owner acquires only while the service runs.
    let borrowed = format!("quarter_map:{BORROWED_KEY}");
    let borrowed_points = dir.join("map-borrowed.parquet");
    write_points(&borrowed_points, &borrowed, BORROWED, None);
    views.push(view_args(&borrowed, &borrowed_points, &pairs, None));
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![
            GroupDescriptor {
                title: None,
                point_default: Some("public".to_string()),
                visibility: None,
                name: "quarter".to_string(),
                members_of: None,
                views: roster(true),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
            GroupDescriptor {
                title: None,
                point_default: Some("public".to_string()),
                visibility: None,
                name: "quarter_map".to_string(),
                // The same keys, a second layout: a family over these views belongs to the group
                // that owns them, and renders under both.
                members_of: Some("quarter".to_string()),
                views: sharing_roster(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
        ],
        scoped_attributes: match declared {
            // **`render` without `index`**: the placement this file is about, and — since
            // `views.md` §5 r26 — the whole of the licence the filter cases below are answered
            // by. The family has two homes, the hot tail of each view of the group and the
            // entity-space column beside it that every build writes whatever the flags.
            true => vec![
                ScopedColumnFamily {
                    attribute: Attribute {
                        name: "heat".to_string(),
                        title: None,
                        field: None,
                        ty: ScalarType::F32,
                        analyser: None,
                        vocabulary: None,
                        value_set: None,
                        index: false,
                        render: true,
                    },
                    group: "quarter".to_string(),
                    views: family_views.clone(),
                    source: None,
                },
                // **A `text` family**, indexed so it has a column at all. Its extent is a token
                // dictionary and positional postings and holds **no value per entity**, which is why
                // the cell arm cannot compare its stored prose across a flush and refuses instead
                // (decision 0116, review finding F1).
                ScopedColumnFamily {
                    attribute: Attribute {
                        name: "note".to_string(),
                        title: None,
                        field: None,
                        ty: ScalarType::Text,
                        analyser: Some("unicode/icu4x-2.2/p1".to_string()),
                        vocabulary: None,
                        value_set: None,
                        index: true,
                        render: false,
                    },
                    group: "quarter".to_string(),
                    views: family_views.clone(),
                    source: None,
                },
                // **Neither flag**: stored, served at the drill-down, not searchable and not drawn
                // (owner ruling). It is here because a flush gated its extents on the *filter* licence,
                // so such a family served the build's values and nothing ingested since.
                ScopedColumnFamily {
                    attribute: Attribute {
                        name: "tag".to_string(),
                        title: None,
                        field: None,
                        ty: ScalarType::F32,
                        analyser: None,
                        vocabulary: None,
                        value_set: None,
                        index: false,
                        render: false,
                    },
                    group: "quarter".to_string(),
                    views: family_views.clone(),
                    source: None,
                },
            ],
            false => Vec::new(),
        },
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("the fixture builds");
    out
}

struct Served {
    server: TestServer,
    token: String,
    bundle: std::path::PathBuf,
    _tmp: TempDir,
}

async fn serve() -> Served {
    serve_with(default_engine_config()).await
}

/// The fixture under a caller-chosen configuration — the lifecycle test below narrows
/// `coalesce_width` so the entity-space coalesce is reachable from a handful of flushes.
async fn serve_with(config: EngineConfig) -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_bundle(tmp.path(), true);
    let server = spawn_server_with_config(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;
    // Both terms, so every entity is visible and a view's answer is its population rather than a
    // slice of it. The mask's own effect on the tail is `a_masked_row_carries_no_tail` below.
    let auth = authorise(&server, &["0", "1"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    Served {
        server,
        token,
        bundle,
        _tmp: tmp,
    }
}

async fn viewport_bytes(served: &Served, token: &str, view: &str) -> (u16, Vec<u8>) {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.bytes().await.unwrap().to_vec())
}

/// One ingest batch into `view`. The fixture declares no entity-scoped attribute, so a row is its
/// external id, its position, its access label — and, where `heat` is non-empty, the group-scoped
/// family's value **under its plain name** (`views.md` §5): the view is known from the header, so
/// the column is not qualified and the view decides which of the family's columns the value is
/// for. An empty `heat` is a batch that names no such column at all, which is a family every row
/// is absent in rather than a malformed batch.
async fn ingest_with_heat(
    served: &Served,
    batch_id: &str,
    view: &str,
    rows: &[(Vec<u8>, f32, f32, &str)],
    heat: &[Option<f32>],
) {
    let body = if heat.is_empty() {
        build_ingest_batch_optional(
            &rows
                .iter()
                .map(|(id, x, y, access)| (Some(id.as_slice()), *x, *y, *access))
                .collect::<Vec<_>>(),
        )
    } else {
        batch_with_heat(rows, heat)
    };
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the batch is accepted");
}

/// The same batch, refused or not, with its status and body returned — for the cases where the
/// refusal *is* the assertion.
async fn try_ingest_with_heat(
    served: &Served,
    batch_id: &str,
    view: &str,
    rows: &[(Vec<u8>, f32, f32, &str)],
    heat: &[Option<f32>],
) -> (u16, String) {
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(batch_with_heat(rows, heat))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// One ingest batch naming any subset of the group's three families, with its status and body —
/// the general form of [`ingest_with_heat`], for the cases that write `note` or `tag`.
///
/// A `None` list is a batch that names no such column at all, which is a family every row is
/// absent in rather than a malformed batch.
async fn try_ingest_families(
    served: &Served,
    batch_id: &str,
    view: &str,
    rows: &[(Vec<u8>, f32, f32, &str)],
    heat: Option<&[Option<f32>]>,
    note: Option<&[Option<&str>]>,
    tag: Option<&[Option<f32>]>,
) -> (u16, String) {
    use arrow::array::{BinaryArray, StringArray};
    let access = access_column(rows.iter().map(|(_, _, _, a)| *a));
    let mut fields = vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
    ];
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(BinaryArray::from_iter(
            rows.iter().map(|(id, _, _, _)| Some(id.as_slice())),
        )),
        Arc::new(Float32Array::from_iter_values(
            rows.iter().map(|(_, x, _, _)| *x),
        )),
        Arc::new(Float32Array::from_iter_values(
            rows.iter().map(|(_, _, y, _)| *y),
        )),
        Arc::new(access),
    ];
    if let Some(heat) = heat {
        fields.push(Field::new("heat", DataType::Float32, true));
        columns.push(Arc::new(Float32Array::from(heat.to_vec())));
    }
    if let Some(note) = note {
        fields.push(Field::new("note", DataType::Utf8, true));
        columns.push(Arc::new(StringArray::from(note.to_vec())));
    }
    if let Some(tag) = tag {
        fields.push(Field::new("tag", DataType::Float32, true));
        columns.push(Arc::new(Float32Array::from(tag.to_vec())));
    }
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    w.write(&batch).unwrap();
    let body = w.into_inner().unwrap();
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// An Arrow ingest body carrying the reserved columns and a nullable `heat`.
fn batch_with_heat(rows: &[(Vec<u8>, f32, f32, &str)], heat: &[Option<f32>]) -> Vec<u8> {
    use arrow::array::BinaryArray;
    let access = access_column(rows.iter().map(|(_, _, _, a)| *a));
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("heat", DataType::Float32, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter(
                rows.iter().map(|(id, _, _, _)| Some(id.as_slice())),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|(_, x, _, _)| *x),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|(_, _, y, _)| *y),
            )),
            Arc::new(access),
            Arc::new(Float32Array::from(heat.to_vec())),
        ],
    )
    .unwrap();
    let mut w = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    w.write(&batch).unwrap();
    w.into_inner().unwrap()
}

/// Flush until the buffer is empty — a flush unit is one view, so a batch that landed in two needs
/// two ticks (`views_write.rs` carries the same helper and the same argument).
async fn flush(served: &Served) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let before = served.server.state.engine.write_executor_stats().flushes;
        let resp = served
            .server
            .client
            .post(served.server.control_url("/control/flush"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        while served.server.state.engine.write_executor_stats().flushes == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never published"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if served.server.state.engine.buffered_items() == 0 {
            break;
        }
    }
}

/// The points frames' column names, and `heat` per `tessera_id` where the frame carries it.
///
/// Read **by name**, which is what contracts §3.2 requires of a client: the scoped columns follow
/// the entity-scoped ones, and a reader that indexed positionally would be reading the scope's
/// placement rather than the schema's.
fn points_columns(body: &[u8]) -> (Vec<String>, BTreeMap<u64, f32>) {
    let frames = tessera_wire::split_frames(body).expect("well-formed frames");
    let mut names: Vec<String> = Vec::new();
    let mut heat = BTreeMap::new();
    for (kind, payload) in frames {
        if kind != tessera_wire::FRAME_POINTS {
            continue;
        }
        let reader =
            arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(payload.to_vec()), None)
                .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            names = batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
            let ids = batch.column(0).clone();
            let ids = ids.as_any().downcast_ref::<UInt64Array>().unwrap().clone();
            if let Some(index) = names.iter().position(|n| n == "heat") {
                let values = batch.column(index).clone();
                let values = values
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .unwrap()
                    .clone();
                for row in 0..batch.num_rows() {
                    heat.insert(ids.value(row), values.value(row));
                }
            }
        }
    }
    (names, heat)
}

/// The source entity a served `tessera_id` stands for, read through the drill-down's
/// `external_id` — the build mints the source id as eight little-endian bytes.
///
/// **Through the API rather than through the bundle**, because the entity id a build assigns is
/// not the source id (they are signature-sorted, §11.1) and the identity permutation is the
/// server's alone (I10). One request per served point; the fixture serves tens.
async fn entity_of(served: &Served, token: &str, id: u64) -> u64 {
    let body: Value = served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/items/{id}")))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body["external_id"].as_str().expect("an external id"))
        .unwrap();
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

/// [`entity_of`] over a whole response, keyed by source entity.
async fn by_entity(
    served: &Served,
    token: &str,
    values: &BTreeMap<u64, f32>,
) -> BTreeMap<u64, f32> {
    let mut out = BTreeMap::new();
    for (&id, &value) in values {
        out.insert(entity_of(served, token, id).await, value);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The placement
// ---------------------------------------------------------------------------------------------

/// **The value reaches the hot tail of each view of the group, and it is that view's own value.**
///
/// Every entity of a quarter is checked against the same function the parquet was written from, so
/// a disagreement is between the served row and the data. The entities in *both* quarters are
/// checked to differ across the two responses, which is what a scope buys and what no
/// bundle-wide column can do.
#[tokio::test]
async fn a_scoped_render_column_reaches_every_view_of_its_group_with_that_views_values() {
    let served = serve().await;
    let mut seen: Vec<BTreeMap<u64, f32>> = Vec::new();
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let (status, body) = viewport_bytes(&served, &served.token, &view).await;
        assert_eq!(status, 200, "{view}");
        let (names, values) = points_columns(&body);
        assert!(
            names.contains(&"heat".to_string()),
            "{view} carries the scoped render column: {names:?}"
        );
        let by_entity = by_entity(&served, &served.token, &values).await;
        assert_eq!(
            by_entity.keys().copied().collect::<Vec<_>>(),
            members(slot).collect::<Vec<_>>(),
            "{view} serves its own rows"
        );
        for (&entity, &value) in &by_entity {
            assert_eq!(
                value,
                heat_on_the_wire(slot, entity),
                "{view}, entity {entity}"
            );
        }
        seen.push(by_entity);
    }
    // The overlap is the point: one entity, two views, two values.
    let overlap: Vec<u64> = members(0).filter(|e| members(1).contains(e)).collect();
    assert!(!overlap.is_empty(), "the two quarters overlap");
    let differing = overlap.iter().filter(|e| seen[0][e] != seen[1][e]).count();
    assert!(
        differing > 0,
        "an entity in both quarters draws with each quarter's own value"
    );
}

/// **A view outside the group gets nothing** — not a column of zeros, not a column of nulls: no
/// column, and no byte in its row space.
///
/// Asserted twice over. On the wire, the plain view's points frame names no `heat`. On disc, its
/// segment is compared with the **same corpus built without the family declared at all**: the
/// declaration must not move a byte of a row space no scope reaches.
#[tokio::test]
async fn a_view_outside_the_group_is_byte_identical_to_a_build_without_the_family() {
    let served = serve().await;
    let (status, body) = viewport_bytes(&served, &served.token, "world").await;
    assert_eq!(status, 200);
    let (names, values) = points_columns(&body);
    assert!(
        !names.contains(&"heat".to_string()),
        "the plain view names no scoped column: {names:?}"
    );
    assert!(values.is_empty());

    let tmp = TempDir::new().unwrap();
    let without = build_bundle(tmp.path(), false);
    // `v00000/partitions/<phash>/views/world/segments/<seg>/` — walked rather than spelt, the
    // partition hash and the segment id being the build's to choose.
    let segment_dir = |root: &Path| {
        let one = |dir: &Path| {
            std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .next()
                .expect("one entry")
                .unwrap()
                .path()
        };
        let phash = one(&root.join("v00000").join("partitions"));
        one(&phash.join("views").join("world").join("segments"))
    };
    // **The listing as well as the bytes.** A presence bitmap is a file *beside* `columns.arrow`
    // (decision 0064), so a lane leaking into a view outside the scope could leave
    // `presence/heat.roaring` behind while the column file itself stayed byte-equal — an artefact
    // the manifest digests and nothing else would notice.
    let listing = |dir: &Path| {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            for entry in std::fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        out.sort();
        out
    };
    let (with, plain) = (segment_dir(&served.bundle), segment_dir(&without));
    assert_eq!(
        listing(&with),
        listing(&plain),
        "declaring a scoped render family leaves no file in a segment it does not reach"
    );
    assert_eq!(
        std::fs::read(with.join("columns.arrow")).unwrap(),
        std::fs::read(plain.join("columns.arrow")).unwrap(),
        "declaring a scoped render family changes no byte of a row space it does not reach"
    );
}

/// **A group sharing the views renders the owner's family** (`views.md` §3.3): the keys are the
/// owner's, so `quarter_map:2026-Q1` carries `quarter`'s column — under its own view id, in its
/// own row order and its own geometry.
#[tokio::test]
async fn a_sharing_group_renders_the_owners_family_under_its_own_views() {
    let served = serve().await;
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter_map:{key}");
        let (status, body) = viewport_bytes(&served, &served.token, &view).await;
        assert_eq!(status, 200, "{view}");
        let (names, values) = points_columns(&body);
        assert!(names.contains(&"heat".to_string()), "{view}: {names:?}");
        assert_eq!(values.len(), members(slot).count(), "{view}");
        for (entity, value) in by_entity(&served, &served.token, &values).await {
            assert_eq!(value, heat_on_the_wire(slot, entity), "{view}, {entity}");
        }
    }
}

/// **An absent value is the type's zero, and the rows that carry one are the fixture's.** The hot
/// column is non-nullable (contracts R4), so this is decision 0064's placeholder rather than a
/// wire null — the same thing an entity-scoped render column's absence is.
#[tokio::test]
async fn an_entity_with_no_value_in_a_view_takes_the_render_placeholder() {
    let served = serve().await;
    let (status, body) = viewport_bytes(&served, &served.token, "quarter:2026-Q1").await;
    assert_eq!(status, 200);
    let (_, values) = points_columns(&body);
    let absent: Vec<u64> = members(0).filter(|&e| heat(0, e).is_none()).collect();
    assert!(!absent.is_empty(), "the fixture has absences to serve");
    for (entity, value) in by_entity(&served, &served.token, &values).await {
        if absent.contains(&entity) {
            assert_eq!(value, 0.0, "entity {entity} carries no value in 2026-Q1");
        } else {
            assert_ne!(heat(0, entity), None);
        }
    }
}

/// **The tail is read per served row, so a masked-out row's value is on no wire at all** (I2, I7).
///
/// A principal holding term `1` alone sees a third of the corpus; the response carries exactly
/// those rows' values and no slot for the rest. This is structural — the gather walks the rows the
/// selection returned — and is asserted rather than assumed.
#[tokio::test]
async fn a_row_the_mask_excludes_carries_no_scoped_value() {
    let served = serve().await;
    let narrow = authorise(&served.server, &["1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = viewport_bytes(&served, &narrow, "quarter:2026-Q1").await;
    assert_eq!(status, 200);
    let (names, values) = points_columns(&body);
    assert!(names.contains(&"heat".to_string()));
    let by_entity = by_entity(&served, &narrow, &values).await;
    let expected: Vec<u64> = members(0).filter(|&e| terms_of(e).contains(&1)).collect();
    assert_eq!(
        by_entity.keys().copied().collect::<Vec<_>>(),
        expected,
        "the mask decides the rows"
    );
    for (entity, value) in by_entity {
        assert_eq!(value, heat_on_the_wire(0, entity));
    }
}

/// **`/v1/meta` says which views the column arrives under**, so a client knows to expect it in a
/// quarter's points batch and not in the plain view's.
#[tokio::test]
async fn meta_publishes_the_render_placement_and_the_views_that_have_a_column() {
    let served = serve().await;
    let body: Value = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let families = body["scoped_scalars"].as_array().unwrap();
    // The fixture's three: the rendered `heat` this test is about, an indexed `text` family, and
    // one carrying neither flag — the last two exist for the two-door and drill-down cases below
    // and are named here so a family appearing or vanishing is a failure rather than a surprise.
    let named: Vec<&str> = families
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(named, vec!["heat", "note", "tag"]);
    let heat = families
        .iter()
        .find(|f| f["name"] == "heat")
        .expect("the rendered family");
    assert_eq!(heat["name"], "heat");
    assert_eq!(heat["arrow_type"], "f32");
    assert_eq!(heat["scope"]["group"], "quarter");
    assert_eq!(heat["render"], true);
    // Render-only, and an operand on that alone since `views.md` §5 r26 — the flag says where the
    // value is drawn, not whether it can be filtered.
    assert_eq!(heat["index"], false);
    // **Every view whose rows carry the column**, the owning group's and the sharing group's
    // alike: a client under `quarter_map:2026-Q1` receives the column and must find that id here.
    assert_eq!(
        heat["views"],
        json!([
            "quarter:2026-Q1",
            "quarter:2026-Q2",
            "quarter_map:2026-Q1",
            "quarter_map:2026-Q2"
        ]),
        "the ids of every view that renders the family, this principal reaching them all"
    );
    // **`render` alone is the operand licence** (`views.md` §5 r26): the entry is the one an
    // indexed family gets — the family's own operator names, and the scope that says a bare leaf
    // needs a view of the group behind it.
    let operand = body["filter_operands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["column"] == "heat")
        .expect("a rendered family is a filter operand")
        .clone();
    assert_eq!(operand["family"], "numeric");
    assert_eq!(operand["operands"], json!(["eq", "in", "range"]));
    assert_eq!(operand["scope"]["group"], "quarter");
}

/// **A view created while the service runs starts with no column of the family and acquires one
/// at its first flush** (`views.md` §5).
///
/// Before the flush the response simply does not name the column and the family's own `views` list
/// does not name the view — ordinary absence, not a refusal and not a column of zeros. After a
/// batch carrying `heat` has flushed, both say the opposite, and the values served are the ones
/// the batch carried. That transition is the whole of what the write half buys: a view minted
/// today draws with its own numbers without a rebuild.
#[tokio::test]
async fn a_view_created_at_runtime_gains_its_scoped_column_at_the_first_flush() {
    let served = serve().await;
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/views/quarter/2026-Q3"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "the view is created");
    // A session resolves its visible views once, so the new view needs a new session.
    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = viewport_bytes(&served, &token, "quarter:2026-Q3").await;
    assert_eq!(status, 200, "an empty view serves");
    let (names, values) = points_columns(&body);
    assert!(
        !names.contains(&"heat".to_string()),
        "a view with no column of the family names none: {names:?}"
    );
    assert!(values.is_empty());

    let meta: Value = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        meta["scoped_scalars"][0]["views"],
        json!([
            "quarter:2026-Q1",
            "quarter:2026-Q2",
            "quarter_map:2026-Q1",
            "quarter_map:2026-Q2"
        ]),
        "the created view is on the roster and not yet on the family's list"
    );

    // ---- and now a batch into it, carrying the family's value under its plain name ----------
    const MINTED: [u64; 2] = [9_101, 9_102];
    ingest_with_heat(
        &served,
        "heat-runtime",
        "quarter:2026-Q3",
        &[
            (external_id_of(MINTED[0]), 250.0, 250.0, "0"),
            (external_id_of(MINTED[1]), 350.0, 350.0, "0"),
        ],
        &[Some(7.5), None],
    )
    .await;
    flush(&served).await;

    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (names, values) = settled_points(&served, &token, "quarter:2026-Q3", 2).await;
    assert!(
        names.contains(&"heat".to_string()),
        "the view now has a column of the family: {names:?}"
    );
    let by_entity = by_entity(&served, &token, &values).await;
    assert_eq!(
        by_entity[&MINTED[0]], 7.5,
        "the value the batch carried is the value the row renders"
    );
    assert_eq!(
        by_entity[&MINTED[1]], 0.0,
        "a row with no value takes the render placeholder, as an absence always has"
    );

    let meta: Value = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let views = meta["scoped_scalars"][0]["views"].as_array().unwrap();
    assert!(
        views.contains(&json!("quarter:2026-Q3")),
        "the flush put the view on the family's list: {views:?}"
    );
}

/// The settled points frames for `view`, once the response holds `expected` rows.
///
/// A flush's publication and a session's sight of what it minted are two events, the second
/// following the first by an asynchronous refresh with no wire signal — so every test here waits
/// for the settled count rather than asserting the first response.
async fn settled_points(
    served: &Served,
    token: &str,
    view: &str,
    expected: usize,
) -> (Vec<String>, BTreeMap<u64, f32>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        // A `429` is the admission gate shedding under machine load (contracts §3.1) and is not
        // the answer under test — ask again, as every other polling test here does.
        let (status, body) = viewport_bytes(served, token, view).await;
        if status == 200 {
            let read = points_columns(&body);
            if read.1.len() == expected {
                return read;
            }
        } else {
            assert_eq!(status, 429, "a served view answers or sheds");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{view} never settled at {expected} rows");
}

/// **A segment the write path produced carries the lane, and its rows carry the values the batch
/// supplied** (`views.md` §5) — the inverse of what this test asserted while the write half was
/// absent.
///
/// A view of the group ends up holding **two** kinds of segment at once and one response gathers
/// across both: the build's rows keep their values and the flushed row carries its own. Nothing
/// about the response's schema changes — the column is the manifest's, not the segment's.
#[tokio::test]
async fn a_flushed_segment_of_a_group_view_serves_the_scoped_value_the_batch_carried() {
    let served = serve().await;
    // An entity the corpus has never seen, ingested into a view of the group.
    const NEW: u64 = 9_001;
    ingest_with_heat(
        &served,
        "heat-flush",
        "quarter:2026-Q1",
        &[(external_id_of(NEW), 250.0, 250.0, "0")],
        &[Some(42.25)],
    )
    .await;
    flush(&served).await;

    let expected = members(0).count() + 1;
    let (names, values) = settled_points(&served, &served.token, "quarter:2026-Q1", expected).await;
    assert_eq!(values.len(), expected, "the flushed row is served");
    assert!(
        names.contains(&"heat".to_string()),
        "the column is the manifest's, so a flushed segment does not take it off the schema: \
         {names:?}"
    );

    let by_entity = by_entity(&served, &served.token, &values).await;
    assert_eq!(
        by_entity[&NEW], 42.25,
        "the flush wrote the lane, and the value in it is the one the batch carried"
    );
    for entity in members(0) {
        assert_eq!(
            by_entity[&entity],
            heat_on_the_wire(0, entity),
            "the build's own rows are untouched by the segment beside them, entity {entity}"
        );
    }
}

/// **The same column on an entity-space batch is still refused, and with the same message**
/// (`views.md` §5).
///
/// That refusal is what makes a scoped column un-nameable outside the views its key addresses:
/// `heat` has no slot in `MANIFEST.declared_scalars` and names no registered layer, so on a plain
/// view it is an undeclared column and nothing else. The admission above is scoped to the families
/// whose owning group's key set holds the named view's key (decision 0116); a plain view holds no
/// key at all, so this path is the one it always was.
#[tokio::test]
async fn a_scoped_column_on_an_entity_space_batch_is_still_refused() {
    let served = serve().await;
    let (status, body) = try_ingest_with_heat(
        &served,
        "heat-plain",
        "world",
        &[(external_id_of(9_201), 250.0, 250.0, "0")],
        &[Some(1.0)],
    )
    .await;
    assert_eq!(status, 422, "an undeclared column is a malformed request");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"], "contract", "{body}");
    assert!(body["detail"].as_str().unwrap().contains("'heat'"), "{body}");
}

/// **A join row carries that view's scoped value, and it is the one thing it carries beyond
/// geometry** (`views.md` §4, §5).
///
/// The entity is already in `2026-Q1`; a second batch puts it in `2026-Q2` with a different
/// number, and each view then draws it with its own. A join's exclusion from every entity-space
/// pass is what makes a *label* on such a row inert — and a scoped value is not entity-space: it
/// belongs to the `(entity, view)` pair the join is creating.
#[tokio::test]
async fn a_join_row_carries_this_views_scoped_value() {
    let served = serve().await;
    const NEW: u64 = 9_301;
    ingest_with_heat(
        &served,
        "join-first",
        "quarter:2026-Q1",
        &[(external_id_of(NEW), 250.0, 250.0, "0")],
        &[Some(11.0)],
    )
    .await;
    flush(&served).await;
    ingest_with_heat(
        &served,
        "join-second",
        "quarter:2026-Q2",
        &[(external_id_of(NEW), 260.0, 260.0, "0")],
        &[Some(22.0)],
    )
    .await;
    flush(&served).await;

    for (slot, value) in [(0usize, 11.0f32), (1, 22.0)] {
        let view = format!("quarter:{}", QUARTERS[slot].0);
        let expected = members(slot).count() + 1;
        let (_, values) = settled_points(&served, &served.token, &view, expected).await;
        let by_entity = by_entity(&served, &served.token, &values).await;
        assert_eq!(
            by_entity[&NEW], value,
            "{view} draws the joined entity with its own scoped value"
        );
    }
}

/// **A fold of a group's view keeps the family's lane, and the values survive it** — the defect
/// this file's `render` work left behind (`views.md` §5, r24).
///
/// A merge and a fold took their writer schema from the bundle-wide render list, which a family
/// has no row in, so a rewritten segment of a group's view carried the entity-scoped tail alone.
/// Values served correctly before the rewrite came back as the type's zero afterwards, which is
/// exactly what an absence looks like — no error anywhere, and nothing in a functional test to
/// notice. The schema is the **view's** now, and this drives it end to end: the build's rows and a
/// flushed row are read before the fold and again after, and both must be unchanged.
#[tokio::test]
async fn a_fold_of_a_group_view_keeps_the_scoped_render_lane() {
    let served = serve().await;
    const NEW: u64 = 9_401;
    ingest_with_heat(
        &served,
        "heat-fold",
        "quarter:2026-Q1",
        &[(external_id_of(NEW), 250.0, 250.0, "0")],
        &[Some(33.5)],
    )
    .await;
    flush(&served).await;

    let expected = members(0).count() + 1;
    let (_, values) = settled_points(&served, &served.token, "quarter:2026-Q1", expected).await;
    let before = by_entity(&served, &served.token, &values).await;
    assert_eq!(before[&NEW], 33.5, "the flushed row's value is served");

    fold(&served).await;

    // A fold rewrites the whole prefix, so a session that authorised against the old one is asking
    // about a bundle that has gone; a fresh session is what a client would have.
    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (names, values) = settled_points(&served, &token, "quarter:2026-Q1", expected).await;
    assert!(
        names.contains(&"heat".to_string()),
        "the folded segment still carries the family's lane: {names:?}"
    );
    let after = by_entity(&served, &token, &values).await;
    assert_eq!(
        after, before,
        "every value survives the rewrite — the build's rows and the flushed one alike"
    );
}

/// Request a compaction fold and block until it has published (`POST /control/compact`,
/// contracts §3.4). The counter is the only "done" there is: the fold runs on its own thread and
/// publishes at the executor's next loop iteration, so the acceptance code says nothing about
/// completion.
async fn fold(served: &Served) {
    let before = served.server.state.engine.write_executor_stats().folds;
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "a fold is accepted at any time");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let stats = served.server.state.engine.write_executor_stats();
        assert_eq!(
            stats.fold_failures, 0,
            "the fold failed rather than publishing"
        );
        if stats.folds > before {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published: {} folds, {} discarded",
            stats.folds,
            stats.fold_failures
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// **A key created again carries none of its predecessor's scoped values** (`views.md` §5,
/// decision 0115).
///
/// The same entity holds a value in the first incarnation of `quarter:2026-Q3`, the key is dropped
/// and created again, and the entity rejoins the new view with no value of its own. Its row
/// renders the placeholder before the fold, and after the fold that rewrites the family's column
/// it renders the placeholder and answers no leaf over the family — the dead incarnation's extents
/// being listed under the same view id the live one writes into.
///
/// The leaf and the drill-down are asked the same question at each of the three states the
/// values can be read from: the columns this process holds after the recreated view's flush, the
/// extents a restart composes from the side-manifest, and the folded column.
///
/// A second entity carries a value in the new incarnation, which is what gives it a column of the
/// family at all; its own value is checked, so the column is being read rather than missing.
#[tokio::test]
async fn a_recreated_view_adopts_no_scoped_value_of_its_predecessor() {
    let served = serve().await;
    const REJOINS: u64 = 9_901;
    const FRESH: u64 = 9_902;
    /// Create the key, which is a `201` whether or not it has been held before.
    async fn create(served: &Served) {
        let resp = served
            .server
            .client
            .put(served.server.control_url("/control/views/quarter/2026-Q3"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201, "the view is created");
    }
    create(&served).await;
    // A session resolves its visible views once, so every read below takes a fresh one.
    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = try_ingest_families(
        &served,
        "heat-first-incarnation",
        "quarter:2026-Q3",
        &[(external_id_of(REJOINS), 250.0, 250.0, "0")],
        // Above the threshold, so a value adopted by the next incarnation answers the leaf.
        Some(&[Some(90.0)]),
        Some(&[Some("peregrine")]),
        None,
    )
    .await;
    assert_eq!(status, 200, "the batch is accepted: {body}");
    flush(&served).await;
    let (_, values) = settled_points(&served, &token, "quarter:2026-Q3", 1).await;
    assert_eq!(
        by_entity(&served, &token, &values).await[&REJOINS],
        90.0,
        "the first incarnation renders the value its batch carried"
    );

    let resp = served
        .server
        .client
        .delete(served.server.control_url("/control/views/quarter/2026-Q3"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the drop is accepted");
    create(&served).await;
    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = try_ingest_families(
        &served,
        "heat-second-incarnation",
        "quarter:2026-Q3",
        &[
            (external_id_of(REJOINS), 250.0, 250.0, "0"),
            (external_id_of(FRESH), 350.0, 350.0, "0"),
        ],
        Some(&[None, Some(5.0)]),
        Some(&[None, Some("linnet")]),
        None,
    )
    .await;
    assert_eq!(status, 200, "the batch is accepted: {body}");
    flush(&served).await;

    /// The rendered rows of the new incarnation: the rejoining entity's placeholder and the
    /// fresh entity's own value.
    async fn rendered(served: &Served, token: &str, rejoins: u64, fresh: u64) {
        let (names, values) = settled_points(served, token, "quarter:2026-Q3", 2).await;
        assert!(
            names.contains(&"heat".to_string()),
            "the new incarnation has a column of the family: {names:?}"
        );
        let by_entity = by_entity(served, token, &values).await;
        assert_eq!(
            by_entity[&rejoins], 0.0,
            "the rejoining entity takes the render placeholder"
        );
        assert_eq!(by_entity[&fresh], 5.0, "and the new value is served");
    }
    rendered(&served, &token, REJOINS, FRESH).await;

    /// What the recreated view owes about the value its predecessor held: the leaf matches the
    /// rejoining entity nowhere, and the drill-down serves it no value under the key — while the
    /// fresh entity's own value is served, so an empty answer is an absence and not a missing
    /// column.
    async fn adopts_nothing(served: &Served, token: &str, rejoins: u64, fresh: u64, stage: &str) {
        let id = id_of(served, token, "quarter:2026-Q3", rejoins).await;
        let body = item(served, token, id).await;
        let keys: Vec<String> = body["scoped"]["heat"]
            .as_object()
            .map(|heat| heat.keys().cloned().collect())
            .unwrap_or_default();
        assert!(
            !keys.contains(&"2026-Q3".to_string()),
            "{stage}: the drill-down serves the rejoining entity no value under the key: {body}"
        );
        let id = id_of(served, token, "quarter:2026-Q3", fresh).await;
        let body = item(served, token, id).await;
        assert_eq!(
            body["scoped"]["heat"]["2026-Q3"],
            json!(5.0),
            "{stage}: and the new incarnation's own value is served: {body}"
        );
        assert_eq!(
            filtered_entities(served, token, "quarter:2026-Q3", range("heat")).await,
            BTreeSet::new(),
            "{stage}: the predecessor's value answers no leaf under the recreated view"
        );
        assert_eq!(
            filtered_entities(
                served,
                token,
                "quarter:2026-Q3",
                json!({ "note": {"match": "peregrine"} })
            )
            .await,
            BTreeSet::new(),
            "{stage}: nor does the predecessor's text"
        );
        assert_eq!(
            filtered_entities(
                served,
                token,
                "quarter:2026-Q3",
                json!({ "note": {"match": "linnet"} })
            )
            .await,
            BTreeSet::from([fresh]),
            "{stage}: while the new incarnation's own text matches"
        );
    }
    adopts_nothing(&served, &token, REJOINS, FRESH, "in process").await;

    // The same two answers after a restart, which composes the extents the side-manifest names
    // rather than the columns this process holds.
    let Served { server, bundle, _tmp, .. } = served;
    server.shutdown().await;
    let server = spawn_server(
        &bundle,
        &_tmp.path().join("cache"),
        &_tmp.path().join("wal.log"),
    )
    .await;
    let token = authorise(&server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let served = Served {
        server,
        token: token.clone(),
        bundle,
        _tmp,
    };
    rendered(&served, &token, REJOINS, FRESH).await;
    adopts_nothing(&served, &token, REJOINS, FRESH, "after a restart").await;

    fold(&served).await;
    // A fold rewrites the whole prefix, so a session authorised against the old one is asking
    // about a bundle that has gone.
    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    rendered(&served, &token, REJOINS, FRESH).await;
    assert_eq!(
        filtered_entities(&served, &token, "quarter:2026-Q3", range("heat")).await,
        BTreeSet::new(),
        "and the folded column holds no value of the predecessor to answer the leaf"
    );
    adopts_nothing(&served, &token, REJOINS, FRESH, "after the fold").await;
}

/// **A flush through a view that borrows the owner's scoped columns writes artifacts the owner
/// view's incarnation carries** (`views.md` §3.3, §5, decision 0115).
///
/// The sharing group declares [`BORROWED_KEY`] and the owning group acquires it while the service
/// runs, so the two views of the key sit at different incarnations. A batch through the borrowing
/// view writes its scoped extents under the owner's name and incarnation; stamped with the
/// flushing view's own, a reopen compares the stamp against the owner view's and skips the extent,
/// and a restart of the same bundle answers where the live generation answered.
#[tokio::test]
async fn a_borrowing_views_scoped_values_survive_a_restart() {
    let served = serve().await;
    const OWNED_ENTITY: u64 = 9_801;
    const BORROWED_ENTITY: u64 = 9_802;
    let owner = format!("quarter:{BORROWED_KEY}");
    let borrower = format!("quarter_map:{BORROWED_KEY}");
    let resp = served
        .server
        .client
        .put(
            served
                .server
                .control_url(&format!("/control/views/quarter/{BORROWED_KEY}")),
        )
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "the owner's view of the key is created"
    );
    // The owner's own batch first: it is what gives the families their columns for this key, so
    // the batch through the borrowing view below writes an extent onto a base that is there.
    let (status, body) = try_ingest_families(
        &served,
        "borrowed-owner",
        &owner,
        &[(external_id_of(OWNED_ENTITY), 250.0, 250.0, "0")],
        Some(&[Some(90.0)]),
        Some(&[Some("peregrine")]),
        None,
    )
    .await;
    assert_eq!(status, 200, "the owner's batch is accepted: {body}");
    flush(&served).await;
    let (status, body) = try_ingest_families(
        &served,
        "borrowed-sharing",
        &borrower,
        &[(external_id_of(BORROWED_ENTITY), 350.0, 350.0, "0")],
        Some(&[Some(80.0)]),
        Some(&[Some("linnet")]),
        None,
    )
    .await;
    assert_eq!(status, 200, "the borrowing view's batch is accepted: {body}");
    flush(&served).await;

    /// What the borrowing view owes about the value its own batch carried: the leaf, the text
    /// match and the drill-down, each over the column the owner's view names.
    async fn answers(served: &Served, borrower: &str, entity: u64, stage: &str) {
        let token = authorise(&served.server, &["0", "1"]).await["token"]
            .as_str()
            .unwrap()
            .to_string();
        let rows = (BORROWED.end - BORROWED.start) as usize + 1;
        settled_points(served, &token, borrower, rows).await;
        assert_eq!(
            filtered_entities(served, &token, borrower, range("heat")).await,
            BTreeSet::from([entity]),
            "{stage}: the value the borrowing view's batch carried answers the leaf"
        );
        assert_eq!(
            filtered_entities(
                served,
                &token,
                borrower,
                json!({ "note": {"match": "linnet"} })
            )
            .await,
            BTreeSet::from([entity]),
            "{stage}: and its text matches"
        );
        let id = id_of(served, &token, borrower, entity).await;
        assert_eq!(
            item(served, &token, id).await["scoped"]["heat"][BORROWED_KEY],
            json!(80.0),
            "{stage}: and the drill-down serves it under the key"
        );
    }
    answers(&served, &borrower, BORROWED_ENTITY, "in process").await;

    // The same answers after a restart, which composes the extents the side-manifest names rather
    // than the columns this process holds.
    let Served {
        server,
        bundle,
        _tmp,
        ..
    } = served;
    server.shutdown().await;
    let server = spawn_server(
        &bundle,
        &_tmp.path().join("cache"),
        &_tmp.path().join("wal.log"),
    )
    .await;
    let token = authorise(&server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let served = Served {
        server,
        token,
        bundle,
        _tmp,
    };
    answers(&served, &borrower, BORROWED_ENTITY, "after a restart").await;
}

/// **A principal who reaches a sharing group's view and not the owner's is told the truth about
/// both** (`views.md` §3.3, §6). `quarter:2026-Q2` is gated and `quarter_map:2026-Q2` is not, so
/// for a principal holding neither term the column arrives under an id the family's own
/// `views` list does not name — and the list must say so, or a client reading it would conclude
/// the column it is being served does not exist.
///
/// The gated id is absent from the same list on the same test the roster is filtered by: it is a
/// view this principal cannot reach, and this list must not become the one place the document
/// names it.
#[tokio::test]
async fn a_sharing_groups_view_is_listed_where_the_owners_gated_one_is_not() {
    let served = serve().await;
    // Term `0` alone: every entity carries it, so this principal's mask is the whole corpus and
    // what it cannot reach is a *view* rather than an item. `quarter:2026-Q2`'s gate names term
    // `1`, which it does not hold, and every `quarter_map` view is public.
    let outsider = authorise(&served.server, &["0"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let meta: Value = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let heat = &meta["scoped_scalars"][0];
    assert_eq!(heat["render"], true);
    assert_eq!(
        heat["views"],
        json!([
            "quarter:2026-Q1",
            "quarter_map:2026-Q1",
            "quarter_map:2026-Q2"
        ]),
        "the sharing group's ids are listed; the gated owner view is not"
    );

    // And the list is not a promise about a column that fails to arrive: the id it names under the
    // sharing group serves the gated quarter's own values.
    let (status, body) = viewport_bytes(&served, &outsider, "quarter_map:2026-Q2").await;
    assert_eq!(status, 200);
    let (names, values) = points_columns(&body);
    assert!(names.contains(&"heat".to_string()), "{names:?}");
    for (entity, value) in by_entity(&served, &outsider, &values).await {
        assert_eq!(value, heat_on_the_wire(1, entity), "entity {entity}");
    }

    // The owner's own view stays unreachable, and its 404 is the one an unknown name gets.
    let (status, _) = viewport_bytes(&served, &outsider, "quarter:2026-Q2").await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------------------------
// The filter surface `render` alone licences (`views.md` §5 r26)
// ---------------------------------------------------------------------------------------------

/// The threshold every `range` below uses. Chosen so each quarter's matching set is a proper,
/// non-empty subset of its population — a filter holding everything or nothing would pass against
/// the wrong column as readily as the right one.
const THRESHOLD: f64 = 30.0;

/// The entities of a quarter whose `heat` clears the threshold — the expected answer, from the
/// same function the parquet was written from rather than from a second reading of the rule.
fn matching(slot: usize) -> BTreeSet<u64> {
    members(slot)
        .filter(|&e| heat(slot, e).is_some_and(|v| f64::from(v) >= THRESHOLD))
        .collect()
}

/// A `range` over `heat`, spelt as a client would: bare, or pinned to a view by key.
fn range(leaf: &str) -> Value {
    json!({ leaf: {"range": {"gte": THRESHOLD}} })
}

/// One filtered viewport, with its status — the refusal cases are assertions too.
async fn filtered_bytes(
    served: &Served,
    token: &str,
    view: &str,
    filters: Value,
) -> (u16, Vec<u8>) {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "filters": filters
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.bytes().await.unwrap().to_vec())
}

/// The **entities** a filtered view answers with. An identifier is the entity's wherever it
/// appears (`views.md` §1), so two views' answers compare directly.
async fn filtered_entities(
    served: &Served,
    token: &str,
    view: &str,
    filters: Value,
) -> BTreeSet<u64> {
    let (status, body) = filtered_bytes(served, token, view, filters).await;
    assert_eq!(status, 200, "{view} answers under a filter");
    let (_, values) = points_columns(&body);
    let mut out = BTreeSet::new();
    for &id in values.keys() {
        out.insert(entity_of(served, token, id).await);
    }
    out
}

/// **A bare leaf under a view of the group answers from that view's column, on `render` alone.**
///
/// `heat` is declared `index = false`, so the operand exists because the family is rendered and
/// for no other reason (`views.md` §5 r26). Both quarters are checked against their own expected
/// sets, and the two sets are checked to differ — reading the family's first column wherever the
/// leaf resolves would pass one assertion and fail the other.
#[tokio::test]
async fn a_render_only_family_answers_a_bare_leaf_under_a_view_of_its_group() {
    let served = serve().await;
    let mut answers = Vec::new();
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        // The gated quarter is reachable: this session holds both terms.
        let view = format!("quarter:{key}");
        let answer = filtered_entities(&served, &served.token, &view, range("heat")).await;
        assert_eq!(answer, matching(slot), "{view} answers its own column");
        assert!(
            answer.len() < members(slot).count(),
            "{view}: the threshold must exclude something, or the column is not being read"
        );
        answers.push(answer);
    }
    assert_ne!(
        answers[0], answers[1],
        "the two quarters disagree, which is what makes reading the right column observable"
    );
}

/// **A pin reads the named view's column and not the lane in front of the request.**
///
/// Under `quarter:2026-Q2`, whose rows carry Q2's `heat` in their tail, `heat@2026-Q1` answers
/// Q1's column: the entities Q2 holds that clear the threshold **in Q1**. That is route (a)'s
/// property — the operand is the family's per-view entity-space column, so a leaf naming another
/// view is read where that view's values live rather than from the rows in front of the request,
/// which hold different numbers. The answer is asserted to differ from Q2's own, which is what a
/// route through the lane would have returned.
///
/// (The pin from a view outside the group entirely is `scoped_filter.rs`'s
/// `a_pin_projects_one_views_column_into_another_views_rows`, which this file's plain view cannot
/// carry: `world` serves no points in this fixture.)
#[tokio::test]
async fn a_pin_of_a_render_only_family_reads_the_named_views_column() {
    let served = serve().await;
    let answer = filtered_entities(
        &served,
        &served.token,
        "quarter:2026-Q2",
        range("heat@2026-Q1"),
    )
    .await;
    let expected: BTreeSet<u64> = matching(0)
        .intersection(&members(1).collect())
        .copied()
        .collect();
    assert!(
        !expected.is_empty(),
        "the fixture's quarters must overlap above the threshold, or this says nothing"
    );
    assert_eq!(answer, expected, "the pinned column is Q1's");
    let own = filtered_entities(&served, &served.token, "quarter:2026-Q2", range("heat")).await;
    assert_ne!(
        answer, own,
        "and it is not Q2's own answer, which the row tail in front of the request holds"
    );
}

/// **A bare leaf where nothing decides the view is the `422` naming the group**, and a pin naming
/// no view of it is the unknown-view `404` — the same two answers an indexed family gives, since
/// the resolution is one site and the licence is all that changed.
#[tokio::test]
async fn a_render_only_familys_leaf_takes_the_same_refusals_an_indexed_ones_does() {
    let served = serve().await;
    let (status, body) = filtered_bytes(&served, &served.token, "world", range("heat")).await;
    assert_eq!(status, 422, "a bare leaf on a plain view decides nothing");
    let detail = String::from_utf8_lossy(&body).to_string();
    assert!(
        detail.contains("quarter"),
        "the refusal names the group: {detail}"
    );
    let (status, _) = filtered_bytes(&served, &served.token, "world", range("heat@2029-Q9")).await;
    assert_eq!(
        status, 404,
        "a pin naming no view of the group is the unknown-view 404"
    );
}

/// **An ingested value is filterable once flushed, and stays so across a fold** (`views.md` §5).
///
/// The write half writes the family's per-view extent for a rendered family exactly as for an
/// indexed one — the same predicate gates the opener, the flush and the fold — so a row that
/// arrived by ingest answers the leaf that the build's rows answer, before and after the rewrite.
#[tokio::test]
async fn an_ingested_value_of_a_render_only_family_filters_after_a_flush_and_a_fold() {
    let served = serve().await;
    const NEW: u64 = 9_501;
    ingest_with_heat(
        &served,
        "heat-filter",
        "quarter:2026-Q1",
        &[(external_id_of(NEW), 250.0, 250.0, "0")],
        // Above the threshold, so the answer changes by exactly this entity.
        &[Some(90.0)],
    )
    .await;
    flush(&served).await;
    // Let the flushed row settle into the served generation before the set is compared.
    settled_points(
        &served,
        &served.token,
        "quarter:2026-Q1",
        members(0).count() + 1,
    )
    .await;

    let mut expected = matching(0);
    expected.insert(NEW);
    let answer = filtered_entities(&served, &served.token, "quarter:2026-Q1", range("heat")).await;
    assert_eq!(answer, expected, "the flushed extent answers the leaf");

    fold(&served).await;
    // A fold rewrites the whole prefix, so a session authorised against the old one is asking
    // about a bundle that has gone.
    let token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    settled_points(&served, &token, "quarter:2026-Q1", members(0).count() + 1).await;
    let after = filtered_entities(&served, &token, "quarter:2026-Q1", range("heat")).await;
    assert_eq!(
        after, expected,
        "and the fold's rewritten column answers it too"
    );
}

// ---------------------------------------------------------------------------------------------
// The two doors (decision 0116)
// ---------------------------------------------------------------------------------------------

/// **A sharing group's view writes the owner's cell, and both groups read it back**
/// (`views.md` §5, decision 0116).
///
/// A scoped value's address is `(attribute → its group, key)`. `quarter_map` declares `members` of
/// `quarter`, so its `2026-Q1` view holds the owner's key and addresses the same cell — a batch
/// through that door writes it. Until 2026-09-01 the write half admitted the owning group's views
/// alone, and this batch's `heat` column was refused as undeclared.
///
/// What is asserted is the cell's identity rather than the door's: the value written through
/// `quarter_map:2026-Q1` is answered by a leaf under `quarter:2026-Q1`, whose rows are a different
/// row space entirely, and by one under `quarter_map:2026-Q1` beside it.
#[tokio::test]
async fn a_sharing_groups_door_writes_the_cell_the_owners_view_addresses() {
    let served = serve().await;
    const NEW: u64 = 9_401;
    // Above the fixture's threshold, so the leaf below separates it from the absent rows.
    const VALUE: f32 = 77.5;
    ingest_with_heat(
        &served,
        "sharing-door",
        "quarter_map:2026-Q1",
        &[(external_id_of(NEW), 250.0, 250.0, "0")],
        &[Some(VALUE)],
    )
    .await;
    // The same entity joins the owner's own view carrying **no** value: the cell is already
    // written, and a join that omits a family's column names nothing to disagree with.
    ingest_with_heat(
        &served,
        "sharing-door-owner",
        "quarter:2026-Q1",
        &[(external_id_of(NEW), 260.0, 260.0, "0")],
        &[None],
    )
    .await;
    flush(&served).await;

    for view in ["quarter:2026-Q1", "quarter_map:2026-Q1"] {
        let answer = filtered_entities(&served, &served.token, view, range("heat")).await;
        assert!(
            answer.contains(&NEW),
            "{view} reads the cell the sharing door wrote: {answer:?}"
        );
    }
    // And the lane the sharing door's own row carries is the value it supplied, not the absence a
    // view that could not write the family used to take.
    let expected = members(0).count() + 1;
    let (_, values) = settled_points(&served, &served.token, "quarter_map:2026-Q1", expected).await;
    let by_entity = by_entity(&served, &served.token, &values).await;
    assert_eq!(
        by_entity[&NEW], VALUE,
        "the sharing group's row renders the value its own batch carried"
    );
}

/// **One cell, one value: an identical second write dedupes and a differing one is a 409**
/// (`views.md` §5, decision 0116).
///
/// This is what replaces the old one-door rule's argument. Two views of one key can both name the
/// cell, so the writer settles it: the same value is dropped from the second row — one claimant, so
/// the extents stay disjoint in entity space — and a different value is refused naming the column
/// and the key, before the WAL append, whole batch without effect.
///
/// The refusal names neither group. A caller writing through `quarter_map` learns that the key
/// already holds a value, which is its own request measured against the schema, and nothing about
/// who owns the family.
#[tokio::test]
async fn a_second_door_naming_one_cell_dedupes_an_equal_value_and_refuses_a_different_one() {
    let served = serve().await;
    const AGREES: u64 = 9_501;
    const DISAGREES: u64 = 9_502;
    const VALUE: f32 = 88.25;
    ingest_with_heat(
        &served,
        "cell-first",
        "quarter:2026-Q1",
        &[
            (external_id_of(AGREES), 250.0, 250.0, "0"),
            (external_id_of(DISAGREES), 251.0, 251.0, "0"),
        ],
        &[Some(VALUE), Some(VALUE)],
    )
    .await;

    // The same value through the other door: accepted, and the second copy is not written.
    ingest_with_heat(
        &served,
        "cell-agrees",
        "quarter_map:2026-Q1",
        &[(external_id_of(AGREES), 300.0, 300.0, "0")],
        &[Some(VALUE)],
    )
    .await;

    // A different one: refused, naming the column and the key.
    let (status, body) = try_ingest_with_heat(
        &served,
        "cell-disagrees",
        "quarter_map:2026-Q1",
        &[(external_id_of(DISAGREES), 301.0, 301.0, "0")],
        &[Some(VALUE + 1.0)],
    )
    .await;
    assert_eq!(status, 409, "one cell holds one value: {body}");
    assert!(
        body.contains("group-scoped column 'heat'") && body.contains("key '2026-Q1'"),
        "the refusal names the column and the key: {body}"
    );
    assert!(
        !body.contains("quarter_map") && !body.contains("group 'quarter'"),
        "and names no group: {body}"
    );

    flush(&served).await;
    // The deduped write left one value behind, and both views answer with it.
    for view in ["quarter:2026-Q1", "quarter_map:2026-Q1"] {
        let answer = filtered_entities(&served, &served.token, view, range("heat")).await;
        assert!(
            answer.contains(&AGREES),
            "{view} answers the one value the cell holds: {answer:?}"
        );
    }
}

/// A cell filled through the values route and not yet flushed is held, so a joining row naming
/// it dedupes an equal value and is refused a different one, and the flush that follows publishes.
#[tokio::test]
async fn a_join_naming_a_cell_a_pending_fill_holds_dedupes_or_is_refused() {
    use base64::Engine as _;
    let served = serve().await;
    const AGREES: u64 = 9_601;
    const DISAGREES: u64 = 9_602;
    const VALUE: f32 = 5.0;
    ingest_with_heat(
        &served,
        "pending-first",
        "quarter:2026-Q1",
        &[
            (external_id_of(AGREES), 260.0, 260.0, "0"),
            (external_id_of(DISAGREES), 261.0, 261.0, "0"),
        ],
        &[None, None],
    )
    .await;
    flush(&served).await;

    let id = |e: u64| base64::engine::general_purpose::STANDARD.encode(external_id_of(e));
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "pending-fill")
        .header("x-tessera-view", "quarter:2026-Q1")
        .json(&json!([
            {"external_id": id(AGREES), "tag": VALUE},
            {"external_id": id(DISAGREES), "tag": VALUE},
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the fill is accepted");

    let (status, body) = try_ingest_families(
        &served,
        "pending-agrees",
        "quarter_map:2026-Q1",
        &[(external_id_of(AGREES), 310.0, 310.0, "0")],
        None,
        None,
        Some(&[Some(VALUE)]),
    )
    .await;
    assert_eq!(status, 200, "an equal value joins: {body}");
    let (status, body) = try_ingest_families(
        &served,
        "pending-disagrees",
        "quarter_map:2026-Q1",
        &[(external_id_of(DISAGREES), 311.0, 311.0, "0")],
        None,
        None,
        Some(&[Some(VALUE + 1.0)]),
    )
    .await;
    assert_eq!(status, 409, "one cell holds one value: {body}");

    let failures = served.server.state.engine.write_executor_stats().flush_failures;
    flush(&served).await;
    assert_eq!(
        served.server.state.engine.write_executor_stats().flush_failures,
        failures,
        "the flush publishes the cell once"
    );
}

/// **A cell of a family declaring neither flag holds its value too** (`views.md` §5, decision
/// 0116).
///
/// `tag` is neither indexed nor rendered, so no filter and no tile reads it — but the build writes
/// its value column and the drill-down serves it, so the cell holds a value and the write path has
/// one to compare against. A second write naming a different value is refused as the rendered
/// family's is ([`a_second_door_naming_one_cell_dedupes_an_equal_value_and_refuses_a_different_one`]);
/// one naming the value held is deduped.
///
/// The door is the values route rather than a joining row, because a built cell cannot be reached
/// by one: every view of its key already holds the entity's row, and a second row in a view is
/// refused before any cell is read.
#[tokio::test]
async fn a_written_cell_of_an_unflagged_family_dedupes_or_is_refused() {
    let served = serve().await;
    // An entity the build placed in `2026-Q1`, whose `tag` the build wrote: slot 0, so `e * 2`.
    const CELL: u64 = 4;
    const HELD: f32 = (CELL * 2) as f32;

    let (status, body) = fill(&served, "tag-agrees", "quarter:2026-Q1", CELL, HELD).await;
    assert_eq!(status, 200, "the value the cell holds is deduped: {body}");
    assert_eq!(body["filled"], json!(0), "and nothing is written: {body}");

    let (status, body) = fill(&served, "tag-differs", "quarter:2026-Q1", CELL, HELD + 1.0).await;
    assert_eq!(status, 409, "one cell holds one value: {body}");

    // The cell still holds what the build wrote, neither write having reached it.
    let id = id_of(&served, &served.token, "quarter:2026-Q1", CELL).await;
    let body = item(&served, &served.token, id).await;
    assert_eq!(
        body["scoped"]["tag"]["2026-Q1"],
        json!(f64::from(HELD)),
        "the cell keeps the value the build wrote: {body}"
    );
}

/// One `tag` value for one entity through `POST /control/values`, with its status and body.
async fn fill(
    served: &Served,
    batch_id: &str,
    view: &str,
    entity: u64,
    tag: f32,
) -> (u16, Value) {
    use base64::Engine as _;
    let id = base64::engine::general_purpose::STANDARD.encode(external_id_of(entity));
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .json(&json!([{ "external_id": id, "tag": tag }]))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// **A `text` cell that has flushed takes no second value through either door** (`views.md` §5,
/// decision 0116; review finding F1).
///
/// The cell arm compares a supplied value with the stored one and deduplicates or refuses. It
/// cannot do that for prose once the value has flushed: a text column stores a token dictionary,
/// positional postings and a presence bitmap, and no value per entity to read back. Admitting the
/// row anyway would write a **second text layer stamped with the same view** — text layers have no
/// coverage check, their disjointness having rested on I9, which two doors onto one cell
/// invalidated — and `match` unions across them, so both sets of words would answer under one
/// column with no symptom anywhere.
///
/// So occupancy is asked instead of equality and the answer is the same either way: **a supplied
/// string is refused whether it agrees with the stored prose or not**, because agreement is exactly
/// what cannot be established. Omitting the column passes, and leaves the cell as it stands.
#[tokio::test]
async fn a_flushed_text_cell_refuses_a_second_value_equal_or_not() {
    let served = serve().await;
    const A: u64 = 9_601;
    const B: u64 = 9_602;
    const C: u64 = 9_603;
    let prose = "the stored prose for this cell";

    // Door one writes the cell, and it flushes.
    let (status, body) = try_ingest_families(
        &served,
        "text-first",
        "quarter:2026-Q1",
        &[
            (external_id_of(A), 250.0, 250.0, "0"),
            (external_id_of(B), 251.0, 251.0, "0"),
            (external_id_of(C), 252.0, 252.0, "0"),
        ],
        None,
        Some(&[Some(prose), Some(prose), Some(prose)]),
        None,
    )
    .await;
    assert_eq!(status, 200, "the first door writes the cell: {body}");
    flush(&served).await;

    // Door two, differing prose: refused.
    let (status, body) = try_ingest_families(
        &served,
        "text-differs",
        "quarter_map:2026-Q1",
        &[(external_id_of(A), 300.0, 300.0, "0")],
        None,
        Some(&[Some("different prose entirely")]),
        None,
    )
    .await;
    assert_eq!(status, 409, "a differing string is refused: {body}");
    assert!(
        body.contains("group-scoped column 'note'") && body.contains("key '2026-Q1'"),
        "the refusal names the column and the key: {body}"
    );

    // Door two, the *same* prose: refused too, and the message says why.
    let (status, equal_body) = try_ingest_families(
        &served,
        "text-equal",
        "quarter_map:2026-Q1",
        &[(external_id_of(B), 301.0, 301.0, "0")],
        None,
        Some(&[Some(prose)]),
        None,
    )
    .await;
    assert_eq!(
        status, 409,
        "an equal string is refused as well, equality being unverifiable: {equal_body}"
    );
    assert_eq!(
        equal_body, body,
        "one rule, one message: agreement is not something this arm can establish, so it cannot \
         answer differently for it"
    );

    // Door two, omitting the column: accepted, and the cell stands as it was.
    let (status, body) = try_ingest_families(
        &served,
        "text-absent",
        "quarter_map:2026-Q1",
        &[(external_id_of(C), 302.0, 302.0, "0")],
        None,
        Some(&[None]),
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "a null names no value to disagree with: {body}"
    );
}

/// **In one window the buffer answers, so text compares exactly** (`views.md` §5, decision 0116).
///
/// The refusal above is a property of the *flushed* cell and of nothing else. While the first
/// door's row is still in the commit-window buffer its value is right there to compare, so the two
/// ordinary answers hold: an equal string deduplicates and a differing one is the 409. Without this
/// the fail-closed arm above would read as the rule for text rather than as the cost of a flush.
#[tokio::test]
async fn a_same_window_text_cell_still_dedupes_and_refuses_exactly() {
    let served = serve().await;
    const AGREES: u64 = 9_701;
    const DISAGREES: u64 = 9_702;
    let prose = "prose still sitting in the buffer";

    let (status, body) = try_ingest_families(
        &served,
        "text-window-first",
        "quarter:2026-Q2",
        &[
            (external_id_of(AGREES), 250.0, 250.0, "0"),
            (external_id_of(DISAGREES), 251.0, 251.0, "0"),
        ],
        None,
        Some(&[Some(prose), Some(prose)]),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // No flush between: the first door's rows are in the buffer.
    let (status, body) = try_ingest_families(
        &served,
        "text-window-equal",
        "quarter_map:2026-Q2",
        &[(external_id_of(AGREES), 300.0, 300.0, "0")],
        None,
        Some(&[Some(prose)]),
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "the buffer holds the value, so an equal string deduplicates: {body}"
    );

    let (status, body) = try_ingest_families(
        &served,
        "text-window-differs",
        "quarter_map:2026-Q2",
        &[(external_id_of(DISAGREES), 301.0, 301.0, "0")],
        None,
        Some(&[Some("other prose")]),
        None,
    )
    .await;
    assert_eq!(status, 409, "and a differing one is the 409: {body}");
    assert!(
        body.contains("group-scoped column 'note'") && body.contains("key '2026-Q2'"),
        "naming the column and the key: {body}"
    );
}

/// **A family carrying neither `index` nor `render` gets its extents from a flush** (`views.md` §5).
///
/// Such a family is stored and served at the drill-down without being searchable or drawn (owner
/// ruling). The flush gated its per-view extents on the *filter* licence, so it wrote none: the
/// family served the build's values and nothing ingested since, silently, because a column with no
/// extent for a batch reads exactly as a batch that carried no value.
///
/// The gate is now the **value column**, and this drives the consequence end to end: an ingested
/// value's extent is written, composed into the live generation, and folded like any other. A fold
/// is the assertion that binds it — it rewrites every column the manifest names, so a base and an
/// extent the fold did not know about is the failure this test would have caught before the fix
/// (and did, while the two predicates disagreed).
///
/// ⊘ The drill-down's own assertion belongs with the branch that serves these families; what is
/// proved here is that the values are on disc, in the manifest, and survive a rewrite.
#[tokio::test]
async fn a_neither_flag_family_gains_its_column_from_a_flush_and_keeps_it_through_a_fold() {
    let served = serve().await;
    const NEW: u64 = 9_801;
    let (status, body) = try_ingest_families(
        &served,
        "tag-write",
        "quarter:2026-Q1",
        &[(external_id_of(NEW), 250.0, 250.0, "0")],
        None,
        None,
        Some(&[Some(1234.5)]),
    )
    .await;
    assert_eq!(status, 200, "a neither-flag column is nameable: {body}");
    flush(&served).await;

    // The view is on the family's list, which is what the opener and the drill-down walk.
    let document: Value = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let views_of = |name: &str| -> Vec<String> {
        document["scoped_scalars"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .expect("the family is published")["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        views_of("tag").contains(&"quarter:2026-Q1".to_string()),
        "the flush gave the view a column of the neither-flag family: {:?}",
        views_of("tag")
    );

    // And the fold rewrites it rather than leaving the layers behind. A fold that did not know
    // about this column would refuse, which is exactly how the missing half of this fix surfaced.
    fold(&served).await;
    assert!(
        views_of("tag").contains(&"quarter:2026-Q1".to_string()),
        "and the fold keeps it"
    );
    let (status, _) = viewport_bytes(&served, &served.token, "quarter:2026-Q1").await;
    assert!(
        status == 200 || status == 429,
        "the view still answers after the rewrite, or sheds: {status}"
    );
}

// The drill-down: every view the point is in, and every scoped value, that this principal may see
// ---------------------------------------------------------------------------------------------

/// Every `tessera_id` a view's points frames carry, in the order they arrive.
fn served_ids(body: &[u8]) -> Vec<u64> {
    let frames = tessera_wire::split_frames(body).expect("well-formed frames");
    let mut out = Vec::new();
    for (kind, payload) in frames {
        if kind != tessera_wire::FRAME_POINTS {
            continue;
        }
        let reader =
            arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(payload.to_vec()), None)
                .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let ids = batch.column(0).clone();
            let ids = ids.as_any().downcast_ref::<UInt64Array>().unwrap().clone();
            for row in 0..batch.num_rows() {
                out.push(ids.value(row));
            }
        }
    }
    out
}

/// The `tessera_id` one source entity is served under, found through the drill-down's external id
/// — the identity permutation is the server's alone (I10), so a test cannot compute one.
async fn id_of(served: &Served, token: &str, view: &str, entity: u64) -> u64 {
    let (status, body) = viewport_bytes(served, token, view).await;
    assert_eq!(status, 200, "{view}");
    for id in served_ids(&body) {
        if entity_of(served, token, id).await == entity {
            return id;
        }
    }
    panic!("entity {entity} is not served in {view}");
}

/// `POST /v1/items/{id}`'s body.
async fn item(served: &Served, token: &str, id: u64) -> Value {
    served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/items/{id}")))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Where a view puts an entity, in that view's own grid units — the quantiser the build applied,
/// against the frame every view of this fixture declares.
fn expected_position(view: &str, e: u64) -> (u32, u32) {
    let (x, y) = position(view, e);
    let f = extent();
    (
        tessera_spatial::fixed32(x, f.x_min, f.x_max),
        tessera_spatial::fixed32(y, f.y_min, f.y_max),
    )
}

/// The views entity `e` holds a row in, sorted — the array the drill-down owes a principal who can
/// reach all of them.
fn views_of(e: u64) -> Vec<String> {
    let mut out = Vec::new();
    if WORLD.contains(&e) {
        out.push("world".to_string());
    }
    for (key, range) in QUARTERS.iter() {
        if range.contains(&e) {
            out.push(format!("quarter:{key}"));
            out.push(format!("quarter_map:{key}"));
        }
    }
    out.sort();
    out
}

/// **The drill-down names every view the point is in, with the position that view puts it at**
/// (contracts §3.2, owner ruling 2026-09-01).
///
/// The entity is in five views across three rosters — the plain view, both quarters, and both of
/// the sharing group's layouts over the same keys — and each places it somewhere different. So a
/// response that served one position for the point, or the same position five times, fails here
/// rather than passing on the count: a position is a fact about a **view**, its frame and its
/// projection being the view's own (decision 0040).
#[tokio::test]
async fn the_drill_down_names_every_view_the_point_is_in_with_its_position_there() {
    let served = serve().await;
    // In the plain view and in both quarters, so the sharing group's two layouts hold it as well.
    const ENTITY: u64 = 10;
    let id = id_of(&served, &served.token, "world", ENTITY).await;
    let body = item(&served, &served.token, id).await;

    let names: Vec<String> = body["views"]
        .as_array()
        .expect("the views array")
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, views_of(ENTITY), "every view, sorted by id");

    for view in body["views"].as_array().unwrap() {
        let name = view["id"].as_str().unwrap();
        let (x, y) = expected_position(name, ENTITY);
        assert_eq!(
            (view["x"].as_u64().unwrap(), view["y"].as_u64().unwrap()),
            (x as u64, y as u64),
            "{name} places the point where its own points file put it"
        );
    }
    // And the positions differ, which is the whole reason the array is per view rather than one
    // position beside the record.
    let distinct: BTreeSet<(u64, u64)> = body["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["x"].as_u64().unwrap(), v["y"].as_u64().unwrap()))
        .collect();
    assert!(
        distinct.len() > 1,
        "the layouts differ, so the served positions must: {distinct:?}"
    );
}

/// **The scoped values reach the drill-down keyed by the group's key** (`views.md` §5, decision
/// 0113): one entry per key the principal may reach, and the value is that key's own.
///
/// `heat` is declared `render = true, index = false`, so what is read here is the entity-space
/// column beside the row tail rather than the tail — the same column a pinned leaf resolves to.
/// The two keys carry different values for one entity, which is what a scope buys: a response
/// serving the same number twice would be reading a bundle-wide column under a scoped name.
#[tokio::test]
async fn the_drill_down_serves_the_scoped_values_keyed_by_the_groups_key() {
    let served = serve().await;
    const ENTITY: u64 = 10;
    let id = id_of(&served, &served.token, "world", ENTITY).await;
    let body = item(&served, &served.token, id).await;

    let served_heat = body["scoped"]["heat"]
        .as_object()
        .expect("the scoped family, by name");
    let expected: BTreeMap<String, f64> = QUARTERS
        .iter()
        .enumerate()
        .filter_map(|(slot, (key, range))| {
            range
                .contains(&ENTITY)
                .then(|| heat(slot, ENTITY).map(|v| (key.to_string(), v as f64)))
                .flatten()
        })
        .collect();
    let served_values: BTreeMap<String, f64> = served_heat
        .iter()
        .map(|(key, value)| (key.clone(), value.as_f64().unwrap()))
        .collect();
    assert_eq!(
        served_values, expected,
        "one entry per key, and each key's own value"
    );
    // A key the sharing group holds is one key, not two: the value belongs to the `(entity, key)`
    // pair and the column is the owning group's.
    assert_eq!(served_values.len(), 2);
}

/// **A view the gate refuses is named nowhere on the drill-down, and its key is served only if
/// another roster reaches it** (`views.md` §5, §6, §3.3).
///
/// The principal holds term `0`, which every entity of this fixture carries — so nothing here is
/// about item visibility — and fails `quarter:2026-Q2`'s gate, which is term `1`. What they must
/// see: no `quarter:2026-Q2` in `views`, and the key `2026-Q2` in `scoped` all the same, because
/// `quarter_map:2026-Q2` is public, is a view of that same key, and is already in their roster.
/// The value is not a second fact about a view they cannot reach; it is the value of a key they
/// can.
#[tokio::test]
async fn a_gate_failed_view_is_absent_from_the_drill_down_and_its_key_is_not() {
    let served = serve().await;
    let narrow = authorise(&served.server, &["0"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    const ENTITY: u64 = 10;
    let id = id_of(&served, &narrow, "world", ENTITY).await;
    let body = item(&served, &narrow, id).await;

    let names: Vec<String> = body["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();
    let mut expected = views_of(ENTITY);
    expected.retain(|v| v != "quarter:2026-Q2");
    assert_eq!(
        names, expected,
        "the gated view is absent exactly as a view nobody declared is"
    );
    // The gate is the only thing withheld: the same request under the full token names it.
    assert!(item(&served, &served.token, id).await["views"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == "quarter:2026-Q2"));

    let keys: Vec<String> = body["scoped"]["heat"]
        .as_object()
        .expect("the family is reachable through the sharing group")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        vec!["2026-Q1".to_string(), "2026-Q2".to_string()],
        "the key is a view's address, and `quarter_map:2026-Q2` holds it"
    );
}

/// A group-scoped value is addressed by its view's key, so a fill waiting for the tick when that
/// view is dropped goes with it: the drop says how many, and none of them holds the log.
#[tokio::test]
async fn a_scoped_fill_goes_with_its_dropped_view_and_the_drop_counts_it() {
    use base64::Engine as _;
    let served = serve().await;
    // An entity ingested and published, so the cell this fills is empty and nothing else is
    // buffered. A built cell holds the build's own value, which the fill would be refused for.
    const FRESH: u64 = 9_801;
    ingest_with_heat(
        &served,
        "fill-then-drop-row",
        "quarter:2026-Q1",
        &[(external_id_of(FRESH), 270.0, 270.0, "0")],
        &[None],
    )
    .await;
    flush(&served).await;
    let id = base64::engine::general_purpose::STANDARD.encode(external_id_of(FRESH));
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "fill-then-drop")
        .header("x-tessera-view", "quarter:2026-Q1")
        .json(&json!([{ "external_id": id, "tag": 5.0 }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the fill is accepted");

    let resp = served
        .server
        .client
        .delete(served.server.control_url("/control/views/quarter/2026-Q1"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the drop is accepted");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["fills_dropped"], json!(1), "{body}");
    assert_eq!(
        served.server.state.engine.generation().buffer.oldest_wal_pos(),
        None,
        "nothing buffered holds the log"
    );

    let Served { server, _tmp, bundle, .. } = served;
    server.shutdown().await;
    let server = spawn_server(&bundle, &_tmp.path().join("cache"), &_tmp.path().join("wal.log")).await;
    assert_eq!(
        server.state.engine.generation().buffer.oldest_wal_pos(),
        None,
        "and a restart does not buffer the fill again"
    );
}

// ---------------------------------------------------------------------------------------------
// One column through the whole lifecycle
// ---------------------------------------------------------------------------------------------

/// One row this test writes: the view its batch names, and the three families' values it carries.
struct Written {
    entity: u64,
    view: &'static str,
    heat: f32,
    tag: f32,
    note: &'static str,
}

/// The owning group's door onto `2026-Q1`.
const OWNER: &str = "quarter:2026-Q1";
/// The sharing group's door onto the same key, a different row space over the same cells.
const SHARING: &str = "quarter_map:2026-Q1";
/// A second key of the group, created while the service runs.
const RUNTIME: &str = "quarter:2026-Q3";

/// One entity per batch and one flush per batch, the three views interleaved so two views'
/// extents of one family alternate in the manifest. Each view carries values on both sides of
/// [`THRESHOLD`], so a `range` that answered from the wrong column would answer the wrong set
/// rather than the whole population.
const WRITTEN: [Written; 9] = [
    Written { entity: 9_001, view: OWNER, heat: 100.0, tag: 11.0, note: "kestrel" },
    Written { entity: 9_002, view: SHARING, heat: 101.0, tag: 12.0, note: "marlin" },
    Written { entity: 9_003, view: RUNTIME, heat: 102.0, tag: 13.0, note: "ibex" },
    Written { entity: 9_004, view: OWNER, heat: 2.5, tag: 14.0, note: "gannet" },
    Written { entity: 9_005, view: SHARING, heat: 3.5, tag: 15.0, note: "dipper" },
    Written { entity: 9_006, view: RUNTIME, heat: 4.5, tag: 16.0, note: "vole" },
    Written { entity: 9_007, view: OWNER, heat: 103.0, tag: 17.0, note: "osprey" },
    Written { entity: 9_008, view: SHARING, heat: 104.0, tag: 18.0, note: "quoll" },
    Written { entity: 9_009, view: RUNTIME, heat: 105.0, tag: 19.0, note: "teal" },
];

/// The key a view id addresses.
fn key_of(view: &str) -> &str {
    view.split_once(':').expect("a view of a group").1
}

/// The slot of the build's own quarter behind `view`, where it has one — `2026-Q3` is minted at
/// runtime and the build wrote no row of it.
fn built_slot(view: &str) -> Option<usize> {
    QUARTERS.iter().position(|(key, _)| *key == key_of(view))
}

/// The entities `view` serves once every row below has flushed.
fn population(view: &str) -> BTreeSet<u64> {
    let built = built_slot(view).into_iter().flat_map(members);
    built
        .chain(WRITTEN.iter().filter(|w| w.view == view).map(|w| w.entity))
        .collect()
}

/// The value the family's column for `key` holds for `entity`, or `None` where it holds none —
/// what a leaf pinned to that key is answered from, whichever view the request names. The two
/// doors onto a key write the same cell, so a row written through either is here.
fn scoped_heat(key: &str, entity: u64) -> Option<f32> {
    if let Some(written) = WRITTEN
        .iter()
        .find(|w| key_of(w.view) == key && w.entity == entity)
    {
        return Some(written.heat);
    }
    let slot = QUARTERS.iter().position(|(k, _)| *k == key)?;
    members(slot)
        .contains(&entity)
        .then(|| heat(slot, entity))
        .flatten()
}

/// What `heat` renders as for `entity` under `view`.
fn rendered_heat(view: &str, entity: u64) -> f32 {
    match WRITTEN
        .iter()
        .find(|w| w.view == view && w.entity == entity)
    {
        Some(written) => written.heat,
        None => heat_on_the_wire(built_slot(view).expect("a built view"), entity),
    }
}

/// A fresh session, which is what a client holds after a publication it did not authorise against.
async fn session(served: &Served) -> String {
    authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Everything the three families owe under every view they were written to, at one stage of the
/// lifecycle: the rendered value, the two leaves, and the drill-down's per-key values. `tag`
/// carries neither flag, so the drill-down is its whole surface and no leaf names it.
async fn serves_everything(served: &Served, token: &str, stage: &str) {
    for view in [OWNER, SHARING, RUNTIME] {
        let rows = population(view);
        let (names, values) = settled_points(served, token, view, rows.len()).await;
        assert!(
            names.contains(&"heat".to_string()),
            "{stage}, {view}: the rendered family has a column here: {names:?}"
        );
        let by_entity = by_entity(served, token, &values).await;
        assert_eq!(
            by_entity.keys().copied().collect::<BTreeSet<_>>(),
            rows,
            "{stage}, {view}: the rows served"
        );
        for (&entity, &value) in &by_entity {
            assert_eq!(
                value,
                rendered_heat(view, entity),
                "{stage}, {view}, entity {entity}: the rendered value"
            );
        }

        let expected: BTreeSet<u64> = rows
            .iter()
            .copied()
            .filter(|&e| f64::from(rendered_heat(view, e)) >= THRESHOLD)
            .collect();
        assert_eq!(
            filtered_entities(served, token, view, range("heat")).await,
            expected,
            "{stage}, {view}: the range answers this view's column"
        );

        // The same leaf pinned to the owning group's first key: the operand is the family's
        // column for that key wherever the request is made, so a view holding rows with no cell
        // of it answers with none of them.
        let pinned_key = QUARTERS[0].0;
        let expected: BTreeSet<u64> = rows
            .iter()
            .copied()
            .filter(|&e| scoped_heat(pinned_key, e).is_some_and(|v| f64::from(v) >= THRESHOLD))
            .collect();
        assert_eq!(
            filtered_entities(served, token, view, range(&format!("heat@{pinned_key}"))).await,
            expected,
            "{stage}, {view}: the pinned leaf answers the named key's column"
        );

        for written in WRITTEN.iter() {
            // A token of another view's row is in another row space, and a token of the other key
            // is in another column: either way the answer here is empty.
            let expected = if written.view == view {
                BTreeSet::from([written.entity])
            } else {
                BTreeSet::new()
            };
            let leaf = json!({ "note": {"match": written.note} });
            assert_eq!(
                filtered_entities(served, token, view, leaf).await,
                expected,
                "{stage}, {view}: `{}` matches the rows that hold it",
                written.note
            );
        }
    }

    for written in WRITTEN.iter() {
        let key = key_of(written.view);
        let id = id_of(served, token, written.view, written.entity).await;
        let body = item(served, token, id).await;
        for (family, value) in [("heat", written.heat), ("tag", written.tag)] {
            let entity = written.entity;
            let served_keys: Vec<String> = body["scoped"][family]
                .as_object()
                .unwrap_or_else(|| panic!("{stage}: entity {entity} has {family}: {body}"))
                .keys()
                .cloned()
                .collect();
            assert_eq!(
                served_keys,
                vec![key.to_string()],
                "{stage}, entity {}: {family} under its own key alone",
                written.entity
            );
            assert_eq!(
                body["scoped"][family][key],
                json!(f64::from(value)),
                "{stage}, entity {}: {family}'s value under {key}",
                written.entity
            );
        }
    }
}

/// **One group-scoped column serves the same values at every step of the lifecycle**
/// (`views.md` §5): a flush per extent, a coalesce over the stack, a restart that composes the
/// side-manifest, a fold that rewrites the column, and a restart over what the fold wrote.
///
/// Three views write into two keys of one group — the owning group's door, the sharing group's
/// door onto the same cells, and a key minted while the service runs — so each stage reads a
/// manifest in which two views' extents of one family interleave. What is asserted at each is
/// everything the families owe a client: the rendered value in the row tail, the `range` the
/// rendered family licences, the `match` the indexed text family licences, and the drill-down's
/// values under the key that holds them and no other.
#[tokio::test]
async fn a_scoped_column_serves_the_same_values_through_flush_coalesce_fold_and_restart() {
    // Width two, so a column holding three extents is eligible: the pass is the subject, not its
    // policy.
    let config = || EngineConfig {
        coalesce_width: Some(2),
        ..default_engine_config()
    };
    let served = serve_with(config()).await;
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/views/quarter/2026-Q3"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "the second key is created");

    // Held off while the extents accumulate, so the pass below runs over all of them at once
    // rather than partly on the executor's own clock.
    served.server.state.engine.set_coalesce_for_test(false);
    for (round, written) in WRITTEN.iter().enumerate() {
        let (status, body) = try_ingest_families(
            &served,
            &format!("lifecycle-{round}"),
            written.view,
            &[(external_id_of(written.entity), 250.0 + round as f32, 250.0, "0")],
            Some(&[Some(written.heat)]),
            Some(&[Some(written.note)]),
            Some(&[Some(written.tag)]),
        )
        .await;
        assert_eq!(status, 200, "round {round} is accepted: {body}");
        flush(&served).await;
    }

    let token = session(&served).await;
    serves_everything(&served, &token, "after the flushes").await;

    // ---- the coalesce, on one pulled tick ----------------------------------------------------
    let before = served.server.state.engine.write_executor_stats().coalesces;
    served.server.state.engine.set_coalesce_for_test(true);
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.server.state.engine.write_executor_stats().coalesces == before {
        assert!(
            std::time::Instant::now() < deadline,
            "no coalesce published over three extents per column at width two"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let token = session(&served).await;
    serves_everything(&served, &token, "after a coalesce").await;

    // ---- a restart, which composes the extents the side-manifest names ------------------------
    let served = reopen(served, config()).await;
    serves_everything(&served, &served.token, "after a restart").await;

    // ---- the fold, which rewrites every column the manifest names -----------------------------
    fold(&served).await;
    let token = session(&served).await;
    serves_everything(&served, &token, "after a fold").await;

    // ---- and a restart over what the fold wrote -----------------------------------------------
    let served = reopen(served, config()).await;
    serves_everything(&served, &served.token, "after a second restart").await;
}

/// Stop the server and open the same bundle, cache and log again, under the same configuration.
async fn reopen(served: Served, config: EngineConfig) -> Served {
    let Served { server, bundle, _tmp, .. } = served;
    server.shutdown().await;
    let server = spawn_server_with_config(
        &bundle,
        &_tmp.path().join("cache"),
        &_tmp.path().join("wal.log"),
        config,
    )
    .await;
    let token = authorise(&server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    Served { server, token, bundle, _tmp }
}
