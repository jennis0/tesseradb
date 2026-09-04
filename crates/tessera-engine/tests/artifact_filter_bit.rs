//! **A filter moves the bit beside an artifact, and moves nothing else about it**
//! ([decision 0104](../../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)).
//!
//! Three things have to hold at once, and each fails in a way that looks like the feature working:
//!
//! - **The bit is right.** Checked against the generator's own closed forms — which entities are in
//!   an artifact, which of them the principal may see, and which carry the filtered value — and not
//!   against anything the engine said. An oracle written from the engine's own answer passes on a
//!   system where the probe reads the wrong set.
//! - **The bit is the *only* thing that moves.** A filtered request serves the same artifacts with
//!   the same masked counts as the unfiltered one (**I3**, **I12**): a filter that thinned the
//!   served set would make an artifact appear and disappear as a viewer typed, and one that moved
//!   the count would make the existence criterion a function of the filter.
//! - **The routes agree.** The same partition relation is declared twice — stored member lists
//!   (`generator/partition-enumerated`) and an attribute predicate
//!   (`generator/partition-attribute`) — so the two are the same relation reached by different
//!   machinery, and the probe is asked in a different shape on each. They must give the same bit for
//!   the same key at every viewport and for every principal. The filter's own crossing route also
//!   changes with the viewport, which is what the narrow boxes here are for: the bit's meaning must
//!   not depend on which one the engine took.
//!
//! **Engine-level rather than over HTTP**, for `artifact_predicate.rs`'s reason: the subject is what
//! is computed inside the trust boundary, and the wire is tested where the wire is the subject.

mod common;

use std::collections::BTreeMap;

use common::*;
use tessera_corpus::materialise::PARTITION_LAYER;
use tessera_corpus::{Corpus, Grant, BAY_VALUES};
use tessera_engine::filter::{FilterExpr, FilterOperand};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::membership::{IncomingAttachment, IncomingContent};
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
    SuppliedContent,
};
use tessera_types::{AttrLocalId, EntityId};

const N: u64 = 3_000;
const SEED: u64 = 0x5EED;

const BY_LIST: &str = "generator/partition-enumerated";
const BY_RULE: &str = "generator/partition-attribute";

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
/// The depth the narrow viewports are asked at — at zoom 0 a bbox resolves to the one tile covering
/// the grid and narrows nothing.
const VIEWPORT_ZOOM: u8 = 4;

/// The value the oracle filters on. Any of the seven does; this one is named once so the closed
/// form below and the request cannot drift apart.
const BAY: &str = "cedar";

/// **The vocabulary's code for a `bay` key, pinned by the generator's declaration** — key *i* of
/// [`BAY_VALUES`] is code *i + 1*, code 0 being the reserved *absent* sentinel. Pinned rather than
/// assigned, which is what lets a test name a code at all.
fn bay_code(key: &str) -> AttrLocalId {
    let i = BAY_VALUES
        .iter()
        .position(|v| *v == key)
        .expect("the fixture filters on a value the generator plants");
    AttrLocalId::new(i as u32 + 1)
}

fn bay_is(key: &str) -> FilterExpr {
    FilterExpr::Leaf {
        column: "bay".into(),
        operand: FilterOperand::Equals(bay_code(key)),
    }
}

/// A filter nothing carries: the generator's tags are `kw-<5 hex digits>`, so this is a well-formed
/// predicate over a real column with an empty answer — the case the bit must report as *no matches*
/// rather than as *no filter*.
fn tag_is_absent() -> FilterExpr {
    FilterExpr::Leaf {
        column: "tag".into(),
        operand: FilterOperand::TextEquals("kw-nothing".into()),
    }
}

fn corpus() -> Corpus {
    Corpus::new(SEED, N, extent()).expect("the generator accepts the fixture's extent")
}

