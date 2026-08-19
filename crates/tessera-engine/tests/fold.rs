//! **The compaction fold, end to end** — compaction §12's obligations, against a real fold rather
//! than a stand-in.
//!
//! `tests/prefix_rotation.rs` covers the publication *seam* by publishing a copy of the live prefix
//! under a new `MANIFEST.json`: it models the identity rotation and nothing about a fold's content.
//! These cases run the thing itself — five passes into a new prefix, the rebase, the `CURRENT`
//! flip, the swap, the WAL rotation and the reclamation — so what they pin is the half the seam
//! cases deliberately do not: that a folded deletion loses its row, its postings **and** its
//! overlay entry together, and that everything the fold did not fold is still there afterwards.

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use tessera_authz::{PostingRef, PostingsReader};
use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::ChangeOp;
use tessera_store::read::open_bundle;
use tessera_types::{EntityId, TermId, TesseraId};

/// A fixture bundle and an engine over it, with the executor running and the background refresh
/// off.
///
/// The refresh is off for `tests/prefix_rotation.rs`'s reason: a pass running after the flip
/// rebuilds each resident session's fragment, which is correct and would make a request-path check
/// that *failed* to notice the rotation indistinguishable from one that noticed.
fn engine_over_fixture(tmp: &Path, root: &Path, config: EngineConfig) -> Engine {
    build_fixture(
        root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
    .expect("engine should open against a freshly built bundle");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

/// Term id for the sparse principal `masked_counts_are_identical_across_the_flip_for_every_
/// principal` grants below: `subset_credential` (`SUBSET_TERM`) sits at roughly a third of the
/// fixture, which obligation 5's "several principals including a sparse one" does not really
/// exercise — a third is not sparse, it is just smaller. This term is carried by one item in
/// `SPARSE_STRIDE`, offset so source 3 — the entity that test already deletes, because it is the
/// one item both `full` and `subset` can see — carries it too: every principal's count must move
/// by exactly that one deletion, sparse one included.
///
/// `common::terms_of`'s two terms are fixed, so a sparser one is not expressible through
/// `common::build_fixture` without changing a fixture every other test in this binary shares —
/// this builds its own corpus instead, duplicating `common`'s writer functions rather than
/// widening their contract for one test.
const SPARSE_TERM: u64 = 2;
const SPARSE_STRIDE: u64 = 97;

fn sparse_credential() -> Vec<u8> {
    br#"{"terms": ["2"]}"#.to_vec()
}

/// `common::write_pairs_n`'s term assignment, plus `SPARSE_TERM` for source ids congruent to 3
/// mod `SPARSE_STRIDE` — about a hundredth of the fixture, an order below `SUBSET_TERM`'s third.
fn write_pairs_with_sparse_term(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..n {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
        if e % SPARSE_STRIDE == 3 {
            entities.push(e);
            terms.push(SPARSE_TERM as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// A fixture whose corpus is otherwise identical to [`common::build_fixture`]'s, with `SPARSE_TERM`
/// added — see [`write_pairs_with_sparse_term`].
fn build_fixture_with_sparse_term(out: &Path, points_path: &Path, pairs_path: &Path) {
    write_points_n(points_path, N_ITEMS);
    write_pairs_with_sparse_term(pairs_path, N_ITEMS);
    let args = BuildArgs {
        point_fields: Default::default(),
        corpus_fields: Default::default(),
        points: points_path.to_path_buf(),
        corpus: Some(points_path.to_path_buf()),
        access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        out: out.to_path_buf(),
        extent: extent(),
        view_id: "s0".to_string(),
        // No declared columns: this fixture's subject is the sparse *term*, not the scalar tail,
        // and an empty schema is what `common`'s builder uses for the same reason.
        schema: Default::default(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
    };
    build(&args).expect("sparse-term fixture build should succeed");
}

/// [`engine_over_fixture`], over [`build_fixture_with_sparse_term`]'s corpus rather than
/// `common::build_fixture`'s.
fn engine_over_fixture_with_sparse_term(tmp: &Path, root: &Path, config: EngineConfig) -> Engine {
    build_fixture_with_sparse_term(
        root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
    .expect("engine should open against a freshly built bundle");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

/// Request a fold and block until it has published, asserting it was not discarded.
///
/// The counter is the deterministic wait: the fold runs on its own thread and its publication
/// happens at the executor's next loop iteration, so there is no "done" to poll but the one the
/// publication increments.
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

/// Block until `cond` holds, or fail naming what was waited on. Each call gets its own deadline —
/// a single deadline shared across a test's several waits expires in whichever one happens to be
/// last, which reports the wrong step.
fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Request a fold and block until it has been **discarded**, asserting nothing was published.
fn fold_discarded(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(now.folds, before.folds, "the fold published");
        if now.fold_failures > before.fold_failures {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold neither published nor was discarded"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn visible(engine: &Engine, session: &tessera_engine::Session) -> u64 {
    engine
        .viewport(
            session,
            ViewportRequest::new(
                "s0",
                0,
                [0.0, 0.0, 1000.0, 1000.0],
                (N_ITEMS + 100) as usize,
            ),
        )
        .unwrap()
        .tiles
        .iter()
        .map(|tile| tile.visible)
        .sum()
}

/// The entity id `tessera-build` assigned to fixture source row `source_id`, resolved through the
/// prefix `CURRENT` names at the time of the call.
fn entity_of_source(root: &Path, prefix: &str, source_id: u64) -> EntityId {
    EntityId::new(source_to_new_map(root, prefix)[&source_id])
}

/// Whether the new prefix's base postings still name `entity` under `term`.
///
/// **This is the postings half of obligation 1, and nothing else here can stand in for it.** A
/// viewport count cannot: an entity with no row is invisible whatever its postings say, so the
/// count would pass a fold that rewrote row space and left the term index alone — which is exactly
/// the "both halves or neither" architecture §11.3's r33 ruling forbids, because Rule F's
/// retirement would then re-expose the item.
fn postings_name(postings: &PostingsReader, term: TermId, entity: EntityId) -> bool {
    let entity = entity.raw() as u32;
    match postings.posting(term).expect("the postings file reads") {
        None => false,
        Some(PostingRef::Roaring(view)) => view.contains(entity),
        Some(PostingRef::Array(bytes)) => bytes
            .chunks_exact(4)
            .any(|c| u32::from_le_bytes(c.try_into().unwrap()) == entity),
    }
}

fn base_postings_of(root: &Path, prefix: &str) -> PostingsReader {
    PostingsReader::open(
        &root
            .join(prefix)
            .join("partitions/default/terms/postings.arrow"),
        false,
    )
    .expect("the folded prefix's base postings open")
}

/// The term id `ALL_TERM`'s descriptor was interned at, read from `prefix`'s own dictionary.
///
/// **Resolved, not assumed.** `public` is reserved at term 0 by every build
/// (`per-point-attributes.md` §3.8), so a descriptor's ordinal is a fact about the corpus rather
/// than a constant a test may spell.
fn all_term_of(root: &Path, prefix: &str) -> TermId {
    let bundle = open_bundle(root).expect("the bundle opens");
    let paths: Vec<PathBuf> = bundle.partitions["default"]
        .manifest
        .dict_extents
        .iter()
        .map(|extent| root.join(prefix).join(&extent.path))
        .collect();
    tessera_authz::Dict::load(&paths)
        .expect("the dictionary loads")
        .lookup(ALL_TERM.to_string().as_bytes())
        .expect("every fixture item carries ALL_TERM")
}

/// Ingest one item at (5, 5) carrying the fixture's `ALL_TERM`, under `external_id`.
fn ingest(
    engine: &Engine,
    external_id: Vec<u8>,
    batch: &str,
) -> Result<EntityId, tessera_engine::AcceptError> {
    ingest_with_descriptors(engine, external_id, batch, &[b"0".to_vec()])
}

/// [`ingest`] with the descriptor set spelled out — `&[]` for a **zero-term item**, which no tier
/// names at all and which is therefore reachable only through the run its locator extent covers
/// (compaction §12's obligation 2b).
fn ingest_with_descriptors(
    engine: &Engine,
    external_id: Vec<u8>,
    batch: &str,
    descriptors: &[Vec<u8>],
) -> Result<EntityId, tessera_engine::AcceptError> {
    let row = UnallocatedRow {
        external_id: Some(external_id),
        view: "s0".to_string(),
        descriptors: descriptors.to_vec(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(descriptors),
    };
    engine
        .accept_ingest(vec![row], batch.to_string(), [0u8; 32])
        .map(|ids| ids[0])
}

/// Every WAL member file in `dir`, by name and bytes.
///
/// The log is a **sequence** — `wal-000001.log` and its `.sync` sidecar, beside the stem the engine
/// was opened with — not one file, so copying the stem copies nothing. Rotation seals a member,
/// opens the next and reclaims the ones behind it, which is exactly the state
/// `a_restart_before_the_rotation_…` needs to take a copy of before the fold rotates.
fn snapshot_wal(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut members: Vec<(std::ffi::OsString, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("wal-"))
        .map(|e| (e.file_name(), std::fs::read(e.path()).unwrap()))
        .collect();
    assert!(
        !members.is_empty(),
        "the engine writes a WAL sequence, not one file"
    );
    members.sort_by(|a, b| a.0.cmp(&b.0));
    members
}

/// Put `snapshot` back, removing whatever members are there now — the on-disc state a crash between
/// the `CURRENT` flip and the WAL rotation leaves.
fn restore_wal(dir: &Path, snapshot: &[(std::ffi::OsString, Vec<u8>)]) {
    for entry in std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with("wal-") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    for (name, bytes) in snapshot {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("{} should exist: {e}", path.display()))
        .ino()
}

/// **Obligation 1: all three halves of one deletion, in one test.**
///
/// A folded entity's row is absent from the new base, its postings are absent from the new term
/// index, and its overlay entry is retired. Two of the three passing is the fail-open Rule F's
/// identity match exists to prevent: retirement withdraws the only thing hiding an item, so an
/// entity that kept either its row or its postings is served to every authorised principal
/// afterwards.
///
/// **Mutations this kill:** handing the passes `executed` rather than `D₀` (nothing is dropped, so
/// the row and the postings both survive and the item reappears); publishing without the retirement
/// set (depth stays 1); dropping the row but carrying `postings.arrow` forward (the postings
/// assertion fails — and the item stays invisible, which is why the count alone is not enough).
#[test]
fn all_three_halves_of_one_deletion() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let deleted = entity_of_source(&root, "v00000", 4);
    let survivor = entity_of_source(&root, "v00000", 5);
    let baseline = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    assert_eq!(engine.overlay_depth(), 1);

    fold(&engine);

    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        engine.overlay_depth(),
        0,
        "the tombstone retired: nothing carried its entity forward"
    );

    let bundle = open_bundle(&root).expect("the folded bundle opens on its own");
    let row_space = &bundle.partitions["default"].views["s0"].row_space;
    assert!(
        row_space.row_of(deleted).is_none(),
        "the folded entity has no row in the new base"
    );
    assert!(
        row_space.row_of(survivor).is_some(),
        "and every surviving entity still has one"
    );

    let postings = base_postings_of(&root, "v00001");
    assert!(
        !postings_name(&postings, all_term_of(&root, "v00001"), deleted),
        "the folded entity is in no posting of the new term index — both halves, or Rule F's \
         retirement re-exposes it"
    );
    assert!(
        postings_name(&postings, all_term_of(&root, "v00001"), survivor),
        "and a surviving entity still is, so the sweep did not simply empty the file"
    );

    let after = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &after),
        baseline - 1,
        "one item fewer, and the tombstone that used to hide it is gone"
    );
}

/// **Geometry is byte-exact across the fold, and row space is dense** (obligation 6).
///
/// Every surviving entity resolves to a row, the row count is exactly the survivors, and the item
/// each entity resolves to is the same one. The last is what a dequantise-then-requantise round
/// trip in pass 1 would break — silently, since no row count would show it — and it is checked
/// through `tessera_id`, which the segment stores and which inverts to the entity.
///
/// **Mutation this kills:** scattering `permutation.bin` from an input row index rather than a
/// count of rows emitted (every row after the dropped one is then off by one, so the entities and
/// their coordinates disagree).
#[test]
fn every_surviving_row_keeps_its_own_identity_and_row_space_is_dense() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // Resolved before the fold: entity ids are stable across it (§5.1, and the fold does not
    // renumber the entity axis), but the folded run no longer carries a deleted entity's key —
    // which is what `external_ids_resolve_both_ways_after_a_fold_…` asserts on purpose.
    let source_to_entity = source_to_new_map(&root, "v00000");
    let deleted: Vec<EntityId> = [4u64, 11, 900]
        .iter()
        .map(|s| EntityId::new(source_to_entity[s]))
        .collect();
    for entity in &deleted {
        engine
            .accept_change(*entity, ChangeOp::Delete)
            .expect("a delete is accepted");
    }

    fold(&engine);

    let bundle = open_bundle(&root).expect("the folded bundle opens");
    let view = &bundle.partitions["default"].views["s0"];
    let row_space = &view.row_space;
    assert_eq!(
        row_space.total_rows(),
        N_ITEMS - deleted.len() as u64,
        "row space holds exactly the survivors"
    );

    let segment = &view.segments[0];
    let mut rows_seen = std::collections::BTreeSet::new();
    for source in 0..N_ITEMS {
        let entity = EntityId::new(source_to_entity[&source]);
        match row_space.row_of(entity) {
            None => assert!(
                deleted.contains(&entity),
                "only a deleted entity loses its row"
            ),
            Some(row) => {
                assert!(!deleted.contains(&entity));
                assert!(rows_seen.insert(row.raw()), "two entities claim one row");
                // The stored `tessera_id` at that row must invert to this entity: the row is the
                // entity's own, not the one that used to sit there.
                assert_eq!(
                    segment.columns.tessera_id()[row.raw() as usize],
                    engine.tessera_id_of(entity).unwrap().raw(),
                    "the row at {row:?} belongs to a different entity than the permutation says"
                );
            }
        }
    }
    assert_eq!(
        *rows_seen.last().expect("some row survived") as u64,
        N_ITEMS - deleted.len() as u64 - 1,
        "row ids are dense: a dropped row shifts every row after it rather than leaving a hole"
    );
}

/// **A retired entity's external id is re-ingestible** — the fold's other half of decision 0047,
/// and the one that lives in memory rather than in a file.
///
/// Pass 3 drops `D₀`'s keys from the folded run, and that alone is not enough: the write path's own
/// `established` map is consulted first and is never rebuilt, so a binding left standing there
/// resolves the external id to an entity retirement has just made not-deleted — and both duplicate
/// checks exempt only a *deleted* holder. The re-ingest is then refused **409**, permanently,
/// contradicting decision 0047's "edit is delete plus re-ingest" directly.
///
/// **Mutations this kills:** dropping `LiveState::forget_established` (the re-ingest below fails
/// with `DuplicateExternalId`); pruning the inverse map without the forward one (same); pruning
/// after the swap instead of before it (a window rather than a failure, so this asserts the
/// ordering only in as much as it asserts the outcome — the ordering's argument is at the
/// function).
#[test]
fn a_retired_entitys_external_id_is_re_ingestible() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // An item ingested in *this* process, so its external id is in the live `established` map —
    // which is what the bundle's own sidecar cannot stand in for.
    let key = b"re-ingest-me".to_vec();
    let entity = ingest(&engine, key.clone(), "first").expect("the first ingest is accepted");
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().flushes == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    engine
        .accept_change(entity, ChangeOp::Delete)
        .expect("a delete is accepted");

    fold(&engine);
    assert_eq!(engine.overlay_depth(), 0, "the deletion retired");

    let reborn = ingest(&engine, key.clone(), "second")
        .expect("a lawful re-ingest of a retired entity's external id must not 409");
    assert_ne!(reborn, entity, "a re-ingest takes a fresh entity id (I9)");
    assert_eq!(
        engine.resolve_external_id(&key).unwrap(),
        Some(reborn),
        "and the key now names the new holder"
    );
}

/// **Obligation 13: the old prefix is reclaimed whole, and nothing live is unlinked — asserted by
/// inode.**
///
/// The dictionary is carried forward verbatim at every fold (pass 4), so its extents are the
/// carry-forward every fold has. Hard-linking gives the same inode a second directory entry, so
/// deleting the old tree unlinks a name and never bytes; a *copy* would pass an
/// "is the file still there" check while doubling the disc the fold exists to halve.
///
/// **Mutations this kills:** copying instead of linking (the inodes differ); reclaiming nothing
/// (the old prefix survives); reclaiming before the carry-forwards are linked (the dictionary is
/// unreadable and the restart below fails).
#[test]
fn the_old_prefix_is_reclaimed_whole_and_its_carry_forwards_are_links_not_copies() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let dict_extent = {
        let bundle = open_bundle(&root).unwrap();
        bundle.partitions["default"].manifest.dict_extents[0]
            .path
            .clone()
    };
    let before = inode_of(&root.join("v00000").join(&dict_extent));

    fold(&engine);

    assert_eq!(
        inode_of(&root.join("v00001").join(&dict_extent)),
        before,
        "a carried-forward file is a second name for the same inode, not a copy"
    );
    assert!(
        !root.join("v00000").exists(),
        "and the superseded prefix is gone whole — the build's base, every consumed tier and \
         every superseded side-manifest"
    );

    // The bytes survived the unlink, which is the property the link buys.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(visible(&engine, &session) > 0);
}

/// **Obligation 9: the fold discards rather than forces when a merge published under it, leaving
/// orphans and a re-plannable state.**
///
/// A fold plans against a snapshot and spends minutes to hours away from it. If a merge or coalesce
/// publishes in that time, the artefacts the fold consumed are no longer the live ones — its new
/// base was folded from segments the bundle has since replaced — and publishing anyway would drop
/// every row the merge wrote. The rebase check is what stops that, and **its answer is to throw the
/// fold away**: hours of IO discarded rather than a bundle silently missing rows. Suspension makes
/// this rare; the check is what makes it safe (compaction §7).
///
/// # Constructing the window, which is real and not a contrivance
///
/// Merge and coalesce are suspended for `fold_in_flight`'s duration, so this cannot be produced by
/// racing them from outside. The window is the one the fold's own thread opens: it clears
/// `fold_in_flight` **after** sending its result, so an executor already inside `tick_if_due` reads
/// the cleared flag and dispatches a merge that publishes before the completed fold is drained.
/// Microseconds wide in production. `set_fold_publication_paused_for_test` holds the completed fold
/// undrained, which is that state exactly, for as long as the test needs it.
///
/// # What it pins is the outcome, because three guards catch this and each masks the next
///
/// Measured by mutation, and worth stating because it is not what a reader would guess. A merge
/// disturbs three things at once — the segment list, the tiers and runs beneath it, and the entity
/// span the locator covers — and `publish_fold` checks all three independently. Disabling the
/// consumed-artefact rebase check alone still discards, at *"a carried-forward segment begins below
/// the fold's own base permutation"*; disabling that too still discards, at *"a carried-forward
/// locator extent begins below the fold's own base locator"*. Only with all three gone does the
/// fold get past them, and then this test fails.
///
/// So the mutation it kills is **the conjunction**, not any one check, and the assertions are
/// written to the property rather than to a mechanism: discarded rather than published, the merge's
/// rows still served, orphans left, and the next fold able to plan. A test written against one
/// check would pass while that check was deleted, which is the failure this note exists to prevent
/// someone rediscovering.
///
/// It does also kill, on its own: counting a discard as a publication, and a discard that leaves
/// the node unable to plan again.
#[test]
fn a_fold_discards_when_a_merge_published_under_it_and_the_state_is_re_plannable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // **The bundle must already hold segments the merge will want, or this case is the benign
    // one.** Measured, not supposed: with a fold planned against a fresh bundle, the merge selects
    // only the post-snapshot flush segments, the fold's own inputs are all still listed, the rebase
    // check correctly passes and the fold publishes. The harmful interleaving needs the merge to
    // consume something the *plan* named — so three segments exist before the fold is requested
    // (`tier_width` is 4, so three is one short of selecting) and the fourth arrives during its
    // flight.
    let mut round = 0usize;
    let publish_one = |engine: &Engine, round: &mut usize| {
        ingest(
            engine,
            format!("merge-{}", *round).into_bytes(),
            &format!("m{}", *round),
        )
        .expect("ingest is accepted");
        *round += 1;
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_for("a flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
    };
    publish_one(&engine, &mut round);
    publish_one(&engine, &mut round);
    publish_one(&engine, &mut round);
    let before = engine.write_executor_stats();
    assert_eq!(
        before.merges, 0,
        "nothing has merged yet — the base segment is not selectable at this size, so three flush \
         segments are one short of `tier_width`"
    );

    // Both hooks, and each opens a different half of the window. `fold_paused` holds the thread
    // after its passes, which is how this waits on "the passes are done" rather than guessing;
    // releasing it lets the thread send and clear `fold_in_flight`, which lifts merge's suspension.
    // `fold_publication_paused` then keeps the executor from draining the result, which is the
    // state a real fold occupies for microseconds.
    engine.set_fold_publication_paused_for_test(true);
    engine.set_fold_paused_for_test(true);
    engine.request_fold();
    wait_for("the fold's passes to finish", || {
        engine.fold_is_holding_for_test()
    });
    engine.set_fold_paused_for_test(false);

    // The fourth flush segment. The merge now selects four adjacent same-tier segments — three of
    // which the held fold's plan named as its own inputs.
    publish_one(&engine, &mut round);
    // **The tick is pulled while waiting, and that is not impatience.** `dispatch_merge` runs from
    // `tick_if_due` and nowhere else, so the merge's chance to dispatch is a tick — and the tick
    // that followed the fourth flush may have read `fold_in_flight` before the fold's thread
    // cleared it, in which case merge was still suspended. On an idle engine the next tick is
    // `flush_max_age_secs` away. A flush request with an empty buffer publishes nothing and pulls
    // the tick, which is the one operator lever that does (`request_flush`).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().merges == before.merges {
        assert!(
            std::time::Instant::now() < deadline,
            "no merge published under the held fold, so this case never arose: {:?}",
            engine.write_executor_stats()
        );
        engine.request_flush();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let held = engine.write_executor_stats();
    assert_eq!(
        (held.folds, held.fold_failures),
        (before.folds, before.fold_failures),
        "the fold must still be held, undrained, at the moment the merge has published"
    );

    engine.set_fold_publication_paused_for_test(false);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().fold_failures == before.fold_failures {
        assert!(
            std::time::Instant::now() < deadline,
            "the held fold was neither published nor discarded: {:?}",
            engine.write_executor_stats()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        engine.write_executor_stats().folds,
        before.folds,
        "a discard is not a publication — the counter an operator alarms on must not move"
    );

    // **Orphans, and CURRENT untouched.** The discarded fold's five passes are on disc under a
    // prefix nothing names; the bundle is still the one it planned against, plus the merge.
    assert!(
        root.join("v00001").exists(),
        "the discarded fold's output stands as an orphan (compaction §7), which is what the \
         startup sweep exists for"
    );
    assert!(
        root.join("v00000").exists(),
        "and every artefact it consumed is still live — nothing was reclaimed on this path"
    );

    // **Re-plannable**, which is the half a discard that wedged the node would fail: the next fold
    // plans against the merged bundle and publishes, into a prefix past the orphan.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let live = visible(&engine, &session);
    assert_eq!(
        live,
        N_ITEMS + round as u64,
        "the merge's rows survived the discard — publishing the stale fold would have dropped them"
    );
    fold(&engine);
    assert!(
        root.join("v00002").exists(),
        "the next fold takes a fresh prefix"
    );
    let after = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &after),
        live,
        "and it folds the merged bundle, losing nothing"
    );
}

/// **Compaction §7's startup sweep: an orphaned prefix is reclaimed when a write executor starts,
/// and the live one is not.**
///
/// A discarded fold — or a process that exits mid-fold — leaves a complete prefix under a name
/// `CURRENT` never took, and *nothing* used to come back for it: `next_prefix_name` steps past an
/// orphan by construction, and a later fold's own reclamation takes only the prefix it superseded.
/// Each occurrence therefore cost a bundle of disc until an operator noticed, which is what made
/// the interval floor on a discarded fold the only thing between one bad configuration value and a
/// full device (compaction §8).
///
/// The orphan here is planted rather than produced by a discard, and deliberately: what is under
/// test is the sweep's selection rule, and constructing a real discard would make the case depend
/// on whichever discard cause happened to be reachable.
///
/// **Mutations this kills** (each run): sweeping nothing — the orphan survives; treating any
/// directory as a prefix — `stray/` is gone; moving the sweep to `Engine::open` — the read-only
/// engine takes the orphan before any executor exists; moving it onto the executor thread — the
/// assertion below runs before the thread is scheduled.
///
/// **What it deliberately does not cover is the live prefix**, and the reason is that it cannot:
/// removing `name == live` from the filter changes nothing observable, because `reclaim_prefix`
/// reads `CURRENT` itself and refuses. That refusal is the load-bearing guard and it is tested
/// where it lives (`tessera-store/tests/reclaim.rs`); the filter here is the cheaper first pass, and
/// a test that appeared to cover it would be crediting this file for the store's work.
#[test]
fn the_startup_sweep_reclaims_an_orphaned_prefix_and_leaves_everything_else() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    {
        let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());
        drop(engine);
    }

    // What a discarded fold leaves: a complete-looking tree under a name CURRENT never took.
    let orphan = root.join("v00007");
    std::fs::create_dir_all(orphan.join("partitions/default/terms")).unwrap();
    std::fs::write(orphan.join("MANIFEST.json"), b"{}").unwrap();
    // And something that is not a prefix at all, which the sweep must not guess about.
    let stray = root.join("stray");
    std::fs::create_dir_all(&stray).unwrap();

    // **An engine with no write executor sweeps nothing** — a node that has not declared itself
    // the bundle's writer does not delete another process's trees.
    let reader = Engine::open(
        &root,
        &tmp.path().join("cache-r"),
        &tmp.path().join("wal-r.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("a read-only engine opens");
    assert!(orphan.exists(), "no executor, no sweep");
    drop(reader);

    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the engine reopens");
    engine.start_write_executor(8).expect("the executor starts");

    // **No wait, and its absence is the assertion.** The sweep runs synchronously inside
    // `start_write_executor`, before the thread is spawned, so "it has returned" *is* "the sweep
    // has finished". On the thread instead it would race whatever the caller does next — which is
    // not hypothetical: `tests/prefix_rotation.rs` stages a prefix immediately afterwards and had
    // it deleted between the directory's creation and its `CURRENT` flip.
    assert!(
        !orphan.exists(),
        "the startup sweep must have reclaimed the orphaned prefix before start_write_executor \
         returned"
    );

    assert!(
        root.join("v00000").exists(),
        "the live prefix is not an orphan and must survive"
    );
    assert!(
        stray.exists(),
        "a directory that is not a `v#####` prefix is not this sweep's business"
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(
        visible(&engine, &session) > 0,
        "and the node serves from the prefix it kept"
    );
}

/// **A generation holding a sidecar that a coalesce replaced still holds the prefix back.**
///
/// Reclamation waits on the readers, and the question it has to answer is *"can any live generation
/// still resolve a path under this prefix"* — a generation resolves external ids through its
/// sidecar, and the sidecar opens its runs **lazily**, so unlinking the tree under one turns its
/// next lookup into an IO error rather than an answer.
///
/// **The wait used to reach every generation but this one.** A flush publishes by *cloning* the
/// live sidecar `Arc`, so one strong count answers for every generation a flush produced. A
/// **coalesce** does not: it builds a new sidecar over an unchanged prefix, and from that moment a
/// generation still holding the old one is counted by neither the held generation's own reference
/// nor its sidecar's. `Executor::superseded_sidecars` closes it with a `Weak` per replaced sidecar —
/// which answers the question and, unlike holding them strongly, does not keep the mappings of every
/// sidecar the prefix ever had alive for its whole life.
///
/// **Mutations this kills:** dropping the weak list (the prefix is unlinked while this generation
/// holds it, and the lookup below fails); holding the list *strongly* (nothing is ever reclaimable,
/// because the executor itself is a holder — the assertion after the drop fails).
#[test]
fn a_generation_holding_a_coalesce_superseded_sidecar_holds_the_prefix_back() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(
        tmp.path(),
        &root,
        EngineConfig {
            // Two same-tier runs select a coalesce, so one extra flush reaches it.
            ..config_uncapped()
        },
    );
    ingest(&engine, b"coalesce-witness".to_vec(), "w0").expect("ingest is accepted");
    engine.request_flush();
    wait_for("the witness to be flushed", || {
        engine.write_executor_stats().flushes > 0
    });

    // **A generation captured before any coalesce**, held for the rest of the case. This is the
    // holder the two old counts could not see once a coalesce replaced what it points at: it is
    // neither the generation the fold supersedes (several publications newer) nor a holder of that
    // generation's sidecar (the coalesce built a new one).
    let held = engine.generation();

    // Drive flushes until a coalesce publishes a *new* sidecar over the same prefix.
    let mut round = 1;
    while engine.write_executor_stats().coalesces == 0 {
        assert!(round < 64, "no coalesce published in {round} rounds");
        ingest(
            &engine,
            format!("c{round}").into_bytes(),
            &format!("c{round}"),
        )
        .expect("ingest is accepted");
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_for("a flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
        round += 1;
    }
    // That a coalesce builds a *new* sidecar rather than cloning the live one is stated at its
    // publication site and is the whole reason this case differs from a flush's; the sidecar
    // pointer itself is crate-internal, so what is asserted here is that a coalesce happened at
    // all — without one, this test is about nothing.
    assert!(engine.write_executor_stats().coalesces > 0);

    fold(&engine);

    // **The prefix stands while the pre-coalesce generation is held.** Reclamation is retried at
    // every tick, so this is not a race that has not happened yet; it is a wait that is holding.
    for _ in 0..5 {
        tick(&engine);
    }
    assert!(
        root.join("v00000").exists(),
        "the superseded prefix must not be unlinked while a generation over it is still held"
    );
    // Released — and now it goes.
    drop(held);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while root.join("v00000").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the prefix was never reclaimed after its last reader released it"
        );
        tick(&engine);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **Obligation 15's converging half: a restart onto the folded prefix serves the same state.**
///
/// `CURRENT` is the commit point, so a process that has flipped it has already published whatever
/// a restart will find — the swap that follows is in-memory only. This is that restart: a fresh
/// engine over the same bundle root and the same WAL, asserted to open the folded prefix, to serve
/// the same items, and to hold the same overlay.
///
/// **The retirement is durable here because the rotation reached it.** The fold rotates the WAL
/// immediately after its swap and the buffer is empty, so the whole durable prefix — including the
/// `ChangeByEntity{Delete}` record — is reclaimed, and replay has nothing to resurrect. A restart
/// landing *inside* that window instead would resurrect the entry harmlessly and permanently
/// (compaction §5), which is the case this one is the other side of.
#[test]
fn a_restart_onto_the_folded_prefix_converges() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let deleted = entity_of_source(&root, "v00000", 4);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    fold(&engine);
    let live = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    assert_eq!(engine.overlay_depth(), 0);
    drop(engine);

    let restarted = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the folded prefix opens on its own, with every digest it names verifying");
    assert_eq!(restarted.generation().prefix, "v00001");
    assert_eq!(
        restarted.overlay_depth(),
        0,
        "the retirement is durable: the rotation reclaimed the delete record the replay would \
         otherwise resurrect from"
    );
    let session = restarted.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(visible(&restarted, &session), live);
}

/// **Obligation 15's other half: a restart inside the durability window resurrects the retired
/// entries — harmlessly, and permanently.**
///
/// Retirement's durable homes are the manifest seed and the WAL (write-path §4.5). The fold's new
/// manifest omits the executed entries, but the WAL still holds the original
/// `ChangeByEntity{Delete}` records until a rotation whose head snapshot postdates the fold has
/// reclaimed them — so a restart landing in that window replays them and puts the tombstones back.
/// The fold rotates immediately after its swap to make the window as short as it can be; this is
/// what happens when a restart lands inside it anyway, which is compaction §7's "crash between
/// `CURRENT` and the swap" reached without a crash: the on-disc state is exactly (new `CURRENT`,
/// pre-rotation WAL), and both halves of it are real files.
///
/// **Harmless** because a resurrected entry names an entity with no row and no postings, so
/// `verdict` denies something nothing can reach and no row-space mask changes — **which rests on
/// `permutation.bin` being sentinel-filled rather than zero-filled**: `denied_rows_of` skips an
/// entity only when `row_of` answers `None`, and a zero-filled slot answers row 0. That is what the
/// visible count below is checking, and it is why it is checked at all.
///
/// **Permanent** because `apply_snapshot` applies entries and never assigns: a later rotation's
/// head snapshot can add a resurrected delete back and can never remove one. So the entry survives
/// every later rotation and clears only at the **next fold**, which executes it again — cheaply,
/// since it now has no postings. The last two assertions are that claim, not a healing.
///
/// **Mutation this kills:** zero-filling `permutation.bin` instead of sentinel-filling it — the
/// resurrected tombstone then lands on row 0, and a surviving item disappears from every mask.
///
/// **What it does not kill, and where that lives instead:** publishing the folded manifest from the
/// *un-retired* overlay, so its `tombstones` still names the executed entries. This case cannot see
/// it — the WAL is resurrecting those entries anyway, so the seed carrying them too changes
/// nothing — and `a_restart_onto_the_folded_prefix_converges` is where it shows, because there the
/// rotation has reclaimed the records and the seed is the only thing left that could put them back.
/// Verified by running the mutation against both.
#[test]
fn a_restart_before_the_rotation_resurrects_the_retirement_harmlessly_and_permanently() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let wal = tmp.path().join("wal.log");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let deleted = entity_of_source(&root, "v00000", 4);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    // The window's contents: the delete is durable, and nothing has rotated it away yet. Taken
    // here because the fold appends nothing of its own — only its rotation touches the log.
    let pre_rotation = snapshot_wal(tmp.path());

    fold(&engine);
    assert_eq!(engine.overlay_depth(), 0, "the fold retired it");
    let folded_visible = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    drop(engine);

    // The restart: the folded bundle, and the WAL as it stood before the rotation.
    restore_wal(tmp.path(), &pre_rotation);
    let restarted = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &wal,
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the folded prefix opens");
    let mut restarted = restarted;
    restarted
        .start_write_executor(8)
        .expect("the executor starts");
    restarted.set_background_refresh_for_test(false);

    assert_eq!(restarted.generation().prefix, "v00001");
    assert_eq!(
        restarted.overlay_depth(),
        1,
        "the retired entry is back: replay runs over the manifest seed, and the seed's omission \
         cannot subtract a record the WAL still holds"
    );
    assert!(restarted.generation().overlay.is_deleted(deleted));
    let session = restarted.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&restarted, &session),
        folded_visible,
        "and it is harmless: the entity it names has no row and no postings, so it denies nothing \
         reachable and takes no surviving item's row with it"
    );

    // **Permanent.** Force a rotation by giving the log something to grow with: a snapshot written
    // now *applies* the resurrected entry rather than assigning over it, so it cannot remove one.
    ingest(&restarted, b"after-restart".to_vec(), "after").expect("ingest is accepted");
    restarted.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while restarted.write_executor_stats().flushes == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        restarted.overlay_depth(),
        1,
        "a rotation after the resurrection does not clear it — `apply_snapshot` applies entries \
         and never assigns"
    );

    // And it clears at the next fold, which executes it again over an entity that now has nothing
    // to remove.
    fold(&restarted);
    assert_eq!(
        restarted.overlay_depth(),
        0,
        "the next fold takes it: `executed` is the whole of `D₀` because nothing carries it forward"
    );
}

