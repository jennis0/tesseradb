//! **The fold's artifact pass**: a fold rewrites every membership into the prefix it publishes, and
//! what a deletion took away does not come back at the flip.
//!
//! Row space renumbers globally at a fold and membership extent paths are prefix-relative, so a
//! fold that did nothing here would produce one of two failures, both silent: a bundle naming
//! extents the new prefix does not contain, or artifacts that come back registered, addressable and
//! served as absent. Both look like a clustering that failed its existence criterion.
//!
//! The distinction these cases exist to hold is the one this corpus has caught twice: **a deletion
//! retires at the fold that executes it, and a live member's suppression only on unsuppress.**
//! A pass that dropped a suppressed member's bit while it was at it would give a suppression a
//! second retirement route, and the member would not come back at the unsuppress — fail-open, and
//! indistinguishable from a cluster that had always been that size.
//!
//! A test of decision 0107's rule for a generating set the fold emptied was removed with the
//! permissive mode that rule bounded (decision 0135): the fold withdraws content whose set lost a
//! member, so no set is emptied.

mod common;

use common::*;
use mosaica_engine::Engine;
use mosaica_lifecycle::membership::IncomingContent;
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_lifecycle::IncomingArtifact;
use mosaica_types::layer::{ContentDeclaration, LayerDeclaration};
use mosaica_types::{EntityId, Target};

fn declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        // **No criterion**, deliberately: these cases are about what the membership *is* after a
        // fold, and a criterion would turn a wrong count into an absence, which is a weaker
        // assertion than a wrong number.
        require_member_visibility: None,
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
        },
        // Pinned to rows: these cases are about the tile index and the containment partition an
        // artifact-major level folds into, which a flat level is not served in otherwise.
        layout: Some(mosaica_types::layer::ServingLayout::ArtifactMajor),
        ..flat_declaration(name)
    }
}

impl Fixture {
    /// The corpus entity behind one source id — what a delete or a suppress is addressed to.
    fn member(&self, source_id: u64) -> EntityId {
        self.members(std::iter::once(source_id))[0]
    }

    /// The prefix directory the bundle currently serves.
    fn live_prefix(&self, engine: &Engine) -> std::path::PathBuf {
        self.root.join(&engine.generation().prefix)
    }

    /// The files with extension `ext` under derived `kind` in the prefix now being served.
    fn files(&self, engine: &Engine, kind: &str, ext: &str) -> Vec<std::path::PathBuf> {
        live_shard_files(&self.root, engine, kind)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == ext))
            .collect()
    }

    /// The containment partitions the fold wrote into the prefix now being served.
    fn containment_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        self.files(engine, "containment", "tscp")
    }

    /// The tile-index extent columns the fold wrote into the prefix now being served — one per
    /// `(view, layer, level)`, because an extent is a pair of rows and a row space is per view.
    fn tile_index_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        self.files(engine, "tile-index", "tsti")
    }

    fn membership_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        self.files(engine, "members", "tsmb")
    }

    /// The `level_versions` the served side manifest states, as `(layer, level, version)`.
    fn stated_level_versions(&self) -> Vec<(String, u32, u64)> {
        let manifest = side_manifest(&self.root);
        manifest
            .layer_spaces
            .iter()
            .flat_map(|space| {
                let layer = manifest
                    .partition
                    .layers
                    .iter()
                    .find(|layer| layer.space == space.space)
                    .map(|layer| layer.declaration.name.clone())
                    .unwrap_or_default();
                space
                    .level_versions
                    .iter()
                    .map(move |v| (layer.clone(), v.level, v.version))
            })
            .collect()
    }
}

