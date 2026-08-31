//! **A multi-view bundle, served** (`views.md` §3.2): the roster on the wire, and every view of it
//! answering a viewer verb.
//!
//! What is at stake here is not the build — `tessera-build`'s own tests cover that a declaration
//! becomes nine row spaces — but the half above it: whether a client that reads `/v1/meta` can
//! name any of them, order a group's views without interpreting a key, and get *that view's*
//! geometry back. Four facts carry it, and each is a way the serving half could be wrong while
//! every build test still passed:
//!
//! - **The roster is published, in creation order.** A client offering previous-and-next walks
//!   the group's own list, there being no number to sort by (decision 0113); a list in some other
//!   order is a picker that walks a corpus's quarters out of sequence.
//! - **A group's view is a view.** `quarter:2026-Q2` on `/v1/viewport` answers with that view's
//!   own layout, not the plain view's — the failure the shared entity below would show is a
//!   server that resolved every request to the first declared view and drew one map for all nine.
//! - **Two layouts over one key set are one membership and two geometries** (`views.md` §3.3),
//!   which is what `quarter_alt` exists to check: the same entities, in different places.
//! - **Unknown is one 404.** A name nobody declared and a well-formed key no view holds must be
//!   the same answer, or the difference between them is an existence oracle over the roster.
//!
//! The bundle is built here rather than from `test_corpora/multiview` because that fixture's
//! positions are sampled from another rung's built points; the shapes asserted below are the
//! declaration's, and a synthetic corpus states them without the data dependency.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupMetadataField, GroupViewDescriptor, Quantisation,
    ViewArgs, ViewMetadataType, ViewMetadataValue,
};

const ENTITIES: u64 = 30;
/// The plain view holds the first twenty; each quarter holds its own slice, overlapping the plain
/// view and each other. Every quarter shares entities 10..15 with `world`, which is where the
/// two-geometries assertion below takes its shared entity from.
const WORLD: std::ops::Range<u64> = 0..20;
const QUARTERS: [(&str, &str, std::ops::Range<u64>); 4] = [
    ("2026-Q1", "Q1 2026", 0..15),
    ("2026-Q2", "Q2 2026", 10..30),
    ("2026-Q3", "Q3 2026", 5..25),
    ("2026-Q4", "Q4 2026", 8..28),
];

/// A view's own layout: the same entity sits somewhere different in each, which is the whole point
/// of a second view. The three families are deliberately unlike one another — a shared entity's
/// Morton code differs between any two of them.
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

fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| position(view, e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| position(view, e).1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// [`extent`] in the manifest's own shape — one frame for a group, which is what its own copy
/// records so a view created while the service runs has one to take.
fn group_frame() -> Quantisation {
    let e = extent();
    Quantisation {
        x_min: e.x_min,
        x_max: e.x_max,
        y_min: e.y_min,
        y_max: e.y_max,
    }
}

fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        visibility: None,
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

/// One microsecond timestamp per quarter boundary — the roster metadata's `timestamp_us` values,
/// stated here so the wire can be compared against a number this file wrote. `slot` is this
/// file's own index into [`QUARTERS`] and nothing the service knows about.
fn starts_us(slot: u32) -> i64 {
    1_767_225_600_000_000 + i64::from(slot) * 7_776_000_000_000
}

/// The nine-view bundle: one plain view, one group of four with metadata, and a second group over
/// the same four keys with its own layout (`views.md` §3.1, §3.3).
fn build_multiview(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_points(&world_points, "world", WORLD);
    let mut views = vec![view_args("world", &world_points, &pairs)];
    for group in ["quarter", "quarter_alt"] {
        for (key, _, members) in QUARTERS {
            let id = format!("{group}:{key}");
            let points = dir.join(format!("{group}-{key}.parquet"));
            write_points(&points, &id, members);
            views.push(view_args(&id, &points, &pairs));
        }
    }
    let roster = |with_metadata: bool| {
        QUARTERS
            .iter()
            .enumerate()
            .map(|(slot, (key, label, _))| GroupViewDescriptor {
                key: key.to_string(),
                visibility: None,
                // A `members` group declares no metadata: the keys and the values belong to the
                // group that owns them (`views.md` §3.3).
                metadata: if with_metadata {
                    [
                        (
                            "label".to_string(),
                            ViewMetadataValue::Text(label.to_string()),
                        ),
                        (
                            "starts".to_string(),
                            ViewMetadataValue::TimestampUs(starts_us(slot as u32)),
                        ),
                    ]
                    .into_iter()
                    .collect()
                } else {
                    Default::default()
                },
            })
            .collect::<Vec<_>>()
    };
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        // `world` is the declared anchor: within a signature group, ids are ordered by the Morton
        // code an item holds *there* (decision 0112).
        anchor: 0,
        groups: vec![
            GroupDescriptor {
                // Declared on one group and not the other, so `/v1/meta` is asked both questions.
                title: Some("Quarters".to_string()),
                visibility: None,
                name: "quarter".to_string(),
                members_of: None,
                scoped_scalars: Vec::new(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                // The declared metadata schema, which is what a view created while the service
                // runs is measured against (`views.md` §3.2).
                metadata: vec![
                    GroupMetadataField {
                        name: "label".to_string(),
                        ty: ViewMetadataType::Text,
                        vocabulary: None,
                    },
                    GroupMetadataField {
                        name: "starts".to_string(),
                        ty: ViewMetadataType::TimestampUs,
                        vocabulary: None,
                    },
                ],
                views: roster(true),
            },
            GroupDescriptor {
                title: None,
                visibility: None,
                name: "quarter_alt".to_string(),
                members_of: Some("quarter".to_string()),
                scoped_scalars: Vec::new(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                // A `members` group declares none: they belong to the group that owns the views.
                metadata: Vec::new(),
                views: roster(false),
            },
        ],
        scoped_attributes: Vec::new(),
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
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a nine-view build succeeds");
    out
}

struct Served {
    server: TestServer,
    token: String,
    _tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_multiview(tmp.path());
    let server = spawn_server(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    // Both terms, so every entity is visible and what a view answers with is its population
    // rather than this principal's slice of it.
    let auth = authorise(&server, &["0", "1"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    Served {
        server,
        token,
        _tmp: tmp,
    }
}

async fn meta(served: &Served) -> Value {
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

/// `/v1/viewport` over the whole extent, at a zoom fine enough that two layouts do not collapse
/// into one cell. Returns `(tessera_id, code)` per point.
async fn viewport(served: &Served, view: &str) -> reqwest::Response {
    served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&served.token)
        .json(&json!({
            "view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap()
}

async fn points(served: &Served, view: &str) -> Vec<PointRow> {
    let resp = viewport(served, view).await;
    assert_eq!(resp.status().as_u16(), 200, "a served view answers: {view}");
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points
}

/// The `Meta` schema `docs/openapi/tessera.yaml` publishes, compiled in its own context so
/// `$ref`s resolve as they do in the file — `tests/openapi.rs`' own mechanism, reused here because
/// the roster's non-null branch exists only under a multi-view bundle and that file's fixture has
/// one view.
fn meta_schema() -> jsonschema::Validator {
    let doc: Value = serde_yaml_ng::from_str(include_str!("../../../docs/openapi/tessera.yaml"))
        .expect("tessera.yaml parses as YAML");
    jsonschema::draft202012::new(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/components/schemas/Meta",
        "components": doc["components"].clone(),
    }))
    .expect("the Meta schema compiles")
}

/// **The roster on the wire** (`views.md` §3.2): nine views, plain first and then each group's in
/// creation order, every group view carrying its key, group and typed metadata — and no number,
/// the key being a view's only address (decision 0113) — with a `groups` structure a client can
/// walk without reading a key.
#[tokio::test]
async fn meta_publishes_every_view_and_its_roster_in_creation_order() {
    let served = serve().await;
    let body = meta(&served).await;
    let errors: Vec<String> = meta_schema()
        .iter_errors(&body)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "the published description accepts a multi-view document:\n  {}",
        errors.join("\n  ")
    );
    let views = body["views"].as_array().unwrap();

    let ids: Vec<&str> = views.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        vec![
            "world",
            "quarter:2026-Q1",
            "quarter:2026-Q2",
            "quarter:2026-Q3",
            "quarter:2026-Q4",
            "quarter_alt:2026-Q1",
            "quarter_alt:2026-Q2",
            "quarter_alt:2026-Q3",
            "quarter_alt:2026-Q4",
        ],
        "the plain view in manifest order, then each group's views in creation order: {body}"
    );

    // The plain view has no roster record, and says so in the same spelling the fields beside it
    // use for absence.
    let world = &views[0];
    for field in ["group", "key", "metadata"] {
        assert_eq!(world[field], Value::Null, "a plain view has no {field}");
    }

    // Every group view carries the whole record, and the metadata is typed.
    let q2 = views
        .iter()
        .find(|v| v["id"] == "quarter:2026-Q2")
        .expect("the group's second view");
    assert_eq!(q2["group"], "quarter");
    assert_eq!(q2["key"], "2026-Q2");
    assert!(
        q2.get("ordinal").is_none(),
        "a roster record carries no ordinal: {q2}"
    );
    assert_eq!(
        q2["metadata"],
        json!({
            "label": {"type": "text", "value": "Q2 2026"},
            "starts": {"type": "timestamp_us", "value": starts_us(1)},
        }),
        "one typed value per declared metadata name: {q2}"
    );

    // A `members` group's views carry keys — they are the owner's — and no metadata.
    let alt = views
        .iter()
        .find(|v| v["id"] == "quarter_alt:2026-Q2")
        .expect("the sharing group's view");
    assert_eq!(alt["group"], "quarter_alt");
    assert_eq!(alt["key"], "2026-Q2");
    assert_eq!(alt["metadata"], json!({}));

    // **Previous-and-next without interpreting a key**: the groups list is the order, and its
    // entries are the ids a request names.
    assert_eq!(
        body["groups"],
        json!([
            {
                "name": "quarter",
                // The declared title, served; `null` on the group that declared none.
                "title": "Quarters",
                "members_of": Value::Null,
                "views": [
                    "quarter:2026-Q1", "quarter:2026-Q2", "quarter:2026-Q3", "quarter:2026-Q4"
                ],
            },
            {
                "name": "quarter_alt",
                "title": Value::Null,
                "members_of": "quarter",
                "views": [
                    "quarter_alt:2026-Q1", "quarter_alt:2026-Q2", "quarter_alt:2026-Q3",
                    "quarter_alt:2026-Q4"
                ],
            },
        ]),
        "{body}"
    );
}

/// **A group's view is a view**: it answers a viewer verb, and it answers with its own layout.
/// The shared entity is the observable — one identity, two positions (`views.md` §1).
#[tokio::test]
async fn a_groups_view_answers_with_its_own_geometry() {
    let served = serve().await;
    let world = points(&served, "world").await;
    let quarter = points(&served, "quarter:2026-Q2").await;
    assert_eq!(world.len(), (WORLD.end - WORLD.start) as usize);
    assert_eq!(
        quarter.len(),
        (QUARTERS[1].2.end - QUARTERS[1].2.start) as usize
    );

    let codes = |rows: &[PointRow]| -> std::collections::HashMap<u64, u64> {
        rows.iter().copied().collect()
    };
    let (world_codes, quarter_codes) = (codes(&world), codes(&quarter));
    let shared: Vec<u64> = world_codes
        .keys()
        .filter(|id| quarter_codes.contains_key(id))
        .copied()
        .collect();
    assert!(
        !shared.is_empty(),
        "the fixture overlaps `world` and `quarter:2026-Q2`, or this proves nothing"
    );
    for id in shared {
        assert_ne!(
            world_codes[&id], quarter_codes[&id],
            "entity {id} sits in one place in `world` and another in `quarter:2026-Q2`"
        );
    }
}

/// **Two layouts over one key set** (`views.md` §3.3): `quarter_alt:2026-Q2` holds exactly the
/// entities `quarter:2026-Q2` does, drawn somewhere else.
#[tokio::test]
async fn a_sharing_group_serves_one_membership_in_two_geometries() {
    let served = serve().await;
    let owner = points(&served, "quarter:2026-Q2").await;
    let sharing = points(&served, "quarter_alt:2026-Q2").await;

    let ids = |rows: &[PointRow]| -> std::collections::BTreeSet<u64> {
        rows.iter().map(|(id, _)| *id).collect()
    };
    assert_eq!(
        ids(&owner),
        ids(&sharing),
        "the same key is the same membership in both groups"
    );
    let codes = |rows: &[PointRow]| -> std::collections::HashMap<u64, u64> {
        rows.iter().copied().collect()
    };
    let (owner_codes, sharing_codes) = (codes(&owner), codes(&sharing));
    assert!(
        owner_codes
            .iter()
            .any(|(id, code)| sharing_codes[id] != *code),
        "two layouts over one key set are two geometries"
    );
}

/// **A key addresses its view on both planes** (`views.md` §3.2, decision 0113): `<group>:<key>`
/// is the whole of a view's address, and one namespace answers the viewer plane and the ingest
/// plane — a second resolution would eventually disagree about what a name means.
#[tokio::test]
async fn a_key_addresses_its_view_on_both_planes() {
    let served = serve().await;
    assert!(
        !points(&served, "quarter:2026-Q2").await.is_empty(),
        "the group's second view answers by key"
    );
    // The ingest plane resolves the same id — a batch naming it is not a 404.
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "key-address")
        .header("x-tessera-view", "quarter:2026-Q2")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&[(
            Some(&external_id_of(9_001)),
            42.0,
            42.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "the ingest plane resolves the same id: {}",
        resp.text().await.unwrap()
    );
}

/// **Unknown is one 404, whatever shape the name has.** A name nobody declared, a well-formed key
/// no view holds and an id in the retired `#<ordinal>` form — which addresses nothing at all now
/// (decision 0113) — are the same answer with the same detail; the difference between them would
/// be an existence oracle over the roster.
#[tokio::test]
async fn an_unknown_view_and_an_absent_key_are_the_same_404() {
    let served = serve().await;
    // A group is not a view either (`views.md` §3.1): naming one is the same 404 as naming
    // nothing, because it has no row space to answer from.
    for view in ["no_such_view", "quarter:2099-Q9", "quarter:#99", "quarter"] {
        let resp = viewport(&served, view).await;
        assert_eq!(resp.status().as_u16(), 404, "{view} is not a served view");
        let body: Value = resp.json().await.unwrap();
        // One code and one detail shape, differing only in the caller's own words back — which is
        // the same information the request carried, and so no oracle.
        assert_eq!(body["error"], "unknown", "{body}");
        assert_eq!(body["detail"], format!("unknown view '{view}'"), "{body}");
    }
}
