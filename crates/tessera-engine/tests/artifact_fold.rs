//! **The fold's artifact pass**: a fold rewrites every membership into the prefix it publishes, and
//! what a deletion took away does not come back at the flip.
//!
//! Row space renumbers globally at a fold and membership extent paths are prefix-relative, so a
//! fold that did nothing here would produce one of two failures, both silent: a bundle naming
//! extents the new prefix does not contain, or artifacts that come back registered, addressable and
//! served as absent. Both look like a clustering that failed its existence criterion.
//!
//! The distinction these cases exist to hold is the one this corpus has caught twice: **a deletion
//! retires at the fold that executes it, and a suppression retires only on unsuppress.** A pass that
//! dropped a suppressed member's bit while it was at it would give a suppression a second
//! retirement route, and the member would not come back at the unsuppress — fail-open, and
//! indistinguishable from a cluster that had always been that size.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::membership::IncomingVariation;
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerAccess, LayerDeclaration, MembershipSource,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

fn declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        name: name.into(),
        title: format!("{name} (title)"),
        slices: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        access: LayerAccess {
            label: None,
            artifacts_carry_own: false,
        },
        // **No criterion**, deliberately: these cases are about what the membership *is* after a
        // fold, and a criterion would turn a wrong count into an absence, which is a weaker
        // assertion than a wrong number.
        visible_when: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            derived: vec!["centroid".into()],
            supplied: Vec::new(),
            on_member_deletion: Default::default(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    fn members(&self, source_ids: impl Iterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }

    /// The corpus entity behind one source id — what a delete or a suppress is addressed to.
    fn member(&self, source_id: u64) -> EntityId {
        self.members(std::iter::once(source_id))[0]
    }

    /// The prefix directory the bundle currently serves.
    fn live_prefix(&self, engine: &Engine) -> std::path::PathBuf {
        self.root.join(&engine.generation().prefix)
    }

    fn membership_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        let dir = self
            .live_prefix(engine)
            .join("partitions")
            .join("default")
            .join("members");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tsmb"))
            .collect()
    }
}

fn artifacts_of(engine: &Engine) -> Vec<ArtifactOut> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// The one artifact's masked count, for a principal who can see everything — so the number is the
/// membership's own size and any movement in it is the pass's doing.
fn count(engine: &Engine) -> u64 {
    let artifacts = artifacts_of(engine);
    assert_eq!(artifacts.len(), 1, "the fixture publishes exactly one artifact");
    artifacts[0].masked_count
}

/// Request a fold and block until it has published, asserting it was not discarded.
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

/// Wait until the executor has written the memberships of everything published so far.
fn wait_for_publication(fx: &Fixture, engine: &Engine, files: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fx.membership_files(engine).len() < files {
        assert!(
            std::time::Instant::now() < deadline,
            "the membership extents were never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Publish one artifact over `sources` and wait for it to be durable.
fn publish(fx: &Fixture, engine: &Engine, sources: std::ops::Range<u64>) -> tessera_types::TesseraId {
    engine.register_layer(declaration("clusters/a")).unwrap();
    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources),
            )],
        )
        .unwrap();
    wait_for_publication(fx, engine, 1);
    ids[0]
}

/// Invert an artifact's identifier through the admin plane's own resolver — the route
/// `/control/changes` takes, so a deletion here goes through the misdirection guard rather than
/// round it.
fn artifact_entity(engine: &Engine, id: tessera_types::TesseraId) -> EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0].expect("it names what was issued")
}

/// **A node holding artifacts folds at all** — which it did not until the pass existed, because the
/// alternative to rewriting the extents was refusing the publication outright.
///
/// And what it publishes is a *new* file under the *new* prefix: the paths are prefix-relative, so
/// a fold that carried the manifest entries forward would name files nothing contains and the
/// bundle would refuse at its next open.
#[test]
fn a_fold_rewrites_the_memberships_into_the_prefix_it_publishes() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    let before = fx.live_prefix(&engine);

    fold(&engine);

    let after = fx.live_prefix(&engine);
    assert_ne!(before, after, "the fold published a new prefix");
    let files = fx.membership_files(&engine);
    assert_eq!(
        files.len(),
        1,
        "one extent per level, and it is under the prefix the fold published"
    );
    assert!(
        files[0].starts_with(&after),
        "the extent the manifest names lives in the new prefix"
    );
    assert_eq!(count(&engine), 300, "and it holds what it held");
}

