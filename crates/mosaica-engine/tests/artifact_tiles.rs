//! **What things are here, tile by tile**, against an oracle that knows only the viewer's visible
//! points and where each one lies.
//!
//! The oracle reads the visible points tile by tile from an uncapped viewport, and for each tile
//! names the artifacts with a visible member there, ordered by whole visible count and then by
//! `tessera_id`, the first `per_tile` of them. The engine's frames must say exactly that, for a
//! level stored by row with its members and coverings, for an overlapping level pinned to be stored
//! by artifact, and for an overlapping level of a dozen artifacts with no pin, which is stored by
//! row in the list form, at several depths, for viewers who see everything, a third, and nothing.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use mosaica_engine::{
    ArtifactOut, Engine, EngineError, LayerSelection, SinkResult, ViewportArtifactsHead,
    ViewportArtifactsOut, ViewportArtifactsRequest, ViewportArtifactsSink, ViewportRequest,
};
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_lifecycle::membership::IncomingAttachment;
use mosaica_lifecycle::IncomingArtifact;
use mosaica_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ServingLayout,
};
use mosaica_types::{EntityId, TesseraId};

/// A partition of the corpus, stored by row: every artifact is a run of source ids, which the
/// fixture scatters over the whole map, in a handful of sizes so counts tie.
const ROWS: &str = "clusters/rows";
/// Overlapping residue classes, stored by artifact.
const OVERLAP: &str = "clusters/overlap";
/// Overlapping residue classes with no pin.
const AUTO: &str = "clusters/auto";

fn declaration(name: &str, layout: Option<ServingLayout>) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into(), "box".into()],
            supplied: Vec::new(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout,
        shape: None,
    }
}

/// Each layer's artifacts by key, as source ids.
type Memberships = BTreeMap<String, BTreeSet<u64>>;

struct Fixture {
    _tmp: tempfile::TempDir,
    engine: Engine,
    /// Entity to source id.
    source_of: BTreeMap<u64, u64>,
    layers: BTreeMap<&'static str, Memberships>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let map = source_to_new_map(&root, "v00000");
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config_uncapped(),
    )
    .expect("the engine opens");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");

    let sizes = [100u64, 150, 200, 250, 300];
    let mut rows = Memberships::new();
    let mut at = 0u64;
    for k in 0.. {
        if at >= N_ITEMS {
            break;
        }
        let end = (at + sizes[k % sizes.len()]).min(N_ITEMS);
        rows.insert(format!("r{k:02}"), (at..end).collect());
        at = end;
    }
    let mut overlap = Memberships::new();
    for r in 0..7u64 {
        overlap.insert(
            format!("m7-{r}"),
            (0..N_ITEMS).filter(|e| e % 7 == r).collect(),
        );
    }
    for q in 0..5u64 {
        overlap.insert(
            format!("m11-{q}"),
            (0..N_ITEMS).filter(|e| e % 11 == q).collect(),
        );
    }
    let mut auto = Memberships::new();
    for r in 0..13u64 {
        auto.insert(
            format!("m13-{r}"),
            (0..N_ITEMS).filter(|e| e % 13 == r || e % 29 == r).collect(),
        );
    }
    let layers = BTreeMap::from([(ROWS, rows), (OVERLAP, overlap), (AUTO, auto)]);
    for (name, layout) in [
        (ROWS, Some(ServingLayout::RowMajorLabel)),
        (OVERLAP, Some(ServingLayout::ArtifactMajor)),
        (AUTO, None),
    ] {
        engine.register_layer(declaration(name, layout)).unwrap();
        let artifacts = layers[name]
            .iter()
            .map(|(key, sources)| {
                IncomingArtifact::from_entities(
                    Some(key.clone()),
                    sources.iter().map(|s| EntityId::new(map[s])),
                )
            })
            .collect();
        engine.publish_artifacts(name.into(), 0, artifacts).unwrap();
    }
    tick(&engine);
    assert_eq!(
        engine.recorded_layout(ROWS, 0),
        Some(ServingLayout::RowMajorLabel)
    );
    assert_eq!(
        engine.recorded_layout(AUTO, 0),
        Some(ServingLayout::RowMajorList)
    );
    Fixture {
        _tmp: tmp,
        engine,
        source_of: map
            .into_iter()
            .map(|(source, entity)| (entity, source))
            .collect(),
        layers,
    }
}

