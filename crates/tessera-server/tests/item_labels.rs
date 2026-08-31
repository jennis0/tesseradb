//! `POST /v1/items/{tessera_id}`'s `labels` array: **the satisfied terms only** (contracts §3.2,
//! [decision 0114](../../../docs/decisions/0114-the-drill-down-serves-the-satisfied-labels-only.md)).
//!
//! The ruling's own sentence is the test: an item's labels intersected with the session's
//! satisfied set, and never the full set. What makes that worth its own file rather than a case
//! in `http.rs` is the shape of the failure — the endpoint would still answer `200` with the right
//! record while naming a compartment the viewer does not hold, so every assertion here is about
//! what is *absent* from a successful response.
//!
//! The fixture's labelling is `common::terms_of`: every item carries `"0"`, and a multiple of
//! three also carries `"1"`. So one bundle offers an item with two labels and an item with one,
//! and three principals — `{0}`, `{1}`, `{0,1}` — see three different intersections of them.

mod common;

use tempfile::TempDir;

use common::*;

/// Everything below needs the same bundle and two ids: one item carrying both labels and one
/// carrying only `"0"`. The ids come from a principal's own viewport, which is the only route a
/// client has to one (I10 — nothing inverts an identity here, not even a test).
struct Fixture {
    _tmp: TempDir,
    server: TestServer,
    /// A `tessera_id` for an item labelled `{"0", "1"}`.
    two_labels: u64,
    /// A `tessera_id` for an item labelled `{"0"}` alone.
    one_label: u64,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
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

    // A principal holding both labels sees every item, and the `"1"`-only principal's viewport is
    // exactly the multiples of three — so an id in both is a two-label item and an id in the first
    // and not the second carries `"0"` alone.
    let both = authorise(&server, &["0", "1"]).await;
    let both = both["token"].as_str().unwrap().to_string();
    let narrow = authorise(&server, &["1"]).await;
    let narrow = narrow["token"].as_str().unwrap().to_string();

    // Classified by **visibility**, not by the field under test: the `"1"`-only principal can see
    // exactly the multiples of three, so a `200` from its own drill-down says the item carries
    // `"1"` and a `404` says it does not. A viewport is a *sample*, so the two principals' point
    // sets are not nested and set arithmetic over them would be the flake this avoids.
    let mut two_labels = None;
    let mut one_label = None;
    for id in ids_visible_to(&server, &both).await {
        let carries_one = post_item(&server, &narrow, id).await.status() == 200;
        if carries_one {
            two_labels.get_or_insert(id);
        } else {
            one_label.get_or_insert(id);
        }
        if two_labels.is_some() && one_label.is_some() {
            break;
        }
    }

    Fixture {
        _tmp: tmp,
        two_labels: two_labels.expect("the fixture holds an item labelled both"),
        one_label: one_label.expect("and one labelled only `0`"),
        server,
    }
}

async fn ids_visible_to(server: &TestServer, token: &str) -> Vec<u64> {
    let response = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 500
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let (_tiles, points) = decode_viewport(&response.bytes().await.unwrap());
    points.iter().map(|p| p.0).collect()
}

/// `labels` on a drill-down, or the failure that produced no `200`.
async fn labels_of(server: &TestServer, token: &str, id: u64) -> Vec<String> {
    let response = post_item(server, token, id).await;
    assert_eq!(response.status(), 200, "the item is visible to this token");
    let body: serde_json::Value = response.json().await.unwrap();
    body["labels"]
        .as_array()
        .expect("`labels` is always present, empty included")
        .iter()
        .map(|v| v.as_str().expect("a label is a string").to_string())
        .collect()
}

/// **The ruling's own sentence.** An item with several labels, drilled down by a principal
/// satisfying a subset, serves exactly that subset — and the principal holding both gets both.
#[tokio::test]
async fn a_drill_down_serves_the_intersection_and_not_the_item_s_label_set() {
    let fx = fixture().await;
    let both = authorise(&fx.server, &["0", "1"]).await;
    let both = both["token"].as_str().unwrap();
    let only_zero = authorise(&fx.server, &["0"]).await;
    let only_zero = only_zero["token"].as_str().unwrap();

    assert_eq!(
        labels_of(&fx.server, both, fx.two_labels).await,
        vec!["0".to_string(), "1".to_string()],
        "a principal holding both labels of a two-label item is served both"
    );
    assert_eq!(
        labels_of(&fx.server, only_zero, fx.two_labels).await,
        vec!["0".to_string()],
        "and one holding only `0` is served only `0` — the item's `1` is a compartment they do \
         not hold, and learning it exists on this item is the disclosure this rules out"
    );
    assert_eq!(
        labels_of(&fx.server, only_zero, fx.one_label).await,
        vec!["0".to_string()],
        "a one-label item is its one label where the principal holds it"
    );
}

