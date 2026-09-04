//! **`GET /v1/categories/{column}`: what a code stands for, and who may be told.**
//!
//! The hot path ships codes, so a client holding a viewport response has integers. These cases
//! cover the route that turns them into keys, and the three rules that make it safe to expose:
//!
//! - **One gate, both request forms.** Bulk lookup and enumeration run the same `visibility` check.
//!   A gate reached by one door and not the other is an existence oracle by another route, and
//!   the two forms are separate code paths, so nothing but a test keeps them agreeing.
//! - **An unresolvable code is omitted, never refused.** "No such code" and "a value you cannot
//!   see" must be one outcome (per-point-attributes §3.8).
//! - **A `derived` column is filtered per principal**, by §3.3's membership predicate: a value is
//!   offered iff the principal can see an item carrying it. Serving the set unfiltered is the C11
//!   disclosure; serving it empty for a principal who *does* have members is the availability
//!   failure on the other side, and only a fixture whose values cut across the term model tells the
//!   two apart.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use common::*;
use tessera_build::{build, BuildArgs};

const N: u64 = 64;

/// One `public` category and one `derived` one, so a single fixture exercises both sides of the
/// gate. `archive`'s five values exceed the test server's page size of 4, which is what puts the
/// cursor on the ordinary path rather than only on a contrived one.
///
/// `department`'s ten values interleave in key order: the odd-numbered ones are carried only by
/// items every principal can see, the even-numbered ones only by items the narrow principal cannot
/// (`terms_of` grants term `1` on `e % 3 == 0`). So the narrow principal is offered five values with
/// four invisible ones interleaved between them, which is what forces the walk to filter *before* it
/// cuts a page: taking the page first and filtering it after would return two values and stop.
/// `d00` is declared and carried by nothing at all — the empty-value case, offered to nobody.
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
  math = 44
  quant = 55

[[vocabulary]]
name       = "department"
width      = "u8"
value_set  = "closed"
visibility = "derived"
  [vocabulary.values]
  d00 = 100
  d01 = 101
  d02 = 102
  d03 = 103
  d04 = 104
  d05 = 105
  d06 = 106
  d07 = 107
  d08 = 108
  d09 = 109
  d10 = 110

[[attribute]]
name       = "archive"
type       = "category"
render     = true
vocabulary = "archive"

[[attribute]]
name       = "department"
type       = "category"
render     = true
vocabulary = "department"

[[attribute]]
name     = "score"
type     = "f32"
render = true

# **Neither `render` nor `index`** — the default placement, and the one a category may take
# without losing its value list. It is blob-resident (records §3): no hot column, no entity-space
# structure, no `filter_operands` entry — and `/v1/meta` still publishes its `category` block and
# drill-down still returns its code, so the code still needs a key. Every other category here is
# rendered, which is why this column exists: it is the only one whose value list is asked for over
# a column the *filter* surface does not carry.
[[vocabulary]]
name       = "origin"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  born = 7
  found = 8
  given = 9

[[attribute]]
name       = "origin"
type       = "category"
vocabulary = "origin"
"#;

/// The rendered number, **with absences** — every seventh item carries none.
///
/// The absences are the point. A render column is non-nullable, so an absent number is written as
/// the type's zero (decision 0064), and this fixture's filter below is `score < 16`, a range that
/// contains zero. If the presence bitmap beside the column were not honoured, every absent item
/// would match it — the 2026-08-11 defect, end to end, through a real build and a real request.
fn score_of(entity: u64) -> Option<f32> {
    if entity.is_multiple_of(7) {
        return None;
    }
    Some((entity % 97) as f32 * 0.5)
}

/// Five archives, so several codes are live and no code is the only one present.
fn archive_of(entity: u64) -> &'static str {
    ["astro", "cond", "hep", "math", "quant"][(entity % 5) as usize]
}

/// The blob-resident category's three values, so none of them is the only one present.
fn origin_of(entity: u64) -> &'static str {
    ["born", "found", "given"][(entity % 3) as usize]
}