/// **A suppression survives a fold verbatim, and an unsuppress afterwards reveals its item**
/// (obligation 4). Rule S gives a suppression no retirement route at all, and the fold gives it
/// none either: its row and its postings are folded through untouched.
///
/// **Mutations this kills:** folding `deleted ∪ suppressed` rather than `deleted` (the suppressed
/// item loses its row, so the unsuppress reveals nothing and the count stays down); retiring the
/// suppression alongside the deletion (the item reappears at the fold rather than at the
/// unsuppress).
#[test]
fn a_suppression_survives_the_fold_and_an_unsuppress_afterwards_reveals_its_item() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let suppressed = entity_of_source(&root, "v00000", 8);
    let baseline = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("a suppression is accepted");

    fold(&engine);

    assert_eq!(
        engine.overlay_depth(),
        1,
        "Rule S: a suppression never retires"
    );
    let after = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(visible(&engine, &after), baseline - 1, "still hidden");

    engine
        .accept_change(suppressed, ChangeOp::Unsuppress)
        .expect("an unsuppress is accepted");
    let revealed = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &revealed),
        baseline,
        "and its row survived the fold, so there is something to reveal"
    );
}

/// **Every external id resolves both ways after the fold, and a folded-away entity's key is gone**
/// (obligation 7).
///
/// Pass 3's locator is sized to the *snapshot's* entity space and the new `MANIFEST.json` carries
/// that bound as `entity_id_high_water`, which is the only thing that reads it — the sidecar's
/// declared locator length. A live value there would make the base locator claim every
/// post-snapshot entity; a mismatched one fails the sidecar's own length check at first touch.
///
/// **Mutations this kills:** writing the live `entity_id_high_water` into `MANIFEST.json` (the
/// locator's declared length no longer matches its bytes and every reverse resolution errors);
/// carrying the pre-fold sidecar onto the new generation (it resolves through the reclaimed prefix,
/// so both directions fail once `v00000` is gone).
#[test]
fn external_ids_resolve_both_ways_after_a_fold_and_a_folded_entitys_key_is_gone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let deleted = entity_of_source(&root, "v00000", 4);
    let survivor = entity_of_source(&root, "v00000", 5);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");

    fold(&engine);

    assert_eq!(
        engine.external_id_of(survivor).unwrap(),
        Some(source_id_key(5)),
        "a survivor still names its external id, through a sidecar opened on the new prefix"
    );
    assert_eq!(
        engine.resolve_external_id(&source_id_key(5)).unwrap(),
        Some(survivor),
        "and the reverse direction agrees"
    );
    assert_eq!(
        engine.resolve_external_id(&source_id_key(4)).unwrap(),
        None,
        "the folded entity's key is gone from the run: leaving it standing 409s a lawful \
         re-ingest of that external id (decision 0047)"
    );
}

