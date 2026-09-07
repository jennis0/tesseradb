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
//!
//! A test of decision 0107's rule for a generating set the fold emptied was removed with the
//! permissive mode that rule bounded (decision 0135): the fold withdraws content whose set lost a
//! member, so no set is emptied.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::membership::IncomingContent;
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

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
        // **No criterion**, deliberately: these cases are about what the membership *is* after a
        // fold, and a criterion would turn a wrong count into an absence, which is a weaker
        // assertion than a wrong number.
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
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

    /// The containment partitions the fold wrote into the prefix now being served.
    fn containment_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        let dir = self
            .live_prefix(engine)
            .join("partitions")
            .join("default")
            .join("containment");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tscp"))
            .collect()
    }

    /// The tile-index extent columns the fold wrote into the prefix now being served — one per
    /// `(view, layer, level)`, because an extent is a pair of rows and a row space is per view.
    fn tile_index_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        let dir = self
            .live_prefix(engine)
            .join("partitions")
            .join("default")
            .join("tile-index");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tsti"))
            .collect()
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

    /// The `level_versions` the newest side manifest of the live prefix states, as
    /// `(layer, level, version)`.
    fn stated_level_versions(&self, engine: &Engine) -> Vec<(String, u32, u64)> {
        let dir = self.live_prefix(engine).join("partitions").join("default");
        let newest = std::fs::read_dir(&dir)
            .expect("the partition directory exists")
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("SEGMENTS-") && n.ends_with(".json"))
            })
            .max()
            .expect("the live prefix carries a side manifest");
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap();
        json["level_versions"]
            .as_array()
            .expect("level_versions is a list")
            .iter()
            .map(|v| {
                (
                    v["layer"].as_str().unwrap().to_string(),
                    v["level"].as_u64().unwrap() as u32,
                    v["version"].as_u64().unwrap(),
                )
            })
            .collect()
    }
}

fn artifacts_of(engine: &Engine) -> Vec<ArtifactOut> {
    artifacts_for(engine, &full_coverage_credential())
}

/// What one credential is served over the whole map.
///
/// **The discriminating form of [`artifacts_of`]**, which authorises with full coverage — and a
/// full-coverage mask contains *every* generating set, an empty one included. A claim about which
/// set a fold wrote can therefore only be made from a principal that fails one of them.
fn artifacts_for(engine: &Engine, credential: &[u8]) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
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
    assert_eq!(
        artifacts.len(),
        1,
        "the fixture publishes exactly one artifact"
    );
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
fn publish(
    fx: &Fixture,
    engine: &Engine,
    sources: std::ops::Range<u64>,
) -> tessera_types::TesseraId {
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

// ---- the containment partition, across a restart -----------------------------------------------

/// Run ticks until the log has rotated far enough that a restart replays nothing — which is what
/// releases the artifact pins the fold has just cleared. Two, for `artifact_interleavings`'
/// reason: the first seals the member the publication is in, the second reclaims it.
fn rotate(engine: &Engine) {
    for _ in 0..2 {
        let before = engine.write_executor_stats().ticks;
        engine.request_flush();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while engine.write_executor_stats().ticks <= before {
            assert!(
                std::time::Instant::now() < deadline,
                "the tick that rotates the log never ran"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// **The fold writes a containment partition and a restart takes it up.** The alternative is not a
/// wrong answer — a level with no partition composes one on first use — it is the whole
/// consolidation the fold exists to do, landing on whichever request arrives first instead.
///
/// The adoption test is the coordinate and nothing weaker: the level version the manifest recorded
/// against the file, against the version the restart's own store arrives at. This case is the one
/// where they agree.
#[test]
fn the_fold_writes_a_containment_partition_a_restart_adopts() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        fold(&engine);
        assert_eq!(
            fx.containment_files(&engine).len(),
            1,
            "one partition per level, under the prefix the fold published"
        );
        rotate(&engine);
    }

    let engine = fx.open();
    assert_eq!(
        engine.artifact_containment_partitions_adopted(),
        1,
        "the coordinate held, so the partition was mapped rather than recomposed"
    );
    // And it is the partition the level actually serves from: a request that composed one would
    // move the other gauge.
    assert_eq!(count(&engine), 300);
    assert_eq!(
        engine.artifact_containment_partitions(),
        0,
        "nothing was composed, so the adopted partition is what answered"
    );
}

/// **A growth after the fold moves the level past the coordinate, and the partition is dropped.**
///
/// This is the direction that matters. A growth adds members to a generating set, which makes
/// containment *harder*; a reader that adopted the fold's partition anyway would answer the
/// **easier** question — the permissive one — on the test **I3** exists to make conservative. So
/// the rule is equality and nothing weaker, and this is the case that proves it is.
#[test]
fn a_growth_after_the_fold_leaves_the_partition_unadopted() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        // Resolved before the fold: the external-id map is read from the prefix the fold is about
        // to reclaim.
        let joining = fx.members(300..320);
        fold(&engine);
        assert_eq!(fx.containment_files(&engine).len(), 1);
        // The level moves under the file the fold just named.
        engine
            .grow_memberships(
                "clusters/a".into(),
                0,
                vec![tessera_lifecycle::IncomingGrowth::from_entities(
                    "c0".into(),
                    joining,
                )],
            )
            .expect("the growth is accepted");
        rotate(&engine);
        assert_eq!(
            count(&engine),
            320,
            "the growth is in force before the restart"
        );
    }

    let engine = fx.open();
    assert_eq!(
        engine.artifact_containment_partitions_adopted(),
        0,
        "the level moved after the partition was composed, so it must not be adopted"
    );
    assert_eq!(
        count(&engine),
        320,
        "and the grown membership is what is served"
    );
    assert_eq!(
        engine.artifact_containment_partitions(),
        1,
        "the level recomposed on first use, which is the whole of the fallback"
    );
}

/// **A fold that retires a member names no partition for that level.**
///
/// The fold writes its manifest *before* it retires, because a retirement is irreversible and a
/// manifest that would not commit must leave it undone. So at the moment the manifest is written
/// the store still holds the pre-retirement records while the prefix already holds the
/// post-retirement ones — and under `WithdrawContent` the retirement drops a content whole, which
/// **shifts the ranks**. A partition adopted at the wrong rank answers containment with another
/// content's generating set, which is not conservative in any direction anyone chose.
///
/// So the level gets no partition and recomposes on first use. Its version is stated, as the one
/// the level has once the retirement has run, which is what its tile index is stamped with
/// (`a_fold_that_retires_a_member_writes_the_tile_index_the_publication_adopts`).
#[test]
fn a_fold_that_retires_a_member_names_no_partition_for_that_level() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    fold(&engine);
    assert!(
        fx.containment_files(&engine).is_empty(),
        "the fold retired a member of this level, so it wrote no partition for it"
    );
    assert!(
        fx.stated_level_versions(&engine)
            .iter()
            .any(|(layer, level, _)| layer == "clusters/a" && *level == 0),
        "and it states the level's version, so a restart seeds the level at the version the \
         fold's other structures are stamped with"
    );
    rotate(&engine);
    drop(engine);

    let engine = fx.open();
    assert_eq!(engine.artifact_containment_partitions_adopted(), 0);
    assert_eq!(count(&engine), 299, "and the retired member is gone");
    assert_eq!(
        engine.artifact_containment_partitions(),
        1,
        "the level recomposed on first use"
    );
}

