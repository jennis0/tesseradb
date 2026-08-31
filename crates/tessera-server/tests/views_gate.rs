//! **The view gate, served** (`views.md` §6): which views a principal may reach, and what a
//! principal who may not reach one is told.
//!
//! This is the one stage of the views work whose failure mode is a disclosure, so the assertions
//! here are about *indistinguishability* at least as much as about filtering. Six facts carry it,
//! and each is a way the gate could be wrong while every other test in this directory still
//! passed:
//!
//! - **Discovery is filtered, and filtered consistently.** A view whose gate this principal fails
//!   is absent from `/v1/meta`'s `views`; a gate-failed **group** is absent from `groups` and takes
//!   its whole roster with it. A roster that disagreed with what a viewer verb will answer is an
//!   existence oracle by subtraction.
//! - **The refusal is the refusal an unknown name gets** — same status, same detail shape — for a
//!   gated view and for a key nobody declared. A different code, or a different sentence, is the
//!   oracle the filtering exists to prevent.
//! - **Intersection, not the conservative label join.** `atlas` is gated `finance,legal`: a
//!   principal holding *either* term reaches it and a principal holding *neither* does not. Under
//!   the required-set reading a disjunctive gate has an empty required set and **every** principal
//!   passes, so the outsider's 404 on `atlas` is the assertion that separates the two semantics.
//! - **The scoped surface collapses whole** (`views.md` §5): for a principal who cannot reach the
//!   owning group, `sentiment` is undeclared — absent from `filter_operands`, and both leaf
//!   spellings, bare and pinned, take the ordinary unknown-column `422` that names no group.
//! - **The gate is conjunctive with item labels, never substitutive.** Inside a gate they pass,
//!   a principal still sees only the items their own mask admits.
//! - **The set is fixed for the session's life.** A view created after a session authorised is a
//!   404 to that session and is served to the next one, which is the price of resolving the whole
//!   set once (owner ruling 2026-08-30).
//!
//! **A filtered roster is a shorter list and nothing else** ([decision 0113](../../../docs/decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md),
//! `views.md` §9). Views carry no position of their own, so a principal failing
//! `quarter:2026-Q3`'s gate reads three keys in creation order with nothing to count the missing
//! one by — which is what closes the gap the ordinal used to leave.
//!
//! The bundle is synthetic and built here. The multi-view fixtures beside this file are all-public,
//! and a gate needs a corpus that gates something.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::config::{AccessInput, AccessSource, Attribute, Fields};
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation, ScopedColumnFamily,
    ViewArgs,
};
use tessera_spatial::tiler::ScalarType;

/// The item labels, one per entity and the **same in every view** — pass one unions an entity's
/// label over every source and refuses a disagreement, and the label is the entity's rather than
/// the row's (`views.md` §7).
///
/// Three labels, so that a gate can be satisfied by one of them and an item's own visibility by
/// another: `finance` and `legal` gate views below, and every principal holds `public`.
fn label_of(e: u64) -> &'static str {
    match e % 3 {
        0 => "finance",
        1 => "legal",
        _ => "public",
    }
}

/// The gated view inside the otherwise public `quarter` group: the key a failing principal never
/// sees, and the one this file's indistinguishability cases are built on.
const GATED_QUARTER_KEY: &str = "2026-Q3";

const QUARTERS: [(&str, std::ops::Range<u64>, Option<&str>); 4] = [
    ("2026-Q1", 0..15, None),
    ("2026-Q2", 10..30, None),
    // **A gated view inside a public group**: the group's gate passes for everyone and this one
    // does not, so a failing principal reads the roster with this key simply absent.
    ("2026-Q3", 5..25, Some("finance")),
    ("2026-Q4", 8..28, None),
];

/// The `sealed` group's own roster. The **group** is gated, so a failing principal sees neither
/// row nor view nor the attribute scoped to it.
const SEALED: [(&str, std::ops::Range<u64>); 2] = [("s1", 0..15), ("s2", 10..30)];

/// A view's own layout: the same entity sits somewhere different in each.
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

