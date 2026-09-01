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
use tessera_engine::EngineConfig;
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
    /// Held rather than dropped, because a restart reopens the same bundle, cache and log
    /// ([`restart`]).
    tmp: TempDir,
}

async fn serve() -> Served {
    serve_with(default_engine_config()).await
}

/// The fixture built once and served under a caller-chosen configuration — the write cycle below
/// narrows `coalesce_width` so the entity-space coalesce is reachable in a test.
async fn serve_with(config: EngineConfig) -> Served {
    let tmp = TempDir::new().unwrap();
    build_families(tmp.path());
    open(tmp, config).await
}

/// Open a server over an existing directory: the same bundle root, cache and WAL. Passing a
/// directory a previous [`Served`] has released is exactly the restart case.
async fn open(tmp: TempDir, config: EngineConfig) -> Served {
    let server = spawn_server_with_config(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;
    let auth = authorise(&server, &["0", "1"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    Served { server, token, tmp }
}

/// Reopen the same bundle and the same WAL. The old server is dropped first so its executor
/// releases the log.
async fn restart(served: Served, config: EngineConfig) -> Served {
    let Served { server, tmp, .. } = served;
    drop(server);
    open(tmp, config).await
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

// =================================================================================================
// The write cycle
// =================================================================================================
//
// Everything above is served from artefacts the **build** wrote. What follows drives the same four
// families through the write path end to end — ingest, flush, the entity-space coalesce, the
// compaction fold and a restart — and asserts the same answers at every stage
// (`views.md` §5, r24).
//
// The families are what make this worth its length: `mood` and `sector` are categories, so their
// per-view columns owe keyed postings; `note` is text, so its column is a token dictionary and
// positional postings and no value column at all; `score` is a rendered number read from its own
// source. Each takes a different branch of the flush's writer, of the empty base a view created at
// runtime acquires, and of the fold's per-view merge — and every one of those branches serves a
// **value**, so the failure they have in common is a wrong answer rather than an error.

/// New entities, allocated above the build's high-water, so nothing here collides with the
/// fixture's own and every assertion below is about rows the write path made.
const JOINED: u64 = 9_001;
const Q3_ONLY: u64 = 9_002;
const IN_MINTED: u64 = 9_003;

/// The key created while the service runs — a view the families have **no column for** until the
/// flush that covers it writes one.
const MINTED_KEY: &str = "2026-Q9";

/// What one ingested row carries for each of the four families under one view.
#[derive(Clone, Copy)]
struct Written {
    mood: &'static str,
    sector: &'static str,
    word: &'static str,
    score: f32,
}

/// `JOINED`'s values under `2026-Q1` and under `2026-Q3`: **one entity, two views, four families,
/// and every value different**. A column read from the wrong view is then a wrong answer rather
/// than a missing one, which is the only failure this whole surface has in common.
const IN_Q1: Written = Written {
    mood: "calm",
    sector: "north",
    word: "alpha",
    score: 0.9,
};
const IN_Q3: Written = Written {
    mood: "wild",
    sector: "east",
    word: "gamma",
    score: 0.1,
};
/// And under the minted view, a third set again.
const IN_Q9: Written = Written {
    mood: "tense",
    sector: "south",
    word: "delta",
    score: 0.8,
};
/// What the rows written only to make the coalesce eligible carry. **Deliberately inert**: `wild`
/// is not the value the built-rows check asks `2026-Q1` for, and `zeta` is in no quarter's prose,
/// so filling a column to its width moves no count this test reads.
const FILLER: Written = Written {
    mood: "wild",
    sector: "east",
    word: "zeta",
    score: 0.1,
};

/// An ingest body carrying the reserved columns and all four scoped families **under their plain
/// names** (`views.md` §5): the view comes from `x-tessera-view`, so the column is not qualified
/// and the view decides which of each family's columns the value lands in. A category arrives as
/// its **key**, never a code.
fn scoped_batch(rows: &[(u64, f64, f64, Written)]) -> Vec<u8> {
    use arrow::array::BinaryArray;
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("access", DataType::Utf8, false),
        Field::new("mood", DataType::Utf8, true),
        Field::new("sector", DataType::Utf8, true),
        Field::new("note", DataType::Utf8, true),
        Field::new("score", DataType::Float32, true),
    ]));
    let ids: Vec<Vec<u8>> = rows.iter().map(|(e, ..)| external_id_of(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter(
                ids.iter().map(|id| Some(id.as_slice())),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|(_, x, ..)| *x),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|(_, _, y, _)| *y),
            )),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|_| "0"))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|(.., w)| w.mood),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|(.., w)| w.sector),
            )),
            Arc::new(StringArray::from_iter_values(rows.iter().map(
                |(e, .., w)| format!("the {} report for entity {e}", w.word),
            ))),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|(.., w)| w.score),
            )),
        ],
    )
    .unwrap();
    let mut w = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    w.write(&batch).unwrap();
    w.into_inner().unwrap()
}

