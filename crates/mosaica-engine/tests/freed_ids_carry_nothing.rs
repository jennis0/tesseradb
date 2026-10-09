//! **An entity id a fold freed carries nothing of the item that held it.** An edit moves an item
//! to a new entity; the fold that removes the old entity's rows frees its id, and the allocator
//! issues it again before any id from the high-water. The item that takes it must hold only what
//! its own rows say: no value, label, unique value, membership or content of the
//! item that held the id before, in any home, through a flush, a merge, a restart and a fold.
//!
//! The freed id is the edited item's middle entity: its first is its number, which stays reserved
//! so its `tessera_id` never names another item, and its last is the one it holds. A suppressed
//! item's old entities lose their suppression at the fold that removes their rows, while the item
//! stays suppressed through the entity it holds, so the item that takes one is not suppressed. So
//! does an entity an edit made and a deletion removed before any flush placed it. An
//! id a record the log kept names is not issued when the service restarts, since the replay has
//! applied that record to it; nor does an edit resolved to the entity before it was freed reach the
//! item that took it.

mod common;
mod homes;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use homes::fixture::*;
use homes::Home;
use tessera_engine::filter::{Endpoint, FilterOperand, Scalar};
use tessera_engine::{AcceptError, Engine, IngestRequest, ScalarOut};
use tessera_lifecycle::membership::{IncomingArtifact, IncomingContent};
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::IngestRow;
use tessera_types::{AttrLocalId, EntityId, TesseraId};

/// The source of the items the test edits twice: `X` visibly, `W` under a suppression.
const W: u64 = 20;

fn send(engine: &Engine, batch: &str, view: &str, rows: Vec<IngestRow>) -> Vec<TesseraId> {
    let mut body_hash = [0u8; 32];
    body_hash[..batch.len()].copy_from_slice(batch.as_bytes());
    engine
        .ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash,
            view: Some(view.to_string()),
            rows,
            artifacts: Default::default(),
            strict: false,
            tessera_id_column: false,
        })
        .unwrap_or_else(|e| panic!("{batch} is accepted: {e}"))
        .tessera_ids
        .into_iter()
        .map(|id| id.expect("an accepted row has a tessera_id"))
        .collect()
}

/// A row giving the item `tid` names a new score.
fn rescore(tid: TesseraId, score: i32) -> IngestRow {
    let mut row = IngestRow {
        tessera_id: Some(tid),
        labels: None,
        position: None,
        scalars: vec![WalScalar::Null; DECLARED],
        scoped: vec![WalScalar::Null; 2],
        omitted: (0..DECLARED + 2).filter(|at| *at != SCORE_AT).collect(),
    };
    row.scalars[SCORE_AT] = WalScalar::I32(score);
    row
}

/// A row creating an item with a label and a position and nothing else.
fn create(label: &str, at: (f64, f64)) -> IngestRow {
    IngestRow {
        tessera_id: None,
        labels: Some(vec![label.as_bytes().to_vec()]),
        position: Some(at),
        scalars: vec![WalScalar::Null; DECLARED],
        scoped: vec![WalScalar::Null; 2],
        omitted: (0..DECLARED + 2).collect(),
    }
}

fn entity_of(engine: &Engine, tid: TesseraId) -> EntityId {
    engine.resolve_tessera_ids(&[tid]).unwrap()[0]
        .unwrap_or_else(|| panic!("{tid:?} names an item"))
}

fn exactly(v: f64) -> FilterOperand {
    let at = |value: f64| {
        Some(Endpoint {
            value: if value.fract() == 0.0 {
                Scalar::Int(value as i128)
            } else {
                Scalar::Float(value)
            },
            inclusive: true,
        })
    };
    FilterOperand::Range {
        lo: at(v),
        hi: at(v),
    }
}

/// The group's view `z` is created in, which `x` is in too.
const Q1: &str = "quarter:q1";

/// What `z` holds: its own value in every family, and its own scoped values under `q1`.
const Z_BAND: u8 = 3;
const Z_SCORE: i32 = 9_001;
const Z_IDENT: u64 = 99_001;
const Z_HEAT: f32 = 42.5;