/// The one artifact's masked count, for a principal who can see everything — so the number is the
/// membership's own size and any movement in it is the pass's doing.
fn count(engine: &Engine) -> u64 {
    let artifacts = artifacts_of(engine, &full_coverage_credential());
    assert_eq!(
        artifacts.len(),
        1,
        "the fixture publishes exactly one artifact"
    );
    artifacts[0].masked_count
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
) -> mosaica_types::MosaicaId {
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

/// **A node holding artifacts folds at all** — which it did not until the pass existed, because the
/// alternative to rewriting the extents was refusing the publication outright.
///
/// And what it publishes is a *new* file under the *new* prefix: the paths are prefix-relative, so
/// a fold that carried the manifest entries forward would name files nothing contains and the
/// bundle would refuse at its next open.
#[test]
fn a_fold_rewrites_the_memberships_into_the_prefix_it_publishes() {
    let fx = fixture();
    let engine = fx.open_unrefreshed();
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
/// so the count stays down; the live member's suppression retires only on unsuppress, so its bit
/// is still there and the member comes back.
#[test]
fn a_deleted_member_is_gone_after_the_fold_and_a_suppressed_one_comes_back() {
    let fx = fixture();
    let engine = fx.open_unrefreshed();
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
        let engine = fx.open_unrefreshed();
        publish(&fx, &engine, 0..300);
        engine
            .accept_change(fx.member(7), ChangeOp::Delete)
            .expect("the delete is accepted");
        fold(&engine);
        assert_eq!(count(&engine), 299);
    }

    let engine = fx.open_unrefreshed();
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
        let engine = fx.open_unrefreshed();
        publish(&fx, &engine, 0..300);
        fold(&engine);
        assert_eq!(
            fx.containment_files(&engine).len(),
            1,
            "one partition per level, under the prefix the fold published"
        );
        reclaim_log(&engine);
    }

    let engine = fx.open_unrefreshed();
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
        let engine = fx.open_unrefreshed();
        publish(&fx, &engine, 0..300);
        // Resolved before the fold: the `id` index is read from the prefix the fold is about to
        // reclaim.
        let joining = fx.members(300..320);
        fold(&engine);
        assert_eq!(fx.containment_files(&engine).len(), 1);
        // The level moves under the file the fold just named.
        engine
            .grow_memberships(
                "clusters/a".into(),
                0,
                vec![mosaica_lifecycle::IncomingGrowth::from_entities(
                    "c0".into(),
                    joining,
                )],
            )
            .expect("the growth is accepted");
        reclaim_log(&engine);
        assert_eq!(
            count(&engine),
            320,
            "the growth is in force before the restart"
        );
    }

    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
        fx.stated_level_versions()
            .iter()
            .any(|(layer, level, _)| layer == "clusters/a" && *level == 0),
        "and it states the level's version, so a restart seeds the level at the version the \
         fold's other structures are stamped with"
    );
    reclaim_log(&engine);
    drop(engine);

    let engine = fx.open_unrefreshed();
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
        let engine = fx.open_unrefreshed();
        publish(&fx, &engine, 0..300);
        fold(&engine);
        assert_eq!(
            fx.tile_index_files(&engine).len(),
            1,
            "one column per (view, layer, level), under the prefix the fold published"
        );
        reclaim_log(&engine);
    }

    let engine = fx.open_unrefreshed();
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
        let engine = fx.open_unrefreshed();
        publish(&fx, &engine, 0..300);
        // Resolved before the fold: the `id` index is read from the prefix the fold is about to
        // reclaim.
        let joining = fx.members(300..320);
        fold(&engine);
        assert_eq!(fx.tile_index_files(&engine).len(), 1);
        engine
            .grow_memberships(
                "clusters/a".into(),
                0,
                vec![mosaica_lifecycle::IncomingGrowth::from_entities(
                    "c0".into(),
                    joining,
                )],
            )
            .expect("the growth is accepted");
        reclaim_log(&engine);
    }

    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
    let stated_after_retiring = fx.stated_level_versions();

    // The stamped version is the one the level actually has once the retirement has run: a
    // second fold, which retires nothing, states the store's own version for the level, and it
    // is the same number.
    fold(&engine);
    assert_eq!(
        fx.stated_level_versions(),
        stated_after_retiring,
        "the first fold stated the version the level had after its retirement"
    );
    assert_eq!(engine.artifact_tile_indexes_adopted(), 2);
    reclaim_log(&engine);
    drop(engine);

    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
        let mut out: Vec<_> = artifacts_of(engine, &full_coverage_credential())
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

    let deleted = artifact_target(&engine, ids[retired]);
    engine
        .accept_changes(vec![(deleted, ChangeOp::Delete)])
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
    reclaim_log(&engine);
    drop(engine);

    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
/// **The batch name and the idempotency key are derived from `name`**, and that is not
/// tidiness: a second ingest under a batch key already seen is *replayed* rather than accepted, so
/// a helper with a fixed key silently ingests nothing the second time it is called and every flush
/// after the first has nothing to publish.
fn ingest(engine: &Engine, name: &[u8]) -> EntityId {
    let descriptors = vec![b"0".to_vec()];
    let mut key = [0u8; 32];
    for (slot, byte) in key.iter_mut().zip(name) {
        *slot = *byte;
    }
    let row = mosaica_lifecycle::command::UnallocatedRow {
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
        .ingest_rows(
            vec![row],
            String::from_utf8_lossy(name).into_owned(),
            key,
        )
        .expect("the ingest is accepted")[0]
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
    let engine = fx.open_unrefreshed();
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

    publish_buffered(&engine);
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
    let engine = fx.open_unrefreshed();
    publish(&fx, &engine, 0..300);
    assert_eq!(count(&engine), 300);

    ingest(&engine, b"unrelated");
    publish_buffered(&engine);

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
    let engine = fx.open_unrefreshed();
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
        publish_buffered(&engine);
    }

    engine.set_merge_for_test(true);
    ingest(&engine, b"merge-e");
    publish_buffered(&engine);
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
    d.content.supplied = vec![mosaica_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: mosaica_types::layer::SuppliedRequirement::All,
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
        let engine = fx.open_unrefreshed();
        engine.register_layer(content_layer()).unwrap();
        publish_described(&fx, &engine);
        assert_eq!(
            artifacts_of(&engine, &full_coverage_credential()).len(),
            1,
            "served with its description"
        );

        // Inside the generating sample, so containment fails for everyone from the ack.
        engine
            .accept_change(fx.member(7), ChangeOp::Delete)
            .expect("the delete is accepted");
        assert!(
            artifacts_of(&engine, &full_coverage_credential()).is_empty(),
            "withheld at the ack — emergent from containment, with nothing stored"
        );

        fold(&engine);
        assert!(
            artifacts_of(&engine, &full_coverage_credential()).is_empty(),
            "and still withheld after the fold: the content was withdrawn, not re-based onto a \
             smaller set"
        );
    }

    let engine = fx.open_unrefreshed();
    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
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
    let engine = fx.open_unrefreshed();
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
        artifacts_of(&engine, &full_coverage_credential()).len(),
        2,
        "both serve before the deletion"
    );

    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    let at_ack: Vec<Option<String>> = artifacts_of(&engine, &full_coverage_credential()).into_iter().map(|a| a.key).collect();
    assert_eq!(
        at_ack,
        vec![Some("c1".to_string())],
        "withheld at the ack by containment alone: the deleted document is outside every mask"
    );

    fold(&engine);

    let served = artifacts_of(&engine, &full_coverage_credential());
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

    let narrow: Vec<Option<String>> = artifacts_of(&engine, &subset_credential())
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

