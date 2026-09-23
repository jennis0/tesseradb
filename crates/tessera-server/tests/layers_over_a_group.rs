//! **Layers in a database with a plain view and a view group**, declared and filled by the build
//! and by the running service, which must store the same thing.
//!
//! The fixture is `papers`, a plain view over every entity, and `years`, a group whose two views
//! hold the entities of one year each.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryArray, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::*;
use tessera_build::{build, BuildArgs, ViewArgs};

const ENTITIES: u64 = 40;
const YEARS: [&str; 2] = ["2010", "2011"];

fn year_of(e: u64) -> &'static str {
    YEARS[(e * YEARS.len() as u64 / ENTITIES) as usize]
}

/// The entity-scoped clustering: four keys over every paper.
fn cluster_of(e: u64) -> String {
    format!("c{}", e % 4)
}

/// The group-scoped clustering: two keys per year, each drawn on its own year's view.
fn yearly_of(e: u64) -> String {
    format!("{}-{}", year_of(e), e % 2)
}

fn write(path: &Path, columns: Vec<(&str, ArrayRef)>) {
    let schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
            .collect::<Vec<_>>(),
    ));
    let batch = RecordBatch::try_new(
        schema.clone(),
        columns.into_iter().map(|(_, array)| array).collect(),
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn text(values: impl Iterator<Item = String>) -> ArrayRef {
    Arc::new(StringArray::from(values.map(Some).collect::<Vec<_>>()))
}

/// The points of both row spaces, and the member tables the build's layers read.
fn write_sources(dir: &Path, keyed: &dyn Fn(u64) -> bool) {
    let ids = || 0..ENTITIES;
    let xs = || Arc::new(Float64Array::from_iter_values(ids().map(|e| ((e * 37) % 1000) as f64)));
    let ys = || Arc::new(Float64Array::from_iter_values(ids().map(|e| ((e * 53) % 1000) as f64)));
    let entity = || Arc::new(UInt64Array::from_iter_values(ids())) as ArrayRef;
    write(
        &dir.join("papers.parquet"),
        vec![("entity_id", entity()), ("x", xs()), ("y", ys())],
    );
    write(
        &dir.join("years.parquet"),
        vec![
            ("entity_id", entity()),
            ("x", xs()),
            ("y", ys()),
            ("year", text(ids().map(|e| year_of(e).to_string()))),
        ],
    );
    let kept: Vec<u64> = ids().filter(|e| keyed(*e)).collect();
    write(
        &dir.join("clusters.parquet"),
        vec![
            ("entity_id", Arc::new(UInt64Array::from(kept.clone()))),
            ("cluster", text(kept.iter().map(|e| cluster_of(*e)))),
        ],
    );
    write(
        &dir.join("yearly.parquet"),
        vec![
            ("entity_id", Arc::new(UInt64Array::from(kept.clone()))),
            ("cluster", text(kept.iter().map(|e| yearly_of(*e)))),
            ("view", text(kept.iter().map(|e| year_of(*e).to_string()))),
        ],
    );
    write_pairs_n(&dir.join("pairs.parquet"), ENTITIES);
}

const VIEWS_TOML: &str = r#"
[sources]
papers   = "papers.parquet"
years    = "years.parquet"
clusters = "clusters.parquet"
yearly   = "yearly.parquet"

[defaults]
allocation_view = "papers"

[[view]]
name             = "papers"
source           = "papers"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[view_group]]
name             = "years"
extent           = { min = 0.0, max = 1000.0 }
source           = "years"
fields           = { view = "year" }
point_visibility = { default = "public" }
"#;

/// A layer block. `scope` is the TOML value of the layer's scope, or empty for entity scope.
fn layer_toml(name: &str, views: &[&str], scope: &str, source: &str) -> String {
    let views = views
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let scope = match scope {
        "" => String::new(),
        group => format!("scope = {{ group = \"{group}\" }}\n"),
    };
    format!(
        r#"
[[layer]]
name = "{name}"
title = "{name}"
views = [{views}]
{scope}membership = "enumerated"
value_set = "open"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
content = {{ computed = ["centroid", "box"] }}

  [layer.members]
  source = "{source}"
  fields = {{ key = "cluster", entity = "entity_id" }}
"#
    )
}

/// The same layer as `PUT /control/layers` takes it.
fn layer_json(name: &str, views: &[&str], scope: Option<&str>) -> Value {
    json!({
        "name": name,
        "title": name,
        "views": views,
        "membership": "enumerated",
        "value_set": "open",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": ["centroid", "box"], "supplied": [] },
        "depends_on": [],
        "levels": [],
        "scope": match scope {
            Some(group) => json!({ "group": group }),
            None => json!("entity"),
        }
    })
}

struct Built {
    _tmp: TempDir,
    dir: PathBuf,
}