/// **A fold that would leave a deployment the next startup refuses is discarded, loudly.**
///
/// Write-path §7's relation — `merge.max_merged_segment_bytes` strictly below the base segment's
/// bytes — is checked at startup by `tessera-server`'s loader, and a fold is the one operation that
/// can move the figure it is checked against. The case to catch is the small corpus where the
/// folded base is *smaller* than a cap an operator set against a larger one.
///
/// **Mutations this kills:** dropping the check (the fold publishes and the next `prepare` refuses
/// the bundle); checking the resolved policy value rather than the configured one (the built-in
/// 256 MiB default would then discard every fold over a fixture-sized corpus).
#[test]
fn a_fold_that_would_break_the_merge_size_relation_is_discarded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let config = EngineConfig {
        // Far above anything a 10,000-item fixture's base segment can reach.
        max_merged_segment_bytes: Some(1 << 40),
        ..config_uncapped()
    };
    let engine = engine_over_fixture(tmp.path(), &root, config);

    fold_discarded(&engine);

    assert_eq!(
        engine.generation().prefix,
        "v00000",
        "nothing was published and the live prefix is untouched"
    );
    assert!(
        root.join("v00000").exists(),
        "and nothing was reclaimed either"
    );
}

/// **An unset `max_merged_segment_bytes` does not discard a fold**, which is the other half of the
/// relation's rule: an unset value is derived from the base segment when merge selection lands, so
/// it cannot violate the relation and must not be checked as though it could.
///
/// Covered by every other case here — all of them run with the key unset and all of them publish —
/// and stated separately because the mutation it kills (checking the resolved policy value instead
/// of the configured one) makes *every* fold over a small corpus fail, which reads like a fixture
/// problem rather than like a rule.
#[test]
fn an_unset_merge_cap_never_discards_a_fold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    assert!(config_uncapped().max_merged_segment_bytes.is_none());
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());
    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");
}

