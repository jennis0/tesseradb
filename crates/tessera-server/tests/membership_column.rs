//! **A point names its artifacts on the wire** (`artifacts-from-points.md` §6.2), and the database
//! it produces is the one a build produces from the same corpus.
//!
//! [Decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md) rules
//! that there is no difference in functionality or client experience between a build and an ingest,
//! and marked exactly one breach of its own rule: a point could name its artifacts in a file and not
//! on the wire. This file is that breach closed, and the test it names is the headline here — the
//! same corpus, split between the two entry points, is the same database *to every client*: same
//! memberships, same masked counts, same computed content, same gates. Not the same bytes, because
//! ids are assigned differently at the two entry points and nothing a client holds exposes that.
//!
//! **The corpus is split rather than wholly ingested, and that is the build's own rule speaking**:
//! `tessera build` refuses a bundle with no items ("no points selected — a bundle with no items has
//! no expressible entity range"), so *ingest into an empty database* is not a state this system can
//! be put in through its own tools. The honest form of 0091's test is therefore one corpus arriving
//! two ways: every point built on one side, a seed built and the rest ingested on the other. It
//! asserts what the rule is about — that the route a point took leaves no trace a client can see —
//! and it exercises the mixed state a real deployment is always in.
//!
//! **The list column is here because the lineage inference is the half most likely to drift.** A
//! scalar key is one lookup; a list's positions carry levels and its adjacency carries edges, and
//! those rules live in `tessera_types::layer` precisely so a build and a wire batch cannot read one
//! file two ways.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, Float32Array, Float64Array, Int64Array, ListArray, StringArray,
    UInt32Array, UInt64Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use serde_json::json;
use tempfile::TempDir;

use common::*;
use tessera_build::{build, BuildArgs};

/// The corpus. Small enough for a flush and a fold inside a test, wide enough that a clustering
/// over it has clusters of different sizes and a masked count that moves between principals.
const N: u64 = 240;

/// How many of those points the *ingest* side builds. Every key the clustering uses appears within
/// this prefix, because minting from an unknown key at ingest is the next stage: the artifacts must
/// exist before the points that name them arrive.
const SEED: u64 = 30;

const LAYER: &str = "clusters/a";

fn x_of(e: u64) -> f64 {
    ((e * 37) % 1000) as f64
}
fn y_of(e: u64) -> f64 {
    ((e * 53) % 1000) as f64
}

/// The scalar clustering: eight clusters, every tenth point noise (`-1`) and every seventeenth
/// unclustered (null) — the two spellings of *this point is in no artifact*.
fn cluster_of(e: u64) -> Option<i64> {
    if e.is_multiple_of(17) {
        None
    } else if e.is_multiple_of(10) {
        Some(-1)
    } else {
        Some((e % 8) as i64)
    }
}

/// The lineage: one root, three groups under it, six leaves under those — each point a member of
/// every artifact its list names, and the adjacency the edges between them.
fn lineage_of(e: u64) -> Vec<Option<i64>> {
    if e.is_multiple_of(17) {
        // Noise at the finest resolution: a point clustered at the two coarser levels and at none
        // of the leaves. The edge across the gap is the one that must not be invented.
        return vec![Some(1), Some(10 + (e % 3) as i64), None];
    }
    vec![
        Some(1),
        Some(10 + (e % 3) as i64),
        Some(100 + (e % 6) as i64),
    ]
}

/// The artifacts a point belongs to under each spelling — the fixture's own answer, which the
/// oracle counts and the two inputs are written from.
fn scalar_key_of(e: u64) -> Vec<i64> {
    cluster_of(e).filter(|k| *k >= 0).into_iter().collect()
}

fn lineage_keys_of(e: u64) -> Vec<i64> {
    lineage_of(e).into_iter().flatten().collect()
}

// ---------------------------------------------------------------------------------------------
// The two sides: a build, and a build plus an ingest
// ---------------------------------------------------------------------------------------------

const VIEW_TOML: &str = r#"
[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }
"#;

