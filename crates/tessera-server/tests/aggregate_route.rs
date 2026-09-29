//! `POST /v1/aggregate` over HTTP: each table is the count the fixture's own data gives for what
//! the viewer may see, a density table is the viewport's tiles, a table read through its cursor is
//! the whole table, a suppression applies from the next response, every refusal has its status,
//! a request takes its slot from the viewport's gate or, with cells, the bulk-read lane, and
//! frees it when the client goes away, a trailer says when a table was counted over a changed
//! corpus, and `/v1/meta` publishes its limits.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use arrow::array::StringArray;
use arrow::datatypes::DataType;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_server::state::{ComputeGate, ServeLimits};

const N: u64 = 3_000;
const LAYER: &str = "groups/flat";
const ARCHIVES: [&str; 3] = ["astro", "cond", "hep"];

/// `archive` is drawn and indexed; `tag` is a category held only in the record store, so it
/// cannot be counted.
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

[[vocabulary]]
name       = "tag"
width      = "u8"
value_set  = "open"
visibility = "public"

[[attribute]]
name       = "archive"
type       = "category"
render     = true
index      = true
vocabulary = "archive"

[[attribute]]
name       = "tag"
type       = "category"
vocabulary = "tag"
"#;

/// Item `e`'s archive: none on every tenth, otherwise spread so that both viewers see all three.
fn archive_of(e: u64) -> Option<&'static str> {
    (!e.is_multiple_of(10)).then(|| ARCHIVES[((e / 3) % 3) as usize])
}

/// Whether the viewer holding `terms` sees item `e`: term 0 reaches every item, term 1 a third.
fn visible(terms: &[&str], e: u64) -> bool {
    terms_of(e)
        .iter()
        .any(|t| terms.contains(&t.to_string().as_str()))
}

/// The artifacts: `(key, members)`. The first two overlap, and some items are in none.
fn planted() -> Vec<(&'static str, std::ops::Range<u64>)> {
    vec![
        ("a0", 0..800),
        ("a1", 600..1400),
        ("a2", 1400..2000),
        ("a3", 2500..2600),
    ]
}

fn build_bundle(dir: &Path) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..N).collect();
    let archives = StringArray::from_iter(ids.iter().map(|&e| archive_of(e)));
    let tags = StringArray::from_iter_values(ids.iter().map(|&e| format!("t{}", e % 4)));
    write_points(
        &points,
        &ids,
        scatter,
        vec![column("archive", true, archives), column("tag", false, tags)],
    );
    write_pairs_n(&pairs, N);
    build_declared(
        &dir.join("bundle"),
        &points,
        &pairs,
        &format!("{SCHEMA_TOML}{ID_ATTRIBUTE}"),
    );
}

struct Fixture {
    _tmp: TempDir,
    server: TestServer,
}

async fn fixture_with(compute_gate: ComputeGate, tune: impl FnOnce(&mut ServeLimits)) -> Fixture {
    fixture_with_gates(compute_gate, generous_bulk_gate(), tune).await
}

async fn fixture_with_gates(
    compute_gate: ComputeGate,
    bulk_gate: ComputeGate,
    tune: impl FnOnce(&mut ServeLimits),
) -> Fixture {
    static BUILT: OnceLock<TempDir> = OnceLock::new();
    let tmp = TempDir::new().unwrap();
    let bundle = copy_built(&BUILT, tmp.path(), build_bundle);
    let server = spawn_server_with_bulk_reads(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        compute_gate,
        bulk_gate,
        tune,
    )
    .await;
    register(&server, flat_layer(LAYER)).await;
    let artifacts: Vec<Value> = planted()
        .into_iter()
        .map(|(key, range)| json!({ "key": key, "members": members(range) }))
        .collect();
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            LAYER.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "field": "id", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "{}", resp.text().await.unwrap());
    tick(&server).await;
    Fixture { _tmp: tmp, server }
}

async fn fixture() -> Fixture {
    fixture_with(generous_test_gate(), |_| {}).await
}

async fn post(server: &TestServer, route: &str, token: &str, body: &Value) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url(route))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// One response, which must be a 200, decoded, with its headers.
async fn aggregate_ok(
    server: &TestServer,
    token: &str,
    body: &Value,
) -> (reqwest::header::HeaderMap, DecodedAggregate) {
    let resp = post(server, "/v1/aggregate", token, body).await;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    (headers, decode_aggregate(&bytes))
}

/// A whole read, carried across responses by each trailer's `next`.
async fn read_all(server: &TestServer, token: &str, body: &Value) -> Vec<DecodedAggregate> {
    let mut body = body.clone();
    let mut responses = Vec::new();
    loop {
        let (_, decoded) = aggregate_ok(server, token, &body).await;
        let next = decoded.trailer["next"].clone();
        responses.push(decoded);
        assert!(responses.len() < 10_000, "a read that never ends");
        match next {
            Value::Null => return responses,
            Value::String(cursor) => body["cursor"] = json!(cursor),
            other => panic!("next is {other}"),
        }
    }
}

/// The rows of one grouping across `responses`.
fn table(responses: &[DecodedAggregate], grouping: u64) -> Vec<AggregateRow> {
    responses
        .iter()
        .flat_map(DecodedAggregate::rows)
        .filter(|(g, _)| *g == grouping)
        .map(|(_, row)| row)
        .collect()
}