// ---- the tile index, across a restart ----------------------------------------------------------

/// **The fold writes a tile index and a restart maps it.** The consolidation is what the layout
/// memo's constraint 13 asks for: at ten million artifacts a level's extent column is eighty
/// megabytes, and page cache is reclaimable where an anonymous allocation is an OOM.
///
/// The adoption test is the coordinate and nothing weaker — prefix, view and the level version the
/// manifest recorded against the file. This case is the one where they all agree.
#[test]
fn the_fold_writes_a_tile_index_a_restart_adopts() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        fold(&engine);
        assert_eq!(
            fx.tile_index_files(&engine).len(),
            1,
            "one column per (view, layer, level), under the prefix the fold published"
        );
        rotate(&engine);
    }

    let engine = fx.open();
    // The first request for the level claims it; before that nothing has asked.
    assert_eq!(count(&engine), 300);
    assert_eq!(
        engine.artifact_tile_indexes_adopted(),
        1,
        "the coordinate held, so the column was mapped rather than derived"
    );
}

/// **A growth after the fold moves the level past the coordinate, and the column is dropped.**
///
/// This is the direction that matters, and it is the mirror image of the containment partition's. A
/// growth adds members, so the fold's extents are **narrow**: an artifact that has grown past its
/// node would be found in no node the viewport touches and served to nobody, and one settled on a
/// narrow extent would have its probe taken against `viewport ∩ M_auth` on a claim — that its
/// membership is inside the viewport — which the growth made false. So the rule is equality.
#[test]
fn a_growth_after_the_fold_leaves_the_tile_index_unadopted() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        // Resolved before the fold: the external-id map is read from the prefix the fold is about
        // to reclaim.
        let joining = fx.members(300..320);
        fold(&engine);
        assert_eq!(fx.tile_index_files(&engine).len(), 1);
        engine
            .grow_memberships(
                "clusters/a".into(),
                0,
                vec![tessera_lifecycle::IncomingGrowth::from_entities(
                    "c0".into(),
                    joining,
                )],
            )
            .expect("the growth is accepted");
        rotate(&engine);
    }

    let engine = fx.open();
    assert_eq!(
        count(&engine),
        320,
        "the grown membership is what is served"
    );
    assert_eq!(
        engine.artifact_tile_indexes_adopted(),
        0,
        "the level moved after the column was projected, so it must not be claimed"
    );
}

