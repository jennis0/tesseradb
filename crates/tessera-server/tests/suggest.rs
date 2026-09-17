//! **`GET /v1/categories/{column}/suggest`: the typeahead over a category vocabulary.**
//!
//! One gate with `/v1/categories` (`tests/categories.rs` covers that gate's own mechanics in
//! depth — a public and a derived column, an interleaved narrow principal), so these cases cover
//! what is new here: the wire shape (`match`, `count`, `more`), the two refusals the enumeration
//! has no need of (`q` over 256 bytes, an unknown query parameter), the walk budget, and the
//! per-session admission (`value-suggestion.md` §5.1).

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

/// One `public` category (`archive`, also `index = true` so counts can be asked of it) and one
/// `derived` one (`department`), on `tests/categories.rs`'s own interleaving: odd-numbered
/// departments sit on items every principal can see, even-numbered ones only on items the narrow
/// principal (term `1`) cannot — so a suggestion page over `department` genuinely differs between
/// principals rather than merely being permitted to.
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
index      = true
vocabulary = "archive"

[[attribute]]
name       = "department"
type       = "category"
render     = true
vocabulary = "department"

[[attribute]]
name     = "score"
type     = "f32"
render   = true
"#;

fn archive_of(entity: u64) -> &'static str {
    ["astro", "cond", "hep", "math", "quant"][(entity % 5) as usize]
}

/// Odd-numbered departments on the items the narrow principal can see, even-numbered ones on the
/// rest — `tests/categories.rs`'s own construction, reused verbatim so the two files' fixtures
/// agree on what a narrow principal may see.
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
        Field::new("score", DataType::Float32, true),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let archives: Vec<&str> = ids.iter().map(|&e| archive_of(e)).collect();
    let departments: Vec<String> = ids.iter().map(|&e| department_of(e)).collect();
    let scores: Vec<Option<f32>> = ids.iter().map(|&e| Some((e % 97) as f32 * 0.5)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives)),
            Arc::new(StringArray::from(departments)),
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

/// A server over the suggestion fixture, plus a session token for a fully-granted principal.
/// `max_suggestions = 4` and `max_suggestion_walk = 1_000` (`spawn_server`'s test defaults), small
/// enough that the fixture's five- and eleven-value vocabularies exercise `limit` and paging
/// behaviour on an ordinary request rather than only on a contrived one.
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

fn keys_of(body: &serde_json::Value) -> Vec<String> {
    body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap().to_string())
        .collect()
}

/// The public column: every value with `q` as a prefix, ascending by matched text, `match`
/// pointing at the key (there is no title), and `code`/`key` matching `/v1/categories`' own.
#[tokio::test]
async fn a_public_column_is_suggested_as_authored() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=c").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["column"], "archive");
    assert_eq!(body["q"], "c");
    let values = body["values"].as_array().unwrap();
    assert_eq!(values.len(), 1, "{body}");
    assert_eq!(values[0]["key"], "cond");
    assert_eq!(values[0]["code"], 22);
    assert!(values[0]["title"].is_null(), "no author wrote one: {body}");
    assert_eq!(values[0]["match"]["field"], "key");
    assert_eq!(values[0]["match"]["start"], 0);
    assert_eq!(values[0]["match"]["len"], 1);
    assert!(
        values[0].get("count").is_none(),
        "counts were not asked for: {body}"
    );
}