/// A row creating `z` in [`Q1`] with a value in every home.
fn create_z() -> IngestRow {
    IngestRow {
        tessera_id: None,
        labels: Some(vec![b"1".to_vec()]),
        position: Some((901.0, 902.0)),
        scalars: vec![
            WalScalar::U8(Z_BAND),
            WalScalar::I32(Z_SCORE),
            WalScalar::Utf8("tag-z".into()),
            WalScalar::Utf8("note-z".into()),
            WalScalar::Utf8("zeta pzq".into()),
            WalScalar::U64(Z_IDENT),
        ],
        scoped: vec![
            WalScalar::F32(Z_HEAT),
            WalScalar::Utf8("memo zmemo".into()),
        ],
        omitted: Vec::new(),
    }
}

/// A principal holding both labels, so it sees `x`, `z` and the artifacts.
fn both_credential() -> Vec<u8> {
    br#"{"terms": ["0", "1"]}"#.to_vec()
}

/// The artifact `key`'s count in `view` for a principal holding both labels.
fn count_in(engine: &Engine, view: &str, key: &str) -> Option<u64> {
    let session = engine.authorise(&both_credential()).unwrap();
    engine
        .viewport_artifacts(
            &session,
            tessera_engine::ViewportArtifactsRequest::new(view, 0, WHOLE, usize::MAX),
        )
        .expect("a viewport over the whole map")
        .artifacts()
        .into_iter()
        .find(|a| a.key.as_deref() == Some(key))
        .map(|a| a.masked_count)
}

