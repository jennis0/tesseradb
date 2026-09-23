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
        "content": { "computed": ["centroid", "hull"], "supplied": [] },
        "depends_on": [],
        "levels": []
    })
}

/// The disclosure rule, over the wire. A layer gated on a term this principal does not hold is
/// exactly as absent from `/v1/meta` as a layer nobody ever registered.
#[tokio::test]
async fn the_meta_layer_list_is_filtered_per_principal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let (status, body) = put_layer(&server, declaration("clusters/open", None)).await;
    assert_eq!(status, 201, "{body}");
    assert!(
        body["tessera_id"].is_string(),
        "the identifier comes back, since it is the only address by which the layer can later be \
         suppressed: {body}"
    );

    let (status, _) = put_layer(&server, declaration("clusters/restricted", Some("1"))).await;
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
    put_layer(&server, declaration("clusters/a", Some("0"))).await;

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
    let (status, body) = put_layer(&server, bad).await;
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
    put_layer(&server, declaration("clusters/a", None)).await;
    assert_eq!(meta_layers(&server, &["0"]).await.len(), 1);

    let resp = server
        .client
        .delete(server.control_url("/control/layers/clusters%2Fa"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    // 200 with a body, not 204: every write acknowledgement on this plane carries its
    // publication number (contracts §3.4).
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["publication"].as_u64().is_some(),
        "the drop names the cycle it is published in: {body}"
    );
    assert!(meta_layers(&server, &["0"]).await.is_empty());

    let (status, body) = put_layer(&server, declaration("clusters/a", None)).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("drop"),
        "bookmarks, edges and suppressions travel by name, so the caller needs to know the name is \
         spent rather than merely taken: {body}"
    );
}

/// Each layer's `/v1/meta` version as `(name, version)`.
async fn layer_versions(server: &TestServer) -> Vec<(String, u64)> {
    meta_layers(server, &["0"])
        .await
        .iter()
        .map(|l| (l["name"].as_str().unwrap().to_string(), l["version"].as_u64().unwrap()))
        .collect()
}

/// Register `name`, publish one artifact into it and grow that artifact. The growth keeps the
/// registration in the log beside the manifest that also carries it.
async fn register_grown(server: &TestServer, name: &str) {
    register(server, declaration(name, None)).await;
    let artifacts = json!({
        "addressing": "external",
        "artifacts": [{ "key": "c0", "members": [member(0), member(1), member(2)] }]
    });
    assert_eq!(publish(server, name, artifacts).await.0, 201);
    let route = format!("/control/layers/{}/artifacts", name.replace('/', "%2F"));
    let grown = server
        .client
        .patch(server.control_url(&route))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": [member(3), member(4)] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(grown.status().as_u16(), 200);
}

/// Publish, then restart twice, and require every layer's version to be where it was.
async fn assert_versions_survive_restarts(server: TestServer, tmp: &TempDir) {
    tick(&server).await;
    let before = layer_versions(&server).await;
    let mut server = server;
    for _ in 0..2 {
        server.shutdown().await;
        server = spawn_server(
            &tmp.path().join("bundle"),
            &tmp.path().join("cache"),
            &tmp.path().join("wal.log"),
        )
        .await;
        assert_eq!(layer_versions(&server).await, before);
    }
}

/// A restart changes nothing about a layer, so the version a client echoes to notice a change
/// stays where it was, after the first restart and after a second.
#[tokio::test]
async fn a_restart_leaves_every_layer_version_where_it_was() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register_grown(&server, "clusters/a").await;
    register_grown(&server, "clusters/b").await;
    assert_eq!(layer_versions(&server).await.len(), 2);
    assert_versions_survive_restarts(server, &tmp).await;
}