/// Run a fold with a flush landing **inside its flight**: the interleaving compaction §2's
/// carry-forward table exists for, and the only route by which a fold's publication ever sees a
/// carried-forward artefact.
///
/// Returns `(the entity the mid-flight flush published, the live watermark and entity_id_high_water
/// right after that flush publishes)` — the two fields `write.rs`'s `publish_fold` carries through
/// from `live_manifest` untouched, captured at the one moment they can be told apart from the
/// fold's own pre-flight snapshot (`the_watermark_and_high_water_published_are_the_live_ones_not_
/// the_snapshot` and `the_folded_manifests_high_water_is_the_snapshots_entity_space` are why that
/// moment matters).
///
/// The hold is a test hook and models duration, not behaviour — see
/// `Engine::set_fold_paused_for_test`. Everything the flush does here it does exactly as it would
/// in production: it plans against the live generation, writes into the **old** prefix, and
/// publishes there, because the fold has not flipped `CURRENT` yet.
fn fold_with_a_flush_in_flight(engine: &Engine, key: Vec<u8>) -> (EntityId, u64, u64) {
    engine.set_fold_paused_for_test(true);
    let before = engine.write_executor_stats();
    engine.request_fold();

    // Wait until the fold's passes are done and it is holding: from here everything published is
    // post-snapshot and must be carried forward rather than folded.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !engine.fold_is_holding_for_test() {
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never reached its hold"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let entity = ingest(engine, key, "mid-flight").expect("ingest is accepted during a fold");
    engine.request_flush();
    while engine.write_executor_stats().flushes == before.flushes {
        assert!(
            std::time::Instant::now() < deadline,
            "the mid-flight flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // The flush has published into the still-live (pre-flip) generation: this is "live", as
    // `publish_fold` will read it moments later, and it is the last point at which reading it is
    // this straightforward — after the flip it is what the new generation carries, which is
    // exactly the claim under test.
    let mid_flight = engine.generation();
    let mid_flight_watermark = mid_flight.watermark;
    let mid_flight_high_water = mid_flight.bundle.partitions["default"]
        .manifest
        .entity_id_high_water;

    engine.set_fold_paused_for_test(false);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded"
        );
        if now.folds > before.folds {
            return (entity, mid_flight_watermark, mid_flight_high_water);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **Obligation 8: a flush published during the fold's flight is carried forward**, and its items
/// are visible after the flip at their re-based row ids.
///
/// This is the only case here in which the publication has anything to carry — every other fold
/// consumes the whole bundle — so it is also the only one that exercises the rebase check, the
/// hard-linking of a carried segment, the run and locator lists' recency order, and the
/// carry-forward set that retirement subtracts.
///
/// **Mutations this kill:** dropping carried segments from the assembled manifest (the mid-flight
/// item is invisible and its external id resolves to nothing); listing the carried segment *before*
/// the fold's own base (the reader takes the first segment of a view as the base
/// `permutation.bin` addresses, so the bundle either refuses to open or serves the wrong row
/// space); listing the carried run before the folded run 0 (the sidecar derives the base locator's
/// path from `external_id_runs[0]`, so it takes a flush's entity-range extent for the full-length
/// base locator and every reverse resolution errors).
#[test]
fn a_flush_inside_the_folds_flight_is_carried_forward() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let baseline = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    let key = b"landed-mid-fold".to_vec();
    let (entity, _, _) = fold_with_a_flush_in_flight(&engine, key.clone());

    assert_eq!(engine.generation().prefix, "v00001");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &session),
        baseline + 1,
        "the mid-flight item is visible after the flip, at its re-based row id"
    );
    assert_eq!(
        engine.resolve_external_id(&key).unwrap(),
        Some(entity),
        "and its external-id binding was carried forward with it"
    );
    assert_eq!(
        engine.external_id_of(entity).unwrap(),
        Some(key),
        "including the reverse direction, which resolves past the fold's own base locator into \
         the carried-forward extent"
    );

    // And the bundle a restart opens says the same thing.
    let bundle = open_bundle(&root).expect("the folded bundle opens");
    let partition = &bundle.partitions["default"];
    assert_eq!(
        partition.views["s0"].row_space.extent_count(),
        1,
        "one carried-forward extent above the folded base"
    );
    assert!(
        partition.views["s0"].row_space.row_of(entity).is_some(),
        "whose rows the base permutation does not claim"
    );
}