/// Odd-numbered departments on the items the narrow principal can see, even-numbered ones on the
/// rest — see [`SCHEMA_TOML`]. `d00` is carried by nothing.
fn department_of(entity: u64) -> String {
    if entity.is_multiple_of(3) {
        format!("d{:02}", 1 + 2 * ((entity / 3) % 5))
    } else {
        format!("d{:02}", 2 + 2 * (entity % 5))
    }
}

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("archive", DataType::Utf8, false),
        Field::new("department", DataType::Utf8, false),
        Field::new("origin", DataType::Utf8, false),
        Field::new("score", DataType::Float32, true),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let archives: Vec<&str> = ids.iter().map(|&e| archive_of(e)).collect();
    let departments: Vec<String> = ids.iter().map(|&e| department_of(e)).collect();
    let origins: Vec<&str> = ids.iter().map(|&e| origin_of(e)).collect();
    let scores: Vec<Option<f32>> = ids.iter().map(|&e| score_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives)),
            Arc::new(StringArray::from(departments)),
            Arc::new(StringArray::from(origins)),
            Arc::new(arrow::array::Float32Array::from(scores)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_fixture_with_categories(out: &Path, points: &Path, pairs: &Path) {
    write_points(points, N);
    write_pairs_n(pairs, N);
    let schema_path = points.with_file_name("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .unwrap()
        .schema;
    let args = BuildArgs {
        arena_order: Default::default(),
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    build(&args).expect("fixture build should succeed");
}

/// A server over the categories fixture, plus a session token for a fully-granted principal.
async fn serve(tmp: &TempDir) -> (TestServer, String) {
    let bundle_root = tmp.path().join("bundle");
    build_fixture_with_categories(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    (server, token)
}

/// Every key a column offers this principal, paged to exhaustion through the server's own cursor.
async fn page_all(server: &TestServer, token: &str, path: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let url = match &after {
            Some(cursor) => format!("{path}?after={cursor}"),
            None => path.to_string(),
        };
        let (status, body) = get(server, token, &url).await;
        assert_eq!(status, 200, "{url}: {body}");
        keys.extend(
            body["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["key"].as_str().unwrap().to_string()),
        );
        match body["next"].as_str() {
            Some(cursor) => after = Some(cursor.to_string()),
            None => break,
        }
    }
    keys
}

async fn get(server: &TestServer, token: &str, path: &str) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .get(server.viewer_url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// `/v1/meta` must say which columns are categories, or a client cannot tell a `u8` category from
/// a `u8` integer — the hot path ships the code and nothing else.
#[tokio::test]
async fn meta_publishes_the_category_descriptor_and_no_values() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/meta").await;
    assert_eq!(status, 200);
    let scalars = body["declared_scalars"].as_array().unwrap();

    let archive = scalars.iter().find(|s| s["name"] == "archive").unwrap();
    assert_eq!(archive["arrow_type"], "u8");
    assert_eq!(archive["category"]["vocabulary"], "archive");
    assert_eq!(archive["category"]["kind"], "declared");
    assert_eq!(archive["category"]["visibility"], "public");

    // A plain column carries no descriptor at all: its *absence* is the signal.
    let score = scalars.iter().find(|s| s["name"] == "score").unwrap();
    assert!(
        score["category"].is_null(),
        "a plain scalar must carry no category descriptor: {score}"
    );

    // Values live behind `/v1/categories`, so that a large vocabulary cannot dominate the one
    // document every client fetches at startup.
    let raw = body.to_string();
    assert!(
        !raw.contains("astro"),
        "/v1/meta must not carry vocabulary values: {raw}"
    );
}

/// **`/v1/meta` publishes what a client may filter on, and a rendered number is on that list** with
/// its family's full operator set — decision 0064's render half, which put a number's absence in a
/// bitmap beside the hot column and so made the row route safe for one.
///
/// The fixture's `score` is `render`-only: it has no entity-space column at all, so if it appears
/// here it can only be answered over the request's own rows. A client cannot tell which route
/// served it, which is 0068's whole licence to have two — so the operand list says `numeric` and
/// nothing about placement.
#[tokio::test]
async fn meta_publishes_a_rendered_number_as_a_filterable_numeric_operand() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/meta").await;
    assert_eq!(status, 200);
    let operands = body["filter_operands"].as_array().unwrap();
    let of = |column: &str| {
        operands
            .iter()
            .find(|o| o["column"] == column)
            .unwrap_or_else(|| panic!("{column} must be filterable: {body}"))
            .clone()
    };

    let score = of("score");
    assert_eq!(score["family"], "numeric");
    assert_eq!(
        score["operands"].as_array().unwrap(),
        &vec!["eq", "in", "range"],
        "a rendered number takes its family's whole operator set, `range` included"
    );

    // The rendered categories are still published as they were, by the same predicate.
    assert_eq!(of("archive")["family"], "category");
    assert_eq!(of("department")["family"], "category");
}

/// **A client can actually filter on the rendered number `/v1/meta` offers it**, end to end: the
/// request parses, the row route answers it, and the counts narrow.
///
/// The published list and the parse gate are the same predicate (`filter::is_filterable`), so an
/// operand advertised and then refused would be a contradiction inside one function — which is
/// exactly why the pair is asserted from the outside rather than trusted.
#[tokio::test]
async fn a_viewport_filters_on_a_rendered_number_over_its_own_rows() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let viewport = |filter: Option<serde_json::Value>| {
        let mut body = serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1000
        });
        if let Some(filter) = filter {
            body["filters"] = filter;
        }
        server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(&token)
            .json(&body)
            .send()
    };

    // `score` is `(e % 97) * 0.5` over 64 items with every seventh absent, so this is the lower
    // half of the corpus minus those — a real narrowing, a fractional bound (the endpoint form an
    // integer column would have had to round and a float column must not), and a range that
    // **contains zero**, which is what makes the absences load-bearing rather than decorative.
    let resp = viewport(Some(serde_json::json!({"score": {"range": {"lt": 16.0}}})))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a rendered number must be filterable");
    let (tiles, points) = decode_viewport(&resp.bytes().await.unwrap());
    let matched: u64 = tiles.iter().map(|t| t.2).sum();
    let visible: u64 = tiles.iter().map(|t| t.1).sum();

    let expected = (0..N)
        .filter(|&e| score_of(e).is_some_and(|v| v < 16.0))
        .count() as u64;
    assert_eq!(
        matched, expected,
        "the filtered count disagrees with the corpus"
    );
    assert!(matched < visible, "the filter narrowed nothing");
    // The absence half, stated as its own assertion because the count above would also pass if
    // the corpus happened to have none: an item with no score is stored as 0.0 in the hot column,
    // and 0.0 is inside this range. Only the presence bitmap keeps it out.
    let absent = (0..N).filter(|&e| score_of(e).is_none()).count() as u64;
    assert!(
        absent > 0,
        "the fixture must plant absences for this to mean anything"
    );
    assert_eq!(
        matched, expected,
        "an item with no score matched a range containing zero — decision 0064's bitmap is not \
         being honoured on the row route"
    );
    assert_eq!(points.len() as u64, matched, "every matching item is drawn");

    // The unfiltered request over the same window: `visible` is the composed mask's own count and
    // must not have moved (§7.1, I12).
    let resp = viewport(None).await.unwrap();
    let (all_tiles, _) = decode_viewport(&resp.bytes().await.unwrap());
    assert_eq!(
        all_tiles.iter().map(|t| t.1).sum::<u64>(),
        visible,
        "the filter moved `visible`, which is computed above it"
    );
}