/// One batch into `view`, returning the `tessera_id` of each accepted row in the batch's own order
/// — an identifier the fold and the restart both preserve, so one handle serves every stage.
async fn ingest_scoped(
    served: &Served,
    batch_id: &str,
    view: &str,
    rows: &[(u64, f64, f64, Written)],
) -> Vec<u64> {
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/octet-stream")
        .body(scoped_batch(rows))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{view} accepts the batch: {body}");
    body["tessera_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect()
}

/// Flush until the buffer is empty — a flush unit is one view, so rows in two views need two
/// ticks.
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
            return;
        }
    }
}

/// Request a compaction fold and block until it has published (`POST /control/compact`).
/// Where one view's column of a group-scoped family lives under a partition directory.
///
/// **The key's directory carries its incarnation above the build's** (`views.md` §5,
/// [decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)): a view a
/// build declared is at `<key>/`, and one created while the service runs is at `<key>@<n>/`, so a
/// recreated key's base never lands on the path its predecessor's occupies. The number is
/// internal — no wire surface carries it — so a test matches the prefix rather than naming it.
fn scoped_dir(partition: &std::path::Path, family: &str, key: &str) -> std::path::PathBuf {
    let group = partition.join("attrs").join(family).join("quarter");
    let exact = group.join(key);
    if exact.is_dir() {
        return exact;
    }
    let suffixed = format!("{key}@");
    std::fs::read_dir(&group)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&suffixed))
        })
        .unwrap_or(exact)
}

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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
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
            "the fold never published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The newest side-manifest of the live prefix's only partition.
fn side_manifest(served: &Served) -> Value {
    let root = served.tmp.path().join("bundle");
    let current: Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).unwrap()).unwrap();
    let partition = root
        .join(current["prefix"].as_str().expect("CURRENT names a prefix"))
        .join("partitions/default");
    let newest = std::fs::read_dir(&partition)
        .expect("the partition directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("SEGMENTS-"))
        })
        .max_by_key(|p| {
            p.file_stem()
                .and_then(|n| n.to_str())
                .and_then(|n| n.trim_start_matches("SEGMENTS-").parse::<u64>().ok())
                .unwrap_or(0)
        })
        .expect("a side-manifest");
    serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap()
}

