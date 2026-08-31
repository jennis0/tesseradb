//! **The category and text families of a group-scoped attribute** (`views.md` §5), and a scoped
//! attribute reading its own source.
//!
//! The numeric family reached the filter surface first and `scoped_filter.rs` covers what a leaf
//! resolves to. What is at stake here is the two families that owe more than a value column, and
//! the second way a family's values can arrive:
//!
//! - a **category** owes per-view postings, which are both what an `eq` is answered from on a
//!   `public` vocabulary and what `/v1/categories` derives a value list from on a `derived` one —
//!   so the value list is *per view*, and two views of one group offer different sets;
//! - a **text** column owes a per-view token dictionary and postings and no value column at all,
//!   so `match` under one view answers from that view's prose;
//! - a family declaring its **own `source`** carries one row per `(entity, view)`, its `fields.view`
//!   discriminator saying which view each row's value is for.
//!
//! Every expected set below is computed from the same per-view functions this file writes into the
//! parquets, so a disagreement is between the served answer and the data rather than between two
//! copies of a rule. Every count is a **masked** count: the resolved column answers a bitmap in
//! entity space and the mask meets it there, before any permutation (I2, I7).

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::config::{
    Attribute, Fields, Schema, ScopedAttributeFile, ValueSet, Visibility, Vocabulary,
};
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation, ScopedColumnFamily,
    ViewArgs,
};
use tessera_spatial::tiler::ScalarType;

/// The analyser identity the manifest records for the text family — the same string the build
/// resolves from a `[[attribute]]` declaring none, spelt here because this fixture is built
/// programmatically.
const ANALYSER: &str = "unicode/icu4x-2.2/p1";

const ENTITIES: u64 = 30;
/// The plain view holds the first twenty; each quarter holds its own slice, so a pin on the
/// whole-corpus map answers over the entities in both.
const WORLD: std::ops::Range<u64> = 0..20;
const QUARTERS: [(&str, std::ops::Range<u64>); 4] = [
    ("2026-Q1", 0..15),
    ("2026-Q2", 10..30),
    ("2026-Q3", 5..25),
    ("2026-Q4", 8..28),
];

/// `mood`'s value set: closed, `public`, so its list is authored and its filter route is the
/// postings one.
const MOODS: [&str; 3] = ["calm", "tense", "wild"];

/// `sector`'s value set: closed, `derived`, so a value is offered only where a visible entity
/// carries it — which makes the list a **per-view** answer over the per-view postings.
///
/// `rare` is carried by exactly **one** entity in each quarter ([`rare_entity`]), which is what
/// makes the derivation observable at the value: one entity leaving the principal's visible set —
/// because it was suppressed, or because the principal never held its term — takes that value off
/// the list and no other. A value several entities carry cannot tell a derivation that works from
/// one that returns the authored set.
const SECTORS: [&str; 4] = ["north", "south", "east", "rare"];

/// The one entity carrying `rare` in a quarter. `+ 1` off the low end, so it is neither a multiple
/// of 5 (which [`sector`] gives no value at all) nor, for `2026-Q1`, an entity the narrow
/// principal below can see — `terms_of` grants term 1 on `e % 3 == 0`.
fn rare_entity(slot: usize) -> u64 {
    QUARTERS[slot].1.start + 1
}

/// The word every quarter's prose carries, one per quarter, so a `match` that read the wrong
/// view's postings answers the empty set rather than a plausible one.
const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

/// **`mood`, per view and per entity.** An entity's value differs between quarters, so reading
/// Q4's column where Q3's was asked for is observable; one entity in four carries no value at all
/// in a given quarter, which is the presence bitmap's ordinary case (decision 0064).
fn mood(slot: usize, entity: u64) -> Option<&'static str> {
    if (entity + slot as u64).is_multiple_of(4) {
        return None;
    }
    Some(MOODS[((entity + slot as u64 * 2) % 3) as usize])
}

/// **`sector`, per view** — and deliberately not onto the whole value set in every view: a quarter
/// uses two of the three, and which two rotates, so a value list derived from Q1's postings differs
/// from one derived from Q3's.
fn sector(slot: usize, entity: u64) -> Option<&'static str> {
    if entity == rare_entity(slot) {
        return Some("rare");
    }
    if entity.is_multiple_of(5) {
        return None;
    }
    let pair = [slot % 3, (slot + 1) % 3];
    Some(SECTORS[pair[(entity % 2) as usize]])
}

