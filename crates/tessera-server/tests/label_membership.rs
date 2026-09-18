//! **A label with no members of its own is the label of its cluster** (owner ruling H,
//! 2026-09-18; `python-sdk.md` §4.7): an attached artifact that declares no membership is placed
//! where its target is placed, counted over its target's members, gated by its target's gate, and
//! served to whoever is served the target.
//!
//! What is asserted, over the real routes: a label published with no `members` is drawn with its
//! text in the viewport's artifact frame and counted over the cluster's members, for a principal
//! who is served the cluster — and is absent, whole, for one who is not; the cluster's growth
//! grows the label's count at the next publication; a label published onto a warm level takes the
//! same membership; a label that declares its own members keeps them; for every principal the
//! label's masked count is the cluster's; and the same rule holds in a bundle the build wrote.
//!
//! **The counts are read from the drill-down**, not from the viewport row: a dependent's viewport
//! row carries its target's masked count whatever its own membership is (decision 0104's D13,
//! owner ruling 2026-08-25), so a number read there would be true before this rule as well.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

const CLUSTERS: &str = "clusters/labelled";
const TOPICS: &str = "topics/labelled";

/// The bar the cluster's own existence criterion sets: `["0"]` sees every member and clears it,
/// `["1"]` sees the multiples of three alone and does not.
const BAR: u64 = 20;

fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

/// How many of `range` carry term 1 — the narrow principal's masked count.
fn narrow_count(range: std::ops::Range<u64>) -> u64 {
    range.filter(|s| terms_of(*s).contains(&1)).count() as u64
}

async fn serve(tmp: &TempDir) -> TestServer {
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

async fn register(server: &TestServer, declaration: serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(status, 201, "{}", resp.text().await.unwrap());
}

/// The clustering: no supplied content, and an absolute criterion the narrow principal fails.
fn clusters() -> serde_json::Value {
    json!({
        "name": CLUSTERS,
        "title": CLUSTERS,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": { "count": BAR },
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": []
    })
}

/// The label set: one supplied text whose content requirement is `inherited`, attached to the
/// clustering, and no criterion of its own — everything that decides is the cluster's.
fn topics() -> serde_json::Value {
    json!({
        "name": TOPICS,
        "title": TOPICS,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "open",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": {
            "computed": [],
            "supplied": [{
                "name": "topic",
                "type": "text",
                "require_member_visibility": "inherited"
            }]
        },
        "depends_on": [CLUSTERS],
        "levels": []
    })
}

fn artifacts_url(server: &TestServer, layer: &str) -> String {
    server.control_url(&format!(
        "/control/layers/{}/artifacts",
        layer.replace('/', "%2F")
    ))
}

async fn put(
    server: &TestServer,
    layer: &str,
    artifacts: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .put(artifacts_url(server, layer))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    if status < 300 {
        // Durable at the acknowledgement, served from the next publication (`ingest.md` §1.3).
        tick(server).await;
    }
    (status, body)
}

async fn patch(
    server: &TestServer,
    layer: &str,
    artifacts: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .patch(artifacts_url(server, layer))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    if status < 300 {
        tick(server).await;
    }
    (status, body)
}

/// Every artifact of `layers` this principal is served, by `(layer, key)`.
async fn served(server: &TestServer, terms: &[&str]) -> Vec<ArtifactRow> {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0",
            "zoom": 0,
            "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "k": 200,
            "layers": [CLUSTERS, TOPICS],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
}

fn row<'a>(rows: &'a [ArtifactRow], layer: &str, key: &str) -> Option<&'a ArtifactRow> {
    rows.iter()
        .find(|row| row.layer == layer && row.key.as_deref() == Some(key))
}