/// The whole point of rewriting rather than carrying forward: **what a fold retires leaves the
/// membership, and what a suppression hid does not.**
///
/// Deleting a member and suppressing another moves the served count by two, both at the ack. What
/// the fold changes is which of those is *structural*: the deletion is executed and its bit goes,
/// so the count stays down; the suppression retires only on unsuppress, so its bit is still there
/// and the member comes back.
#[test]
fn a_deleted_member_is_gone_after_the_fold_and_a_suppressed_one_comes_back() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    assert_eq!(count(&engine), 300);

    let deleted = fx.member(7);
    let suppressed = fx.member(11);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("the delete is accepted");
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("the suppress is accepted");
    assert_eq!(
        count(&engine),
        298,
        "both leave every masked count at the ack, before any fold"
    );

    fold(&engine);
    assert_eq!(
        count(&engine),
        298,
        "the fold changes what is stored, never what is served"
    );

    engine
        .accept_change(suppressed, ChangeOp::Unsuppress)
        .expect("the unsuppress is accepted");
    assert_eq!(
        count(&engine),
        299,
        "the suppressed member returns — its bit was never dropped (Rule S) — and the deleted one \
         does not, which is the pass working rather than a copy"
    );
}

/// **The stage's regression test**, one level up from the counts: the membership the fold wrote is
/// what a restart reads, so a member the fold retired is gone from the durable form and not merely
/// from a resident one.
#[test]
fn what_the_fold_retired_is_gone_from_the_prefix_a_restart_opens() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        engine
            .accept_change(fx.member(7), ChangeOp::Delete)
            .expect("the delete is accepted");
        fold(&engine);
        assert_eq!(count(&engine), 299);
    }

    let engine = fx.open();
    assert_eq!(
        engine.published_artifacts(),
        1,
        "the artifact came back from the prefix the fold published"
    );
    assert_eq!(
        count(&engine),
        299,
        "and it came back without the member the fold retired"
    );
}

/// A second fold over an already-folded prefix is the case that catches a rewrite which reads its
/// input from the manifest rather than from the store: the first fold collapses every extent into
/// one, and a second finds a state the first never saw.
#[test]
fn a_second_fold_rewrites_what_the_first_one_wrote() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    fold(&engine);
    let after_first = fx.live_prefix(&engine);

    fold(&engine);

    let after_second = fx.live_prefix(&engine);
    assert_ne!(after_first, after_second);
    assert_eq!(fx.membership_files(&engine).len(), 1);
    assert_eq!(count(&engine), 300);
}

// ---- Rule F's artifact arm ---------------------------------------------------------------------

/// The cluster layer a label hangs from, and the label layer itself. Both ungated, so every
/// withholding below comes from the arm under test rather than from an access label.
fn labels_over(target: &str) -> LayerDeclaration {
    let mut d = declaration("topics/x");
    d.content.supplied = vec![tessera_types::layer::SuppliedContent {
        kind: "label_text".into(),
        corpus_derived: false,
    }];
    d.depends_on = vec![target.into()];
    d
}

/// **A deleted artifact leaves its level at the fold that executes the deletion**, and the ordinal
/// it held becomes a hole rather than closing up.
///
/// The hole is the whole of the durable state: an ordinal is identity, so packing around the gap
/// would hand every later artifact in the level the identity of its neighbour, and every
/// `tessera_id` a caller holds beyond it would resolve to the wrong cluster.
#[test]
fn a_deleted_artifact_leaves_the_level_at_the_fold_and_its_ordinal_stays_a_hole() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();
    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![
                IncomingArtifact::from_entities(Some("c0".into()), fx.members(0..100)),
                IncomingArtifact::from_entities(Some("c1".into()), fx.members(100..200)),
                IncomingArtifact::from_entities(Some("c2".into()), fx.members(200..300)),
            ],
        )
        .unwrap();
    wait_for_publication(&fx, &engine, 1);
    assert_eq!(artifacts_of(&engine).len(), 3);

    // The middle one, so a level that packed around the gap would be caught by the survivor after
    // it rather than by a count alone.
    let deleted = artifact_entity(&engine, ids[1]);
    let last_before = artifact_entity(&engine, ids[2]);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("an artifact takes a deletion like any other entity");
    assert_eq!(artifacts_of(&engine).len(), 2, "hidden at the ack");

    fold(&engine);

    assert_eq!(artifacts_of(&engine).len(), 2, "and still hidden after it");
    assert_eq!(engine.published_artifacts(), 2, "the level holds two records");
    let survivor = engine
        .locate_artifact(last_before)
        .expect("the survivor still has an address");
    assert_eq!(
        (survivor.ordinal, survivor.stable_key.as_deref()),
        (2, Some("c2")),
        "the survivor after the hole keeps its ordinal and its key — its identity did not shift up"
    );
    // The **address** still resolves, because it is the layer's reserved run that answers it and a
    // run is not per artifact. What is gone is the record at that ordinal, which is the hole.
    assert_eq!(
        engine
            .locate_artifact(deleted)
            .and_then(|at| at.stable_key),
        None,
        "and the deleted artifact's slot holds nothing"
    );
}