/// `z` holds the freed id and carries its own values and nothing of `x`, which held it before, in
/// any home; `x` carries its own. Checked through what each principal is served: `x` carries `0`
/// alone and `z` carries `1` alone, so each principal sees one of them.
fn check(engine: &Engine, x: TesseraId, z: TesseraId, x_score: i32, after: &str) {
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();
    let card = engine
        .item(&subset, z)
        .unwrap()
        .unwrap_or_else(|| panic!("z has a card, after {after}"));
    let field = |name: &str| {
        card.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.clone())
    };
    // A value of `x` names `x` alone for the principal who sees it and never `z` for the one who
    // sees `z`, and a value of `z` names `z` alone for the one who sees it.
    let names_alone = |view: &str, column: &str, of_x: FilterOperand, of_z: FilterOperand| {
        assert_eq!(
            served(engine, &full, view, leaf(column, of_x.clone())),
            BTreeSet::from([x.raw()]),
            "{column} of x names x alone in {view}, after {after}"
        );
        assert!(
            !served(engine, &subset, view, leaf(column, of_x)).contains(&z.raw()),
            "{column} of x names z in {view}, after {after}"
        );
        assert_eq!(
            served(engine, &subset, view, leaf(column, of_z)),
            BTreeSet::from([z.raw()]),
            "{column} of z names z alone in {view}, after {after}"
        );
    };
    for home in Home::ALL {
        match home {
            Home::Row => {
                assert_eq!(
                    card.views.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
                    vec![Q1],
                    "Home::Row: z's views, after {after}"
                );
                assert!(served(engine, &subset, Q1, None).contains(&z.raw()));
            }
            Home::RenderColumn | Home::RenderPresence => {
                // Served first, so the projection a fold's refresh may be building is built.
                assert!(served(engine, &subset, Q1, None).contains(&z.raw()));
                let out = engine
                    .viewport(&subset, tessera_engine::ViewportRequest::new(Q1, 0, WHOLE, 10_000))
                    .unwrap();
                let at = out
                    .points
                    .tessera_ids
                    .iter()
                    .position(|t| *t == z.raw())
                    .expect("q1 serves z");
                let score = out.scalar_names.iter().position(|n| n == "score").unwrap();
                let tessera_engine::ColumnBuf::I32(scores) = &out.points.scalars[score].values
                else {
                    panic!("score renders as i32")
                };
                assert!(
                    out.points.scalars[score].is_present(at) && scores[at] == Z_SCORE,
                    "{home:?}: z renders its own score, after {after}"
                );
            }
            Home::ValueColumn => names_alone(
                Q1,
                "score",
                exactly(f64::from(x_score)),
                exactly(f64::from(Z_SCORE)),
            ),
            Home::CategoryPostings => {
                let band = |code: u8| FilterOperand::Equals(AttrLocalId::new(u32::from(code)));
                assert!(
                    served(engine, &full, Q1, leaf("band", band(band_code(X)))).contains(&x.raw()),
                    "Home::CategoryPostings: x's band names x, after {after}"
                );
                for session in [&full, &subset] {
                    assert!(
                        !served(engine, session, Q1, leaf("band", band(band_code(X))))
                            .contains(&z.raw()),
                        "Home::CategoryPostings: x's band names z, after {after}"
                    );
                }
                assert!(
                    served(engine, &subset, Q1, leaf("band", band(Z_BAND))).contains(&z.raw()),
                    "Home::CategoryPostings: z's band names z, after {after}"
                );
                assert!(
                    !served(engine, &full, Q1, leaf("band", band(Z_BAND))).contains(&z.raw()),
                    "Home::CategoryPostings: z's band shows z without its label, after {after}"
                );
            }
            Home::KeywordDictionary => names_alone(
                Q1,
                "tag",
                FilterOperand::TextEquals(tag_of(X)),
                FilterOperand::TextEquals("tag-z".into()),
            ),
            Home::TextIndex => names_alone(
                Q1,
                "prose",
                FilterOperand::Match {
                    query: format!("p{X}q"),
                    minimum: None,
                },
                FilterOperand::Match {
                    query: "pzq".into(),
                    minimum: None,
                },
            ),
            Home::RecordBlob => assert_eq!(
                (
                    field("note"),
                    field("prose"),
                    field("tag"),
                    field("band"),
                    field("ident")
                ),
                (
                    Some(ScalarOut::Utf8("note-z".into())),
                    Some(ScalarOut::Utf8("zeta pzq".into())),
                    Some(ScalarOut::Utf8("tag-z".into())),
                    Some(ScalarOut::Utf8("high".into())),
                    Some(ScalarOut::U64(Z_IDENT)),
                ),
                "Home::RecordBlob: z's stored values, after {after}"
            ),
            Home::TermPostings => {
                let seen_by_full = served(engine, &full, Q1, None);
                let seen_by_subset = served(engine, &subset, Q1, None);
                assert!(
                    seen_by_full.contains(&x.raw()) && !seen_by_full.contains(&z.raw()),
                    "Home::TermPostings: the full principal sees x and not z, after {after}"
                );
                assert!(
                    seen_by_subset.contains(&z.raw()) && !seen_by_subset.contains(&x.raw()),
                    "Home::TermPostings: the subset principal sees z and not x, after {after}"
                );
                assert!(
                    engine.item(&full, z).unwrap().is_none(),
                    "Home::TermPostings: z has a card without its label, after {after}"
                );
                assert_eq!(card.labels, vec!["1".to_string()], "z's labels, after {after}");
            }
            Home::UniqueIndex => names_alone(
                Q1,
                "ident",
                FilterOperand::NumIn(vec![Scalar::Int(i128::from(ident_of(X)))]),
                FilterOperand::NumIn(vec![Scalar::Int(i128::from(Z_IDENT))]),
            ),
            // `z` was created on the freed id, so the id is its number: the fold that freed it
            // dropped the edited-items pair naming `x`'s number, or `z` would answer `x`'s id.
            Home::EditedItems => {
                let entity = entity_of(engine, z);
                assert_eq!(
                    engine.tessera_id_of(entity).unwrap(),
                    z,
                    "Home::EditedItems: z's entity answers another tessera_id, after {after}"
                );
                assert_ne!(entity_of(engine, x), entity, "x and z name one entity");
            }
            // `t0` holds every built item, `x` among them and `W` suppressed: the sixteen `q1`
            // holds and `W` not one. `z` is a member of `tz` alone.
            Home::Membership => assert_eq!(
                (count_in(engine, Q1, "t0"), count_in(engine, Q1, "tz")),
                (Some(QUARTERS[0].1.end - QUARTERS[0].1.start), Some(1)),
                "Home::Membership: t0 and tz in q1, after {after}"
            ),
            Home::GeneratingSet => {
                let t0 = artifacts_of(engine, &full_coverage_credential())
                    .into_iter()
                    .find(|a| a.key.as_deref() == Some("t0"))
                    .expect("the artifact is served");
                assert_eq!(
                    t0.content,
                    vec![CONTENT.to_string()],
                    "Home::GeneratingSet: the content generated from x, after {after}"
                );
            }
            // Served to the subset principal above, so no suppression stands against it.
            Home::Suppression => {}
            Home::ScopedValue => {
                assert_eq!(
                    card.scoped
                        .iter()
                        .find(|s| s.name == "heat")
                        .map(|s| s.values.clone()),
                    Some(vec![("q1".to_string(), ScalarOut::F32(Z_HEAT))]),
                    "Home::ScopedValue: z's heat, after {after}"
                );
                names_alone(
                    Q1,
                    "heat@quarter:q1",
                    exactly(f64::from(heat(0, X))),
                    exactly(f64::from(Z_HEAT)),
                );
            }
            Home::ScopedProse => names_alone(
                Q1,
                "memo@quarter:q1",
                FilterOperand::Match {
                    query: format!("m0n{X}x"),
                    minimum: None,
                },
                FilterOperand::Match {
                    query: "zmemo".into(),
                    minimum: None,
                },
            ),
        }
    }
}