/// The `sentiment` value an entity carries in the `sealed` view at `ordinal`, or `None` where it
/// carries none.
fn sentiment(ordinal: usize, e: u64) -> Option<f32> {
    match (e + ordinal as u64) % 4 {
        0 => None,
        n => Some(n as f32 * 10.0),
    }
}

/// A points file: geometry, the entity's own access label, and — for a view of `sealed` — that
/// view's own `sentiment` column.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, scoped: Option<usize>) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("access", DataType::Utf8, false),
    ];
    if scoped.is_some() {
        fields.push(Field::new("sentiment", DataType::Float32, true));
    }
    let schema = Arc::new(Schema::new(fields));
    let ids: Vec<u64> = ids.collect();
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(UInt64Array::from(ids.clone())),
        Arc::new(Float64Array::from(
            ids.iter().map(|&e| position(view, e).0).collect::<Vec<_>>(),
        )),
        Arc::new(Float64Array::from(
            ids.iter().map(|&e| position(view, e).1).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            ids.iter().map(|&e| label_of(e)).collect::<Vec<_>>(),
        )),
    ];
    if let Some(ordinal) = scoped {
        columns.push(Arc::new(Float32Array::from(
            ids.iter()
                .map(|&e| sentiment(ordinal, e))
                .collect::<Vec<_>>(),
        )));
    }
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
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

fn view_args(view: &str, points: &Path, visibility: Option<&str>) -> ViewArgs {
    ViewArgs {
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Fields::default(),
        select: None,
        // The label is a column of the view's own points file, so the descriptors interned into
        // the dictionary are the words this file writes — which is what lets a gate below name one.
        access: AccessInput {
            source: AccessSource::Field("access".to_string()),
            default: "public".to_string(),
        },
        visibility: visibility.map(str::to_string),
    }
}

fn roster(views: &[(&str, Option<&str>)]) -> Vec<GroupViewDescriptor> {
    views
        .iter()
        .map(|(key, visibility)| GroupViewDescriptor {
            key: key.to_string(),
            visibility: visibility.map(str::to_string),
            metadata: Default::default(),
        })
        .collect()
}