fn credential(grant: &str) -> Vec<u8> {
    let terms: Vec<String> = Grant::parse(grant)
        .expect("the grant is inside the generator's term space")
        .terms()
        .iter()
        .map(|t| format!("\"{}\"", t.raw()))
        .collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

/// The principals this is checked for. A viewer seeing everything would compare two unmasked
/// answers and never exercise the masking the bit is computed inside.
fn grants() -> Vec<&'static str> {
    vec!["0", "0,1", "1,2,3", "5,6,7,8"]
}

fn viewports() -> Vec<(u8, [f64; 4])> {
    vec![
        (0, WHOLE_MAP),
        (VIEWPORT_ZOOM, WHOLE_MAP),
        (VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]),
        (VIEWPORT_ZOOM, [250.0, 250.0, 750.0, 750.0]),
        (VIEWPORT_ZOOM, [900.0, 900.0, 1000.0, 1000.0]),
    ]
}

/// What one layer serves one principal at one viewport: key against masked count and the two bits.
fn served(
    engine: &Engine,
    grant: &str,
    layer: &str,
    zoom: u8,
    bbox: [f64; 4],
    filter: Option<FilterExpr>,
) -> BTreeMap<String, (u64, Option<bool>)> {
    lit(engine, grant, layer, zoom, bbox, filter, None)
        .into_iter()
        .map(|(key, (count, matched, _))| (key, (count, matched)))
        .collect()
}