#[test]
fn a_freed_id_carries_nothing_of_the_item_that_held_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let map = source_to_new_map(&root, "v00000");
    let x = engine.tessera_id_of(EntityId::new(map[&X])).unwrap();
    let w = engine.tessera_id_of(EntityId::new(map[&W])).unwrap();

    // `x` is edited twice; its middle entity is the one a fold frees.
    send(&engine, "x1", "s0", vec![rescore(x, 501)]);
    publish_buffered(&engine);
    let freed = entity_of(&engine, x);
    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);
    assert_ne!(entity_of(&engine, x), freed);

    // `w` is edited twice under a suppression, which each edit carries to its new entity.
    engine
        .accept_change(EntityId::new(map[&W]), ChangeOp::Suppress)
        .unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    publish_buffered(&engine);
    let suppressed = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    publish_buffered(&engine);
    assert_eq!(
        (engine.overlay_depth(), engine.retirable_deletions()),
        (5, 4),
        "x's and w's two old entities deleted, and w's three entities suppressed"
    );

    let high_water = engine.allocator_high_water();
    fold(&engine);
    publish_buffered(&engine);
    assert_eq!(
        engine.overlay_depth(),
        1,
        "the fold leaves w's suppression on the entity it holds and on no entity it removed"
    );

    // Three new items: `z`, first in the allocation order, takes x's freed id, and `z2` takes the
    // id w's suppressed middle entity left, with no suppression.
    let made = send(
        &engine,
        "new",
        Q1,
        vec![
            create_z(),
            create("1", (903.0, 904.0)),
            create("1", (905.0, 906.0)),
        ],
    );
    let entities: Vec<EntityId> = made.iter().map(|t| entity_of(&engine, *t)).collect();
    assert_eq!(entities[0], freed, "z takes the freed id: {entities:?}");
    assert_eq!(entities[1], suppressed, "z2 takes the id w left: {entities:?}");
    assert_eq!(
        engine.allocator_high_water(),
        high_water + 1,
        "two of the three came from the freed ids"
    );
    let z = made[0];
    publish_buffered(&engine);
    let unsuppressed = |after: &str| {
        let subset = engine.authorise(&subset_credential()).unwrap();
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        assert!(
            served(&engine, &subset, Q1, None).contains(&made[1].raw()),
            "z2 is served on the id a suppressed entity left, after {after}"
        );
        assert!(
            !served(&engine, &full, "s0", None).contains(&w.raw()),
            "w stays suppressed, after {after}"
        );
        assert_eq!(engine.overlay_depth(), 1, "after {after}");
    };
    unsuppressed("the flush that placed z2");
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("tz".into()),
                vec![freed],
                vec![IncomingContent::new(vec!["z's".to_string()], vec![freed])],
            )],
        )
        .expect("an artifact over z publishes");
    tick(&engine);
    check(&engine, x, z, 502, "the flush that placed z");

    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, x, z, 502, "a restart");
    let subset = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(served(&engine, &subset, Q1, None).contains(&made[1].raw()));
    assert!(!served(&engine, &full, "s0", None).contains(&w.raw()));
    assert_eq!(engine.overlay_depth(), 1, "after a restart");

    // Four more segments, merged with the one that lists z's row.
    engine.set_merge_for_test(true);
    let merges = engine.write_executor_stats().merges;
    for i in 0..4u64 {
        send(
            &engine,
            &format!("more{i}"),
            Q1,
            vec![create("0", (10.0 + i as f64, 20.0))],
        );
        publish_buffered(&engine);
    }
    tick_until(&engine, "a merge", Duration::from_secs(60), || {
        engine.write_executor_stats().merges > merges
    });
    check(&engine, x, z, 502, "a merge");
    tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the merged bundle verifies");

    fold(&engine);
    check(&engine, x, z, 502, "a second fold");
    drop(engine);
    let engine = open(tmp.path(), &root);
    check(&engine, x, z, 502, "a restart after the second fold");
    tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the folded bundle verifies");
}

