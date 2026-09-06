//! **A group-scoped attribute answers filters** (`views.md` §5): one family, one column per view,
//! and a leaf that resolves to exactly one of them.
//!
//! The build has written the family since 2026-08-31 — one entity-space column per view of the
//! group, each with its own presence bitmap — and nothing read it. What is at stake here is the
//! half above that: which column a leaf reads, and what happens when nothing decides.
//!
//! - **Under a view of the group the request's own view decides**, and under a group *sharing*
//!   those views it decides the same column — `quarter_alt:2026-Q3` filters by `quarter`'s Q3
//!   values, because the two groups are two layouts over one key set (`views.md` §3.3). Nothing is
//!   added to the wire in either case.
//! - **Under any other view the leaf must pin one**, by key — a view's only address — and the
//!   pinned bitmap is an ordinary entity-space one that composes with everything else: *the documents that were
//!   negative in Q3, on the whole-corpus map*.
//! - **An unpinned leaf there is a `422` naming the group**, not an empty answer — a leaf with no
//!   column to read is a malformed request rather than a constraint — and a pin naming no view of
//!   the group is the `404` an unknown view already gets.
//!
//! Every count below is a **masked** count: the resolved column answers a bitmap in entity space
//! and the mask meets it there, before any permutation, which is the whole of why a scoped
//! attribute changes nothing about I2 (`views.md` §5, decision 0109).
//!
//! The expected sets are computed from the same per-view arrays this file writes into the
//! parquets, so a disagreement is between the served answer and the data rather than between two
//! copies of a rule.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
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

const ENTITIES: u64 = 30;
/// The plain view holds the first twenty; each quarter holds its own slice. `world` overlaps every
/// quarter partially, which is what makes the projection case below say something: a pinned leaf
/// on the whole-corpus map answers over the entities in *both*.
const WORLD: std::ops::Range<u64> = 0..20;
const QUARTERS: [(&str, std::ops::Range<u64>); 4] = [
    ("2026-Q1", 0..15),
    ("2026-Q2", 10..30),
    ("2026-Q3", 5..25),
    ("2026-Q4", 8..28),
];

/// The threshold every `range` below uses. Chosen so that each quarter's matching set is a proper,
/// non-empty subset of its population — a filter that held everything or nothing would pass
/// against the wrong column as readily as the right one.
const THRESHOLD: f64 = 0.5;

/// **`sentiment`, per view and per entity** — the values written into each quarter's points file,
/// and the only definition of what the served answer is checked against.
///
/// Two properties carry the file: an entity's value **differs between quarters**, so reading Q4's
/// column where Q3's was asked for is observable; and one entity in three carries **no value at
/// all** in a given quarter, which is the presence bitmap's ordinary case (decision 0064) rather
/// than a hole to fill.
fn group_frame() -> Quantisation {
    let e = extent();
    Quantisation {
        x_min: e.x_min,
        x_max: e.x_max,
        y_min: e.y_min,
        y_max: e.y_max,
    }
}

fn sentiment(slot: usize, entity: u64) -> Option<f32> {
    if (entity + slot as u64).is_multiple_of(3) {
        return None;
    }
    // A different phase per quarter: the same entity is above the threshold in one and below it in
    // another, so the two columns disagree about almost every entity that has both.
    let phase = (entity * 7 + slot as u64 * 11) % 10;
    Some(phase as f32 / 10.0)
}

/// **`heat`, per view and per entity** — the second family, declared `render = true` and
/// `index = false`, whose whole licence to be an operand is `render` (`views.md` §5 r26). Its
/// values are deliberately *not* `sentiment`'s: a leaf resolved to the wrong family's column would
/// otherwise answer the right set by accident.
fn heat(slot: usize, entity: u64) -> Option<f32> {
    if (entity + slot as u64).is_multiple_of(4) {
        return None;
    }
    let phase = (entity * 3 + slot as u64 * 5) % 10;
    Some(phase as f32 / 10.0)
}

/// How many entities a quarter's view holds.
fn population(slot: usize) -> usize {
    (QUARTERS[slot].1.end - QUARTERS[slot].1.start) as usize
}

/// The entities of a quarter whose value clears the threshold — the expected answer, from the same
/// arrays the parquet carries.
fn matching(slot: usize) -> BTreeSet<u64> {
    QUARTERS[slot]
        .1
        .clone()
        .filter(|&e| sentiment(slot, e).is_some_and(|v| f64::from(v) >= THRESHOLD))
        .collect()
}