/// The viewer's normal path: it knows which codes it drew, so it resolves exactly those.
#[tokio::test]
async fn bulk_lookup_returns_the_named_codes_and_omits_the_rest() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/categories/archive?codes=11,44").await;
    assert_eq!(status, 200);
    assert_eq!(body["column"], "archive");
    let values = body["values"].as_array().unwrap();
    let keys: Vec<&str> = values.iter().map(|v| v["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["astro", "math"], "{body}");
    assert!(
        body["next"].is_null(),
        "bulk lookup is bounded by the request, so it never pages: {body}"
    );
}

/// **The existence-oracle rule** (§3.8). A code that is unbound, that is the *absent* sentinel, or
/// that a principal may not see must all be one outcome: omitted from `values`, with a 200. A 404
/// or a 422 for any of them would let a caller enumerate the vocabulary by probing.
#[tokio::test]
async fn an_unresolvable_code_is_omitted_rather_than_refused() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    // 0 is the absent sentinel, 99 is bound to nothing, 11 is real.
    let (status, body) = get(&server, &token, "/v1/categories/archive?codes=0,99,11").await;
    assert_eq!(status, 200, "an unknown code is not a refusal: {body}");
    let values = body["values"].as_array().unwrap();
    assert_eq!(values.len(), 1, "{body}");
    assert_eq!(values[0]["key"], "astro");

    // Asking for nothing but unknown codes is an empty answer, still a 200 — indistinguishable
    // from a principal who may see none of them, which is the point.
    let (status, body) = get(&server, &token, "/v1/categories/archive?codes=99").await;
    assert_eq!(status, 200);
    assert!(body["values"].as_array().unwrap().is_empty(), "{body}");
}