/// Register both layers, publish one cluster over `range`, and attach one label carrying `text`
/// and no members of its own.
async fn fixture(server: &TestServer, range: std::ops::Range<u64>, text: &str) -> String {
    register(server, clusters()).await;
    register(server, topics()).await;
    let (status, body) = put(
        server,
        CLUSTERS,
        json!([{ "key": "c", "members": members(range) }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) = put(
        server,
        TOPICS,
        json!([{
            "key": "t",
            "members": [],
            "attached_to": { "layer": CLUSTERS, "key": "c" },
            "content": [{ "values": [text] }]
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    id_of(&body, 0)
}

/// The drill-down's status and body for one artifact, as `terms` sees it.
///
/// **The route D13 does not reach.** The viewport substitutes a dependent's target's masked count
/// on the wire (decision 0104's D13, owner ruling 2026-08-25), so a number read there says nothing
/// about which membership the server counted. This route answers one artifact from its own
/// verdict, so the number here is the membership the ruling is about.
async fn drill(server: &TestServer, terms: &[&str], tessera_id: &str) -> (u16, serde_json::Value) {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url(&format!("/v1/artifacts/{tessera_id}")))
        .bearer_auth(token)
        .json(&json!({ "view": "s0" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

fn id_of(body: &serde_json::Value, at: usize) -> String {
    body["artifacts"][at]["tessera_id"]
        .as_str()
        .expect("a publication answers an identifier")
        .to_string()
}

/// **The headline.** The label is placed where the cluster is placed and carries its text there,
/// its own masked count is the cluster's, and the principal the cluster is withheld from is served
/// neither — the label having nothing of its own to be served on.
#[tokio::test]
async fn a_label_with_no_members_is_served_over_its_clusters_membership() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let label = fixture(&server, 0..30, "shipping").await;

    let wide = served(&server, &["0"]).await;
    let cluster = row(&wide, CLUSTERS, "c").expect("the cluster is served to the wide principal");
    let served_label = row(&wide, TOPICS, "t").expect("and so is its label");
    assert_eq!(cluster.masked_count, 30, "the cluster's own masked count");
    assert_eq!(
        served_label.content,
        vec!["shipping".to_string()],
        "the label carries its text in the artifact frame"
    );

    // The membership itself, on the route that answers from the label's own verdict.
    let (status, body) = drill(&server, &["0"], &label).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["masked_count"], 30,
        "the label is counted over the cluster's members: {body}"
    );
    assert_eq!(body["content"], json!(["shipping"]), "{body}");

    // The narrow principal sees ten of the thirty, which does not clear the cluster's criterion.
    assert!(narrow_count(0..30) < BAR);
    let narrow = served(&server, &["1"]).await;
    assert!(
        row(&narrow, CLUSTERS, "c").is_none(),
        "the cluster is withheld by its own criterion: {narrow:?}"
    );
    assert!(
        row(&narrow, TOPICS, "t").is_none(),
        "and a label inherits nothing its cluster's gate would withhold: {narrow:?}"
    );
    assert_eq!(
        drill(&server, &["1"], &label).await.0,
        404,
        "on every route, and by the same absence"
    );

    server.shutdown().await;
}

/// **A conformance-style differential**: over every principal the fixture can produce, the label's
/// masked count is the cluster's, and the label is served exactly where the cluster is. The pair is
/// the ruling, so it is asserted as a pair.
#[tokio::test]
async fn the_labels_masked_count_is_the_clusters_for_every_principal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    // A bar of one, so the narrow principal clears it too and the differential compares both
    // states of *served* rather than one.
    register(&server, {
        let mut declaration = clusters();
        declaration["require_member_visibility"] = json!({ "count": 1 });
        declaration
    })
    .await;
    register(&server, topics()).await;
    let (status, body) = put(
        &server,
        CLUSTERS,
        json!([{ "key": "c", "members": members(0..30) }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let cluster = id_of(&body, 0);
    let (status, body) = put(
        &server,
        TOPICS,
        json!([{
            "key": "t",
            "members": [],
            "attached_to": { "layer": CLUSTERS, "key": "c" },
            "content": [{ "values": ["shipping"] }]
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let label = id_of(&body, 0);

    for terms in [vec!["0"], vec!["1"], vec!["0", "1"]] {
        let (cluster_status, cluster_body) = drill(&server, &terms, &cluster).await;
        let (label_status, label_body) = drill(&server, &terms, &label).await;
        assert_eq!(
            label_status, cluster_status,
            "{terms:?}: the label is served exactly where its cluster is"
        );
        assert_eq!(
            label_body["masked_count"], cluster_body["masked_count"],
            "{terms:?}: at the cluster's masked count"
        );
        let rows = served(&server, &terms).await;
        assert_eq!(
            row(&rows, TOPICS, "t").is_some(),
            row(&rows, CLUSTERS, "c").is_some(),
            "{terms:?}: and the viewport agrees with the drill-down"
        );
    }
    assert_eq!(
        drill(&server, &["1"], &label).await.1["masked_count"],
        narrow_count(0..30),
        "and the number is that principal's own, never the cluster's size"
    );

    server.shutdown().await;
}

/// **A cluster that grows grows its label**, at the next publication and with nothing published to
/// the label at all: the membership is borrowed at the version the target holds now, not copied at
/// the label's own publication.
#[tokio::test]
async fn a_cluster_that_grows_grows_its_labels_count() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let label = fixture(&server, 0..30, "shipping").await;
    assert_eq!(drill(&server, &["0"], &label).await.1["masked_count"], 30);

    let (status, body) = patch(
        &server,
        CLUSTERS,
        json!([{ "key": "c", "members": members(30..60) }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert_eq!(
        row(&served(&server, &["0"]).await, CLUSTERS, "c")
            .expect("served")
            .masked_count,
        60,
        "the cluster grew"
    );
    let (status, body) = drill(&server, &["0"], &label).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["masked_count"], 60,
        "and its label grew with it, nothing having been published to the label: {body}"
    );

    server.shutdown().await;
}

/// **A label that declares its own members keeps them.** They are the caller's claim about where
/// the text came from (decision 0135) and the ruling does not touch them: this label is counted
/// over its own six where the label beside it — attached to the same cluster, declaring none —
/// is counted over the cluster's thirty.
#[tokio::test]
async fn a_label_with_its_own_members_keeps_them() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, clusters()).await;
    register(&server, topics()).await;
    let (status, body) = put(
        &server,
        CLUSTERS,
        json!([{ "key": "c", "members": members(0..30) }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) = put(
        &server,
        TOPICS,
        json!([
            {
                "key": "own",
                "members": members(0..6),
                "attached_to": { "layer": CLUSTERS, "key": "c" },
                "content": [{ "values": ["from six documents"] }]
            },
            {
                "key": "borrowed",
                "members": [],
                "attached_to": { "layer": CLUSTERS, "key": "c" },
                "content": [{ "values": ["of the cluster"] }]
            }
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let own = id_of(&body, 0);
    let borrowed = id_of(&body, 1);

    assert_eq!(
        drill(&server, &["0"], &own).await.1["masked_count"],
        6,
        "a declared membership is the one counted"
    );
    assert_eq!(
        drill(&server, &["0"], &borrowed).await.1["masked_count"],
        30,
        "and the label beside it, declaring none, takes the cluster's"
    );
    let rows = served(&server, &["0"]).await;
    for key in ["own", "borrowed"] {
        // **Both carry the cluster's number on the wire**, which is D13 and not this ruling: a
        // dependent's viewport row takes its target's masked count whatever its own membership is
        // (decision 0104's D13). The two labels differ where the server counts, above.
        assert_eq!(
            row(&rows, TOPICS, key).expect("served").masked_count,
            30,
            "{key}"
        );
    }

    server.shutdown().await;
}

/// **A label published onto a level a request has already warmed** takes its cluster's membership
/// too. The held form is amended by what a level's own records changed by, and a borrowed
/// membership is in no record — so a form holding one is dropped at the publication rather than
/// amended, and the next request resolves it again.
#[tokio::test]
async fn a_label_published_after_the_form_is_warm_is_served_over_its_cluster() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    fixture(&server, 0..30, "shipping").await;
    // Warms the label level's form: one label, borrowing.
    assert!(row(&served(&server, &["0"]).await, TOPICS, "t").is_some());

    let (status, body) = put(
        &server,
        TOPICS,
        json!([{
            "key": "second",
            "members": [],
            "attached_to": { "layer": CLUSTERS, "key": "c" },
            "content": [{ "values": ["also of the cluster"] }]
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let second = id_of(&body, 0);

    let rows = served(&server, &["0"]).await;
    let served_second = row(&rows, TOPICS, "second").expect("the second label is served: {rows:?}");
    assert_eq!(
        served_second.content,
        vec!["also of the cluster".to_string()]
    );
    assert_eq!(
        drill(&server, &["0"], &second).await.1["masked_count"],
        30,
        "over the cluster's membership, as the first one is"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// The same rule in a bundle the build wrote (decision 0091: a build is an ingest into an empty
// database, so a label set published at the build is the one published through the control plane)
// ---------------------------------------------------------------------------------------------

const BUILT_CLUSTERS: &str = "clusters/built";
const BUILT_TOPICS: &str = "topics/built";

/// The declaration the bundle below is built from: a clustering whose membership is a **column on
/// the points** (`artifacts-from-points.md` §2) and a label set over it with **no member table**.
const BUILT_TOML: &str = r#"
[sources]
points = "points.parquet"
roster = "roster.parquet"
topics = "topics.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "clusters/built"
title = "clusters"
views = ["s0"]
source = "roster"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 20 }
hierarchy = { kind = "flat" }

  [layer.members]
  source = "points"
  fields = { key = "cluster_id", entity = "entity_id" }

  [layer.labels]
  name = "topics/built"
  title = "topics"
  source = "topics"
  type = "text"
  membership = "enumerated"
  require_member_visibility = "none"
  artifact_visibility = { default = "inherited" }

    [layer.labels.content]
    require_member_visibility = "inherited"
"#;

/// The points, with the cluster column the layer reads: the first thirty in `0`, the rest in `1`.
fn write_clustered_points(path: &std::path::Path, n: u64) {
    use arrow::array::{ArrayRef, Float64Array, StringArray, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("cluster_id", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let clusters: Vec<String> = ids
        .iter()
        .map(|e| if *e < 30 { "0" } else { "1" }.to_string())
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(UInt64Array::from(ids)) as ArrayRef,
            std::sync::Arc::new(Float64Array::from(xs)),
            std::sync::Arc::new(Float64Array::from(ys)),
            std::sync::Arc::new(StringArray::from(clusters)),
        ],
    )
    .unwrap();
    write_parquet(path, schema, batch);
}

fn write_parquet(
    path: &std::path::Path,
    schema: std::sync::Arc<arrow::datatypes::Schema>,
    batch: arrow::record_batch::RecordBatch,
) {
    let mut w =
        parquet::arrow::ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
            .unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One row per cluster — the roster the layer is named from.
fn write_roster(path: &std::path::Path) {
    use arrow::array::{ArrayRef, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![std::sync::Arc::new(StringArray::from(vec!["0", "1"])) as ArrayRef],
    )
    .unwrap();
    write_parquet(path, schema, batch);
}

/// One label per cluster: a text, an attachment, and no members.
fn write_labels(path: &std::path::Path) {
    use arrow::array::{ArrayRef, ListBuilder, StringArray, StringBuilder};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    let ranked = DataType::List(std::sync::Arc::new(Field::new(
        "item",
        DataType::List(std::sync::Arc::new(Field::new(
            "item",
            DataType::Utf8,
            true,
        ))),
        true,
    )));
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("contents", ranked, true),
        Field::new("attached_layer", DataType::Utf8, true),
        Field::new("attached_key", DataType::Utf8, true),
    ]));
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    for text in ["the first thirty", "everything else"] {
        contents.values().values().append_value(text);
        contents.values().append(true);
        contents.append(true);
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(StringArray::from(vec!["l-0", "l-1"])) as ArrayRef,
            std::sync::Arc::new(contents.finish()),
            std::sync::Arc::new(StringArray::from(vec![
                Some(BUILT_CLUSTERS),
                Some(BUILT_CLUSTERS),
            ])),
            std::sync::Arc::new(StringArray::from(vec![Some("0"), Some("1")])),
        ],
    )
    .unwrap();
    write_parquet(path, schema, batch);
}

/// Build the bundle the test above's rules are asserted over again, from a declaration rather
/// than from the control plane.
fn build_with_labels(dir: &std::path::Path) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let config = dir.join("config.toml");
    write_clustered_points(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    write_roster(&dir.join("roster.parquet"));
    write_labels(&dir.join("topics.parquet"));
    std::fs::write(&config, BUILT_TOML).unwrap();
    let parsed = tessera_build::config::Config::parse(&config, &Default::default())
        .expect("the declaration parses");
    let args = tessera_build::BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            // The pairs relation, so a principal's mask is the fixture's own and the two
            // principals below differ.
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: dir.join("bundle"),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: parsed.layers,
        layer_inputs: parsed.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: parsed.schema,
    };
    tessera_build::build(&args).expect("the bundle builds");
}

/// **The built bundle serves it too** (decision 0091). The label of the small cluster is drawn
/// with its text for the principal who is served that cluster and is absent for the one who is
/// not; the label of the large cluster is drawn for both. Nothing here was published through the
/// control plane.
#[tokio::test]
async fn a_built_label_set_with_no_members_is_served_over_its_clustering() {
    let tmp = TempDir::new().unwrap();
    build_with_labels(tmp.path());
    let server = spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let wide = built_rows(&server, &["0"]).await;
    for (cluster, label, text) in [
        ("0", "l-0", "the first thirty"),
        ("1", "l-1", "everything else"),
    ] {
        let cluster = row(&wide, BUILT_CLUSTERS, cluster).expect("the cluster is served");
        let served_label = row(&wide, BUILT_TOPICS, label).expect("and its label with it");
        assert_eq!(
            served_label.content,
            vec![text.to_string()],
            "the label carries its text"
        );
        assert_eq!(
            served_label.masked_count, cluster.masked_count,
            "beside the cluster's number"
        );
    }

    let narrow = built_rows(&server, &["1"]).await;
    assert!(
        row(&narrow, BUILT_CLUSTERS, "0").is_none() && row(&narrow, BUILT_TOPICS, "l-0").is_none(),
        "the small cluster is below its own bar for this principal, and its label goes with it: \
         {narrow:?}"
    );
    assert!(
        row(&narrow, BUILT_CLUSTERS, "1").is_some() && row(&narrow, BUILT_TOPICS, "l-1").is_some(),
        "the large one clears it, and so its label is drawn: {narrow:?}"
    );

    // **The build and the running service answer alike** (decision 0091, decision 0139): the same
    // label, over the same cluster, published through the control plane onto the clustering the
    // build wrote — and every principal is served the two identically.
    let (status, body) = put(
        &server,
        BUILT_TOPICS,
        json!([{
            "key": "l-0-live",
            "members": [],
            "attached_to": { "layer": BUILT_CLUSTERS, "key": "0" },
            "content": [{ "values": ["the first thirty"] }]
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let live = id_of(&body, 0);
    let built = row(&built_rows(&server, &["0"]).await, BUILT_TOPICS, "l-0")
        .expect("the built label")
        .tessera_id
        .to_string();
    for terms in [vec!["0"], vec!["1"], vec!["0", "1"]] {
        let (built_status, built_body) = drill(&server, &terms, &built).await;
        let (live_status, live_body) = drill(&server, &terms, &live).await;
        assert_eq!(
            built_status, live_status,
            "{terms:?}: the built label and the published one are served alike"
        );
        assert_eq!(
            built_body["masked_count"], live_body["masked_count"],
            "{terms:?}: and at one number"
        );
        assert_eq!(built_body["content"], live_body["content"], "{terms:?}");
        let rows = built_rows(&server, &terms).await;
        assert_eq!(
            row(&rows, BUILT_TOPICS, "l-0").is_some(),
            row(&rows, BUILT_TOPICS, "l-0-live").is_some(),
            "{terms:?}: and drawn together in the viewport"
        );
    }

    server.shutdown().await;
}

/// The two built layers' rows for one principal.
async fn built_rows(server: &TestServer, terms: &[&str]) -> Vec<ArtifactRow> {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0",
            "zoom": 0,
            "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "k": 200,
            "layers": [BUILT_CLUSTERS, BUILT_TOPICS],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
}