/// **Obligation 2: a delete accepted *after* the snapshot survives the fold** — its entity keeps
/// its row in the folded base, its id is in the new manifest's `tombstones`, its overlay entry does
/// not retire, and it is invisible throughout.
///
/// This is the fail-open compaction §2 calls the reason three of its four carried categories exist:
/// the `tombstones` line is the one arithmetic that must be a **set difference against live state**.
/// A delete accepted during the fold's flight names an entity whose row the fold did *not* drop —
/// it was not in `D₀` — so publishing the executed set, or copying the plan's, would retire that
/// deletion while its row survives in the rebuilt base, and the item would be drawn, counted and
/// served to every authorised principal.
///
/// **Mutations this kills:** publishing `tombstones` from the plan's `D₀` rather than from the live
/// overlay minus `executed` (the id is absent from the manifest, so a restart re-exposes the item);
/// retiring `D₀ ∪ (live deleted)` (depth drops to zero and the item comes back immediately).
#[test]
fn a_delete_accepted_after_the_snapshot_survives_the_fold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let baseline = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    // Deleted before the fold, so it *is* in `D₀`: the contrast that stops this case passing
    // because nothing retired at all.
    let folded = entity_of_source(&root, "v00000", 3);
    engine
        .accept_change(folded, ChangeOp::Delete)
        .expect("a delete is accepted");

    // Now hold the fold and accept a second delete *inside its flight* — after the plan cloned
    // `D₀`, so this one is not in it and the fold's passes never saw it.
    engine.set_fold_paused_for_test(true);
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !engine.fold_is_holding_for_test() {
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never reached its hold"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let mid_flight = entity_of_source(&root, "v00000", 9);
    engine
        .accept_change(mid_flight, ChangeOp::Delete)
        .expect("a delete is accepted during the fold's flight");
    {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        assert_eq!(
            visible(&engine, &session),
            baseline - 2,
            "invisible from the moment it is acked, which is before the fold publishes"
        );
    }

    engine.set_fold_paused_for_test(false);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let bundle = open_bundle(&root).expect("the folded bundle opens");
    let partition = &bundle.partitions["default"];
    assert!(
        partition.views["s0"]
            .row_space
            .row_of(mid_flight)
            .is_some(),
        "the post-snapshot deletion keeps its row: the fold's passes ran over `D₀`, which did not \
         name it"
    );
    assert!(
        partition.views["s0"].row_space.row_of(folded).is_none(),
        "and the pre-snapshot one lost its row, so the fold did fold something"
    );
    assert!(
        partition.manifest.tombstones.contains(&mid_flight.raw()),
        "its id is in the new manifest's tombstones — the seed a restart reads, and the only thing \
         still hiding it: tombstones is `live deleted − executed`, never the plan's set"
    );
    assert!(
        !partition.manifest.tombstones.contains(&folded.raw()),
        "while the executed one is gone from the seed"
    );
    assert_eq!(
        engine.overlay_depth(),
        1,
        "its overlay entry did not retire; the executed one's did"
    );
    let after = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &after),
        baseline - 2,
        "and it is still invisible after the flip — throughout, with no window in which the fold's \
         own publication re-exposed it"
    );
}

/// **Obligation 2b, end to end: a delete in `D₀` whose entity a carried-forward artefact still
/// names does not retire** — the fail-open compaction §5's rule replaced, in the interleaving that
/// actually produces it.
///
/// The r3 finding's shape is *a flush whose plan predates the fold's and whose publication
/// postdates it*, and the tick produces exactly that on its own: it plans and dispatches a flush,
/// and then dispatches the fold against the generation that flush has not published into yet. So
/// the fold's snapshot names none of the flush's rows, the flush publishes them into the old
/// prefix, and the publication carries that segment forward.
///
/// **The entity it protects has no row in that segment at all**, which is the point. `doomed` is
/// deleted before the fold plans — so it is in `D₀`, and `drop_deleted` has already taken it out of
/// the buffer, so the flush never writes a row for it. What names it is the segment's **declared
/// range**, `[a, c]`, which spans it because `a` and `c` were still buffered on either side. That
/// is `CarriedForward::add_segment`'s documented over-approximation doing the work it exists for:
/// naming more is fail-closed, naming fewer is the fail-open.
///
/// **A zero-term item runs beside it**, deleted the same way. It has no postings, so no tier could
/// name it; what the carry-forward set covers is its *entity*, through the run and locator extent
/// the same flush publishes. That is the shape 2b was added for.
///
/// **And the re-ingest the obligation's own text requires**, which this case omitted until it was
/// recounted. A protected entity keeps its tombstone, and a tombstone must not become a
/// reservation on the external id: decision 0047 makes an edit a delete plus a re-ingest, so a
/// deleted holder that blocked one would fail every edit of a mid-fold deletion until some later
/// fold happened to run. This is a **different rule** from the retired case that
/// `a_retired_entitys_external_id_is_re_ingestible` covers — that one passes unmoved when
/// `established_collisions` is made to collide on a still-deleted holder, and this one does not.
///
/// **Mutations this kills:** retiring `D₀` wholesale (all three retire, which is the r3
/// fail-open); leaving the carry-forward set empty; building it from the plan rather than from the
/// live manifest at publication (the flush's four artefacts are not in the plan, so nothing is
/// carried and all retire); and colliding a re-ingest against a deleted holder.
///
/// **What it does *not* kill, verified rather than assumed: dropping either single artefact kind.**
/// A flush publishes a segment, a tier, a run and a locator extent *together, over one entity
/// range*, so end to end the segment adder and the locator adder each cover both protected
/// entities on their own and removing either leaves this case green — checked by running both
/// mutations. That is not a hole in the rule, it is the reason the rule is stated over the whole
/// carry-forward set; the per-artefact independence is where a fixture can actually separate them,
/// in `compact.rs`'s `none_of_obligation_2bs_three_shapes_retires`, which gives one flush only a
/// segment and another only a locator extent. Read the two together.
#[test]
fn a_deletion_a_carried_forward_segment_still_names_does_not_retire() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // Three ingests in three windows, so the ids are consecutive and ascending, and the middle one
    // is deleted before anything flushes. `a` and `c` stay buffered; `doomed` is dropped from the
    // buffer at its own deny apply, so no flush will ever write a row for it.
    let a = ingest(&engine, b"a".to_vec(), "a").expect("ingest is accepted");
    let doomed = ingest(&engine, b"doomed".to_vec(), "doomed").expect("ingest is accepted");
    // **A zero-term item, deleted the same way.** No tier names it — it has no postings to hold —
    // so what stands between it and a fail-open retirement is the carry-forward set's coverage of
    // its *entity*, through the run and locator extent the same flush publishes. This is the shape
    // obligation 2b was added for, and it never ran end to end before.
    let zero_term = ingest_with_descriptors(&engine, b"zero-term".to_vec(), "zero-term", &[])
        .expect("accepted");
    let c = ingest(&engine, b"c".to_vec(), "c").expect("ingest is accepted");
    assert_eq!(doomed.raw(), a.raw() + 1);
    assert_eq!(zero_term.raw(), doomed.raw() + 1);
    assert_eq!(c.raw(), zero_term.raw() + 1);
    engine
        .accept_change(doomed, ChangeOp::Delete)
        .expect("a delete is accepted");
    engine
        .accept_change(zero_term, ChangeOp::Delete)
        .expect("a delete is accepted");

    // A second deletion, of an entity the base holds and nothing carries forward: the fold removes
    // its row and its postings, so it retires. Without it this case could not tell "the rule
    // protected the right entity" from "the rule retired nothing".
    let folded = entity_of_source(&root, "v00000", 4);
    engine
        .accept_change(folded, ChangeOp::Delete)
        .expect("a delete is accepted");
    assert_eq!(engine.overlay_depth(), 3);

    // One tick dispatches the flush of `[a, c]` and then the fold, in that order — so the fold's
    // snapshot predates the flush's publication and the two overlap for real. The hold is only
    // there to make the overlap wide enough to observe deterministically.
    engine.set_fold_paused_for_test(true);
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !engine.fold_is_holding_for_test()
        || engine.write_executor_stats().flushes == before.flushes
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the fold and the flush never overlapped"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    engine.set_fold_paused_for_test(false);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        engine.overlay_depth(),
        2,
        "two deletions were carried forward and one was not: `executed = {{ e ∈ D₀ : no \
         carried-forward artefact names e }}`"
    );
    assert!(
        engine.generation().overlay.is_deleted(doomed),
        "the entity a carried-forward segment's declared range names keeps its tombstone for \
         another round — fail-closed, and the next fold takes it"
    );
    assert!(
        engine.generation().overlay.is_deleted(zero_term),
        "and so does the zero-term one, which no tier could have named: the rule is stated over \
         the carry-forward set, not over the postings"
    );
    assert!(
        !engine.generation().overlay.is_deleted(folded),
        "and the one the fold demonstrably removed does not"
    );

    // The mid-flight flush's own items are unharmed by any of it.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(visible(&engine, &session), N_ITEMS + 1);
    assert_eq!(engine.resolve_external_id(b"a").unwrap(), Some(a));
    assert_eq!(engine.resolve_external_id(b"c").unwrap(), Some(c));

    // **And the re-ingest 2b's own text requires, which this case never attempted.** A protected
    // entity keeps its tombstone, and a tombstone must not become a reservation on the external
    // id: decision 0047 makes an edit a delete followed by a re-ingest, so a deleted holder
    // blocking the re-ingest would make every edit of a mid-fold deletion fail with a 409 until
    // some later fold happened to run. The new item is a *different* entity — the id is not
    // recycled (I9) — and the old one stays deleted.
    for (external_id, deleted) in [
        (b"doomed".to_vec(), doomed),
        (b"zero-term".to_vec(), zero_term),
    ] {
        // A distinct batch label per re-ingest: the two calls share an idempotency digest, so one
        // label would make the second a *replay* of the first and return its ids unchanged.
        let batch = format!("re-ingest-{}", String::from_utf8_lossy(&external_id));
        let reborn = ingest(&engine, external_id.clone(), &batch)
            .expect("a re-ingest of a protected entity's external id succeeds, never 409s");
        assert_ne!(
            reborn, deleted,
            "the re-ingest takes a fresh entity id; I9 never reissues the deleted one"
        );
        assert!(
            engine.generation().overlay.is_deleted(deleted),
            "and the re-ingest does not resurrect the entity that was deleted"
        );
        assert_eq!(
            engine.resolve_external_id(&external_id).unwrap(),
            Some(reborn),
            "the external id now resolves to the new entity"
        );
    }
}

/// **The new `MANIFEST.json`'s `entity_id_high_water` is the *snapshot's* entity space, not the
/// live one** — compaction §3 pass 3's fidelity F1, which was fatal as first written.
///
/// That field is read by exactly one thing: the base locator's declared length. The base locator
/// has absolute priority for every entity below it and the carried-forward extents are consulted
/// only *past* it, so a live value makes the fold's own locator claim every post-snapshot entity
/// and answer "this item has no external id" for items that have one — contracts §2.4's
/// wrong-answer-wearing-a-legitimate-state's-clothes.
///
/// **This is the only case in which the two values differ**, which is why it needs a mid-flight
/// flush: without one the snapshot's entity space *is* the live one, and the mutation survives.
///
/// **Mutation this kills:** `bundle_manifest.entity_id_high_water = live_manifest
/// .entity_id_high_water` — the locator's declared length then exceeds its bytes and the sidecar's
/// own length check fails every reverse resolution, and if it did not, the carried-forward extent
/// would be unreachable.
#[test]
fn the_folded_manifests_high_water_is_the_snapshots_entity_space() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let (entity, _, _) = fold_with_a_flush_in_flight(&engine, b"landed-mid-fold".to_vec());

    let bundle = open_bundle(&root).expect("the folded bundle opens");
    let snapshot_bound = bundle.manifest.entity_id_high_water;
    let live_bound = bundle.partitions["default"].manifest.entity_id_high_water;
    assert!(
        snapshot_bound < live_bound,
        "the mid-flight flush moved the side-manifest's high-water past the fold's locator bound \
         ({snapshot_bound} vs {live_bound}); without that this case proves nothing"
    );
    assert_eq!(
        snapshot_bound,
        entity.raw(),
        "the locator covers exactly the entities that had a row at the snapshot"
    );
    assert_eq!(
        std::fs::metadata(root.join("v00001/partitions/default/entities/ext-locator.u32"))
            .unwrap()
            .len(),
        snapshot_bound * 4,
        "and the file's bytes are what the manifest declares — the sidecar checks this at open, \
         so a live bound here is a hard failure at the first reverse resolution"
    );
}

