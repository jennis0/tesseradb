//! Rotation end to end: a published flush reclaims the log, and nothing acked is lost doing it.
//!
//! `mosaica-lifecycle`'s `rotation.rs` pins the sequence's own properties — oldest-first deletion,
//! the gap refusal, the position chain. What needs a whole engine is the two things the ordering
//! exists for, because both need a flush to have happened: that a row acked while a flush was in
//! flight survives the rotation that follows it, and that a suppression outlives the reclamation of
//! the `Change` record that carried it.

mod common;

use std::time::Duration;

use common::*;
use mosaica_engine::Engine;
use mosaica_lifecycle::command::UnallocatedRow;

use mosaica_lifecycle::ChangeOp;
use mosaica_types::EntityId;

const WAIT: Duration = Duration::from_secs(20);

fn members(tmp: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wal-") && n.ends_with(".log"))
        .collect();
    names.sort();
    names
}

/// **A flush rotates the log**, which is the whole point of the sequence: without this the WAL
/// grows for the life of the deployment, because nothing else can reclaim the front of it.
#[test]
fn a_published_flush_rotates_the_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    assert_eq!(members(tmp.path()), vec!["wal-000001.log"]);

    ingest(&engine, "ext-1");
    wait_until("the flush to publish", WAIT, || {
        engine.write_executor_stats().flushes >= 1
    });
    // The buffer is empty — everything ingested has geometry — so the whole durable prefix was
    // reclaimable and member 1 goes. What survives is the member the snapshot was just written to.
    // Only a rotation deletes a member, and it opens the next one in the same call, so the two
    // are waited for as one state.
    wait_until(
        "the rotation to follow it and reclaim member 1",
        WAIT,
        || !members(tmp.path()).contains(&"wal-000001.log".to_string()),
    );
}

/// **A row acked while a flush was in flight survives the rotation that follows it.**
///
/// This is `wal_pos`'s definition, pinned by its consequence rather than by its prose. Under the
/// other reading — `wal_pos` is the `Flush` record's own offset — rotation deletes rows appended
/// after the flush's snapshot point, which were never consumed and carry entity ids at or above the
/// new watermark. §7.1 then reconstructs them from nothing: acked ingest, silently lost at the next
/// restart.
///
/// The second row is ingested while the first flush is still running, so it is in the buffer, not
/// in a segment, when the rotation decides what to reclaim.
#[test]
fn a_row_acked_during_a_flush_survives_rotation_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());

    let second = {
        // A slow tick, so the two ingests land in different flushes rather than the same one.
        let engine = engine_at(tmp.path(), &root, 1);
        ingest(&engine, "ext-1");
        wait_until("the first flush", WAIT, || {
            engine.write_executor_stats().flushes >= 1
        });

        // Acked after that flush consumed the first row, so it is buffered when the rotation runs.
        let second = ingest(&engine, "ext-2");
        assert!(engine.generation().buffer.contains(second));
        // The first flush's rotation opened member 2 before this ingest was acked; the member
        // the next rotation opens is the one that follows the second row's flush.
        wait_until("a rotation", WAIT, || {
            members(tmp.path())
                .iter()
                .any(|m| m.as_str() >= "wal-000003.log")
        });
        second
    };

    // The reopened engine must find the second row somewhere — buffered from a surviving WAL
    // member, or already flushed into a segment. What it must not be is nowhere.
    let reopened = engine_at(tmp.path(), &root, 3600);
    let bundle = mosaica_store::open_bundle(&root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    let has_geometry = partition.views["s0"].row_space.row_of(second).is_some();
    assert!(
        reopened.generation().buffer.contains(second) || has_geometry,
        "an acked ingest was silently lost: entity {} is in neither the buffer nor a segment after \
         a rotation and a restart",
        second.raw()
    );
}