/// The same, with a `highlight` beside the filter: key against masked count, `matched` and
/// `highlighted` (`highlight-and-hierarchy.md` §2).
fn lit(
    engine: &Engine,
    grant: &str,
    layer: &str,
    zoom: u8,
    bbox: [f64; 4],
    filter: Option<FilterExpr>,
    highlight: Option<FilterExpr>,
) -> BTreeMap<String, (u64, Option<bool>, Option<bool>)> {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [layer];
    let mut request = ViewportRequest::new("s0", zoom, bbox, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    request.filter = filter;
    request.highlight = highlight;
    engine
        .viewport(&session, request)
        .expect("a viewport over the fixture")
        .artifacts
        .into_iter()
        .map(|artifact| {
            (
                artifact.key.expect("both layers carry the value's key"),
                (artifact.masked_count, artifact.matched, artifact.highlighted),
            )
        })
        .collect()
}

#[allow(dead_code)]
struct Fixture {
    _tmp: tempfile::TempDir,
    engine: Engine,
    corpus: Corpus,
    root: std::path::PathBuf,
}

impl Fixture {
    /// The engine's entity ids for a run of the generator's own source ids.
    fn members(&self, sources: impl Iterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        sources.map(|s| EntityId::new(map[&s])).collect()
    }
}

fn fixture() -> Fixture {
    let corpus = corpus();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    corpus.write_points_parquet(&points).expect("points");
    corpus.write_pairs_parquet(&pairs).expect("pairs");
    build_corpus_fixture_with_layers(&root, &points, &pairs, &corpus);

    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.set_background_refresh_for_test(false);
    Fixture {
        _tmp: tmp,
        engine,
        corpus,
        root,
    }
}

/// **The oracle**: which partition artifacts hold a member this grant may see that carries `BAY`,
/// computed from the generator's closed forms alone.
///
/// Only sound over the whole map, where every row is in view — the bit is scoped to the request's
/// tiles, so a narrower box is a different question and is checked by agreement instead.
fn expected_matched(corpus: &Corpus, grant: &str) -> BTreeMap<String, bool> {
    let grant = Grant::parse(grant).expect("the grant parses");
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for e in 0..N {
        let a = corpus.partition_artifact_of(PARTITION_LAYER, e);
        let hit = corpus.visible(e, &grant) && corpus.item(e).bay == Some(BAY);
        let slot = out.entry(a.to_string()).or_insert(false);
        *slot = *slot || hit;
    }
    out
}

/// **The headline.** Over the whole map, the bit beside every served artifact is exactly what the
/// generator says — for two membership routes and four principals.
#[test]
fn the_bit_is_what_the_corpus_says_for_this_principal() {
    let fx = fixture();
    for grant in grants() {
        let oracle = expected_matched(&fx.corpus, grant);
        for layer in [BY_LIST, BY_RULE] {
            let got = served(&fx.engine, grant, layer, 0, WHOLE_MAP, Some(bay_is(BAY)));
            assert!(
                got.len() > 4,
                "{layer} served {} artifacts to {grant}, too few for this to mean anything",
                got.len()
            );
            // Both answers are non-trivial: a bit that was always true, or always false, would
            // satisfy an equality against an oracle that happened to be constant too.
            assert!(
                got.values().any(|(_, m)| *m == Some(true)),
                "{layer}/{grant}: no artifact matched, so the check below is vacuous"
            );
            assert!(
                got.values().any(|(_, m)| *m == Some(false)),
                "{layer}/{grant}: every artifact matched, so the check below is vacuous"
            );
            for (key, (count, matched)) in &got {
                assert_eq!(
                    *matched,
                    Some(oracle[key]),
                    "{layer}/{grant}: artifact {key} (masked count {count}) carries the wrong bit"
                );
            }
        }
    }
}

/// **A filter moves the bit and nothing else** — the same artifacts, with the same masked counts,
/// filtered and unfiltered. This is I3 and I12 at the artifact: a filter may not thin the served
/// set and may not move the number beside it.
#[test]
fn a_filter_moves_neither_the_served_set_nor_the_masked_count() {
    let fx = fixture();
    for grant in grants() {
        for layer in [BY_LIST, BY_RULE] {
            for (zoom, bbox) in viewports() {
                let plain = served(&fx.engine, grant, layer, zoom, bbox, None);
                let filtered = served(&fx.engine, grant, layer, zoom, bbox, Some(bay_is(BAY)));
                let empty = served(&fx.engine, grant, layer, zoom, bbox, Some(tag_is_absent()));
                let where_ = format!("{layer}/{grant} at zoom {zoom} over {bbox:?}");

                let counts = |m: &BTreeMap<String, (u64, Option<bool>)>| -> BTreeMap<String, u64> {
                    m.iter().map(|(k, (c, _))| (k.clone(), *c)).collect()
                };
                assert_eq!(
                    counts(&plain),
                    counts(&filtered),
                    "{where_}: the filter changed which artifacts were served, or their counts"
                );
                assert_eq!(
                    counts(&plain),
                    counts(&empty),
                    "{where_}: a filter matching nothing still serves every artifact, unchanged"
                );

                // **Absent is not false.** An unfiltered request asked no question.
                assert!(
                    plain.values().all(|(_, m)| m.is_none()),
                    "{where_}: an unfiltered response carried a bit"
                );
                assert!(
                    empty.values().all(|(_, m)| *m == Some(false)),
                    "{where_}: a filter matching nothing must answer false, not absent"
                );
            }
        }
    }
}

/// **The two membership routes agree, at every viewport.** One relation, stored and derived, over
/// boxes that also move the filter's own crossing route: the bit must be a property of the question
/// and not of the machinery that answered it.
#[test]
fn the_stored_and_derived_routes_carry_the_same_bit() {
    let fx = fixture();
    for grant in grants() {
        for (zoom, bbox) in viewports() {
            let by_list = served(&fx.engine, grant, BY_LIST, zoom, bbox, Some(bay_is(BAY)));
            let by_rule = served(&fx.engine, grant, BY_RULE, zoom, bbox, Some(bay_is(BAY)));
            assert_eq!(
                by_list, by_rule,
                "{grant} at zoom {zoom} over {bbox:?}: the two routes disagree"
            );
        }
    }
}

/// **The bit answers about the members in view**, where the masked count answers about the whole
/// visible membership — the difference decision 0104 takes deliberately, asserted so that a later
/// change to whole-membership is a failing test rather than a silent widening.
///
/// The case: an artifact matched over the whole map whose matching members all lie outside a
/// narrower box, which is served there — same identity, same count, and a bit that has turned off.
#[test]
fn the_bit_is_scoped_to_the_requested_tiles() {
    let fx = fixture();
    let grant = "0,1";
    let whole = served(&fx.engine, grant, BY_LIST, 0, WHOLE_MAP, Some(bay_is(BAY)));
    let mut found = false;
    for (zoom, bbox) in viewports() {
        if zoom == 0 {
            continue;
        }
        for (key, (_, matched)) in served(&fx.engine, grant, BY_LIST, zoom, bbox, Some(bay_is(BAY)))
        {
            let (_, over_map) = whole[&key];
            assert!(
                !(matched == Some(true) && over_map == Some(false)),
                "artifact {key} matched inside a box and not over the whole map, which is not a \
                 narrowing at all"
            );
            found |= matched == Some(false) && over_map == Some(true);
        }
    }
    assert!(
        found,
        "no artifact was served in a narrow box with its matches outside it — the fixture no \
         longer exercises the scoping, so this test proves nothing"
    );
}

// ---------------------------------------------------------------------------------------------
// A label carries its target's bit, on the same argument its count does (D13, decision 0104).
// ---------------------------------------------------------------------------------------------

const CLUSTERS: &str = "case/clusters";
const LABELS: &str = "case/labels";

fn clusters() -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: CLUSTERS.into(),
        title: Some("clusters".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// The label layer. **Its own membership is empty**, which is the case the rule exists for: a bit
/// over nothing is `false` under every filter, and that is what a label would carry without it.
fn labels() -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: LABELS.into(),
        title: Some("labels".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "topic".into(),
                ty: "text".into(),
                // `inherited`: the label's text serves on the container's own gate, so this case
                // is about the bit rather than about containment — which has its own file.
                require_member_visibility: tessera_types::layer::SuppliedRequirement::Inherited,
            }],
            withdraw_on_member_deletion: true,
        },
        depends_on: vec![CLUSTERS.into()],
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// **A label's bits are its cluster's** — D13's rule for the count, applied to the two fields
/// beside it (decision 0104; `highlight-and-hierarchy.md` §2 for the second).
///
/// The case is a label whose own membership would answer differently: `c-hit` holds members
/// carrying `BAY` and its label holds only members that do not, so a bit computed over the label's
/// own membership reads `false` beside a cluster reading `true`. A label describes its cluster, so
/// *does anything here match* is the cluster's question — the same reason its count is the
/// cluster's.
///
/// **`highlighted` is asserted beside `matched` and against it**, because the two are one answer
/// under two expressions: the same clause is sent in `filters` and in `highlight`, and the label's
/// two bits must agree with each other and with its cluster's. That pairing is what fails when
/// only one of them inherits — which is a defect no assertion about `matched` alone can see, the
/// label's own answer being a well-formed `false`.
#[test]
fn a_label_carries_its_targets_bits() {
    let fx = fixture();
    fx.engine.register_layer(clusters()).unwrap();
    fx.engine.register_layer(labels()).unwrap();

    // Sorted by the generator's own answer, so each cluster is a pure case of one bit.
    let (with_bay, without): (Vec<u64>, Vec<u64>) =
        (0..N).partition(|e| fx.corpus.item(*e).bay == Some(BAY));
    assert!(
        with_bay.len() > 10 && without.len() > 10,
        "the generator must plant both cases"
    );
    // The label's own membership, in both cases: members carrying no `BAY` at all. An artifact with
    // no members is never a candidate anywhere, so a label has some; what makes the case is that
    // they are the wrong ones to answer the cluster's question from.
    let label_members: Vec<u64> = without.iter().copied().take(10).collect();

    for (key, sources) in [("hit", &with_bay), ("miss", &without)] {
        fx.engine
            .publish_artifacts(
                CLUSTERS.into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some(key.into()),
                    fx.members(sources.iter().copied()),
                )],
            )
            .unwrap();
        fx.engine
            .publish_artifacts(
                LABELS.into(),
                0,
                vec![IncomingArtifact::attached(
                    Some(format!("label-{key}")),
                    fx.members(label_members.iter().copied()),
                    // No generating set: `inherited` content must not carry one, a set that is
                    // never tested being a claim the service would hold without meaning (C28).
                    vec![IncomingContent::new(vec![key.into()], Vec::new())],
                    IncomingAttachment {
                        layer: CLUSTERS.into(),
                        level: 0,
                        key: key.into(),
                    },
                )],
            )
            .unwrap();
    }

    // The whole corpus, so every cluster is served and the partition above is the oracle. **Both
    // layers in one request**, which is what a layer picker offering the closure sends
    // (decision 0096) and what puts the target in the response for its dependent to read.
    let grant = "0,1,2,3,4,5,6,7,8";
    type Row = (u64, Option<bool>, Option<bool>);
    let both = |filter: Option<FilterExpr>,
                highlight: Option<FilterExpr>|
     -> BTreeMap<(String, String), Row> {
        let session = fx.engine.authorise(&credential(grant)).unwrap();
        let names = [CLUSTERS, LABELS];
        let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize);
        request.layers = tessera_engine::LayerSelection::Named(&names);
        request.filter = filter;
        request.highlight = highlight;
        fx.engine
            .viewport(&session, request)
            .expect("a viewport over the fixture")
            .artifacts
            .into_iter()
            .map(|a| {
                (
                    (a.layer, a.key.expect("both layers publish keyed artifacts")),
                    (a.masked_count, a.matched, a.highlighted),
                )
            })
            .collect()
    };
    // **One clause in both fields**, so the two answers about one cluster are comparable: a label
    // that inherited one bit and kept its own for the other would disagree with itself here.
    let filtered = both(Some(bay_is(BAY)), Some(bay_is(BAY)));
    let at = |layer: &str, key: &str| filtered[&(layer.to_string(), key.to_string())];
    assert_eq!(at(CLUSTERS, "hit").1, Some(true));
    assert_eq!(at(CLUSTERS, "miss").1, Some(false));
    assert_eq!(
        at(LABELS, "label-hit").1,
        Some(true),
        "a label whose own members carry none of the value must still answer for its cluster"
    );
    assert_eq!(at(LABELS, "label-miss").1, Some(false));
    for (layer, key) in [
        (CLUSTERS, "hit"),
        (CLUSTERS, "miss"),
        (LABELS, "label-hit"),
        (LABELS, "label-miss"),
    ] {
        let (_, matched, highlighted) = at(layer, key);
        assert_eq!(
            highlighted, matched,
            "{layer}/{key}: one clause in both fields is one answer, and the label's must be its \
             cluster's in the second field exactly as in the first"
        );
    }
    // The count rule and the bit rules agree about which artifact is being described.
    assert_eq!(at(LABELS, "label-hit").0, at(CLUSTERS, "hit").0);
    assert_eq!(at(LABELS, "label-miss").0, at(CLUSTERS, "miss").0);

    // A highlight with **no** filter beside it: the label still answers for its cluster, and
    // `matched` is null because no filter was asked — the two fields are independent questions.
    let lit = both(None, Some(bay_is(BAY)));
    let lit_at = |layer: &str, key: &str| lit[&(layer.to_string(), key.to_string())];
    assert_eq!(lit_at(LABELS, "label-hit").2, Some(true));
    assert_eq!(lit_at(LABELS, "label-miss").2, Some(false));
    assert!(lit.values().all(|(_, matched, _)| matched.is_none()));

    // And with neither there is still no question, on a label as on anything else.
    assert!(both(None, None)
        .values()
        .all(|(_, matched, highlighted)| matched.is_none() && highlighted.is_none()));
}

