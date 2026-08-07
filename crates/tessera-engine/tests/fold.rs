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

use std::path::Path;

use common::*;
use tessera_authz::{PostingRef, PostingsReader};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::ChangeOp;
use tessera_store::read::open_bundle;
use tessera_types::{EntityId, TermId};

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
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 100) as usize),
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

/// Ingest one item at (5, 5) carrying the fixture's `ALL_TERM`, under `external_id`.
fn ingest(
    engine: &Engine,
    external_id: Vec<u8>,
    batch: &str,
) -> Result<EntityId, tessera_engine::AcceptError> {
    let row = UnallocatedRow {
        external_id: Some(external_id),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
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
    assert!(!members.is_empty(), "the engine writes a WAL sequence, not one file");
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
    let row_space = &bundle.partitions["default"].slices["s0"].row_space;
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
        !postings_name(&postings, TermId::new(ALL_TERM as u32), deleted),
        "the folded entity is in no posting of the new term index — both halves, or Rule F's \
         retirement re-exposes it"
    );
    assert!(
        postings_name(&postings, TermId::new(ALL_TERM as u32), survivor),
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
    let slice = &bundle.partitions["default"].slices["s0"];
    let row_space = &slice.row_space;
    assert_eq!(
        row_space.total_rows(),
        N_ITEMS - deleted.len() as u64,
        "row space holds exactly the survivors"
    );

    let segment = &slice.segments[0];
    let mut rows_seen = std::collections::BTreeSet::new();
    for source in 0..N_ITEMS {
        let entity = EntityId::new(source_to_entity[&source]);
        match row_space.row_of(entity) {
            None => assert!(deleted.contains(&entity), "only a deleted entity loses its row"),
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
        assert!(std::time::Instant::now() < deadline, "the flush never landed");
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
    restarted.start_write_executor(8).expect("the executor starts");
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
        assert!(std::time::Instant::now() < deadline, "the flush never landed");
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

    assert_eq!(engine.overlay_depth(), 1, "Rule S: a suppression never retires");
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
        engine.generation().prefix, "v00000",
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
/// Returns `(the entity the mid-flight flush published, the entity deleted before the fold whose
/// row that flush carries)`.
///
/// The hold is a test hook and models duration, not behaviour — see
/// `Engine::set_fold_paused_for_test`. Everything the flush does here it does exactly as it would
/// in production: it plans against the live generation, writes into the **old** prefix, and
/// publishes there, because the fold has not flipped `CURRENT` yet.
fn fold_with_a_flush_in_flight(engine: &Engine, key: Vec<u8>) -> EntityId {
    engine.set_fold_paused_for_test(true);
    let before = engine.write_executor_stats();
    engine.request_fold();

    // Wait until the fold's passes are done and it is holding: from here everything published is
    // post-snapshot and must be carried forward rather than folded.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !engine.fold_is_holding_for_test() {
        assert!(std::time::Instant::now() < deadline, "the fold never reached its hold");
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

    engine.set_fold_paused_for_test(false);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(now.fold_failures, before.fold_failures, "the fold was discarded");
        if now.folds > before.folds {
            return entity;
        }
        assert!(std::time::Instant::now() < deadline, "the fold never published");
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
/// the fold's own base (the reader takes the first segment of a slice as the base
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
    let entity = fold_with_a_flush_in_flight(&engine, key.clone());

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
        partition.slices["s0"].row_space.extent_count(),
        1,
        "one carried-forward extent above the folded base"
    );
    assert!(
        partition.slices["s0"].row_space.row_of(entity).is_some(),
        "whose rows the base permutation does not claim"
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
/// **Mutations this kills:** retiring `D₀` wholesale (both retire, which is the r3 fail-open);
/// leaving the carry-forward set empty; building it from the plan rather than from the live
/// manifest at publication (the flush's four artefacts are not in the plan, so nothing is carried
/// and both retire).
///
/// **What it does *not* kill, verified rather than assumed: dropping either single artefact kind.**
/// A flush publishes a segment, a tier, a run and a locator extent *together, over one entity
/// range*, so end to end the segment adder and the locator adder each cover `doomed` on their own
/// and removing either leaves this case green — checked by running both mutations. That is not a
/// hole in the rule, it is the reason the rule is stated over the whole carry-forward set; the
/// per-artefact independence is where a fixture can actually separate them, in `compact.rs`'s
/// `none_of_obligation_2bs_three_shapes_retires`, which gives one flush only a segment and another
/// only a locator extent. Read the two together.
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
    let c = ingest(&engine, b"c".to_vec(), "c").expect("ingest is accepted");
    assert_eq!(doomed.raw(), a.raw() + 1);
    assert_eq!(c.raw(), doomed.raw() + 1);
    engine
        .accept_change(doomed, ChangeOp::Delete)
        .expect("a delete is accepted");

    // A second deletion, of an entity the base holds and nothing carries forward: the fold removes
    // its row and its postings, so it retires. Without it this case could not tell "the rule
    // protected the right entity" from "the rule retired nothing".
    let folded = entity_of_source(&root, "v00000", 4);
    engine
        .accept_change(folded, ChangeOp::Delete)
        .expect("a delete is accepted");
    assert_eq!(engine.overlay_depth(), 2);

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
        assert_eq!(now.fold_failures, before.fold_failures, "the fold was discarded");
        if now.folds > before.folds {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the fold never published");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        engine.overlay_depth(),
        1,
        "one deletion retired and one did not: `executed = {{ e ∈ D₀ : no carried-forward \
         artefact names e }}`"
    );
    assert!(
        engine.generation().overlay.is_deleted(doomed),
        "the entity a carried-forward segment's declared range names keeps its tombstone for \
         another round — fail-closed, and the next fold takes it"
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

    let entity = fold_with_a_flush_in_flight(&engine, b"landed-mid-fold".to_vec());

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

/// **The flip does not refuse anything: a fold's publication does not arm the refresh shed**
/// (decision 0053).
///
/// A merge arms it, and a racer inside a merge's refresh window is shed with a 429 —
/// `merge.rs`'s `a_racer_inside_a_merges_refresh_window_is_shed_rather_than_rebuilding` is that
/// case, and this is deliberately its mirror. The rule that separates them: *shed only while the
/// refresh pass is shorter than the rebuild it would save.* A merge satisfies it (a ~0.7 s pass
/// against a measured 4 550 ms rebuild); a fold inverts it by two orders (~180 s against 10.7 s),
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
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 100) as usize),
        )
        .expect("a request after a fold takes an ordinary cache miss, never a refusal");
    assert_eq!(
        served.tiles[0].visible, baseline,
        "and it is served the whole map, rebuilt inline against the new row space"
    );
    engine.set_refresh_paused_for_test(false);
}

/// **Obligations 5 and 14: masked counts are identical across the flip for every principal, up to
/// exactly the folded deletions — and the fold's output cannot reach a principal's response.**
///
/// Three principals, including a sparse one and one granted nothing at all, each established
/// *before* the flip and asked again after it. The fold reads every column unmasked, which is
/// sanctioned only because its outputs are bundle artefacts (I2, spec §11) — so the thing to prove
/// is that having read them changes no answer: each principal still sees exactly `M_auth`, and the
/// deletion each of them could see is the only difference.
///
/// **The zero-grant principal is the one that would catch a leak.** It is authorised for no term,
/// so its `M_auth` is empty and *any* item reaching it is a disclosure — which is what a fold that
/// published its unmasked read into anything a request composes against would produce.
///
/// **Mutations this kills:** publishing the fold's own row set as a mask rather than rebuilding
/// each session's (the zero-grant count stops being zero); folding the subset term's postings away
/// (the sparse principal's count collapses).
#[test]
fn masked_counts_are_identical_across_the_flip_for_every_principal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root, config_uncapped());

    // Source 3 carries both terms, so it is visible to the full grant *and* to the sparse one —
    // which is what makes "up to exactly the folded deletions" a claim about both counts.
    let deleted = entity_of_source(&root, "v00000", 3);

    let principals = [
        ("full", full_coverage_credential()),
        ("subset", subset_credential()),
        ("zero", zero_credential()),
    ];
    let sessions: Vec<_> = principals
        .iter()
        .map(|(name, credential)| {
            let session = engine.authorise(credential).unwrap();
            (*name, visible(&engine, &session), session)
        })
        .collect();
    assert_eq!(sessions[2].1, 0, "a zero-grant principal sees nothing to begin with");
    assert!(
        sessions[1].1 > 0 && sessions[1].1 < sessions[0].1,
        "and the sparse principal sees some but not all of it: {:?}",
        sessions.iter().map(|(n, c, _)| (*n, *c)).collect::<Vec<_>>()
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
        visible(&engine, &sessions[2].2),
        0,
        "and the zero-grant principal still sees nothing — the fold read every column unmasked, \
         and none of that reached a response"
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
            after_deletions: Some(3),
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
    assert_eq!(engine.overlay_depth(), 3, "depth is three; the retirable part is two");
    tick(&engine);
    tick(&engine);
    assert_eq!(
        engine.write_executor_stats().folds,
        0,
        "a suppression is not retirable, so it moves the gauge a fold keys on by nothing — and a \
         trigger reading total depth instead would have fired here"
    );

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

/// **The windowed route fires inside its window and not outside it**, which is the whole of what a
/// start time buys: a fold is minutes to hours of IO that costs a concurrent viewport a measured
/// up-to-2.03×, and an operator setting `00:00` is saying "not during the day".
///
/// Both halves in one case, against the same clock: a window that opened half an hour ago folds,
/// and one that opens in six hours does not — with the same segment count, so the only variable is
/// the hour.
///
/// **Mutations this kills:** ignoring the window and firing on the segment gauge alone (the closed
/// case folds); ignoring the segment gauge (the closed case is unaffected, but the *first*
/// assertion below — that a window over an already-folded bundle dispatches nothing — fails).
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
        window_min_segments: 1,
        after_deletions: None,
    };
    let engine = engine_over_fixture(tmp.path(), &root, config_scheduling(closed));

    tick(&engine);
    tick(&engine);
    assert_eq!(
        engine.write_executor_stats().folds,
        0,
        "one segment is over the threshold and the window is shut, so nothing happens — a start \
         time that fires outside its own window is not a start time"
    );
    assert_eq!(engine.generation().prefix, "v00000");

    // The same deployment, the same segment count, a window that opened half an hour ago.
    drop(engine);
    let open = tessera_engine::CompactionSchedule {
        window_start_secs: Some((now + 86_400 - 1_800) % 86_400),
        ..closed
    };
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_scheduling(open),
    )
    .expect("the engine reopens");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);

    tick(&engine);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().folds == 0 {
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
