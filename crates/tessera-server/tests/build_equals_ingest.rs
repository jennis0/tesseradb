//! **A build is an ingest into an empty database.** A corpus built from its files, and the same
//! declaration built empty with each file then sent to the running service in declaration order,
//! answer alike: the same items in each view at the same positions with the same values, seen by
//! the same principals, in the same artifacts; and the rows the build reports refused are the rows
//! the service lists as refused, reason by reason and row by row, the report's sample of each
//! reason's first rows included. Under `strict=true` the service refuses each file that carries a
//! refused row, whole. The service is restarted before it is compared, so what it answers is what
//! its write-ahead log replays.
//!
//! The corpus has two views' points, an attribute file whose rows can carry a `tessera_id`, an
//! access relation, which the service is sent as a batch naming each item once with its labels,
//! and a members file, which it is sent as one publication.
//!
//! Items are compared by the values they hold, since the two paths number them independently.
//! Corpora are drawn from a seed over one or two unique fields with small value ranges, so values
//! repeat within and across files, collide across fields and go missing.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, Float64Array, Int64Array, StringArray, UInt32Array, UInt64Array,
};
use arrow::datatypes::{Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs, BuildReport};
use tessera_spatial::Bounds;

const LAYER: &str = "groups";
/// The access terms the relation gives items: every item one everyone is given, and some a
/// narrower one. Each is the label of its digits at the service.
const EVERYONE: u32 = 0;
const TERM: u32 = 7;
const PRINCIPALS: [&[&str]; 2] = [&["0"], &["0", "7"]];

/// A seeded draw: small ranges, so values repeat and collide.
struct Draws(u64);

impl Draws {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % below
    }

    /// A value of `a`, null one time in ten.
    fn a(&mut self, range: u64) -> Option<u64> {
        (self.next(10) > 0).then(|| self.next(range))
    }

    /// A value of `b`, null one time in five.
    fn b(&mut self, range: u64) -> Option<String> {
        (self.next(10) > 1).then(|| format!("b{}", self.next(range)))
    }
}

/// One file's rows, column by column, as the build reads them and the service is sent them.
#[derive(Clone, Default)]
struct Rows {
    a: Vec<Option<u64>>,
    b: Option<Vec<Option<String>>>,
    xy: Option<Vec<(f64, f64)>>,
    score: Option<Vec<i64>>,
    tessera_id: Option<Vec<Option<String>>>,
    /// `rid`, a unique field only the points carry, one value per row, which no file edits: the
    /// access relation names items by it.
    rid: Option<Vec<u64>>,
    key: Option<Vec<String>>,
    term: Option<Vec<u32>>,
}

impl Rows {
    fn len(&self) -> usize {
        self.a.len().max(self.rid.as_ref().map_or(0, Vec::len))
    }