// ---------------------------------------------------------------------------------------------
// I12's third conjunct: containment is what it is, filter or no filter.
// ---------------------------------------------------------------------------------------------

const DESCRIBED: &str = "case/described";

/// A layer carrying **corpus-derived** supplied content: `All`, so a served description is one
/// whose generating set the viewer holds entire.
///
/// The distinction this layer exists to make is between a layer's content *schema* and a published
/// artifact's `generated_from`. Every other artifact reachable under a filter in this repository
/// carries an empty generating set — inherited content must not carry one (C28) — so containment
/// answers `NothingToContain` and the conjunct is never evaluated at all.
fn described() -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: DESCRIBED.into(),
        title: Some("described".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "topic".into(),
                ty: "text".into(),
                require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
            }],
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// What the described layer serves one principal over the whole map: key against masked count and
/// the description itself, which is the served form of the containment verdict.
fn described_served(
    engine: &Engine,
    grant: &str,
    filter: Option<FilterExpr>,
) -> BTreeMap<String, (u64, Vec<String>)> {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [DESCRIBED];
    let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    request.filter = filter;
    engine
        .viewport(&session, request)
        .expect("a viewport over the fixture")
        .artifacts
        .into_iter()
        .map(|a| {
            (
                a.key.expect("this layer publishes keyed artifacts"),
                (a.masked_count, a.content),
            )
        })
        .collect()
}

