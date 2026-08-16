//! **The layer registry over HTTP: who is told a layer exists, and what they are told about it.**
//!
//! Two rules, and each fails silently when it is wrong:
//!
//! - **`/v1/meta`'s layer list is per-principal**, and it is the only field on that document that
//!   is. Everything else there is a deployment constant identical for every caller, so a shared
//!   cache over the response would be harmless — and would hand one principal another's registry
//!   the moment layers arrived. A gate-failed layer is absent by the same route a name nobody
//!   registered is.
//! - **The artifact cardinality is never published.** It is the obvious field to add and it is
//!   C8's row: a corpus-wide count over objects the principal may not individually see.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

async fn serve(tmp: &TempDir) -> TestServer {
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

async fn meta_layers(server: &TestServer, terms: &[&str]) -> Vec<serde_json::Value> {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    body["layers"].as_array().cloned().unwrap_or_default()
}

/// The minimum a declaration needs. `artifacts_carry_own` has no default, deliberately, so it is
/// spelled out at every call — that field decides whether an artifact's existence derives from its
/// members' visibility or from its own label, and a default would let a corpus-derived layer
/// acquire the wrong one silently.
fn declaration(name: &str, gate: Option<&str>) -> serde_json::Value {
    json!({
        "name": name,
        "title": format!("{name} (title)"),
        "slices": ["s0"],
        "membership": "enumerated",
        "access": { "label": gate, "artifacts_carry_own": false },
        "visible_when": { "min_visible": 50 },
        "hierarchy": { "kind": "nested", "prune_children": true },
        "content": { "derived": ["centroid", "hull"], "supplied": [], "on_member_deletion": "withdraw_content" },
        "depends_on": [],
        "levels": []
    })
}

async fn register(server: &TestServer, declaration: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// The disclosure rule, over the wire. A layer gated on a term this principal does not hold is
/// exactly as absent from `/v1/meta` as a layer nobody ever registered.
#[tokio::test]
async fn the_meta_layer_list_is_filtered_per_principal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let (status, body) = register(&server, declaration("clusters/open", None)).await;
    assert_eq!(status, 201, "{body}");
    assert!(
        body["tessera_id"].is_string(),
        "the identifier comes back, since it is the only address by which the layer can later be \
         suppressed: {body}"
    );

    let (status, _) = register(&server, declaration("clusters/restricted", Some("1"))).await;
    assert_eq!(status, 201);

    let broad: Vec<String> = meta_layers(&server, &["0"])
        .await
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        broad,
        vec!["clusters/open".to_string()],
        "the gated layer is absent, and so is every name nobody registered — one absence, one route"
    );

    let narrow: Vec<String> = meta_layers(&server, &["1"])
        .await
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        narrow,
        vec!["clusters/open".to_string(), "clusters/restricted".to_string()],
        "an ungated layer is reachable by every principal — the gate narrows, it never widens, so \
         holding term 1 adds the restricted layer rather than exchanging one for the other"
    );
}

/// What the document says about a layer a principal *may* see — and the two things it must not say.
#[tokio::test]
async fn a_published_layer_carries_its_declaration_and_never_its_cardinality() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration("clusters/a", Some("0"))).await;

    let layers = meta_layers(&server, &["0"]).await;
    let layer = &layers[0];

    assert_eq!(layer["name"], "clusters/a");
    assert_eq!(layer["title"], "clusters/a (title)");
    assert_eq!(layer["hierarchy"]["kind"], "nested");
    assert_eq!(layer["hierarchy"]["prune_children"], true);
    assert_eq!(layer["derived_content"], json!(["centroid", "hull"]));
    // A nested layer declares no levels: its lineage is its edges, and a level number would say
    // nothing about position in it.
    assert_eq!(layer["levels"], json!([]));

    let object = layer.as_object().unwrap();
    // **C8.** A count of artifacts in a layer is a corpus-wide count over objects this principal
    // may not individually see. It is the obvious field to add, which is why its absence is
    // asserted rather than assumed.
    for forbidden in ["artifact_count", "artifacts", "cardinality", "count", "size"] {
        assert!(
            !object.contains_key(forbidden),
            "the artifact cardinality must never be published (C8): {layer}"
        );
    }
    // The gate label is not published either. A caller who reaches the layer has already satisfied
    // it and can do nothing with the name; a caller who has not never sees the entry — and putting
    // a term name on this document is what would make the unreachable case distinguishable from the
    // nonexistent one.
    assert!(
        !object.contains_key("access") && !object.contains_key("gate"),
        "the gate label must not reach the wire: {layer}"
    );
}

/// A declaration the model forbids is refused with a message the caller can act on, and nothing is
/// left behind.
#[tokio::test]
async fn an_incoherent_declaration_is_refused_with_a_reason() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    // Nested means the hierarchy is in the edges, so declaring levels beside it is the one
    // combination that is always a mistake.
    let mut bad = declaration("clusters/bad", None);
    bad["levels"] = json!([{ "level": 0, "title": "L0", "zoom": null }]);
    let (status, body) = register(&server, bad).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("edges"),
        "the refusal has to say what to fix: {body}"
    );

    assert!(meta_layers(&server, &["0"]).await.is_empty());
}

/// Drop tombstones the name for ever, and the recreation refusal says so.
#[tokio::test]
async fn a_dropped_name_is_gone_from_meta_and_refused_on_recreation() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration("clusters/a", None)).await;
    assert_eq!(meta_layers(&server, &["0"]).await.len(), 1);

    let resp = server
        .client
        .delete(server.control_url("/control/layers/clusters%2Fa"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert!(meta_layers(&server, &["0"]).await.is_empty());

    let (status, body) = register(&server, declaration("clusters/a", None)).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("drop"),
        "bookmarks, edges and suppressions travel by name, so the caller needs to know the name is \
         spent rather than merely taken: {body}"
    );
}

/// The whole plane is behind the operator credential, and a route added to it inherits that check
/// rather than asking for it. Asserted because the layer routes are new arrivals on that router.
#[tokio::test]
async fn the_layer_routes_require_the_operator_credential() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .json(&declaration("clusters/a", None))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let resp = server
        .client
        .delete(server.control_url("/control/layers/anything"))
        .bearer_auth("not-the-operator-credential")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}