    /// The file, or with `empty` the file's schema and no rows.
    fn write(&self, path: &Path, empty: bool) {
        let n = if empty { 0 } else { self.len() };
        let mut columns: Vec<(&str, ArrayRef)> = Vec::new();
        if let Some(key) = &self.key {
            columns.push(("key", Arc::new(StringArray::from_iter_values(key[..n].iter()))));
        }
        if !self.a.is_empty() {
            columns.push(("a", Arc::new(UInt64Array::from(self.a[..n].to_vec()))));
        }
        if let Some(rid) = &self.rid {
            columns.push(("rid", Arc::new(UInt64Array::from(rid[..n].to_vec()))));
        }
        if let Some(b) = &self.b {
            columns.push(("b", Arc::new(StringArray::from(b[..n].to_vec()))));
        }
        if let Some(xy) = &self.xy {
            columns.push(("x", Arc::new(Float64Array::from_iter_values(xy[..n].iter().map(|p| p.0)))));
            columns.push(("y", Arc::new(Float64Array::from_iter_values(xy[..n].iter().map(|p| p.1)))));
        }
        if let Some(score) = &self.score {
            columns.push(("score", Arc::new(Int64Array::from(score[..n].to_vec()))));
        }
        if let Some(tessera_id) = &self.tessera_id {
            columns.push(("tessera_id", Arc::new(StringArray::from(tessera_id[..n].to_vec()))));
        }
        if let Some(term) = &self.term {
            columns.push(("term_id", Arc::new(UInt32Array::from(term[..n].to_vec()))));
        }
        let fields: Vec<Field> = columns
            .iter()
            .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
            .collect();
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns.into_iter().map(|c| c.1).collect())
            .unwrap();
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    /// The rows as an ingest batch's JSON, a null cell left out so that it keeps what the item
    /// holds, as a null cell does at a build.
    fn ingest_body(&self) -> Value {
        let rows: Vec<Value> = (0..self.len())
            .map(|i| {
                let mut row = serde_json::Map::new();
                if let Some(a) = self.a[i] {
                    row.insert("a".into(), json!(a));
                }
                if let Some(rid) = &self.rid {
                    row.insert("rid".into(), json!(rid[i]));
                }
                if let Some(Some(b)) = self.b.as_ref().map(|b| &b[i]) {
                    row.insert("b".into(), json!(b));
                }
                if let Some(xy) = &self.xy {
                    row.insert("x".into(), json!(xy[i].0));
                    row.insert("y".into(), json!(xy[i].1));
                }
                if let Some(score) = &self.score {
                    row.insert("score".into(), json!(score[i]));
                }
                if let Some(Some(id)) = self.tessera_id.as_ref().map(|t| &t[i]) {
                    row.insert("tessera_id".into(), json!(id));
                }
                Value::Object(row)
            })
            .collect();
        Value::Array(rows)
    }

    /// The members as one publication: an artifact per key, in the order keys first appear, its
    /// members a table of the file's unique columns.
    fn publish_body(&self) -> Value {
        let keys = self.key.as_ref().expect("a members file carries keys");
        let mut order: Vec<&String> = Vec::new();
        for key in keys {
            if !order.contains(&key) {
                order.push(key);
            }
        }
        let artifacts: Vec<Value> = order
            .into_iter()
            .map(|key| {
                let rows: Vec<usize> = (0..self.len()).filter(|i| &keys[*i] == key).collect();
                let mut table = json!({ "a": rows.iter().map(|i| self.a[*i]).collect::<Vec<_>>() });
                if let Some(b) = &self.b {
                    table["b"] = json!(rows.iter().map(|i| b[*i].clone()).collect::<Vec<_>>());
                }
                json!({ "key": key, "members": table })
            })
            .collect();
        json!({ "artifacts": artifacts })
    }

    /// The access relation as one batch naming each item once, in the order its identifier first
    /// appears, with every label the relation gives it; and each batch row's file rows.
    fn relation_body(&self) -> (Value, Vec<Vec<usize>>) {
        let term = self.term.as_ref().expect("a relation carries terms");
        let rid = self.rid.as_ref().expect("a relation names items by rid");
        let mut order: Vec<u64> = Vec::new();
        let mut rows_of: Vec<Vec<usize>> = Vec::new();
        for (i, a) in rid.iter().copied().enumerate() {
            match order.iter().position(|held| *held == a) {
                Some(at) => rows_of[at].push(i),
                None => {
                    order.push(a);
                    rows_of.push(vec![i]);
                }
            }
        }
        let body = order
            .iter()
            .zip(&rows_of)
            .map(|(a, rows)| {
                let mut labels: Vec<String> = rows.iter().map(|i| term[*i].to_string()).collect();
                labels.dedup();
                json!({ "rid": a, "access": labels })
            })
            .collect();
        (Value::Array(body), rows_of)
    }