/// **`note`, per view** — prose carrying this quarter's own word, for half the entities.
fn note(slot: usize, entity: u64) -> Option<String> {
    if (entity + slot as u64).is_multiple_of(2) {
        return None;
    }
    Some(format!("the {} report for entity {entity}", WORDS[slot]))
}

/// **`score`, per view** — the family with a source of its own, one row per `(entity, view)` in a
/// single file discriminated by `fields.view`.
fn score(slot: usize, entity: u64) -> Option<f32> {
    if (entity + slot as u64).is_multiple_of(3) {
        return None;
    }
    Some(((entity * 7 + slot as u64 * 11) % 10) as f32 / 10.0)
}

/// The threshold `score`'s `range` uses — a proper, non-empty subset of every quarter's
/// population, so a filter that held everything or nothing would pass against the wrong column as
/// readily as the right one.
const THRESHOLD: f64 = 0.5;

fn members(slot: usize) -> std::ops::Range<u64> {
    QUARTERS[slot].1.clone()
}

/// The entities of a quarter whose `mood` is `value` — the expected answer, from the same function
/// the parquet was written from.
fn with_mood(slot: usize, value: &str) -> BTreeSet<u64> {
    members(slot)
        .filter(|&e| mood(slot, e) == Some(value))
        .collect()
}

/// The entities of a quarter whose prose carries `word`.
fn with_word(slot: usize, word: &str) -> BTreeSet<u64> {
    members(slot)
        .filter(|&e| note(slot, e).is_some_and(|prose| prose.contains(word)))
        .collect()
}

/// The entities of a quarter whose `score` clears the threshold.
fn over_threshold(slot: usize) -> BTreeSet<u64> {
    members(slot)
        .filter(|&e| score(slot, e).is_some_and(|v| f64::from(v) >= THRESHOLD))
        .collect()
}

/// The `sector` values a quarter's entities carry at all — what a `derived` list may offer under
/// that view, before any mask narrows it.
fn sectors_in(slot: usize) -> BTreeSet<&'static str> {
    sectors_visible_to(slot, &|_| true)
}

/// The `sector` values a quarter's entities carry **that this principal can see** — §3.3's
/// membership predicate, written from the fixture's own arrays.
fn sectors_visible_to(slot: usize, visible: &dyn Fn(u64) -> bool) -> BTreeSet<&'static str> {
    members(slot)
        .filter(|&e| visible(e))
        .filter_map(|e| sector(slot, e))
        .collect()
}

/// The entities a principal holding term `1` alone can see — `common::terms_of` grants it on
/// `e % 3 == 0` and term `0` on everything, so this is a proper, non-trivial slice of the corpus.
fn narrow_mask(entity: u64) -> bool {
    entity.is_multiple_of(3)
}