/// **A fold that retires a member of a level writes that level's tile index, and the publication
/// adopts it.** The manifest is written before the retirement, so the store's version is the
/// pre-retirement one while the prefix's records are the post-retirement ones. The index is the
/// memberships projected through the folded row space, in which a retired member has no row, so
/// it is projected from the records the retirement leaves and stamped with the version the level
/// has once the retirement has run. The warm after the flip claims it rather than projecting the
/// level's memberships, and so does a restart. A fold that omitted the level paid the whole
/// projection at its warm: 135 s and 3.7 GB of resident memory on rung 3's `mesh/descriptors`
/// (`probes/2026-09-04-epoch-shard-fold-decomposition/`).
#[test]
fn a_fold_that_retires_a_member_writes_the_tile_index_the_publication_adopts() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    assert_eq!(count(&engine), 300);
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    fold(&engine);
    assert_eq!(
        fx.tile_index_files(&engine).len(),
        1,
        "the fold wrote the level's index although the retirement moved the level"
    );
    assert_eq!(
        engine.artifact_tile_indexes_adopted(),
        1,
        "and its warm claimed the index rather than projecting the level"
    );
    assert_eq!(count(&engine), 299, "the retired member is gone");
    assert_eq!(
        engine.artifact_tile_indexes_adopted(),
        1,
        "the request read the warmed form"
    );
    let stated_after_retiring = fx.stated_level_versions(&engine);

    // The stamped version is the one the level actually has once the retirement has run: a
    // second fold, which retires nothing, states the store's own version for the level, and it
    // is the same number.
    fold(&engine);
    assert_eq!(
        fx.stated_level_versions(&engine),
        stated_after_retiring,
        "the first fold stated the version the level had after its retirement"
    );
    assert_eq!(engine.artifact_tile_indexes_adopted(), 2);
    rotate(&engine);
    drop(engine);

    let engine = fx.open();
    assert_eq!(count(&engine), 299);
    assert_eq!(
        engine.artifact_tile_indexes_adopted(),
        1,
        "the stated version is the one the level restarts at, so the restart claims it too"
    );
}

/// Publish three artifacts, retire the one at `retired` by its own entity, fold, and check the
/// index the fold writes leaves it out: the publication claims the index, the survivors are
/// served whole, and a restart claims it again.
fn own_entity_retired_at(retired: usize) {
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
    let served = |engine: &Engine| -> Vec<(Option<String>, u64)> {
        let mut out: Vec<_> = artifacts_of(engine)
            .into_iter()
            .map(|a| (a.key, a.masked_count))
            .collect();
        out.sort();
        out
    };
    let survivors: Vec<(Option<String>, u64)> = (0..3)
        .filter(|ordinal| *ordinal != retired)
        .map(|ordinal| (Some(format!("c{ordinal}")), 100))
        .collect();

    let deleted = artifact_entity(&engine, ids[retired]);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("an artifact takes a deletion like any other entity");
    assert_eq!(served(&engine), survivors, "hidden at the ack");
    fold(&engine);
    assert_eq!(fx.tile_index_files(&engine).len(), 1);
    assert_eq!(
        engine.artifact_tile_indexes_adopted(),
        1,
        "the warm claimed the fold's index"
    );
    assert_eq!(
        served(&engine),
        survivors,
        "the retired artifact is absent from the adopted index and the survivors are whole"
    );
    rotate(&engine);
    drop(engine);

    let engine = fx.open();
    assert_eq!(served(&engine), survivors);
    assert_eq!(engine.artifact_tile_indexes_adopted(), 1);
}

/// **A fold that retires an artifact's own entity leaves that artifact out of the tile index it
/// writes.** The artifact's slot becomes a hole at the retirement, and the index the fold writes
/// for the level, stamped with the post-retirement version, has a hole at its ordinal too; the
/// survivors after it keep their ordinals. The middle one, so the hole sits between two live
/// ordinals.
#[test]
fn a_fold_that_retires_an_artifacts_own_entity_leaves_it_out_of_the_tile_index() {
    own_entity_retired_at(1);
}

