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
//! - **Intersection, not the conservative label join.** `atlas` is gated on the list
//!   `["finance", "legal"]`: a principal holding *either* term reaches it and a principal holding
//!   *neither* does not. Under the required-set reading a disjunctive gate has an empty required
//!   set and **every** principal passes, so the outsider's 404 on `atlas` is the assertion that
//!   separates the two semantics.
//! - **A gate's label is one label** (decision 0132). `ledger` is gated on the one label
//!   `finance,legal`, which is one term with a comma in it: the principal holding that term
//!   reaches it, and a principal holding `finance`, `legal` or both does not. A reader that split
//!   the label would gate `ledger` as `atlas` is gated.
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
use tessera_build::config::{AccessInput, AccessSource, Attribute, Fields, ValueSet};
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

/// A quarter's key, its entities, and its own gate as a list of labels (decision 0132).
type Quarter = (&'static str, std::ops::Range<u64>, Option<&'static [&'static str]>);

const QUARTERS: [Quarter; 4] = [
    ("2026-Q1", 0..15, None),
    ("2026-Q2", 10..30, None),
    // **A gated view inside a public group**: the group's gate passes for everyone and this one
    // does not, so a failing principal reads the roster with this key simply absent.
    ("2026-Q3", 5..25, Some(&["finance"])),
    ("2026-Q4", 8..28, None),
];

/// The one label `ledger` is gated on: one term, the comma being part of it (decision 0132).
/// Its entities lie outside every other view's range, so interning the term touches no other
/// entity's label set.
const COMMA_TERM: &str = "finance,legal";
const LEDGER: std::ops::Range<u64> = 40..50;

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

/// `mood`'s value set, and the word each view's prose carries.
const MOODS: [&str; 3] = ["calm", "tense", "wild"];

/// The analyser identity the manifest records for the text family — spelt here because this
/// fixture is built programmatically rather than parsed from TOML.
const ANALYSER: &str = "unicode/icu4x-2.2/p1";

/// The `sentiment` value an entity carries in the `sealed` view at `ordinal`, or `None` where it
/// carries none.
fn sentiment(ordinal: usize, e: u64) -> Option<f32> {
    match (e + ordinal as u64) % 4 {
        0 => None,
        n => Some(n as f32 * 10.0),
    }
}

/// The `mood` value an entity carries in the `sealed` view at `ordinal` — the **category** family,
/// which owes per-view postings and a `/v1/categories` value list besides.
fn mood(ordinal: usize, e: u64) -> &'static str {
    MOODS[((e + ordinal as u64) % MOODS.len() as u64) as usize]
}

/// The prose an entity carries there — the **text** family, whose only artefacts are a per-view
/// token dictionary and the postings over it.
fn note(ordinal: usize, e: u64) -> String {
    format!("sealed {} note for {e}", MOODS[ordinal % MOODS.len()])
}

/// The `glow` value an entity carries there — the **render-only** family, declared `index = false`
/// and an operand on `render` alone (`views.md` §5 r26). It is here because that licence is a
/// fourth way into the same collapse: a family the gate must hide from a principal who cannot
/// reach `sealed`, on a surface the other three do not reach it by.
fn glow(ordinal: usize, e: u64) -> Option<f32> {
    match (e + ordinal as u64) % 5 {
        0 => None,
        n => Some(n as f32 * 3.0),
    }
}

/// The `tint` value an entity carries there — a **category** on the surface by `render` alone,
/// over the same vocabulary `mood` uses. It is the family where the two admissions could disagree
/// on disc rather than only on a surface: a category on the filter surface owes per-view keyed
/// postings, which `/v1/categories` reads too, so a build that wrote none for it would refuse the
/// open outright.
fn tint(ordinal: usize, e: u64) -> &'static str {
    MOODS[((e * 2 + ordinal as u64) % MOODS.len() as u64) as usize]
}

/// A points file: geometry, the entity's own access label, and — for a view of `sealed` — that
/// view's own columns of the five scoped families.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, scoped: Option<usize>) {
    write_points_labelled(path, view, ids, scoped, label_of)
}