/// The items `terms` may see that `set` admits.
fn items(terms: &[&str], set: impl Fn(u64) -> bool) -> Vec<u64> {
    (0..N).filter(|&e| visible(terms, e) && set(e)).collect()
}

/// A value grouping's rows by the contract: the listed keys with their counts, then `rest` and
/// `none` where non-zero.
fn value_rows(
    items: &[u64],
    listed: impl FnOnce(&BTreeMap<&'static str, u64>) -> Vec<&'static str>,
) -> Vec<(String, Option<String>, u64)> {
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut none = 0;
    for &e in items {
        match archive_of(e) {
            Some(archive) => *counts.entry(archive).or_default() += 1,
            None => none += 1,
        }
    }
    let listed = listed(&counts);
    let mut rows: Vec<(String, Option<String>, u64)> = listed
        .iter()
        .map(|&key| {
            (
                "listed".to_string(),
                Some(key.to_string()),
                counts.get(key).copied().unwrap_or(0),
            )
        })
        .collect();
    let rest: u64 = counts
        .iter()
        .filter(|(key, _)| !listed.contains(key))
        .map(|(_, n)| n)
        .sum();
    if rest > 0 {
        rows.push(("rest".to_string(), None, rest));
    }
    if none > 0 {
        rows.push(("none".to_string(), None, none));
    }
    rows
}

/// The `n` keys with the most items, ties by key.
fn top(counts: &BTreeMap<&'static str, u64>, n: usize) -> Vec<&'static str> {
    let mut ranked: Vec<(&'static str, u64)> = counts.iter().map(|(k, n)| (*k, *n)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    ranked.into_iter().take(n).map(|(k, _)| k).collect()
}

fn as_value_rows(rows: &[AggregateRow]) -> Vec<(String, Option<String>, u64)> {
    rows.iter()
        .map(|row| {
            let key = row.key.as_ref().map(|key| match key {
                AggregateKey::Text(text) => text.clone(),
                AggregateKey::Id(id) => id.to_string(),
            });
            (row.group.clone().unwrap(), key, row.count)
        })
        .collect()
}

/// The left half of the extent, as a region leaf, and whether item `e` lies in it.
fn left_half() -> Value {
    json!({ "region": { "bbox": [0.0, 0.0, 499.5, 1000.0] } })
}

fn in_left_half(e: u64) -> bool {
    scatter(e).0 < 499.5
}

async fn viewport_tiles(
    server: &TestServer,
    token: &str,
    zoom: u8,
    filters: Option<&Value>,
) -> DecodedViewport {
    let mut body = json!({ "view": "s0", "zoom": zoom, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0 });
    if let Some(filters) = filters {
        body["filters"] = filters.clone();
    }
    let resp = post(server, "/v1/viewport", token, &body).await;
    assert_eq!(resp.status().as_u16(), 200);
    decode_viewport_frames(&resp.bytes().await.unwrap())
}

/// **A value grouping counts each value among what the viewer may see**: the size of the set is
/// the viewport's matched count, `top` lists the values with the most items then `rest` and
/// `none`, a named list keeps its order and a known value with no item, and an unknown value gets
/// no row.
#[tokio::test]
async fn a_value_grouping_counts_each_value_the_viewer_sees() {
    let f = fixture().await;
    for terms in [&["0"][..], &["1"][..]] {
        let token = token_for(&f.server, terms).await;
        for filters in [None, Some(left_half())] {
            let set = items(terms, |e| filters.is_none() || in_left_half(e));
            let matched: u64 = viewport_tiles(&f.server, &token, 0, filters.as_ref())
                .await
                .tiles
                .iter()
                .map(|t| t.2)
                .sum();
            assert_eq!(matched, set.len() as u64);
            let mut body = json!({
                "view": "s0",
                "groupings": [
                    {},
                    { "by": { "field": "archive", "top": 2 } },
                    { "by": { "field": "archive", "values": ["hep", "nope", "astro"] } },
                ],
            });
            if let Some(filters) = &filters {
                body["filters"] = filters.clone();
            }
            let (headers, decoded) = aggregate_ok(&f.server, &token, &body).await;
            assert_eq!(
                headers.get("x-tessera-region").is_some(),
                filters.is_some(),
                "the region header is present exactly with a region leaf"
            );
            let heads: Vec<&Value> = decoded.tables.iter().map(|(head, _)| head).collect();
            assert_eq!(heads.len(), 3);
            for (index, head) in heads.iter().enumerate() {
                assert_eq!(head["grouping"], index);
                assert_eq!(head["total"], set.len());
                assert_eq!(head["resumed"], false);
                assert!(head.get("reference_total").is_none());
            }
            assert!(heads[0].get("groups").is_none());
            let distinct: HashSet<_> = set.iter().filter_map(|&e| archive_of(e)).collect();
            assert_eq!(heads[1]["groups"], distinct.len());

            let responses = [decoded];
            let size = table(&responses, 0);
            assert_eq!(size.len(), 1);
            assert_eq!(size[0].count, set.len() as u64);
            assert_eq!(size[0].group, None);
            assert_eq!(
                as_value_rows(&table(&responses, 1)),
                value_rows(&set, |counts| top(counts, 2)),
                "{terms:?} {filters:?}"
            );
            assert_eq!(
                as_value_rows(&table(&responses, 2)),
                value_rows(&set, |_| vec!["hep", "astro"]),
                "{terms:?} {filters:?}"
            );
            let sum: u64 = table(&responses, 1).iter().map(|row| row.count).sum();
            assert_eq!(sum, set.len() as u64, "a value table sums to the set");
        }
    }
}

/// **The body and its columns are the contract's**: a table head before each table's first page,
/// then its pages, then the trailer; `group` a dictionary of `int8`, a field's `key` and `title`
/// dictionaries of `int32`, a layer's `key` a `uint64`, `cell`, `count` and `reference_count`
/// `uint64` and `lift` `float64`.
#[tokio::test]
async fn the_body_and_its_columns_are_the_contracts() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let (_, decoded) = aggregate_ok(
        &f.server,
        &token,
        &json!({
            "view": "s0",
            "reference": {},
            "groupings": [
                {},
                { "by": { "field": "archive", "top": 3 }, "cells": { "depth": 4 } },
                { "by": { "layer": LAYER, "top": 2 } },
            ],
        }),
    )
    .await;
    let types = |grouping: usize| -> Vec<(String, DataType)> {
        decoded.tables[grouping].1[0]
            .0
            .schema()
            .fields()
            .iter()
            .map(|f| (f.name().clone(), f.data_type().clone()))
            .collect()
    };
    let dictionary =
        |key: DataType| DataType::Dictionary(Box::new(key), Box::new(DataType::Utf8));
    assert_eq!(
        types(0),
        vec![
            ("count".into(), DataType::UInt64),
            ("reference_count".into(), DataType::UInt64),
            ("lift".into(), DataType::Float64),
        ]
    );
    assert_eq!(
        types(1),
        vec![
            ("group".into(), dictionary(DataType::Int8)),
            ("key".into(), dictionary(DataType::Int32)),
            ("title".into(), dictionary(DataType::Int32)),
            ("cell".into(), DataType::UInt64),
            ("count".into(), DataType::UInt64),
            ("reference_count".into(), DataType::UInt64),
            ("lift".into(), DataType::Float64),
        ]
    );
    assert_eq!(
        types(2),
        vec![
            ("group".into(), dictionary(DataType::Int8)),
            ("key".into(), DataType::UInt64),
            ("count".into(), DataType::UInt64),
            ("reference_count".into(), DataType::UInt64),
            ("lift".into(), DataType::Float64),
        ]
    );
    for (head, pages) in &decoded.tables {
        assert_eq!(head["reference_total"], N, "{head}");
        assert!(!pages.is_empty());
    }
    let trailer = &decoded.trailer;
    assert_eq!((&trailer["next"], &trailer["ended_by"]), (&Value::Null, &json!("end")));
    let rows: usize = decoded
        .tables
        .iter()
        .flat_map(|(_, pages)| pages)
        .map(|(batch, _)| batch.num_rows())
        .sum();
    assert_eq!(trailer["rows"], rows);
    assert_eq!(trailer["pages"], 3);
}

/// **A density table is the viewport's tiles**: at a depth the viewport can draw, every non-empty
/// cell and its count is a tile's matched count, and in each cell a value grouping's rows sum to
/// that count.
#[tokio::test]
async fn a_density_table_is_the_viewports_tiles() {
    let f = fixture().await;
    let token = token_for(&f.server, &["1"]).await;
    let filters = json!({ "archive": { "in": ["astro", "hep"] } });
    for depth in [0u8, 3, 6] {
        let tiles: Vec<(u64, u64)> = viewport_tiles(&f.server, &token, depth, Some(&filters))
            .await
            .tiles
            .iter()
            .filter(|t| t.2 > 0)
            .map(|t| (t.0, t.2))
            .collect();
        let (_, decoded) = aggregate_ok(
            &f.server,
            &token,
            &json!({
                "view": "s0",
                "filters": filters,
                "groupings": [
                    { "cells": { "depth": depth } },
                    { "by": { "field": "archive", "top": 1 }, "cells": { "depth": depth } },
                ],
            }),
        )
        .await;
        let responses = [decoded];
        let cells: Vec<(u64, u64)> = table(&responses, 0)
            .iter()
            .map(|row| (row.cell.unwrap(), row.count))
            .collect();
        let mut sorted = tiles.clone();
        sorted.sort();
        assert_eq!(cells, sorted, "depth {depth}");
        let mut by_cell: BTreeMap<u64, u64> = BTreeMap::new();
        for row in table(&responses, 1) {
            *by_cell.entry(row.cell.unwrap()).or_default() += row.count;
        }
        assert_eq!(by_cell.into_iter().collect::<Vec<_>>(), cells, "depth {depth}");
    }
}

/// **An artifact grouping counts the members the viewer sees**: `top` lists the artifacts with
/// the most, a named list keeps its order, `rest` holds items in an unlisted artifact and in no
/// listed one, `none` the items in no artifact, and an id naming nothing gets no row.
#[tokio::test]
async fn an_artifact_grouping_counts_the_members_the_viewer_sees() {
    let f = fixture().await;
    let token = token_for(&f.server, &["1"]).await;
    let ids = {
        let resp = post(
            &f.server,
            "/v1/artifacts",
            &token,
            &json!({ "view": "s0", "layer": LAYER, "fields": ["key"] }),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);
        let decoded = decode_records(&resp.bytes().await.unwrap());
        let (batch, _) = &decoded.pages[0];
        let keys = batch
            .column_by_name("key")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let ids = decoded.tessera_ids();
        keys.iter()
            .zip(ids)
            .map(|(key, id)| (key.unwrap().to_string(), id))
            .collect::<BTreeMap<String, u64>>()
    };
    let set = items(&["1"], in_left_half);
    let count = |range: &std::ops::Range<u64>| set.iter().filter(|e| range.contains(e)).count() as u64;
    let in_any = |e: &u64, keys: &[&str]| {
        planted()
            .iter()
            .any(|(key, range)| keys.contains(key) && range.contains(e))
    };
    let all: Vec<&str> = planted().iter().map(|(key, _)| *key).collect();
    let expected = |listed: &[&str]| -> Vec<(String, Option<String>, u64)> {
        let mut rows: Vec<_> = listed
            .iter()
            .map(|key| {
                let range = &planted().into_iter().find(|(k, _)| k == key).unwrap().1;
                ("listed".to_string(), Some(ids[*key].to_string()), count(range))
            })
            .collect();
        let rest = set
            .iter()
            .filter(|e| in_any(e, &all) && !in_any(e, listed))
            .count() as u64;
        let none = set.iter().filter(|e| !in_any(e, &all)).count() as u64;
        for (group, n) in [("rest", rest), ("none", none)] {
            if n > 0 {
                rows.push((group.to_string(), None, n));
            }
        }
        rows
    };
    let mut ranked: Vec<(&str, u64)> = planted()
        .iter()
        .map(|(key, range)| (*key, count(range)))
        .filter(|(_, n)| *n > 0)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(ids[a.0].cmp(&ids[b.0])));
    let top2: Vec<&str> = ranked.iter().take(2).map(|(key, _)| *key).collect();

    let (_, decoded) = aggregate_ok(
        &f.server,
        &token,
        &json!({
            "view": "s0",
            "filters": left_half(),
            "groupings": [
                { "by": { "layer": LAYER, "top": 2 } },
                { "by": { "layer": LAYER, "artifacts": [ids["a2"].to_string(), 7, ids["a0"]] } },
            ],
        }),
    )
    .await;
    assert_eq!(decoded.tables[0].0["groups"], ranked.len());
    let responses = [decoded];
    assert_eq!(as_value_rows(&table(&responses, 0)), expected(&top2));
    assert_eq!(as_value_rows(&table(&responses, 1)), expected(&["a2", "a0"]));
}

/// **A reference adds its counts and the lift**: `{}` is the whole visible set, a filter a subset
/// of it, and each row's lift is its share of the set over its share of the reference.
#[tokio::test]
async fn a_reference_adds_its_counts_and_the_lift() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let set = items(&["0"], in_left_half);
    for (reference, admitted) in [
        (json!({}), Box::new(|_: u64| true) as Box<dyn Fn(u64) -> bool>),
        (
            json!({ "archive": { "in": ["cond"] } }),
            Box::new(|e: u64| archive_of(e) == Some("cond")),
        ),
    ] {
        let refset = items(&["0"], admitted);
        let (headers, decoded) = aggregate_ok(
            &f.server,
            &token,
            &json!({
                "view": "s0",
                "filters": left_half(),
                "reference": reference,
                "groupings": [ {}, { "by": { "field": "archive", "values": ARCHIVES } } ],
            }),
        )
        .await;
        assert_eq!(headers["x-tessera-region"], "exact");
        for (head, _) in &decoded.tables {
            assert_eq!(head["total"], set.len());
            assert_eq!(head["reference_total"], refset.len());
        }
        let responses = [decoded];
        let size = &table(&responses, 0)[0];
        assert_eq!(size.reference_count, Some(refset.len() as u64));
        let lift = |count: u64, reference: u64| {
            (reference > 0).then(|| {
                (count as f64 / set.len() as f64) / (reference as f64 / refset.len() as f64)
            })
        };
        assert_eq!(size.lift, lift(set.len() as u64, refset.len() as u64));
        for row in table(&responses, 1) {
            let of = |items: &[u64]| -> u64 {
                items
                    .iter()
                    .filter(|&&e| match &row.key {
                        Some(AggregateKey::Text(key)) => archive_of(e) == Some(key.as_str()),
                        _ => archive_of(e).is_none(),
                    })
                    .count() as u64
            };
            assert_ne!(row.group.as_deref(), Some("rest"), "every value is named");
            assert_eq!((row.count, row.reference_count), (of(&set), Some(of(&refset))), "{row:?}");
            match (row.lift, lift(row.count, of(&refset))) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-12, "{row:?}"),
                (a, b) => assert_eq!(a, b, "{row:?}"),
            }
        }
    }
}