    /// A row as the build's report names it: each identifying value it carries, `field = value`,
    /// or its position where it carries none.
    fn value_text(&self, row: usize) -> String {
        let mut parts = Vec::new();
        if let Some(Some(a)) = self.a.get(row) {
            parts.push(format!("a = {a}"));
        }
        if let Some(Some(b)) = self.b.as_ref().map(|b| &b[row]) {
            parts.push(format!("b = {b}"));
        }
        if let Some(rid) = &self.rid {
            parts.push(format!("rid = {}", rid[row]));
        }
        if let Some(score) = &self.score {
            parts.push(format!("score = {}", score[row]));
        }
        if let Some(Some(id)) = self.tessera_id.as_ref().map(|t| &t[row]) {
            parts.push(format!("tessera_id = {id}"));
        }
        match parts.is_empty() {
            true => format!("row {row}"),
            false => parts.join(", "),
        }
    }

    /// Each member row's position in [`Self::publish_body`]: `(artifact, row)`.
    fn member_positions(&self) -> Vec<(usize, usize)> {
        let keys = self.key.as_ref().unwrap();
        let mut order: Vec<&String> = Vec::new();
        let mut seen: BTreeMap<&String, usize> = BTreeMap::new();
        keys.iter()
            .map(|key| {
                if !order.contains(&key) {
                    order.push(key);
                }
                let artifact = order.iter().position(|k| *k == key).unwrap();
                let row = seen.entry(key).or_insert(0);
                *row += 1;
                (artifact, *row - 1)
            })
            .collect()
    }
}

/// A seeded corpus: two views' points, an attribute file and a members file. The attribute
/// file's own column is declared unique, so it owes no item a row: a build refuses a corpus in
/// which an item has no row in a source of a column that is not.
struct Corpus {
    two_fields: bool,
    world: Rows,
    near: Rows,
    notes: Rows,
    relation: Rows,
    members: Rows,
}

impl Corpus {
    fn draw(seed: u64) -> Corpus {
        let mut d = Draws(seed);
        let two_fields = seed.is_multiple_of(2);
        let rows = |d: &mut Draws, n: usize, range: u64, points: bool| {
            let at = |i: usize, salt: u64| ((i as u64 * 37 + salt) % 97) as f64 + 0.5;
            Rows {
                a: (0..n).map(|_| d.a(range)).collect(),
                b: two_fields.then(|| (0..n).map(|_| d.b(range)).collect()),
                xy: points.then(|| (0..n).map(|i| (at(i, seed), at(i, seed + 11))).collect()),
                ..Rows::default()
            }
        };
        let mut world = rows(&mut d, 40, 40, true);
        world.rid = Some((1000..1040).collect());
        let mut near = rows(&mut d, 20, 50, true);
        near.rid = Some((2000..2020).collect());
        let mut notes = rows(&mut d, 25, 50, false);
        notes.score = Some((0..25).map(|i| 100 + i).collect());
        notes.tessera_id =
            Some((0..25).map(|i| (i % 6 == 5).then(|| format!("9999999999{i}"))).collect());
        // A refused row refuses no later row: the first row gives an item held in the world a new
        // `b`, the second gives another held item that value and is refused, and the third names
        // that other item alone and is kept.
        if let (Some(b), true) = (notes.b.as_mut(), two_fields) {
            let mut held = world.a.iter().flatten().copied().collect::<Vec<_>>();
            held.dedup();
            held.sort_unstable();
            held.dedup();
            let (x, y) = (held[0], held[1]);
            notes.a[0..3].copy_from_slice(&[Some(x), Some(y), Some(y)]);
            b[0..3].clone_from_slice(&[Some("bz".to_string()), Some("bz".to_string()), None]);
            notes.tessera_id.as_mut().unwrap()[0..3].fill(None);
        }
        // Every point row's `rid` gets a term: every fifth the narrower one alone, the rest the
        // one everyone holds. A build gives an item the relation names nothing no label at all,
        // where the service gives it the view's default, so every item is named. More rows give
        // some the narrower term too, and a few name nothing.
        let mut rid: Vec<u64> = (1000..1040).chain(2000..2020).collect();
        let mut term: Vec<u32> = (0..rid.len())
            .map(|i| if i % 5 == 0 { TERM } else { EVERYONE })
            .collect();
        for _ in 0..12 {
            rid.push([1000, 2000, 3000][d.next(3) as usize] + d.next(40));
            term.push(TERM);
        }
        let relation = Rows {
            rid: Some(rid),
            term: Some(term),
            ..Rows::default()
        };
        let mut members = rows(&mut d, 40, 50, false);
        members.key = Some((0..40).map(|_| format!("k{}", d.next(4))).collect());
        Corpus {
            two_fields,
            world,
            near,
            notes,
            relation,
            members,
        }
    }