/// **Obligation 10: the watermark and `entity_id_high_water` published in `SEGMENTS-<n>.json` are
/// the *live* values at the flip, not anything derived from the fold's own snapshot.**
///
/// `write.rs`'s `publish_fold` states the claim directly, on `SegmentsManifest`'s two fields:
/// "Live, and untouched. Deriving either from the fold's inputs moves the watermark backwards past
/// every post-snapshot entity, and composition treats an entity at or above it as buffered rather
/// than rowed — so the gap goes invisible to every principal with no error." That sentence is
/// about the `SEGMENTS-<n>.json` half of each field — the counterpart
/// `the_folded_manifests_high_water_is_the_snapshots_entity_space` pins is `MANIFEST.json`'s, which
/// is deliberately the *other* value.
///
/// A mid-flight flush is required to tell "live" from "snapshot" apart at all: without one the two
/// coincide and a fold that mistakenly published its own pre-flight bound would pass by accident,
/// same as the high-water case above.
///
/// **Mutations this kills:** `watermark: live_manifest.watermark` replaced by any value fixed at
/// the fold's own snapshot, `0` included; likewise for `entity_id_high_water` in
/// `SegmentsManifest`. Verified against `watermark: 0` — see this test's module-level report.
#[test]
fn the_watermark_and_high_water_published_are_the_live_ones_not_the_snapshot() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    let baseline_watermark = engine.generation().watermark;
    let baseline_high_water = engine.generation().bundle.partitions["default"]
        .manifest
        .entity_id_high_water;

    let (_, mid_flight_watermark, mid_flight_high_water) =
        fold_with_a_flush_in_flight(&engine, b"landed-mid-fold-watermark".to_vec());

    assert!(
        mid_flight_watermark > baseline_watermark,
        "the mid-flight flush must move the live watermark past the fold's own snapshot \
         ({baseline_watermark} vs {mid_flight_watermark}), or this case cannot tell 'live' from \
         'snapshot' apart"
    );
    assert!(
        mid_flight_high_water > baseline_high_water,
        "and likewise for entity_id_high_water ({baseline_high_water} vs \
         {mid_flight_high_water})"
    );

    assert_eq!(engine.generation().prefix, "v00001");
    let bundle = open_bundle(&root).expect("the folded bundle opens");
    let published = &bundle.partitions["default"].manifest;
    assert_eq!(
        published.watermark, mid_flight_watermark,
        "the published SEGMENTS-<n>.json watermark is the live one at the flip, not a value \
         frozen at the fold's own snapshot"
    );
    assert_eq!(
        published.entity_id_high_water, mid_flight_high_water,
        "and likewise for entity_id_high_water — the SEGMENTS-<n>.json half is the live \
         allocator floor, where MANIFEST.json's is deliberately the snapshot bound"
    );
}

/// **The flip does not refuse anything: a fold's publication does not arm the refresh shed**
/// (decision 0053).
///
/// A merge arms it, and a racer inside a merge's refresh window is shed with a 429 —
/// `merge.rs`'s `a_racer_inside_a_merges_refresh_window_is_shed_rather_than_rebuilding` is that
/// case, and this is deliberately its mirror. The rule that separates them: *shed only while the
/// refresh pass is shorter than the rebuild it would save.* A merge satisfies it (a ~0.7 s pass
/// against a measured 1 277 ms rebuild); a fold inverts it by two orders (~180 s against 1.3 s),
/// so arming would refuse every session for minutes to avoid a burst that clears in seconds.
///
/// The refresh is **held** here rather than switched off, because those are different states: a
/// pass that finishes without producing anything clears the flag, and then a request builds for
/// the ordinary reason rather than because the fold declined to arm it. Held, the flag would stay
/// set for the whole window if anything had set it — so a request served here is a request the
/// shed never saw.
///
/// **Mutation this kills:** arming `refresh.in_flight` in the rotation arm of `publish_geometry`
/// (the request below is refused `ProjectionBuilding` instead of served).
#[test]
fn the_flip_does_not_arm_the_refresh_shed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        tmp.path().join("bundle").as_path(),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("engine should open");
    engine.start_write_executor(8).expect("the executor starts");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let baseline = visible(&engine, &session);

    // Hold the refresh, so a flag armed at the swap would stay armed for the whole window.
    engine.set_refresh_paused_for_test(true);
    fold(&engine);

    let served = engine
        .viewport(
            &session,
            ViewportRequest::new(
                "s0",
                0,
                [0.0, 0.0, 1000.0, 1000.0],
                (N_ITEMS + 100) as usize,
            ),
        )
        .expect("a request after a fold takes an ordinary cache miss, never a refusal");
    assert_eq!(
        served.tiles[0].visible, baseline,
        "and it is served the whole map, rebuilt inline against the new row space"
    );
    engine.set_refresh_paused_for_test(false);
}

/// **Obligation 5: masked counts are identical across the flip for every principal, up to exactly
/// the folded deletions — over several principals including a genuinely sparse one.**
///
/// Four principals, established *before* the flip and asked again after it: full coverage, a
/// subset (a third of the fixture, via `SUBSET_TERM`), a sparse grant (`SPARSE_TERM`, about a
/// hundredth — see [`write_pairs_with_sparse_term`], since `common`'s fixed two-term fixture
/// cannot express one), and zero. The fold reads every column unmasked, which is sanctioned only
/// because its outputs are bundle artefacts (I2, spec §11) — so the thing to prove is that having
/// read them changes no answer: each principal still sees exactly `M_auth`, and the deletion each
/// of them could see is the only difference.
///
/// **The zero-grant principal is the one that would catch a leak.** It is authorised for no term,
/// so its `M_auth` is empty and *any* item reaching it is a disclosure — which is what a fold that
/// published its unmasked read into anything a request composes against would produce.
///
/// **Mutations this kills:** publishing the fold's own row set as a mask rather than rebuilding
/// each session's (the zero-grant count stops being zero); folding the subset term's postings away
/// (the subset principal's count collapses); folding the sparse term's postings away (only the
/// sparse principal's count collapses — the subset assertion alone would not catch this one,
/// which is why a hundredth is not the same case as a third).
#[test]
fn masked_counts_are_identical_across_the_flip_for_every_principal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture_with_sparse_term(tmp.path(), &root, config_uncapped());

    // Source 3 carries `ALL_TERM`, `SUBSET_TERM` (3 % 3 == 0) and `SPARSE_TERM` (3 % 97 == 3), so
    // it is visible to every non-zero grant here — which is what makes "up to exactly the folded
    // deletions" a claim about all three counts at once.
    let deleted = entity_of_source(&root, "v00000", 3);

    let principals = [
        ("full", full_coverage_credential()),
        ("subset", subset_credential()),
        ("sparse", sparse_credential()),
        ("zero", zero_credential()),
    ];
    let sessions: Vec<_> = principals
        .iter()
        .map(|(name, credential)| {
            let session = engine.authorise(credential).unwrap();
            (*name, visible(&engine, &session), session)
        })
        .collect();
    let by_name = |name: &str| sessions.iter().find(|(n, _, _)| *n == name).unwrap();
    let summary = || {
        sessions
            .iter()
            .map(|(n, c, _)| (*n, *c))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        by_name("zero").1,
        0,
        "a zero-grant principal sees nothing to begin with"
    );
    assert!(
        by_name("subset").1 > 0 && by_name("subset").1 < by_name("full").1,
        "the subset principal sees some but not all of it: {:?}",
        summary()
    );
    assert!(
        by_name("sparse").1 > 0 && by_name("sparse").1 < by_name("subset").1,
        "and the sparse principal sees fewer still — genuinely sparse, not merely partial \
         coverage: {:?}",
        summary()
    );

    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    fold(&engine);
    assert_eq!(engine.overlay_depth(), 0);

    for (name, before, session) in &sessions {
        // The session was established before the flip and is asked again after it: its fragment is
        // rebuilt under the new identity, over the new term index, projected through the new row
        // space.
        assert_eq!(
            visible(&engine, session),
            before.saturating_sub(1),
            "{name}: the count moved by exactly the folded deletion and by nothing else"
        );
    }
    assert_eq!(
        visible(&engine, &by_name("zero").2),
        0,
        "and the zero-grant principal still sees nothing — the fold read every column unmasked, \
         and none of that reached a response"
    );
}

/// **Obligation 14, the detail-lookup half: `Engine::item` answers exactly `M_auth` across a
/// fold, the same as the viewport does.**
///
/// The sibling above proves this only for `viewport` (hence its name staying "masked_counts", not
/// "every_verb") — there is no `.item(` call anywhere else in this file, and `/v1/items/{id}` is a
/// mounted verb the fold's unmasked read could just as easily leak through. Three cases in one
/// fold: an item a restricted principal cannot see must answer nothing for it, both before and
/// after; an item the full-coverage principal can see must keep answering after the fold, at its
/// unchanged `tessera_id`; and an entity deleted before the fold — folded away, so both rowless and
/// postingless afterwards — must answer nothing even to the full-coverage principal, or Rule F's
/// retirement has re-exposed it through the one verb the counts test does not exercise.
///
/// **Mutations this kill:** disabling `plan.tombstones` in both the row-space pass and the
/// postings sweep (`compact.rs`'s pass 1 and pass 2) — the deleted entity keeps both its row and
/// its postings, the overlay still retires its tombstone because retirement does not depend on
/// what the passes actually dropped, and the last assertion below is the one that catches it: the
/// item resurrects for the full-coverage principal despite the fold having "completed".
#[test]
fn item_lookup_answers_exactly_m_auth_across_a_fold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // Source 5: not a multiple of 3, so it carries only `ALL_TERM` — visible to `full`, invisible
    // to `subset`.
    let restricted = entity_of_source(&root, "v00000", 5);
    // Source 6: a multiple of 3, so it carries both terms — visible to both, and the one this test
    // deletes and folds away.
    let deleted = entity_of_source(&root, "v00000", 6);

    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();

    let restricted_id: TesseraId = engine.tessera_id_of(restricted).unwrap();
    let deleted_id: TesseraId = engine.tessera_id_of(deleted).unwrap();

    // Before the fold: the ordinary masking rule, and confirmation the fixture set the case up as
    // intended (a delete on an item nobody could resurrect from proves nothing).
    assert!(engine.item(&full, restricted_id, None).unwrap().is_some());
    assert!(engine.item(&subset, restricted_id, None).unwrap().is_none());
    assert!(engine.item(&full, deleted_id, None).unwrap().is_some());

    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");

    // Both sessions were established before the flip: `Engine::item` takes the freshest resident
    // fragment or rebuilds against the live generation, in both cases scoped to `v00001`
    // (`RowProjectionCache::freshest_fragment`'s prefix filter) — so this is a real post-fold
    // answer, not a stale one.
    assert!(
        engine.item(&full, restricted_id, None).unwrap().is_some(),
        "still visible to the principal authorised for it — the fold's unmasked read did not \
         change the answer"
    );
    assert!(
        engine.item(&subset, restricted_id, None).unwrap().is_none(),
        "still invisible to the principal that never was"
    );
    assert!(
        engine.item(&full, deleted_id, None).unwrap().is_none(),
        "the folded-away entity answers nothing even to the principal that could see \
         everything else — its row and postings are both gone, so the identifier names \
         nothing rather than resurrecting it"
    );
}

