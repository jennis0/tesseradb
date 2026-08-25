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

/// The minimum a declaration needs. `artifact_visibility` has no default, deliberately, so it is
/// spelled out at every call — that field decides whether an artifact's existence derives from its
/// members' visibility or from its own label, and a default would let a corpus-derived layer
/// acquire the wrong one silently.
fn declaration(name: &str, visibility: Option<&str>) -> serde_json::Value {
    json!({
        "name": name,
        "title": format!("{name} (title)"),
        "views": ["s0"],
        "membership": "enumerated",
        "visibility": visibility,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": { "count": 50 },
        "hierarchy": { "kind": "nested", "prune_children": true },
        "content": { "computed": ["centroid", "hull"], "supplied": [], "withdraw_on_member_deletion": true },
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
        vec![
            "clusters/open".to_string(),
            "clusters/restricted".to_string()
        ],
        "a `public` layer is reachable by every principal — the gate narrows, it never widens, so \
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
    assert_eq!(layer["computed_content"], json!(["centroid", "hull"]));
    // A nested layer declares no levels: its lineage is its edges, and a level number would say
    // nothing about position in it.
    assert_eq!(layer["levels"], json!([]));

    let object = layer.as_object().unwrap();
    // **C8.** A count of artifacts in a layer is a corpus-wide count over objects this principal
    // may not individually see. It is the obvious field to add, which is why its absence is
    // asserted rather than assumed.
    for forbidden in [
        "artifact_count",
        "artifacts",
        "cardinality",
        "count",
        "size",
    ] {
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
        !object.contains_key("visibility") && !object.contains_key("gate"),
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

// ---- publication -------------------------------------------------------------------------------

/// Base64 the way `/control/changes` does it — external ids are bytes, not text.
fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

async fn publish(
    server: &TestServer,
    layer: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    // A layer name is path-shaped, so its slash is percent-encoded into the one path segment the
    // route captures — the same encoding the drop route already takes.
    let encoded = layer.replace('/', "%2F");
    let resp = server
        .client
        .put(server.control_url(&format!("/control/layers/{encoded}/artifacts")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// The publication round trip, and the fields the response must not carry.
///
/// **A `tessera_id` per artifact and never an ordinal.** An ordinal is a position in a dense level,
/// so two of them tell the holder how many artifacts sit between — a corpus-wide count over objects
/// they may not individually see, which is C8's row. The identifier is the only artifact address
/// that crosses the wire, and it is what a later suppression names.
#[tokio::test]
async fn publishing_artifacts_returns_an_identifier_each_and_never_an_ordinal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let (status, _) = register(&server, declaration("clusters/a", None)).await;
    assert_eq!(status, 201);

    let (status, body) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [
                { "key": "c0", "members": [member(0), member(1), member(2)] },
                { "key": "c1", "members": [member(3), member(4)] },
            ]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let artifacts = body["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
    let mut ids = std::collections::BTreeSet::new();
    for (i, artifact) in artifacts.iter().enumerate() {
        assert_eq!(artifact["key"], ["c0", "c1"][i]);
        assert!(
            artifact["tessera_id"].is_string(),
            "string-encoded, since a bare JSON number loses a u64 past 2^53: {artifact}"
        );
        assert!(ids.insert(artifact["tessera_id"].as_str().unwrap().to_string()));
        assert!(
            artifact.get("ordinal").is_none(),
            "an ordinal is a position in a dense level, so it never crosses the wire: {artifact}"
        );
        assert!(
            artifact.get("members").is_none() && artifact.get("size").is_none(),
            "and neither does an unmasked membership or its count: {artifact}"
        );
    }
    assert_eq!(server.state.engine.published_artifacts(), 2);
}

/// **An unresolvable member refuses the batch rather than being dropped.** A silently dropped member
/// shrinks both the masked count a viewer is shown and the declared size the proportional criterion
/// divides by — so a typo in a pipeline would move artifacts across their own existence threshold,
/// in the direction of hiding them, with nothing anywhere saying so.
#[tokio::test]
async fn an_unresolvable_member_refuses_the_whole_batch() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration("clusters/a", None)).await;

    use base64::Engine as _;
    let nonexistent = base64::engine::general_purpose::STANDARD.encode(external_id_of(u64::MAX));
    let (status, body) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [
                { "key": "c0", "members": [member(0)] },
                { "key": "c1", "members": [member(1), nonexistent] },
            ]
        }),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    let detail = body.to_string();
    assert!(
        detail.contains("id 1 of artifact 1"),
        "the refusal names the coordinate the caller's pipeline holds: {detail}"
    );
    assert_eq!(
        server.state.engine.published_artifacts(),
        0,
        "and the first artifact was not published either — the batch is the commit unit"
    );
}

/// A refusal the caller can act on: their own declaration measured against the deployment's rules.
#[tokio::test]
async fn publishing_into_a_layer_that_does_not_take_artifacts_is_a_422_that_says_why() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    // A predicate layer's artifacts are derived from a rule, so it declares none of the things a
    // published artifact carries beside its membership: no computed content, a flat hierarchy.
    let mut predicate = declaration("regions/uk", None);
    predicate["membership"] = json!({ "attribute": "severity" });
    predicate["require_member_visibility"] = json!({ "count": 25 });
    predicate["hierarchy"] = json!({ "kind": "flat", "prune_children": false });
    predicate["content"] =
        json!({ "computed": [], "supplied": [], "withdraw_on_member_deletion": true });
    assert_eq!(register(&server, predicate).await.0, 201);

    let (status, body) = publish(
        &server,
        "regions/uk",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": [member(0)] }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body.to_string().contains("predicate"),
        "it names what is wrong with the declaration, not an opaque code: {body}"
    );

    // And a name nobody registered is refused by the same route, saying the same kind of thing.
    let (status, _) = publish(
        &server,
        "clusters/never",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": [member(0)] }]
        }),
    )
    .await;
    assert_eq!(status, 422);
}