    /// The files in `dir`, whole or with no rows, and the declaration.
    fn write(&self, dir: &Path, empty: bool) -> String {
        std::fs::create_dir_all(dir).unwrap();
        self.world.write(&dir.join("world.parquet"), empty);
        self.near.write(&dir.join("near.parquet"), empty);
        self.notes.write(&dir.join("notes.parquet"), empty);
        self.relation.write(&dir.join("access.parquet"), empty);
        self.members.write(&dir.join("members.parquet"), empty);
        let b = if self.two_fields {
            "\n[[attribute]]\nname   = \"b\"\ntype   = \"keyword\"\nunique = true\n"
        } else {
            ""
        };
        format!(
            r#"
[sources]
world   = "world.parquet"
near    = "near.parquet"
notes   = "notes.parquet"
access  = "access.parquet"
members = "members.parquet"

[defaults]
source          = "world"
allocation_view = "world"

[[view]]
name             = "world"
extent           = {{ min = 0.0, max = 100.0 }}
point_visibility = {{ source = "access", default = "public" }}

[[view]]
name             = "near"
source           = "near"
extent           = {{ min = 0.0, max = 100.0 }}
point_visibility = {{ source = "access", default = "public" }}

[[attribute]]
name   = "a"
type   = "u64"
unique = true
{b}
[[attribute]]
name   = "rid"
type   = "u64"
unique = true

[[attribute]]
name   = "score"
type   = "i64"
unique = true
source = "notes"

[[layer]]
name                      = "{LAYER}"
views                     = ["world", "near"]
membership                = "enumerated"
value_set                 = "open"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "any"

  [layer.members]
  source = "members"
"#
        )
    }
}

/// Every build argument the declaration implies, each view's frame 0..100.
fn args(dir: &Path, declaration: &str, out: &Path) -> BuildArgs {
    let path = dir.join("corpus.toml");
    std::fs::write(&path, declaration).unwrap();
    let config = Config::parse(&path, &Default::default()).expect("the declaration parses");
    let registry = config.build_views().expect("the views compile");
    let anchor = config.anchor_view(&registry).expect("an anchor");
    let acquired = config.acquire().expect("the files acquire");
    let views = registry
        .iter()
        .map(|view| {
            let acquired = tessera_build::config::acquire_view(view).expect("the view acquires");
            tessera_build::ViewArgs {
                visibility: None,
                view_id: view.id.clone(),
                projection: view.projection,
                extent: Bounds {
                    x_min: 0.0,
                    x_max: 100.0,
                    y_min: 0.0,
                    y_max: 100.0,
                },
                points: acquired.points,
                point_fields: acquired.point_fields,
                select: acquired.select,
                access: acquired.access,
            }
        })
        .collect();
    let mut layers = config.layers.clone();
    for layer in &mut layers {
        layer.views = Config::expand_layer_views(&registry, &layer.views);
    }
    BuildArgs {
        views,
        anchor,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: acquired.attribute_sources,
        out: out.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers,
        layer_inputs: acquired.layers,
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    }
}

/// The rows the build refused from the block `object` reads, by reason.
fn reported(report: &BuildReport, object: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for entry in report.refused.iter().filter(|e| e.object == object && e.is_refusal()) {
        *out.entry(entry.reason.clone()).or_default() += entry.rows;
    }
    out
}