/// **A suppression outlives the reclamation of the `Change` record that carried it.**
///
/// The overlay's only durable home is the WAL, and a suppression retires *only* on unsuppress — so
/// the rotation's snapshot is the sole thing standing between a reclaimed member and an item that
/// silently comes back at the next restart.
#[test]
fn a_suppression_accepted_before_a_rotation_is_still_in_force_after_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());

    let suppressed = {
        let engine = engine_at(tmp.path(), &root, 1);
        let id = ingest(&engine, "ext-1");
        engine
            .accept_change(id, ChangeOp::Suppress)
            .expect("the suppression is accepted");

        wait_until("the flush", WAIT, || {
            engine.write_executor_stats().flushes >= 1
        });
        wait_until("member 1 to be reclaimed", WAIT, || {
            !members(tmp.path()).contains(&"wal-000001.log".to_string())
        });
        id
    };

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(
        reopened.generation().overlay.is_suppressed(suppressed),
        "the record that carried this suppression was reclaimed; only the rotation's snapshot \
         stands between that and a re-exposed item"
    );
}

/// **A row deleted before its first flush does not pin the log** (write-path §4.2).
///
/// A deleted row acquires no geometry — `plan_flush` skips it — so no flush ever consumes it, and
/// before the delete removed it from the buffer it sat there for the process's lifetime holding
/// `oldest_wal_pos` down: its member, and every member after it, unreclaimable. A deployment that
/// deletes before flushing therefore stopped reclaiming the log at all, which is the one lane that
/// structurally cannot be shed.
///
/// The end state asserted here is decision 0047's *forgotten*: no row, no buffer entry, the id not
/// issued again before a fold frees it, and the deny still in force across a restart from the
/// rotation's snapshot alone.
#[test]
fn a_row_deleted_before_its_first_flush_stops_pinning_the_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());

    let deleted = {
        let engine = engine_at(tmp.path(), &root, 1);
        let id = ingest(&engine, "ext-1");
        assert!(engine.generation().buffer.contains(id));
        engine
            .accept_change(id, ChangeOp::Delete)
            .expect("the delete is accepted");
        assert!(
            !engine.generation().buffer.contains(id),
            "the delete must drop the row from the buffer; nothing else ever will"
        );

        // No flush publishes — the one buffered row was deleted — so what rotates is the tick's
        // own growth-gated rotation, and it may only reclaim because the buffer is now empty.
        wait_until("member 1 to be reclaimed", WAIT, || {
            !members(tmp.path()).contains(&"wal-000001.log".to_string())
        });
        id
    };

    let reopened = engine_at(tmp.path(), &root, 3600);
    let generation = reopened.generation();
    assert!(
        generation.overlay.is_deleted(deleted),
        "the record that carried this deletion was reclaimed; the rotation's snapshot is what \
         keeps it in force"
    );
    assert!(
        !generation.buffer.contains(deleted),
        "a reclaimed row must reconstruct nothing — the entity is forgotten (decision 0047)"
    );
    let bundle = mosaica_store::open_bundle(&root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    assert!(
        partition.views["s0"].row_space.row_of(deleted).is_none(),
        "and it acquired no geometry on the way out"
    );
    assert!(
        reopened.allocator_high_water() >= deleted.raw(),
        "the high-water stays past the deleted id (I9): only a fold frees a deleted item's number"
    );
}