/// The layer both sides declare, at the hierarchy kind the case is about. `value_set = "open"` so
/// the build mints an artifact per key it meets — which is what makes the point column a roster on
/// the build side, and what the ingest side's seed then hands to the wire.
fn layer_toml(kind: &str, key_column: &str) -> String {
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
require_member_visibility = {{ count = 4 }}
hierarchy = {{ kind = "{kind}", prune_children = false }}
content = {{ computed = ["centroid", "box"] }}

  [layer.members]
  source = "points.parquet"
  fields = {{ key = "{key_column}", entity = "entity_id" }}
"#
    )
}

/// The points file: geometry, and the two membership spellings beside it.
///
/// `rows` is which of the corpus's points this file carries — the whole of it on the built side, its
/// seed on the ingested one.
fn write_points(path: &Path, rows: &[u64]) {
    let item = Arc::new(Field::new("item", DataType::Int64, true));
    let mut offsets: Vec<i32> = vec![0];
    let mut entries: Vec<Option<i64>> = Vec::new();
    for e in rows {
        entries.extend(lineage_of(*e));
        offsets.push(entries.len() as i32);
    }
    let lineage: ArrayRef = Arc::new(ListArray::new(
        item,
        OffsetBuffer::new(offsets.into()),
        Arc::new(Int64Array::from(entries)) as ArrayRef,
        None,
    ));
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("cluster", DataType::Int64, true),
        Field::new("lineage", lineage.data_type().clone(), true),
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
            Arc::new(Int64Array::from(
                rows.iter().map(|e| cluster_of(*e)).collect::<Vec<_>>(),
            )),
            lineage,
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The access relation, in the same shape [`common::terms_of`] gives the shared fixture — so an
/// ingested row's `access` string and a built row's pairs rows describe one labelling.
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

/// An external id as `/control/layers/{name}/artifacts` addresses one: base64 of the bytes the
/// build minted, which is the same convention `/control/changes` uses.
fn base64_external_id(e: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(e))
}

/// The access label an ingested row carries — the same terms the pairs file gives a built one.
fn access_of(e: u64) -> String {
    terms_of(e)
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

struct Built {
    _tmp: TempDir,
    root: PathBuf,
    dir: PathBuf,
}

/// Build a bundle over `rows`, with the layer reading its members from the point table.
fn build_side(rows: &[u64], layer: &str) -> Built {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let config_path = dir.join("config.toml");
    write_points(&points, rows);
    write_pairs(&pairs, rows);
    std::fs::write(&config_path, format!("{VIEW_TOML}{layer}")).unwrap();

    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture declaration parses");
    let root = dir.join("bundle");
    let args = BuildArgs {
        point_fields: Default::default(),
        corpus_fields: Default::default(),
        points: points.clone(),
        corpus: Some(points),
        access: tessera_build::config::AccessInput::relation(pairs),
        out: root.clone(),
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    build(&args).expect("the fixture build succeeds");
    Built {
        _tmp: tmp,
        root,
        dir,
    }
}

async fn serve(built: &Built) -> TestServer {
    spawn_server(&built.root, &built.dir.join("cache"), &built.dir.join("wal")).await
}

// ---------------------------------------------------------------------------------------------
// The wire: an ingest batch carrying a column named for a layer
// ---------------------------------------------------------------------------------------------

/// One ingest batch: the reserved geometry columns, and a column **named for the layer** carrying
/// each point's artifacts.
fn ingest_batch(rows: &[u64], column: &str, keys: ArrayRef) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
        Field::new(column, keys.data_type().clone(), true),
    ]));
    let ext: Vec<Vec<u8>> = rows.iter().map(|e| external_id_of(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                ext.iter().map(|v| v.as_slice()),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|e| x_of(*e) as f32),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|e| y_of(*e) as f32),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|e| access_of(*e)),
            )),
            keys,
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// The scalar column: one cluster key per point.
fn scalar_keys(rows: &[u64]) -> ArrayRef {
    Arc::new(Int64Array::from(
        rows.iter().map(|e| cluster_of(*e)).collect::<Vec<_>>(),
    ))
}