/// The file rows an answer refused, by reason: `row_of` turns an answer's entry into the file
/// rows it stands for.
fn listed(answer: &Value, row_of: impl Fn(&Value) -> Vec<usize>) -> BTreeMap<String, Vec<usize>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for entry in answer["refused"].as_array().expect("the answer lists refused rows") {
        let reason = entry["reason"].as_str().unwrap().to_string();
        out.entry(reason).or_default().extend(row_of(entry));
    }
    for rows in out.values_mut() {
        rows.sort_unstable();
    }
    out
}

/// Hold the rows the service refused of one file to the build's report on it: the same count for
/// each reason, and the report's sample, the values of the first ten rows each once, as those rows
/// of the file read.
fn assert_refused_alike(
    report: &BuildReport,
    object: &str,
    listed: &BTreeMap<String, Vec<usize>>,
    rows: &Rows,
    what: &str,
) {
    let reported = reported(report, object);
    let counts: BTreeMap<String, u64> =
        listed.iter().map(|(reason, rows)| (reason.clone(), rows.len() as u64)).collect();
    assert_eq!(counts, reported, "{what}: refused rows by reason");
    for entry in report.refused.iter().filter(|e| e.object == object && e.is_refusal()) {
        let mut sample: Vec<String> = Vec::new();
        for row in listed[&entry.reason].iter().take(10) {
            let text = rows.value_text(*row);
            if !sample.contains(&text) {
                sample.push(text);
            }
        }
        assert_eq!(entry.values, sample, "{what}: the first {} rows", entry.reason);
    }
}

/// Send one file's rows to `path`, first strict, which must be refused exactly where the build
/// refused rows of it, then, where it was, as the rows alone the rule accepts. Answers the
/// accepted answer.
async fn send(
    server: &TestServer,
    method: reqwest::Method,
    path: &str,
    view: Option<&str>,
    body: &Value,
    refuses: bool,
    batch: &str,
) -> Value {
    let request = |strict: bool| {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = match strict {
            true => server.control_url(&format!("{path}{sep}strict=true")),
            false => server.control_url(path),
        };
        let mut request = server
            .client
            .request(method.clone(), url)
            .bearer_auth(OPERATOR_CREDENTIAL)
            .header("x-tessera-batch-id", format!("{batch}-{strict}"))
            .json(body);
        if let Some(view) = view {
            request = request.header("x-tessera-view", view);
        }
        request
    };
    let resp = request(true).send().await.unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    if !refuses {
        assert!(status < 300, "{batch}: the build refused no row of it: {status} {answer}");
        return answer;
    }
    assert!(
        matches!(status, 404 | 409),
        "{batch}: strict refuses a file the build refused rows of: {status} {answer}"
    );
    let resp = request(false).send().await.unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(status < 300, "{batch}: {status} {answer}");
    answer
}

/// What one principal sees of one view: each item's values and position, in a sorted list.
type Seen = Vec<(Option<u64>, Option<String>, Option<i64>, i64, i64)>;

