//! **`/v1/categories/{column}/suggest`: the typeahead over a category vocabulary.**
//!
//! One gate with `/v1/categories` (`tests/categories.rs` covers that gate's own mechanics in
//! depth — a public and a derived column, an interleaved narrow principal), so these cases cover
//! what is new here: the wire shape (`match`, `count`, `more`), the two refusals the enumeration
//! has no need of (`q` over 256 bytes, an unknown query parameter), the walk budget, the
//! per-session admission (`value-suggestion.md` §5.1), and the `POST` form's counts under a filter
//! and its compute admission.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::StringArray;
use tempfile::TempDir;

use common::*;

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

/// The suggestion fixture in `dir`: [`N`] items carrying the columns [`SCHEMA_TOML`] declares.
fn build_categories(dir: &Path) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..N).collect();
    let archive = StringArray::from_iter_values(ids.iter().map(|&e| archive_of(e)));
    let department = StringArray::from_iter_values(ids.iter().map(|&e| department_of(e)));
    let score =
        arrow::array::Float32Array::from_iter(ids.iter().map(|&e| Some((e % 97) as f32 * 0.5)));
    write_points(
        &points,
        &ids,
        scatter,
        vec![
            column("archive", false, archive),
            column("department", false, department),
            column("score", true, score),
        ],
    );
    write_pairs_n(&pairs, N);
    let schema = format!("{SCHEMA_TOML}\n{ID_ATTRIBUTE}");
    build_declared(&dir.join("bundle"), &points, &pairs, &schema);
}

/// A copy of [`build_categories`]' bundle in `tmp`, built once for this binary.
fn copy_categories(tmp: &TempDir) {
    static BUILT: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    copy_built(&BUILT, tmp.path(), build_categories);
}

/// A server over the suggestion fixture, plus a session token for a fully-granted principal.
/// `max_suggestions = 4` and `max_suggestion_walk = 1_000` (`spawn_server`'s test defaults), small
/// enough that the fixture's five- and eleven-value vocabularies exercise `limit` and paging
/// behaviour on an ordinary request rather than only on a contrived one.
async fn serve(tmp: &TempDir) -> (TestServer, String) {
    copy_categories(tmp);
    let server = open(tmp).await;
    let token = token_for(&server, &["0"]).await;
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
    assert!(values[0].get("count").is_none(), "counts were not asked for: {body}");
}

/// **The gate is `/v1/categories`' own, unchanged.** A `derived` column is filtered per
/// principal, against the composed candidate — never against the request's own filters, which
/// this route accepts none of at all. The full principal (term `0`, term `1`) sees every
/// department; the narrow one (term `1` only — `tests/categories.rs`'s own split) sees the
/// odd-numbered five and none of the even-numbered ones or `d00`, which is carried by nothing.
#[tokio::test]
async fn a_derived_column_is_filtered_per_principal_exactly_as_the_enumeration_is() {
    let tmp = TempDir::new().unwrap();
    copy_categories(&tmp);
    let server = open(&tmp).await;
    let full = token_for(&server, &["0"]).await;
    let narrow = token_for(&server, &["1"]).await;

    // The full principal, paged past the server's `max_suggestions = 4` in one call via a limit
    // above the ceiling — clamped, not refused, exactly as `/v1/categories` clamps.
    let (status, body) = get(&server, &full, "/v1/categories/department/suggest?q=d&limit=100").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(keys_of(&body).len(), 4, "clamped to max_suggestions: {body}");
    assert_eq!(body["more"], true, "11 values sit under the prefix: {body}");

    let (status, body) = get(&server, &narrow, "/v1/categories/department/suggest?q=d&limit=100").await;
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
    copy_categories(&tmp);
    let server = open(&tmp).await;
    let wide = token_for(&server, &["0"]).await;
    let narrow = token_for(&server, &["1"]).await;

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

    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=a&counts=true").await;
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
    copy_categories(&tmp);
    let bundle_root = tmp.path().join("bundle");
    let engine_config = default_engine_config();
    let max_k = engine_config.max_k;
    let engine = tessera_engine::Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config,
    )
    .expect("engine should open against a freshly built bundle");
    let mut engine = engine;
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once per engine");
    let (catalogue, integrator_key, catalogue_dir) = test_identity();
    let state = Arc::new(tessera_server::state::AppState {
        engine,
        sessions: parking_lot::Mutex::new(tessera_server::state::SessionRegistry::default()),
        heap: tessera_server::memory::HeapWatch::default(),
        limits: tessera_server::state::ServeLimits {
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
            ingest_max_batch_rows: 200_000,
            ingest_buffer_max_items: 10_000_000,
            ingest_max_batch_bytes: 64 * 1024 * 1024,
            ..Default::default()
        },
        suggest_admission: tessera_server::state::SuggestAdmission::new(),
        compute_gate: generous_test_gate(),
        password_gate: generous_password_gate(),
        bulk_gate: generous_bulk_gate(),
        artifact_gate: generous_artifact_gate(),
        ingest_admission: tessera_server::state::IngestAdmission::new(64),
        catalogue,
        oidc: tessera_server::oidc::Verifier::new(),
        operator_credential: OPERATOR_CREDENTIAL.to_string(),
        request_log: None,
        faults: Arc::new(tessera_lifecycle::faults::FaultSwitchboard::new()),
    });

    let server = serve_state(state, integrator_key, Some(catalogue_dir)).await;

    let token = token_for(&server, &["0"]).await;

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

    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=a&limit=0").await;
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