/// **The region header is the viewport's**: `exact`, or a cover and its depth once the region's
/// cells pass `max_region_cells`, whether the region is in the set or the reference.
#[tokio::test]
async fn the_region_header_is_exact_or_a_cover() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let circle = json!({ "region": { "circle": [500.0, 500.0, 300.0], "space": "view" } });
    let body = |key: &str| json!({ "view": "s0", key: circle, "groupings": [{}] });
    for key in ["filters", "reference"] {
        let (headers, _) = aggregate_ok(&f.server, &token, &body(key)).await;
        assert_eq!(headers["x-tessera-region"], "exact", "{key}");
    }
    f.server.state.engine.set_max_region_cells(1);
    for key in ["filters", "reference"] {
        let (headers, _) = aggregate_ok(&f.server, &token, &body(key)).await;
        let verdict = headers["x-tessera-region"].to_str().unwrap();
        assert!(verdict.starts_with("cover; depth="), "{key}: {verdict}");
    }
    let (headers, _) =
        aggregate_ok(&f.server, &token, &json!({ "view": "s0", "groupings": [{}] })).await;
    assert!(headers.get("x-tessera-region").is_none());
    assert!(headers.contains_key("x-tessera-identity-key"));
    for header in ["x-tessera-server-us", "x-tessera-admission-us"] {
        assert!(headers[header].to_str().unwrap().bytes().all(|b| b.is_ascii_digit()));
    }
}