/// **Obligation 11: an individual term ordinal is unchanged across a fold, not merely the total.**
///
/// `prefix_rotation.rs`'s `dict.len()` equality is a headcount: a rotation that renumbered every
/// ordinal while holding the total steady would pass it silently. This resolves two descriptors'
/// ordinals before a fold — one from the fixture's original dictionary and one promoted by a flush
/// just beforehand, so the dictionary carries more than one `dict_extents` entry across the fold —
/// and asserts the identical ordinals answer after it. A single-extent dictionary cannot tell
/// "preserved" from "coincidentally the same"; two can, because reordering the extents changes
/// every ordinal after the first one.
///
/// **Checked against a freshly reopened engine, not the live one.** `write.rs`'s `publish_fold`
/// carries the *live process's* dictionary forward as `Arc::clone(&live.dict)` — never reloaded
/// from the new manifest's `dict_extents` at all — so the live generation's ordinals are stable by
/// construction and asking it again would prove nothing about whether the fold wrote a correct
/// `dict_extents` list. What has to be checked is what a fresh open reconstructs from disc
/// (`session.rs`'s `Dict::load(&dict_paths)`, taken from the *new* prefix's manifest), which is
/// what a restart, and every future fold's own `open_rotation`, both depend on.
///
/// **Mutation this kills:** reversing `dict_extents`' listed order before it is carried into the
/// new `SEGMENTS-<n>.json` (`write.rs`'s `publish_fold` currently copies `live_manifest
/// .dict_extents` verbatim) — `Dict::load` assigns ordinals by position in the listed
/// concatenation (`tessera-authz`'s `dict.rs`), so a reordered list answers a different ordinal
/// for the descriptor promoted after the base extent, on the next open.
#[test]
fn term_ordinals_are_stable_across_a_fold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // Promote a novel descriptor before the fold, so the dictionary carries a second
    // `dict_extents` entry across it — the shape a single-extent dictionary cannot distinguish
    // from "coincidentally unchanged".
    let novel_row = UnallocatedRow {
        external_id: Some(b"novel-holder".to_vec()),
        view: "s0".to_string(),
        descriptors: vec![b"novel".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"novel".to_vec()]),
    };
    engine
        .accept_ingest(vec![novel_row], "promote-novel".to_string(), [0u8; 32])
        .expect("the novel descriptor is accepted");
    let flushes_before = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_for("the promoting flush to publish", || {
        engine.write_executor_stats().flushes > flushes_before
    });

    let before = engine.generation();
    let all_term_id = before
        .dict
        .lookup(b"0")
        .expect("ALL_TERM is in the fixture's base dictionary");
    let subset_term_id = before
        .dict
        .lookup(b"1")
        .expect("SUBSET_TERM is in the fixture's base dictionary");
    let novel_term_id = before
        .dict
        .lookup(b"novel")
        .expect("the promoted descriptor is now an ordinary ordinal, in a second extent");
    let len_before = before.dict.len();
    assert_eq!(
        before.bundle.partitions["default"]
            .manifest
            .dict_extents
            .len(),
        2,
        "the promotion above must add a second dict_extents entry, or reordering them proves \
         nothing"
    );

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");
    drop(engine);

    // A fresh open, over the folded bundle on disc — not the live process's carried-forward
    // `Arc<Dict>`, which cannot observe a `dict_extents` bug at all (see this test's doc).
    let restarted = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the folded prefix opens on its own");
    assert_eq!(restarted.generation().prefix, "v00001");

    let after = restarted.generation();
    assert_eq!(
        after.dict.lookup(b"0"),
        Some(all_term_id),
        "a descriptor's ordinal must not move across a fold"
    );
    assert_eq!(after.dict.lookup(b"1"), Some(subset_term_id));
    assert_eq!(
        after.dict.lookup(b"novel"),
        Some(novel_term_id),
        "including a descriptor promoted from a second dict_extents entry, not just the base one"
    );
    assert!(
        after.dict.len() >= len_before,
        "dict.len() must never decrease across a fold"
    );
}

/// Seconds past UTC midnight, now.
fn utc_time_of_day() -> u32 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        % 86_400) as u32
}

/// A config whose fold schedule is `schedule` and whose tick is otherwise the fixture's.
fn config_scheduling(schedule: tessera_engine::CompactionSchedule) -> EngineConfig {
    EngineConfig {
        compaction: schedule,
        ..config_uncapped()
    }
}

/// Drive ticks for `secs` and assert the schedule dispatched nothing in that time.
///
/// **A negative assertion here has to wait, and `folds == 0` straight after a tick does not.**
/// `tick` returns when the tick *counter* moves, which happens before `dispatch_fold` runs — and
/// even once it has run, a dispatched fold is on its own thread and increments nothing until it
/// publishes. So the naive check passes while a fold is in flight, which is exactly how the first
/// version of `the_windowed_route_…` came to survive the mutation its doc claimed to kill. The
/// fixture folds in well under a second; this waits several times that.
fn assert_no_fold_within(engine: &Engine, secs: u64) {
    let before = engine.write_executor_stats();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < until {
        tick(engine);
        let now = engine.write_executor_stats();
        assert_eq!(
            (now.folds, now.fold_failures),
            (before.folds, before.fold_failures),
            "the schedule dispatched a fold it had no route to dispatch"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Pull the tick forward and wait for the schedule to be evaluated on it.
///
/// `request_flush` is the deterministic way to make a tick happen now — it is the one operator
/// trigger that pulls the deadline — and the fold schedule is read on that same tick, beside the
/// flush's own plan. So this is a tick, not a sleep, and `ticks` is what says one happened.
fn tick(engine: &Engine) {
    let before = engine.write_executor_stats().ticks;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().ticks == before {
        assert!(std::time::Instant::now() < deadline, "the tick never fired");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// **The retirable-depth route dispatches a fold with nobody asking for one, at any hour**
/// (compaction §9, decision 0056).
///
/// The urgent route: retirable depth is a write cost that grows without bound — the overlay grows
/// monotonically under deletion churn and every deny acceptance clones it — so it is not windowed.
///
/// **The threshold is three, and the fixture is one suppression plus deletions**, which is what
/// makes this case able to tell the two gauges apart. At one suppression and two deletions the
/// overlay's *depth* is already 3 and its *retirable* part is 2 — so a trigger keyed on
/// `Overlay::len()` fires here and the correct one does not. That is r3's memory F5 in its exact
/// shape: a suppression never retires, so a `len`-keyed trigger dispatches a full fold that
/// retires nothing, every interval, for ever.
///
/// **Mutations this kills:** never consulting the schedule (no fold happens at all); keying the
/// gauge on `Overlay::len()` rather than `deleted_len()` (the fold fires one deletion early, at the
/// assertion below).
#[test]
fn the_retirable_depth_route_dispatches_a_fold_without_anyone_asking() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(
        tmp.path(),
        &root,
        config_scheduling(tessera_engine::CompactionSchedule {
            min_interval_secs: 0,
            window_start_secs: None,
            window_secs: 0,
            window_min_segments: 0,
            // The segment ceiling is off, so nothing but the deletion gauge can dispatch here —
            // which is what makes this case about that gauge rather than about the fixture's
            // segment count.
            max_segments: None,
            after_deletions: Some(3),
            tombstoned_rows_fraction: None,
            dead_bytes_ratio: None,
        }),
    );

    let suppressed = entity_of_source(&root, "v00000", 8);
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    for source in [3u64, 4] {
        let entity = entity_of_source(&root, "v00000", source);
        engine
            .accept_change(entity, ChangeOp::Delete)
            .expect("a delete is accepted");
    }
    assert_eq!(
        engine.overlay_depth(),
        3,
        "depth is three; the retirable part is two"
    );
    // A suppression is not retirable, so it moves the gauge a fold keys on by nothing — and a
    // trigger reading total depth instead would have fired here.
    assert_no_fold_within(&engine, 2);

    let third = entity_of_source(&root, "v00000", 5);
    engine
        .accept_change(third, ChangeOp::Delete)
        .expect("a delete is accepted");
    tick(&engine);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().folds == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the schedule never dispatched a fold"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        engine.overlay_depth(),
        1,
        "the three deletions retired and the suppression did not — Rule S is untouched by a \
         scheduled fold exactly as it is by a requested one"
    );
}

/// `(on_disc − named) / named` under the live prefix — the same two-map sum the schedule's
/// dead-bytes gauge takes, computed here so a failure reports the ratio rather than only its
/// verdict.
///
/// **Two manifests.** The build's artefacts are digested in the bundle-level `MANIFEST.json` and
/// everything the write path produced is in the partition's side-manifest; summing one alone
/// reported a 1065× orphan ratio in a measured run, which was a missing addend and not a leak.
fn dead_ratio(root: &Path, engine: &Engine) -> f64 {
    fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), total);
            } else {
                *total += meta.len();
            }
        }
    }
    let generation = engine.generation();
    let mut on_disc = 0u64;
    walk(&root.join(&generation.prefix), &mut on_disc);
    let named: u64 = generation
        .bundle
        .manifest
        .files
        .values()
        .map(|d| d.size)
        .chain(
            generation
                .bundle
                .partitions
                .values()
                .flat_map(|p| p.manifest.files.values())
                .map(|d| d.size),
        )
        .sum();
    on_disc.saturating_sub(named) as f64 / named.max(1) as f64
}