/// A content withdrawn at a fold stays withdrawn, and the content ranked below it is served with
/// its own value, whether the record was restored from the prefix before the fold or after it:
/// each credential is served `wide`, gated by `wide`'s set, and never `near`, whose set names the
/// deleted document and items the subset credential cannot see.
#[test]
fn the_content_below_a_withdrawn_one_keeps_its_value_across_a_restart() {
    let fx = fixture();
    let wide_set = fx.members((0..90).filter(|s| terms_of(*s).contains(&SUBSET_TERM)));
    let deleted = fx.member(7);
    let served = |engine: &Engine| -> Vec<Vec<Vec<String>>> {
        [full_coverage_credential(), subset_credential()]
            .iter()
            .map(|credential| {
                artifacts_of(engine, credential)
                    .into_iter()
                    .map(|a| a.content)
                    .collect()
            })
            .collect()
    };
    let expected = vec![vec![vec!["wide".to_string()]]; 2];
    {
        let engine = fx.open_unrefreshed();
        engine.register_layer(content_layer()).unwrap();
        engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("c0".into()),
                    fx.members(0..300),
                    vec![
                        IncomingContent::new(vec!["near".into()], fx.members(0..30)),
                        IncomingContent::new(vec!["wide".into()], wide_set),
                    ],
                )],
            )
            .unwrap();
        wait_for_publication(&fx, &engine, 1);
        // Releases the log, so the next open restores the record from the prefix alone.
        fold(&engine);
    }
    {
        let engine = fx.open_unrefreshed();
        engine
            .accept_change(deleted, ChangeOp::Delete)
            .expect("the delete is accepted");
        assert_eq!(served(&engine), expected, "at the ack");
        fold(&engine);
        assert_eq!(served(&engine), expected, "after the fold");
    }
    let engine = fx.open_unrefreshed();
    assert_eq!(served(&engine), expected, "across a restart");
}

/// **A declared member that is deleted refuses the batch; a suppressed one is accepted.** The two
/// are not near-neighbours: a deletion is irreversible, so the content would be unservable from
/// birth and the count short for ever, while a suppression is an operator's reversible action that
/// must not refuse an unrelated publication.
#[test]
fn publication_refuses_a_deleted_member_and_accepts_a_suppressed_one() {
    let fx = fixture();
    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
    let mut layer = declaration("clusters/a");
    layer.content.supplied = vec![mosaica_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: mosaica_types::layer::SuppliedRequirement::All,
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
    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
    let engine = fx.open_unrefreshed();
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
    d.content.supplied = vec![mosaica_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: mosaica_types::layer::SuppliedRequirement::Inherited,
    }];
    d.depends_on = vec![target.into()];
    d
}