/// **An id a kept record names is not issued after a restart.** A row buffered while a fold is in
/// flight keeps the log from the fold's publication, so the records naming the freed id's previous
/// holder, among them the deletion a rotation restated, are replayed at the restart. The items
/// created after it are served and join no artifact the id's previous holder was a member of.
#[test]
fn an_id_a_kept_record_names_is_not_issued_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let map = source_to_new_map(&root, "v00000");
    let x = engine.tessera_id_of(EntityId::new(map[&X])).unwrap();
    send(&engine, "x1", "s0", vec![rescore(x, 501)]);
    publish_buffered(&engine);
    let freed = entity_of(&engine, x);
    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);

    // The fold holds after its passes while a row is buffered, and publishes with it unflushed.
    let folds = engine.write_executor_stats().folds;
    engine.set_fold_paused_for_test(true);
    engine.request_fold();
    wait_until("the fold holds", Duration::from_secs(60), || {
        engine.fold_is_holding_for_test()
    });
    let pinned = send(&engine, "pin", "s0", vec![create("0", (5.0, 5.0))]);
    engine.set_fold_paused_for_test(false);
    wait_until("the fold publishes", Duration::from_secs(60), || {
        engine.write_executor_stats().folds > folds
    });

    drop(engine);
    engine = open(tmp.path(), &root);
    publish_buffered(&engine);
    let made = send(
        &engine,
        "new",
        "s0",
        (0..4)
            .map(|i| create("1", (40.0 + f64::from(i), 41.0)))
            .collect(),
    );
    publish_buffered(&engine);
    let entities: Vec<EntityId> = made.iter().map(|t| entity_of(&engine, *t)).collect();
    assert!(
        !entities.contains(&freed),
        "an id whose previous holder the replay restored is not issued: {entities:?}"
    );
    let subset = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let seen = served(&engine, &subset, "s0", None);
    assert!(made.iter().all(|t| seen.contains(&t.raw())), "every new item is served");
    assert!(served(&engine, &full, "s0", None).contains(&pinned[0].raw()));
    assert_eq!(
        count_in(&engine, "s0", "t0"),
        Some(N),
        "no new item joins the artifact"
    );
}