/// The viewer's visible source ids, by tile, read from an uncapped viewport.
fn visible_by_tile(fx: &Fixture, credential: &[u8], zoom: u8) -> BTreeMap<u64, BTreeSet<u64>> {
    let session = fx.engine.authorise(credential).unwrap();
    let out = fx
        .engine
        .viewport(
            &session,
            ViewportRequest::new("s0", zoom, WHOLE_MAP, N_ITEMS as usize)
                .layers(LayerSelection::Named(&[])),
        )
        .unwrap();
    let ids: Vec<TesseraId> = out
        .points
        .tessera_ids
        .iter()
        .map(|&id| TesseraId::new(id))
        .collect();
    let entities = fx.engine.resolve_tessera_ids(&ids).unwrap();
    let mut points = entities
        .into_iter()
        .map(|e| fx.source_of[&e.unwrap().raw()]);
    let mut by_tile = BTreeMap::new();
    for tile in &out.tiles {
        assert_eq!(tile.served, tile.visible, "the viewport is uncapped");
        by_tile.insert(
            tile.tile,
            points.by_ref().take(tile.served as usize).collect(),
        );
    }
    by_tile
}

fn ask(
    fx: &Fixture,
    credential: &[u8],
    layer: &str,
    zoom: u8,
    per_tile: usize,
) -> ViewportArtifactsOut {
    let session = fx.engine.authorise(credential).unwrap();
    let names = [layer];
    fx.engine
        .viewport_artifacts(
            &session,
            ViewportArtifactsRequest::new("s0", zoom, WHOLE_MAP, per_tile)
                .layers(LayerSelection::Named(&names)),
        )
        .unwrap()
}

/// Every frame as `tile → [(key, count)]`, with every artifact's figures the same in every frame.
fn frames(out: &ViewportArtifactsOut) -> BTreeMap<u64, Vec<(String, u64)>> {
    let mut figures: BTreeMap<String, &ArtifactOut> = BTreeMap::new();
    let mut by_tile = BTreeMap::new();
    for frame in &out.frames {
        let tile = frame.tile.expect("a flat layer has a tile");
        let rows = frame
            .artifacts
            .iter()
            .map(|a| {
                let key = a.key.clone().unwrap();
                let held = figures.entry(key.clone()).or_insert(a);
                assert_eq!(
                    (
                        held.masked_count,
                        held.derived.centroid,
                        held.derived.bbox,
                        held.tessera_id
                    ),
                    (
                        a.masked_count,
                        a.derived.centroid,
                        a.derived.bbox,
                        a.tessera_id
                    ),
                    "{key} is served with other figures in tile {tile}"
                );
                (key, a.masked_count)
            })
            .collect();
        assert!(
            by_tile.insert(tile, rows).is_none(),
            "tile {tile} is answered once"
        );
    }
    by_tile
}

/// The oracle's answer for every tile holding a visible point.
fn oracle(
    members: &Memberships,
    visible: &BTreeMap<u64, BTreeSet<u64>>,
    ids: &BTreeMap<String, u64>,
    per_tile: usize,
) -> BTreeMap<u64, Vec<(String, u64)>> {
    let everywhere: BTreeSet<u64> = visible.values().flatten().copied().collect();
    let whole: BTreeMap<&String, u64> = members
        .iter()
        .map(|(key, sources)| (key, sources.intersection(&everywhere).count() as u64))
        .collect();
    visible
        .iter()
        .map(|(&tile, here)| {
            let mut present: Vec<(u64, u64, String)> = members
                .iter()
                .filter(|(_, sources)| !sources.is_disjoint(here))
                .map(|(key, _)| {
                    let id = *ids.get(key).unwrap_or_else(|| {
                        panic!("{key} has a visible member and is never served")
                    });
                    (whole[key], id, key.clone())
                })
                .collect();
            present.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            present.truncate(per_tile);
            (
                tile,
                present
                    .into_iter()
                    .map(|(count, _, key)| (key, count))
                    .collect(),
            )
        })
        .collect()
}