/// The entities of a quarter whose **`heat`** clears the threshold.
fn matching_heat(slot: usize) -> BTreeSet<u64> {
    QUARTERS[slot]
        .1
        .clone()
        .filter(|&e| heat(slot, e).is_some_and(|v| f64::from(v) >= THRESHOLD))
        .collect()
}

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

/// A points file, with this view's own `sentiment` column where the family reads one from it.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, slot: Option<usize>) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ];
    if slot.is_some() {
        // Nullable: a null and a row this view does not carry are the same state, absent.
        fields.push(Field::new("sentiment", DataType::Float32, true));
        fields.push(Field::new("heat", DataType::Float32, true));
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
    ];
    if let Some(slot) = slot {
        columns.push(Arc::new(Float32Array::from(
            ids.iter().map(|&e| sentiment(slot, e)).collect::<Vec<_>>(),
        )));
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

/// The bundle: one plain view, a group of four quarters each carrying its own `sentiment` column,
/// and a second group over the same four keys with its own layout and no values of its own — the
/// family belongs to the group that owns the views (`views.md` §3.3, §5).
fn build_scoped(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_points(&world_points, "world", WORLD, None);
    let mut views = vec![view_args("world", &world_points, &pairs)];
    let mut family_views = Vec::new();
    for group in ["quarter", "quarter_alt"] {
        for (slot, (key, members)) in QUARTERS.iter().enumerate() {
            let id = format!("{group}:{key}");
            let points = dir.join(format!("{group}-{key}.parquet"));
            // Only the owning group's files carry the values; `quarter_alt` is a second geometry
            // over the same keys and has no column family of its own.
            let carries = (group == "quarter").then_some(slot);
            write_points(&points, &id, members.clone(), carries);
            if carries.is_some() {
                family_views.push(views.len());
            }
            views.push(view_args(&id, &points, &pairs));
        }
    }
    let roster = || {
        QUARTERS
            .iter()
            .map(|(key, _)| GroupViewDescriptor {
                key: key.to_string(),
                visibility: None,
                metadata: Default::default(),
            })
            .collect::<Vec<_>>()
    };
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
                views: roster(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                // Derived at the manifest write from `scoped_attributes` below, so the family list
                // has one origin.
                scoped_scalars: Vec::new(),
            },
            GroupDescriptor {
                title: None,
                point_default: Some("public".to_string()),
                visibility: None,
                name: "quarter_alt".to_string(),
                members_of: Some("quarter".to_string()),
                views: roster(),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                scoped_scalars: Vec::new(),
            },
        ],
        scoped_attributes: vec![
            ScopedColumnFamily {
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
                group: "quarter".to_string(),
                views: family_views.clone(),
                // No `source` of its own: each view's column is read from that view's own points
                // file, which is the shape most declarations want (`views.md` §5).
                source: None,
            },
            // **The same family, licensed by `render` instead of `index`** (`views.md` §5 r26).
            // It is declared beside the indexed one so that every resolution case below can be
            // asked of both, the two being one rule since the asymmetry closed.
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
                views: family_views,
                source: None,
            },
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
        schema: Default::default(),
    })
    .expect("a nine-view build with one scoped family succeeds");
    out
}

struct Served {
    server: TestServer,
    /// Every term, so the mask is the whole corpus and a count is the view's population rather
    /// than a principal's slice of it. The narrower principal is authorised per test.
    token: String,
    _tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_scoped(tmp.path());
    let server = spawn_server(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0", "1"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    Served {
        server,
        token,
        _tmp: tmp,
    }
}