/// The list column: the artifacts a point belongs to, coarse to fine.
fn lineage_keys(rows: &[u64]) -> ArrayRef {
    let item = Arc::new(Field::new("item", DataType::Int64, true));
    let mut offsets: Vec<i32> = vec![0];
    let mut entries: Vec<Option<i64>> = Vec::new();
    for e in rows {
        entries.extend(lineage_of(*e));
        offsets.push(entries.len() as i32);
    }
    Arc::new(ListArray::new(
        item,
        OffsetBuffer::new(offsets.into()),
        Arc::new(Int64Array::from(entries)) as ArrayRef,
        None,
    ))
}

async fn post_ingest(server: &TestServer, batch_id: &str, body: Vec<u8>) -> (u16, String) {
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// Ingest the corpus's tail in batches, then flush and fold so the rows are base rows.
///
/// **The fold is not tidiness.** An artifact's membership is projected through *base* rows
/// (`annotation-write-cycle.md` §4.1), so a point ingested since the last fold contributes to no
/// masked count however its membership was recorded — fail-closed, and the reason the comparison
/// below runs after one.
async fn ingest_tail(server: &TestServer, column: &str, keys: fn(&[u64]) -> ArrayRef) {
    let tail: Vec<u64> = (SEED..N).collect();
    for (i, chunk) in tail.chunks(70).enumerate() {
        let body = ingest_batch(chunk, column, keys(chunk));
        let (status, detail) = post_ingest(server, &format!("tail-{i}"), body).await;
        assert_eq!(status, 200, "{detail}");
    }
    flush_and_fold(server).await;
}

async fn flush_and_fold(server: &TestServer) {
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the flush published", move |now| {
        now.flushes > before.flushes
    })
    .await;

    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the fold published", move |now| {
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        now.folds > before.folds
    })
    .await;
}

async fn wait_until(
    server: &TestServer,
    what: &str,
    done: impl Fn(&tessera_engine::ExecutorStats) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if done(&server.state.engine.write_executor_stats()) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what}: never happened");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// What a client sees
// ---------------------------------------------------------------------------------------------

/// One artifact as a client reads it out of the artifacts frame, **with the identifiers replaced by
/// the caller's own keys**.
///
/// A `tessera_id` is a blinding permutation of an entity id, and 0091 says in as many words that the
/// two entry points assign entities differently — so an identifier is exactly the field that may
/// differ. The key is what a publisher named the artifact, and the parent travels as *its* key,
/// resolved within the response, which is the same edge without the identity.
#[derive(Debug, Clone, PartialEq)]
struct ClientArtifact {
    layer: String,
    key: Option<String>,
    masked_count: u64,
    centroid: Option<[f64; 2]>,
    bbox: Option<[u32; 4]>,
    parent_key: Option<String>,
}

/// The whole map, as one principal sees it: the tile counts, and every artifact served.
struct ClientView {
    tiles: Vec<TileRow>,
    points: usize,
    artifacts: Vec<ClientArtifact>,
}

async fn client_view(server: &TestServer, terms: &[&str]) -> ClientView {
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
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = resp.bytes().await.unwrap();
    let decoded = decode_viewport_frames(&bytes);
    let artifacts = artifacts_by_key(&bytes);
    ClientView {
        tiles: decoded.tiles,
        points: decoded.points.len(),
        artifacts,
    }
}