/// The same with a layer dropped between registrations: the layers left, and one registered after
/// the drop, keep their versions across restarts.
#[tokio::test]
async fn a_restart_after_a_drop_leaves_every_layer_version_where_it_was() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register_grown(&server, "clusters/a").await;
    register_grown(&server, "clusters/b").await;
    let resp = server
        .client
        .delete(server.control_url("/control/layers/clusters%2Fa"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    register_grown(&server, "clusters/c").await;
    assert_eq!(layer_versions(&server).await.len(), 2);
    assert_versions_survive_restarts(server, &tmp).await;
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
    let (status, _) = put_layer(&server, declaration("clusters/a", None)).await;
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
    put_layer(&server, declaration("clusters/a", None)).await;

    let nonexistent = member(u64::MAX);
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
        json!({ "computed": [], "supplied": [] });
    assert_eq!(put_layer(&server, predicate).await.0, 201);

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
    assert_eq!(put_layer(&server, d).await.0, 201);

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
    put_layer(&server, d).await;
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
    put_layer(&server, d).await;
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
    assert_eq!(put_layer(&server, d).await.0, 201);
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
    put_layer(&server, declaration("clusters/a", None)).await;

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
    assert_eq!(put_layer(&server, d).await.0, 201);

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
    assert!(b.shape.is_some() && n.shape.is_some(), "hull is declared");
    assert!(b.bbox.is_none() && n.bbox.is_none(), "box is not");

    assert!(
        (bc[0] - nc[0]).abs() > 1.0 || (bc[1] - nc[1]).abs() > 1.0,
        "one cluster, two principals, and a centroid that did not move: {bc:?} against {nc:?} — \
         which is what a build-time centroid looks like on this wire"
    );
    // The narrow principal's hull is inside the broad one's bounds: they see a subset of the
    // members, so their hull cannot reach further out than the full one.
    let broad_hull = b.shape.as_ref().unwrap();
    let narrow_hull = n.shape.as_ref().unwrap();
    // Over every ring of every part, because a hull is a list of them.
    let bounds = |h: &Vec<Vec<Vec<[u32; 2]>>>| {
        let v = || h.iter().flatten().flatten();
        [
            v().map(|p| p[0]).min().unwrap(),
            v().map(|p| p[1]).min().unwrap(),
            v().map(|p| p[0]).max().unwrap(),
            v().map(|p| p[1]).max().unwrap(),
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

// ---------------------------------------------------------------------------------------------
// `levels` on the wire (2026-08-28).
// ---------------------------------------------------------------------------------------------

/// A tiered layer with three levels carrying GeoNames' own overlapping zoom ranges.
fn tiered_zoomed(name: &str) -> serde_json::Value {
    let mut d = declaration(name, None);
    d["require_member_visibility"] = serde_json::Value::Null;
    d["hierarchy"] = json!({ "kind": "tiered", "prune_children": true });
    d["content"] =
        json!({ "computed": ["centroid"], "supplied": [] });
    d["levels"] = json!([
        { "level": 0, "title": "Country", "zoom": [0, 4] },
        { "level": 1, "title": "Admin 1", "zoom": [3, 7] },
        { "level": 2, "title": "Admin 2", "zoom": [6, 10] },
    ]);
    d
}

/// Plant one artifact at each of three levels of `admin/boundaries`.
async fn plant_three_levels(server: &TestServer) {
    assert_eq!(
        put_layer(server, tiered_zoomed("admin/boundaries")).await.0,
        201
    );
    for (level, key) in [(0u32, "country"), (1, "state"), (2, "county")] {
        let members: Vec<String> = (0..300u64).map(member).collect();
        let (status, body) = publish(
            server,
            "admin/boundaries",
            json!({
                "level": level,
                "addressing": "external",
                "artifacts": [{ "key": key, "members": members }]
            }),
        )
        .await;
        assert_eq!(status, 201, "{body}");
    }
}

/// Ask at one depth with one `levels` spelling, and report which levels came back.
async fn levels_at(server: &TestServer, zoom: u32, extra: serde_json::Value) -> Vec<u32> {
    let rows = viewport_artifacts(server, &["0"], {
        let mut e = json!({ "zoom": zoom });
        for (k, v) in extra.as_object().unwrap() {
            e[k] = v.clone();
        }
        e
    })
    .await
    .expect("a served layer carries the frame");
    // `rung` is the declared level here — the layer under test is levelled.
    let mut levels: Vec<u32> = rows.iter().map(|r| r.rung).collect();
    levels.sort_unstable();
    levels.dedup();
    levels
}

/// **Omitting `levels` follows the declaration's own zoom→level map** — the wire half of the
/// change. Until now every level was served on every request and a client that read the published
/// map paid for all of them and drew one.
#[tokio::test]
async fn omitting_levels_follows_the_declared_zoom_map() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    plant_three_levels(&server).await;

    assert_eq!(levels_at(&server, 0, json!({})).await, vec![0]);
    assert_eq!(
        levels_at(&server, 3, json!({})).await,
        vec![0, 1],
        "the ranges overlap at their seam, so a depth inside two of them is answered at both"
    );
    assert_eq!(levels_at(&server, 8, json!({})).await, vec![2]);
}

/// **`"all"` and a list both override it**, and a level the layer does not hold is absent rather
/// than a refusal — the route an unreachable layer name takes.
#[tokio::test]
async fn naming_levels_on_the_wire_overrides_the_map() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    plant_three_levels(&server).await;

    assert_eq!(
        levels_at(&server, 0, json!({ "levels": "all" })).await,
        vec![0, 1, 2]
    );
    assert_eq!(
        levels_at(&server, 0, json!({ "levels": [2] })).await,
        vec![2]
    );
    assert_eq!(
        levels_at(&server, 0, json!({ "levels": [0, 9] })).await,
        vec![0],
        "a level the layer does not hold is simply absent"
    );
}

/// **An empty list is *none***, as `layers: []` is — and the symmetry is worth a test because the
/// two fields' *absent* cases are deliberately opposite, so a reader who has just learnt that
/// omitting `levels` means *the declared map* may reasonably guess `[]` means the same.
#[tokio::test]
async fn an_empty_levels_list_is_none() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    plant_three_levels(&server).await;

    let rows = viewport_artifacts(&server, &["0"], json!({ "zoom": 0, "levels": [] })).await;
    assert!(
        rows.is_none(),
        "no level selected serves no artifact, so the frame is absent entirely"
    );
}

/// **Any other spelling is a 422**, the same shape `layers` takes: a stray string must not become
/// a selection that silently matches nothing.
#[tokio::test]
async fn a_levels_field_that_is_neither_a_list_nor_all_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    plant_three_levels(&server).await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 10,
            "layers": "all", "levels": "every"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
}

