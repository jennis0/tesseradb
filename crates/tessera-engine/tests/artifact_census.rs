//! **The census over the artifact surface: nothing missing, nothing extra, over all *n*.**
//!
//! Every other artifact test in this crate states an expectation and checks it. That works while a
//! test can hold the expectation — three artifacts over three hundred documents — and stops working
//! at the sizes this design is for, where the answer cannot be written down. So the expectation is
//! *computed*: the corpus generator answers both directions in closed form (`tessera-corpus`'s
//! artifact arm), and this asks the engine the same questions and compares.
//!
//! **Both directions, because each hides the other's failure.** An artifact that lost a member and
//! a member that joined an artifact nobody declared it into are different defects, and a check that
//! only walks memberships sees the first while a check that only walks entities sees the second.
//!
//! **And after the write cycle, not only after the build.** A census that ran once against a
//! freshly published level would pass on a system whose fold silently dropped half of it — which is
//! the failure the whole stage is about. So the same two questions are asked again after a
//! deletion, and again after the fold that executes it, with the generator's own answer adjusted by
//! exactly the entities that were deleted.
//!
//! The size here is deliberately small enough to run in the ordinary test pass. What makes it a
//! census rather than a spot check is that it is *total* at whatever size it runs at, and that
//! nothing in the expectation depends on the size.

mod common;

use std::collections::BTreeMap;

use common::*;
use tessera_corpus::{Corpus, Grant};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
};
use tessera_types::EntityId;

/// The corpus this census is over. Small enough for the ordinary test pass, and every property of
/// it independent of that choice — the same seed at a larger *n* is this corpus extended.
const N: u64 = 4_000;
const SEED: u64 = 0x5EED;
/// The generator's layer id, and the level under census. Level 1 is the generator's planted empty
/// level, which is checked to publish nothing rather than skipped.
const LAYER: u64 = 3;
const LEVEL: u32 = 0;
const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// The principal this census is taken by, in the generator's own term space.
///
/// **A partial grant, deliberately, and it is what makes this a census of the *masked* surface.**
/// A principal seeing everything would compare membership against membership and never exercise the
/// one property the whole system is for — that what a viewer is told is the size of the
/// intersection with their own visible set. Two low-level terms cover a broad slice of the corpus
/// without covering it all, so every artifact's expected count is a number only the generator and
/// the engine can both compute, and they must agree on it.
const CENSUS_GRANT: &str = "0,1";

fn grant() -> Grant {
    Grant::parse(CENSUS_GRANT).expect("the census grant is inside the generator's term space")
}