/// The declaration as `tessera build` reads it: parsed, its views enumerated, each layer's `views`
/// expanded against them.
fn parse(dir: &Path, layers: &str) -> Result<tessera_build::config::Config, String> {
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, format!("{VIEWS_TOML}{layers}")).unwrap();
    tessera_build::config::Config::parse(&config_path, &Default::default()).map_err(|e| e.to_string())
}

/// Build the fixture the way the binary does, with `layers` declared and `keyed` saying which
/// entities the member tables carry.
fn build_side(layers: &str, keyed: &dyn Fn(u64) -> bool) -> Built {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    write_sources(&dir, keyed);
    let mut config = parse(&dir, layers).expect("the declaration parses");
    let registry = config.build_views().expect("the roster enumerates");
    let anchor = config.anchor_view(&registry).expect("the anchor is declared");
    let views: Vec<ViewArgs> = registry
        .iter()
        .map(|view| ViewArgs {
            visibility: view.visibility.clone(),
            view_id: view.id.clone(),
            projection: view.projection,
            extent: extent(),
            points: view.source.clone().expect("every view names its points"),
            point_fields: view.fields.clone(),
            select: view.select.clone(),
            access: tessera_build::config::AccessInput::relation(dir.join("pairs.parquet")),
        })
        .collect();
    let groups = config.group_registry(&registry, &views);
    for layer in &mut config.layers {
        layer.views = tessera_build::config::Config::expand_layer_views(&registry, &layer.views);
    }
    let scoped_layers = config
        .scopes
        .layers
        .iter()
        .map(|(layer, group)| {
            (
                layer.clone(),
                tessera_build::ScopedLayer {
                    group: group.clone(),
                    column: "view".to_string(),
                    keys: YEARS.iter().map(|y| y.to_string()).collect(),
                },
            )
        })
        .collect();
    build(&BuildArgs {
        views,
        anchor,
        groups,
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: dir.join("bundle"),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    })
    .expect("the fixture builds");
    Built { _tmp: tmp, dir }
}

async fn serve(built: &Built) -> TestServer {
    spawn_server(
        &built.dir.join("bundle"),
        &built.dir.join("cache"),
        &built.dir.join("wal"),
    )
    .await
}