/// [`write_points`] with the entity's label chosen by the caller, for the one view whose
/// entities carry a term no other file interns.
fn write_points_labelled(
    path: &Path,
    view: &str,
    ids: std::ops::Range<u64>,
    scoped: Option<usize>,
    label_of: fn(u64) -> &'static str,
) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("access", DataType::Utf8, false),
    ];
    if scoped.is_some() {
        fields.push(Field::new("sentiment", DataType::Float32, true));
        fields.push(Field::new("mood", DataType::Utf8, true));
        fields.push(Field::new("note", DataType::Utf8, true));
        fields.push(Field::new("glow", DataType::Float32, true));
        fields.push(Field::new("tint", DataType::Utf8, true));
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
        columns.push(Arc::new(StringArray::from(
            ids.iter().map(|&e| mood(ordinal, e)).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(StringArray::from(
            ids.iter().map(|&e| note(ordinal, e)).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(Float32Array::from(
            ids.iter().map(|&e| glow(ordinal, e)).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(StringArray::from(
            ids.iter().map(|&e| tint(ordinal, e)).collect::<Vec<_>>(),
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

fn view_args(view: &str, points: &Path, visibility: Option<&[&str]>) -> ViewArgs {
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
            default: Some("public".to_string()),
        },
        visibility: visibility.map(labels),
    }
}

/// A gate as the manifest stores it: one term per element (decision 0132).
fn labels(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|l| l.to_string()).collect()
}

fn sealed_family(attribute: Attribute, views: Vec<usize>) -> ScopedColumnFamily {
    ScopedColumnFamily {
        attribute,
        group: "sealed".to_string(),
        views,
        source: None,
    }
}

fn roster(views: &[(&str, Option<&[&str]>)]) -> Vec<GroupViewDescriptor> {
    views
        .iter()
        .map(|(key, visibility)| GroupViewDescriptor {
            key: key.to_string(),
            visibility: visibility.map(labels),
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
    let ledger_points = dir.join("ledger.parquet");
    write_points_labelled(&ledger_points, "ledger", LEDGER, None, |_| COMMA_TERM);
    let mut views = vec![
        view_args("world", &world_points, None),
        // **A disjunctive gate.** A gate wanting several terms declares them as a list
        // (decision 0132): this is the term set {finance, legal}, and the gate is satisfied by
        // intersection with the principal's.
        view_args("atlas", &atlas_points, Some(&["finance", "legal"])),
        // **One label with a comma in it**: one term, gating exactly the principals who hold it.
        view_args("ledger", &ledger_points, Some(&[COMMA_TERM])),
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
                title: None,
                name: "quarter".to_string(),
                members_of: None,
                point_default: Some("public".to_string()),
                visibility: None,
                views: roster(&QUARTERS.map(|(key, _, v)| (key, v))),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
            GroupDescriptor {
                title: None,
                name: "sealed".to_string(),
                members_of: None,
                point_default: Some("public".to_string()),
                // **The group's own gate, the outer bound over its whole roster.**
                visibility: Some(labels(&["finance"])),
                views: roster(&SEALED.map(|(key, _)| (key, None))),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
        ],
        // **Five families, one gate.** The collapse is one site ahead of the pin/bare split
        // (`views.md` §5), so a second family taking a different answer from the first would be
        // the defect this fixture exists to catch. The last two are licensed by `render` alone
        // (r26), which is a different admission reaching the same site — and the second of them
        // is a category, whose per-view postings and value list that admission owes as well.
        scoped_attributes: vec![
            sealed_family(
                // **Rendered as well as indexed**, so the collapse has a second surface to be
                // checked on: the value reaches the hot row tail of `sealed`'s views, and a
                // principal who fails the group's gate must not see the column named in any
                // response at all (`views.md` §5, §6).
                Attribute {
                    name: "sentiment".to_string(),
                    title: None,
                    field: None,
                    ty: ScalarType::F32,
                    analyser: None,
                    vocabulary: None,
                    value_set: None,
                    index: true,
                    render: true,
                },
                family_views.clone(),
            ),
            sealed_family(
                Attribute {
                    name: "mood".to_string(),
                    title: None,
                    field: None,
                    ty: ScalarType::U8,
                    analyser: None,
                    vocabulary: Some("mood".to_string()),
                    value_set: Some(ValueSet::Closed),
                    index: true,
                    render: false,
                },
                family_views.clone(),
            ),
            sealed_family(
                Attribute {
                    name: "note".to_string(),
                    title: None,
                    field: None,
                    ty: ScalarType::Text,
                    analyser: Some(ANALYSER.to_string()),
                    vocabulary: None,
                    value_set: None,
                    index: true,
                    render: false,
                },
                family_views.clone(),
            ),
            sealed_family(
                // **On the surface by `render` alone** (`views.md` §5 r26): no `index`, and an
                // operand all the same, so the gate has a fourth family to collapse.
                Attribute {
                    name: "glow".to_string(),
                    title: None,
                    field: None,
                    ty: ScalarType::F32,
                    analyser: None,
                    vocabulary: None,
                    value_set: None,
                    index: false,
                    render: true,
                },
                family_views.clone(),
            ),
            sealed_family(
                // A **category** on the surface by `render` alone: it owes the keyed postings an
                // `eq` and `/v1/categories` are both answered from, on the same admission.
                Attribute {
                    name: "tint".to_string(),
                    title: None,
                    field: None,
                    ty: ScalarType::U8,
                    analyser: None,
                    vocabulary: Some("mood".to_string()),
                    value_set: Some(ValueSet::Closed),
                    index: false,
                    render: true,
                },
                family_views.clone(),
            ),
        ],
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
        schema: tessera_build::config::Schema {
            attributes: Vec::new(),
            // The value set `mood`'s codes index. `public`, so the list is authored and
            // `/v1/categories` filters nothing — which is what makes the gate the *only* thing
            // that can withhold it from the outsider below.
            vocabularies: std::collections::HashMap::from([(
                "mood".to_string(),
                tessera_build::config::Vocabulary {
                    name: "mood".to_string(),
                    title: None,
                    value_set: ValueSet::Closed,
                    width: ScalarType::U8,
                    values: tessera_build::config::VocabularyMinter::declared(
                        "mood",
                        tessera_build::config::VocabularyKind::Declared,
                        tessera_build::config::Visibility::Public,
                        ScalarType::U8,
                        &[],
                        MOODS.iter().zip(1..).map(|(key, code)| (*key, code)),
                        [],
                    )
                    .expect("distinct pinned codes"),
                    reserved: Vec::new(),
                },
            )]),
        },
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
/// needs: `atlas` is gated on the list `["finance", "legal"]`, and either term alone reaches it.
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
            "'{term}' alone satisfies the gate [\"finance\", \"legal\"]"
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

/// **A gate's label is one label, taken verbatim** (`views.md` §6, decision 0132): `ledger` is
/// gated on `finance,legal`, which is one term with a comma in it. The principal holding that
/// term reaches the view; a principal holding `finance`, `legal` or both does not, because none of
/// those is the term. A reader that split the label on the comma would admit all three and gate
/// `ledger` exactly as `atlas` is gated, so the holders of the fragments are the discriminating
/// assertion.
#[tokio::test]
async fn a_label_containing_a_comma_is_one_term() {
    let served = serve().await;
    let held = meta(&served, &token(&served, &[COMMA_TERM]).await).await;
    assert!(
        view_ids(&held).contains("ledger"),
        "the principal holding the term `finance,legal` reaches the view gated on it"
    );
    assert!(
        !view_ids(&held).contains("atlas"),
        "and that term is neither `finance` nor `legal`, so it does not open `atlas`"
    );
    for fragments in [&["finance"][..], &["legal"], &["finance", "legal"]] {
        let out = meta(&served, &token(&served, fragments).await).await;
        assert!(
            !view_ids(&out).contains("ledger"),
            "{fragments:?} does not satisfy the one-label gate `finance,legal`: a fragment of a \
             label is not the label"
        );
    }
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

/// **The collapse is one site, so a second family cannot take a different answer from the first**
/// (`views.md` §5). `mood` is a category, `note` is text and `glow` is on the surface by `render`
/// alone (r26), and for a principal who cannot reach `sealed` all three are undeclared exactly as
/// `sentiment` is — absent from `filter_operands`, the
/// unknown-column `422` for either spelling, and for the category the same `404`
/// `/v1/categories` gives a name that is nothing at all. A category is the family where getting
/// this wrong costs most: its value list is a second surface, and one gated at the filter and not
/// at the list would publish a gated group's value names to anyone with a session.
#[tokio::test]
async fn the_category_text_and_render_only_families_collapse_at_the_same_site() {
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
    let held = operands(&meta(&served, &holder).await);
    let out = operands(&meta(&served, &outsider).await);
    for column in ["mood", "note", "glow", "tint"] {
        assert!(held.contains(&column.to_string()), "{column}: {held:?}");
        assert!(!out.contains(&column.to_string()), "{column}: {out:?}");
    }

    for (spelling, body) in [
        ("mood", json!({"mood": {"eq": "calm"}})),
        ("mood@s1", json!({"mood@s1": {"eq": "calm"}})),
        ("note", json!({"note": {"match": "calm"}})),
        ("note@s1", json!({"note@s1": {"match": "calm"}})),
        ("glow", json!({"glow": {"range": {"gte": 1.0}}})),
        ("glow@s1", json!({"glow@s1": {"range": {"gte": 1.0}}})),
        ("tint", json!({"tint": {"eq": "calm"}})),
        ("tint@s1", json!({"tint@s1": {"eq": "calm"}})),
    ] {
        let (status, answer) = viewport(&served, &outsider, "world", Some(body)).await;
        assert_eq!(status, 422, "{spelling}");
        let detail = answer["detail"].as_str().unwrap_or_default().to_string();
        assert!(
            detail.contains("not a filterable column"),
            "{spelling}: {detail}"
        );
        assert!(!detail.contains("sealed"), "{spelling}: {detail}");
    }

    // The value list, the surface a category has and no other family does. The outsider gets the
    // `404` an unknown column gets, whichever spelling names the view; the holder gets the list.
    let list = |token: &str, path: &str| {
        let url = served.server.viewer_url(&format!("/v1/categories/{path}"));
        let client = served.server.client.clone();
        let token = token.to_string();
        async move { client.get(url).bearer_auth(token).send().await.unwrap() }
    };
    for path in [
        "mood?view=sealed:s1",
        "mood@s1",
        "mood",
        "tint?view=sealed:s1",
        "tint@s1",
        // The bare form resolves through the owning group rather than through a pin — a distinct
        // path, so the render-only category drives it as the indexed one above does.
        "tint",
    ] {
        assert_eq!(
            list(&outsider, path).await.status().as_u16(),
            404,
            "{path} must be unknown to a principal outside the group"
        );
    }
    for column in ["mood", "tint"] {
        // The render-only category's list is owed on the same admission as its operand, so the
        // holder gets it and the per-view postings behind it exist.
        let resp = list(&holder, &format!("{column}?view=sealed:s1")).await;
        assert_eq!(resp.status().as_u16(), 200, "{column}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["values"].as_array().unwrap().len(), MOODS.len());
    }
}

/// **A sealed family's rendered column is on no surface a gate-failed principal touches**
/// (`views.md` §5, §6). The collapse is about *naming*: `sentiment` renders in the row tail of
/// `sealed`'s views, so the two things to check are that a holder gets it there and that an
/// outsider — who reaches those views not at all — finds the column named nowhere: not in a
/// viewport response's schema under any view they can reach, and not in `/v1/meta`'s family list,
/// which is the only place the document would name the group.
#[tokio::test]
async fn a_sealed_familys_render_column_is_named_in_no_response_outside_the_gate() {
    let served = serve().await;
    let holder = token(&served, &["finance"]).await;
    let outsider = token(&served, &[]).await;

    // The holder: the column is in the tail of the group's views, and in no other view's.
    assert!(points_schema(&served, &holder, "sealed:s1")
        .await
        .contains(&"sentiment".to_string()));
    assert!(!points_schema(&served, &holder, "world")
        .await
        .contains(&"sentiment".to_string()));

    // The outsider: `sealed:s1` is a 404 (asserted next door), so every view they *can* reach is
    // checked instead — none names the column.
    for view in ["world", "quarter:2026-Q1", "quarter:2026-Q4"] {
        let names = points_schema(&served, &outsider, view).await;
        assert!(
            !names.contains(&"sentiment".to_string()),
            "{view}: {names:?}"
        );
    }
    let document = meta(&served, &outsider).await;
    assert!(
        document["scoped_scalars"].as_array().unwrap().is_empty(),
        "the sealed family is undeclared for a principal outside its gate: {}",
        document["scoped_scalars"]
    );
    // And the holder does see it, so the assertion above is about the gate rather than about a
    // list nobody fills.
    let holder_families = meta(&served, &holder).await;
    assert!(holder_families["scoped_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == "sentiment" && f["render"] == true));
}

/// The column names a `/v1/viewport` response's points frames carry, or an empty list where the
/// response has no points frame. Read by name, as contracts §3.2 requires of a client.
async fn points_schema(served: &Served, token: &str, view: &str) -> Vec<String> {
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
    assert_eq!(resp.status().as_u16(), 200, "{view}");
    let bytes = resp.bytes().await.unwrap();
    let frames = tessera_wire::split_frames(&bytes).expect("well-formed frames");
    let mut names = Vec::new();
    for (kind, payload) in frames {
        if kind != tessera_wire::FRAME_POINTS {
            continue;
        }
        let reader = arrow::ipc::reader::StreamReader::try_new(
            std::io::Cursor::new(payload.to_vec()),
            None,
        )
        .unwrap();
        for batch in reader {
            names = batch
                .unwrap()
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
        }
    }
    names
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

/// **A created view may carry a gate, and the labels are checked against the plugin that will
/// evaluate them** (decision 0132). One label is a string and several are a list, each element
/// one term; a gate naming no terms, or carrying an empty element, is refused rather than stored:
/// it would be a gate satisfied by nobody, the view reachable by no principal including its
/// author.
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
        create_view(
            &served,
            "quarter",
            "either",
            json!({"visibility": ["finance", "legal"]})
        )
        .await
        .status(),
        201,
        "a list declares one term per element"
    );
    assert_eq!(
        create_view(&served, "quarter", "empty", json!({"visibility": []}))
            .await
            .status(),
        422,
        "a gate naming no terms is refused at acceptance"
    );
    assert_eq!(
        create_view(&served, "quarter", "hole", json!({"visibility": ["finance", ""]}))
            .await
            .status(),
        422,
        "an empty element is no label, and is refused at acceptance"
    );

    let holder = token(&served, &["finance"]).await;
    let legal = token(&served, &["legal"]).await;
    let outsider = token(&served, &[]).await;
    assert!(view_ids(&meta(&served, &holder).await).contains("quarter:gated"));
    assert!(!view_ids(&meta(&served, &outsider).await).contains("quarter:gated"));
    assert_eq!(
        viewport(&served, &outsider, "quarter:gated", None).await.0,
        404
    );
    for (name, token) in [("finance", &holder), ("legal", &legal)] {
        assert!(
            view_ids(&meta(&served, token).await).contains("quarter:either"),
            "'{name}' alone satisfies the gate created as [\"finance\", \"legal\"]"
        );
    }
    assert!(
        !view_ids(&meta(&served, &outsider).await).contains("quarter:either"),
        "and a principal holding neither does not"
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

// ---------------------------------------------------------------------------------------------
// The drill-down, which the gate reaches for the same reason every other surface does
// ---------------------------------------------------------------------------------------------

/// The `tessera_id`s a view serves this principal.
async fn point_ids(served: &Served, token: &str, view: &str) -> Vec<u64> {
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
    assert_eq!(resp.status().as_u16(), 200, "{view}");
    let bytes = resp.bytes().await.unwrap();
    let (_, points) = decode_viewport(&bytes);
    points.into_iter().map(|(id, _)| id).collect()
}

/// The `tessera_id` one source entity is served under — found through the drill-down's external
/// id, the identity permutation being the server's alone (I10).
async fn id_of(served: &Served, token: &str, view: &str, entity: u64) -> u64 {
    for id in point_ids(served, token, view).await {
        let body: Value = post_item(&served.server, token, id)
            .await
            .json()
            .await
            .unwrap();
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body["external_id"].as_str().expect("an external id"))
            .unwrap();
        if u64::from_le_bytes(bytes.try_into().expect("eight bytes")) == entity {
            return id;
        }
    }
    panic!("entity {entity} is not served in {view}");
}

/// `POST /v1/items/{id}`'s `views` array, by id.
async fn item_views(served: &Served, token: &str, id: u64) -> Vec<String> {
    let body: Value = post_item(&served.server, token, id)
        .await
        .json()
        .await
        .unwrap();
    body["views"]
        .as_array()
        .expect("the views array is always present")
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

/// **The drill-down names no view and no scoped value behind a gate** (`views.md` §5, §6; owner
/// ruling 2026-09-01) — the fifth surface the collapse reaches, and the first that names a view
/// without being asked for one.
///
/// The entity is labelled `public`, so both principals can see the item itself and nothing here is
/// about item visibility. It holds a row in all eight views. The outsider must be served the four
/// they can reach and an empty `scoped`; the holder, all eight and the family's values under both
/// of `sealed`'s keys. A response that listed the item's views without the gate would name
/// `sealed:s1` — the one place the whole document otherwise never names it — to a principal for
/// whom the group does not exist.
#[tokio::test]
async fn the_drill_down_names_no_gate_failed_view_and_no_sealed_value() {
    let served = serve().await;
    let outsider = token(&served, &[]).await;
    let holder = token(&served, &["finance"]).await;
    // `public`, and in every view of this fixture: `world`/`atlas` (0..20), all four quarters, and
    // both of `sealed`'s.
    const ENTITY: u64 = 11;
    assert_eq!(label_of(ENTITY), "public");
    let id = id_of(&served, &outsider, "world", ENTITY).await;

    assert_eq!(
        item_views(&served, &outsider, id).await,
        vec![
            "quarter:2026-Q1".to_string(),
            "quarter:2026-Q2".to_string(),
            "quarter:2026-Q4".to_string(),
            "world".to_string(),
        ],
        "the gated plain view, the gated key and the whole gated group are absent"
    );
    assert_eq!(
        item_views(&served, &holder, id).await,
        vec![
            "atlas".to_string(),
            "quarter:2026-Q1".to_string(),
            "quarter:2026-Q2".to_string(),
            "quarter:2026-Q3".to_string(),
            "quarter:2026-Q4".to_string(),
            "sealed:s1".to_string(),
            "sealed:s2".to_string(),
            "world".to_string(),
        ],
        "and the holder is served every one of them, so the assertion above is about the gate"
    );

    let outside: Value = post_item(&served.server, &outsider, id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        outside["scoped"],
        json!({}),
        "no family of a group this principal cannot reach, under any key"
    );
    let inside: Value = post_item(&served.server, &holder, id)
        .await
        .json()
        .await
        .unwrap();
    // Every family with a per-view value column, keyed by the group's key. `note` is `text` and
    // has no per-entity slot to read, so it is the one declared family absent here.
    let mut families: Vec<&String> = inside["scoped"].as_object().unwrap().keys().collect();
    families.sort();
    assert_eq!(families, ["glow", "mood", "sentiment", "tint"]);
    for (ordinal, (key, _)) in SEALED.iter().enumerate() {
        assert_eq!(
            inside["scoped"]["mood"][key], json!(mood(ordinal, ENTITY)),
            "a category arrives as its vocabulary key, under the key of the view that holds it"
        );
        assert_eq!(inside["scoped"]["tint"][key], json!(tint(ordinal, ENTITY)));
        assert_eq!(
            inside["scoped"]["sentiment"][key],
            json!(sentiment(ordinal, ENTITY)),
            "an absence is an absent key, never a null"
        );
        assert_eq!(inside["scoped"]["glow"][key], json!(glow(ordinal, ENTITY)));
    }
}


// ---------------------------------------------------------------------------------------------
// A sealed owner shared under a public roster — the shape where the family's own gate is the
// only thing standing (`views.md` §3.3, §5, §6)
// ---------------------------------------------------------------------------------------------

/// The entity this fixture asks about: `public`, so item visibility decides nothing here; held by
/// every view of the fixture; and carrying a `sentiment` value under **both** of `sealed`'s keys,
/// so the key whose owning view is gated is served with a value rather than absent for a second
/// reason.
const SHARED_ENTITY: u64 = 14;

/// A points file for the shared-sealed fixture: geometry, the access label, and — for a view of
/// `sealed` itself — that view's own `sentiment`.
fn write_shared_points(path: &Path, view: &str, ids: std::ops::Range<u64>, scoped: Option<usize>) {
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

/// **A group-gated owner whose views a *public* group shares** (`views.md` §3.3): `sealed` is
/// gated on `finance` and carries the `sentiment` family; `sealed_map` declares `members` of it,
/// is public, and its two views are public. `sealed:s2` additionally carries a view gate of its
/// own, so the fixture holds a key reachable through the sharer and not through its owner.
///
/// This is the shape that separates the two gates a scoped family passes. A principal failing
/// `sealed`'s **group** gate still reaches `sealed_map:s1` and `sealed_map:s2`; `owning_key_of`
/// resolves each to the owner's key, the family's `views` list holds that key, and every *per-view*
/// test passes. Only the family's own group gate refuses them.
fn build_shared_sealed(dir: &Path) -> std::path::PathBuf {
    let world_points = dir.join("shared-world.parquet");
    write_shared_points(&world_points, "world", 0..20, None);
    let mut views = vec![view_args("world", &world_points, None)];

    let mut family_views = Vec::new();
    for (ordinal, (key, members)) in SEALED.iter().enumerate() {
        let id = format!("sealed:{key}");
        let points = dir.join(format!("shared-sealed-{key}.parquet"));
        write_shared_points(&points, &id, members.clone(), Some(ordinal));
        family_views.push(views.len());
        // `s2` carries a view gate inside the group, so a `finance` holder reaches its key only
        // through the public sharer.
        let gate: Option<&[&str]> = (*key == "s2").then_some(&["legal"]);
        views.push(view_args(&id, &points, gate));
    }
    // The sharer's views: public, a different layout over the same keys, and carrying no scoped
    // column of their own — the column is the owner's and is reached through the key.
    for (key, members) in SEALED.iter() {
        let id = format!("sealed_map:{key}");
        let points = dir.join(format!("shared-map-{key}.parquet"));
        write_shared_points(&points, &id, members.clone(), None);
        views.push(view_args(&id, &points, None));
    }

    let out = dir.join("shared-bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![
            GroupDescriptor {
                title: None,
                name: "sealed".to_string(),
                members_of: None,
                point_default: Some("public".to_string()),
                visibility: Some(labels(&["finance"])),
                views: roster(&[("s1", None), ("s2", Some(&["legal"]))]),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
            GroupDescriptor {
                title: None,
                name: "sealed_map".to_string(),
                members_of: Some("sealed".to_string()),
                point_default: Some("public".to_string()),
                // **Public, and nothing requires it to agree with the owner's gate.** Publishing a
                // second layout of someone else's quarters is the ordinary reason to declare
                // `members`, and the sharer's own audience is its own question.
                visibility: None,
                views: roster(&SEALED.map(|(key, _)| (key, None))),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
        ],
        scoped_attributes: vec![sealed_family(
            Attribute {
                name: "sentiment".to_string(),
                title: None,
                field: None,
                ty: ScalarType::F32,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: true,
                render: true,
            },
            family_views,
        )],
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
    .expect("a sealed owner shared under a public roster builds");
    out
}

async fn serve_shared() -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_shared_sealed(tmp.path());
    let server = spawn_server(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    Served { server, _tmp: tmp }
}

/// **The family's own group gate is tested, and the per-view gate does not stand in for it**
/// (`views.md` §5, §6; owner ruling 2026-09-01).
///
/// The regression this pins: `scoped_values_of` tested `contains_view` and resolved keys through
/// §3.3's ownership rule, and never tested `contains_group`. A principal failing `sealed`'s group
/// gate reaches `sealed_map`'s public views, each of which resolves to one of `sealed`'s keys —
/// so every per-view test passed and the drill-down served the sealed family's **name** and its
/// **values** to a principal for whom `/v1/meta` omits the family from both its lists and a filter
/// leaf naming it takes the unknown-column `422`. That is the collapse this file exists to hold,
/// arriving on the one surface that had not been given the test.
///
/// Three principals, because the two gates must be seen to be different:
///
/// - the **outsider** fails the group gate: the family is absent whole, though they can reach two
///   views that hold its columns;
/// - the **`finance` holder** passes the group gate and fails `sealed:s2`'s own view gate: they
///   are served both keys, `s2`'s through the public sharer's view of it, which is the per-view
///   case and is correct — the key is a view's address and they hold a view of it;
/// - the **`finance, legal` holder** reaches every view, which is what makes the first two
///   assertions about the gates rather than about an empty fixture.
///
/// ⊘ **The record's home view is not asserted here**, and cannot be from a build: `Engine::item`
/// prefers a row of a view the principal may reach, but home 1 reads the *declared* scalars, which
/// are entity space — one value per entity, permuted into every view's tail — so no build can give
/// two views different values for one entity. The difference arises only from a **join** at
/// ingest, whose row carries geometry and nothing else (`views.md` §4). A join was written for
/// this fixture and withdrawn: it is accepted with a `200` and its flush never completes and never
/// fails, which wedges the executor's flush loop — a write-path finding, reported rather than
/// worked around here.
#[tokio::test]
async fn a_sealed_familys_values_need_the_groups_gate_and_not_only_a_reachable_view() {
    let served = serve_shared().await;
    let outsider = token(&served, &[]).await;
    let holder = token(&served, &["finance"]).await;
    let both = token(&served, &["finance", "legal"]).await;
    assert_eq!(label_of(SHARED_ENTITY), "public");
    let id = id_of(&served, &outsider, "world", SHARED_ENTITY).await;

    let outside: Value = post_item(&served.server, &outsider, id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        outside["scoped"],
        json!({}),
        "the family belongs to a group this principal cannot reach, and reaching two views that \
         hold its columns is not the same question"
    );
    assert_eq!(
        item_views(&served, &outsider, id).await,
        vec![
            "sealed_map:s1".to_string(),
            "sealed_map:s2".to_string(),
            "world".to_string(),
        ],
        "the sharer's views are public and the owner's are not"
    );

    // The holder: the group is reachable, so the family is — and `s2`'s key comes through the
    // public sharer though its owner's view is gated.
    let inside: Value = post_item(&served.server, &holder, id)
        .await
        .json()
        .await
        .unwrap();
    // Every key the fixture holds a value under — `s2`'s absence for this entity is an absent
    // key rather than a null, the same rule the record's fields follow.
    let expected: serde_json::Map<String, Value> = SEALED
        .iter()
        .enumerate()
        .filter_map(|(ordinal, (key, _))| {
            sentiment(ordinal, SHARED_ENTITY).map(|v| (key.to_string(), json!(v)))
        })
        .collect();
    assert_eq!(
        inside["scoped"]["sentiment"],
        Value::Object(expected),
        "each key carrying its own view's value, `s2`'s reached through the public sharer"
    );
    // The fixture's premise: `s2` is the key whose owning view this principal cannot reach, and
    // it carries a value — so the assertion above is about the sharer resolving the key and not
    // about an entity that has nothing under it.
    assert!(sentiment(1, SHARED_ENTITY).is_some());
    assert!(sentiment(0, SHARED_ENTITY).is_some());
    assert_eq!(
        item_views(&served, &holder, id).await,
        vec![
            "sealed:s1".to_string(),
            "sealed_map:s1".to_string(),
            "sealed_map:s2".to_string(),
            "world".to_string(),
        ],
        "`sealed:s2` fails its own view gate; its key does not"
    );
    assert!(item_views(&served, &both, id)
        .await
        .contains(&"sealed:s2".to_string()));
}