/// **I12's third conjunct, at the one place it can be evaluated.** An artifact's existence verdict,
/// its masked count *and its containment* are identical with and without a filter, because all
/// three run against `M_auth` alone.
///
/// The fixture is what makes the claim testable: one artifact per principal, each carrying a
/// generating set drawn **across** the filtered value — half its members carry `BAY` and half do
/// not — and each drawn inside that principal's own coverage, so the description is served
/// unfiltered and there is something for a filter to take away.
///
/// **Mutations this kills:** composing the filter into the mask handed to
/// `ArtifactRows::satisfied_rank` — containment then fails for a viewer whose filter excludes a
/// generating-set member, and the artifact and its description appear and disappear as that viewer
/// types. Fail-closed rather than fail-open, and still a filter deciding what `M_auth` decides.
#[test]
fn a_filter_moves_neither_containment_nor_the_description_it_serves() {
    let fx = fixture();
    fx.engine.register_layer(described()).unwrap();

    for grant in grants() {
        let parsed = Grant::parse(grant).expect("the grant parses");
        let members: Vec<u64> = (0..N)
            .filter(|e| fx.corpus.visible(*e, &parsed))
            .take(200)
            .collect();
        let (with_bay, without): (Vec<u64>, Vec<u64>) = members
            .iter()
            .copied()
            .partition(|e| fx.corpus.item(*e).bay == Some(BAY));
        assert!(
            with_bay.len() >= 5 && without.len() >= 5,
            "{grant}: the generating set must be drawn across the filtered value — a set every \
             member of which the filter admits could not tell a filtered containment from an \
             unfiltered one, and this test would prove nothing"
        );
        let generating: Vec<u64> = with_bay
            .iter()
            .take(5)
            .chain(without.iter().take(5))
            .copied()
            .collect();
        fx.engine
            .publish_artifacts(
                DESCRIBED.into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some(format!("g{}", grant.replace(',', "-"))),
                    fx.members(members.iter().copied()),
                    vec![IncomingContent::new(
                        vec![format!("generated from {grant}")],
                        fx.members(generating.iter().copied()),
                    )],
                )],
            )
            .expect("a described artifact publishes");
    }

    for grant in grants() {
        let plain = described_served(&fx.engine, grant, None);
        let filtered = described_served(&fx.engine, grant, Some(bay_is(BAY)));
        let empty = described_served(&fx.engine, grant, Some(tag_is_absent()));

        assert!(
            plain.values().any(|(_, content)| !content.is_empty()),
            "{grant} is served no description at all unfiltered, so the comparisons below are \
             vacuous"
        );
        assert_eq!(
            plain, filtered,
            "{grant}: a filter moved which artifacts are served, their counts, or which \
             description they carry — containment is a question about `M_auth` and a filter is \
             not part of it"
        );
        assert_eq!(
            plain, empty,
            "{grant}: a filter matching nothing still serves every artifact with its description \
             unchanged"
        );
    }
}