/// **A table read through its cursor is the whole table**: pages of a few rows, a few pages a
/// response, give the rows one response gives, each response opening with the head of the table
/// it continues, marked resumed.
#[tokio::test]
async fn a_table_read_through_its_cursor_is_the_whole_table() {
    let f = fixture().await;
    let token = token_for(&f.server, &["1"]).await;
    let groupings = json!([
        { "cells": { "depth": 10 } },
        { "by": { "field": "archive", "top": 2 }, "cells": { "depth": 8 } },
        { "by": { "layer": LAYER, "top": 3 } },
    ]);
    let whole = aggregate_ok(
        &f.server,
        &token,
        &json!({ "view": "s0", "reference": {}, "groupings": groupings }),
    )
    .await
    .1;
    let paged = read_all(
        &f.server,
        &token,
        &json!({ "view": "s0", "reference": {}, "groupings": groupings, "page_rows": 37,
                 "pages": 3 }),
    )
    .await;
    assert!(paged.len() > 10, "the read spans many responses");
    for grouping in 0..3 {
        assert_eq!(table(&paged, grouping), table(std::slice::from_ref(&whole), grouping));
    }
    for (i, response) in paged.iter().enumerate() {
        let (head, _) = &response.tables[0];
        let continued = i > 0 && response.tables[0].0["grouping"] == paged[i - 1].tables.last().unwrap().0["grouping"];
        assert_eq!(head["resumed"], continued, "response {i}: {head}");
        for (_, pages) in &response.tables {
            for (batch, end) in pages {
                assert!(batch.num_rows() <= 37);
                let ended_by = end["ended_by"].as_str().unwrap();
                assert!(["rows", "end"].contains(&ended_by), "{end}");
            }
        }
    }
    let cursor = paged[0].trailer["next"].as_str().unwrap();
    let resp = post(
        &f.server,
        "/v1/aggregate",
        &token,
        &json!({ "view": "s0", "groupings": [{}], "cursor": cursor }),
    )
    .await;
    assert_eq!(
        refused(resp, 422).await,
        "contract",
        "a cursor opens only for its own request"
    );
}