async fn register(server: &TestServer, declaration: Value) -> (u16, Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST /control/values` in Arrow: an id column and a column named for the layer, under the view
/// header where `view` is given.
async fn post_keys(
    server: &TestServer,
    batch_id: &str,
    view: Option<&str>,
    layer: &str,
    rows: &[u64],
    key_of: &dyn Fn(u64) -> String,
) -> (u16, Value) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new(layer, DataType::Utf8, true),
    ]));
    let ext: Vec<Vec<u8>> = rows.iter().map(|e| external_id_of(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(ext.iter().map(Vec::as_slice))) as ArrayRef,
            text(rows.iter().map(|e| key_of(*e))),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    let body = writer.into_inner().unwrap();
    let mut request = server
        .client
        .post(server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/vnd.apache.arrow.stream");
    if let Some(view) = view {
        request = request.header("x-tessera-view", view);
    }
    let resp = request.body(body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The views `/v1/meta` says a layer is drawn on.
async fn drawn_on(server: &TestServer, layer: &str) -> Vec<String> {
    let auth = authorise(server, &["0", "1"]).await;
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let meta: Value = resp.json().await.unwrap();
    let entry = meta["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == layer)
        .unwrap_or_else(|| panic!("layer {layer} is published: {meta}"));
    let mut views: Vec<String> = entry["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    views.sort();
    views
}

/// One view's artifacts of a layer as browse serves them: key to masked count.
async fn browse(server: &TestServer, view: &str, layer: &str) -> BTreeMap<String, u64> {
    let auth = authorise(server, &["0", "1"]).await;
    let resp = server
        .client
        .post(server.viewer_url("/v1/artifacts/browse"))
        .bearer_auth(auth["token"].as_str().unwrap())
        .json(&json!({ "view": view, "layer": layer, "limit": 200 }))
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
        .map(|row| {
            (
                row["key"].as_str().unwrap().to_string(),
                row["masked_count"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn every_view() -> Vec<String> {
    let mut views = vec!["papers".to_string()];
    views.extend(YEARS.iter().map(|y| format!("years:{y}")));
    views.sort();
    views
}

/// **A layer naming a group is drawn on every view of it, at both paths.**
#[tokio::test]
async fn a_layer_naming_a_group_is_drawn_on_every_view_of_it_at_both_paths() {
    let built = build_side(
        &layer_toml("clusters/topics", &["papers", "years"], "", "clusters"),
        &|_| true,
    );
    let by_build = serve(&built).await;
    assert_eq!(drawn_on(&by_build, "clusters/topics").await, every_view());

    let built = build_side("", &|_| true);
    let live = serve(&built).await;
    let (status, body) = register(
        &live,
        layer_json("clusters/topics", &["papers", "years"], None),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(drawn_on(&live, "clusters/topics").await, every_view());
}

/// **A name in `views` that is neither a view nor a group is refused at both paths**: a layer on
/// no view is registered, reachable and empty.
#[tokio::test]
async fn a_layer_naming_neither_a_view_nor_a_group_is_refused_at_both_paths() {
    let tmp = TempDir::new().unwrap();
    write_sources(tmp.path(), &|_| true);
    assert!(parse(
        tmp.path(),
        &layer_toml("clusters/lost", &["papers", "nowhere"], "", "clusters")
    )
    .is_err());

    let built = build_side("", &|_| true);
    let live = serve(&built).await;
    let (status, body) = register(
        &live,
        layer_json("clusters/lost", &["papers", "nowhere"], None),
    )
    .await;
    assert_eq!(status, 422, "{body}");
}

/// **A group-scoped layer is drawn on its group's views and no others, at both paths**: its
/// artifacts are a set per view of the group, so a view outside the group would hold none.
#[tokio::test]
async fn a_group_scoped_layer_naming_a_view_outside_its_group_is_refused_at_both_paths() {
    const LAYER: &str = "clusters/yearly";
    let tmp = TempDir::new().unwrap();
    write_sources(tmp.path(), &|_| true);
    assert!(parse(
        tmp.path(),
        &layer_toml(LAYER, &["papers", "years"], "years", "yearly")
    )
    .is_err());

    let built = build_side("", &|_| true);
    let live = serve(&built).await;
    let (status, body) = register(
        &live,
        layer_json(LAYER, &["papers", "years"], Some("years")),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
    let (status, body) = register(&live, layer_json(LAYER, &["years"], Some("years"))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(drawn_on(&live, LAYER).await.len(), YEARS.len());
}

fn counts(rows: impl Iterator<Item = u64>, key_of: &dyn Fn(u64) -> String) -> BTreeMap<String, u64> {
    let mut sizes = BTreeMap::new();
    for e in rows {
        *sizes.entry(key_of(e)).or_default() += 1;
    }
    sizes
}

fn of_year(year: &str) -> Vec<u64> {
    (0..ENTITIES).filter(|e| year_of(*e) == year).collect()
}

/// **A key column on a group-scoped layer mints per view, at both paths.** The build reads the
/// view off each member row; the values route reads it off the page's view header, which is how a
/// client sends one page per view.
#[tokio::test]
async fn a_key_column_on_a_group_scoped_layer_mints_in_the_view_it_names() {
    const LAYER: &str = "clusters/yearly";
    let built = build_side(&layer_toml(LAYER, &["years"], "years", "yearly"), &|_| true);
    let by_build = serve(&built).await;

    let built = build_side("", &|_| true);
    let live = serve(&built).await;
    let (status, body) = register(&live, layer_json(LAYER, &["years"], Some("years"))).await;
    assert_eq!(status, 201, "{body}");
    for year in YEARS {
        let view = format!("years:{year}");
        let (status, body) =
            post_keys(&live, year, Some(&view), LAYER, &of_year(year), &yearly_of).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["minted"].as_u64(), Some(2), "{body}");
    }
    tick(&live).await;

    live.shutdown().await;
    let reopened = serve(&built).await;
    for year in YEARS {
        let view = format!("years:{year}");
        let expected = counts(of_year(year).into_iter(), &yearly_of);
        assert_eq!(browse(&by_build, &view, LAYER).await, expected, "the build, {view}");
        assert_eq!(
            browse(&reopened, &view, LAYER).await,
            expected,
            "the values route, after a restart, {view}"
        );
    }
}

/// **A key column on an entity-scoped layer needs no view**, whatever the database's view count:
/// the layer has one artifact set, drawn on every view it names. A group-scoped layer's column
/// still needs one, since its keys are a set per view.
#[tokio::test]
async fn a_key_column_on_an_entity_scoped_layer_needs_no_view_header() {
    const LAYER: &str = "clusters/kmeans";
    const SEEDED: u64 = 8;
    let layers = layer_toml(LAYER, &["papers"], "", "clusters");
    let built = build_side(&layers, &|_| true);
    let by_build = serve(&built).await;

    let built = build_side(&layers, &|e| e < SEEDED);
    let live = serve(&built).await;
    let tail: Vec<u64> = (SEEDED..ENTITIES).collect();
    let (status, body) = post_keys(&live, "week", None, LAYER, &tail, &cluster_of).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["minted"].as_u64(), Some(0), "every key was seeded: {body}");
    tick(&live).await;

    let expected = counts(0..ENTITIES, &cluster_of);
    assert_eq!(browse(&by_build, "papers", LAYER).await, expected);
    assert_eq!(browse(&live, "papers", LAYER).await, expected);
    live.shutdown().await;
    let live = serve(&built).await;
    assert_eq!(
        browse(&live, "papers", LAYER).await,
        expected,
        "a batch naming no view survives a restart"
    );

    let (status, body) = register(
        &live,
        layer_json("clusters/yearly", &["years"], Some("years")),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) =
        post_keys(&live, "no-view", None, "clusters/yearly", &of_year("2010"), &yearly_of).await;
    assert_eq!(status, 422, "{body}");
}