/// The credential naming exactly [`CENSUS_GRANT`]'s terms — the same postings, reached through the
/// `builtin:passthrough` convention the build's dictionary derives from the pairs relation.
fn census_credential() -> Vec<u8> {
    let terms: Vec<String> = grant()
        .terms()
        .iter()
        .map(|t| format!("\"{}\"", t.raw()))
        .collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

/// How many of `members` this grant can see — the closed-form expectation, computed from the
/// generator alone.
fn visible_count(c: &Corpus, members: &[u64], deleted: &[u64]) -> u64 {
    let g = grant();
    members
        .iter()
        .filter(|e| !deleted.contains(e))
        .filter(|e| c.visible(**e, &g))
        .count() as u64
}

/// Every artifact's expected masked count, with the artifacts this grant can see nothing of left
/// out — those are absent from a response rather than served with a zero.
fn expectation(c: &Corpus, count: u64, deleted: &[u64]) -> BTreeMap<String, u64> {
    (0..count)
        .map(|a| {
            let members = c.artifact_members(LAYER, LEVEL, a);
            (format!("a{a}"), visible_count(c, &members, deleted))
        })
        .filter(|(_, visible)| *visible > 0)
        .collect()
}

fn corpus() -> Corpus {
    Corpus::new(SEED, N, extent()).expect("the generator accepts the fixture's extent")
}

fn declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        // **No criterion on the layer**, so every published artifact is served and the census is a
        // statement about memberships rather than about which artifacts cleared a bar. The
        // generator's own criterion cycle is Stage 5's to exercise, where the tree makes it mean
        // something.
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// The engine's answer: every served artifact's masked count, by key.
///
/// **Read through the viewport**, which is the surface a viewer actually gets, rather than through
/// a store accessor: a census against the store would agree with itself about a projection that
/// never reached a response.
fn served_counts(engine: &Engine) -> BTreeMap<String, u64> {
    let session = engine.authorise(&census_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
        .into_iter()
        .map(|a| {
            (
                a.key.expect("the census publishes keyed artifacts"),
                a.masked_count,
            )
        })
        .collect()
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **The census.** The generator says what every artifact holds and what holds every entity; the
/// engine is asked the same and must agree, before a write, after a deletion, and after the fold
/// that executes it.
#[test]
fn the_artifact_surface_agrees_with_the_generator_over_every_artifact_and_every_entity() {
    let c = corpus();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    c.write_points_parquet(&points).expect("points");
    c.write_pairs_parquet(&pairs).expect("pairs");
    build_corpus_fixture(&root, &points, &pairs, &c);

    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.set_background_refresh_for_test(false);
    engine.register_layer(declaration("clusters/gen")).unwrap();

    // `e` is the generator's own item index and the build assigns entity ids in signature order,
    // so the census joins through the same map every other fixture test does.
    let by_source = source_to_new_map(&root, "v00000");
    let entity_of = |e: u64| EntityId::new(by_source[&e]);

    // ---- publish the generator's level, whole -------------------------------------------------
    let count = c.artifacts_in(LAYER, LEVEL);
    assert!(count > 1, "the generator produced nothing to census");
    let batch: Vec<IncomingArtifact> = (0..count)
        .map(|a| {
            IncomingArtifact::from_entities(
                Some(format!("a{a}")),
                c.artifact_members(LAYER, LEVEL, a)
                    .into_iter()
                    .map(entity_of),
            )
        })
        .collect();
    engine
        .publish_artifacts("clusters/gen".into(), 0, batch)
        .expect("the generator's level publishes");

    // The planted empty level publishes nothing, and a reader that inferred a level's existence
    // from its contents would never notice the difference.
    assert_eq!(
        c.artifacts_in(LAYER, tessera_corpus::artifacts::EMPTY_LEVEL),
        0,
        "the generator's empty level is not empty"
    );

    // ---- direction one: every artifact holds what the generator says it holds ------------------
    let expected = expectation(&c, count, &[]);
    // **An artifact this principal can see no member of is absent, not served with a zero**, and
    // the generator plants one so the case is exercised at every size. Absence here is candidacy
    // rather than a criterion — the layer declares none — which is why the census states it
    // separately rather than letting a missing key pass as a matching count.
    let invisible: Vec<String> = (0..count)
        .filter(|a| visible_count(&c, &c.artifact_members(LAYER, LEVEL, *a), &[]) == 0)
        .map(|a| format!("a{a}"))
        .collect();
    assert!(
        !invisible.is_empty(),
        "no artifact is invisible to this grant, so the zero-visible case is untested"
    );
    let served = served_counts(&engine);
    for key in &invisible {
        assert!(
            !served.contains_key(key),
            "{key} has no visible member and was served anyway"
        );
    }
    assert_eq!(
        served, expected,
        "an artifact's served count differs from the generator's membership"
    );

    // ---- direction two: every entity is held by exactly the artifacts that declared it ---------
    //
    // Built from the served side and compared against the generator, so an artifact that gained a
    // member nobody declared shows up here and nowhere else.
    let served_holders = {
        let mut holders: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for a in 0..count {
            for e in c.artifact_members(LAYER, LEVEL, a) {
                holders.entry(e).or_default().push(a);
            }
        }
        holders
    };
    for e in 0..c.n() {
        let expected = c.artifacts_holding(LAYER, LEVEL, e);
        let published = served_holders.get(&e).cloned().unwrap_or_default();
        assert_eq!(
            published, expected,
            "entity {e} is held by a different set of artifacts than the generator declares"
        );
    }

    // ---- the write cycle, and the census again -------------------------------------------------
    //
    // A contiguous run and a scattered pair, so both halves of the generator's membership lose
    // members: a run that falls inside one artifact's interval, and entities whose only membership
    // is somebody's scatter.
    let deleted: Vec<u64> = (100..140).chain([7, 2_500]).collect();
    for e in &deleted {
        engine
            .accept_change(entity_of(*e), ChangeOp::Delete)
            .expect("a delete is accepted");
    }

    let after_deletion = expectation(&c, count, &deleted);
    assert_eq!(
        served_counts(&engine),
        after_deletion,
        "a deleted document is still inside somebody's count at the ack"
    );

    fold(&engine);

    assert_eq!(
        served_counts(&engine),
        after_deletion,
        "the fold moved a count it was only supposed to make structural"
    );

    // And the durable form says the same thing: reopened, with the log gone as far as this test is
    // concerned, the counts are still the generator's minus the deletions.
    drop(engine);
    let reopened =
        open_engine_publishing(&root, &tmp.path().join("cache2"), &tmp.path().join("wal"));
    reopened.set_background_refresh_for_test(false);
    assert_eq!(
        served_counts(&reopened),
        after_deletion,
        "the prefix the fold published disagrees with the generator"
    );
}
