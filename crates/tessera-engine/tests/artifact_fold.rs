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
fn publish(fx: &Fixture, engine: &Engine, sources: std::ops::Range<u64>) {
    engine.register_layer(declaration("clusters/a")).unwrap();
    engine
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