/// **A suppressed item's old entities stay unsuppressed after a restart that replays their
/// suppression.** A row buffered while the fold is in flight keeps the log from before the fold's
/// publication, so the restart replays the suppression the edits carried to each entity; the item
/// ends the replay suppressed through the entity it holds and through none the fold removed.
#[test]
fn a_moved_entitys_suppression_stays_withdrawn_after_a_restart_replays_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&W]);
    let w = engine.tessera_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    publish_buffered(&engine);
    let middle = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    publish_buffered(&engine);
    let holds = entity_of(&engine, w);

    let folds = engine.write_executor_stats().folds;
    engine.set_fold_paused_for_test(true);
    engine.request_fold();
    wait_until("the fold holds", Duration::from_secs(60), || {
        engine.fold_is_holding_for_test()
    });
    send(&engine, "pin", "s0", vec![create("0", (5.0, 5.0))]);
    engine.set_fold_paused_for_test(false);
    wait_until("the fold publishes", Duration::from_secs(60), || {
        engine.write_executor_stats().folds > folds
    });

    let check = |engine: &Engine, after: &str| {
        let overlay = Arc::clone(&engine.generation().overlay);
        assert!(
            !overlay.is_suppressed(number) && !overlay.is_suppressed(middle),
            "the entities the fold removed hold no suppression, after {after}"
        );
        assert!(overlay.is_suppressed(holds), "w's own entity does, after {after}");
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        assert!(
            !served(engine, &full, "s0", None).contains(&w.raw()),
            "w stays suppressed, after {after}"
        );
    };
    check(&engine, "the fold");
    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, "a restart");
    publish_buffered(&engine);
    fold(&engine);
    check(&engine, "a second fold");
}

/// **A fold discarded after it logged the suppressions leaving with its entities hides nothing
/// less.** The fold is parked before its `CURRENT` flip, with the unsuppression of the suppressed
/// item's old entities already in the log, and the flip then fails. Those entities are still
/// deleted, before and after a restart that replays the unsuppression over rows the discarded fold
/// never removed, so the item stays hidden; the next fold completes and leaves the item suppressed
/// through the entity it holds.
#[test]
fn a_fold_discarded_after_logging_the_unsuppression_hides_nothing_less() {
    use tessera_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_engine::EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            ..config_uncapped()
        },
    )
    .expect("the engine opens");
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(8, Arc::clone(&faults))
        .expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&W]);
    let w = engine.tessera_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    publish_buffered(&engine);
    let middle = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    publish_buffered(&engine);
    let holds = entity_of(&engine, w);

    let failures = engine.write_executor_stats().fold_failures;
    faults.arm_pause(PauseSite::BeforeCurrentFlip, PauseAction::Stall);
    engine.request_fold();
    faults.await_arrivals(PauseSite::BeforeCurrentFlip, 1, Duration::from_secs(60));
    // A directory where the flip writes its temporary file fails the flip.
    let blocker = root.join("CURRENT.tmp");
    std::fs::create_dir(&blocker).unwrap();
    faults.release();
    wait_until("the fold is discarded", Duration::from_secs(60), || {
        engine.write_executor_stats().fold_failures > failures
    });
    std::fs::remove_dir(&blocker).unwrap();

    let hidden = |engine: &Engine, after: &str| {
        let overlay = Arc::clone(&engine.generation().overlay);
        assert!(
            overlay.is_deleted(number) && overlay.is_deleted(middle),
            "the old entities stay deleted, after {after}"
        );
        assert!(overlay.is_suppressed(holds), "w's own entity is suppressed, after {after}");
        for credential in [full_coverage_credential(), subset_credential()] {
            let session = engine.authorise(&credential).unwrap();
            assert!(
                !served(engine, &session, "s0", None).contains(&w.raw()),
                "w stays hidden, after {after}"
            );
            assert!(engine.item(&session, w).unwrap().is_none(), "w has no card, after {after}");
        }
    };
    hidden(&engine, "the discarded fold");
    drop(engine);
    let engine = open(tmp.path(), &root);
    hidden(&engine, "a restart");
    let overlay = Arc::clone(&engine.generation().overlay);
    assert!(
        !overlay.is_suppressed(number) && !overlay.is_suppressed(middle),
        "the restart replayed the unsuppression the discarded fold logged"
    );

    fold(&engine);
    let overlay = Arc::clone(&engine.generation().overlay);
    assert!(
        !overlay.touches(number) && !overlay.touches(middle),
        "the next fold removes the old entities and their deny records"
    );
    assert!(overlay.is_suppressed(holds));
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(!served(&engine, &full, "s0", None).contains(&w.raw()), "w stays suppressed");
}