/// **The `highlighted` bit is this bit under a second expression** — decision 0104's probe with
/// the highlight's crossed set in place of the filter's (`highlight-and-hierarchy.md` §2).
///
/// Three things at once, each of which fails looking like the feature working:
///
/// - **It is the conjunction's bit.** A request carrying `filters = A` and `highlight = B` must
///   report, per artifact, exactly the `matched` a request carrying `all_of[A, B]` in `filters`
///   reports. That is C32's whole argument — a highlight discloses what one such request would
///   have disclosed, differently arranged — so an implementation that answered `B` alone would
///   satisfy every other check here and make the register row wrong.
/// - **`null` is *there was no question*.** A request with no highlight leaves the column null on
///   every row, including the rows whose `matched` is `false`; a `false` there would answer a
///   question that was never asked, and no client could tell it from a highlight matching nothing.
/// - **It moves nothing else.** The served set, the masked counts and `matched` itself are
///   identical with and without a highlight (**I3**, **I12**), exactly as they are with and
///   without a filter.
#[test]
fn the_highlighted_bit_is_the_conjunctions_bit_and_moves_nothing_else() {
    let fx = fixture();
    // The far-corner viewport serves no artifact to some principals, which makes the checks below
    // vacuous there rather than wrong; this counts the rows actually compared so a fixture that
    // stopped serving anything cannot pass silently.
    let mut compared = 0usize;
    for grant in grants() {
        for layer in [BY_LIST, BY_RULE] {
            for (zoom, bbox) in viewports() {
                // No highlight: the column is null on every row, whatever `matched` says.
                let plain = lit(&fx.engine, grant, layer, zoom, bbox, Some(bay_is(BAY)), None);
                assert!(
                    plain.values().all(|(_, _, h)| h.is_none()),
                    "{layer}/{grant}: no highlight, and yet a bit"
                );

                let a = bay_is(BAY);
                let b = bay_is("dune");
                let lit_rows = lit(
                    &fx.engine,
                    grant,
                    layer,
                    zoom,
                    bbox,
                    Some(a.clone()),
                    Some(b.clone()),
                );
                let conjoined = served(
                    &fx.engine,
                    grant,
                    layer,
                    zoom,
                    bbox,
                    Some(FilterExpr::AllOf(vec![a.clone(), b.clone()])),
                );
                let what = format!("{layer}/{grant} at zoom {zoom} over {bbox:?}");
                for (key, (count, matched, highlighted)) in &lit_rows {
                    assert_eq!(
                        (*count, *matched),
                        (plain[key].0, plain[key].1),
                        "{what}: the highlight moved artifact {key}'s count or its filter bit"
                    );
                    assert_eq!(
                        *highlighted,
                        conjoined[key].1,
                        "{what}: artifact {key}'s highlight bit is not the conjunction's"
                    );
                }
                assert_eq!(
                    lit_rows.len(),
                    conjoined.len(),
                    "{what}: the two requests served different artifacts"
                );

                // A highlight matching nothing is `false` everywhere, and still not `null`: the
                // question was asked and the answer is no.
                let empty = lit(
                    &fx.engine,
                    grant,
                    layer,
                    zoom,
                    bbox,
                    None,
                    Some(tag_is_absent()),
                );
                assert!(
                    empty.values().all(|(_, _, h)| *h == Some(false)),
                    "{what}: a highlight matching nothing is false, never null"
                );
                compared += lit_rows.len();
            }
        }
    }
    assert!(
        compared > 100,
        "only {compared} artifact rows were compared, too few for this to mean anything"
    );
}