/// Enumeration pages in **key** order, and the cursor resumes exactly where the page stopped.
/// The test server's page size is 4 against five values, so this is the ordinary path.
#[tokio::test]
async fn enumeration_pages_in_key_order_and_the_cursor_resumes() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, first) = get(&server, &token, "/v1/categories/archive").await;
    assert_eq!(status, 200);
    let keys: Vec<&str> = first["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["astro", "cond", "hep", "math"], "{first}");
    let cursor = first["next"].as_str().expect("a fifth value remains");
    assert_eq!(cursor, "math", "the cursor is the last key returned");

    let (status, second) = get(
        &server,
        &token,
        &format!("/v1/categories/archive?after={cursor}"),
    )
    .await;
    assert_eq!(status, 200);
    let keys: Vec<&str> = second["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["quant"], "{second}");
    assert!(
        second["next"].is_null(),
        "the set is complete, so there is no next cursor: {second}"
    );
}

/// **A `derived` value is offered iff the principal can see an item carrying it** (§3.3).
///
/// Two principals over one fixture: term `0` reaches every item, term `1` only `e % 3 == 0`. The
/// even-numbered departments are carried exclusively by items the narrow principal cannot see, so
/// they are values that certainly exist, that the wide principal is offered, and that this one must
/// not be. `d00` is carried by nothing and is offered to neither — the empty-value case.
#[tokio::test]
async fn a_derived_value_set_is_filtered_per_principal() {
    let tmp = TempDir::new().unwrap();
    let (server, wide) = serve(&tmp).await;
    let narrow = authorise(&server, &["1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        page_all(&server, &wide, "/v1/categories/department").await,
        vec!["d01", "d02", "d03", "d04", "d05", "d06", "d07", "d08", "d09", "d10"],
    );
    assert_eq!(
        page_all(&server, &narrow, "/v1/categories/department").await,
        vec!["d01", "d03", "d05", "d07", "d09"],
        "a value whose every member is outside this principal's mask must not be offered"
    );
}

/// **One gate, both request forms.** Bulk lookup and enumeration are separate walks in the engine,
/// so a gate applied to one is the existence oracle reached through the other. An invisible value's
/// code must come back exactly as an unbound code does: omitted, with a 200.
#[tokio::test]
async fn the_membership_gate_applies_to_both_request_forms() {
    let tmp = TempDir::new().unwrap();
    let (server, _wide) = serve(&tmp).await;
    let narrow = authorise(&server, &["1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    // 101 is visible to this principal, 102 exists but none of its members are, 100 is declared and
    // carried by nobody, 199 is bound to nothing at all.
    let (status, body) = get(
        &server,
        &narrow,
        "/v1/categories/department?codes=100,101,102,199",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let keys: Vec<&str> = body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["d01"], "{body}");
}

/// **The page is cut after the gate, never before.** The narrow principal's five visible values have
/// four invisible ones interleaved between them, so a walk that took a page and then filtered it
/// would return two values on the first page and stop — short pages whose length is itself a count
/// of what the principal cannot see.
#[tokio::test]
async fn a_derived_page_is_filled_with_visible_values() {
    let tmp = TempDir::new().unwrap();
    let (server, _wide) = serve(&tmp).await;
    let narrow = authorise(&server, &["1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, first) = get(&server, &narrow, "/v1/categories/department").await;
    assert_eq!(status, 200, "{first}");
    let keys: Vec<&str> = first["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec!["d01", "d03", "d05", "d07"],
        "the page must be filled to the server's ceiling of 4 with *visible* values: {first}"
    );
    assert_eq!(first["next"].as_str(), Some("d07"), "{first}");
}

/// **A principal who can see nothing is offered nothing — with a 200, not a refusal.** An empty
/// value set is a real answer here: it is exactly what this principal's derivation yields. The
/// refusal is reserved for a predicate that could not be *evaluated*, which is a different thing and
/// must stay distinguishable in the server's own behaviour.
#[tokio::test]
async fn a_principal_who_can_see_nothing_is_offered_an_empty_set() {
    let tmp = TempDir::new().unwrap();
    let (server, _wide) = serve(&tmp).await;
    let none = authorise(&server, &[]).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = get(&server, &none, "/v1/categories/department").await;
    assert_eq!(status, 200, "{body}");
    assert!(body["values"].as_array().unwrap().is_empty(), "{body}");
    assert!(body["next"].is_null(), "{body}");
    // And the `public` column is still served in full to that same principal, which is the whole
    // difference between the two visibilities.
    let (status, body) = get(&server, &none, "/v1/categories/archive").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["values"].as_array().unwrap().len(), 4, "{body}");
}

/// **A category has a value list whether or not it is an operand.** `origin` is declared with
/// neither `render` nor `index` — the default placement, blob-resident (records §3) — so it has no
/// hot column, no entity-space structure and no `filter_operands` entry, and `/v1/meta` still
/// publishes its `category` block and drill-down still returns its code.
///
/// Regression: `/v1/categories` resolves a column spelling through the same site the *filter* leaf
/// does, because a group-scoped family's gate must collapse identically on both surfaces
/// (`views.md` §5). Resolving the entity-scoped names through the filter's own *admission* as well
/// would 404 exactly this column — declared, published, and answered on every earlier revision.
/// Every other category in this fixture is `render = true`, which is why nothing here caught it.
#[tokio::test]
async fn a_category_that_is_not_an_operand_still_has_a_value_list() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/categories/origin").await;
    assert_eq!(status, 200, "{body}");
    let keys: Vec<&str> = body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["born", "found", "given"], "{body}");

    // The bulk form takes the same route and the same admission.
    let (status, body) = get(&server, &token, "/v1/categories/origin?codes=8").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["values"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["values"][0]["key"], "found", "{body}");

    // It is genuinely not an operand: `/v1/meta` publishes the column and its category block, and
    // does not offer it in `filter_operands`. If that ever changes this case stops testing the
    // placement it is named for.
    let (status, meta) = get(&server, &token, "/v1/meta").await;
    assert_eq!(status, 200, "{meta}");
    let declared = meta["declared_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "origin")
        .unwrap_or_else(|| panic!("origin is declared: {meta}"));
    assert_eq!(declared["category"]["vocabulary"], "origin", "{meta}");
    assert!(!declared["render"].as_bool().unwrap(), "{meta}");
    assert!(!declared["index"].as_bool().unwrap(), "{meta}");
    assert!(
        !meta["filter_operands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["column"] == "origin"),
        "a blob-resident column is not an operand: {meta}"
    );

    // A pin on it is a `422` and not a `404`: it is one value set for the corpus, so there is
    // nothing for `@` to choose between — the same answer a pin on any entity-scoped column gets.
    let (status, body) = get(&server, &token, "/v1/categories/origin@2026-Q3").await;
    assert_eq!(status, 422, "{body}");
}

/// 404 covers "no such column" and "a column that is not a category" identically. `/v1/meta` is
/// where a client learns which columns exist; this route must not become a second, finer answer.
#[tokio::test]
async fn a_plain_scalar_and_an_unknown_name_are_the_same_404() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (plain_status, plain) = get(&server, &token, "/v1/categories/score").await;
    let (missing_status, missing) = get(&server, &token, "/v1/categories/no_such_column").await;
    assert_eq!(plain_status, 404, "{plain}");
    assert_eq!(missing_status, 404, "{missing}");
    assert_eq!(
        plain["detail"], missing["detail"],
        "a plain column and an absent one must be indistinguishable: {plain} vs {missing}"
    );
}

/// Authenticated like every other route on this plane: a vocabulary is corpus shape, and an
/// unauthenticated route would hand it to anyone who can reach the listener.
#[tokio::test]
async fn the_route_requires_a_session_token() {
    let tmp = TempDir::new().unwrap();
    let (server, _token) = serve(&tmp).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/categories/archive"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// The page ceiling clamps rather than refuses — it bounds a response, not a disclosure — but a
/// zero-length page is refused, since a cursor that cannot advance is an infinite loop.
#[tokio::test]
async fn the_page_limit_clamps_and_zero_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/categories/archive?limit=10000").await;
    assert_eq!(status, 200);
    assert_eq!(
        body["values"].as_array().unwrap().len(),
        4,
        "a limit above the deployment's ceiling is clamped to it: {body}"
    );

    let (status, body) = get(&server, &token, "/v1/categories/archive?limit=0").await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract");
}