/// **The top ordinal retired.** The level's slots still reach past it, and the reader sizes the
/// level at one past the highest live ordinal, so an index one ordinal longer would be refused
/// for covering a different range. The fold sizes its index the same way.
#[test]
fn a_fold_that_retires_the_top_artifact_writes_an_index_the_survivors_fit() {
    own_entity_retired_at(2);
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

// ---- the row form's boundary: base rows, and what that costs ------------------------------------

/// Ingest one item at the fixture's origin, carrying the term every principal here holds.
///
/// **The batch name and the idempotency key are derived from `external_id`**, and that is not
/// tidiness: a second ingest under a batch key already seen is *replayed* rather than accepted, so
/// a helper with a fixed key silently ingests nothing the second time it is called and every flush
/// after the first has nothing to publish.
fn ingest(engine: &Engine, external_id: &[u8]) -> EntityId {
    let descriptors = vec![b"0".to_vec()];
    let mut key = [0u8; 32];
    for (slot, byte) in key.iter_mut().zip(external_id) {
        *slot = *byte;
    }
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(external_id.to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(
            vec![row],
            String::from_utf8_lossy(external_id).into_owned(),
            key,
        )
        .expect("the ingest is accepted")[0]
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **A member counts from its flush**, which is the first moment it has a row at all.
///
/// The two boundaries are different and only one of them moved. A **buffered** member has no row
/// anywhere and is in nobody's count, exactly as it is in no viewport; a **flushed** one has an
/// extent row, and the level's held form gains that segment's rows at the publication that adds it
/// (`ArtifactProjections::extend_flushed`) rather than at the next fold. Until 2026-09-03 the form
/// covered base rows alone and this second case understated for as long as the gate between folds
/// — hours — which was fail-closed and is now simply not the case.
///
/// The fold changes no count here, and that is the assertion the third case makes: it renumbers the
/// row the member holds and the form is rebuilt over the new prefix, so the same member is counted
/// by a different row.
#[test]
fn a_member_ingested_since_the_last_fold_counts_from_its_flush() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();

    // A member that does not exist when the corpus is built: ingested here, and named by the
    // artifact published after it.
    let fresh = ingest(&engine, b"fresh-member");
    let mut members = fx.members(0..300);
    members.push(fresh);
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(Some("c0".into()), members)],
        )
        .unwrap();
    wait_for_publication(&fx, &engine, 1);

    assert_eq!(
        count(&engine),
        300,
        "buffered: the member has no row at all yet, so it is in no count"
    );

    flush(&engine);
    assert_eq!(
        count(&engine),
        301,
        "flushed: it holds an extent row, and the flush extended every held form by the segment \
         it published rather than leaving the count short until a fold"
    );

    fold(&engine);
    assert_eq!(
        count(&engine),
        301,
        "folded: the same member, on a base row now — the fold renumbers what it counts and not \
         how many"
    );
}

/// **A flush leaves every artifact's count where it was**, which is the property that lets the form
/// outlive one — and the one a rebuild-on-every-publication cache would hide rather than provide.
///
/// Checked over members that were in the base all along, so the answer is the same before and
/// after: what would break it is a form rebuilt against a row space it was not built for, which is
/// how a stale projection announces itself — as a *changed* count for an unchanged membership.
#[test]
fn a_flush_disturbs_no_artifacts_count() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    assert_eq!(count(&engine), 300);

    ingest(&engine, b"unrelated");
    flush(&engine);

    assert_eq!(
        count(&engine),
        300,
        "an append moved no bit this form holds"
    );
}

/// **The one publication that renumbers rows a form holds.** A merge permutes row space *inside
/// the span it merges*, so a row id in that span names a different entity afterwards — and a form
/// holding those ids would go on counting them, naming whichever documents landed there. That is
/// fail-**open** rather than closed: a stranger's row inside the span counts as a member, and one
/// extra member can lift an artifact over its existence criterion.
///
/// A form covers extent rows (2026-09-03), so the state is reachable and what removes it is the
/// check rather than the absence: `ArtifactRows::covers` compares the segments a form's rows came
/// from against the row space at every cache hit, `seg_id`s are never reused, and a form whose
/// segments were permuted rather than appended to is discarded and projected again. What this pins
/// is the outcome either way — an artifact's count survives a merge that genuinely permuted the
/// rows beneath it.
#[test]
fn a_merge_that_renumbers_extent_rows_disturbs_no_artifacts_count() {
    let fx = fixture();
    let engine = fx.open();
    engine.set_merge_for_test(false);
    publish(&fx, &engine, 0..300);
    assert_eq!(count(&engine), 300);

    // Two flushes, so the merge below has two extents to collapse and a span to permute. The
    // ingests are unrelated to the artifact: what is under test is whether *its* rows survive
    // somebody else's renumbering.
    // Four, which is the tier width the merge policy collapses on — fewer and the merge below
    // never triggers, and the case would pass by never exercising anything.
    for batch in [
        b"merge-a".as_slice(),
        b"merge-b".as_slice(),
        b"merge-c".as_slice(),
        b"merge-d".as_slice(),
    ] {
        ingest(&engine, batch);
        flush(&engine);
    }

    engine.set_merge_for_test(true);
    ingest(&engine, b"merge-e");
    flush(&engine);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().merges == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the merge never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        count(&engine),
        300,
        "the merge renumbered rows above the base and this artifact holds none of them"
    );
}

