//! **An entity id a fold freed carries nothing of the item that held it.** An edit moves an item
//! to a new entity; the fold that removes the old entity's rows frees its id, and the allocator
//! issues it again before any id from the high-water. The item that takes it must hold only what
//! its own rows say: no value, label, external id, unique value, membership or content of the
//! item that held the id before, in any home, through a flush, a merge, a restart and a fold.
//!
//! The freed id is the edited item's middle entity: its first is its number, which stays reserved
//! so its `tessera_id` never names another item, and its last is the one it holds. A suppressed
//! entity is never freed, since its suppression stands until it is lifted.

mod common;
mod homes;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use homes::fixture::*;
use homes::Home;
use tessera_engine::filter::{Endpoint, FilterOperand, Scalar};
use tessera_engine::{Engine, IngestRequest};
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
        })
        .unwrap_or_else(|e| panic!("{batch} is accepted: {e}"))
        .tessera_ids
}

/// A row giving the item `tid` names a new score.
fn rescore(tid: TesseraId, score: i32) -> IngestRow {
    let mut row = IngestRow {
        tessera_id: Some(tid),
        external_id: None,
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
fn create(external_id: &str, label: &str, at: (f64, f64)) -> IngestRow {
    IngestRow {
        tessera_id: None,
        external_id: Some(external_id.as_bytes().to_vec()),
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

fn exactly(v: i128) -> FilterOperand {
    FilterOperand::Range {
        lo: Some(Endpoint {
            value: Scalar::Int(v),
            inclusive: true,
        }),
        hi: Some(Endpoint {
            value: Scalar::Int(v),
            inclusive: true,
        }),
    }
}

/// `z` holds the freed id and carries nothing of `x`, which held it before, in any home. `x`, `z`
/// and the artifact are checked through what each principal is served: `x` carries `0` alone and
/// `z` carries `1` alone, so each principal sees one of them.
fn check(engine: &Engine, x: TesseraId, z: TesseraId, x_score: i32, after: &str) {
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();
    let card = engine
        .item(&subset, z)
        .unwrap()
        .unwrap_or_else(|| panic!("z has a card, after {after}"));
    // A value of `x` names `x` for the principal who sees it, and never `z` for the one who sees
    // `z`.
    let names_x_alone = |column: &str, operand: FilterOperand| {
        assert_eq!(
            served(engine, &full, "s0", leaf(column, operand.clone())),
            BTreeSet::from([x.raw()]),
            "{column} of x names x alone, after {after}"
        );
        assert!(
            !served(engine, &subset, "s0", leaf(column, operand)).contains(&z.raw()),
            "{column} of x names z, after {after}"
        );
    };
    for home in Home::ALL {
        match home {
            Home::Row => assert_eq!(
                card.views.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
                vec!["s0"],
                "Home::Row: z's views, after {after}"
            ),
            Home::RenderColumn | Home::RenderPresence => {
                let out = engine
                    .viewport(&subset, tessera_engine::ViewportRequest::new("s0", 0, WHOLE, 10_000))
                    .unwrap();
                let at = out
                    .points
                    .tessera_ids
                    .iter()
                    .position(|t| *t == z.raw())
                    .expect("s0 serves z");
                let score = out.scalar_names.iter().position(|n| n == "score").unwrap();
                assert!(
                    !out.points.scalars[score].is_present(at),
                    "{home:?}: z renders a score, after {after}"
                );
            }
            Home::ValueColumn => names_x_alone("score", exactly(i128::from(x_score))),
            Home::CategoryPostings => {
                let band = FilterOperand::Equals(AttrLocalId::new(u32::from(band_code(X))));
                assert!(
                    !served(engine, &subset, "s0", leaf("band", band)).contains(&z.raw()),
                    "Home::CategoryPostings: x's band names z, after {after}"
                );
            }
            Home::KeywordDictionary => names_x_alone("tag", FilterOperand::TextEquals(tag_of(X))),
            Home::TextIndex => names_x_alone(
                "prose",
                FilterOperand::Match {
                    query: format!("p{X}q"),
                    minimum: None,
                },
            ),
            Home::RecordBlob => assert!(
                card.fields.is_empty(),
                "Home::RecordBlob: z carries {:?}, after {after}",
                card.fields
                    .iter()
                    .map(|f| (&f.name, &f.value))
                    .collect::<Vec<_>>()
            ),
            Home::ExternalIdSidecar => {
                assert_eq!(
                    card.external_id.as_deref(),
                    Some(b"z".as_slice()),
                    "Home::ExternalIdSidecar: z's external id, after {after}"
                );
                assert_eq!(
                    engine
                        .resolve_external_id(&source_id_key(X))
                        .unwrap()
                        .map(|e| engine.tessera_id_of(e).unwrap()),
                    Some(x),
                    "Home::ExternalIdSidecar: x's external id names x, after {after}"
                );
            }
            Home::TermPostings => {
                let seen_by_full = served(engine, &full, "s0", None);
                let seen_by_subset = served(engine, &subset, "s0", None);
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
            Home::UniqueIndex => names_x_alone(
                "ident",
                FilterOperand::NumIn(vec![Scalar::Int(i128::from(ident_of(X)))]),
            ),
            Home::EditedItems => {
                let entity = entity_of(engine, z);
                assert_eq!(
                    engine.tessera_id_of(entity).unwrap(),
                    z,
                    "Home::EditedItems: z's entity answers another tessera_id, after {after}"
                );
                assert_ne!(entity_of(engine, x), entity, "x and z name one entity");
            }
            // `W`, suppressed, is a member the full principal cannot count, and `z` is none.
            Home::Membership => {
                let count = |credential: &[u8]| {
                    artifacts_of(engine, credential)
                        .into_iter()
                        .find(|a| a.key.as_deref() == Some("t0"))
                        .map(|a| a.masked_count)
                };
                assert_eq!(
                    count(&full_coverage_credential()),
                    Some(N - 1),
                    "Home::Membership: the artifact's members, after {after}"
                );
                let subset_members = (0..N).filter(|s| *s != W && subset_sees(*s)).count() as u64;
                assert!(
                    count(&subset_credential()).is_none_or(|c| c == subset_members),
                    "Home::Membership: the subset principal counts z, after {after}"
                );
            }
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
            Home::ScopedValue | Home::ScopedProse => assert!(
                card.scoped.is_empty(),
                "{home:?}: z carries scoped values, after {after}"
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

    // `w` is edited twice under a suppression; its middle entity keeps it and is not freed.
    engine
        .accept_change(EntityId::new(map[&W]), ChangeOp::Suppress)
        .unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    publish_buffered(&engine);
    let suppressed = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    publish_buffered(&engine);

    let high_water = engine.allocator_high_water();
    fold(&engine);
    publish_buffered(&engine);

    // Three new items: one takes the freed id, and none the suppressed one.
    let made = send(
        &engine,
        "new",
        "s0",
        vec![
            create("z", "1", (901.0, 902.0)),
            create("z2", "1", (903.0, 904.0)),
            create("z3", "1", (905.0, 906.0)),
        ],
    );
    let entities: Vec<EntityId> = made.iter().map(|t| entity_of(&engine, *t)).collect();
    assert!(entities.contains(&freed), "a new item takes the freed id: {entities:?}");
    assert!(!entities.contains(&suppressed), "no item takes a suppressed one");
    assert_eq!(
        engine.allocator_high_water(),
        high_water + 2,
        "one of the three came from the freed ids"
    );
    let z = made[entities.iter().position(|e| *e == freed).unwrap()];
    // The new item is buffered, and every value of `x` still names `x` alone.
    publish_buffered(&engine);
    check(&engine, x, z, 502, "the flush that placed z");

    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, x, z, 502, "a restart");

    // Four more segments, merged with the one that lists z's row.
    engine.set_merge_for_test(true);
    let merges = engine.write_executor_stats().merges;
    for i in 0..4u64 {
        send(
            &engine,
            &format!("more{i}"),
            "s0",
            vec![create(&format!("m{i}"), "0", (10.0 + i as f64, 20.0))],
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
    let first = send(&engine, "a", "s0", vec![create("a", "1", (1.0, 1.0))])[0];
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
        vec![create("b", "1", (2.0, 2.0)), create("c", "1", (3.0, 3.0))],
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
