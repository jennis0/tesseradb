//! `point_rows` naming render columns over HTTP: a view with two render columns serves the named
//! one and not the other, with the same points, counts and membership columns as the full answer.

mod common;

use std::collections::BTreeMap;
use std::io::Cursor;

use arrow::array::{ArrayRef, Float32Array, StringArray};
use arrow::ipc::reader::StreamReader;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;

const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  astro = 11
  cond = 22
  hep = 33

[[attribute]]
name       = "archive"
type       = "category"
render     = true
vocabulary = "archive"

[[attribute]]
name   = "score"
type   = "f32"
render = true
"#;

const N: u64 = 400;
const LAYER: &str = "clusters/flat";

async fn fixture(tmp: &TempDir) -> TestServer {
    let dir = tmp.path();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..N).collect();
    let archives = StringArray::from_iter_values(
        ids.iter()
            .map(|&e| ["astro", "cond", "hep"][(e % 3) as usize]),
    );
    let scores = Float32Array::from_iter(ids.iter().map(|&e| Some((e % 97) as f32 * 0.5)));
    write_points(
        &points,
        &ids,
        scatter,
        vec![
            column("archive", false, archives),
            column("score", true, scores),
        ],
    );
    write_pairs_n(&pairs, N);
    build_declared(&dir.join("bundle"), &points, &pairs, &format!("{SCHEMA_TOML}{ID_ATTRIBUTE}"));
    let server = open(dir).await;
    register(
        &server,
        json!({
            "name": LAYER,
            "title": "flat",
            "views": ["s0"],
            "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": ["centroid"], "supplied": [] },
            "depends_on": [],
            "levels": []
        }),
    )
    .await;
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            LAYER.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "artifacts": [
                { "key": "a", "members": members(0..150) },
                { "key": "b", "members": members(150..300) },
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "{}", resp.text().await.unwrap());
    server
}

async fn ask(server: &TestServer, token: &str, point_rows: Option<Value>) -> Vec<u8> {
    let mut body = json!({
        "view": "s0", "zoom": 2, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 50, "layers": [LAYER]
    });
    if let Some(rows) = point_rows {
        body["point_rows"] = rows;
    }
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.bytes().await.unwrap().to_vec()
}

/// The points frames' column names in schema order, and each column concatenated across frames.
fn columns(body: &[u8]) -> (Vec<String>, BTreeMap<String, ArrayRef>) {
    let mut names = Vec::new();
    let mut parts: BTreeMap<String, Vec<ArrayRef>> = BTreeMap::new();
    for (kind, payload) in tessera_wire::split_frames(body).unwrap() {
        if kind != tessera_wire::FRAME_POINTS {
            continue;
        }
        let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
        names = reader.schema().fields().iter().map(|f| f.name().clone()).collect();
        for batch in reader {
            let batch = batch.unwrap();
            for (i, name) in names.iter().enumerate() {
                parts.entry(name.clone()).or_default().push(batch.column(i).clone());
            }
        }
    }
    let joined = parts
        .into_iter()
        .map(|(name, arrays)| {
            let refs: Vec<&dyn arrow::array::Array> = arrays.iter().map(|a| a.as_ref()).collect();
            (name, arrow::compute::concat(&refs).unwrap())
        })
        .collect();
    (names, joined)
}

#[tokio::test]
async fn a_named_column_reaches_the_wire_alone_with_the_same_points_counts_and_membership() {
    let tmp = TempDir::new().unwrap();
    let server = fixture(&tmp).await;
    let auth = authorise(&server, &["0", "1"]).await;
    let token = auth["token"].as_str().unwrap();
    let membership = format!("membership:{LAYER}");

    let full = ask(&server, token, None).await;
    let (full_names, full_columns) = columns(&full);
    assert_eq!(full_names, ["tessera_id", "code", "archive", "score", membership.as_str()]);

    let named = ask(&server, token, Some(json!(["score"]))).await;
    let (names, named_columns) = columns(&named);
    assert_eq!(names, ["tessera_id", "code", "score", membership.as_str()]);
    for name in &names {
        assert_eq!(&named_columns[name], &full_columns[name], "{name}");
    }
    let membership_values = &named_columns[&membership];
    assert!(
        membership_values.null_count() < membership_values.len(),
        "the membership column names artifacts"
    );

    let full = decode_viewport_frames(&full);
    let named = decode_viewport_frames(&named);
    assert_eq!(named.tiles, full.tiles);
    assert_eq!(named.served, full.served);
    assert_eq!(named.points, full.points);
}
