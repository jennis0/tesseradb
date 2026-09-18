//! **A layer's key column arriving at `POST /control/values` mints and joins** (`ingest.md` §1.4;
//! python-sdk §11.2 F).
//!
//! A table with an id column and a value column is insertable whatever the source's history, so a
//! corpus whose clustering arrives after its points reaches this door as it reaches the other two
//! ([decision 0091]). A key an artifact holds joins the entity to it; a key no artifact holds and
//! whose layer's value set is `open` creates the artifact it names, with the batch's rows as its
//! first members, through the one code path `/control/ingest`'s window close mints by
//! (decision 0139).
//!
//! What is pinned here is the door's behaviour and the equivalence. The cases where minting is
//! hard — a chain created parent before child, a suppressed key that must not mint again — are
//! `membership_column.rs`'s, over the ingest door and the same implementation.
//!
//! [decision 0091]: ../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, Float32Array, Float64Array, ListArray, StringArray, UInt32Array,
    UInt64Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::*;
use tessera_build::{build, BuildArgs};

/// The corpus. Small enough for a tick inside a test, wide enough that the eight keys
/// below each hold several points and the masked count moves between principals.
const N: u64 = 48;

/// How many points carry a key **in the file**. Everything above this names its artifact later, by
/// whichever door the case is about — so four of the eight keys exist after a build and four do
/// not, which is what makes the minting arm the one under test.
const BUILT: u64 = 4;

const LAYER: &str = "clusters/a";

fn x_of(e: u64) -> f64 {
    ((e * 37) % 1000) as f64
}
fn y_of(e: u64) -> f64 {
    ((e * 53) % 1000) as f64
}

/// The clustering: eight keys, each holding every eighth point.
fn key_of(e: u64) -> String {
    format!("k{}", e % 8)
}

/// The lineage the tiered case spells: a root, three groups under it, and the leaf below — one
/// entry per declared level, coarse to fine. **A leaf sits under one group**, the group being a
/// function of the leaf: a key named under two parents is two hierarchies and is refused at both
/// doors, which `membership_column.rs` pins.
fn chain_of(e: u64) -> Vec<String> {
    let leaf = e % 8;
    vec![
        "root".to_string(),
        format!("g{}", leaf / 3),
        format!("k{leaf}"),
    ]
}

/// What the fixture says each key's membership is, over the whole corpus.
fn expected_members() -> BTreeMap<String, u64> {
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for e in 0..N {
        *sizes.entry(key_of(e)).or_default() += 1;
    }
    sizes
}

// ---------------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------------

const VIEW_TOML: &str = r#"
[sources]
points = "points.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }
"#;

/// The layer, at the hierarchy kind the case is about. `value_set = "open"`, which is what says a
/// key no artifact holds creates one; the `closed` half is a control-plane layer below, a build
/// refusing a closed layer with members and no artifact source of its own.
fn layer_toml(kind: &str) -> String {
    format!(
        r#"
[[layer]]
name = "{LAYER}"
title = "clusters"
views = ["s0"]
membership = "enumerated"
value_set = "open"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "{kind}", prune_children = false }}
content = {{ computed = ["centroid", "box"] }}

  [layer.members]
  source = "points"
  fields = {{ key = "cluster", entity = "entity_id" }}
"#
    )
}