/// **Each tile's frame is the oracle's**: the artifacts with a visible member in it, by whole
/// visible count and then `tessera_id`, the first `per_tile`; a tile with nothing visible is a frame
/// of no rows; and an artifact in several tiles carries the same figures in each.
#[test]
fn each_tile_serves_what_the_oracle_names_in_its_order() {
    let fx = fixture();
    for credential in [
        full_coverage_credential(),
        subset_credential(),
        zero_credential(),
    ] {
        for zoom in [0u8, 1, 2, 3] {
            let visible = visible_by_tile(&fx, &credential, zoom);
            for layer in [ROWS, OVERLAP, AUTO] {
                let whole = ask(&fx, &credential, layer, zoom, usize::MAX);
                let ids: BTreeMap<String, u64> = whole
                    .artifacts()
                    .into_iter()
                    .map(|a| (a.key.unwrap(), a.tessera_id.raw()))
                    .collect();
                for per_tile in [usize::MAX, 3] {
                    let at = format!("{layer}, zoom {zoom}, per_tile {per_tile}");
                    let got = frames(&ask(&fx, &credential, layer, zoom, per_tile));
                    let want = oracle(&fx.layers[layer], &visible, &ids, per_tile);
                    assert_eq!(got.len(), 1usize << (2 * zoom), "{at}: a frame per tile");
                    for (tile, rows) in &got {
                        let expected = want.get(tile).cloned().unwrap_or_default();
                        assert_eq!(rows, &expected, "{at}: tile {tile}");
                    }
                    if per_tile == 3 && credential != zero_credential() {
                        assert!(
                            got.values().any(|rows| rows.len() == 3),
                            "{at}: the quota binds somewhere"
                        );
                    }
                }
                if credential == zero_credential() {
                    assert!(
                        ids.is_empty(),
                        "{layer}: a viewer seeing nothing is served nothing"
                    );
                } else {
                    let spanning = whole.frames.iter().flat_map(|f| &f.artifacts).count();
                    assert!(
                        zoom == 0 || spanning > ids.len(),
                        "{layer} at zoom {zoom}: some artifact spans tiles"
                    );
                }
            }
        }
    }
}

/// **A denied member leaves its artifact's tile on the next request**, and the artifact's count
/// everywhere else drops by the members denied.
#[test]
fn a_denied_member_removes_its_artifact_from_its_tile_on_the_next_request() {
    let fx = fixture();
    let credential = full_coverage_credential();
    for layer in [ROWS, OVERLAP, AUTO] {
        let zoom = 2;
        let visible = visible_by_tile(&fx, &credential, zoom);
        let before = ask(&fx, &credential, layer, zoom, usize::MAX);
        let rows = frames(&before);
        // An artifact served in more than one tile, and one tile it is served in.
        let (key, tile) = rows
            .iter()
            .flat_map(|(tile, rows)| rows.iter().map(move |(key, _)| (key.clone(), *tile)))
            .find(|(key, _)| {
                rows.values()
                    .filter(|r| r.iter().any(|(k, _)| k == key))
                    .count()
                    > 1
            })
            .expect("an artifact spans tiles");
        let denied: Vec<u64> = fx.layers[layer][&key]
            .intersection(&visible[&tile])
            .copied()
            .collect();
        let count = before
            .artifacts()
            .iter()
            .find(|a| a.key.as_ref() == Some(&key))
            .unwrap()
            .masked_count;
        let entities: BTreeMap<u64, u64> = fx
            .source_of
            .iter()
            .map(|(&entity, &source)| (source, entity))
            .collect();
        for source in &denied {
            fx.engine
                .accept_change(EntityId::new(entities[source]), ChangeOp::Suppress)
                .unwrap();
        }
        let after = frames(&ask(&fx, &credential, layer, zoom, usize::MAX));
        assert!(
            !after[&tile].iter().any(|(k, _)| k == &key),
            "{layer}: {key} is still served in tile {tile} after its members there were denied"
        );
        let elsewhere: Vec<u64> = after
            .values()
            .flatten()
            .filter(|(k, _)| k == &key)
            .map(|(_, count)| *count)
            .collect();
        assert!(
            !elsewhere.is_empty(),
            "{layer}: {key} is still served where it has members"
        );
        assert!(elsewhere.iter().all(|&n| n == count - denied.len() as u64));
        for source in &denied {
            fx.engine
                .accept_change(EntityId::new(entities[source]), ChangeOp::Unsuppress)
                .unwrap();
        }
    }
}