async fn viewport_artifacts(
    server: &TestServer,
    terms: &[&str],
    extra: serde_json::Value,
) -> Option<Vec<ArtifactRow>> {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let mut body = json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200, "layers": "all" });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
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
    decode_viewport_frames(&resp.bytes().await.unwrap()).artifacts
}

/// **The wire's headline, and the field it must not carry.** Two principals, one cluster, two
/// counts — and no ordinal, no membership and no declared size anywhere in the frame.
#[tokio::test]
async fn the_artifacts_frame_carries_a_masked_count_and_no_unmasked_quantity() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut d = declaration("clusters/a", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    assert_eq!(register(&server, d).await.0, 201);

    // 300 documents; the fixture gives term 1 to every third source id.
    let members: Vec<String> = (0..300u64).map(member).collect();
    let expected_narrow = (0..300u64).filter(|s| terms_of(*s).contains(&1)).count() as u64;
    let (status, body) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": members }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let broad = viewport_artifacts(&server, &["0"], json!({}))
        .await
        .expect("a served cluster carries the frame");
    let narrow = viewport_artifacts(&server, &["1"], json!({}))
        .await
        .expect("and so does the narrow principal's");
    assert_eq!(broad.len(), 1);
    assert_eq!(narrow.len(), 1);

    assert_eq!(broad[0].masked_count, 300);
    assert_eq!(
        narrow[0].masked_count, expected_narrow,
        "the narrow principal is told how many members *they* can see"
    );
    assert_ne!(
        narrow[0].masked_count, 300,
        "a count equal to the membership would mean the mask was never applied"
    );
    // The identifier is stable across principals by construction (C17); only the number moves.
    assert_eq!(broad[0].tessera_id, narrow[0].tessera_id);
    assert_eq!(broad[0].key.as_deref(), Some("c0"));
    assert_eq!(broad[0].layer, "clusters/a");
}

/// A response with nothing to say about artifacts carries **no artifacts frame at all** — a
/// deployment with no layers pays nothing for the channel.
#[tokio::test]
async fn a_response_with_no_artifacts_carries_no_artifacts_frame() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    assert!(viewport_artifacts(&server, &["0"], json!({}))
        .await
        .is_none());

    let mut d = declaration("clusters/a", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    register(&server, d).await;
    publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": [member(0), member(1)] }]
        }),
    )
    .await;
    assert!(viewport_artifacts(&server, &["0"], json!({}))
        .await
        .is_some());

    // A request naming no layer asks nothing and is answered with nothing — and costs no frame.
    assert!(viewport_artifacts(&server, &["0"], json!({ "layers": [] }))
        .await
        .is_none());
    // A gated layer this principal cannot reach is the same answer as a layer nobody registered.
    assert!(
        viewport_artifacts(&server, &["0"], json!({ "layers": ["clusters/never"] }))
            .await
            .is_none()
    );
}

/// The artifact budget is accepted and, on a flat layer, inert — because the only reduction the
/// representation allows is structural, and a flat layer has no structure to reduce by. Meeting it
/// by sampling would give a wrong map rather than a smaller one.
#[tokio::test]
async fn the_artifact_budget_is_accepted_and_never_met_by_sampling() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut d = declaration("clusters/a", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    register(&server, d).await;
    publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [
                { "key": "c0", "members": [member(0), member(3)] },
                { "key": "c1", "members": [member(6), member(9)] },
                { "key": "c2", "members": [member(12), member(15)] },
            ]
        }),
    )
    .await;

    let unbudgeted = viewport_artifacts(&server, &["0"], json!({}))
        .await
        .unwrap();
    assert_eq!(unbudgeted.len(), 3);
    let budgeted = viewport_artifacts(&server, &["0"], json!({ "artifact_budget": 1 }))
        .await
        .unwrap();
    assert_eq!(
        budgeted, unbudgeted,
        "a flat layer has no ancestors to cut to, so the budget is inert — dropping two of three \
         clusters would be a wrong map, not a smaller one"
    );
}