/// A view's own layout: the same entity sits somewhere different in each.
fn position(view: &str, e: u64) -> (f64, f64) {
    match view.split_once(':') {
        None => ((e % 5) as f64 * 100.0, (e / 5) as f64 * 100.0),
        Some((_, key)) => (
            900.0 - (e % 5) as f64 * 100.0,
            (e / 5) as f64 * 70.0 + key.len() as f64,
        ),
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

/// A points file. A view of the group carries the three columns its families read from it; the
/// plain view carries none, which is what it means for the family to be the group's.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, slot: Option<usize>) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ];
    if slot.is_some() {
        // Nullable throughout: a null and a row this view does not carry are the same state,
        // absent (decision 0064).
        fields.push(Field::new("mood", DataType::Utf8, true));
        fields.push(Field::new("sector", DataType::Utf8, true));
        fields.push(Field::new("note", DataType::Utf8, true));
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
        columns.push(Arc::new(StringArray::from(
            ids.iter().map(|&e| mood(slot, e)).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(StringArray::from(
            ids.iter().map(|&e| sector(slot, e)).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(StringArray::from(
            ids.iter().map(|&e| note(slot, e)).collect::<Vec<_>>(),
        )));
    }
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// **`score`'s own source**: one row per `(entity, view)`, the view named by a `quarter` column.
/// Rows for the four quarters are interleaved rather than grouped, so a reader that took the
/// file's order for the discriminator would answer wrongly.
fn write_score_source(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("score", DataType::Float32, true),
    ]));
    let mut ids = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    let mut values: Vec<Option<f32>> = Vec::new();
    for entity in 0..ENTITIES {
        for (slot, (key, range)) in QUARTERS.iter().enumerate() {
            if !range.contains(&entity) {
                continue;
            }
            ids.push(entity);
            keys.push(key);
            values.push(score(slot, entity));
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(StringArray::from(keys)),
            Arc::new(Float32Array::from(values)),
        ],
    )
    .unwrap();
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

fn vocabulary(name: &str, values: &[&str], visibility: Visibility) -> Vocabulary {
    Vocabulary {
        name: name.to_string(),
        title: None,
        value_set: ValueSet::Closed,
        visibility,
        width: ScalarType::U8,
        // Codes pinned from one, code 0 being the reserved *absent* one (§3.6).
        codes: values
            .iter()
            .enumerate()
            .map(|(i, key)| (key.to_string(), i as u32 + 1))
            .collect(),
        titles: BTreeMap::new(),
        reserved: Vec::new(),
    }
}

fn scoped(
    attribute: Attribute,
    views: Vec<usize>,
    source: Option<ScopedAttributeFile>,
) -> ScopedColumnFamily {
    ScopedColumnFamily {
        attribute,
        group: "quarter".to_string(),
        views,
        source,
    }
}

fn category(name: &str) -> Attribute {
    Attribute {
        name: name.to_string(),
        title: None,
        field: None,
        ty: ScalarType::U8,
        analyser: None,
        vocabulary: Some(name.to_string()),
        value_set: Some(ValueSet::Closed),
        index: true,
        render: false,
    }
}

/// The bundle: one plain view and one group of four quarters carrying four scoped families — a
/// `public` category, a `derived` category, a text column, and a numeric read from its own source.
fn build_families(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_points(&world_points, "world", WORLD, None);
    let score_source = dir.join("score.parquet");
    write_score_source(&score_source);
    let mut views = vec![view_args("world", &world_points, &pairs)];
    let mut family_views = Vec::new();
    for (slot, (key, range)) in QUARTERS.iter().enumerate() {
        let id = format!("quarter:{key}");
        let points = dir.join(format!("quarter-{key}.parquet"));
        write_points(&points, &id, range.clone(), Some(slot));
        family_views.push(views.len());
        views.push(view_args(&id, &points, &pairs));
    }
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![GroupDescriptor {
            title: None,
            visibility: None,
            name: "quarter".to_string(),
            members_of: None,
            views: QUARTERS
                .iter()
                .map(|(key, _)| GroupViewDescriptor {
                    key: key.to_string(),
                    visibility: None,
                    metadata: Default::default(),
                })
                .collect(),
            quantisation: group_frame(),
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            // Derived at the manifest write from `scoped_attributes` below, so the family list has
            // one origin.
            scoped_scalars: Vec::new(),
        }],
        scoped_attributes: vec![
            scoped(category("mood"), family_views.clone(), None),
            scoped(category("sector"), family_views.clone(), None),
            scoped(
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
                None,
            ),
            scoped(
                // **The render family** (`views.md` §5): its per-view values reach the hot row
                // tail of each quarter and of no other view, which is what
                // `scoped_render.rs` asserts on the wire.
                Attribute {
                    name: "score".to_string(),
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
                Some(ScopedAttributeFile {
                    path: score_source.clone(),
                    entity_id: "entity_id".to_string(),
                    view_field: "quarter".to_string(),
                }),
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
        schema: Schema {
            attributes: Vec::new(),
            vocabularies: HashMap::from([
                (
                    "mood".to_string(),
                    vocabulary("mood", &MOODS, Visibility::Public),
                ),
                (
                    "sector".to_string(),
                    vocabulary("sector", &SECTORS, Visibility::Derived),
                ),
            ]),
        },
    })
    .expect("a five-view build with four scoped families succeeds");
    out
}

struct Served {
    server: TestServer,
    /// Every term, so the mask is the whole corpus and a count is the view's population rather
    /// than a principal's slice of it.
    token: String,
    _tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_families(tmp.path());
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

async fn viewport(served: &Served, view: &str, filters: Option<Value>) -> reqwest::Response {
    let mut body = json!({"view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200});
    if let Some(filters) = filters {
        body["filters"] = filters;
    }
    served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&served.token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// The `tessera_id`s a view answers with under a filter — an **entity** set, since an identifier
/// is the entity's wherever it appears (`views.md` §1).
async fn ids(served: &Served, view: &str, filters: Option<Value>) -> BTreeSet<u64> {
    let resp = viewport(served, view, filters).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{view} answers: {}",
        resp.text().await.unwrap_or_default()
    );
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

async fn categories(served: &Served, path: &str) -> reqwest::Response {
    categories_as(served, &served.token, path).await
}

async fn categories_as(served: &Served, token: &str, path: &str) -> reqwest::Response {
    served
        .server
        .client
        .get(served.server.viewer_url(&format!("/v1/categories/{path}")))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

/// A session holding exactly these label descriptors.
async fn token(served: &Served, terms: &[&str]) -> String {
    authorise(&served.server, terms).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn post_changes(served: &Served, body: &Value) -> reqwest::Response {
    served
        .server
        .client
        .post(served.server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// The keys a value list offers, in the order it offers them.
async fn keys(served: &Served, path: &str) -> Vec<String> {
    keys_as(served, &served.token, path).await
}

async fn keys_as(served: &Served, token: &str, path: &str) -> Vec<String> {
    let resp = categories_as(served, token, path).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{path} answers: {}",
        resp.text().await.unwrap_or_default()
    );
    let body: Value = resp.json().await.unwrap();
    body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap().to_string())
        .collect()
}

/// **The request's own view decides which column a category leaf reads**, and the columns
/// disagree: Q1's `calm` set and Q3's are different sets over the same entity space, and each
/// served answer is its own view's.
#[tokio::test]
async fn a_scoped_category_answers_from_the_requests_own_view() {
    let served = serve().await;
    // The served answer is a set of `tessera_id`s and the expected one a set of entity numbers, so
    // the comparison is by **cardinality**, as every test in this directory compares them: the
    // identifier is a blinding permutation and never the entity id (I10, decision 0014).
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let matched = ids(&served, &view, Some(json!({"mood": {"eq": "calm"}}))).await;
        assert_eq!(
            matched.len(),
            with_mood(slot, "calm").len(),
            "{view} must answer from its own `mood` column"
        );
        assert!(
            !matched.is_empty() && matched.len() < members(slot).count(),
            "{view}'s predicate is a proper subset, or this test proves nothing"
        );
    }
    // The premise: a test that passed against any column of the family would prove nothing.
    let counts: BTreeSet<usize> = (0..QUARTERS.len())
        .map(|slot| with_mood(slot, "calm").len())
        .collect();
    assert!(counts.len() > 1, "the quarters must not agree by accident");
}

/// **A pin reads a named view's column anywhere**, including on a map that is not the group's at
/// all: *the documents that were calm in Q3, on the whole-corpus map*. The answer is Q3's set
/// intersected with the plain view's own population, which is what makes it an ordinary
/// entity-space bitmap composing with everything else.
#[tokio::test]
async fn a_pinned_category_leaf_answers_on_the_plain_map() {
    let served = serve().await;
    let pinned = ids(
        &served,
        "world",
        Some(json!({"mood@2026-Q3": {"eq": "calm"}})),
    )
    .await;
    let world = ids(&served, "world", None).await;
    let q3 = ids(
        &served,
        "quarter:2026-Q3",
        Some(json!({"mood": {"eq": "calm"}})),
    )
    .await;
    // Asserted as a set identity between three **served** answers, so nothing here depends on a
    // second reading of the fixture.
    assert_eq!(
        pinned,
        world.intersection(&q3).copied().collect::<BTreeSet<_>>(),
        "the entities in `world` that were calm in Q3"
    );
    assert!(
        !pinned.is_empty() && pinned.len() < world.len(),
        "the projection is a proper, non-empty subset of the plain view"
    );
}

/// **A bare scoped leaf under a view that decides no column is a `422` naming the group**, for the
/// category and the text families exactly as for the numeric one: a leaf with no column to read is
/// a malformed request rather than a constraint.
#[tokio::test]
async fn a_bare_scoped_leaf_off_the_group_names_the_group() {
    let served = serve().await;
    for filters in [
        json!({"mood": {"eq": "calm"}}),
        json!({"note": {"match": "alpha"}}),
        json!({"score": {"range": {"gte": THRESHOLD}}}),
    ] {
        let resp = viewport(&served, "world", Some(filters.clone())).await;
        assert_eq!(resp.status().as_u16(), 422, "{filters}");
        let body = resp.text().await.unwrap();
        assert!(body.contains("quarter"), "{body}");
    }
}

/// **A text family answers `match` from the view's own postings.** Every quarter's prose carries
/// its own word, so a `match` that read the wrong view's index answers the empty set — which is
/// exactly what the cross-quarter case below asserts it does not.
#[tokio::test]
async fn a_scoped_text_column_matches_per_view() {
    let served = serve().await;
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let own = ids(
            &served,
            &view,
            Some(json!({"note": {"match": WORDS[slot]}})),
        )
        .await;
        assert_eq!(
            own.len(),
            with_word(slot, WORDS[slot]).len(),
            "{view} must match its own prose"
        );
        assert!(!own.is_empty());
        // Another quarter's word is in no document of this one — the index is the view's.
        let other = ids(
            &served,
            &view,
            Some(json!({"note": {"match": WORDS[(slot + 1) % 4]}})),
        )
        .await;
        assert!(other.is_empty(), "{view} matched another quarter's word");
    }
    // And a pin reaches one quarter's prose from the whole-corpus map: the entities `world` holds
    // whose Q3 prose says `gamma`.
    let pinned = ids(
        &served,
        "world",
        Some(json!({"note@2026-Q3": {"match": "gamma"}})),
    )
    .await;
    let world = ids(&served, "world", None).await;
    let q3 = ids(
        &served,
        "quarter:2026-Q3",
        Some(json!({"note": {"match": "gamma"}})),
    )
    .await;
    assert_eq!(
        pinned,
        world.intersection(&q3).copied().collect::<BTreeSet<_>>()
    );
    assert!(!pinned.is_empty());
}

/// **A family with its own `source` is routed by `fields.view`.** One file carries every
/// `(entity, view)` row interleaved; each view's column is the rows whose discriminator is that
/// view's key, and the per-view answers differ.
#[tokio::test]
async fn a_scoped_family_reads_its_own_source_per_view() {
    let served = serve().await;
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let matched = ids(
            &served,
            &view,
            Some(json!({"score": {"range": {"gte": THRESHOLD}}})),
        )
        .await;
        assert_eq!(
            matched.len(),
            over_threshold(slot).len(),
            "{view} must read its own rows of the shared source"
        );
        assert!(
            !matched.is_empty() && matched.len() < members(slot).count(),
            "{view}'s predicate is a proper subset, or this test proves nothing"
        );
    }
    let counts: BTreeSet<usize> = (0..QUARTERS.len())
        .map(|slot| over_threshold(slot).len())
        .collect();
    assert!(
        counts.len() > 1,
        "the discriminator must route genuinely different rows per view"
    );
}

/// **`/v1/categories` resolves per view, and a `derived` list is the view's own.** `sector`'s
/// values are derived from the per-view postings, and a quarter carries two of the three — so the
/// list under Q1 and the list under Q3 are different sets, and each is that view's.
#[tokio::test]
async fn the_value_list_is_the_views_own() {
    let served = serve().await;
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let listed: BTreeSet<String> = keys(&served, &format!("sector?view=quarter:{key}"))
            .await
            .into_iter()
            .collect();
        let expected: BTreeSet<String> = sectors_in(slot).into_iter().map(str::to_string).collect();
        assert_eq!(listed, expected, "quarter:{key}'s value list");
    }
    assert_ne!(sectors_in(0), sectors_in(2));
    // A pin is the other spelling of the same address, and answers the same list — here from the
    // plain view, which decides nothing of its own.
    let pinned: BTreeSet<String> = keys(&served, "sector@2026-Q3").await.into_iter().collect();
    assert_eq!(
        pinned,
        sectors_in(2).into_iter().map(str::to_string).collect()
    );
}

/// **A `derived` list narrows per principal *and* per view** — the C11 channel this change opens
/// over a per-view column (per-point-attributes §3.3).
///
/// The list is derived from that view's postings intersected with the principal's own mask, so
/// two principals under one view are offered two lists. The narrow principal holds term `1`
/// alone, which `terms_of` grants on `e % 3 == 0`; `rare` is carried by one entity per quarter and
/// that entity is not one of them, so the value is on the wide principal's list and off the narrow
/// one's — a difference at a named value, not merely a shorter list.
///
/// Both directions matter. Serving the authored set to the narrow principal is the disclosure;
/// serving it empty to a principal who does have members is the availability failure on the other
/// side, and only an exact expectation tells the two apart.
#[tokio::test]
async fn a_derived_value_list_narrows_per_principal_under_each_view() {
    let served = serve().await;
    let narrow = token(&served, &["1"]).await;

    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let path = format!("sector?view=quarter:{key}");
        let listed: BTreeSet<String> = keys_as(&served, &narrow, &path).await.into_iter().collect();
        let expected: BTreeSet<String> = sectors_visible_to(slot, &narrow_mask)
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(listed, expected, "quarter:{key}, narrow principal");
        assert!(
            !listed.is_empty(),
            "quarter:{key}: the narrow principal has members here"
        );
    }

    // The named value, in both directions and under one view: the wide principal is offered
    // `rare`, the narrow one is not, and the entity that separates them is the only one carrying
    // it.
    let wide: BTreeSet<String> = keys(&served, "sector?view=quarter:2026-Q1")
        .await
        .into_iter()
        .collect();
    let narrow_q1: BTreeSet<String> = keys_as(&served, &narrow, "sector?view=quarter:2026-Q1")
        .await
        .into_iter()
        .collect();
    assert!(wide.contains("rare"), "{wide:?}");
    assert!(!narrow_q1.contains("rare"), "{narrow_q1:?}");
    assert!(
        narrow_q1.is_subset(&wide) && narrow_q1 != wide,
        "the narrow list is a proper subset: {narrow_q1:?} vs {wide:?}"
    );
    assert!(!narrow_mask(rare_entity(0)), "the fixture's premise");
}

/// **A suppression retires a value and a term from the scoped routes, with no third rule.**
///
/// Suppression is entity space and the scoped columns are entity space, so a suppressed entity
/// leaves both surfaces by the same arithmetic that already governs the unscoped ones: its tokens
/// stop matching, and the `derived` value only it carried stops being offered. That second half is
/// §3.3's self-retirement — the reason a maintained union of visible values was rejected — and it
/// is the property most worth pinning here, because a per-view postings file is a second place a
/// membership set could have been cached.
#[tokio::test]
async fn a_suppression_reaches_both_scoped_routes() {
    let served = serve().await;
    let view = "quarter:2026-Q1";

    // The victim: the one entity carrying `rare` in Q1, which also carries Q1's prose. Named by
    // the served answer rather than by an entity id, which is the only name a client has (I10).
    let rare = ids(&served, view, Some(json!({"sector": {"eq": "rare"}}))).await;
    assert_eq!(rare.len(), 1, "`rare` is carried by exactly one entity");
    let victim = *rare.iter().next().unwrap();
    let matched = ids(&served, view, Some(json!({"note": {"match": "alpha"}}))).await;
    assert!(
        matched.contains(&victim),
        "the fixture's premise: the victim carries Q1's prose too"
    );
    assert!(
        keys(&served, "sector?view=quarter:2026-Q1")
            .await
            .contains(&"rare".to_string()),
        "and is what puts `rare` on the list"
    );

    let resp = post_changes(
        &served,
        &json!([{ "tessera_id": victim.to_string(), "idset": FIXTURE_IDSET, "op": "suppress" }]),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );

    // The text route: the suppressed entity's tokens answer nothing, and the rest of the match is
    // untouched — a suppression narrows, it does not blank.
    let after = ids(&served, view, Some(json!({"note": {"match": "alpha"}}))).await;
    assert!(
        !after.contains(&victim),
        "a suppressed entity still matched"
    );
    assert_eq!(
        after.len(),
        matched.len() - 1,
        "exactly the suppressed entity left the answer"
    );

    // The value list: `rare`'s last visible member is gone, so the value is gone with it, and the
    // values other entities carry stay.
    let listed: BTreeSet<String> = keys(&served, "sector?view=quarter:2026-Q1")
        .await
        .into_iter()
        .collect();
    assert!(!listed.contains("rare"), "{listed:?}");
    assert!(!listed.is_empty(), "{listed:?}");
    assert!(
        listed.is_subset(
            &sectors_in(0)
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        ),
        "{listed:?}"
    );
}

/// **A `public` value set is authored and is served whole under any view of the group** — the
/// vocabulary's `visibility` decides whether the list is derived, and the scope decides only which
/// column a derived one is derived from.
#[tokio::test]
async fn a_public_value_set_is_served_as_authored() {
    let served = serve().await;
    let listed = keys(&served, "mood?view=quarter:2026-Q1").await;
    assert_eq!(listed, MOODS.map(str::to_string).to_vec());
}

/// **A scoped column named with no view decides nothing, and says so naming the group** — the
/// `/v1/categories` half of the bare leaf's `422`. A pin naming no view of the group is the `404`
/// an unknown view already gets, from the same rule.
#[tokio::test]
async fn a_value_list_with_no_view_names_the_group() {
    let served = serve().await;
    let resp = categories(&served, "sector").await;
    assert_eq!(resp.status().as_u16(), 422);
    let body = resp.text().await.unwrap();
    assert!(body.contains("quarter"), "{body}");

    let resp = categories(&served, "sector@2027-Q9").await;
    assert_eq!(resp.status().as_u16(), 404);

    // A view of the group is a view of the group, whichever spelling reached it: an unknown view
    // on `?view=` is the same 404 a viewer verb gives.
    let resp = categories(&served, "sector?view=quarter:2027-Q9").await;
    assert_eq!(resp.status().as_u16(), 404);
}

/// **A text family is not a value list**, and asking for one is the same `404` a name that is
/// nothing at all gets — this route must not become a finer answer about which columns exist than
/// `/v1/meta`'s.
#[tokio::test]
async fn a_text_family_has_no_value_list() {
    let served = serve().await;
    let resp = categories(&served, "note?view=quarter:2026-Q1").await;
    assert_eq!(resp.status().as_u16(), 404);
}

/// **`/v1/meta` publishes each family twice over, and each list answers its own question**: the
/// operand entry says which operators a leaf may carry and which group decides its view, and the
/// `scoped_scalars` entry is the family's `declared_scalars` row — type, vocabulary, analyser and
/// the two placement flags — so a client can draw the control, fill it, and know which views the
/// column arrives under, without inferring anything.
#[tokio::test]
async fn meta_publishes_every_family_with_its_scope() {
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
    let operands: HashMap<String, Value> = body["filter_operands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (entry["column"].as_str().unwrap().to_string(), entry.clone()))
        .collect();
    for (column, family) in [
        ("mood", "category"),
        ("sector", "category"),
        ("note", "text"),
        ("score", "numeric"),
    ] {
        let entry = operands.get(column).unwrap_or_else(|| panic!("{column}"));
        assert_eq!(entry["family"], family);
        assert_eq!(entry["scope"]["group"], "quarter");
    }
    let families: HashMap<String, Value> = body["scoped_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (entry["name"].as_str().unwrap().to_string(), entry.clone()))
        .collect();
    assert_eq!(families["mood"]["category"]["vocabulary"], "mood");
    assert_eq!(families["mood"]["category"]["visibility"], "public");
    assert_eq!(families["sector"]["category"]["visibility"], "derived");
    assert!(families["note"]["analyser"].is_string());
    assert!(families["score"]["category"].is_null());
    for name in ["mood", "sector", "note", "score"] {
        assert_eq!(families[name]["scope"]["group"], "quarter");
        assert_eq!(families[name]["index"], true, "{name}");
    }
    // The placement flags say which of the two homes a family has: `score` is the one declaring
    // `render`, so it is the one a points batch under a quarter carries.
    assert_eq!(families["score"]["render"], true);
    for name in ["mood", "sector", "note"] {
        assert_eq!(families[name]["render"], false, "{name}");
    }
    // The views that have a column, in the owning group's ids — the whole roster here, every
    // family having been written at the build.
    let views: Vec<&str> = families["score"]["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        views,
        QUARTERS
            .iter()
            .map(|(key, _)| format!("quarter:{key}"))
            .collect::<Vec<_>>()
    );
}