/// Every answer the write path owes, at one stage of it.
///
/// **Positive and negative on the same row, per family.** The row is in three views with a
/// different value in each, so each family is asked once for a value the view has and once for a
/// value another view has — and the second answer is what a column read from the wrong view would
/// get right by accident.
async fn assert_written_answers(served: &Served, stage: &str, joined: &[u64], minted: u64) {
    let (q1, q3) = (joined[0], joined[1]);
    for (view, own, other) in [
        ("quarter:2026-Q1", (q1, IN_Q1), (q3, IN_Q3)),
        ("quarter:2026-Q3", (q3, IN_Q3), (q1, IN_Q1)),
    ] {
        let (id, mine) = own;
        let theirs = other.1;
        for (column, operand, value) in [
            ("mood", "eq", mine.mood),
            ("sector", "eq", mine.sector),
            ("note", "match", mine.word),
        ] {
            let matched = ids(served, view, Some(json!({column: {operand: value}}))).await;
            assert!(
                matched.contains(&id),
                "{stage}: {view} must answer {column} {value} with the row that carries it"
            );
        }
        // The other view's value, on the same row, in this view: a wrong column would find it.
        for (column, operand, value) in [
            ("mood", "eq", theirs.mood),
            ("sector", "eq", theirs.sector),
            ("note", "match", theirs.word),
        ] {
            let matched = ids(served, view, Some(json!({column: {operand: value}}))).await;
            assert!(
                !matched.contains(&id),
                "{stage}: {view} answered {column} {value}, which is another view's value for \
                 that row"
            );
        }
        // The number, both directions of the threshold.
        let above = ids(
            served,
            view,
            Some(json!({"score": {"range": {"gte": THRESHOLD}}})),
        )
        .await;
        assert_eq!(
            above.contains(&id),
            f64::from(mine.score) >= THRESHOLD,
            "{stage}: {view}'s `score` column decides the range"
        );
    }

    // The **minted** view: a key created while the service ran, whose columns exist only because a
    // flush wrote them and their empty bases.
    let minted_view = format!("quarter:{MINTED_KEY}");
    for (column, operand, value) in [
        ("mood", "eq", IN_Q9.mood),
        ("sector", "eq", IN_Q9.sector),
        ("note", "match", IN_Q9.word),
    ] {
        let matched = ids(
            served,
            &minted_view,
            Some(json!({column: {operand: value}})),
        )
        .await;
        assert_eq!(
            matched,
            BTreeSet::from([minted]),
            "{stage}: {minted_view} answers {column} from the column its first flush wrote"
        );
    }
    let above = ids(
        served,
        &minted_view,
        Some(json!({"score": {"range": {"gte": THRESHOLD}}})),
    )
    .await;
    assert_eq!(
        above,
        BTreeSet::from([minted]),
        "{stage}: {minted_view}'s `score` column answers"
    );

    // A **pin** reaches each of those columns from a map that is not the group's, which is the one
    // route that reads a view's column without being under it.
    let world = ids(served, "world", None).await;
    let pinned = ids(
        served,
        "world",
        Some(json!({"mood@2026-Q1": {"eq": IN_Q1.mood}})),
    )
    .await;
    let bare = ids(
        served,
        "quarter:2026-Q1",
        Some(json!({"mood": {"eq": IN_Q1.mood}})),
    )
    .await;
    assert_eq!(
        pinned,
        world.intersection(&bare).copied().collect::<BTreeSet<_>>(),
        "{stage}: a pin is the named view's column met with the asking view's population"
    );

    // And the **build's** own rows still answer, unchanged by everything the write path did.
    for (slot, (key, _)) in QUARTERS.iter().enumerate() {
        let view = format!("quarter:{key}");
        let matched = ids(served, &view, Some(json!({"mood": {"eq": "calm"}}))).await;
        let expected = with_mood(slot, "calm").len() + usize::from(matched.contains(&q1));
        assert_eq!(
            matched.len(),
            expected,
            "{stage}: {view}'s built rows are untouched"
        );
        let prose = ids(served, &view, Some(json!({"note": {"match": WORDS[slot]}}))).await;
        assert!(
            prose.len() >= with_word(slot, WORDS[slot]).len(),
            "{stage}: {view}'s built prose is still indexed"
        );
    }
}