/// A sink that cancels the request once it has its first frame.
struct CancelAfterFirst {
    cancel: mosaica_engine::CancelToken,
    frames: usize,
}

impl ViewportArtifactsSink for CancelAfterFirst {
    fn head(&mut self, _: ViewportArtifactsHead) -> SinkResult {
        Ok(())
    }

    fn frame(&mut self, _: Option<u64>, _: &[ArtifactOut]) -> SinkResult {
        self.frames += 1;
        self.cancel.cancel();
        Ok(())
    }
}

/// **A cancellation is seen between tiles**: a request cancelled while its first tile's frame is
/// being sent walks no further tile.
#[test]
fn a_cancelled_request_stops_between_tiles() {
    let fx = fixture();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let cancel = mosaica_engine::CancelToken::new();
    let mut sink = CancelAfterFirst {
        cancel: cancel.clone(),
        frames: 0,
    };
    let names = [ROWS];
    let outcome = fx.engine.viewport_artifacts_stream(
        &session,
        ViewportArtifactsRequest::new("s0", 2, WHOLE_MAP, usize::MAX)
            .layers(LayerSelection::Named(&names))
            .cancel(Some(cancel)),
        &mut sink,
    );
    assert!(
        matches!(outcome, Err(EngineError::Cancelled)),
        "{outcome:?}"
    );
    assert_eq!(sink.frames, 1, "no tile after the cancellation was walked");
}

/// **A dependent takes a place in a tile's quota only where its target is in the frame.** Two
/// clusters, the larger served alone under a quota of one, and a label on each: the label on the
/// cluster left out is the larger label, and yet the tile serves the other, since the frame would
/// drop a label whose cluster it does not hold. The request names the labels first; the clusters
/// are still chosen first.
#[test]
fn a_dependent_whose_target_the_frame_lacks_takes_no_place_in_the_quota() {
    let fx = fixture();
    let (clusters, labels) = ("deps/clusters", "deps/labels");
    let mut target = declaration(clusters, Some(ServingLayout::ArtifactMajor));
    target.content.computed = Vec::new();
    fx.engine.register_layer(target).unwrap();
    let mut dependent = declaration(labels, Some(ServingLayout::ArtifactMajor));
    dependent.content.computed = Vec::new();
    dependent.depends_on = vec![clusters.into()];
    fx.engine.register_layer(dependent).unwrap();
    let entities: BTreeMap<u64, u64> =
        fx.source_of.iter().map(|(&entity, &source)| (source, entity)).collect();
    let members = |range: std::ops::Range<u64>| range.map(|s| EntityId::new(entities[&s]));
    fx.engine
        .publish_artifacts(
            clusters.into(),
            0,
            vec![
                IncomingArtifact::from_entities(Some("big".into()), members(0..600)),
                IncomingArtifact::from_entities(Some("small".into()), members(600..900)),
            ],
        )
        .unwrap();
    let label = |key: &str, target: &str, sources: std::ops::Range<u64>| {
        IncomingArtifact::attached(
            Some(key.into()),
            members(sources),
            Vec::new(),
            IncomingAttachment {
                layer: clusters.into(),
                level: 0,
                key: target.into(),
            },
        )
    };
    fx.engine
        .publish_artifacts(
            labels.into(),
            0,
            vec![
                label("on-big", "big", 0..100),
                label("on-small", "small", 600..900),
            ],
        )
        .unwrap();
    tick(&fx.engine);

    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let names = [labels, clusters];
    let out = fx
        .engine
        .viewport_artifacts(
            &session,
            ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, 1)
                .layers(LayerSelection::Named(&names)),
        )
        .unwrap();
    let keys: Vec<(String, String)> = out.frames[0]
        .artifacts
        .iter()
        .map(|a| (a.layer.clone(), a.key.clone().unwrap()))
        .collect();
    assert_eq!(
        keys,
        [
            (clusters.to_string(), "big".to_string()),
            (labels.to_string(), "on-big".to_string())
        ],
        "the cluster level first, then the one label whose cluster the tile holds"
    );
    let served = &out.frames[0].artifacts;
    assert_eq!(served[1].target, Some(served[0].tessera_id));
}