/// **A deleted artifact leaves its level at the fold that executes the deletion**, and the ordinal
/// it held becomes a hole rather than closing up.
///
/// The hole is the whole of the durable state: an ordinal is identity, so packing around the gap
/// would hand every later artifact in the level the identity of its neighbour, and every
/// `mosaica_id` a caller holds beyond it would resolve to the wrong cluster.
#[test]
fn a_deleted_artifact_leaves_the_level_at_the_fold_and_its_ordinal_stays_a_hole() {
    let fx = fixture();
    let engine = fx.open_unrefreshed();
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
    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 3);

    // The middle one, so a level that packed around the gap would be caught by the survivor after
    // it rather than by a count alone.
    let deleted = artifact_target(&engine, ids[1]);
    let last_before = artifact_target(&engine, ids[2]);
    engine
        .accept_changes(vec![(deleted, ChangeOp::Delete)])
        .expect("an artifact takes a deletion like any other entity");
    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 2, "hidden at the ack");
    fold(&engine);

    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 2, "and still hidden after it");
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
/// deleted was published under. Two artifacts, one `mosaica_id`, the second answering for the first.
#[test]
fn deleting_the_last_artifact_of_a_level_does_not_hand_its_identity_to_the_next_publication() {
    let fx = fixture();
    // Resolved before the fold: `members` reads the built prefix's `id` index, and the fold
    // reclaims that prefix. Entities are stable across it, so the set is still the right one.
    let later_members = fx.members(200..300);
    let deleted = {
        let engine = fx.open_unrefreshed();
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
        let target = artifact_target(&engine, ids[1]);
        engine
            .accept_changes(vec![(target, ChangeOp::Delete)])
            .expect("the delete is accepted");
        fold(&engine);
        target
    };

    // Reopened, so the level is what the extent says rather than what this process remembered.
    let engine = fx.open_unrefreshed();
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
    let fresh = artifact_target(&engine, ids[0]);

    assert_ne!(
        fresh, deleted,
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
        let engine = fx.open_unrefreshed();
        let id = publish(&fx, &engine, 0..300);
        let target = artifact_target(&engine, id);
        engine
            .accept_changes(vec![(target, ChangeOp::Delete)])
            .expect("the delete is accepted");
        fold(&engine);
        assert!(artifacts_of(&engine, &full_coverage_credential()).is_empty());
    }

    // **Reopened, which is where a surviving slot would show.** The overlay entry is retired and
    // gone from the manifest; nothing but the absence of the record keeps the artifact away.
    let engine = fx.open_unrefreshed();
    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
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
    let engine = fx.open_unrefreshed();
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
                mosaica_lifecycle::membership::IncomingAttachment {
                    layer: "clusters/a".into(),
                    level: 0,
                    key: "c0".into(),
                },
            )],
        )
        .unwrap();
    wait_for_publication(&fx, &engine, 2);
    assert_eq!(
        artifacts_of(&engine, &full_coverage_credential()).len(),
        2,
        "the cluster and its label both serve to begin with"
    );

    let cluster = artifact_target(&engine, cluster_id);
    engine
        .accept_changes(vec![(cluster, ChangeOp::Delete)])
        .expect("the delete is accepted");
    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
        "both go at the ack: the cluster on its own disposition, the label on its target's"
    );

    fold(&engine);

    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
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
        let engine = fx.open_unrefreshed();
        let mut layer = declaration("topics/a");
        layer.content.supplied = vec![mosaica_types::layer::SuppliedContent {
            name: "topic".into(),
            ty: "text".into(),
            require_member_visibility: mosaica_types::layer::SuppliedRequirement::All,
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
        let served = artifacts_of(&engine, &full_coverage_credential());
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].content, vec!["a label from the whole sample"]);
    }

    // **Restarted, which is the half that needs the blob.** The resident copy carries the values;
    // a record restored from a packed extent does not, and the serving path reads them from the
    // blob the fold carried forward.
    let engine = fx.open_unrefreshed();
    let served = artifacts_of(&engine, &full_coverage_credential());
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
        let engine = fx.open_unrefreshed();
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
                    mosaica_lifecycle::membership::IncomingAttachment {
                        layer: "clusters/a".into(),
                        level: 0,
                        key: "c0".into(),
                    },
                )],
            )
            .unwrap();
        wait_for_publication(&fx, &engine, 2);
        assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 2);
        assert_eq!(engine.published_artifacts(), 2);

        let cluster = artifact_target(&engine, cluster_id);
        assert_eq!(
            engine.generation_status().layer_spaces.retirable,
            0,
            "nothing is deleted yet"
        );
        engine
            .accept_changes(vec![(cluster, ChangeOp::Delete)])
            .expect("the delete is accepted");

        // **Two dispositions from one command.** The cluster's own, and the label's — the cascade,
        // carried in the same window, on the same lane, with its own durable record.
        assert_eq!(
            engine.generation_status().layer_spaces.retirable,
            2,
            "the label was deleted with its cluster rather than merely withheld behind it"
        );
        assert!(artifacts_of(&engine, &full_coverage_credential()).is_empty(), "both go at the ack");

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
    let engine = fx.open_unrefreshed();
    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
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