// ---- a deleted source, executed at the fold -------------------------------------------------------

/// A layer carrying corpus-derived content.
fn content_layer() -> LayerDeclaration {
    let mut d = declaration("clusters/a");
    d.content.supplied = vec![tessera_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
    }];
    d
}

/// Publish one described artifact over `0..300`, generated from `0..30`.
fn publish_described(fx: &Fixture, engine: &Engine) {
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("c0".into()),
                fx.members(0..300),
                vec![IncomingContent::new(
                    vec!["shipping and logistics".into()],
                    fx.members(0..30),
                )],
            )],
        )
        .unwrap();
    wait_for_publication(fx, engine, 1);
}

/// **The test the stage exists for.** Delete a document a label was generated from, watch the label
/// vanish at the ack, run a fold, and it **stays gone** — served on no set that no longer names what
/// the text was derived from.
///
/// And the artifact goes with it. Its layer declares supplied content; the fold withdrew the only
/// content that had it; so what is left is an identity and a count with no description, which
/// decision 0076 forbids serving. The caller re-declares (decision 0135).
#[test]
fn a_deleted_source_withdraws_the_content_at_the_fold_and_the_artifact_with_it() {
    let fx = fixture();
    {
        let engine = fx.open();
        engine.register_layer(content_layer()).unwrap();
        publish_described(&fx, &engine);
        assert_eq!(
            artifacts_of(&engine).len(),
            1,
            "served with its description"
        );

        // Inside the generating sample, so containment fails for everyone from the ack.
        engine
            .accept_change(fx.member(7), ChangeOp::Delete)
            .expect("the delete is accepted");
        assert!(
            artifacts_of(&engine).is_empty(),
            "withheld at the ack — emergent from containment, with nothing stored"
        );

        fold(&engine);
        assert!(
            artifacts_of(&engine).is_empty(),
            "and still withheld after the fold: the content was withdrawn, not re-based onto a \
             smaller set"
        );
    }

    let engine = fx.open();
    assert!(
        artifacts_of(&engine).is_empty(),
        "the withdrawal is what the prefix says, not something the process was remembering"
    );
}

/// **The withdrawal reaches exactly the content whose generating set named the deleted document**
/// (decision 0135). The fold does not shrink the set and keep the content on the survivors: what a
/// content was derived from is the caller's claim, and a principal satisfying the survivors would
/// otherwise read text derived from a document they cannot see. A content whose set names no
/// retired entity is untouched, and its artifact serves on.
///
/// `c1` is the control: the same layer, the same fold, the same request, a generating set drawn
/// inside the subset term and not naming the deleted document. An artifact absent because the
/// fold broke the layer, or because the viewport returned nothing at all, would satisfy a careless
/// assertion that `c0` is gone; and the narrow principal being served `c1` shows the layer still
/// serves on the set the caller declared.
///
/// **Mutations this kills:** a fold that subtracts the retired members from the set and keeps the
/// content (the dropped permissive mode: `c0` would come back for full coverage); one that
/// withdraws every content of the level rather than the ones whose set lost a member (would take
/// `c1`); one that withdraws the artifact rather than the content (also takes `c1`, which shares
/// the level).
#[test]
fn a_deleted_source_withdraws_only_the_content_whose_set_named_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(content_layer()).unwrap();

    // The fixture gives the subset term to every third source id, so `7` is outside this set.
    let control_set = fx.members((0..90).filter(|s| terms_of(*s).contains(&SUBSET_TERM)));
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![
                IncomingArtifact::with_content(
                    Some("c0".into()),
                    fx.members(0..300),
                    vec![IncomingContent::new(
                        vec!["shipping and logistics".into()],
                        fx.members(0..30),
                    )],
                ),
                IncomingArtifact::with_content(
                    Some("c1".into()),
                    fx.members(0..300),
                    vec![IncomingContent::new(
                        vec!["what the subset term covers".into()],
                        control_set,
                    )],
                ),
            ],
        )
        .expect("two described artifacts publish");
    wait_for_publication(&fx, &engine, 1);
    assert_eq!(
        artifacts_of(&engine).len(),
        2,
        "both serve before the deletion"
    );

    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    let at_ack: Vec<Option<String>> = artifacts_of(&engine).into_iter().map(|a| a.key).collect();
    assert_eq!(
        at_ack,
        vec![Some("c1".to_string())],
        "withheld at the ack by containment alone: the deleted document is outside every mask"
    );

    fold(&engine);

    let served = artifacts_of(&engine);
    let keys: Vec<Option<String>> = served.iter().map(|a| a.key.clone()).collect();
    assert_eq!(
        keys,
        vec![Some("c1".to_string())],
        "the content whose set named the deleted document is withdrawn and its artifact with it, \
         while the one published beside it — same layer, same fold, same request — is served"
    );
    assert_eq!(
        served[0].content,
        vec!["what the subset term covers"],
        "and `c1`'s own generating set is untouched: the withdrawal reaches only sets naming a \
         retired entity"
    );
    assert_eq!(
        served[0].masked_count, 299,
        "the deletion left the membership as it leaves any other"
    );

    let narrow: Vec<Option<String>> = artifacts_for(&engine, &subset_credential())
        .into_iter()
        .map(|a| a.key)
        .collect();
    assert_eq!(
        narrow,
        vec![Some("c1".to_string())],
        "a principal who satisfies `c1`'s declared set is served it, so the layer serves on the \
         caller's claim and not on a set the fold edited"
    );
}