/// **A principal satisfying via one term never sees the other compartments**, including the one
/// every other item in the corpus carries. `"0"` is on every item in this fixture, so a
/// `"1"`-only principal seeing it would be the widest possible form of the leak.
#[tokio::test]
async fn a_principal_satisfying_one_term_is_served_that_term_alone() {
    let fx = fixture().await;
    let narrow = authorise(&fx.server, &["1"]).await;
    let narrow = narrow["token"].as_str().unwrap();

    assert_eq!(
        labels_of(&fx.server, narrow, fx.two_labels).await,
        vec!["1".to_string()],
        "`0` is on this item and on every other, and is still not served to a principal who does \
         not hold it"
    );
}

/// **The passthrough's presentation is the identity, and that is its real answer** (decision
/// 0114): its descriptors *are* the caller's own label strings, so what comes back is byte-equal
/// to what the credential presented — not a rendering of a term id, and not an internal name.
#[tokio::test]
async fn the_passthrough_serves_the_caller_s_own_strings_verbatim() {
    let fx = fixture().await;
    // The credential presents these two exactly. The response must use the same bytes.
    let presented = ["0", "1"];
    let token = authorise(&fx.server, &presented).await;
    let token = token["token"].as_str().unwrap();
    let served = labels_of(&fx.server, token, fx.two_labels).await;
    for label in &served {
        assert!(
            presented.contains(&label.as_str()),
            "every served label is one the credential itself presented, got {served:?}"
        );
    }
}

/// **Deterministic order**: sorted by the presented string, so two identical requests agree and
/// the order says nothing about the corpus's interning. Asserted against the sorted copy rather
/// than a literal, so the rule survives a fixture whose labels change.
#[tokio::test]
async fn labels_are_sorted_and_the_same_on_every_request() {
    let fx = fixture().await;
    let token = authorise(&fx.server, &["1", "0"]).await;
    let token = token["token"].as_str().unwrap();

    let first = labels_of(&fx.server, token, fx.two_labels).await;
    let second = labels_of(&fx.server, token, fx.two_labels).await;
    assert_eq!(first, second, "two identical requests agree");
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted, "and the order is the sorted one");

    // The credential presented them in the other order; the response does not depend on that.
    let reversed = authorise(&fx.server, &["0", "1"]).await;
    let reversed = reversed["token"].as_str().unwrap();
    assert_eq!(
        labels_of(&fx.server, reversed, fx.two_labels).await,
        first,
        "and not on the order the credential listed its terms in"
    );
}

/// **A masked item is the same `404` it was**, and the new field changes nothing about it: the
/// body is byte-identical to the one an identifier naming nothing gets, so the `labels` array
/// cannot become the oracle contracts §3.2 closed.
#[tokio::test]
async fn an_invisible_item_is_still_the_identical_404() {
    let fx = fixture().await;
    let narrow = authorise(&fx.server, &["1"]).await;
    let narrow = narrow["token"].as_str().unwrap();

    // `one_label` carries `"0"` alone, so the `"1"`-only principal cannot see it.
    let invisible = post_item(&fx.server, narrow, fx.one_label).await;
    assert_eq!(invisible.status(), 404);
    let invisible_body = invisible.text().await.unwrap();

    let unknown = post_item(&fx.server, narrow, 0).await;
    assert_eq!(unknown.status(), 404);
    let unknown_body = unknown.text().await.unwrap();

    assert_eq!(
        invisible_body, unknown_body,
        "the two 404s stay byte-identical: an item this principal cannot see and an identifier \
         naming nothing"
    );
    assert!(
        !invisible_body.contains("label") && !invisible_body.contains('0'),
        "and the refusal names no label: {invisible_body}"
    );
}