/// The bundle: two plain views (one public, one gated disjunctively), a public group with one
/// gated view in it, and a gated group carrying a scoped attribute.
fn build_gated(dir: &Path) -> std::path::PathBuf {
    let world_points = dir.join("world.parquet");
    write_points(&world_points, "world", 0..20, None);
    let atlas_points = dir.join("atlas.parquet");
    write_points(&atlas_points, "atlas", 0..20, None);
    let mut views = vec![
        view_args("world", &world_points, None),
        // **A disjunctive gate.** `builtin:passthrough` splits a label on commas, so this is the
        // term set {finance, legal} and the gate is satisfied by intersection with the principal's.
        view_args("atlas", &atlas_points, Some("finance,legal")),
    ];
    for (key, members, visibility) in QUARTERS {
        let id = format!("quarter:{key}");
        let points = dir.join(format!("quarter-{key}.parquet"));
        write_points(&points, &id, members, None);
        views.push(view_args(&id, &points, visibility));
    }
    let mut family_views = Vec::new();
    for (ordinal, (key, members)) in SEALED.iter().enumerate() {
        let id = format!("sealed:{key}");
        let points = dir.join(format!("sealed-{key}.parquet"));
        write_points(&points, &id, members.clone(), Some(ordinal));
        family_views.push(views.len());
        views.push(view_args(&id, &points, None));
    }
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![
            GroupDescriptor {
                name: "quarter".to_string(),
                members_of: None,
                visibility: None,
                views: roster(&QUARTERS.map(|(key, _, v)| (key, v))),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
            GroupDescriptor {
                name: "sealed".to_string(),
                members_of: None,
                // **The group's own gate, the outer bound over its whole roster.**
                visibility: Some("finance".to_string()),
                views: roster(&SEALED.map(|(key, _)| (key, None))),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
        ],
        scoped_attributes: vec![ScopedColumnFamily {
            attribute: Attribute {
                name: "sentiment".to_string(),
                title: None,
                field: None,
                ty: ScalarType::F32,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: true,
                render: false,
            },
            group: "sealed".to_string(),
            views: family_views,
        }],
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
    .expect("a gated eight-view build succeeds");
    out
}

struct Served {
    server: TestServer,
    _tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_gated(tmp.path());
    let server = spawn_server(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    Served { server, _tmp: tmp }
}

/// A session holding exactly these label descriptors. `&[]` is the outsider: `public` and nothing
/// else, which `Engine::authorise` adds inside the trust boundary.
async fn token(served: &Served, terms: &[&str]) -> String {
    authorise(&served.server, terms).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn meta(served: &Served, token: &str) -> Value {
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

fn view_ids(meta: &Value) -> BTreeSet<String> {
    meta["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

fn group_names(meta: &Value) -> BTreeSet<String> {
    meta["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap().to_string())
        .collect()
}

async fn viewport(
    served: &Served,
    token: &str,
    view: &str,
    filters: Option<Value>,
) -> (u16, Value) {
    let mut body = json!({"view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200});
    if let Some(filters) = filters {
        body["filters"] = filters;
    }
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    // A refusal is JSON; a served viewport is a frame stream, and only its point count is read.
    let body = match status {
        200 => {
            let (_, points) = decode_viewport(&bytes);
            json!({"points": points.len()})
        }
        _ => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    };
    (status, body)
}

async fn create_view(served: &Served, group: &str, key: &str, body: Value) -> reqwest::Response {
    served
        .server
        .client
        .put(
            served
                .server
                .control_url(&format!("/control/views/{group}/{key}")),
        )
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

/// **What each principal is told exists.** The holder sees the whole declaration; the outsider
/// sees the public views of the public group and the public plain view, and nothing else — no
/// gated plain view, no gated view inside a public group, and no gated group at all.
#[tokio::test]
async fn meta_publishes_the_views_a_principal_may_reach_and_no_others() {
    let served = serve().await;
    let holder = token(&served, &["finance"]).await;
    let outsider = token(&served, &[]).await;

    let all: BTreeSet<String> = [
        "world",
        "atlas",
        "quarter:2026-Q1",
        "quarter:2026-Q2",
        "quarter:2026-Q3",
        "quarter:2026-Q4",
        "sealed:s1",
        "sealed:s2",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let held = meta(&served, &holder).await;
    assert_eq!(
        view_ids(&held),
        all,
        "a principal holding `finance` reaches every view"
    );
    assert_eq!(
        group_names(&held),
        ["quarter", "sealed"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );

    let out = meta(&served, &outsider).await;
    assert_eq!(
        view_ids(&out),
        [
            "world",
            "quarter:2026-Q1",
            "quarter:2026-Q2",
            "quarter:2026-Q4"
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>(),
        "a principal holding only `public` reaches the public views and no others"
    );
    assert_eq!(
        group_names(&out),
        ["quarter"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>(),
        "a gate-failed group takes its own `groups` row with it"
    );
    let quarter = out["groups"].as_array().unwrap()[0]["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        !quarter.contains(&"quarter:2026-Q3".to_string()),
        "the group's own ordering lists only the views this principal may reach: {quarter:?}"
    );
}

/// **The gate is satisfied by intersection** (`views.md` §6.1), which is what a disjunctive gate
/// needs: `atlas` is `finance,legal`, and either term alone reaches it.
///
/// The **discriminating** assertion is the outsider's, not the holders'. Under the conservative
/// label join's required-set reading a disjunctive gate yields an empty required set, which is
/// contained in every principal's mask, so *every* principal would pass — including this one.
#[tokio::test]
async fn a_disjunctive_gate_admits_either_term_and_neither_admits_nobody() {
    let served = serve().await;
    for term in ["finance", "legal"] {
        let held = meta(&served, &token(&served, &[term]).await).await;
        assert!(
            view_ids(&held).contains("atlas"),
            "'{term}' alone satisfies the gate `finance,legal`"
        );
    }
    let out = meta(&served, &token(&served, &[]).await).await;
    assert!(
        !view_ids(&out).contains("atlas"),
        "a principal holding neither term does NOT pass a disjunctive gate — the required-set \
         reading, under which everyone passes, is what this asserts against"
    );
    assert!(
        !view_ids(&out).contains("sealed:s1"),
        "and `legal` does not open a `finance` group either: the gate is an intersection, not a \
         wildcard"
    );
    let legal = meta(&served, &token(&served, &["legal"]).await).await;
    assert!(
        !view_ids(&legal).contains("sealed:s1"),
        "holding one term does not satisfy a gate naming another"
    );
}

/// **Each served layer's `views` list is inside the gate too** — the gate governs every
/// view-valued surface, not only discovery. The bundle declares no layer, so what is asserted here
/// is the shape: the key is present and lists only reachable views.
#[tokio::test]
async fn the_meta_document_names_no_unreachable_view_anywhere() {
    let served = serve().await;
    let out = meta(&served, &token(&served, &[]).await).await;
    let reachable = view_ids(&out);
    for layer in out["layers"].as_array().unwrap() {
        for view in layer["views"].as_array().unwrap() {
            assert!(
                reachable.contains(view.as_str().unwrap()),
                "a served layer names a view this principal cannot reach"
            );
        }
    }
    let text = out.to_string();
    for hidden in ["atlas", "sealed", "2026-Q3"] {
        assert!(
            !text.contains(hidden),
            "'{hidden}' appears somewhere on the document a failing principal reads"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Indistinguishability
// ---------------------------------------------------------------------------------------------

/// **A gate-failed view and a name nobody declared are one answer.**
///
/// The comparison substitutes the requested id out of the detail before comparing, because the
/// detail echoes the caller's own string: what must be identical is the status and the shape, and
/// a client that could tell the two apart at all would hold an existence oracle over the roster.
#[tokio::test]
async fn a_gate_failed_view_answers_exactly_as_a_view_that_never_existed() {
    let served = serve().await;
    let outsider = token(&served, &[]).await;

    let cases = [
        // (gate-failed, never declared)
        ("sealed:s1", "sealed:nosuch"),
        // The gated view's own key, in the only form that addresses it (decision 0113), against a
        // key of the same group that nobody declared: the comparison this case exists to make.
        (
            &format!("quarter:{GATED_QUARTER_KEY}"),
            "quarter:2026-Q9",
        ),
        ("atlas", "nosuchview"),
        // The retired `#`-prefixed form addresses nothing at all now (decision 0113), and one
        // carrying the **real** gated key must be no more informative than a name nobody
        // declared — a spelling that resolved would be an existence oracle over the roster.
        (
            &format!("quarter:#{GATED_QUARTER_KEY}"),
            "quarter:2026-Q9",
        ),
    ];
    for (gated, absent) in cases {
        let (gated_status, gated_body) = viewport(&served, &outsider, gated, None).await;
        let (absent_status, absent_body) = viewport(&served, &outsider, absent, None).await;
        assert_eq!(gated_status, 404, "a gate-failed view is a 404: {gated}");
        assert_eq!(absent_status, 404);
        assert_eq!(
            gated_body.to_string().replace(gated, "<view>"),
            absent_body.to_string().replace(absent, "<view>"),
            "'{gated}' and '{absent}' must answer with one code and one detail shape"
        );
    }

    // `/v1/artifacts` resolves a view the same way, so it refuses the same way.
    for (gated, absent) in [("sealed:s1", "sealed:nosuch")] {
        let one = artifact_status(&served, &outsider, gated).await;
        let two = artifact_status(&served, &outsider, absent).await;
        assert_eq!((one, two), (404, 404));
    }

    // And the holder is served the same names, which is what makes the assertions above about the
    // gate rather than about the fixture.
    let holder = token(&served, &["finance"]).await;
    for view in ["sealed:s1", "quarter:2026-Q3", "atlas"] {
        assert_eq!(
            viewport(&served, &holder, view, None).await.0,
            200,
            "{view} is served to a principal who passes its gate"
        );
    }
}

async fn artifact_status(served: &Served, token: &str, view: &str) -> u16 {
    served
        .server
        .client
        .post(served.server.viewer_url("/v1/artifacts/1"))
        .bearer_auth(token)
        .json(&json!({"view": view, "zoom": 8}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// **A gate-failed view leaves no trace in the roster it is filtered out of** (decision 0113,
/// `views.md` §9). The views a failing principal reads are the three public keys in creation
/// order and nothing else: no position, no count and no number to notice a hole in. The roster
/// record's whole published shape is asserted here, so a field carrying an ordinal again would
/// fail this test rather than reappear quietly.
#[tokio::test]
async fn a_gate_failed_view_leaves_nothing_countable_in_the_roster() {
    let served = serve().await;
    let out = meta(&served, &token(&served, &[]).await).await;
    let rostered: Vec<&Value> = out["views"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["group"] == json!("quarter"))
        .collect();
    let keys: Vec<&str> = rostered
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec!["2026-Q1", "2026-Q2", "2026-Q4"],
        "the gated key is absent, and the rest keep creation order"
    );
    for view in rostered {
        assert!(
            view.get("ordinal").is_none(),
            "a view carries no ordinal on the wire: {view}"
        );
    }
    assert_eq!(
        out["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == json!("quarter"))
            .unwrap()["views"],
        json!(["quarter:2026-Q1", "quarter:2026-Q2", "quarter:2026-Q4"]),
        "the group lists the keys it serves, in creation order"
    );
}

// ---------------------------------------------------------------------------------------------
// The scoped surface
// ---------------------------------------------------------------------------------------------

fn leaf(column: &str) -> Value {
    json!({ column: {"range": {"gte": 5.0}} })
}

/// **For a principal who cannot reach the group, the scoped attribute is undeclared**
/// (`views.md` §5): absent from `filter_operands`, and both spellings — bare and pinned — take the
/// ordinary unknown-column `422`, which names no group and confirms no key.
#[tokio::test]
async fn a_scoped_attribute_collapses_whole_outside_its_groups_gate() {
    let served = serve().await;
    let outsider = token(&served, &[]).await;
    let holder = token(&served, &["finance"]).await;

    let operands = |m: &Value| -> Vec<String> {
        m["filter_operands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["column"].as_str().unwrap().to_string())
            .collect()
    };
    let held = meta(&served, &holder).await;
    assert!(
        operands(&held).contains(&"sentiment".to_string()),
        "the holder is offered the operand"
    );
    let out = meta(&served, &outsider).await;
    assert!(
        !operands(&out).contains(&"sentiment".to_string()),
        "the outsider is not: {:?}",
        operands(&out)
    );

    for spelling in ["sentiment", "sentiment@s1", "sentiment@#0"] {
        let (status, body) = viewport(&served, &outsider, "world", Some(leaf(spelling))).await;
        assert_eq!(status, 422, "{spelling} is refused as an unknown column");
        let detail = body["detail"].as_str().unwrap_or_default().to_string();
        assert!(
            detail.contains("not a filterable column"),
            "{spelling}: {detail}"
        );
        assert!(
            !detail.contains("sealed"),
            "the refusal must not name the group: {detail}"
        );
    }

    // The holder gets the ordinary answers instead: a bare leaf under a view that decides nothing
    // is the `422` naming the group, and a pin resolves.
    let (status, body) = viewport(&served, &holder, "world", Some(leaf("sentiment"))).await;
    assert_eq!(status, 422);
    assert!(
        body["detail"].as_str().unwrap().contains("sealed"),
        "a principal inside the gate is told which group to pin: {body}"
    );
    assert_eq!(
        viewport(&served, &holder, "world", Some(leaf("sentiment@s1")))
            .await
            .0,
        200,
        "and a pinned leaf answers"
    );
}

// ---------------------------------------------------------------------------------------------
// Conjunction with item labels, and the fixed set
// ---------------------------------------------------------------------------------------------

/// **The gate is conjunctive with item labels, never substitutive.** Passing `sealed`'s gate does
/// not make its items visible: a principal holding `finance` sees the `finance` and `public` items
/// of `sealed:s1` and not its `legal` ones.
#[tokio::test]
async fn passing_a_gate_does_not_widen_the_mask_inside_it() {
    let served = serve().await;
    let holder = token(&served, &["finance"]).await;
    let (status, body) = viewport(&served, &holder, "sealed:s1", None).await;
    assert_eq!(status, 200);
    let expected = SEALED[0]
        .1
        .clone()
        .filter(|&e| matches!(label_of(e), "finance" | "public"))
        .count();
    assert_eq!(
        body["points"].as_u64().unwrap() as usize,
        expected,
        "the items inside a passed gate are still governed by their own labels"
    );
    assert!(
        expected < SEALED[0].1.clone().count(),
        "the fixture must actually withhold something for that to say anything"
    );
}

/// **The visible-view set is fixed for the session's life** (owner ruling 2026-08-30): a view
/// created after a session authorised is a 404 to that session — indistinguishable, as ever, from
/// one that never existed — and is served to the next session, which re-resolves the set.
#[tokio::test]
async fn a_view_created_after_a_session_authorised_waits_for_re_authorisation() {
    let served = serve().await;
    let before = token(&served, &[]).await;
    assert_eq!(
        create_view(&served, "quarter", "2026-Q5", json!({}))
            .await
            .status(),
        201
    );

    assert_eq!(
        viewport(&served, &before, "quarter:2026-Q5", None).await.0,
        404,
        "the session that authorised first holds the set it resolved"
    );
    assert!(
        !view_ids(&meta(&served, &before).await).contains("quarter:2026-Q5"),
        "and its `/v1/meta` says the same thing the verb does"
    );

    let after = token(&served, &[]).await;
    assert!(
        view_ids(&meta(&served, &after).await).contains("quarter:2026-Q5"),
        "a session authorised after the create sees it"
    );
    assert_eq!(
        viewport(&served, &after, "quarter:2026-Q5", None).await.0,
        200,
        "and can name it"
    );
}

/// **A created view may carry a gate, and the label is checked against the plugin that will
/// evaluate it.** A label naming no terms is refused rather than stored: it would be a gate
/// satisfied by nobody, the view reachable by no principal including its author.
#[tokio::test]
async fn a_create_takes_a_gate_and_refuses_one_no_principal_could_satisfy() {
    let served = serve().await;
    assert_eq!(
        create_view(
            &served,
            "quarter",
            "gated",
            json!({"visibility": "finance"})
        )
        .await
        .status(),
        201
    );
    assert_eq!(
        create_view(&served, "quarter", "empty", json!({"visibility": " , , "}))
            .await
            .status(),
        422,
        "a label naming no terms is refused at acceptance"
    );

    let holder = token(&served, &["finance"]).await;
    let outsider = token(&served, &[]).await;
    assert!(view_ids(&meta(&served, &holder).await).contains("quarter:gated"));
    assert!(!view_ids(&meta(&served, &outsider).await).contains("quarter:gated"));
    assert_eq!(
        viewport(&served, &outsider, "quarter:gated", None).await.0,
        404
    );
}

/// **The control plane is not gated** (`views.md` §6): it holds the operator credential and is the
/// single authority that writes the roster, so a gate there would be a gate against its own
/// author. The gated group's views are creatable and droppable with no label presented.
#[tokio::test]
async fn the_control_plane_reaches_a_gated_group() {
    let served = serve().await;
    assert_eq!(
        create_view(&served, "sealed", "s3", json!({}))
            .await
            .status(),
        201,
        "the operator creates a view of a gated group"
    );
    let outsider = token(&served, &[]).await;
    assert!(!view_ids(&meta(&served, &outsider).await).contains("sealed:s3"));
    let holder = token(&served, &["finance"]).await;
    assert!(view_ids(&meta(&served, &holder).await).contains("sealed:s3"));
}