// ---------------------------------------------------------------------------------------------
// `computed` on the wire (2026-08-28).
// ---------------------------------------------------------------------------------------------

/// One cluster on a layer declaring `centroid` and `hull`, asked for with one `computed` spelling.
async fn computed_row(server: &TestServer, extra: serde_json::Value) -> ArtifactRow {
    let rows = viewport_artifacts(server, &["0"], extra)
        .await
        .expect("a served layer carries the frame");
    rows.into_iter().next().expect("one cluster")
}

async fn one_cluster(server: &TestServer) {
    let mut d = declaration("clusters/a", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    assert_eq!(put_layer(server, d).await.0, 201);
    let members: Vec<String> = (0..300u64).map(member).collect();
    let (status, body) = publish(
        server,
        "clusters/a",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": members }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
}

/// **A request narrows the declaration, and the narrowing reaches the wire.** The layer declares
/// `centroid` and `hull`; a request naming only `centroid` is served the centroid and a null hull.
///
/// This is the field's whole purpose: the client draws one hull and was being served every
/// artifact's, which measured at 94% of a `k = 0` artifacts request on the 2.42M corpus
/// (`artifact-shapes.md` §7).
#[tokio::test]
async fn a_named_computed_set_narrows_what_the_frame_carries() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    one_cluster(&server).await;

    let all = computed_row(&server, json!({})).await;
    assert!(
        all.centroid.is_some() && all.shape.is_some(),
        "absent is the declaration's own set"
    );

    let narrowed = computed_row(&server, json!({ "computed": ["centroid"] })).await;
    assert!(narrowed.centroid.is_some(), "asked for");
    assert!(narrowed.shape.is_none(), "not asked for");
    assert_eq!(
        narrowed.tessera_id, all.tessera_id,
        "the same artifact is served either way — only what is said about it moved"
    );
    assert_eq!(
        narrowed.masked_count, all.masked_count,
        "a geometry selection is not a disclosure control: the count is untouched"
    );
}

/// **Asking for less is never a way to see more.** `box` is not declared by this layer, so naming
/// it serves nothing — the request intersects the declaration and can never union with it.
#[tokio::test]
async fn naming_an_undeclared_property_serves_it_no_more_than_omitting_it() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    one_cluster(&server).await;

    let row = computed_row(&server, json!({ "computed": ["box", "centroid"] })).await;
    assert!(
        row.bbox.is_none(),
        "the layer declares no box; naming it adds none"
    );
    assert!(row.centroid.is_some());
    assert!(row.shape.is_none());
}