/// **A fold leaves no membership reading through the prefix it superseded.**
///
/// A membership seeded at open is a view over that prefix's extent, and the pack holding it alive
/// is not something reclamation can see: `remove_dir_all` unlinks the directory, the mapping
/// outlives the directory entry, and the file's blocks stay allocated under no name at all. So the
/// fold reads its own extents back and the old packs go with the records that held them.
///
/// The deletion is here to reach the other half: retirement rewrites the surviving membership
/// through `Members::to_mut`, which copies it to the heap, and only the rehousing puts it back on
/// a mapping.
///
/// **Mutation:** remove the rehousing from the fold's publication and the memberships come back
/// owned, with `/proc/self/maps` still naming the superseded prefix's `members/`.
#[test]
fn a_fold_rehouses_every_membership_onto_the_prefix_it_published() {
    let fx = fixture();
    {
        let engine = fx.open_unrefreshed();
        publish(&fx, &engine, 0..300);
    }
    // **The log goes, so the seed is the extent's own doing.** A WAL record postdates any state a
    // manifest carries and replay applies it over the seed, which puts that artifact's membership
    // back on the heap — correct, and not what this case is about.
    remove_the_whole_log(&fx);

    // Reopened, so the memberships are the seed's views over the prefix's extent rather than the
    // heap bitmaps the publication built.
    let engine = fx.open_unrefreshed();
    let superseded = fx.live_prefix(&engine);
    let seeded = engine.level_memberships_for_test("clusters/a", 0);
    assert_eq!(seeded.len(), 1);
    assert!(
        seeded[0].2,
        "the seed reads a published membership through the extent that carries it"
    );

    engine
        .accept_change(fx.member(7), ChangeOp::Delete)
        .expect("the delete is accepted");
    fold(&engine);

    let folded = engine.level_memberships_for_test("clusters/a", 0);
    assert_eq!(folded.len(), 1);
    assert_eq!(
        folded[0].1.len(),
        299,
        "the retirement took the deleted member and nothing else"
    );
    assert!(
        folded[0].2,
        "and the membership the retirement rewrote is read through the extent the fold wrote"
    );

    assert_eq!(
        engine.owned_memberships_for_test(),
        0,
        "and no membership anywhere in the store is left on the heap"
    );

    // **The whole prefix, not only its memberships.** The rehousing is what releases the packs,
    // and reclamation is what removes the tree once no generation names it; a mapping into
    // anything under the old root would hold that file's blocks with no name to see them under.
    // Reclamation runs on the executor's own tick, so this waits for it rather than assuming it
    // has already run.
    let root = superseded.to_str().expect("a UTF-8 path");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let maps = std::fs::read_to_string("/proc/self/maps")
            .expect("this process's own mappings are readable");
        if !maps.contains(root) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a file under the prefix the fold superseded is still mapped, so its blocks are held \
             for the life of the process:\n{}",
            maps.lines()
                .filter(|line| line.contains(root))
                .collect::<Vec<&str>>()
                .join("\n")
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Every log member, removed — the extent is the membership's durable home, so what comes back at
/// the next open came from the prefix (`artifact_publish.rs` asserts that on its own).
fn remove_the_whole_log(fx: &Fixture) {
    let dir = fx.wal.parent().expect("the log has a directory");
    let stem = fx.wal.file_stem().expect("the log has a stem").to_owned();
    for entry in std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{}-", stem.to_string_lossy())) {
            std::fs::remove_file(entry.path()).expect("a log member is removable");
        }
    }
    let _ = std::fs::remove_file(&fx.wal);
}

// ---- a borrowed membership across a flush and a fold -------------------------------------------

/// A label attached to `clusters/a`'s one artifact, declaring no members of its own. Its membership
/// is the cluster's (decision 0145), so what these two cases read is whether the cluster's rows
/// reach it at the publication that gives the cluster them.
fn publish_borrowing_label(engine: &Engine) {
    let mut label_layer = declaration(LABELS);
    label_layer.depends_on = vec!["clusters/a".into()];
    engine.register_layer(label_layer).unwrap();
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::attached(
                Some("l0".into()),
                Vec::new(),
                Vec::new(),
                mosaica_lifecycle::membership::IncomingAttachment {
                    layer: "clusters/a".into(),
                    level: 0,
                    key: "c0".into(),
                },
            )],
        )
        .unwrap();
}

const LABELS: &str = "topics/x";

/// The masked count of one key, for a principal who can see everything.
fn count_of(engine: &Engine, layer: &str, key: &str) -> u64 {
    artifacts_of(engine, &full_coverage_credential())
        .into_iter()
        .find(|a| a.layer == layer && a.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("{layer}/{key} is served"))
        .masked_count
}

/// A member ingested since the last fold counts for the label as it counts for the cluster, from
/// the flush that gives it a row.
///
/// The form is warmed first, so what is read after the flush is a form that was already held. A
/// flush extends each held form by the segment it published, and a borrowing artifact's membership
/// is in no record of its own level, so such a form is dropped instead and the next request
/// resolves the cluster's rows again (`ArtifactProjections::drop_borrowed`). Without that the label
/// would stay at the count the cluster had when the label's form was built.
#[test]
fn a_borrowing_label_takes_its_targets_flushed_member() {
    let fx = fixture();
    let engine = fx.open_unrefreshed();
    engine.register_layer(declaration("clusters/a")).unwrap();
    let fresh = ingest(&engine, b"fresh-for-the-label");
    let mut members = fx.members(0..300);
    members.push(fresh);
    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(Some("c0".into()), members)],
        )
        .unwrap();
    publish_borrowing_label(&engine);
    wait_for_publication(&fx, &engine, 2);

    // Warm: both forms are built and held, the ingested member having no row yet.
    assert_eq!(count_of(&engine, "clusters/a", "c0"), 300);
    assert_eq!(
        count_of(&engine, LABELS, "l0"),
        300,
        "the label is counted over the cluster's members"
    );

    publish_buffered(&engine);
    assert_eq!(count_of(&engine, "clusters/a", "c0"), 301);
    assert_eq!(
        count_of(&engine, LABELS, "l0"),
        301,
        "and over them after the flush that gave the new member a row"
    );
}