/// The points file. `keyed` is which of its rows carry a key at all: the rest hold a null, which
/// is the file's own spelling of *this point is in no artifact yet*.
fn write_points(path: &Path, rows: &[u64], keyed: &dyn Fn(u64) -> bool) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("cluster", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(rows.to_vec())) as ArrayRef,
            Arc::new(Float64Array::from(
                rows.iter().map(|e| x_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|e| y_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|e| keyed(*e).then(|| key_of(*e)))
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path, rows: &[u64]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in rows {
        for t in terms_of(*e) {
            entities.push(*e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

struct Built {
    _tmp: TempDir,
    root: PathBuf,
    dir: PathBuf,
}

fn build_side(rows: &[u64], keyed: &dyn Fn(u64) -> bool, layer: &str) -> Built {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let config_path = dir.join("config.toml");
    write_points(&points, rows, keyed);
    write_pairs(&pairs, rows);
    std::fs::write(&config_path, format!("{VIEW_TOML}{layer}")).unwrap();

    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture declaration parses");
    let root = dir.join("bundle");
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &config.schema),
        out: root.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    })
    .expect("the fixture build succeeds");
    Built {
        _tmp: tmp,
        root,
        dir,
    }
}

async fn serve(built: &Built) -> TestServer {
    spawn_server(
        &built.root,
        &built.dir.join("cache"),
        &built.dir.join("wal"),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

fn base64_external_id(e: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(e))
}

/// One `POST /control/values` batch in JSON: an id column and a column named for the layer.
fn values_body(rows: &[u64], column: &str, key_of: &dyn Fn(u64) -> Value) -> Value {
    Value::Array(
        rows.iter()
            .map(|e| json!({ "external_id": base64_external_id(*e), column: key_of(*e) }))
            .collect(),
    )
}

async fn post_values(server: &TestServer, batch_id: &str, body: Value) -> (u16, Value) {
    let resp = server
        .client
        .post(server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The same batch as an Arrow IPC stream, which is how a **list** column travels: a JSON body
/// carries one key per row and a lineage is a list per row.
fn values_arrow(rows: &[u64], column: &str, keys: ArrayRef) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new(column, keys.data_type().clone(), true),
    ]));
    let ext: Vec<Vec<u8>> = rows.iter().map(|e| external_id_of(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                ext.iter().map(|v| v.as_slice()),
            )) as ArrayRef,
            keys,
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn post_values_arrow(server: &TestServer, batch_id: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = server
        .client
        .post(server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The chain column: one entry per declared level, coarse to fine.
fn chain_column(rows: &[u64]) -> ArrayRef {
    let item = Arc::new(Field::new("item", DataType::Utf8, true));
    let mut offsets: Vec<i32> = vec![0];
    let mut entries: Vec<Option<String>> = Vec::new();
    for e in rows {
        entries.extend(chain_of(*e).into_iter().map(Some));
        offsets.push(entries.len() as i32);
    }
    Arc::new(ListArray::new(
        item,
        OffsetBuffer::new(offsets.into()),
        Arc::new(StringArray::from(entries)) as ArrayRef,
        None,
    ))
}

/// One `/control/ingest` batch, for the equivalence's middle arm: the reserved geometry columns,
/// the access list, and a column named for the layer.
fn ingest_batch(rows: &[u64], column: &str) -> Vec<u8> {
    let labels: Vec<Vec<String>> = rows
        .iter()
        .map(|e| terms_of(*e).iter().map(u64::to_string).collect())
        .collect();
    let labels: Vec<Vec<&str>> = labels
        .iter()
        .map(|row| row.iter().map(String::as_str).collect())
        .collect();
    let labels: Vec<&[&str]> = labels.iter().map(Vec::as_slice).collect();
    let access = access_lists(&labels);
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new(column, DataType::Utf8, true),
    ]));
    let ext: Vec<Vec<u8>> = rows.iter().map(|e| external_id_of(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                ext.iter().map(|v| v.as_slice()),
            )) as ArrayRef,
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|e| x_of(*e) as f32),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|e| y_of(*e) as f32),
            )),
            Arc::new(access),
            Arc::new(StringArray::from(
                rows.iter().map(|e| Some(key_of(*e))).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn post_ingest(server: &TestServer, batch_id: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

// ---------------------------------------------------------------------------------------------
// What a client sees
// ---------------------------------------------------------------------------------------------

/// One layer level's artifacts as `POST /v1/artifacts/browse` serves them to one principal:
/// key → masked count. The verb is the one a panel and a notebook read, and it is independent of
/// the viewport, which is what makes it the comparison surface here.
async fn browse_counts(
    server: &TestServer,
    terms: &[&str],
    layer: &str,
    level: Option<u32>,
) -> BTreeMap<String, u64> {
    browse_rows(server, terms, layer, level, None)
        .await
        .into_iter()
        .map(|row| (row.key, row.masked_count))
        .collect()
}

/// One browse row, with the fields this file compares.
struct BrowseRow {
    tessera_id: String,
    key: String,
    masked_count: u64,
    parents: usize,
}

/// One page of `POST /v1/artifacts/browse`: the **roots** form where `parent` is absent, and the
/// **children** form where it names an artifact.
///
/// The roots form of a levelled layer serves the artifacts of that level with no *served* parent,
/// so a tiered layer's finer levels are reached by walking down from the root rather than by
/// naming the level — which is the verb's own shape and not something this ruling changes.
async fn browse_rows(
    server: &TestServer,
    terms: &[&str],
    layer: &str,
    level: Option<u32>,
    parent: Option<&str>,
) -> Vec<BrowseRow> {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let mut request = json!({ "view": "s0", "layer": layer, "limit": 200 });
    if let Some(level) = level {
        request["level"] = json!(level);
    }
    if let Some(parent) = parent {
        request["parent"] = json!(parent);
    }
    let resp = server
        .client
        .post(server.viewer_url("/v1/artifacts/browse"))
        .bearer_auth(token)
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    body["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| {
            Some(BrowseRow {
                tessera_id: row["tessera_id"].as_str()?.to_string(),
                key: row["key"].as_str()?.to_string(),
                masked_count: row["masked_count"].as_u64().unwrap(),
                parents: row["parent_ids"].as_array().map_or(0, Vec::len),
            })
        })
        .collect()
}

/// Register a layer over the control plane, for the two cases a build cannot express: a `closed`
/// value set binding members with no artifact source of its own, and a layer whose artifacts carry
/// supplied content.
async fn register_layer(server: &TestServer, name: &str, value_set: &str, supplied: Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": name,
            "title": name,
            "views": ["s0"],
            "membership": "enumerated",
            "value_set": value_set,
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": [], "supplied": supplied },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(
        status,
        201,
        "the fixture layer registers: {}",
        resp.text().await.unwrap()
    );
}

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

/// **The headline.** A values page carrying held ids and a mix of held and new keys mints the keys
/// nothing holds, joins every row, and the counts a client browses are the fixture's own. A second
/// identical page under a fresh batch id creates nothing and joins nothing: membership grows
/// monotonically and a row already a member is a no-op.
#[tokio::test]
async fn a_values_page_mints_the_keys_nothing_holds_and_joins_every_row() {
    let built = build_side(
        &(0..N).collect::<Vec<_>>(),
        &|e| e < BUILT,
        &layer_toml("flat"),
    );
    let server = serve(&built).await;

    // Four of the eight keys came out of the file, so the page below is the mixed case: half its
    // rows join an artifact that exists and half name one that does not.
    let held = browse_counts(&server, &["0", "1"], LAYER, None).await;
    assert_eq!(
        held.len(),
        BUILT as usize,
        "the build's own column minted one artifact per key it carried: {held:?}"
    );

    let rows: Vec<u64> = (0..N).collect();
    let (status, body) = post_values(
        &server,
        "values-1",
        values_body(&rows, LAYER, &|e| json!(key_of(e))),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["minted"].as_u64(),
        Some(8 - BUILT),
        "the keys no artifact held were created: {body}"
    );
    assert_eq!(
        body["joined"].as_u64(),
        Some(N - BUILT),
        "every row that was not already a member joined: {body}"
    );

    tick(&server).await;
    let after = browse_counts(&server, &["0", "1"], LAYER, None).await;
    assert_eq!(
        after,
        expected_members(),
        "the memberships a client browses are the fixture's own"
    );

    // **A second identical page is a no-op.** A fresh batch id, so this is the fill rule and the
    // set join answering rather than the replay index.
    let (status, again) = post_values(
        &server,
        "values-2",
        values_body(&rows, LAYER, &|e| json!(key_of(e))),
    )
    .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["minted"].as_u64(), Some(0), "{again}");
    assert_eq!(again["joined"].as_u64(), Some(0), "{again}");
    tick(&server).await;
    assert_eq!(
        browse_counts(&server, &["0", "1"], LAYER, None).await,
        expected_members(),
        "a resent page moved nothing"
    );
}