/// One row in `view`, for a batch of its own, adding `item` to the view where one is named.
fn ingest_into(engine: &Engine, batch: &str, item: Option<EntityId>, view: &str) -> EntityId {
    let descriptors = vec![b"0".to_vec()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(batch.as_bytes()) {
        *slot = *byte;
    }
    let row = UnallocatedRow {
        view: view.to_string(),
        join: item,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .ingest_rows(vec![row], batch.to_string(), hash)
        .expect("the batch is accepted")[0]
}

/// A second plain view, so an item can hold a row in two of them.
fn create_second_view(engine: &Engine) {
    engine
        .create_plain_view(mosaica_engine::PlainViewDeclaration {
            name: "s1".to_string(),
            title: None,
            projection: "none".to_string(),
            frame: mosaica_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
        })
        .expect("the second view is created");
}

/// Wake the reopened engine, and answer whether every member the restart inherited has gone.
///
/// A rotation reclaims below the buffer's oldest position, so a member holding a record nothing
/// will ever consume stays for the life of the process, and so does every member after it.
fn inherited_members_reclaimed(engine: &Engine, tmp: &std::path::Path, inherited: &[String]) {
    ingest_into(engine, "wake", None, "s0");
    publish_buffered(engine);
    wait_until("the inherited members to be reclaimed", WAIT, || {
        let now = members(tmp);
        !inherited.iter().any(|name| now.contains(name))
    });
}

/// **A deleted entity's join row does not survive a restart, and does not pin the log.**
///
/// The live delete takes every row the entity holds, in whichever view. Replay rebuilds the buffer
/// from the log and has to take the same set: a join carries no terms and is invisible to the
/// entity-space walk, so a rule written over that walk leaves it behind — and no flush will ever
/// consume a deleted entity's row, so it holds the rotation bound at its own position for the life
/// of the process.
#[test]
fn a_deleted_entitys_join_row_is_not_rebuilt_at_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());

    let (deleted, inherited) = {
        let engine = engine_at(tmp.path(), &root, 3600);
        create_second_view(&engine);
        let id = ingest_into(&engine, "b1", None, "s0");
        publish_buffered(&engine);

        // A row in the second view, unflushed, and then the delete that takes both.
        assert_eq!(ingest_into(&engine, "b2", Some(id), "s1"), id);
        assert!(engine.generation().buffer.contains(id));
        engine
            .accept_change(id, ChangeOp::Delete)
            .expect("the delete is accepted");
        assert!(
            !engine.generation().buffer.contains(id),
            "the live path takes the join row with the rest"
        );
        (id, members(tmp.path()))
    };

    let reopened = engine_at(tmp.path(), &root, 1);
    assert!(
        !reopened.generation().buffer.contains(deleted),
        "replay must take the join row too: nothing will ever flush it"
    );
    assert_eq!(
        reopened.generation().buffer.len(),
        0,
        "and nothing else of the deleted entity is buffered either"
    );
    inherited_members_reclaimed(&reopened, tmp.path(), &inherited);
}

/// **The same for an edit: an edited item deleted before its flush is not rebuilt either.**
///
/// An edit buffers the item's new entity and deletes the old one in one record, so a restart meets
/// both; the item's later deletion takes the new entity's rows, and replay must too.
#[test]
fn a_deleted_edits_rows_are_not_rebuilt_at_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());

    let (edited, inherited) = {
        let engine = engine_at(tmp.path(), &root, 3600);
        let first = ingest_into(&engine, "b1", None, "s0");
        publish_buffered(&engine);
        assert!(!engine.generation().buffer.contains(first));

        let mut hash = [2u8; 32];
        hash[0] = b'e';
        let row = UnallocatedRow {
            view: "s0".to_string(),
            join: Some(first),
            descriptors: vec![b"0".to_vec()],
            x: 7.0,
            y: 7.0,
            scalars: Vec::new(),
            terms: Vec::new(),
            scoped: Vec::new(),
        };
        let edited = engine
            .ingest_rows(vec![row], "b2".to_string(), hash)
            .expect("the edit is accepted")[0];
        assert_ne!(edited, first, "an edit moves the item to a new entity");
        assert!(engine.generation().buffer.contains(edited));
        engine
            .accept_change(edited, ChangeOp::Delete)
            .expect("the delete is accepted");
        assert_eq!(
            engine.generation().buffer.len(),
            0,
            "the live path takes the edit's rows"
        );
        (edited, members(tmp.path()))
    };

    let reopened = engine_at(tmp.path(), &root, 1);
    assert_eq!(
        reopened.generation().buffer.len(),
        0,
        "replay must take the deleted edit's rows too"
    );
    assert!(!reopened.generation().buffer.contains(edited));
    inherited_members_reclaimed(&reopened, tmp.path(), &inherited);
}
