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

mod common;

use std::collections::BTreeMap;
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
use tessera_spatial::tiler::ScalarType;

const ENTITIES: u64 = 24;
/// The plain view holds the first twenty; each quarter holds its own slice, so the two quarters
/// overlap on a run of entities and each of those draws twice, with two values.
const WORLD: std::ops::Range<u64> = 0..20;
const QUARTERS: [(&str, std::ops::Range<u64>); 2] = [("2026-Q1", 0..15), ("2026-Q2", 8..24)];

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
    }
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        visibility: None,
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Fields::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

fn roster() -> Vec<GroupViewDescriptor> {
    QUARTERS
        .iter()
        .map(|(key, _)| GroupViewDescriptor {
            key: key.to_string(),
            visibility: None,
            metadata: Default::default(),
        })
        .collect()
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
    let mut views = vec![view_args("world", &world_points, &pairs)];
    let mut family_views = Vec::new();
    for (slot, (key, range)) in QUARTERS.iter().enumerate() {
        let id = format!("quarter:{key}");
        let points = dir.join(format!("quarter-{key}.parquet"));
        write_points(&points, &id, range.clone(), Some(slot));
        family_views.push(views.len());
        views.push(view_args(&id, &points, &pairs));
    }
    for (slot, (key, range)) in QUARTERS.iter().enumerate() {
        let id = format!("quarter_map:{key}");
        let points = dir.join(format!("map-{key}.parquet"));
        write_points(&points, &id, range.clone(), Some(slot));
        views.push(view_args(&id, &points, &pairs));
    }
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![
            GroupDescriptor {
                visibility: None,
                name: "quarter".to_string(),
                members_of: None,
                views: roster(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
            GroupDescriptor {
                visibility: None,
                name: "quarter_map".to_string(),
                // The same keys, a second layout: a family over these views belongs to the group
                // that owns them, and renders under both.
                members_of: Some("quarter".to_string()),
                views: roster(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
        ],
        scoped_attributes: match declared {
            // **`render` without `index`**: the family is on no filter surface at all and has one
            // home, the hot tail, which is exactly the placement this file is about.
            true => vec![ScopedColumnFamily {
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
                views: family_views,
                source: None,
            }],
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
    let tmp = TempDir::new().unwrap();
    let bundle = build_bundle(tmp.path(), true);
    let server = spawn_server(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
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
async fn by_entity(served: &Served, token: &str, values: &BTreeMap<u64, f32>) -> BTreeMap<u64, f32> {
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
    let differing = overlap
        .iter()
        .filter(|e| seen[0][e] != seen[1][e])
        .count();
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
    // `v00000/partitions/<phash>/views/world/segments/<seg>/columns.arrow` — walked rather than
    // spelt, the partition hash and the segment id being the build's to choose.
    let segment = |root: &Path| {
        let one = |dir: &Path| {
            std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .next()
                .expect("one entry")
                .unwrap()
                .path()
        };
        let phash = one(&root.join("v00000").join("partitions"));
        let seg = one(&phash.join("views").join("world").join("segments"));
        std::fs::read(seg.join("columns.arrow")).unwrap()
    };
    assert_eq!(
        segment(&served.bundle),
        segment(&without),
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
    assert_eq!(families.len(), 1);
    let heat = &families[0];
    assert_eq!(heat["name"], "heat");
    assert_eq!(heat["arrow_type"], "f32");
    assert_eq!(heat["scope"]["group"], "quarter");
    assert_eq!(heat["render"], true);
    // Render-only: on no filter surface, so it is on this list and on no other.
    assert_eq!(heat["index"], false);
    assert_eq!(
        heat["views"],
        json!(["quarter:2026-Q1", "quarter:2026-Q2"]),
        "the views that have a column, in the owning group's own ids"
    );
    assert!(
        !body["filter_operands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["column"] == "heat"),
        "`render` alone is not an operand for a scoped family — a scoped column has no row route"
    );
}

/// **A view created while the service runs has no column of the family, and serving says so by
/// omission** (`views.md` §5): no batch can carry a scoped value — a buffered row's scalars are
/// positional against `declared_scalars`, which a family is deliberately absent from — so the
/// column is written by a build or not at all, and the new view's response simply does not name
/// it. Ordinary absence, not a refusal and not a column of zeros.
#[tokio::test]
async fn a_view_created_at_runtime_renders_no_scoped_column() {
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
        json!(["quarter:2026-Q1", "quarter:2026-Q2"]),
        "the created view is on the roster and not on the family's list"
    );
}