/// Decode the artifacts frame in full — the shared decoder drops `parent_id`, and the edge is half
/// of what a lineage column is for.
fn artifacts_by_key(body: &[u8]) -> Vec<ClientArtifact> {
    use arrow::array::Float64Array as F64;
    let frames = tessera_wire::split_frames(body).expect("well-formed frames");
    let Some((_, payload)) = frames
        .iter()
        .find(|(kind, _)| *kind == tessera_wire::FRAME_ARTIFACTS)
    else {
        return Vec::new();
    };
    let reader =
        arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(payload.to_vec()), None)
            .unwrap();
    let mut rows: Vec<(u64, ClientArtifact)> = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let column = |i: usize| batch.column(i).clone();
        let layer = column(0);
        let layer = layer.as_any().downcast_ref::<StringArray>().unwrap();
        let ids = column(1);
        let ids = ids.as_any().downcast_ref::<UInt64Array>().unwrap();
        let keys = column(2);
        let keys = keys.as_any().downcast_ref::<StringArray>().unwrap();
        let counts = column(3);
        let counts = counts.as_any().downcast_ref::<UInt64Array>().unwrap();
        let parents = column(13);
        let parents = parents.as_any().downcast_ref::<UInt64Array>().unwrap();
        let f64_at = |col: usize, i: usize| {
            let a = batch.column(col).clone();
            let a = a.as_any().downcast_ref::<F64>().unwrap().clone();
            a.is_valid(i).then(|| a.value(i))
        };
        let u32_at = |col: usize, i: usize| {
            let a = batch.column(col).clone();
            let a = a.as_any().downcast_ref::<UInt32Array>().unwrap().clone();
            a.is_valid(i).then(|| a.value(i))
        };
        for i in 0..batch.num_rows() {
            let centroid = match (f64_at(4, i), f64_at(5, i)) {
                (Some(x), Some(y)) => Some([x, y]),
                _ => None,
            };
            let bbox = match (u32_at(6, i), u32_at(7, i), u32_at(8, i), u32_at(9, i)) {
                (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
                _ => None,
            };
            rows.push((
                ids.value(i),
                ClientArtifact {
                    layer: layer.value(i).to_string(),
                    key: keys.is_valid(i).then(|| keys.value(i).to_string()),
                    masked_count: counts.value(i),
                    centroid,
                    bbox,
                    parent_key: parents.is_valid(i).then(|| parents.value(i).to_string()),
                },
            ));
        }
    }
    // The parent travels as an identifier, and an identifier is the one field that legitimately
    // differs between the two databases. Resolve it against this same response, which is the only
    // place a client could resolve it either.
    let key_of: std::collections::BTreeMap<String, Option<String>> = rows
        .iter()
        .map(|(id, a)| (id.to_string(), a.key.clone()))
        .collect();
    let mut artifacts: Vec<ClientArtifact> = rows
        .into_iter()
        .map(|(_, mut a)| {
            a.parent_key = a
                .parent_key
                .map(|id| key_of.get(&id).cloned().flatten().unwrap_or(id));
            a
        })
        .collect();
    artifacts.sort_by(|a, b| (&a.layer, &a.key).cmp(&(&b.layer, &b.key)));
    artifacts
}

/// What the fixture *says* the memberships are, for a principal who can see every point — computed
/// from the same functions both inputs are written from.
///
/// **The equality between the two sides is not enough on its own.** Two databases that both dropped
/// the membership column would agree perfectly, so one of them is pinned to the fixture here and the
/// comparison carries the other.
fn expected_memberships(keys_of: fn(u64) -> Vec<i64>) -> std::collections::BTreeMap<String, u64> {
    let mut sizes: std::collections::BTreeMap<String, u64> = Default::default();
    for e in 0..N {
        let mut keys = keys_of(e);
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            *sizes.entry(key.to_string()).or_default() += 1;
        }
    }
    // The layer's criterion: an artifact serving four or more visible members. Applied here so the
    // oracle is the whole of what a client should see rather than a subset of it.
    sizes.retain(|_, size| *size >= 4);
    sizes
}

/// The comparison 0091 names: two databases, one client at a time.
async fn assert_same_database(
    built: &TestServer,
    ingested: &TestServer,
    keys_of: fn(u64) -> Vec<i64>,
    what: &str,
) {
    // The oracle first: what the built side serves a full-coverage principal is what the fixture
    // says it should be, key by key.
    let full = client_view(built, &["0", "1"]).await;
    let served: std::collections::BTreeMap<String, u64> = full
        .artifacts
        .iter()
        .map(|a| (a.key.clone().expect("a built artifact carries its key"), a.masked_count))
        .collect();
    assert_eq!(
        served,
        expected_memberships(keys_of),
        "{what}: the built side does not serve the memberships the fixture declares"
    );

    for terms in [vec!["0"], vec!["1"], vec!["0", "1"]] {
        let a = client_view(built, &terms).await;
        let b = client_view(ingested, &terms).await;
        assert_eq!(
            a.tiles, b.tiles,
            "{what}: the tile counts differ for a principal holding {terms:?} — the same points at \
             the same coordinates under the same labels"
        );
        assert_eq!(a.points, b.points, "{what}: the points served differ");
        assert_eq!(
            a.artifacts, b.artifacts,
            "{what}: the artifacts differ for a principal holding {terms:?}"
        );
        assert!(
            !a.artifacts.is_empty(),
            "{what}: neither side served an artifact, so the comparison proved nothing"
        );
    }
    // Not a vacuous comparison: the layer's criterion and the mask must actually bite, or two
    // identical *empty* answers would pass every assertion above.
    let broad = client_view(built, &["0", "1"]).await;
    let narrow = client_view(built, &["1"]).await;
    assert_eq!(
        broad.tiles.iter().map(|(_, visible, _)| visible).sum::<u64>(),
        N,
        "{what}: the principal who can see everything is not being shown the whole corpus, so the \
         two sides could agree on a corpus neither of them holds"
    );
    assert!(
        narrow.artifacts.len() < broad.artifacts.len()
            || narrow.artifacts.iter().map(|a| a.masked_count).sum::<u64>()
                < broad.artifacts.iter().map(|a| a.masked_count).sum::<u64>(),
        "{what}: masking makes no difference to this fixture, so it cannot show that the two \
         entry points mask alike"
    );
}