/// **An edit's new entity deleted before its flush is freed without its suppression.** A
/// suppressed item is edited, and deleted before a flush places the entity the edit gave it. The
/// fold removes both entities and drops both suppressions; it frees the new entity, which is no
/// item's number, and keeps the item's number reserved. The item that takes the freed id is served.
#[test]
fn a_new_entity_deleted_before_its_flush_is_freed_without_its_suppression() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&W]);
    let w = engine.tessera_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    let unflushed = entity_of(&engine, w);
    engine.accept_change(unflushed, ChangeOp::Delete).unwrap();
    assert_eq!(engine.overlay_depth(), 2, "both entities deleted and suppressed");

    let high_water = engine.allocator_high_water();
    fold(&engine);
    publish_buffered(&engine);
    assert_eq!(
        engine.overlay_depth(),
        0,
        "the fold removes both entities and every deny record naming them"
    );
    let made = send(
        &engine,
        "new",
        Q1,
        vec![create("1", (901.0, 902.0)), create("1", (903.0, 904.0))],
    );
    let entities: Vec<EntityId> = made.iter().map(|t| entity_of(&engine, *t)).collect();
    assert!(entities.contains(&unflushed), "a new item takes the freed id: {entities:?}");
    assert!(!entities.contains(&number), "no item takes w's number: {entities:?}");
    assert_eq!(engine.allocator_high_water(), high_water + 1);
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let seen = served(&engine, &subset, Q1, None);
    assert!(
        made.iter().all(|t| seen.contains(&t.raw())),
        "the item on the freed id is served"
    );
    assert!(engine.resolve_tessera_ids(&[w]).unwrap()[0].is_none(), "w names nothing");
}

/// **The pair of an edit's new entity deleted before its flush survives a restart.** The restart
/// restores the pair from the edit's record, so the fold after it still tells the entity from an
/// item's number and frees it without its suppression.
#[test]
fn a_new_entity_deleted_before_its_flush_is_freed_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&W]);
    let w = engine.tessera_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    let unflushed = entity_of(&engine, w);
    engine.accept_change(unflushed, ChangeOp::Delete).unwrap();
    drop(engine);

    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    fold(&engine);
    publish_buffered(&engine);
    assert_eq!(engine.overlay_depth(), 0, "the fold removes both entities and their denies");
    let made = send(&engine, "new", Q1, vec![create("1", (901.0, 902.0))]);
    assert_eq!(entity_of(&engine, made[0]), unflushed, "the new item takes the freed id");
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    assert!(served(&engine, &subset, Q1, None).contains(&made[0].raw()));
}

/// **An entity edited away before its flush is freed without its suppression.** A suppressed item
/// is edited twice with no flush between, so its middle entity never has a row. The fold frees the
/// middle entity and drops its suppression, and the item stays suppressed through the entity it
/// holds.
#[test]
fn an_entity_edited_away_before_its_flush_is_freed_without_its_suppression() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&W]);
    let w = engine.tessera_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    let middle = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    let holds = entity_of(&engine, w);
    publish_buffered(&engine);

    fold(&engine);
    publish_buffered(&engine);
    let overlay = Arc::clone(&engine.generation().overlay);
    assert!(!overlay.touches(number) && !overlay.touches(middle));
    assert!(overlay.is_suppressed(holds), "w stays suppressed through the entity it holds");
    let made = send(&engine, "new", Q1, vec![create("1", (901.0, 902.0))]);
    assert_eq!(entity_of(&engine, made[0]), middle, "the new item takes the middle entity's id");
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(served(&engine, &subset, Q1, None).contains(&made[0].raw()), "z is served");
    assert!(!served(&engine, &full, "s0", None).contains(&w.raw()), "w stays hidden");
}

