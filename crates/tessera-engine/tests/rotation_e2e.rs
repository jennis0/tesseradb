//! Rotation end to end: a published flush reclaims the log, and nothing acked is lost doing it.
//!
//! `tessera-lifecycle`'s `rotation.rs` pins the sequence's own properties — oldest-first deletion,
//! the gap refusal, the position chain. What needs a whole engine is the two things the ordering
//! exists for, because both need a flush to have happened: that a row acked while a flush was in
//! flight survives the rotation that follows it, and that a suppression outlives the reclamation of
//! the `Change` record that carried it.

mod common;

use std::time::Duration;

use common::*;
use tessera_engine::Engine;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::ChangeOp;
use tessera_types::EntityId;

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
    wait_until("the rotation to follow it", WAIT, || {
        members(tmp.path()).len() >= 2
    });

    // The buffer is empty — everything ingested has geometry — so the whole durable prefix was
    // reclaimable and member 1 goes. What survives is the member the snapshot was just written to.
    wait_until("member 1 to be reclaimed", WAIT, || {
        !members(tmp.path()).contains(&"wal-000001.log".to_string())
    });
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
        wait_until("a rotation", WAIT, || members(tmp.path()).len() >= 2);
        second
    };

    // The reopened engine must find the second row somewhere — buffered from a surviving WAL
    // member, or already flushed into a segment. What it must not be is nowhere.
    let reopened = engine_at(tmp.path(), &root, 3600);
    let bundle = tessera_store::open_bundle(&root).expect("the bundle opens");
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
/// The end state asserted here is decision 0047's *forgotten*: no row, no buffer entry, the id
/// still burned, and the deny still in force across a restart from the rotation's snapshot alone.
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
    let bundle = tessera_store::open_bundle(&root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    assert!(
        partition.views["s0"].row_space.row_of(deleted).is_none(),
        "and it acquired no geometry on the way out"
    );
    assert!(
        reopened.allocator_high_water() >= deleted.raw(),
        "the id stays burned (I9) — a deletion never returns one to the allocator"
    );
}

/// One row for `external_id` in `view`, for a batch of its own. A second view of an item already
/// ingested is a join: admission resolves the external id to the entity it already names.
fn ingest_into(engine: &Engine, batch: &str, external_id: &str, view: &str) -> EntityId {
    let descriptors = vec![b"0".to_vec()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(batch.as_bytes()) {
        *slot = *byte;
    }
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: view.to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], batch.to_string(), hash)
        .expect("the batch is accepted")[0]
}

/// A second plain view, so an item can hold a row in two of them.
fn create_second_view(engine: &Engine) {
    engine
        .create_plain_view(tessera_engine::PlainViewDeclaration {
            name: "s1".to_string(),
            title: None,
            projection: "none".to_string(),
            frame: tessera_engine::DeclaredFrame {
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
    ingest_into(engine, "wake", "ext-wake", "s0");
    flush(engine);
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
        let id = ingest_into(&engine, "b1", "ext-1", "s0");
        flush(&engine);

        // A row in the second view, unflushed, and then the delete that takes both.
        assert_eq!(ingest_into(&engine, "b2", "ext-1", "s1"), id);
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

/// **The same for a values fill: a deleted entity's unflushed cells are not rebuilt either.**
///
/// A fill is not a row — it creates nothing and names an entity that exists — so an entity may hold
/// one and no buffered row at all. It pins the log exactly as a row does, because the record is the
/// only copy of the values until a flush writes them, and no flush writes a deleted entity's.
#[test]
fn a_deleted_entitys_values_fill_is_not_rebuilt_at_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture_in(tmp.path());

    let (deleted, inherited) = {
        let engine = engine_at(tmp.path(), &root, 3600);
        engine
            .declare_attribute(tessera_engine::AttributeRequest {
                name: "note".to_string(),
                title: None,
                ty: "keyword".to_string(),
                vocabulary: None,
                analyser: None,
                index: true,
                render: false,
                scope: tessera_types::layer::LayerScope::Entity,
            })
            .expect("the column is declared");
        let id = ingest_into(&engine, "b1", "ext-1", "s0");
        flush(&engine);
        assert!(
            !engine.generation().buffer.contains(id),
            "the flush consumed the row, so the fill below is all the buffer holds for it"
        );

        engine
            .fill_values(tessera_engine::ValuesRequest {
                batch_id: "v1".to_string(),
                body_hash: [1u8; 32],
                view: "s0".to_string(),
                columns: vec!["note".to_string()],
                rows: vec![tessera_engine::IncomingValues {
                    entity: id,
                    values: vec![WalScalar::Utf8("a private note".to_string())],
                }],
                artifacts: Default::default(),
            })
            .expect("the fill is accepted");
        assert_eq!(engine.generation().buffer.fill_count(), 1);
        engine
            .accept_change(id, ChangeOp::Delete)
            .expect("the delete is accepted");
        assert_eq!(
            engine.generation().buffer.fill_count(),
            0,
            "the live path takes the fill with the rows"
        );
        (id, members(tmp.path()))
    };

    let reopened = engine_at(tmp.path(), &root, 1);
    assert_eq!(
        reopened.generation().buffer.fill_count(),
        0,
        "replay must take the deleted entity's fill too"
    );
    assert!(!reopened.generation().buffer.contains(deleted));
    inherited_members_reclaimed(&reopened, tmp.path(), &inherited);
}