// ---------------------------------------------------------------------------------------------
// 0091's own test
// ---------------------------------------------------------------------------------------------

/// **The headline, at a scalar key.** One corpus, one clustering, two entry points.
#[tokio::test]
async fn a_scalar_membership_column_ingests_the_database_a_member_table_builds() {
    let layer = layer_toml("flat", "cluster");
    let all: Vec<u64> = (0..N).collect();
    let built = build_side(&all, &layer);
    let seed: Vec<u64> = (0..SEED).collect();
    let ingested = build_side(&seed, &layer);

    let built = serve(&built).await;
    let ingested = serve(&ingested).await;
    ingest_tail(&ingested, LAYER, scalar_keys).await;

    assert_same_database(&built, &ingested, scalar_key_of, "a cluster column").await;
}

/// **The same, at a list key — and the lineage is the half that would drift.** Every entry is a
/// membership and every consecutive pair an edge, so a client's view carries both: the counts at
/// three resolutions, and which artifact contains which.
#[tokio::test]
async fn a_lineage_column_ingests_the_database_a_member_table_builds() {
    let layer = layer_toml("nested", "lineage");
    let all: Vec<u64> = (0..N).collect();
    let built = build_side(&all, &layer);
    let seed: Vec<u64> = (0..SEED).collect();
    let ingested = build_side(&seed, &layer);

    let built = serve(&built).await;
    let ingested = serve(&ingested).await;
    ingest_tail(&ingested, LAYER, lineage_keys).await;

    // The fixture is a tree, and the comparison is only worth running if the response says so.
    let view = client_view(&built, &["0", "1"]).await;
    assert!(
        view.artifacts.iter().any(|a| a.parent_key.is_some()),
        "the built side served no edge, so the lineage comparison would prove nothing"
    );

    assert_same_database(&built, &ingested, lineage_keys_of, "a lineage column").await;
}

// ---------------------------------------------------------------------------------------------
// The wire's own rules
// ---------------------------------------------------------------------------------------------

/// **A key no artifact holds refuses the batch, naming it** — and the batch has no effect: no
/// entity id, no row, no partial membership. Minting from an unknown key is the next stage
/// (`artifacts-from-points.md` §6.2), and until it lands this refusal is the honest answer.
#[tokio::test]
async fn an_unknown_key_refuses_the_batch_and_ingests_nothing() {
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer_toml("flat", "cluster"));
    let server = serve(&built).await;

    let before = control_status(&server).await;
    let rows = [SEED, SEED + 1];
    let keys: ArrayRef = Arc::new(Int64Array::from(vec![Some(3), Some(4_242)]));
    let (status, detail) = post_ingest(&server, "unknown-key", ingest_batch(&rows, LAYER, keys)).await;
    assert_eq!(status, 422, "{detail}");
    assert!(
        detail.contains("4242"),
        "the refusal names the key the caller can act on: {detail}"
    );
    let after = control_status(&server).await;
    assert_eq!(
        before["entity_id_high_water"], after["entity_id_high_water"],
        "a refused batch spends no entity id: {detail}"
    );

    // And the same batch with every key known is accepted, so the refusal was the key and not the
    // column.
    let keys: ArrayRef = Arc::new(Int64Array::from(vec![Some(3), Some(4)]));
    let (status, detail) = post_ingest(&server, "known-key", ingest_batch(&rows, LAYER, keys)).await;
    assert_eq!(status, 200, "{detail}");
}