async fn viewport(
    served: &Served,
    token: &str,
    view: &str,
    filters: Option<Value>,
) -> reqwest::Response {
    let mut body = json!({"view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200});
    if let Some(filters) = filters {
        body["filters"] = filters;
    }
    served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// The `tessera_id`s a view answers with under a filter — an **entity** set, since an identifier is
/// the entity's wherever it appears (`views.md` §1), which is what lets two views' answers be
/// compared directly.
async fn ids(served: &Served, view: &str, filters: Option<Value>) -> BTreeSet<u64> {
    let resp = viewport(served, &served.token, view, filters).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{view} answers: {}",
        resp.text().await.unwrap_or_default()
    );
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

/// A `range` over `sentiment`, spelt as a client would: bare, or pinned to a view of the group.
fn range(leaf: &str) -> Value {
    json!({leaf: {"range": {"gte": THRESHOLD}}})
}

/// **The request's own view decides, and it decides between columns that disagree.**
///
/// Under `quarter:2026-Q3` a bare `sentiment` leaf answers Q3's values — the count is that
/// quarter's own, and it is not Q4's, which is the failure a resolution that read the first column
/// of the family (or the last) would show.
#[tokio::test]
async fn a_bare_leaf_under_a_view_of_the_group_reads_that_views_column() {
    let served = serve().await;
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let matched = ids(&served, &view, Some(range("sentiment"))).await;
        assert_eq!(
            matched.len(),
            matching(slot).len(),
            "{view} filters by its own column: expected {} of {} rows",
            matching(slot).len(),
            population(slot)
        );
        assert!(
            !matched.is_empty() && matched.len() < population(slot),
            "{view}'s predicate is a proper subset, or this test proves nothing"
        );
    }
    // The columns genuinely disagree: no two quarters would answer the same count by accident.
    let counts: BTreeSet<usize> = (0..QUARTERS.len()).map(|o| matching(o).len()).collect();
    assert!(
        counts.len() > 1,
        "the fixture's quarters must not all match the same number of entities"
    );
}

/// **A group sharing the views shares the column** (`views.md` §3.3, §5): `quarter_alt:2026-Q3` is
/// a second layout over `quarter:2026-Q3`'s key, so a bare leaf there reads the same Q3 column —
/// the same entities, drawn elsewhere.
#[tokio::test]
async fn a_bare_leaf_under_a_sharing_group_reads_the_owners_column() {
    let served = serve().await;
    assert_eq!(
        ids(&served, "quarter:2026-Q3", Some(range("sentiment"))).await,
        ids(&served, "quarter_alt:2026-Q3", Some(range("sentiment"))).await,
        "one membership, one column, two geometries"
    );
}

/// **A pinned leaf is an ordinary entity-space bitmap and composes** (`views.md` §5): on the
/// whole-corpus map, `sentiment@2026-Q3` holds exactly the entities `world` and the Q3-filtered
/// view both hold. Asserted as a set identity between two served answers, so nothing here depends
/// on a second reading of the fixture.
#[tokio::test]
async fn a_pin_projects_one_views_column_into_another_views_rows() {
    let served = serve().await;
    let pinned = ids(&served, "world", Some(range("sentiment@2026-Q3"))).await;
    let world = ids(&served, "world", None).await;
    let q3 = ids(&served, "quarter:2026-Q3", Some(range("sentiment"))).await;
    let both: BTreeSet<u64> = world.intersection(&q3).copied().collect();
    assert_eq!(
        pinned, both,
        "the entities in `world` that were positive in Q3"
    );
    assert!(
        !pinned.is_empty() && pinned.len() < world.len(),
        "the projection is a proper, non-empty subset of the plain view"
    );
}

/// **A pin under a view of the same group is allowed and means what it says**: Q4's map, filtered
/// by Q3's sentiment. It is not Q4's own answer, which is what makes it worth spelling.
#[tokio::test]
async fn a_pin_under_a_sibling_view_reads_the_pinned_column() {
    let served = serve().await;
    let q4_by_q3 = ids(&served, "quarter:2026-Q4", Some(range("sentiment@2026-Q3"))).await;
    let q4_by_q4 = ids(&served, "quarter:2026-Q4", Some(range("sentiment"))).await;
    assert_ne!(
        q4_by_q3, q4_by_q4,
        "a pin overrides the request's own view, or the pin is doing nothing"
    );
    let q4_rows = ids(&served, "quarter:2026-Q4", None).await;
    let q3 = ids(&served, "quarter:2026-Q3", Some(range("sentiment"))).await;
    assert_eq!(
        q4_by_q3,
        q4_rows.intersection(&q3).copied().collect::<BTreeSet<_>>(),
        "Q4's rows that were positive in Q3"
    );
}

/// **An unpinned leaf under a view that decides nothing is a `422` naming the group** — a leaf with
/// no column to read is a malformed request rather than a constraint, so it must not be answered
/// as an empty filter or ignored as an absent one.
#[tokio::test]
async fn a_bare_leaf_under_an_unrelated_view_is_a_422_naming_the_group() {
    let served = serve().await;
    let resp = viewport(&served, &served.token, "world", Some(range("sentiment"))).await;
    assert_eq!(resp.status().as_u16(), 422);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract", "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("quarter"), "it names the group: {detail}");
    assert!(
        detail.contains("sentiment@"),
        "it says how to pin one: {detail}"
    );
}

/// **A pin naming no view of the group is the `404` an unknown view gets** — an undeclared key,
/// and an id in the retired `#<ordinal>` form, which now names a key nobody declared and is
/// nothing else (decision 0113). The detail says no more than that. The gate is unbuilt
/// (`views.md` §6), so nothing is filtered out of the roster today; when it lands, a gate-failed
/// pin joins these here rather than earning a code of its own.
#[tokio::test]
async fn a_pin_naming_nothing_is_the_unknown_view_404() {
    let served = serve().await;
    for pin in ["2099-Q9", "#3"] {
        let leaf = format!("sentiment@{pin}");
        let resp = viewport(&served, &served.token, "world", Some(range(&leaf))).await;
        assert_eq!(resp.status().as_u16(), 404, "{leaf}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "unknown", "{body}");
        assert_eq!(
            body["detail"],
            format!("unknown view '{pin}' of group 'quarter'"),
            "{body}"
        );
    }
}

/// **Absence is absence, in a predicate and in its negation** (decision 0064): an entity carrying no
/// Q3 value is in no Q3 answer. `none_of` subtracts from the set of entities *carrying* a value, so
/// the negation of a predicate covering the whole domain is empty rather than "everyone with no
/// value" — which is the difference between a filter and a presence test.
#[tokio::test]
async fn an_entity_with_no_value_matches_no_predicate_of_that_view() {
    let served = serve().await;
    let rows = ids(&served, "quarter:2026-Q3", None).await;
    let whole_domain = json!({"sentiment": {"range": {"gte": -1.0, "lte": 2.0}}});
    let present = ids(&served, "quarter:2026-Q3", Some(whole_domain.clone())).await;
    let with_value = QUARTERS[2]
        .1
        .clone()
        .filter(|&e| sentiment(2, e).is_some())
        .count();
    assert_eq!(
        present.len(),
        with_value,
        "a predicate over the whole domain holds exactly the entities with a value"
    );
    assert!(
        present.len() < rows.len(),
        "the fixture leaves some of Q3's entities valueless, or this proves nothing"
    );
    let negated = ids(
        &served,
        "quarter:2026-Q3",
        Some(json!({"none_of": [whole_domain]})),
    )
    .await;
    assert!(
        negated.is_empty(),
        "a valueless entity is not resurrected by a negation: {negated:?}"
    );
}

/// **I12's direction, over a pin**: a filter may move the frontier up and never down. The same
/// principal, the same view, the same mask — the pinned answer is a subset of the unfiltered one,
/// and the narrower principal's is a subset of the wider one's.
#[tokio::test]
async fn a_filtered_count_never_exceeds_the_unfiltered_one_on_the_same_mask() {
    let served = serve().await;
    // The narrower principal: `terms_of` gives term 1 to one entity in three.
    let narrow = authorise(&served.server, &["1"]).await;
    let narrow = narrow["token"].as_str().unwrap().to_string();
    for view in ["world", "quarter:2026-Q3"] {
        for token in [&served.token, &narrow] {
            let unfiltered = viewport(&served, token, view, None).await;
            let filtered = viewport(&served, token, view, Some(range("sentiment@2026-Q3"))).await;
            assert_eq!(unfiltered.status().as_u16(), 200);
            assert_eq!(filtered.status().as_u16(), 200);
            let (_, all) = decode_viewport(&unfiltered.bytes().await.unwrap());
            let (_, some) = decode_viewport(&filtered.bytes().await.unwrap());
            let all: BTreeSet<u64> = all.into_iter().map(|(id, _)| id).collect();
            let some: BTreeSet<u64> = some.into_iter().map(|(id, _)| id).collect();
            assert!(
                some.is_subset(&all),
                "{view}: a filter selects within the mask, never beyond it"
            );
        }
    }
}

/// **`/v1/meta` says the leaf is scoped, and to what** (`views.md` §5, §11). Without the `scope` a
/// client has no way to know the bare leaf is answerable only under some views — it would meet the
/// `422` on its first request from the whole-corpus map.
#[tokio::test]
async fn the_operand_list_carries_the_scope() {
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
    let operands = body["filter_operands"].as_array().expect("an operand list");
    let sentiment = operands
        .iter()
        .find(|o| o["column"] == "sentiment")
        .expect("the scoped family is on the filter surface");
    assert_eq!(sentiment["family"], "numeric");
    assert_eq!(sentiment["operands"], json!(["eq", "in", "range"]));
    assert_eq!(sentiment["scope"], json!({"group": "quarter"}));
}

// ---------------------------------------------------------------------------------------------
// The same rule, licensed by `render` (`views.md` §5 r26)
// ---------------------------------------------------------------------------------------------

/// **A family declared `render = true` and `index = false` resolves exactly as the indexed one
/// does** — bare under a view of the group, pinned anywhere else — because the licence is now
/// `index` *or* `render` and the column both spellings read is the same per-view entity-space one.
///
/// `heat`'s values are not `sentiment`'s, so an answer that came from the wrong family's column
/// would be a different set rather than the same one.
#[tokio::test]
async fn a_render_only_family_resolves_exactly_as_the_indexed_one_does() {
    let served = serve().await;
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let rendered = ids(&served, &view, Some(range("heat"))).await;
        assert_eq!(
            rendered.len(),
            matching_heat(slot).len(),
            "{view} filters by its own column of the rendered family: expected {} of {} rows",
            matching_heat(slot).len(),
            population(slot)
        );
        let indexed = ids(&served, &view, Some(range("sentiment"))).await;
        assert_ne!(
            rendered, indexed,
            "{view}: and it is `heat`'s column, not the indexed family's"
        );
    }
}

/// **A pin of a render-only family projects into a view that holds no lane for it** — the
/// property route (a) buys. `world` is outside the group, so its rows carry no `heat` at all, and
/// `heat@2026-Q3` there is exactly the entities `world` and Q3's own filtered answer share.
/// Asserted as a set identity between two served answers, so nothing depends on a second reading
/// of the fixture.
#[tokio::test]
async fn a_pin_of_a_render_only_family_projects_into_a_view_with_no_lane() {
    let served = serve().await;
    let projected = ids(&served, "world", Some(range("heat@2026-Q3"))).await;
    let world_rows = ids(&served, "world", None).await;
    let q3 = ids(&served, "quarter:2026-Q3", Some(range("heat"))).await;
    assert_eq!(
        projected,
        world_rows
            .intersection(&q3)
            .copied()
            .collect::<BTreeSet<_>>()
    );
    assert!(
        !projected.is_empty() && projected.len() < world_rows.len(),
        "a proper, non-empty subset, or the assertion above holds vacuously"
    );
}

/// **The refusals are the same two**, since the resolution is one site: a bare leaf where nothing
/// decides the view names the group, and a pin naming no view of it is the unknown-view `404`.
#[tokio::test]
async fn a_render_only_familys_refusals_are_the_indexed_ones() {
    let served = serve().await;
    let resp = viewport(&served, &served.token, "world", Some(range("heat"))).await;
    assert_eq!(resp.status().as_u16(), 422);
    let body: Value = resp.json().await.unwrap();
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("quarter"), "it names the group: {detail}");
    assert!(detail.contains("heat@"), "it says how to pin one: {detail}");

    let resp = viewport(&served, &served.token, "world", Some(range("heat@2099-Q9"))).await;
    assert_eq!(resp.status().as_u16(), 404);
}

/// **`/v1/meta` publishes it as an operand, with its family's operators and its scope** — the
/// discovery half of the same rule. A client cannot tell the two families apart by their entries,
/// which is the point: `index` decides where the value is stored, not whether it is filterable.
#[tokio::test]
async fn meta_publishes_the_render_only_family_as_an_operand() {
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
    let operands = body["filter_operands"].as_array().unwrap();
    let entry = |column: &str| -> Value {
        operands
            .iter()
            .find(|e| e["column"] == column)
            .unwrap_or_else(|| panic!("{column} is an operand: {operands:?}"))
            .clone()
    };
    let mut indexed = entry("sentiment");
    let mut rendered = entry("heat");
    assert_eq!(rendered["family"], "numeric");
    assert_eq!(rendered["operands"], json!(["eq", "in", "range"]));
    assert_eq!(rendered["scope"]["group"], "quarter");
    indexed["column"] = json!(null);
    rendered["column"] = json!(null);
    assert_eq!(
        indexed, rendered,
        "the two entries differ only in the column's name"
    );
}