/// **A suppression applies from the next response**: the set shrinks by the item, as does its
/// value's row.
#[tokio::test]
async fn a_suppression_applies_from_the_next_response() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let body = json!({
        "view": "s0",
        "groupings": [ {}, { "by": { "field": "archive", "values": ["hep"] } } ],
    });
    let counts = |decoded: &DecodedAggregate| -> (u64, u64) {
        let responses = std::slice::from_ref(decoded);
        (table(responses, 0)[0].count, table(responses, 1)[0].count)
    };
    let (_, before) = aggregate_ok(&f.server, &token, &body).await;
    let victim = (1..N).find(|&e| archive_of(e) == Some("hep")).unwrap();
    let resp = f
        .server
        .client
        .post(f.server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!([{ "field": "id", "value": member(victim), "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (_, after) = aggregate_ok(&f.server, &token, &body).await;
    let (total, hep) = counts(&before);
    assert_eq!(counts(&after), (total - 1, hep - 1));
}

/// **Every refusal has its status**, decided before any row is sent, and a limit exceeded is named.
#[tokio::test]
async fn every_refusal_has_its_status() {
    let f = fixture_with(generous_test_gate(), |limits| {
        limits.max_aggregate_groupings = 2;
        limits.max_aggregate_top = 5;
        limits.max_aggregate_named = 3;
    })
    .await;
    let token = token_for(&f.server, &["0"]).await;
    let (_, first) = aggregate_ok(
        &f.server,
        &token,
        &json!({ "view": "s0", "groupings": [{ "cells": { "depth": 10 } }], "page_rows": 10,
                 "pages": 1 }),
    )
    .await;
    let cursor = first.trailer["next"].as_str().unwrap().to_string();
    let by = |by: Value| json!({ "view": "s0", "groupings": [{ "by": by }] });
    let contract = [
        (json!({ "view": "s0", "groupings": [{}], "unknown": 1 }), None),
        (json!({ "groupings": [{}] }), None),
        (json!({ "view": "s0" }), None),
        (json!({ "view": "s0", "groupings": [] }), None),
        (json!({ "view": "s0", "groupings": [{}, {}, {}] }), Some("max_aggregate_groupings")),
        (json!({ "view": "s0", "groupings": [{ "unknown": 1 }] }), None),
        (json!({ "view": "s0", "groupings": [{ "cells": { "depth": 33 } }] }), None),
        (json!({ "view": "s0", "groupings": [{ "cells": { "depth": 300 } }] }), None),
        (json!({ "view": "s0", "groupings": [{ "cells": { "depth": 4, "area": [5.0, 0.0, 1.0, 9.0] } }] }), None),
        (json!({ "view": "s0", "groupings": [{ "cells": { "depth": 4, "area": [0.0, 0.0, 1.0] } }] }), None),
        (json!({ "view": "s0", "groupings": [{ "cells": { "zoom": 3 } }] }), None),
        (by(json!({ "top": 2 })), None),
        (by(json!({ "field": "archive", "layer": LAYER, "top": 2 })), None),
        (by(json!({ "field": "archive" })), None),
        (by(json!({ "field": "archive", "top": 2, "values": ["hep"] })), None),
        (by(json!({ "field": "archive", "top": 0 })), None),
        (by(json!({ "field": "archive", "top": 6 })), Some("max_aggregate_top")),
        (by(json!({ "field": "archive", "values": [] })), None),
        (by(json!({ "field": "archive", "values": ["a", "b", "c", "d"] })), Some("max_aggregate_named")),
        (by(json!({ "field": "archive", "artifacts": [1] })), None),
        (by(json!({ "field": "archive", "top": 1, "level": 0 })), None),
        (by(json!({ "field": "archive", "top": 1, "unknown": 1 })), None),
        (by(json!({ "layer": LAYER, "values": ["a"] })), None),
        (by(json!({ "layer": LAYER, "artifacts": [] })), None),
        (by(json!({ "layer": LAYER, "artifacts": ["one"] })), None),
        (by(json!({ "layer": LAYER, "top": 1, "artifacts": [1] })), None),
        (by(json!({ "layer": LAYER, "top": 1, "level": 0 })), None),
        (by(json!({ "layer": "no/such", "top": 1 })), None),
        (by(json!({ "field": "nope", "top": 1 })), None),
        (by(json!({ "field": "id", "top": 1 })), None),
        (by(json!({ "field": "tag", "top": 1 })), None),
        (json!({ "view": "s0", "groupings": [{}], "filters": { "nope": { "eq": 1 } } }), None),
        (json!({ "view": "s0", "groupings": [{}], "reference": { "nope": { "eq": 1 } } }), None),
        (json!({ "view": "s0", "groupings": [{}], "page_rows": 0 }), None),
        (json!({ "view": "s0", "groupings": [{}], "cursor": "not a cursor" }), None),
        (json!({ "view": "s0", "groupings": [{}], "cursor": cursor }), None),
    ];
    for (body, limit) in contract {
        let resp = post(&f.server, "/v1/aggregate", &token, &body).await;
        assert_eq!(resp.status().as_u16(), 422, "{body}");
        let text = resp.text().await.unwrap();
        assert_eq!(error_code(&text), "contract", "{body}");
        if let Some(limit) = limit {
            assert!(text.contains(limit), "{body}: {text}");
        }
    }
    // An unknown field, a field that is not a category and a category that cannot be counted
    // are three refusals, each with its own detail.
    let details = [
        refusal_detail(&f.server, &token, "nope").await,
        refusal_detail(&f.server, &token, "id").await,
        refusal_detail(&f.server, &token, "tag").await,
    ];
    assert_eq!(details.iter().collect::<HashSet<_>>().len(), 3, "{details:?}");

    let resp = post(&f.server, "/v1/aggregate", &token, &json!({ "view": "nowhere", "groupings": [{}] })).await;
    assert_eq!(refused(resp, 404).await, "unknown");
    let resp = f
        .server
        .client
        .post(f.server.viewer_url("/v1/aggregate"))
        .json(&json!({ "view": "s0", "groupings": [{}] }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused(resp, 401).await, "bad-credential");
}

/// The detail of the refusal to count `field`, with the field's name taken out.
async fn refusal_detail(server: &TestServer, token: &str, field: &str) -> String {
    let body = json!({ "view": "s0", "groupings": [{ "by": { "field": field, "top": 1 } }] });
    let resp = post(server, "/v1/aggregate", token, &body).await;
    assert_eq!(resp.status().as_u16(), 422);
    let body: Value = resp.json().await.unwrap();
    body["detail"].as_str().unwrap().replace(field, "<field>")
}

/// **Every request takes its slot from the viewport's gate**, with cells or without: each is shed
/// with that gate full and served with the bulk-read lane full.
#[tokio::test]
async fn every_request_is_admitted_as_the_viewport_is() {
    let f = fixture_with_gates(ComputeGate::new(1, 0, 250), ComputeGate::for_bulk_reads(1), |_| {})
        .await;
    let token = token_for(&f.server, &["0"]).await;
    let counts = json!({ "view": "s0", "groupings": [{}, { "by": { "field": "archive", "top": 2 } }] });
    let cells = json!({ "view": "s0", "groupings": [{}, { "cells": { "depth": 6 } }] });
    for body in [&counts, &cells] {
        let held = hold(&f.server.state.compute_gate).await;
        let resp = post(&f.server, "/v1/aggregate", &token, body).await;
        assert!(resp.headers().contains_key("retry-after"));
        assert_eq!(refused(resp, 429).await, "backpressure", "{body}");
        drop(held);
        let held = hold(&f.server.state.bulk_gate).await;
        aggregate_ok(&f.server, &token, body).await;
        drop(held);
    }
}

/// Both permits of `gate`, once the request before has let them go.
async fn hold(gate: &ComputeGate) -> tessera_server::state::GatePermits {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match gate.admit().await {
            Ok((permits, _)) => return permits,
            Err(_) => {
                assert!(Instant::now() < deadline, "the gate never came free");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// **A client that goes away stops the work and frees the slot.** The response is far larger
/// than the sockets buffer, so its producer waits on the reader holding the viewport's only slot,
/// and a viewport is shed; once the reader goes, a viewport is served well inside the stall budget
/// the producer would otherwise wait out.
#[tokio::test]
async fn a_client_that_goes_away_stops_the_work() {
    let f = fixture_with(ComputeGate::new(1, 0, 250), |limits| {
        limits.stream_write_stall_ms = 120_000;
    })
    .await;
    let token = token_for(&f.server, &["0"]).await;
    let groupings: Vec<Value> = (0..16)
        .map(|_| json!({ "by": { "field": "archive", "top": 3 }, "cells": { "depth": 10 } }))
        .collect();
    let mut reader = post(
        &f.server,
        "/v1/aggregate",
        &token,
        &json!({ "view": "s0", "groupings": groupings, "page_rows": 1 }),
    )
    .await;
    assert_eq!(reader.status().as_u16(), 200);
    assert!(reader.chunk().await.unwrap().is_some());

    let viewport = json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0 });
    let resp = post(&f.server, "/v1/viewport", &token, &viewport).await;
    assert_eq!(
        refused(resp, 429).await,
        "backpressure",
        "the unread response still holds the slot"
    );

    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let resp = post(&f.server, "/v1/viewport", &token, &viewport).await;
        if resp.status().as_u16() == 200 {
            break;
        }
        assert_eq!(resp.status().as_u16(), 429);
        assert!(
            Instant::now() < deadline,
            "the slot was not freed after the client went away"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **A response that counts a changed corpus says so**: a suppression between two responses of
/// one paged table puts `recomposed: true` in the next trailer, and only there.
#[tokio::test]
async fn a_trailer_says_when_a_table_was_counted_over_a_changed_corpus() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let mut body = json!({
        "view": "s0",
        "groupings": [{ "cells": { "depth": 10 } }],
        "page_rows": 100,
        "pages": 1,
    });
    let (_, first) = aggregate_ok(&f.server, &token, &body).await;
    assert!(first.trailer.get("recomposed").is_none(), "{}", first.trailer);
    body["cursor"] = first.trailer["next"].clone();
    let (_, unchanged) = aggregate_ok(&f.server, &token, &body).await;
    assert!(unchanged.trailer.get("recomposed").is_none(), "{}", unchanged.trailer);

    let resp = f
        .server
        .client
        .post(f.server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!([{ "field": "id", "value": member(N - 1), "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    body["cursor"] = unchanged.trailer["next"].clone();
    let (_, changed) = aggregate_ok(&f.server, &token, &body).await;
    assert_eq!(changed.trailer["recomposed"], true, "{}", changed.trailer);
    body["cursor"] = changed.trailer["next"].clone();
    let (_, after) = aggregate_ok(&f.server, &token, &body).await;
    assert!(after.trailer.get("recomposed").is_none(), "{}", after.trailer);
}

/// **The cell limit counts the cells at a depth in an area**, under the default of 1,048,576: the
/// whole view at depth 10 is accepted and at 11 refused, one detail naming the count, the limit
/// and the deepest depth that fits; a small area at depth 20 is accepted and lists only its
/// cells; and a grouping with several groups is held to the same count as one with none.
#[tokio::test]
async fn the_cell_limit_counts_the_cells_of_the_area() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let whole = |depth: u8, by: Option<Value>| {
        let mut grouping = json!({ "cells": { "depth": depth } });
        if let Some(by) = by {
            grouping["by"] = by;
        }
        json!({ "view": "s0", "groupings": [grouping] })
    };
    for by in [None, Some(json!({ "field": "archive", "top": 3 }))] {
        aggregate_ok(&f.server, &token, &whole(10, by.clone())).await;
        let resp = post(&f.server, "/v1/aggregate", &token, &whole(11, by.clone())).await;
        assert_eq!(resp.status().as_u16(), 422);
        let detail = resp.json::<Value>().await.unwrap()["detail"].as_str().unwrap().to_string();
        for part in ["4194304", "1048576", "max_aggregate_cells", "depth 10"] {
            assert!(detail.contains(part), "{detail}");
        }
    }

    // A small area at depth 20 is under the limit, grouped or not.
    let area = [100.0, 100.0, 100.5, 100.5];
    aggregate_ok(
        &f.server,
        &token,
        &json!({ "view": "s0", "groupings": [
            { "cells": { "depth": 20, "area": area } },
            { "cells": { "depth": 20, "area": area }, "by": { "field": "archive", "top": 3 } },
        ] }),
    )
    .await;
}

/// **An area lists only its own cells**: the rows of a table over an area are the whole view's
/// rows whose cells lie in it, at the viewport's depths and past them, and its head is the
/// whole set's.
#[tokio::test]
async fn an_area_lists_only_its_own_cells() {
    let f = fixture().await;
    let token = token_for(&f.server, &["1"]).await;
    let area = [120.0, 250.5, 610.0, 580.0];
    for depth in [3u8, 7, 10] {
        let body = |area: Option<[f64; 4]>| {
            let mut cells = json!({ "depth": depth });
            if let Some(area) = area {
                cells["area"] = json!(area);
            }
            json!({ "view": "s0", "reference": {}, "groupings": [
                { "cells": cells.clone() },
                { "by": { "field": "archive", "top": 2 }, "cells": cells },
            ] })
        };
        let (_, all) = aggregate_ok(&f.server, &token, &body(None)).await;
        let (_, some) = aggregate_ok(&f.server, &token, &body(Some(area))).await;
        let tiles: HashSet<u64> = if depth <= 7 {
            viewport_tiles_in(&f.server, &token, depth, area).await
        } else {
            HashSet::new()
        };
        for grouping in 0..2 {
            let within: Vec<AggregateRow> = table(std::slice::from_ref(&some), grouping);
            let whole = table(std::slice::from_ref(&all), grouping);
            assert!(!within.is_empty() && within.len() < whole.len(), "depth {depth}");
            let kept: Vec<AggregateRow> = whole
                .into_iter()
                .filter(|row| within.iter().any(|w| w.cell == row.cell))
                .collect();
            assert_eq!(within, kept, "depth {depth}: an area's cell counts all of its items");
            if depth <= 7 {
                assert!(within.iter().all(|row| tiles.contains(&row.cell.unwrap())));
            }
            assert_eq!(some.tables[grouping as usize].0, all.tables[grouping as usize].0);
        }
    }
}

/// The tiles a viewport lists for `bbox` at `zoom`, empty ones included.
async fn viewport_tiles_in(server: &TestServer, token: &str, zoom: u8, bbox: [f64; 4]) -> HashSet<u64> {
    let resp = post(
        server,
        "/v1/viewport",
        token,
        &json!({ "view": "s0", "zoom": zoom, "bbox": bbox, "k": 0 }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    decode_viewport_frames(&resp.bytes().await.unwrap())
        .tiles
        .iter()
        .map(|t| t.0)
        .collect()
}

/// **A large table continues through the cursor within the response budget**: with the budget cut
/// to 64 KiB in pages of 16 KiB, every page's Arrow bytes stay under the page budget, every
/// response's under its budget, and the pages joined are the table read in one response.
#[tokio::test]
async fn a_large_table_continues_through_the_cursor_within_the_budget() {
    let f = fixture_with(generous_test_gate(), |limits| {
        limits.aggregate_response_bytes = 64 << 10;
        limits.aggregate_page_bytes = 16 << 10;
    })
    .await;
    let roomy = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let body = json!({ "view": "s0", "reference": {}, "groupings": [
        { "cells": { "depth": 10 } },
        { "by": { "field": "archive", "top": 3 }, "cells": { "depth": 10 } },
    ] });
    let responses = read_all(&f.server, &token, &body).await;
    assert!(responses.len() > 1, "the table spans responses");
    for response in &responses[..responses.len() - 1] {
        assert_eq!(response.trailer["ended_by"], "budget_bytes");
    }
    for response in &responses {
        let mut bytes = 0;
        for (_, pages) in &response.tables {
            for (batch, _) in pages {
                // The bytes the columns use, not the decoded message they share.
                let page: usize = batch
                    .columns()
                    .iter()
                    .map(|c| c.to_data().get_slice_memory_size().unwrap())
                    .sum();
                assert!(page <= 16 << 10, "a page of {page} bytes");
                bytes += page;
            }
        }
        assert!(bytes <= 64 << 10, "a response of {bytes} bytes");
    }
    let roomy_token = token_for(&roomy.server, &["0"]).await;
    let (_, whole) = aggregate_ok(&roomy.server, &roomy_token, &body).await;
    assert_eq!(whole.trailer["next"], Value::Null, "the default budget holds it in one response");
    for grouping in 0..2 {
        assert_eq!(table(&responses, grouping), table(std::slice::from_ref(&whole), grouping));
    }
}

/// **`/v1/meta` publishes the route's limits** beside the page ceilings.
#[tokio::test]
async fn meta_publishes_the_limits() {
    let f = fixture_with(generous_test_gate(), |limits| {
        limits.max_aggregate_groupings = 7;
        limits.max_aggregate_top = 70;
        limits.max_aggregate_named = 700;
    })
    .await;
    let token = token_for(&f.server, &["0"]).await;
    let meta: Value = f
        .server
        .client
        .get(f.server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let selection = &meta["selection"];
    assert_eq!(selection["max_aggregate_groupings"], 7);
    assert_eq!(selection["max_aggregate_top"], 70);
    assert_eq!(selection["max_aggregate_named"], 700);
    assert_eq!(selection["max_aggregate_cells"], 1 << 20, "the default: every cell at depth 10");
}