async fn seen(
    server: &TestServer,
    token: &str,
    view: &str,
    two_fields: bool,
    filters: Option<Value>,
) -> Seen {
    let fields = match two_fields {
        true => json!(["a", "b", "score"]),
        false => json!(["a", "score"]),
    };
    let mut body = json!({
        "view": view,
        "fields": fields,
        "system_fields": ["position"],
        "order": "map",
    });
    if let Some(filters) = filters {
        body["filters"] = filters;
    }
    let mut out = Vec::new();
    loop {
        let resp = server
            .client
            .post(server.viewer_url("/v1/items"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let decoded = decode_records(&bytes);
        for (batch, _) in &decoded.pages {
            let a = batch.column_by_name("a").unwrap();
            let a = arrow::compute::cast(a, &arrow::datatypes::DataType::UInt64).unwrap();
            let a = a.as_any().downcast_ref::<UInt64Array>().unwrap();
            let b = batch
                .column_by_name("b")
                .map(|b| arrow::compute::cast(b, &arrow::datatypes::DataType::Utf8).unwrap());
            let score = batch.column_by_name("score").unwrap();
            let score = arrow::compute::cast(score, &arrow::datatypes::DataType::Int64).unwrap();
            let score = score.as_any().downcast_ref::<Int64Array>().unwrap();
            let x = batch.column_by_name("tessera:x").unwrap();
            let x = x.as_any().downcast_ref::<Float64Array>().unwrap();
            let y = batch.column_by_name("tessera:y").unwrap();
            let y = y.as_any().downcast_ref::<Float64Array>().unwrap();
            for i in 0..batch.num_rows() {
                let b = b.as_ref().and_then(|b| {
                    let b = b.as_any().downcast_ref::<StringArray>().unwrap();
                    (!b.is_null(i)).then(|| b.value(i).to_string())
                });
                out.push((
                    (!a.is_null(i)).then(|| a.value(i)),
                    b,
                    (!score.is_null(i)).then(|| score.value(i)),
                    (x.value(i) * 1e6).round() as i64,
                    (y.value(i) * 1e6).round() as i64,
                ));
            }
        }
        match decoded.trailer["next"].clone() {
            Value::Null => break,
            cursor => body["cursor"] = cursor,
        }
    }
    out.sort();
    out
}

/// Everything the two paths are compared on, for one server.
async fn state(server: &TestServer, two_fields: bool) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for terms in PRINCIPALS {
        let token = token_for(server, terms).await;
        let who = terms.join("+");
        for view in ["world", "near"] {
            let items = seen(server, &token, view, two_fields, None).await;
            out.insert(format!("{who} {view} items"), json!(items));

            // Each artifact this principal is served in this view, by key: its masked count and
            // the members it holds, by value.
            let resp = server
                .client
                .post(server.viewer_url("/v1/viewport"))
                .bearer_auth(&token)
                .json(&json!({
                    "view": view, "zoom": 0, "bbox": [0.0, 0.0, 100.0, 100.0], "k": 1000,
                    "layers": [LAYER],
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 200);
            let artifacts = decode_viewport_frames(&resp.bytes().await.unwrap())
                .artifacts
                .unwrap_or_default();
            let mut held = BTreeMap::new();
            for artifact in artifacts {
                let filter = json!({ "member_of": {
                    "layer": LAYER, "artifact": artifact.tessera_id.to_string()
                } });
                let members = seen(server, &token, view, two_fields, Some(filter)).await;
                held.insert(
                    artifact.key.clone().unwrap_or_default(),
                    json!([artifact.masked_count, members]),
                );
            }
            out.insert(format!("{who} {view} artifacts"), json!(held));
        }
    }
    out
}

/// Compare the two paths over the corpus `seed` draws; answers the reasons the build refused rows
/// for.
async fn build_equals_ingest(seed: u64) -> Vec<String> {
    let corpus = Corpus::draw(seed);
    let tmp = TempDir::new().unwrap();

    // Path A: the build.
    let full = tmp.path().join("full");
    let declaration = corpus.write(&full, false);
    let built_root = full.join("bundle");
    let report = build(&args(&full, &declaration, &built_root)).expect("the build runs");
    assert!(
        report.refused.iter().any(|entry| entry.is_refusal()),
        "seed {seed} plants refusals"
    );

    // Path B: the same declaration built empty, and each file sent in declaration order.
    let empty = tmp.path().join("empty");
    corpus.write(&empty, true);
    let empty_root = empty.join("bundle");
    build(&args(&empty, &declaration, &empty_root)).expect("the empty build runs");
    let ingested = spawn_server(&empty_root, &empty.join("cache"), &empty.join("wal.log")).await;

    let by_row = |entry: &Value| vec![entry["row"].as_u64().unwrap() as usize];
    for (source, object, rows, view) in [
        ("world", "view 'world'", &corpus.world, Some("world")),
        ("near", "view 'near'", &corpus.near, Some("near")),
        ("notes", "attribute source 'notes'", &corpus.notes, None),
    ] {
        let refuses = !reported(&report, object).is_empty();
        let batch = format!("{seed}-{source}");
        let body = rows.ingest_body();
        let answer =
            send(&ingested, reqwest::Method::POST, "/control/ingest", view, &body, refuses, &batch)
                .await;
        let what = format!("seed {seed}: {source}");
        assert_refused_alike(&report, object, &listed(&answer, by_row), rows, &what);
        tick(&ingested).await;
    }

    // The access relation, one row per item naming it with its labels.
    let object = "point_visibility";
    let (body, rows_of) = corpus.relation.relation_body();
    let refuses = !reported(&report, object).is_empty();
    let batch = format!("{seed}-access");
    let answer =
        send(&ingested, reqwest::Method::POST, "/control/ingest", None, &body, refuses, &batch).await;
    let of_item = |entry: &Value| rows_of[entry["row"].as_u64().unwrap() as usize].clone();
    let what = format!("seed {seed}: the access relation");
    assert_refused_alike(&report, object, &listed(&answer, of_item), &corpus.relation, &what);
    tick(&ingested).await;

    let object = format!("layer '{LAYER}' members");
    let refuses = !reported(&report, &object).is_empty();
    let answer = send(
        &ingested,
        reqwest::Method::PUT,
        &format!("/control/layers/{LAYER}/artifacts"),
        None,
        &corpus.members.publish_body(),
        refuses,
        &format!("{seed}-members"),
    )
    .await;
    let positions = corpus.members.member_positions();
    let of_member = |entry: &Value| {
        let at = (
            entry["artifact"].as_u64().unwrap() as usize,
            entry["row"].as_u64().unwrap() as usize,
        );
        vec![positions.iter().position(|p| *p == at).expect("a member row")]
    };
    let what = format!("seed {seed}: the members");
    assert_refused_alike(&report, &object, &listed(&answer, of_member), &corpus.members, &what);
    tick(&ingested).await;

    // Compared as a restart replays it.
    ingested.shutdown().await;
    let ingested = spawn_server(&empty_root, &empty.join("cache-2"), &empty.join("wal.log")).await;
    let built = spawn_server(&built_root, &full.join("cache"), &full.join("wal.log")).await;
    let left = state(&built, corpus.two_fields).await;
    let right = state(&ingested, corpus.two_fields).await;
    for (what, value) in &left {
        assert_eq!(Some(value), right.get(what), "seed {seed}: {what}");
    }
    assert_eq!(left.len(), right.len());
    for view in ["world", "near"] {
        let items = left[&format!("0+7 {view} items")].as_array().unwrap().len();
        let everyone = left[&format!("0 {view} items")].as_array().unwrap().len();
        assert!(everyone > 0, "seed {seed}: {view} holds items everyone sees");
        assert!(items > everyone, "seed {seed}: {view} holds items only the narrower term shows");
        let artifacts = left[&format!("0+7 {view} artifacts")].as_object().unwrap().len();
        assert!(artifacts > 0, "seed {seed}: {view} serves artifacts");
    }
    built.shutdown().await;
    ingested.shutdown().await;
    report.refused.iter().map(|entry| entry.reason.clone()).collect()
}

#[tokio::test]
async fn a_build_answers_as_its_files_ingested_into_an_empty_database() {
    let mut reasons = std::collections::BTreeSet::new();
    for seed in 1..=6u64 {
        eprintln!("seed {seed}");
        reasons.extend(build_equals_ingest(seed).await);
    }
    for reason in [
        "names_two_items",
        "names_no_item",
        "one_item_twice",
        "one_value_twice",
        "unknown_tessera_id",
    ] {
        assert!(reasons.contains(reason), "the corpora plant {reason}: {reasons:?}");
    }
}