/// **The empty list is none** — counts and no geometry, and the frame is still served, because a
/// geometry selection says nothing about which artifacts exist.
#[tokio::test]
async fn an_empty_computed_list_is_counts_and_no_geometry() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    one_cluster(&server).await;

    let row = computed_row(&server, json!({ "computed": [] })).await;
    assert!(row.centroid.is_none() && row.bbox.is_none() && row.shape.is_none());
    assert!(
        row.masked_count > 0,
        "the artifact is still served, and counted"
    );
}

/// **A name outside the vocabulary is a `422`**, unlike an unreachable layer name, which is
/// absent. The vocabulary is deployment schema — fixed, identical for every principal, published
/// in `/v1/meta` — so refusing discloses nothing; a layer name is viewer data and does.
#[tokio::test]
async fn an_unknown_computed_name_is_refused_and_says_the_vocabulary() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    one_cluster(&server).await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 10,
            "layers": "all", "computed": ["outline"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["detail"].as_str().unwrap().contains("centroid"),
        "the refusal names the vocabulary: {body}"
    );
}

/// **The drill-down route is unaffected**, and that is what makes the narrowing usable: the client
/// asks the viewport for centroids and boxes and this route for the one hull it draws.
#[tokio::test]
async fn the_drill_down_still_carries_the_hull_the_viewport_was_not_asked_for() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    one_cluster(&server).await;

    let row = computed_row(&server, json!({ "computed": ["centroid"] })).await;
    assert!(row.shape.is_none());

    let (status, body) = drill(&server, &["0"], &row.tessera_id.to_string()).await;
    assert_eq!(status, 200, "{body}");
    let rings = body["shape"]
        .as_array()
        .expect("the drill-down serves the shape");
    assert!(!rings.is_empty());
}

// `artifact_rows` on the wire, and the artifacts frame's two shapes (2026-08-28,
// `artifact-fetch-protocol.md` §5.2, §5.3, §8).
// ---------------------------------------------------------------------------------------------

/// One `/v1/viewport` POST, un-asserted — for the cases that check a refusal, or read the raw
/// frame bytes.
async fn viewport_response(
    server: &TestServer,
    terms: &[&str],
    extra: serde_json::Value,
) -> reqwest::Response {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let mut body = json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200, "layers": "all" });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// The kind-5 frame's schema column names, read raw — for the assertions about which columns
/// exist, which the row decoder deliberately papers over.
fn artifact_schema_names(body: &[u8]) -> Option<Vec<String>> {
    let frames = tessera_wire::split_frames(body).expect("well-formed frame sequence");
    let payload = frames
        .iter()
        .find(|(kind, _)| *kind == tessera_wire::FRAME_ARTIFACTS)?
        .1;
    let reader = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(payload.to_vec()), None)
        .expect("a complete Arrow stream");
    Some(reader.schema().fields().iter().map(|f| f.name().clone()).collect())
}