// ---------------------------------------------------------------------------------------------
// The `POST` form: counts under a filter
// ---------------------------------------------------------------------------------------------

async fn post(
    server: &TestServer,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, reqwest::header::HeaderMap, serde_json::Value) {
    let resp = server
        .client
        .post(server.viewer_url(path))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// **A filtered count is the items this principal may see in the view that pass the filter and
/// carry the value**, for a principal who sees everything and one who sees a third, under a filter
/// over a column with no index, so it is answered by the view's rows, and a region. The values
/// offered are the `GET` form's, and the region's verdict is in `x-tessera-region`.
#[tokio::test]
async fn a_filtered_count_is_the_visible_items_passing_the_filter() {
    let tmp = TempDir::new().unwrap();
    let (server, _) = serve(&tmp).await;
    let departments = ["d01", "d03", "d04"];
    let (lo, hi) = (100.5, 800.5);
    let filter = serde_json::json!({ "all_of": [
        { "department": { "in": departments } },
        { "region": { "bbox": [lo, lo, hi, hi] } },
    ] });
    let passes = |e: u64| {
        let (x, y) = scatter(e);
        departments.contains(&department_of(e).as_str())
            && (lo..=hi).contains(&x)
            && (lo..=hi).contains(&y)
    };
    for (terms, sees) in [(["0"], (|_| true) as fn(u64) -> bool), (["1"], |e| e % 3 == 0)] {
        let token = token_for(&server, &terms).await;
        let (_, unfiltered) =
            get(&server, &token, "/v1/categories/archive/suggest?q=&limit=20&counts=true&view=s0")
                .await;
        let (status, headers, body) = post(
            &server,
            &token,
            "/v1/categories/archive/suggest",
            &serde_json::json!({ "q": "", "limit": 20, "counts": true, "view": "s0", "filters": filter }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(headers["x-tessera-region"], "exact");
        assert_eq!(keys_of(&body), keys_of(&unfiltered), "{terms:?}: {body}");
        let mut total = 0;
        for value in body["values"].as_array().unwrap() {
            let key = value["key"].as_str().unwrap();
            let expected = (0..N)
                .filter(|&e| sees(e) && passes(e) && archive_of(e) == key)
                .count() as u64;
            assert_eq!(value["count"], expected, "{terms:?} {key}: {body}");
            total += expected;
        }
        assert!(total > 0, "the filter passes some item for {terms:?}");
        let passing = (0..N).filter(|&e| sees(e) && passes(e)).count() as u64;
        assert_eq!(body["total"], passing, "{terms:?}: total under a region leaf: {body}");
    }
}

/// **A value the filter excludes is offered with count 0**, and a filter on the counted column
/// itself narrows the counts to its own values.
#[tokio::test]
async fn a_value_the_filter_excludes_is_offered_with_zero() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;
    let (status, _, body) = post(
        &server,
        &token,
        "/v1/categories/archive/suggest",
        &serde_json::json!({
            "q": "", "limit": 20, "counts": true, "view": "s0",
            "filters": { "archive": { "in": ["cond"] } },
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // The server's `max_suggestions` is 4.
    assert_eq!(keys_of(&body), ["astro", "cond", "hep", "math"], "{body}");
    for value in body["values"].as_array().unwrap() {
        let expected = if value["key"] == "cond" {
            (0..N).filter(|&e| archive_of(e) == "cond").count() as u64
        } else {
            0
        };
        assert_eq!(value["count"], expected, "{body}");
    }
}

/// **`total` is the size of the set the counts are taken over**: this principal's visible items,
/// within the view, passing the filter. It is present iff counts were asked for, and it counts
/// items whose value the page does not offer.
#[tokio::test]
async fn total_is_the_counted_set() {
    let tmp = TempDir::new().unwrap();
    let (server, _) = serve(&tmp).await;
    let filter = serde_json::json!({ "department": { "in": ["d01", "d02", "d03"] } });
    let passes = |e: u64| ["d01", "d02", "d03"].contains(&department_of(e).as_str());
    for (terms, sees) in [(["0"], (|_| true) as fn(u64) -> bool), (["1"], |e| e % 3 == 0)] {
        let token = token_for(&server, &terms).await;
        let visible = (0..N).filter(|&e| sees(e)).count() as u64;
        let passing = (0..N).filter(|&e| sees(e) && passes(e)).count() as u64;

        let (_, body) = get(&server, &token, "/v1/categories/archive/suggest?q=&counts=true").await;
        assert_eq!(body["total"], visible, "{terms:?}: {body}");
        let (_, body) =
            get(&server, &token, "/v1/categories/archive/suggest?q=&counts=true&view=s0").await;
        assert_eq!(body["total"], visible, "{terms:?}: {body}");
        let (_, body) =
            get(&server, &token, "/v1/categories/archive/suggest?q=a&counts=true&view=s0").await;
        assert_eq!(body["total"], visible, "a narrower page counts over the same set: {body}");
        let (status, _, body) = post(
            &server,
            &token,
            "/v1/categories/archive/suggest",
            &serde_json::json!({ "q": "", "counts": true, "view": "s0", "filters": filter }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["total"], passing, "{terms:?}: {body}");

        let (_, body) = get(&server, &token, "/v1/categories/archive/suggest?q=&view=s0").await;
        assert!(body.get("total").is_none(), "counts were not asked for: {body}");
    }
}

/// `filters` needs `view`, is parsed as the viewport parses it, and without `counts` changes
/// nothing.
#[tokio::test]
async fn the_post_forms_refusals() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;
    let path = "/v1/categories/archive/suggest";
    let filter = serde_json::json!({ "archive": { "in": ["cond"] } });
    let (status, _, body) = post(
        &server,
        &token,
        path,
        &serde_json::json!({ "q": "", "counts": true, "filters": filter }),
    )
    .await;
    assert_eq!((status, body["error"].as_str()), (422, Some("contract")), "{body}");
    let (status, _, body) = post(
        &server,
        &token,
        path,
        &serde_json::json!({ "q": "", "counts": true, "view": "s0", "filters": { "nonesuch": { "in": ["x"] } } }),
    )
    .await;
    assert_eq!((status, body["error"].as_str()), (422, Some("contract")), "{body}");
    let (status, headers, body) = post(
        &server,
        &token,
        path,
        &serde_json::json!({ "q": "", "view": "s0", "filters": filter }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(headers.get("x-tessera-region").is_none());
    assert!(body["values"].as_array().unwrap().iter().all(|v| v.get("count").is_none()));
}

/// **A suggest counting within a view takes a compute permit, and one that does not open a view
/// does not**: with the gate's one permit held and no queue, a count within a view is shed with
/// `429` in either form, filtered or not, and a count across the database and a page without
/// counts are served.
#[tokio::test]
async fn a_suggest_counting_within_a_view_is_subject_to_compute_admission() {
    let tmp = TempDir::new().unwrap();
    copy_categories(&tmp);
    let server = spawn_server_with_config_and_gate(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        default_engine_config(),
        tessera_server::state::ComputeGate::new(1, 0, 250),
    )
    .await;
    let token = token_for(&server, &["0"]).await;
    let held = server.state.compute_gate.admit().await.expect("the one permit is free");
    let path = "/v1/categories/archive/suggest";
    let (status, _, body) = post(
        &server,
        &token,
        path,
        &serde_json::json!({ "counts": true, "view": "s0", "filters": { "archive": { "in": ["cond"] } } }),
    )
    .await;
    assert_eq!((status, body["error"].as_str()), (429, Some("backpressure")), "{body}");
    let (status, _, body) =
        post(&server, &token, path, &serde_json::json!({ "counts": true, "view": "s0" })).await;
    assert_eq!((status, body["error"].as_str()), (429, Some("backpressure")), "{body}");
    let (status, body) =
        get(&server, &token, "/v1/categories/archive/suggest?q=a&counts=true&view=s0").await;
    assert_eq!((status, body["error"].as_str()), (429, Some("backpressure")), "{body}");
    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=a&counts=true").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = get(&server, &token, "/v1/categories/archive/suggest?q=a&view=s0").await;
    assert_eq!(status, 200, "{body}");
    drop(held);
    let (status, _, body) = post(
        &server,
        &token,
        path,
        &serde_json::json!({ "counts": true, "view": "s0", "filters": { "archive": { "in": ["cond"] } } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

/// The counts a filtered `POST` serves for `archive` in `s0`, by key, and the whole body.
async fn filtered_counts(
    server: &TestServer,
    token: &str,
    filters: serde_json::Value,
) -> (u16, serde_json::Value) {
    let (status, _, body) = post(
        server,
        token,
        "/v1/categories/archive/suggest",
        &serde_json::json!({ "q": "", "counts": true, "view": "s0", "filters": filters }),
    )
    .await;
    (status, body)
}

/// **A `member_of` leaf counts the artifact's members this principal may see**; an artifact
/// withheld from the principal answers exactly as an identifier naming nothing, and a layer the
/// principal cannot reach is refused exactly as one that does not exist.
#[tokio::test]
async fn a_member_of_filter_counts_the_members_and_withholds_as_absence() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;
    let mut teams = flat_layer("teams");
    teams["artifact_visibility"] = serde_json::json!({ "field": "team", "default": "inherited" });
    register(&server, teams).await;
    let mut hidden = flat_layer("hidden");
    hidden["visibility"] = serde_json::json!("secret");
    register(&server, hidden).await;
    let publish = |layer: &'static str, artifacts: serde_json::Value| {
        let server = &server;
        async move {
            let resp = server
                .client
                .put(server.control_url(&format!("/control/layers/{layer}/artifacts")))
                .bearer_auth(OPERATOR_CREDENTIAL)
                .json(&serde_json::json!({ "artifacts": artifacts }))
                .send()
                .await
                .unwrap();
            let body: serde_json::Value = resp.json().await.unwrap();
            body["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["tessera_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };
    let ids = publish(
        "teams",
        serde_json::json!([
            { "key": "open", "members": members(0..30), "access": null },
            { "key": "withheld", "members": members(30..50), "access": ["secret"] },
        ]),
    )
    .await;
    publish("hidden", serde_json::json!([{ "key": "h", "members": members(0..10), "access": null }]))
        .await;
    let (open, withheld) = (&ids[0], &ids[1]);

    for (terms, sees) in [(["0"], (|_| true) as fn(u64) -> bool), (["1"], |e| e % 3 == 0)] {
        let token = token_for(&server, &terms).await;
        let (status, body) = filtered_counts(
            &server,
            &token,
            serde_json::json!({ "member_of": { "layer": "teams", "artifact": open } }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        for value in body["values"].as_array().unwrap() {
            let key = value["key"].as_str().unwrap();
            let expected = (0..30).filter(|&e| sees(e) && archive_of(e) == key).count() as u64;
            assert_eq!(value["count"], expected, "{terms:?} {key}: {body}");
        }
    }

    let withheld_answer = filtered_counts(
        &server,
        &token,
        serde_json::json!({ "member_of": { "layer": "teams", "artifact": withheld } }),
    )
    .await;
    let nothing_answer = filtered_counts(
        &server,
        &token,
        serde_json::json!({ "member_of": { "layer": "teams", "artifact": "123456789" } }),
    )
    .await;
    assert_eq!(withheld_answer, nothing_answer);
    assert!(withheld_answer.1["values"].as_array().unwrap().iter().all(|v| v["count"] == 0));

    let (unreachable_status, unreachable) = filtered_counts(
        &server,
        &token,
        serde_json::json!({ "member_of": { "layer": "hidden", "artifact": open } }),
    )
    .await;
    let (unknown_status, unknown) = filtered_counts(
        &server,
        &token,
        serde_json::json!({ "member_of": { "layer": "nowhere", "artifact": open } }),
    )
    .await;
    assert_eq!(unreachable_status, 422, "{unreachable}");
    assert_eq!(unknown_status, unreachable_status);
    assert_eq!(unknown["error"], unreachable["error"]);
    assert_eq!(
        unknown["detail"].as_str().unwrap().replace("nowhere", "hidden"),
        unreachable["detail"].as_str().unwrap()
    );
}

/// **A negated filter counts the items carrying a value in its column that match none of it.**
#[tokio::test]
async fn a_negated_filter_counts_what_it_leaves() {
    let tmp = TempDir::new().unwrap();
    let (server, token) = serve(&tmp).await;
    let (status, body) = filtered_counts(
        &server,
        &token,
        serde_json::json!({ "none_of": [{ "department": { "in": ["d01", "d02"] } }] }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for value in body["values"].as_array().unwrap() {
        let key = value["key"].as_str().unwrap();
        let expected = (0..N)
            .filter(|&e| !["d01", "d02"].contains(&department_of(e).as_str()) && archive_of(e) == key)
            .count() as u64;
        assert_eq!(value["count"], expected, "{key}: {body}");
    }
}