/// **An edit resolved to an entity before it was freed does not reach the item that took it.**
/// The edit is held after its handler resolved the item to its entity; the item moves on, a fold
/// frees the entity, and a new item in the same views takes it. Released, the edit is decided
/// again against the item where it now is, and the new item is untouched.
#[test]
fn an_edit_resolved_before_its_entity_was_freed_does_not_reach_the_new_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = Arc::new(open(tmp.path(), &root));
    engine.set_merge_for_test(false);
    // An item in `s0` alone, as the new one will be, and holding no unique value, which would name
    // it where it has moved.
    let x = send(
        &engine,
        "x",
        "s0",
        vec![IngestRow {
            ..create("0", (700.0, 701.0))
        }],
    )[0];
    publish_buffered(&engine);
    send(&engine, "x1", "s0", vec![rescore(x, 501)]);
    publish_buffered(&engine);
    let resolved = entity_of(&engine, x);

    engine.hold_next_write_check_for_test();
    let held = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            let mut body_hash = [0u8; 32];
            body_hash[..4].copy_from_slice(b"late");
            engine.ingest(IngestRequest {
                batch_id: "late".into(),
                body_hash,
                view: Some("s0".into()),
                rows: vec![rescore(x, 777)],
                artifacts: Default::default(),
                strict: false,
                tessera_id_column: false,
            })
        })
    };
    wait_until("the edit holds", Duration::from_secs(30), || {
        engine.write_check_is_holding_for_test()
    });

    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);
    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create("1", (901.0, 902.0))])[0];
    assert_eq!(entity_of(&engine, z), resolved, "the new item takes the freed id");
    publish_buffered(&engine);

    engine.release_write_check_for_test();
    let answered = held.join().unwrap();
    assert!(
        matches!(answered, Ok(_) | Err(AcceptError::Conflict(_))),
        "the late edit is decided again or refused: {answered:?}"
    );
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let card = engine.item(&subset, z).unwrap().expect("z is served");
    assert!(card.fields.is_empty(), "z carries {:?}", card.fields);
    assert_eq!(entity_of(&engine, z), resolved, "z keeps its entity");
    let x_card = engine.item(&full, x).unwrap().expect("x is served");
    let score = x_card
        .fields
        .iter()
        .find(|f| f.name == "score")
        .map(|f| f.value.clone());
    let expected = if answered.is_ok() { 777 } else { 502 };
    assert_eq!(score, Some(ScalarOut::I32(expected)), "x holds its own score");
}

/// **The freed ids survive a restart.** They are recorded in the fold's side-manifest, and one an
/// item took after the manifest was written, recorded only in the log, is not issued again.
#[test]
fn freed_ids_are_restored_at_open_less_those_issued_since() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let map = source_to_new_map(&root, "v00000");

    // Two items, each edited twice: two middle entities, both freed by the fold.
    let mut freed = BTreeSet::new();
    for source in [X, W] {
        let tid = engine.tessera_id_of(EntityId::new(map[&source])).unwrap();
        send(&engine, &format!("{source}-1"), "s0", vec![rescore(tid, 1)]);
        publish_buffered(&engine);
        freed.insert(entity_of(&engine, tid));
        send(&engine, &format!("{source}-2"), "s0", vec![rescore(tid, 2)]);
        publish_buffered(&engine);
    }
    fold(&engine);
    publish_buffered(&engine);

    // A restart with nothing issued since restores both.
    drop(engine);
    engine = open(tmp.path(), &root);
    let high_water = engine.allocator_high_water();

    // One is taken, and the log alone records it: no flush follows.
    let first = send(&engine, "a", "s0", vec![create("1", (1.0, 1.0))])[0];
    let taken = entity_of(&engine, first);
    assert!(freed.contains(&taken), "the first new item takes a freed id");
    drop(engine);
    engine = open(tmp.path(), &root);
    assert_eq!(entity_of(&engine, first), taken, "the item keeps it across the restart");

    // The next takes the other freed id, never the one already taken, and then the high-water.
    let next = send(
        &engine,
        "b",
        "s0",
        vec![create("1", (2.0, 2.0)), create("1", (3.0, 3.0))],
    );
    let next: BTreeSet<EntityId> = next.iter().map(|t| entity_of(&engine, *t)).collect();
    let other = *freed.iter().find(|e| **e != taken).unwrap();
    assert_eq!(
        next,
        BTreeSet::from([other, EntityId::new(high_water)]),
        "the other freed id and one from the high-water"
    );
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let seen = served(&engine, &subset, "s0", None);
    assert!(
        [first].iter().all(|t| seen.contains(&t.raw())),
        "the item on a restored id is served"
    );
}