/// **A declared member that is deleted refuses the batch; a suppressed one is accepted.** The two
/// are not near-neighbours: a deletion is irreversible, so the content would be unservable from
/// birth and the count short for ever, while a suppression is an operator's reversible action that
/// must not refuse an unrelated publication.
#[test]
fn publication_refuses_a_deleted_member_and_accepts_a_suppressed_one() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();

    let deleted = fx.member(7);
    let suppressed = fx.member(11);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("the delete is accepted");
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("the suppress is accepted");

    let refused = engine.publish_artifacts(
        "clusters/a".into(),
        0,
        vec![IncomingArtifact::from_entities(
            Some("c0".into()),
            fx.members(0..300),
        )],
    );
    let detail = match refused {
        Err(e) => e.to_string(),
        Ok(_) => panic!(
            "a membership naming a deleted document is refused rather than published into silence"
        ),
    };
    // The refusal is the caller's 422 body, so it carries a count and a position and never an
    // entity id (I10): every number in it is one of those two.
    let numbers: Vec<&str> = detail
        .split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect();
    assert_eq!(
        numbers,
        vec!["1", "0"],
        "one deleted member, in artifact 0, and nothing else numeric: {detail}"
    );
    assert!(
        !detail.contains(&deleted.raw().to_string()),
        "the deleted entity's id must not appear: {detail}"
    );

    // The same batch without the deleted member, and still carrying the suppressed one.
    let accepted = engine.publish_artifacts(
        "clusters/a".into(),
        0,
        vec![IncomingArtifact::from_entities(
            Some("c0".into()),
            fx.members((0..300).filter(|s| *s != 7)),
        )],
    );
    assert!(
        accepted.is_ok(),
        "the suppressed member is a live member temporarily outside every mask: {accepted:?}"
    );
    assert_eq!(
        count(&engine),
        298,
        "and it is outside this count until the unsuppress — fail-closed, and not a refusal"
    );
}

/// The refusal reaches a **generating set** as well as a membership, and for the sharper reason: a
/// set naming a deleted document fails containment for every principal, so the description could
/// never be read by anyone.
#[test]
fn publication_refuses_a_generating_set_naming_a_deleted_document() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(content_layer()).unwrap();
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");

    let refused = engine.publish_artifacts(
        "clusters/a".into(),
        0,
        vec![IncomingArtifact::with_content(
            // The membership avoids the deleted document; only the sample names it.
            Some("c0".into()),
            fx.members((0..300).filter(|s| *s != 7)),
            vec![IncomingContent::new(
                vec!["shipping and logistics".into()],
                fx.members(0..30),
            )],
        )],
    );
    assert!(
        refused.is_err(),
        "content generated from a deleted document is unservable from birth"
    );
}

// ---- the fold's report -------------------------------------------------------------------------

/// The report the fold wrote for the prefix it published, as the operator would read it off disk.
fn report_on_disk(fx: &Fixture, prefix: &str) -> serde_json::Value {
    let path = fx.root.join("reports").join(format!("fold-{prefix}.json"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("the fold must have written {}: {e}", path.display()));
    serde_json::from_slice(&bytes).expect("the report is JSON")
}

/// **What discharges the obligation**: a deletion may not retire before the caller has been told
/// what it degraded, so the fold that retires it writes the notice naming the artifact, how much of
/// its membership went, and which supplied content lost a source.
#[test]
fn the_fold_reports_what_its_deletions_took_from_every_artifact_that_held_them() {
    let fx = fixture();
    let engine = fx.open();
    let mut layer = declaration("clusters/a");
    layer.content.supplied = vec![tessera_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
    }];
    engine.register_layer(layer).unwrap();
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("c0".into()),
                fx.members(0..300),
                // Generated from a sample of the membership, so a deletion inside the sample is a
                // content loss as well as a membership one — the two are separate rows of the
                // report and a single number could not carry both.
                vec![IncomingContent::new(
                    vec!["shipping and logistics".into()],
                    fx.members(0..30),
                )],
            )],
        )
        .unwrap();
    wait_for_publication(&fx, &engine, 1);

    // One member inside the generating sample, one outside it.
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    engine
        .accept_change(fx.member(200), ChangeOp::Delete)
        .expect("the delete is accepted");

    fold(&engine);

    let held = engine.last_fold_report();
    assert_eq!(held.len(), 1, "one artifact was degraded");
    assert_eq!(held[0].key.as_deref(), Some("c0"));
    assert_eq!(held[0].members_lost, 2, "both deletions were members");
    assert_eq!(
        held[0].declared_members, 300,
        "against what the caller published, so the notice carries the proportion"
    );
    assert_eq!(
        held[0].contents_lost,
        vec![(0, 1)],
        "and exactly one of them was a source of the description"
    );

    let on_disk = report_on_disk(&fx, &engine.generation().prefix);
    assert_eq!(on_disk["degraded"][0]["key"], "c0");
    assert_eq!(on_disk["degraded"][0]["members_lost"], 2);
}