/// **Retirement's precondition, which is what makes the arm a safety property rather than
/// reclamation.** A deleted artifact has no rows and no postings, so compaction's derivation calls
/// its deletion executed *vacuously* at the first fold and retires the overlay entry — the only
/// thing hiding it. If the slot outlived that entry, the artifact would come back **served**.
#[test]
fn a_deleted_artifact_does_not_return_when_its_overlay_entry_retires() {
    let fx = fixture();
    {
        let engine = fx.open();
        let id = publish(&fx, &engine, 0..300);
        let entity = artifact_entity(&engine, id);
        engine
            .accept_change(entity, ChangeOp::Delete)
            .expect("the delete is accepted");
        fold(&engine);
        assert!(artifacts_of(&engine).is_empty());
    }

    // **Reopened, which is where a surviving slot would show.** The overlay entry is retired and
    // gone from the manifest; nothing but the absence of the record keeps the artifact away.
    let engine = fx.open();
    assert!(
        artifacts_of(&engine).is_empty(),
        "the artifact stayed gone across the retirement of the entry that was hiding it"
    );
    assert_eq!(
        engine.published_artifacts(),
        0,
        "and its slot is not in the prefix the fold published"
    );
}

/// **A label does not outlive what it labels — including past the fold that retires its target.**
///
/// The label is withheld at the ack because the cluster's overlay entry says *deleted*. That entry
/// is retired by the fold, so a term resting on disposition alone would answer "not deleted, not
/// suppressed" afterwards and serve the label again — with its text, describing the cluster that
/// was deleted. What carries the withholding is that the target no longer resolves.
#[test]
fn a_label_stays_withheld_after_the_fold_that_retired_its_cluster() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();
    engine.register_layer(labels_over("clusters/a")).unwrap();
    let cluster_id = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..300),
            )],
        )
        .unwrap()[0];
    engine
        .publish_artifacts(
            "topics/x".into(),
            0,
            vec![IncomingArtifact::attached(
                Some("l0".into()),
                fx.members(0..300),
                vec![IncomingVariation::new(
                    vec!["shipping and logistics".into()],
                    Vec::new(),
                )],
                tessera_lifecycle::membership::IncomingAttachment {
                    layer: "clusters/a".into(),
                    level: 0,
                    stable_key: "c0".into(),
                },
            )],
        )
        .unwrap();
    wait_for_publication(&fx, &engine, 2);
    assert_eq!(
        artifacts_of(&engine).len(),
        2,
        "the cluster and its label both serve to begin with"
    );

    let cluster = artifact_entity(&engine, cluster_id);
    engine
        .accept_change(cluster, ChangeOp::Delete)
        .expect("the delete is accepted");
    assert!(
        artifacts_of(&engine).is_empty(),
        "both go at the ack: the cluster on its own disposition, the label on its target's"
    );

    fold(&engine);

    assert!(
        artifacts_of(&engine).is_empty(),
        "and the label does not come back when the entry that hid its target retires"
    );
}

/// **A label survives its fold, and so does the blob it is read from.**
///
/// Supplied content lives in the record blob at the artifact's own entity, in extents of its own,
/// and a fold publishes a new prefix. A fold that dropped those entries would leave every artifact
/// registered and addressable with a description that cannot be read — which is not an artifact
/// served without its label but one **withheld entirely**, and so indistinguishable from a
/// containment failure the viewer was always going to have.
#[test]
fn supplied_content_survives_the_fold_and_the_restart_after_it() {
    let fx = fixture();
    {
        let engine = fx.open();
        let mut layer = declaration("topics/a");
        layer.content.supplied = vec![tessera_types::layer::SuppliedContent {
            kind: "label_text".into(),
            corpus_derived: true,
        }];
        engine.register_layer(layer).unwrap();
        engine
            .publish_artifacts(
                "topics/a".into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("t0".into()),
                    fx.members(0..300),
                    vec![IncomingVariation::new(
                        vec!["a label from the whole sample".into()],
                        fx.members(0..30),
                    )],
                )],
            )
            .unwrap();
        wait_for_publication(&fx, &engine, 1);

        fold(&engine);
        let served = artifacts_of(&engine);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].content, vec!["a label from the whole sample"]);
    }

    // **Restarted, which is the half that needs the blob.** The resident copy carries the values;
    // a record restored from a packed extent does not, and the serving path reads them from the
    // blob the fold carried forward.
    let engine = fx.open();
    let served = artifacts_of(&engine);
    assert_eq!(served.len(), 1, "the artifact is served rather than withheld");
    assert_eq!(served[0].content, vec!["a label from the whole sample"]);
}