/// The same membership across a fold, which renumbers every row and rewrites the level's files.
///
/// The fold writes a borrowing level's tile index and column over the membership the artifact is
/// served on, and the engine rebuilds the form against the new prefix and resolves the cluster's
/// rows again. A label counted over the cluster before the fold is counted over it after.
#[test]
fn a_borrowing_label_keeps_its_targets_membership_across_a_fold() {
    let fx = fixture();
    let engine = fx.open_unrefreshed();
    publish(&fx, &engine, 0..300);
    publish_borrowing_label(&engine);
    wait_for_publication(&fx, &engine, 2);
    assert_eq!(count_of(&engine, LABELS, "l0"), 300);

    ingest(&engine, b"folded-for-the-label");
    publish_buffered(&engine);
    fold(&engine);

    assert_eq!(count_of(&engine, "clusters/a", "c0"), 300);
    assert_eq!(
        count_of(&engine, LABELS, "l0"),
        300,
        "the fold renumbered the rows the cluster holds, and the label still reads them"
    );
}

// ---- an artifact deletion two shards owe ----------------------------------------------------------

/// Items the two-shard fixture builds, in shards of [`TWO_SIZE`] and the rest.
const TWO_N: u64 = 40;
const TWO_SIZE: u64 = 25;
/// The deleted artifact's key: bytes no other file of the bundle holds.
const ERASED: &str = "erased-cluster-7f3a9c";

fn shard(id: u32) -> mosaica_types::ShardId {
    mosaica_types::ShardId::new(id).unwrap()
}

/// An engine over the two-shard fixture that writes both shards.
fn writing(fx: &Fixture) -> Engine {
    let mut engine = Engine::open_writing_several_shards(&fx.root, &fx.cache, &fx.wal, config())
        .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

/// Each `id` of the fixture's items, as the item it names.
fn items_by_id(engine: &Engine) -> Vec<mosaica_types::ItemAt> {
    let table = mosaica_engine::AddressTable {
        rows: TWO_N as usize,
        mosaica_id: None,
        columns: vec![(
            "id".to_string(),
            (0..TWO_N)
                .map(|id| Some(mosaica_engine::AddressValue::Text(id.to_string())))
                .collect(),
        )],
    };
    engine
        .name_items(&table)
        .unwrap()
        .verdicts
        .into_iter()
        .map(|verdict| verdict.expect("every id names an item"))
        .collect()
}

/// One artifact as it is served: its key, masked count and content.
type Served = (Option<String>, u64, Vec<String>);

/// What the full viewer is served of `clusters/a` on every artifact route: the viewport's
/// artifacts, each artifact's card, its `member_of` and `region` matches, browse's roots and the
/// bulk read's rows. A refused leaf is `None`.
#[derive(Debug, PartialEq)]
struct Routes {
    viewport: Vec<Served>,
    cards: Vec<Option<Served>>,
    member_of: Vec<Option<u64>>,
    region: Vec<Option<u64>>,
    browse: Vec<(Option<String>, u64)>,
    bulk: Vec<Vec<String>>,
}

fn routes(engine: &Engine, ids: &[mosaica_types::MosaicaId]) -> Routes {
    use mosaica_engine::browse::{BrowseForm, BrowseRequest};
    use mosaica_engine::filter::{FilterExpr, MemberOfLeaf, RegionLeaf};
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let served =
        |a: &mosaica_engine::ArtifactOut| (a.key.clone(), a.masked_count, a.content.clone());
    let mut viewport: Vec<_> = artifacts_of(engine, &full_coverage_credential())
        .iter()
        .map(served)
        .collect();
    viewport.sort();
    let matched = |filter: FilterExpr| {
        let request =
            mosaica_engine::ViewportRequest::new("s0", 0, WHOLE_MAP, TWO_N as usize).filter(filter);
        let out = engine.viewport(&session, request).ok()?;
        Some(out.tiles.iter().map(|tile| tile.matched).sum())
    };
    let browse = engine
        .browse(
            &session,
            BrowseRequest {
                view: "s0",
                layer: "clusters/a",
                level: None,
                form: BrowseForm::Roots,
                filter: None,
                limit: 100,
                cursor: None,
                palette_size: None,
            },
        )
        .expect("browse answers");
    let fields = ["key", "masked_count", "content"].map(String::from);
    let mut bulk: Vec<Vec<String>> = drain(|cursor, pages| {
        engine
            .artifacts_stream(
                &session,
                mosaica_engine::ArtifactsRequest {
                    view: "s0",
                    layer: "clusters/a",
                    level: None,
                    parent: None,
                    q: None,
                    ids: None,
                    filter: None,
                    keep_unmatched: false,
                    count: cursor.is_none(),
                    fields: &fields,
                    palette_size: None,
                    page_rows: Some(2),
                    pages: Some(1),
                    cursor,
                    limits: limits(),
                    cancel: None,
                },
                pages,
            )
            .expect("the artifacts read answers")
            .next
    })
    .iter()
    .flat_map(|response| &response.batches)
    .flat_map(|batch| rows_of(batch, &["mosaica_id"], None))
    .collect();
    bulk.sort();
    Routes {
        viewport,
        cards: ids
            .iter()
            .map(|id| {
                engine
                    .artifact(&session, *id, "s0", None)
                    .expect("the card answers")
                    .as_ref()
                    .map(served)
            })
            .collect(),
        member_of: ids
            .iter()
            .map(|id| {
                matched(FilterExpr::MemberOf(MemberOfLeaf {
                    layer: "clusters/a".to_string(),
                    artifact: *id,
                }))
            })
            .collect(),
        region: ids
            .iter()
            .map(|id| matched(FilterExpr::Region(RegionLeaf::Artifact(*id))))
            .collect(),
        browse: browse
            .artifacts
            .iter()
            .map(|row| (row.key.clone(), row.masked_count))
            .collect(),
        bulk,
    }
}

/// What the live prefix holds of the artifact at `entity` of `space`: whether any file holds its
/// key, whether a content extent of its space holds its row, and the ordinals of `space` each
/// shard's member packs list.
fn held_on_disc(
    root: &std::path::Path,
    space: mosaica_types::LayerSpace,
    entity: EntityId,
) -> (bool, bool, Vec<Vec<u32>>) {
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            match entry.path().is_dir() {
                true => files(&entry.path(), out),
                false => out.push(entry.path()),
            }
        }
    }
    let prefix = root.join(current_prefix(root));
    let mut all = Vec::new();
    files(&prefix, &mut all);
    let key = ERASED.as_bytes();
    let holds_key = all.iter().any(|path| {
        std::fs::read(path)
            .unwrap()
            .windows(key.len())
            .any(|window| window == key)
    });
    let manifest = side_manifest(root);
    let holds_content = mosaica_store::manifest::layer_space(&manifest.layer_spaces, space)
        .into_iter()
        .flat_map(|record| &record.content_extents)
        .any(|extent| {
            let bytes = std::fs::read(prefix.join(&extent.hasrow)).unwrap();
            croaring::Bitmap::try_deserialize::<croaring::Portable>(&bytes)
                .expect("a has-row bitmap")
                .contains(entity.raw() as u32)
        });
    let listed = manifest
        .shards
        .records
        .iter()
        .map(|record| {
            let mut ordinals: Vec<u32> = record
                .membership_extents
                .iter()
                .filter(|extent| extent.space == space)
                .flat_map(|extent| {
                    let pack = mosaica_store::membership::LevelPack::open(
                        &prefix.join(&extent.path),
                        mosaica_store::membership::PackKind::Members,
                    )
                    .unwrap();
                    pack.iter()
                        .filter(|(_, blob)| !blob.is_empty())
                        .map(|(ordinal, _)| ordinal)
                        .collect::<Vec<_>>()
                })
                .collect();
            ordinals.sort_unstable();
            ordinals.dedup();
            ordinals
        })
        .collect();
    (holds_key, holds_content, listed)
}