/// A column that names neither a declared scalar nor a registered layer is refused exactly as it
/// was before this existed — and the message says which two things it could have been.
#[tokio::test]
async fn a_column_naming_neither_an_attribute_nor_a_layer_is_refused() {
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer_toml("flat", "cluster"));
    let server = serve(&built).await;

    let keys: ArrayRef = Arc::new(Int64Array::from(vec![Some(3)]));
    let (status, detail) = post_ingest(
        &server,
        "misspelt",
        ingest_batch(&[SEED], "clusters/typo", keys),
    )
    .await;
    assert_eq!(status, 422, "{detail}");
    assert!(
        detail.contains("clusters/typo") && detail.contains("registered layer"),
        "{detail}"
    );
}

/// **`fields` does not reach the wire.** A build maps a file's column name onto the canonical
/// meaning; an ingest batch names the layer. A column named for the *file's* spelling is a column
/// naming nothing, and is refused as one — the same split `[[attribute]]` already has between its
/// `name` and its `field`.
#[tokio::test]
async fn the_build_time_field_name_is_not_a_wire_column() {
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer_toml("flat", "cluster"));
    let server = serve(&built).await;

    let keys: ArrayRef = Arc::new(Int64Array::from(vec![Some(3)]));
    let (status, detail) = post_ingest(&server, "by-field", ingest_batch(&[SEED], "cluster", keys)).await;
    assert_eq!(status, 422, "{detail}");
    assert!(detail.contains("'cluster'"), "{detail}");
}

/// **A list against a levelled declaration is one entry per level**, and a row of another length is
/// a lineage against a levelled layer — refused rather than guessed, because either reading
/// publishes a hierarchy the caller did not write.
#[tokio::test]
async fn a_row_whose_list_is_not_one_entry_per_level_is_refused() {
    let mut layer = layer_toml("tiered", "lineage");
    layer.push_str("\n[[layer.levels]]\nlevel = 0\n\n[[layer.levels]]\nlevel = 1\n\n[[layer.levels]]\nlevel = 2\n");
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer);
    let server = serve(&built).await;

    let item = Arc::new(Field::new("item", DataType::Int64, true));
    let keys: ArrayRef = Arc::new(ListArray::new(
        item,
        OffsetBuffer::new(vec![0i32, 2].into()),
        Arc::new(Int64Array::from(vec![Some(1), Some(10)])) as ArrayRef,
        None,
    ));
    let (status, detail) = post_ingest(&server, "short-list", ingest_batch(&[SEED], LAYER, keys)).await;
    assert_eq!(status, 422, "{detail}");
    assert!(detail.contains("3 levels"), "{detail}");
}

/// **A layer whose membership is evaluated has nothing for a column to say.** A predicate layer
/// answers *who is inside this* per request; a stored membership beside it would be a frozen answer
/// that diverges at the first write — which is what the artifact store already refuses a publication
/// for, made here one step earlier, where the batch can still be rejected without effect.
#[tokio::test]
async fn a_column_naming_a_predicate_layer_is_refused() {
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer_toml("flat", "cluster"));
    let server = serve(&built).await;

    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": "regions/inside",
            "title": "a shape",
            "views": ["s0"],
            "membership": "spatial",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": { "count": 2 },
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": [], "supplied": [], "withdraw_on_member_deletion": true },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "a predicate layer may be declared");

    let keys: ArrayRef = Arc::new(StringArray::from(vec![Some("anything")]));
    let (status, detail) = post_ingest(
        &server,
        "predicate",
        ingest_batch(&[SEED], "regions/inside", keys),
    )
    .await;
    assert_eq!(status, 422, "{detail}");
    assert!(detail.contains("evaluated per request"), "{detail}");
}