/// **Every filterable family survives the whole write cycle, and answers per view at each stage**
/// (`views.md` §5, r24).
///
/// One test rather than four, because the stages are not independent: what a flush wrote is what
/// the next flush layers over, what those layers hold is what the fold reads, and what the fold
/// wrote is what the restart opens. A stage asserted against a fixture the previous one did not
/// produce would pass while the chain was broken.
///
/// The failure this drives out is **silent**: every one of these artefacts is read as *this entity
/// has no value* when it is absent, so a family the flush skipped, a base a minted view never got,
/// a layer composed under the wrong name, or a fold that wrote a prefix without the per-view
/// directories all answer a filter with a smaller set and no error anywhere.
#[tokio::test]
async fn every_scoped_family_survives_ingest_flush_layering_fold_and_restart() {
    // Width two, so the coalesce becomes eligible after a column holds two extents rather than
    // eight — the pass is the subject here, not its policy.
    let config = || EngineConfig {
        coalesce_width: Some(2),
        ..default_engine_config()
    };
    let mut served = serve_with(config()).await;

    // ---- ingest: one entity into two views, with different values in each --------------------
    let q1 = ingest_scoped(
        &served,
        "cycle-q1",
        "quarter:2026-Q1",
        &[(JOINED, 250.0, 250.0, IN_Q1)],
    )
    .await[0];
    // The **join**: the entity already exists, and this row carries the second view's own scoped
    // values — the one thing a joining row brings beyond geometry (`views.md` §4, §5).
    let q3 = ingest_scoped(
        &served,
        "cycle-q3",
        "quarter:2026-Q3",
        &[
            (JOINED, 260.0, 260.0, IN_Q3),
            (Q3_ONLY, 270.0, 270.0, IN_Q3),
        ],
    )
    .await;
    assert_eq!(
        q1, q3[0],
        "a join lands on the entity it names rather than allocating a second"
    );
    flush(&served).await;

    // ---- a view created while the service runs, and its first flush --------------------------
    let created = served
        .server
        .client
        .put(
            served
                .server
                .control_url(&format!("/control/views/quarter/{MINTED_KEY}")),
        )
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status().as_u16(), 201, "a free key creates");
    let minted_view = format!("quarter:{MINTED_KEY}");
    let minted = ingest_scoped(
        &served,
        "cycle-minted",
        &minted_view,
        &[(IN_MINTED, 280.0, 280.0, IN_Q9)],
    )
    .await[0];
    flush(&served).await;

    // The visible-view set is fixed per session (`views.md` §6), so reading the new view needs a
    // new one — exactly as a client would.
    served.token = token(&served, &["0", "1"]).await;
    assert_written_answers(&served, "after the first flush", &[q1, q3[0]], minted).await;

    // `/v1/meta` says the minted view now has a column of every family, which is what a client
    // reads to know the column it is being served exists.
    let meta: Value = served
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
    for family in meta["scoped_scalars"].as_array().unwrap() {
        let views = family["views"].as_array().unwrap();
        assert!(
            views.contains(&json!(minted_view)),
            "the flush put the minted view on {}'s list: {views:?}",
            family["name"]
        );
    }

    // ---- a stack of layers on one view's column -----------------------------------------------
    //
    // Each flush of a view writes one extent per filterable family of its group, so three more
    // flushes of `2026-Q1` leave that view's columns four layers deep while every other view's
    // stays at one. Two things are at stake and both are silent: a read that stopped at the base
    // answers a smaller set, and a layer composed under the **column's** name rather than
    // `(column, view)`'s would put these entities into another quarter's answer.
    //
    // ⊘ The entity-space **coalesce** over such a stack is not driven from here. It has no trigger
    // route — it is planned on the executor's own clock and discarded when a flush publishes under
    // it — so a server test can only wait on it, and the wait is a race rather than an assertion.
    // Its keying is covered where it is deterministic instead:
    // `coalesce::tests::a_scoped_familys_window_is_one_views_own` plans over a manifest carrying
    // two views' extents of one family and asserts one window per view, each holding only its own
    // view's layers and each under its own view's directory.
    for round in 0..3 {
        ingest_scoped(
            &served,
            &format!("cycle-fill-{round}"),
            "quarter:2026-Q1",
            &[(9_100 + round, 300.0 + round as f64, 300.0, FILLER)],
        )
        .await;
        flush(&served).await;
    }
    let manifest = side_manifest(&served);
    let layers = |column: &str, view: &str| {
        manifest["attr_extents"]
            .as_array()
            .expect("attr_extents")
            .iter()
            .filter(|e| e["column"] == column && e["view"] == view)
            .count()
    };
    assert!(
        layers("mood", "quarter:2026-Q1") >= 3,
        "the fillers stacked layers on `2026-Q1`'s column: {}",
        manifest["attr_extents"]
    );
    assert_eq!(
        layers("mood", &format!("quarter:{MINTED_KEY}")),
        1,
        "and on no other view's — a layer belongs to the `(column, view)` that wrote it"
    );
    assert_written_answers(&served, "over a stack of layers", &[q1, q3[0]], minted).await;

    // ---- the fold -----------------------------------------------------------------------------
    fold(&served).await;
    let root = served.tmp.path().join("bundle");
    let current: Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).unwrap()).unwrap();
    let partition = root
        .join(current["prefix"].as_str().expect("CURRENT names a prefix"))
        .join("partitions/default");
    // The folded prefix carries a directory per `(family, view)`, the minted view included — a
    // fold that wrote the bundle-wide columns alone would leave a prefix these are simply not in,
    // and the families would be served as absent from the first read.
    for family in ["mood", "sector", "score"] {
        for key in ["2026-Q1", "2026-Q3", MINTED_KEY] {
            let dir = scoped_dir(&partition, family, key);
            assert!(
                dir.join("values.arrow").is_file(),
                "the fold wrote {family}'s column for {key}: {}",
                dir.display()
            );
        }
    }
    for key in ["2026-Q1", "2026-Q3", MINTED_KEY] {
        // Text owes no value column, per view exactly as bundle-wide: a dictionary and postings.
        let dir = scoped_dir(&partition, "note", key);
        assert!(
            dir.join("postings.arrow").is_file(),
            "the fold wrote `note`'s index for {key}: {}",
            dir.display()
        );
    }
    served.token = token(&served, &["0", "1"]).await;
    assert_written_answers(&served, "after a fold", &[q1, q3[0]], minted).await;

    // ---- and a restart, which opens exactly what the fold wrote ------------------------------
    let served = restart(served, config()).await;
    assert_written_answers(&served, "after a restart", &[q1, q3[0]], minted).await;
}