/// **§5.2's contract sentence, over the wire**: the same rows, rungs and bits in either
/// projection; the identity response is its own fixed four-column schema, and the payload columns
/// are absent from it rather than null.
#[tokio::test]
async fn identity_rows_are_the_full_rows_with_the_payload_columns_absent() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    plant_three_levels(&server).await;

    let ask = |extra: serde_json::Value| viewport_response(&server, &["0"], extra);
    let full = decode_viewport_frames(&ask(json!({ "levels": "all" })).await.bytes().await.unwrap())
        .artifacts
        .expect("three levels served in full rows");
    assert_eq!(full.len(), 3);
    // The tiered layer's rungs are its declared levels, on the wire under the renamed column.
    let mut rungs: Vec<u32> = full.iter().map(|r| r.rung).collect();
    rungs.sort_unstable();
    assert_eq!(rungs, vec![0, 1, 2]);

    let identity_body = ask(json!({ "levels": "all", "artifact_rows": "identity" }))
        .await
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        artifact_schema_names(&identity_body).unwrap(),
        vec!["layer", "tessera_id", "rung", "matched", "highlighted"],
        "the identity projection is its own four-column schema — absent columns, not null ones"
    );
    let decoded = decode_viewport_frames(&identity_body);
    assert!(decoded.artifacts.is_none());
    let identity = decoded.artifacts_identity.expect("the same frame kind, projected");

    let key = |layer: &str, id: u64, rung: u32, matched: Option<bool>| (layer.to_string(), id, rung, matched);
    let full_view: std::collections::BTreeSet<_> = full
        .iter()
        .map(|r| key(&r.layer, r.tessera_id, r.rung, r.matched))
        .collect();
    let identity_view: std::collections::BTreeSet<_> = identity
        .iter()
        .map(|r| key(&r.layer, r.tessera_id, r.rung, r.matched))
        .collect();
    assert_eq!(
        full_view, identity_view,
        "the row set, the rungs and the bits are identical; only the columns change"
    );
    // No filter was sent, so the bit is null in both projections — *no question*, not `false`.
    assert!(identity.iter().all(|r| r.matched.is_none()));
    // And the full rows really carry the payload the identity rows omit.
    assert!(full.iter().all(|r| r.centroid.is_some()));

    // `"full"` spelled out is the default spelled out.
    let explicit = decode_viewport_frames(
        &ask(json!({ "levels": "all", "artifact_rows": "full" })).await.bytes().await.unwrap(),
    )
    .artifacts
    .expect("the explicit default");
    assert_eq!(explicit, full);
}

/// **The hull columns trail, and only when a served layer declares a hull** — an absent column is
/// distinguishable from a null one, so decision 0076's null rule gains no third reading.
#[tokio::test]
async fn the_shape_columns_trail_and_are_absent_when_no_served_layer_declares_one() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    // `plant_three_levels`' layer declares `centroid` alone: no hull columns at all.
    plant_three_levels(&server).await;
    let without = viewport_response(&server, &["0"], json!({ "levels": "all" }))
        .await
        .bytes()
        .await
        .unwrap();
    let names = artifact_schema_names(&without).unwrap();
    assert_eq!(
        names.last().map(String::as_str),
        Some("target"),
        "no served layer declares a hull, so the schema ends at the fixed prefix: {names:?}"
    );
    assert!(!names.iter().any(|n| n.starts_with("shape_")));

    // The default `declaration` computes a hull, so serving it puts the two columns at the tail.
    let mut d = declaration("clusters/hulled", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    assert_eq!(put_layer(&server, d).await.0, 201);
    let (status, body) = publish(
        &server,
        "clusters/hulled",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": [member(0), member(3), member(6)] }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let with = viewport_response(&server, &["0"], json!({ "layers": ["clusters/hulled"] }))
        .await
        .bytes()
        .await
        .unwrap();
    let names = artifact_schema_names(&with).unwrap();
    assert_eq!(
        &names[names.len() - 2..],
        ["shape_x".to_string(), "shape_y".to_string()],
        "a served hull-declaring layer puts the two columns at the tail: {names:?}"
    );
    let rows = decode_viewport_frames(&with).artifacts.unwrap();
    assert!(rows[0].shape.is_some(), "and the row carries its rings");
}

/// **Any other `artifact_rows` spelling is a 422**, the shape `levels` and `layers` take: a stray
/// string must not become a projection that silently serves something else.
#[tokio::test]
async fn an_artifact_rows_value_that_is_neither_full_nor_identity_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let resp = viewport_response(&server, &["0"], json!({ "artifact_rows": "bits" })).await;
    assert_eq!(resp.status().as_u16(), 422);
}