async fn drill(server: &TestServer, terms: &[&str], tessera_id: &str) -> (u16, serde_json::Value) {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url(&format!("/v1/artifacts/{tessera_id}")))
        .bearer_auth(token)
        .json(&json!({ "view": "s0" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// **Drill-down agrees with the viewport, and every withheld case is one `404`.** A cluster
/// reachable by identifier but not on the map would be the one rule transcribed twice.
#[tokio::test]
async fn drilling_down_on_an_artifact_agrees_with_the_viewport_and_withholds_identically() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    // A bar the broad principal clears and the narrow one misses, taken from the fixture's own
    // term rule rather than from anything the server said.
    let expected_narrow = (0..300u64).filter(|s| terms_of(*s).contains(&1)).count() as u64;
    let mut d = declaration("clusters/a", None);
    d["require_member_visibility"] = json!({ "count": expected_narrow + 1 });
    assert_eq!(register(&server, d).await.0, 201);
    let members: Vec<String> = (0..300u64).map(member).collect();
    let (status, _) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": members }]
        }),
    )
    .await;
    assert_eq!(status, 201);

    let served = viewport_artifacts(&server, &["0"], json!({}))
        .await
        .unwrap();
    assert_eq!(served.len(), 1);
    let id = served[0].tessera_id.to_string();

    let (status, body) = drill(&server, &["0"], &id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["layer"], "clusters/a");
    assert_eq!(body["key"], "c0");
    assert_eq!(
        body["masked_count"].as_u64().unwrap(),
        served[0].masked_count,
        "one predicate, one answer — the map and the drill-down cannot disagree"
    );
    assert!(
        body.get("ordinal").is_none() && body.get("members").is_none(),
        "and the drill-down carries no more than the frame does: {body}"
    );

    // Below its criterion for this principal: `404`, and the same `404` a never-issued identifier
    // gets. A held identifier is not a way round the criterion.
    let (status, withheld) = drill(&server, &["1"], &id).await;
    assert_eq!(status, 404, "{withheld}");
    let (nonexistent_status, nonexistent) = drill(&server, &["0"], "123456789").await;
    assert_eq!(nonexistent_status, 404);
    assert_eq!(
        withheld, nonexistent,
        "an artifact withheld and an identifier naming nothing are one answer, byte for byte — a \
         second detail string would be the oracle the single failure shape exists to prevent"
    );
}

/// An idset guards a keyed identifier and means nothing beside an external id, so accepting one
/// there would imply a check that never ran.
#[tokio::test]
async fn an_idset_is_required_with_identifiers_and_refused_beside_external_ids() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration("clusters/a", None)).await;

    let (status, body) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "idset": 1,
            "artifacts": [{ "key": "c0", "members": [member(0)] }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");

    let (status, body) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "tessera",
            "artifacts": [{ "key": "c0", "members": ["12345"] }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("idset"), "{body}");
}

/// **Derived geometry crosses the wire, and it moves with the principal.**
///
/// The count obviously belongs to the viewer; a centroid looks like a property of the cluster,
/// which is what makes a build-time one the fail-open worth a wire-level test of its own. Two
/// principals, one cluster, two different shapes — and the narrow principal's hull is drawn from
/// positions they are entitled to.
#[tokio::test]
async fn the_artifacts_frame_carries_geometry_computed_for_the_asking_principal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut d = declaration("clusters/a", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    assert_eq!(register(&server, d).await.0, 201);

    let members: Vec<String> = (0..300u64).map(member).collect();
    let (status, body) = publish(
        &server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": members }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let broad = viewport_artifacts(&server, &["0"], json!({}))
        .await
        .unwrap();
    let narrow = viewport_artifacts(&server, &["1"], json!({}))
        .await
        .unwrap();

    // The layer declares `centroid` and `hull`, so both arrive and `box` does not.
    let (b, n) = (&broad[0], &narrow[0]);
    let (bc, nc) = (b.centroid.expect("declared"), n.centroid.expect("declared"));
    assert!(b.hull.is_some() && n.hull.is_some(), "hull is declared");
    assert!(b.bbox.is_none() && n.bbox.is_none(), "box is not");

    assert!(
        (bc[0] - nc[0]).abs() > 1.0 || (bc[1] - nc[1]).abs() > 1.0,
        "one cluster, two principals, and a centroid that did not move: {bc:?} against {nc:?} — \
         which is what a build-time centroid looks like on this wire"
    );
    // The narrow principal's hull is inside the broad one's bounds: they see a subset of the
    // members, so their hull cannot reach further out than the full one.
    let broad_hull = b.hull.as_ref().unwrap();
    let narrow_hull = n.hull.as_ref().unwrap();
    let bounds = |h: &Vec<[u32; 2]>| {
        [
            h.iter().map(|v| v[0]).min().unwrap(),
            h.iter().map(|v| v[1]).min().unwrap(),
            h.iter().map(|v| v[0]).max().unwrap(),
            h.iter().map(|v| v[1]).max().unwrap(),
        ]
    };
    let (bb, nb) = (bounds(broad_hull), bounds(narrow_hull));
    assert!(
        nb[0] >= bb[0] && nb[1] >= bb[1] && nb[2] <= bb[2] && nb[3] <= bb[3],
        "the narrow hull {nb:?} reaches outside the broad one {bb:?}"
    );
    // Same artifact throughout: only what is said about it moved.
    assert_eq!(b.tessera_id, n.tessera_id);
}