/// **The tombstoned-row route dispatches a fold on a bundle every count gauge calls healthy**
/// (compaction §9).
///
/// This is the gauge's whole reason for existing. The fixture here has one segment, no window, and
/// an overlay far below any absolute threshold — a deployment the segment ceiling and the
/// retirable-depth route both look at and see nothing wrong. What it also has is a fifth of its
/// rows tombstoned: rows that exist, that every viewport scans, and that no viewer may see. The
/// absolute route cannot reach this, because `after_deletions` is a count and this is a *ratio*: a
/// 50,000-row deployment crosses a fifth long before it crosses 500,000 deletions, and a 10⁹-row
/// one the other way round.
///
/// **Mutations this kills:** dropping the route (nothing dispatches); windowing it (the window is
/// off here, so a windowed route can never fire); using `Overlay::len()` as the numerator (the
/// suppression below would carry it over the threshold one deletion early); dropping the
/// zero-denominator guard, which `a_ratio_with_a_zero_denominator_never_fires` pins at the unit
/// level and which this case cannot reach.
#[test]
fn the_tombstoned_row_route_dispatches_a_fold_on_a_bundle_no_count_gauge_would_fold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    // **A fiftieth rather than compaction §9's default fifth, and the number is not the subject.**
    // What this case is about is that a *ratio* fires where no count does; the fraction only has to
    // be one the fixture can cross, and every deletion below is a deny-lane round trip.
    let threshold = 0.02f64;
    let needed = (N_ITEMS as f64 * threshold).ceil() as u64;
    let engine = engine_over_fixture(
        tmp.path(),
        &root,
        config_scheduling(tessera_engine::CompactionSchedule {
            min_interval_secs: 0,
            // Every other route off: the fraction is the only thing that can dispatch, which is
            // what makes this case about the fraction rather than about the fixture.
            window_start_secs: None,
            window_secs: 0,
            window_min_segments: 0,
            max_segments: None,
            after_deletions: None,
            tombstoned_rows_fraction: Some(threshold),
            dead_bytes_ratio: None,
        }),
    );

    // **The map is resolved once.** `entity_of_source` rebuilds it from the bundle on every call,
    // which is free for the handful of entities every other case here names and is the whole cost
    // of this one.
    let map = source_to_new_map(&root, "v00000");
    let entity = |source: u64| EntityId::new(map[&source]);

    // A suppression first, so a numerator keyed on total depth would fire one deletion early.
    engine
        .accept_change(entity(1), ChangeOp::Suppress)
        .expect("a suppression is accepted");
    for source in 2..(needed + 1) {
        engine
            .accept_change(entity(source), ChangeOp::Delete)
            .expect("a delete is accepted");
    }
    assert_eq!(
        engine.retirable_deletions(),
        needed - 1,
        "one short of the fraction, with the suppression making total depth already over it"
    );
    assert_no_fold_within(&engine, 2);

    engine
        .accept_change(entity(needed + 1), ChangeOp::Delete)
        .expect("a delete is accepted");
    tick(&engine);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().folds == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the tombstoned-row route never dispatched a fold: retirable={} rows={}",
            engine.retirable_deletions(),
            engine.live_rows()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        engine.overlay_depth(),
        1,
        "every deletion retired and the suppression did not"
    );
}

/// **The dead-bytes route dispatches a fold on a bundle with no deletions at all** (compaction §9).
///
/// The reclamation obligation, and the only route that covers it. A deployment whose merge is doing
/// its job has a bounded segment count and a shallow overlay while paying for two or three copies
/// of its corpus: every merged-away segment and every superseded side-manifest stays on disc,
/// because a step-down serves one of them, and **a fold is the only thing that reclaims them**. The
/// measured no-compaction steady state is 2.0–2.6×.
///
/// **The ratio here is 1.0 against a bundle that has merged**, so what fires it is real orphaned
/// bytes rather than a threshold set below every possible measurement — which the config loader
/// refuses and `a_ratio_gauge_takes_a_number_or_off_…` pins.
///
/// **Mutations this kills:** dropping the route (nothing dispatches, with every other gauge off and
/// nothing deleted); walking the bundle root rather than the live prefix (a fresh bundle has no
/// other prefix, so the ratio would be the same — but a *second* fold would then count the swept
/// tree and never converge); counting only one of the two manifests as named (the side-manifest
/// alone reported a 1065× orphan ratio in a measured run, so the route would fire on every bundle
/// ever built).
#[test]
fn the_dead_bytes_route_dispatches_a_fold_on_a_bundle_with_nothing_deleted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(
        tmp.path(),
        &root,
        config_scheduling(tessera_engine::CompactionSchedule {
            min_interval_secs: 0,
            window_start_secs: None,
            window_secs: 0,
            window_min_segments: 0,
            max_segments: None,
            after_deletions: None,
            // **5%, not compaction §9's default of 1.0, and the number is not the subject.** The
            // default means "paying double for storage", which is the measured no-compaction steady
            // state of a *running* deployment (2.0–2.6×) and takes more churn to reach than a test
            // should spend. What this case asserts is the route: a bundle with orphaned bytes and
            // nothing deleted folds, and one without does not.
            tombstoned_rows_fraction: None,
            dead_bytes_ratio: Some(0.05),
        }),
    );

    // **A freshly built bundle is essentially all live.** What it holds that no manifest names is
    // the manifests themselves — `MANIFEST.json` cannot carry its own digest — and that is a
    // fraction of a percent. Asserted rather than assumed, because it is the floor the threshold
    // above has to sit clear of.
    let fresh = dead_ratio(&root, &engine);
    assert!(
        fresh < 0.01,
        "a freshly built bundle should be under 1% dead, measured {fresh:.4}"
    );
    assert_no_fold_within(&engine, 2);

    // Four flush segments and the merge that consumes them: the consumed segments stay on disc,
    // named by no live manifest, which is exactly the dead weight this route is about.
    for round in 0..4 {
        ingest(
            &engine,
            format!("dead-{round}").into_bytes(),
            &format!("d{round}"),
        )
        .expect("ingest is accepted");
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_for("a flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().folds == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the dead-bytes route never dispatched a fold: dead ratio is {:.3}",
            dead_ratio(&root, &engine)
        );
        engine.request_flush();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        engine.overlay_depth(),
        0,
        "nothing was ever deleted — this fold ran for the disc and not for the overlay"
    );
    // And the fold did what the route dispatched it for: the superseded prefix is gone whole, so
    // what was dead is reclaimed rather than merely rewritten beside itself.
    assert!(!root.join("v00000").exists());
    let after = dead_ratio(&root, &engine);
    assert!(
        after < fresh.max(0.01),
        "the folded bundle is back to a freshly-built one's dead fraction, measured {after:.4}"
    );
}

/// **The segment ceiling dispatches a fold at any hour** — the route that says deferring segment
/// growth to the next window has stopped being cheaper than folding now.
///
/// The window here is deliberately **shut** (it opens in six hours) and the deletion route is off,
/// so the only thing that can dispatch is the ceiling. Without it a deployment ingesting through
/// the day reaches a segment count every viewport pays for and waits until midnight anyway, which
/// is the hole a windowed-only segment gauge leaves.
///
/// **Mutations this kills:** windowing the ceiling (nothing folds); dropping the ceiling route
/// (same); reading the ceiling against the window's floor instead of its own value (the fold fires
/// one flush early, at the first assertion).
#[test]
fn the_segment_ceiling_dispatches_a_fold_outside_the_window() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let now = utc_time_of_day();
    let engine = engine_over_fixture(
        tmp.path(),
        &root,
        config_scheduling(tessera_engine::CompactionSchedule {
            min_interval_secs: 0,
            // Shut: opens in six hours, for one hour.
            window_start_secs: Some((now + 6 * 3_600) % 86_400),
            window_secs: 3_600,
            window_min_segments: 2,
            max_segments: Some(3),
            after_deletions: None,
            tombstoned_rows_fraction: None,
            dead_bytes_ratio: None,
        }),
    );
    // A merge would collapse the extents this case is counting, and bounding the segment axis is
    // exactly what it does — so it is off, and the fold is the only thing moving the count.
    engine.set_merge_for_test(false);

    let flush_once = |n: usize| {
        let before = engine.write_executor_stats().flushes;
        ingest(&engine, format!("seg-{n}").into_bytes(), &format!("b{n}")).expect("ingest");
        engine.request_flush();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while engine.write_executor_stats().flushes == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };

    // The base plus one extent: two segments, which clears the *window's* floor and not the
    // ceiling — and the window is shut, so nothing may happen.
    // Two segments is worth a fold tonight and not worth one now, and it is not tonight.
    flush_once(1);
    assert_no_fold_within(&engine, 2);

    // The base plus two extents: three, which is the ceiling.
    flush_once(2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().folds == 0 {
        tick(&engine);
        assert!(
            std::time::Instant::now() < deadline,
            "the ceiling never dispatched a fold outside the window"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.generation().prefix, "v00001");
    let bundle = open_bundle(&root).expect("the folded bundle opens");
    assert_eq!(
        bundle.partitions["default"].views["s0"].segments.len(),
        1,
        "and the fold did what the gauge asked for: one segment per partition-view"
    );
}

/// **The windowed route fires inside its window and not outside it**, which is the whole of what a
/// start time buys: a fold is minutes to hours of IO that costs a concurrent viewport a measured
/// up-to-2.03×, and an operator setting `00:00` is saying "not during the day".
///
/// Both halves in one case, against the same clock: a window that opened half an hour ago folds,
/// and one that opens in six hours does not — with the same segment count, so the only variable is
/// the hour.
///
/// **Both halves are load-bearing, and the fixture is built so that neither alone explains the
/// outcome**: the threshold is two against a one-segment bundle, so the open-window arm dispatches
/// nothing until a flush supplies the second segment. An earlier version used a threshold of one,
/// which the fixture already met — so removing the segment gate entirely left this case green, and
/// it proved only half of what it claimed. Verified by running that mutation.
///
/// **Mutations this kills:** ignoring the window and firing on the segment gauge alone (the closed
/// arm folds); ignoring the segment gauge (the open-but-below-threshold arm folds).
#[test]
fn the_windowed_route_fires_inside_its_window_and_not_outside_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let now = utc_time_of_day();
    let closed = tessera_engine::CompactionSchedule {
        min_interval_secs: 0,
        // Opens in six hours, for one hour: closed now, whatever "now" is when this runs.
        window_start_secs: Some((now + 6 * 3_600) % 86_400),
        window_secs: 3_600,
        // Two, against a fixture that builds one segment — so the gate is what decides, not the
        // fixture.
        window_min_segments: 2,
        // Both any-hour routes off: the window is the only thing that can dispatch, which is the
        // whole of what this case is asking.
        max_segments: None,
        after_deletions: None,
        tombstoned_rows_fraction: None,
        dead_bytes_ratio: None,
    };
    let open = tessera_engine::CompactionSchedule {
        window_start_secs: Some((now + 86_400 - 1_800) % 86_400),
        ..closed
    };

    let engine = engine_over_fixture(tmp.path(), &root, config_scheduling(closed));
    engine.set_merge_for_test(false);
    // Two segments, so the *only* thing keeping this from folding is the shut window.
    ingest(&engine, b"second-segment".to_vec(), "b1").expect("ingest");
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Two segments is over the threshold and the window is shut, so nothing happens — a start time
    // that fires outside its own window is not a start time.
    assert_no_fold_within(&engine, 2);
    assert_eq!(engine.generation().prefix, "v00000");
    drop(engine);

    // A fresh deployment, the window now open, and **one** segment: below the threshold, so the
    // window opening is not on its own a reason to fold.
    let tmp2 = tempfile::TempDir::new().unwrap();
    let root2 = tmp2.path().join("bundle");
    let engine = engine_over_fixture(tmp2.path(), &root2, config_scheduling(open));
    engine.set_merge_for_test(false);
    // Inside the window, below the threshold: there is nothing here worth folding.
    assert_no_fold_within(&engine, 2);

    // The second segment arrives and the same open window now has work.
    ingest(&engine, b"second-segment".to_vec(), "b1").expect("ingest");
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().folds == 0 {
        tick(&engine);
        assert!(
            std::time::Instant::now() < deadline,
            "the schedule never dispatched a fold inside its own window"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.generation().prefix, "v00001");
}

/// **Two folds in a row**, which is what makes prefix naming and the manifest counter a rule rather
/// than a coincidence: `n` continues across the prefix (contracts §2.3) and a prefix name is never
/// reused.
#[test]
fn a_second_fold_publishes_a_third_prefix_and_reclaims_the_second() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");
    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00002");
    assert!(!root.join("v00000").exists());
    assert!(!root.join("v00001").exists());

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(visible(&engine, &session), N_ITEMS);
}