// ---------------------------------------------------------------------------------------------
// One drawn geometry per artifact, of a declared kind (`polygon-membership.md` §7.1, 2026-08-29).
// ---------------------------------------------------------------------------------------------

/// A polygon layer over the fixture's points: a square over the south-west quarter.
fn spatial_declaration(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "title": "boundaries",
        "views": ["s0"],
        "membership": "spatial",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": ["centroid", "box"], "supplied": [] },
        "depends_on": [],
        "levels": [],
        "shape": { "kind": "polygon" }
    })
}

const SQUARE: &str = "POLYGON ((100 100, 500 100, 500 500, 100 500, 100 100), (200 200, 300 200, 300 300, 200 300, 200 200))";

/// **A predicate shape is served through `shape_x`/`shape_y` only when asked, at the request's
/// depth, holes kept, identical for every principal** — and `/v1/meta` says the kind.
#[tokio::test]
async fn a_predicate_shape_is_served_when_asked_and_is_the_same_for_every_principal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    assert_eq!(
        put_layer(&server, spatial_declaration("boundaries/b"))
            .await
            .0,
        201
    );
    let (status, body) = publish(
        &server,
        "boundaries/b",
        json!({ "addressing": "external", "artifacts": [{ "key": "sw", "members": [], "wkt": SQUARE }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let kinds: Vec<(String, serde_json::Value)> = meta_layers(&server, &["0"])
        .await
        .iter()
        .map(|l| (l["name"].as_str().unwrap().to_string(), l["shape"].clone()))
        .collect();
    assert_eq!(kinds, vec![("boundaries/b".to_string(), json!("predicate"))]);

    // Narrowed away: a request naming `centroid` and `box` is not served the shape, and the
    // columns are absent from the schema. An absent `computed` is the declaration's own set, and
    // a layer's drawn geometry is part of what it declared, whatever its kind.
    let quiet = viewport_artifacts(&server, &["0"], json!({ "computed": ["centroid", "box"] }))
        .await
        .expect("served");
    assert!(quiet[0].shape.is_none() && quiet[0].centroid.is_some() && quiet[0].bbox.is_some());
    let declared = viewport_artifacts(&server, &["0"], json!({})).await.expect("served");
    assert!(declared[0].shape.is_some(), "absent is the declaration, shape included");

    let asked = viewport_artifacts(&server, &["0"], json!({ "computed": ["shape", "centroid"] }))
        .await
        .expect("served");
    let shape = asked[0].shape.as_ref().expect("the shape was asked for");
    assert_eq!(shape.len(), 1, "one part");
    assert_eq!(shape[0].len(), 2, "its outer and its hole, the role kept: {shape:?}");
    assert_eq!(shape[0][0].len(), 4);
    assert_eq!(shape[0][1].len(), 4);
    assert!(asked[0].bbox.is_none(), "narrowed away");

    // The narrow principal sees fewer members and the same drawing.
    let narrow = viewport_artifacts(&server, &["1"], json!({ "computed": ["shape"] }))
        .await
        .expect("served");
    assert!(narrow[0].masked_count < asked[0].masked_count);
    assert_eq!(narrow[0].shape, asked[0].shape, "a predicate shape does not move with the principal");

    // The drill-down carries the same nesting under the same name, at the depth it is asked at.
    let (status, body) = drill(&server, &["0"], &asked[0].tessera_id.to_string()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["shape"][0].as_array().map(Vec::len), Some(2));
    assert_eq!(body["shape"][0][1][0], json!([
        asked[0].shape.as_ref().unwrap()[0][1][0][0],
        asked[0].shape.as_ref().unwrap()[0][1][0][1]
    ]));
}

/// **A shape published into a level whose form is already warm is served.**
///
/// A shape layer's membership is not in the record a publication writes — it is the rows inside the
/// box, resolved by `ShapeStore::warm` after the publication and joined at the next request. So the
/// held row form must **not** be brought forward over such a publication: a form amended with an
/// empty `members` set and stamped at the new level version would *hit* on the next request, ahead
/// of the pieces the warm installed, and the shape would be absent from every viewport until the
/// version or the prefix moved again.
///
/// The first publication and the request after it are what make the form warm; the second is the
/// one under test. Deliberately on a bundle nothing has flushed, where the segments version is `0`
/// and so cannot itself tell a rule-derived level from a stored one.
#[tokio::test]
async fn a_shape_published_into_a_warm_level_is_served() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    assert_eq!(
        put_layer(&server, spatial_declaration("boundaries/b"))
            .await
            .0,
        201
    );

    let (status, body) = publish(
        &server,
        "boundaries/b",
        json!({ "addressing": "external", "artifacts": [{ "key": "sw", "members": [], "wkt": SQUARE }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    // Warm: this request builds the level's row form and the next one is entitled to reuse it.
    let first = viewport_artifacts(&server, &["0"], json!({})).await.expect("served");
    assert_eq!(first.len(), 1, "the first shape is served: {first:?}");
    assert!(first[0].masked_count > 0, "over the rows inside it");

    let (status, body) = publish(
        &server,
        "boundaries/b",
        json!({ "addressing": "external", "artifacts": [{ "key": "ne", "members": [], "wkt": NE_SQUARE }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    // The publication is durable at its acknowledgement and reaches the level's row forms at the
    // next tick (`ingest.md` §1.3).
    tick(&server).await;

    let after = viewport_artifacts(&server, &["0"], json!({})).await.expect("served");
    let mut keys: Vec<(String, bool)> = after
        .iter()
        .map(|a| (a.key.clone().unwrap_or_default(), a.masked_count > 0))
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![("ne".to_string(), true), ("sw".to_string(), true)],
        "both shapes are served over the rows inside them: {after:?}"
    );
}

/// A second square, over the north-east quarter — disjoint from [`SQUARE`], so the two shapes
/// answer for different rows and one standing in for the other would be visible as a count.
const NE_SQUARE: &str = "POLYGON ((600 600, 900 600, 900 900, 600 900, 600 600))";

/// **`hull` is not an ask word**: the vocabulary is `centroid`, `box`, `shape`, and a name outside
/// it is the 422 the vocabulary has always been.
#[tokio::test]
async fn asking_for_a_hull_by_that_word_is_refused_and_shape_names_the_derived_one() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    one_cluster(&server).await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0,
            "layers": "all", "computed": ["hull"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
    let detail = resp.text().await.unwrap();
    assert!(detail.contains("centroid, box, shape"), "{detail}");

    // `shape` on a layer whose drawn geometry is the hull is the hull, one part per group.
    let row = computed_row(&server, json!({ "computed": ["shape"] })).await;
    let shape = row.shape.expect("the derived shape");
    assert!(shape.iter().all(|part| part.len() == 1), "a hull has no holes: {shape:?}");
    let kinds: Vec<serde_json::Value> = meta_layers(&server, &["0"]).await.iter().map(|l| l["shape"].clone()).collect();
    assert_eq!(kinds, vec![json!("derived")]);
}

/// **An authored shape content is read at publication as a membership shape is, and served
/// through the same columns** — the content slot on the wire is blank, the geometry travels as
/// rings, and it is gated as the content is.
#[tokio::test]
async fn an_authored_polygon_content_is_canonicalised_at_publication_and_served_as_rings() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut d = declaration("clusters/drawn", None);
    d["require_member_visibility"] = serde_json::Value::Null;
    d["hierarchy"] = json!({ "kind": "flat", "prune_children": false });
    d["content"] = json!({
        "computed": ["centroid", "box"],
        "supplied": [
            { "name": "label", "type": "text", "require_member_visibility": "inherited" },
            { "name": "outline", "type": "polygon", "require_member_visibility": "inherited" }
        ]
    });
    assert_eq!(put_layer(&server, d).await.0, 201);
    let kinds: Vec<serde_json::Value> = meta_layers(&server, &["0"]).await.iter().map(|l| l["shape"].clone()).collect();
    assert_eq!(kinds, vec![json!("authored")]);

    let members: Vec<String> = (0..30u64).map(member).collect();
    // A polygon that is not WKT is refused naming the row and the content, and nothing lands.
    let (status, body) = publish(
        &server,
        "clusters/drawn",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": members, "content": [{ "values": ["a name", "not a polygon"] }] }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("authored polygon content"), "{body}");

    let (status, body) = publish(
        &server,
        "clusters/drawn",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "c0", "members": members, "content": [{ "values": ["a name", SQUARE] }] }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert!(body["shapes"].is_array() || body.get("shapes").is_none(), "{body}");

    let rows = viewport_artifacts(&server, &["0"], json!({ "computed": ["shape"] }))
        .await
        .expect("served");
    let shape = rows[0].shape.as_ref().expect("the authored shape");
    assert_eq!(shape[0].len(), 2, "outer and hole: {shape:?}");
    // Without the ask there is no shape column at all.
    let quiet = viewport_artifacts(&server, &["0"], json!({ "computed": ["centroid"] })).await.expect("served");
    assert!(quiet[0].shape.is_none());

    // The drill-down: the text in its slot, the shape's slot blank — the geometry travels as
    // rings under `shape` and never as the string.
    let (status, body) = drill(&server, &["0"], &rows[0].tessera_id.to_string()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["content"], json!(["a name", ""]));
    assert_eq!(body["shape"][0].as_array().map(Vec::len), Some(2));
}

/// **A layer declares at most one drawn geometry**: a hull beside an authored polygon is refused
/// at registration naming both.
#[tokio::test]
async fn a_hull_beside_an_authored_shape_is_refused_at_registration() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut d = declaration("clusters/two", None);
    d["content"]["supplied"] = json!([{ "name": "outline", "type": "polygon", "require_member_visibility": "inherited" }]);
    let (status, body) = put_layer(&server, d).await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("one drawn geometry"), "{body}");
}

/// **A declaration carrying the removed `withdraw_on_member_deletion` is refused by name**
/// (decision 0135). The strict and permissive modes the field selected between are gone, and a
/// body written for them is refused rather than read with the field ignored (decision 0048): the
/// caller reads that the field was removed and what the one behaviour is, at either value.
#[tokio::test]
async fn a_declaration_carrying_the_removed_withdrawal_field_is_refused_by_name() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    for value in [true, false] {
        let mut d = declaration("clusters/stale", None);
        d["content"]["withdraw_on_member_deletion"] = json!(value);
        let resp = server
            .client
            .put(server.control_url("/control/layers"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .json(&d)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        assert_eq!(status, 422, "{body}");
        assert!(body.contains("`withdraw_on_member_deletion` was removed"), "{body}");
        assert!(body.contains("decision 0135"), "{body}");
    }
    // The same declaration without the field registers, so the refusal was the field's.
    let (status, body) = put_layer(&server, declaration("clusters/stale", None)).await;
    assert_eq!(status, 201, "{body}");
}