/// **The gate is `/v1/categories`' own, unchanged.** A `derived` column is filtered per
/// principal, against the composed candidate — never against the request's own filters, which
/// this route accepts none of at all. The full principal (term `0`, term `1`) sees every
/// department; the narrow one (term `1` only — `tests/categories.rs`'s own split) sees the
/// odd-numbered five and none of the even-numbered ones or `d00`, which is carried by nothing.
#[tokio::test]
async fn a_derived_column_is_filtered_per_principal_exactly_as_the_enumeration_is() {
    let tmp = TempDir::new().unwrap();
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
    let full = authorise(&server, &["0"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let narrow = authorise(&server, &["1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    // The full principal, paged past the server's `max_suggestions = 4` in one call via a limit
    // above the ceiling — clamped, not refused, exactly as `/v1/categories` clamps.
    let (status, body) = get(
        &server,
        &full,
        "/v1/categories/department/suggest?q=d&limit=100",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        keys_of(&body).len(),
        4,
        "clamped to max_suggestions: {body}"
    );
    assert_eq!(body["more"], true, "11 values sit under the prefix: {body}");

    let (status, body) = get(
        &server,
        &narrow,
        "/v1/categories/department/suggest?q=d&limit=100",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let seen = keys_of(&body);
    assert_eq!(
        seen,
        vec!["d01", "d03", "d05", "d07"],
        "the narrow principal sees only the odd-numbered departments, in key order, and never \
         d00 (carried by nothing) or an even one: {body}"
    );
    assert_eq!(
        body["more"], true,
        "d09 sits under the prefix too, hidden from this principal, and `more` is a pre-mask \
         count that must say so regardless (C31): {body}"
    );
}

/// **The disclosure-shaped outcome**: a `derived` column where the narrow principal sees none
/// of the values under a prefix that a wider principal does see. `d10` is even-numbered (carried
/// only by items with `e % 3 != 0`, so the narrow principal — term `1` alone, which sees only
/// `e % 3 == 0` — can see none of them) and is the only value `q = "d10"` matches, so its own
/// range is exhausted inside the walk rather than the budget being spent — `more` is `false`
/// here, not the C31 pre-mask reading the broader `q = "d"` prefix carries above. Asserted
/// against the wide principal too, non-empty, so this cannot pass on a fixture where `d10` is
/// carried by nothing at all.
#[tokio::test]
async fn a_narrow_principal_sees_an_empty_page_where_the_wide_one_does_not() {
    let tmp = TempDir::new().unwrap();
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
    let wide = authorise(&server, &["0"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let narrow = authorise(&server, &["1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = get(&server, &wide, "/v1/categories/department/suggest?q=d10").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        keys_of(&body),
        vec!["d10"],
        "the fixture must carry d10 on at least one visible item, or this test proves nothing: \
         {body}"
    );

    let (status, body) = get(&server, &narrow, "/v1/categories/department/suggest?q=d10").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["values"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new(),
        "the narrow principal may see none of d10's members: {body}"
    );
    assert_eq!(
        body["more"], false,
        "q = \"d10\" matches exactly one value, so the walk's own range is exhausted rather than \
         its budget spent, whatever this principal can see: {body}"
    );
}

/// `more` is `false` exactly when the walk's own range was exhausted — the page filled with
/// everything under the prefix, budget untouched.
#[tokio::test]
async fn more_is_false_when_nothing_is_left_under_the_prefix() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    // `q = "cond"` matches exactly one value, well under both `max_suggestions` and
    // `max_suggestion_walk`.
    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=cond").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(keys_of(&body), vec!["cond"]);
    assert_eq!(body["more"], false, "{body}");
}

/// `count` is present on every value iff `counts=true`, exact, and equal to the enumeration's own
/// `and_cardinality` reading — read here through a full-visibility principal so the two numbers
/// have one composed candidate to agree over.
#[tokio::test]
async fn counts_are_present_iff_asked_and_are_exact() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=a").await;
    assert_eq!(status, 200, "{body}");
    for value in body["values"].as_array().unwrap() {
        assert!(value.get("count").is_none(), "{body}");
    }

    let (status, body) = get(
        &server,
        &token,
        "/v1/categories/archive/suggest?q=a&counts=true",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let values = body["values"].as_array().unwrap();
    assert!(!values.is_empty(), "{body}");
    let expected = (0..N).filter(|&e| archive_of(e) == "astro").count() as u64;
    let astro = values.iter().find(|v| v["key"] == "astro").unwrap();
    assert_eq!(astro["count"], expected, "{body}");
}

/// The walk budget stops a broad prefix early and says so on the wire — a **thresholded** count
/// of at least `max_suggestion_walk` values under the prefix, hidden ones included (C31), on a
/// server configured with a walk budget below the vocabulary's own size.
#[tokio::test]
async fn a_spent_walk_budget_reports_more_even_on_a_short_page() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_with_categories(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine_config = default_engine_config();
    let max_k = engine_config.max_k;
    let engine = tessera_engine::Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        engine_config,
    )
    .expect("engine should open against a freshly built bundle");
    let mut engine = engine;
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once per engine");
    let state = Arc::new(tessera_server::state::AppState {
        engine,
        sessions: parking_lot::Mutex::new(tessera_server::state::SessionRegistry::default()),
        heap: tessera_server::memory::HeapWatch::default(),
        max_k,
        max_category_values: 4,
        // A budget of 2, so a walk over `department`'s 11 values stops on the walk-budget
        // reading rather than the page-filled one — deterministic without needing a vocabulary
        // too large for this test to build. `d00` sorts first and is carried by nothing (§3.3's
        // gate excludes it, never the walk), so a budget of 1 alone would spend its only unit on
        // an invisible value and prove nothing about a value actually being found; 2 leaves room
        // for `d01`, the first this principal can see.
        max_suggestions: 20,
        max_suggestion_walk: 2,
        // The probe route, always: this case is about the walk budget, which the set route does
        // not spend (§6.3).
        max_suggest_set_entities: 1,
        max_browse_rows: 200,
        suggest_admission: tessera_server::state::SuggestAdmission::new(),
        max_shape_vertices: tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES,
        max_region_vertices: 10_000,
        max_region_cells: tessera_engine::DEFAULT_MAX_REGION_CELLS,
        compute_gate: generous_test_gate(),
        ingest_admission: tessera_server::state::IngestAdmission::new(64),
        ingest_max_batch_rows: 200_000,
        ingest_buffer_max_items: 10_000_000,
        ingest_max_batch_bytes: 64 * 1024 * 1024,
        publish_max_body_bytes: 64 * 1024 * 1024,
        max_artifacts_per_request: 10_000,
        max_members_per_request: 5_000_000,
        max_excluded_per_request: 1_000_000,
        stage_timing: false,
        stream_flush_bytes: 1 << 20,
        stream_write_stall_ms: 10_000,
        stream_deadline_ms: 60_000,
        session_credential: SESSION_CREDENTIAL.to_string(),
        operator_credential: OPERATOR_CREDENTIAL.to_string(),
        dev_cors_origins: Vec::new(),
        cors_origins: Vec::new(),
        cors_loopback: false,
        faults: Arc::new(tessera_lifecycle::faults::FaultSwitchboard::new()),
    });

    let viewer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let viewer_addr = viewer_listener.local_addr().unwrap();
    let session_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let session_addr = session_listener.local_addr().unwrap();
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_addr = control_listener.local_addr().unwrap();
    let viewer_router = tessera_server::viewer::router(Arc::clone(&state));
    let session_router = tessera_server::session::router(Arc::clone(&state));
    let control_router = tessera_server::control::router(Arc::clone(&state));
    let serve_tasks = vec![
        tokio::spawn(async move {
            let _ = axum::serve(viewer_listener, viewer_router).await;
        }),
        tokio::spawn(async move {
            let _ = axum::serve(session_listener, session_router).await;
        }),
        tokio::spawn(async move {
            let _ = axum::serve(control_listener, control_router).await;
        }),
    ];
    let server = TestServer {
        viewer_addr,
        session_addr,
        control_addr,
        client: reqwest::Client::new(),
        state,
        serve_tasks,
    };

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let (status, body) = get(&server, &token, "/v1/categories/department/suggest?q=d").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        keys_of(&body),
        vec!["d01"],
        "the budget of 2 spends one unit on invisible d00 and one on visible d01: {body}"
    );
    assert_eq!(body["more"], true, "{body}");
}

/// **404 `unknown` covers "no such column" and "not a category" identically**, as the
/// enumeration's does — the two handlers share one column-resolution function, private to the
/// server crate and so exercised here only through the wire.
#[tokio::test]
async fn unknown_and_non_category_columns_are_404() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, _) = get(&server, &token, "/v1/categories/nonesuch/suggest?q=a").await;
    assert_eq!(status, 404);
    let (status, _) = get(&server, &token, "/v1/categories/score/suggest?q=a").await;
    assert_eq!(status, 404, "score is a plain scalar, not a category");
}

/// `q` over 256 bytes is `422`; `limit=0` is `422` on its own reason (no cursor to advance, but a
/// zero-length page is still a request for no answer); an unknown query parameter is `422` rather
/// than silently ignored, since this route has no cursor and no bulk form to compose with.
#[tokio::test]
async fn the_suggest_specific_refusals() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let long_q = "x".repeat(257);
    let (status, body) = get(
        &server,
        &token,
        &format!("/v1/categories/archive/suggest?q={long_q}"),
    )
    .await;
    assert_eq!(status, 422, "{body}");

    let (status, body) = get(
        &server,
        &token,
        "/v1/categories/archive/suggest?q=a&limit=0",
    )
    .await;
    assert_eq!(status, 422, "{body}");

    let (status, body) = get(
        &server,
        &token,
        "/v1/categories/archive/suggest?q=a&after=cond",
    )
    .await;
    assert_eq!(
        status, 422,
        "`after` is not a parameter this route defines, and must not be silently ignored: {body}"
    );
}

/// `/v1/meta`'s `selection` block publishes `max_suggestions` and `max_suggestion_walk` —
/// `spawn_server`'s test defaults, `4` and `1_000`.
#[tokio::test]
async fn meta_publishes_the_two_suggest_ceilings() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;

    let (status, body) = get(&server, &token, "/v1/meta").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["selection"]["max_suggestions"], 4, "{body}");
    assert_eq!(body["selection"]["max_suggestion_walk"], 1_000, "{body}");
    // The set route's ceiling, published on the same argument (decision 0124). `spawn_server`
    // pins the probe route, so the test default is 0 and the assertion is that the key is
    // published rather than that it carries the shipped default.
    assert_eq!(body["selection"]["max_suggest_set_entities"], 1, "{body}");
}