/// **An edge the layer does not hold is reported and the memberships still land.**
///
/// A growth adds members and never lineage, so a point naming an edge nobody published states
/// something this route cannot execute. Refusing would block a batch over a roster published without
/// its parents — which discloses nothing, costs a republication, and would make the membership half
/// of an unambiguous entry unavailable. So the operator is told and the join happens. ⊘ It stops
/// being reachable when an unknown key mints its artifact (§6.2), where a chain arrives parent
/// before child in one batch.
#[tokio::test]
async fn a_lineage_naming_an_edge_the_layer_does_not_hold_still_joins() {
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer_toml("flat", "cluster"));
    let server = serve(&built).await;

    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": "tree/x",
            "title": "a tree with no edges yet",
            "views": ["s0"],
            "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "nested", "prune_children": false },
            "content": { "computed": [], "supplied": [], "withdraw_on_member_deletion": true },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    let members: Vec<String> = (0..4u64).map(base64_external_id).collect();
    let resp = server
        .client
        // A layer name is path-shaped, so its slash is percent-encoded into the one path segment
        // the route captures — the same encoding the drop route already takes.
        .put(server.control_url("/control/layers/tree%2Fx/artifacts"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
            "artifacts": [
                { "key": "root", "members": members.clone() },
                { "key": "leaf", "members": members },
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "two artifacts, neither carrying a parent");

    let item = Arc::new(Field::new("item", DataType::Utf8, true));
    let keys: ArrayRef = Arc::new(ListArray::new(
        item,
        OffsetBuffer::new(vec![0i32, 2].into()),
        Arc::new(StringArray::from(vec![Some("root"), Some("leaf")])) as ArrayRef,
        None,
    ));
    let (status, detail) = post_ingest(&server, "no-edge", ingest_batch(&[SEED], "tree/x", keys)).await;
    assert_eq!(
        status, 200,
        "the memberships are unambiguous, so the batch lands: {detail}"
    );

    let view = client_view(&server, &["0", "1"]).await;
    let leaf = view
        .artifacts
        .iter()
        .find(|a| a.key.as_deref() == Some("leaf"))
        .expect("the published artifact serves");
    assert_eq!(
        leaf.parent_key, None,
        "the edge was reported, not invented — a growth adds members and never lineage"
    );
}

/// **A scalar names the artifact at level 0**, which is what a member table with no `level` column
/// means — so a levelled layer takes one, and it is not read as a list of the wrong length.
#[tokio::test]
async fn a_scalar_column_on_a_levelled_layer_names_level_zero() {
    let mut layer = layer_toml("tiered", "lineage");
    layer.push_str("\n[[layer.levels]]\nlevel = 0\n\n[[layer.levels]]\nlevel = 1\n\n[[layer.levels]]\nlevel = 2\n");
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer);
    let server = serve(&built).await;

    // Key 1 is the root, published at level 0 by the seed's own lineage column.
    let keys: ArrayRef = Arc::new(Int64Array::from(vec![Some(1)]));
    let (status, detail) = post_ingest(&server, "scalar-tiered", ingest_batch(&[SEED], LAYER, keys)).await;
    assert_eq!(status, 200, "{detail}");
}

/// **Two spellings of one edge, disagreeing, is a refusal** — the same answer a build gives when two
/// points name different parents for one cluster. A growth adds members and never lineage, so the
/// adjacency a point declares is checked against the edge the publication stored.
#[tokio::test]
async fn a_lineage_contradicting_the_stored_edge_refuses_the_batch() {
    let built = build_side(&(0..SEED).collect::<Vec<_>>(), &layer_toml("nested", "lineage"));
    let server = serve(&built).await;

    // 100's parent is 10 in every row of the seed; this point says it is 11.
    let item = Arc::new(Field::new("item", DataType::Int64, true));
    let keys: ArrayRef = Arc::new(ListArray::new(
        item,
        OffsetBuffer::new(vec![0i32, 3].into()),
        Arc::new(Int64Array::from(vec![Some(1), Some(11), Some(100)])) as ArrayRef,
        None,
    ));
    let (status, detail) = post_ingest(&server, "two-parents", ingest_batch(&[SEED], LAYER, keys)).await;
    assert_eq!(status, 422, "{detail}");
    assert!(
        detail.contains("100") && detail.contains("11"),
        "the refusal names the child and the parent claimed: {detail}"
    );
}