/// **A fold that degraded nothing still reports**, because an operator polling the directory must be
/// able to tell that from a fold that never reported — and the second is the state the obligation is
/// about.
#[test]
fn a_fold_that_degraded_nothing_writes_an_empty_report_rather_than_none() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);

    fold(&engine);

    assert!(engine.last_fold_report().is_empty());
    let on_disk = report_on_disk(&fx, &engine.generation().prefix);
    assert_eq!(
        on_disk["degraded"].as_array().map(Vec::len),
        Some(0),
        "the file exists and says nothing was degraded"
    );
}

/// **The report outlives the prefix it reports on.** A fold reclaims the prefix it superseded, so a
/// notice written inside the new prefix would be deleted by the fold after next — taking with it
/// the one a caller had not read yet.
#[test]
fn a_later_fold_does_not_reclaim_an_earlier_folds_report() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    fold(&engine);
    let first = engine.generation().prefix.clone();
    assert_eq!(
        report_on_disk(&fx, &first)["degraded"][0]["members_lost"],
        1
    );

    fold(&engine);

    assert_ne!(engine.generation().prefix, first, "a second prefix");
    assert_eq!(
        report_on_disk(&fx, &first)["degraded"][0]["members_lost"],
        1,
        "the first fold's notice is still there after the prefix it named was reclaimed"
    );
    assert!(
        engine.last_fold_report().is_empty(),
        "and the held copy is the latest fold's, which degraded nothing"
    );
}

/// **A report that cannot be written stops the retirement**, which is the whole of "retirement and
/// report in one publication". Nothing is lost by refusing: the deletions are already in force at
/// their ack, and the next fold reports them.
#[test]
fn a_fold_whose_report_cannot_be_written_is_discarded_and_retires_nothing() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);
    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    assert_eq!(engine.overlay_depth(), 1);

    // `reports/` as a *file*, so creating the directory fails — the cheapest way to make the write
    // fail that does not depend on running as an unprivileged user.
    std::fs::write(fx.root.join("reports"), b"not a directory").unwrap();

    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(now.folds, before.folds, "the fold must not have published");
        if now.fold_failures > before.fold_failures {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold neither published nor was discarded"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        engine.overlay_depth(),
        1,
        "the deletion did not retire, so the notice is still owed"
    );
    assert_eq!(count(&engine), 299, "and it is still in force");
}

// ---- Rule F's artifact arm ---------------------------------------------------------------------