/// **At most one suggest in flight per session.** A second request for a session already holding
/// the admission slot is refused with the shared `429 backpressure` — `Retry-After: 1` and body
/// `retry_after_s: 1`, exactly as [`tessera_server::error::ApiError::Backpressure`] answers the
/// compute-admission gate — refused before any engine call runs at all. The slot is engineered
/// directly through [`tessera_server::state::SuggestAdmission`] rather than raced with a genuinely
/// slow request: the walk here has nothing slow to hold it open on, so this is the same
/// "engineer a tiny, deterministic gate" move `saturated_gate_sheds_a_second_viewport_with_429...`
/// makes in `tests/http.rs`, applied to a one-slot admission set instead of a one-permit
/// `ComputeGate`.
#[tokio::test]
async fn at_most_one_suggest_in_flight_per_session() {
    let tmp = TempDir::new().unwrap();
    let (server, _token) = serve(&tmp).await;

    // A session of the request's own, so the admission slot occupied below and the request sent
    // against it are unambiguously the same session — `serve`'s own token is deliberately unused
    // here, since two different sessions' slots are independent by construction and would prove
    // nothing about this one.
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    let token_id = auth["token_id"].as_u64().unwrap();

    // Occupy this session's one admission slot directly, exactly as the handler does.
    let guard = server
        .state
        .suggest_admission
        .try_begin(token_id)
        .expect("the slot starts free");

    let resp = server
        .client
        .get(server.viewer_url("/v1/categories/archive/suggest?q=a"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);
    assert_eq!(
        resp.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a 429 must carry Retry-After: 1"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "backpressure");
    assert_eq!(body["retry_after_s"], 1);

    // Releasing the slot lets the next request through.
    drop(guard);
    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=a").await;
    assert_eq!(status, 200, "{body}");
}