/// **A `closed` layer's unknown key is the `422` it has always been.** The value set is the whole
/// of the difference: what `open` says is that a key names an artifact that may not exist yet, and
/// `closed` says the roster is the roster.
#[tokio::test]
async fn a_closed_layer_refuses_a_key_no_artifact_holds() {
    let built = build_side(
        &(0..N).collect::<Vec<_>>(),
        &|e| e < BUILT,
        &layer_toml("flat"),
    );
    let server = serve(&built).await;
    register_layer(&server, "roster/c", "closed", json!([])).await;

    let (status, body) = post_values(
        &server,
        "closed",
        values_body(&[0], "roster/c", &|_| json!("nobody-published-this")),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body.to_string();
    assert!(
        detail.contains("nobody-published-this"),
        "the refusal names the key: {detail}"
    );
}

/// **A layer with supplied content refuses the column**, as the ingest route does and for its
/// reason: an artifact served without content its layer declares cannot be told apart from one
/// whose content was withheld, so a key that could only mint a nameless artifact is refused where
/// the batch is still without effect.
#[tokio::test]
async fn a_layer_with_supplied_content_refuses_the_column() {
    let built = build_side(
        &(0..N).collect::<Vec<_>>(),
        &|e| e < BUILT,
        &layer_toml("flat"),
    );
    let server = serve(&built).await;
    register_layer(
        &server,
        "labels/x",
        "open",
        json!([{ "name": "label", "type": "text", "require_member_visibility": "inherited" }]),
    )
    .await;

    let (status, body) = post_values(
        &server,
        "unmintable",
        values_body(&[0], "labels/x", &|_| json!("nobody-declared-this")),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body.to_string();
    assert!(
        detail.contains("nobody-declared-this") && detail.contains("supplied content"),
        "the refusal names the key and why the layer cannot hold it: {detail}"
    );
}

/// **A tiered layer's list column mints the chain**, coarse level before fine, in one batch — the
/// half of the column that a growth could never do, since a growth adds members and never lineage.
#[tokio::test]
async fn a_tiered_list_column_mints_the_chain_it_declares() {
    let mut layer = layer_toml("tiered");
    layer.push_str(
        "\n[[layer.levels]]\nlevel = 0\n\n[[layer.levels]]\nlevel = 1\n\n[[layer.levels]]\nlevel = 2\n",
    );
    // Nothing in the file, so every artifact of every level is this batch's to create.
    let built = build_side(&(0..N).collect::<Vec<_>>(), &|_| false, &layer);
    let server = serve(&built).await;

    let rows: Vec<u64> = (0..N).collect();
    let (status, body) = post_values_arrow(
        &server,
        "chain",
        values_arrow(&rows, LAYER, chain_column(&rows)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["minted"].as_u64(),
        Some(1 + 3 + 8),
        "the root, its three groups and the eight leaves were created: {body}"
    );

    tick(&server).await;
    let roots = browse_rows(&server, &["0", "1"], LAYER, Some(0), None).await;
    assert_eq!(roots.len(), 1, "one root was created");
    assert_eq!(roots[0].key, "root");
    assert_eq!(
        roots[0].masked_count, N,
        "every point is in the root the chain named"
    );

    // **The chain, walked down from the root.** A leaf reached this way is a leaf whose edges were
    // created — the half of a list column a growth could never do, since a growth adds members and
    // never lineage.
    let groups = browse_rows(
        &server,
        &["0", "1"],
        LAYER,
        Some(1),
        Some(&roots[0].tessera_id),
    )
    .await;
    assert_eq!(groups.len(), 3, "the root's three groups");
    assert!(
        groups.iter().all(|g| g.parents == 1),
        "each group is served under the root that the chain named"
    );

    let mut leaves: BTreeMap<String, u64> = BTreeMap::new();
    for group in &groups {
        for leaf in browse_rows(
            &server,
            &["0", "1"],
            LAYER,
            Some(2),
            Some(&group.tessera_id),
        )
        .await
        {
            assert_eq!(leaf.parents, 1, "a leaf is served under its own group");
            leaves.insert(leaf.key, leaf.masked_count);
        }
    }
    assert_eq!(
        leaves,
        expected_members(),
        "the finest level is the clustering the column spelled"
    );
}

/// **The conformance case: one membership, three doors, one database.** The same clustering
/// arriving by build column, by ingest column and by values column gives the same counts to the
/// same principal — which is [decision 0091]'s claim, now over the third door.
///
/// **Per principal and not only for a full-coverage one.** A count is taken inside the viewer's
/// own mask (I2), so three databases could agree about the whole corpus and disagree about what
/// one principal may see. The two partial principals are where a membership recorded against the
/// wrong entity would show.
///
/// [decision 0091]: ../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md
#[tokio::test]
async fn the_same_membership_by_build_ingest_and_values_is_the_same_database() {
    let all: Vec<u64> = (0..N).collect();
    let seed: Vec<u64> = (0..BUILT).collect();
    let tail: Vec<u64> = (BUILT..N).collect();
    let layer = layer_toml("flat");

    // The build door: every point and every key in the file.
    let built_a = build_side(&all, &|_| true, &layer);
    let by_build = serve(&built_a).await;

    // The ingest door: the seed built, the rest arriving as points carrying their keys.
    let built_b = build_side(&seed, &|_| true, &layer);
    let by_ingest = serve(&built_b).await;
    let (status, body) = post_ingest(&by_ingest, "tail", ingest_batch(&tail, LAYER)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["minted"].as_u64(),
        Some(8 - BUILT),
        "the ingested tail created the keys the seed never held: {body}"
    );

    // The values door: every point built, and only the seed's rows keyed in the file — the rest of
    // the clustering arrives afterwards, over entities that already exist.
    let built_c = build_side(&all, &|e| e < BUILT, &layer);
    let by_values = serve(&built_c).await;
    let (status, body) = post_values(
        &by_values,
        "tail",
        values_body(&tail, LAYER, &|e| json!(key_of(e))),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["minted"].as_u64(),
        Some(8 - BUILT),
        "the values page created the same keys: {body}"
    );

    for server in [&by_build, &by_ingest, &by_values] {
        tick(server).await;
    }

    for terms in [&["0"][..], &["1"][..], &["0", "1"][..]] {
        let build = browse_counts(&by_build, terms, LAYER, None).await;
        let ingest = browse_counts(&by_ingest, terms, LAYER, None).await;
        let values = browse_counts(&by_values, terms, LAYER, None).await;
        assert_eq!(
            build, ingest,
            "the build and ingest doors disagree for {terms:?}"
        );
        assert_eq!(
            build, values,
            "the build and values doors disagree for {terms:?}"
        );
        assert!(
            !build.is_empty(),
            "the comparison for {terms:?} compared nothing"
        );
    }

    // And the whole of it is the fixture's own, so three databases that all dropped the column
    // would not pass by agreeing with each other.
    assert_eq!(
        browse_counts(&by_build, &["0", "1"], LAYER, None).await,
        expected_members()
    );
}