/// The cluster layer a label hangs from, and the label layer itself. Both `public`, so every
/// withholding below comes from the arm under test rather than from an access label.
fn labels_over(target: &str) -> LayerDeclaration {
    let mut d = declaration("topics/x");
    d.content.supplied = vec![tessera_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: tessera_types::layer::SuppliedRequirement::Inherited,
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
    assert_eq!(
        engine.published_artifacts(),
        2,
        "the level holds two records"
    );
    let survivor = engine
        .locate_artifact(last_before)
        .expect("the survivor still has an address");
    assert_eq!(
        (survivor.ordinal, survivor.key.as_deref()),
        (2, Some("c2")),
        "the survivor after the hole keeps its ordinal and its key — its identity did not shift up"
    );
    // The **address** still resolves, because it is the layer's reserved run that answers it and a
    // run is not per artifact. What is gone is the record at that ordinal, which is the hole.
    assert_eq!(
        engine.locate_artifact(deleted).and_then(|at| at.key),
        None,
        "and the deleted artifact's slot holds nothing"
    );
}

/// **The hole at the *top* of a level is the one that hides**, and losing it reuses an identity.
///
/// A hole in the middle is implied by the ordinals either side of it, so a level seeded from its
/// records alone still comes back the right length. A hole at the end is implied by nothing: the
/// level comes back short, the next ordinal regresses onto it, and the next publication is handed
/// the ordinal — and therefore the entity, which is a function of it — that the artifact this fold
/// deleted was published under. Two artifacts, one `tessera_id`, the second answering for the first.
#[test]
fn deleting_the_last_artifact_of_a_level_does_not_hand_its_identity_to_the_next_publication() {
    let fx = fixture();
    // Resolved before the fold: `members` reads the built prefix's external-id run, and the fold
    // reclaims that prefix. Entities are stable across it, so the set is still the right one.
    let later_members = fx.members(200..300);
    let deleted_entity = {
        let engine = fx.open();
        engine.register_layer(declaration("clusters/a")).unwrap();
        let ids = engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![
                    IncomingArtifact::from_entities(Some("c0".into()), fx.members(0..100)),
                    IncomingArtifact::from_entities(Some("c1".into()), fx.members(100..200)),
                ],
            )
            .unwrap();
        wait_for_publication(&fx, &engine, 1);
        // The **last** one, so the hole it leaves is at the top of the level.
        let entity = artifact_entity(&engine, ids[1]);
        engine
            .accept_change(entity, ChangeOp::Delete)
            .expect("the delete is accepted");
        fold(&engine);
        entity
    };

    // Reopened, so the level is what the extent says rather than what this process remembered.
    let engine = fx.open();
    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c2".into()),
                later_members,
            )],
        )
        .unwrap();
    let fresh = artifact_entity(&engine, ids[0]);

    assert_ne!(
        fresh, deleted_entity,
        "the new artifact took the deleted one's entity, so one identifier now names two artifacts"
    );
    assert_eq!(
        engine.locate_artifact(fresh).map(|at| at.ordinal),
        Some(2),
        "it lands past the hole rather than in it"
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
                vec![IncomingContent::new(
                    vec!["shipping and logistics".into()],
                    Vec::new(),
                )],
                tessera_lifecycle::membership::IncomingAttachment {
                    layer: "clusters/a".into(),
                    level: 0,
                    key: "c0".into(),
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
            name: "topic".into(),
            ty: "text".into(),
            require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
        }];
        engine.register_layer(layer).unwrap();
        engine
            .publish_artifacts(
                "topics/a".into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("t0".into()),
                    fx.members(0..300),
                    vec![IncomingContent::new(
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
    assert_eq!(
        served.len(),
        1,
        "the artifact is served rather than withheld"
    );
    assert_eq!(served[0].content, vec!["a label from the whole sample"]);
}

/// **Deleting a cluster deletes its labels, and the removal retires where every other deletion
/// retires** ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
/// rule 1).
///
/// The distinction this test carries, and the reason it is here rather than beside the serving
/// tests: the label vanishing at the ack proves only the *visibility* rule, which would hold with
/// the label's record sitting in its level for ever. What rule 1 adds is that the label is
/// **deleted** — one more entry in the overlay, one more record in the WAL, and one more slot the
/// fold empties. A cascade retiring by any other route would be a second removal rule, which is the
/// fail-open write-path §5.4 exists to prevent.
#[test]
fn deleting_a_cluster_deletes_its_labels_and_they_retire_at_the_same_fold() {
    let fx = fixture();
    let cluster_entity = {
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
                    vec![IncomingContent::new(
                        vec!["shipping and logistics".into()],
                        Vec::new(),
                    )],
                    tessera_lifecycle::membership::IncomingAttachment {
                        layer: "clusters/a".into(),
                        level: 0,
                        key: "c0".into(),
                    },
                )],
            )
            .unwrap();
        wait_for_publication(&fx, &engine, 2);
        assert_eq!(artifacts_of(&engine).len(), 2);
        assert_eq!(engine.published_artifacts(), 2);

        let cluster = artifact_entity(&engine, cluster_id);
        assert_eq!(engine.overlay_depth(), 0, "nothing is denied yet");
        engine
            .accept_change(cluster, ChangeOp::Delete)
            .expect("the delete is accepted");

        // **Two dispositions from one command.** The cluster's own, and the label's — the cascade,
        // carried in the same window, on the same lane, with its own durable record.
        assert_eq!(
            engine.overlay_depth(),
            2,
            "the label was deleted with its cluster rather than merely withheld behind it"
        );
        assert!(artifacts_of(&engine).is_empty(), "both go at the ack");

        fold(&engine);

        assert_eq!(
            engine.published_artifacts(),
            0,
            "and both slots left their levels at the fold that executed the deletions — the \
             label's by the same route as the cluster's"
        );
        cluster
    };

    // **Reopened, which is where a cascade that only hid the label would show.** The overlay
    // entries are retired and gone from the manifest, so nothing but the absence of the records
    // keeps either artifact away.
    let engine = fx.open();
    assert!(
        artifacts_of(&engine).is_empty(),
        "the label stayed gone across the retirement of the entry that was hiding it"
    );
    assert_eq!(engine.published_artifacts(), 0);
    assert!(
        engine
            .locate_artifact(cluster_entity)
            .and_then(|at| at.key)
            .is_none(),
        "and the cluster's own slot is a hole, as it was before this rule existed"
    );
}