/// The shards the served side-manifest says owe `entity`'s deletion in `space`, and whether the
/// space still holds the deletion.
fn owed(
    root: &std::path::Path,
    space: mosaica_types::LayerSpace,
    entity: EntityId,
) -> (Vec<u32>, bool) {
    let manifest = side_manifest(root);
    let Some(record) = mosaica_store::manifest::layer_space(&manifest.layer_spaces, space) else {
        return (Vec::new(), false);
    };
    let at = entity.raw() as u32;
    let shards = record
        .owed
        .iter()
        .filter(|owed| owed.artifacts.entities().unwrap().contains(at))
        .map(|owed| owed.shard.raw())
        .collect();
    (shards, record.tombstones.entities().unwrap().contains(at))
}

/// An artifact with members and a generating set in both shards is deleted. It is served on no
/// route from the acknowledgement; the compaction of the first shard takes it out of that shard's
/// member pack and leaves its record, its content and the other shard's slice on the disc, still
/// denied and still counted; the second's removes it from every file. A restart between the two,
/// and one after, changes no answer. A deletion no shard owes leaves at the first compaction.
fn an_artifact_deletion_two_shards_owe(first: u32, second: u32) {
    let fx = fixture_sized(TWO_N, Some(TWO_SIZE));
    let engine = writing(&fx);
    engine.register_layer(content_layer()).unwrap();
    engine
        .register_layer(flat_declaration("clusters/b"))
        .unwrap();
    let items = items_by_id(&engine);
    let in_shard = |at: u32| -> Vec<u64> {
        (0..TWO_N)
            .filter(|id| items[*id as usize].shard == shard(at))
            .collect()
    };
    let (zero, one) = (in_shard(0), in_shard(1));
    assert!(zero.len() >= 4 && one.len() >= 4, "items in both shards");
    let sets = |ids: &[u64]| {
        mosaica_lifecycle::ShardSets::of_items(ids.iter().map(|id| items[*id as usize]))
    };
    let artifact = |key: &str, members: Vec<u64>, from: Vec<u64>| {
        let mut artifact = IncomingArtifact::with_content(
            Some(key.into()),
            Vec::new(),
            vec![IncomingContent::new(
                vec![format!("topic of {key}")],
                Vec::new(),
            )],
        );
        artifact.members = sets(&members);
        artifact.contents[0].generated_from = sets(&from);
        artifact
    };
    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![
                artifact(
                    "kept",
                    vec![zero[0], zero[1], one[0], one[1]],
                    vec![zero[0], one[0]],
                ),
                artifact(
                    ERASED,
                    vec![zero[2], zero[3], one[2], one[3]],
                    vec![zero[2], one[2]],
                ),
            ],
        )
        .unwrap();
    let memberless = engine
        .publish_artifacts(
            "clusters/b".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("none".into()),
                Vec::<EntityId>::new(),
            )],
        )
        .unwrap();
    let Target::Artifact(space, erased) = artifact_target(&engine, ids[1]) else {
        panic!("an artifact's target")
    };
    let Target::Artifact(empty_space, empty) = artifact_target(&engine, memberless[0]) else {
        panic!("an artifact's target")
    };
    tick_until(
        &engine,
        "both shards' member packs and the content",
        std::time::Duration::from_secs(30),
        || {
            let (_, content, listed) = held_on_disc(&fx.root, space, erased);
            content && listed == vec![vec![0, 1], vec![0, 1]]
        },
    );
    let (holds_key, _, _) = held_on_disc(&fx.root, space, erased);
    assert!(
        holds_key,
        "the record pack holds the key, so its absence below is a removal"
    );

    engine
        .accept_changes(vec![
            (Target::Artifact(space, erased), ChangeOp::Delete),
            (Target::Artifact(empty_space, empty), ChangeOp::Delete),
        ])
        .unwrap();
    tick_until(
        &engine,
        "the deletions' publication",
        std::time::Duration::from_secs(30),
        || owed(&fx.root, space, erased).1 && owed(&fx.root, empty_space, empty).1,
    );
    assert_eq!(owed(&fx.root, space, erased), (vec![0, 1], true));
    assert_eq!(
        owed(&fx.root, empty_space, empty),
        (vec![], true),
        "owed by no shard"
    );
    let deleted = routes(&engine, &ids);
    assert_eq!(
        deleted
            .viewport
            .iter()
            .map(|a| a.0.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("kept")]
    );
    assert!(deleted.cards[0].is_some() && deleted.cards[1].is_none());
    assert_eq!(deleted.member_of, vec![Some(4), Some(0)]);
    assert_eq!(deleted.region, vec![Some(4), Some(0)]);
    assert_eq!(
        deleted
            .browse
            .iter()
            .map(|row| row.0.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("kept")]
    );
    assert!(deleted.bulk.len() == 1 && deleted.bulk[0].contains(&"key=kept".to_string()));
    assert_eq!(engine.generation_status().layer_spaces.retirable, 2);

    assert_eq!(compact(&engine, Some(shard(first))), 1);
    assert_eq!(routes(&engine, &ids), deleted);
    assert_eq!(owed(&fx.root, space, erased), (vec![second], true));
    assert_eq!(
        owed(&fx.root, empty_space, empty),
        (vec![], false),
        "the deletion no shard owes leaves at the first compaction"
    );
    assert_eq!(engine.generation_status().layer_spaces.retirable, 1);
    let mut listed = vec![vec![0, 1], vec![0, 1]];
    listed[first as usize] = vec![0];
    assert_eq!(
        held_on_disc(&fx.root, space, erased),
        (true, true, listed.clone()),
        "the first shard's member pack no longer lists it; its record, content and the other \
         shard's slice stay"
    );

    drop(engine);
    let engine = writing(&fx);
    assert_eq!(routes(&engine, &ids), deleted, "a restart changes nothing");
    assert_eq!(
        engine
            .locate_artifact(Target::Artifact(space, erased))
            .and_then(|at| at.key),
        Some(ERASED.to_string()),
        "the record is held, though one slice no longer holds its part of the set"
    );
    assert_eq!(owed(&fx.root, space, erased), (vec![second], true));
    assert_eq!(engine.generation_status().layer_spaces.retirable, 1);
    assert_eq!(held_on_disc(&fx.root, space, erased), (true, true, listed));

    assert_eq!(compact(&engine, Some(shard(second))), 1);
    assert_eq!(routes(&engine, &ids), deleted);
    assert_eq!(owed(&fx.root, space, erased), (vec![], false));
    assert_eq!(engine.generation_status().layer_spaces.retirable, 0);
    assert_eq!(
        held_on_disc(&fx.root, space, erased),
        (false, false, vec![vec![0], vec![0]]),
        "no file of the new prefix holds its record, its content or its members"
    );
    assert_eq!(
        engine
            .locate_artifact(Target::Artifact(space, erased))
            .and_then(|at| at.key),
        None
    );

    drop(engine);
    let engine = writing(&fx);
    assert_eq!(
        routes(&engine, &ids),
        deleted,
        "nor does a restart bring it back"
    );
    assert_eq!(engine.generation_status().layer_spaces.retirable, 0);
}

#[test]
fn an_artifact_deletion_two_shards_owe_leaves_at_the_second_compaction() {
    an_artifact_deletion_two_shards_owe(1, 0);
}

#[test]
fn an_artifact_deletion_leaves_the_write_shards_slice_before_the_other_shards() {
    an_artifact_deletion_two_shards_owe(0, 1);
}
