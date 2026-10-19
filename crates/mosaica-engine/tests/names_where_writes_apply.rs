//! A write's names are resolved again where it applies, so what moved while it waited is reached
//! where it is, and a member deleted in between is refused alone.

mod common;
#[path = "homes/fixture.rs"]
mod homes;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use homes::*;
use mosaica_engine::{
    AddressTable, AddressValue, Engine, MemberNames, MemberSet, NamedList, ScalarOut, WriteScope,
};
use mosaica_lifecycle::membership::{IncomingArtifact, IncomingGrowth};
use mosaica_lifecycle::wal::{ChangeOp, WalScalar};
use mosaica_lifecycle::IngestRow;
use mosaica_types::MosaicaId;

/// A row creating an item in `s0`, visible to the principal holding `0`, holding `ident`.
fn create(ident: u64, at: (f64, f64)) -> IngestRow {
    let mut row = bare(None);
    row.labels = Some(vec![b"0".to_vec()]);
    row.position = Some(at);
    row.scalars[IDENT_AT] = WalScalar::U64(ident);
    row.omitted.retain(|at| *at != IDENT_AT);
    row
}

/// A row giving `score` to the item `mosaica_id` names, or to the one holding `ident`.
fn rescore(mosaica_id: Option<MosaicaId>, ident: Option<u64>, score: i32) -> IngestRow {
    let mut row = bare(mosaica_id);
    row.scalars[SCORE_AT] = WalScalar::I32(score);
    row.omitted.retain(|at| *at != SCORE_AT);
    if let Some(ident) = ident {
        row.scalars[IDENT_AT] = WalScalar::U64(ident);
        row.omitted.retain(|at| *at != IDENT_AT);
    }
    row
}

/// The score the item `tid` is served with, to the principal holding `0`.
fn score(engine: &Engine, tid: MosaicaId) -> Option<ScalarOut> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let card = engine.item(&session, tid).unwrap()?;
    card.fields
        .iter()
        .find(|f| f.name == "score")
        .map(|f| f.value.clone())
}

/// A table naming items by `ident`.
fn by_ident(idents: &[u64]) -> AddressTable {
    AddressTable {
        rows: idents.len(),
        mosaica_id: None,
        columns: vec![(
            "ident".to_string(),
            idents
                .iter()
                .map(|i| Some(AddressValue::Integer(i128::from(*i))))
                .collect(),
        )],
    }
}

/// A table naming items by `mosaica_id`.
fn by_id(ids: &[MosaicaId]) -> AddressTable {
    AddressTable {
        rows: ids.len(),
        mosaica_id: Some(ids.iter().map(|id| Some(*id)).collect()),
        columns: Vec::new(),
    }
}

/// The entities the artifact of [`LAYER`] at ordinal `at` holds.
fn members(engine: &Engine, at: usize) -> Vec<u32> {
    let mut level = engine.level_memberships_for_test(LAYER, 0);
    level.sort_by_key(|(ordinal, ..)| *ordinal);
    level[at].1.clone()
}

fn open_homes(tmp: &Path) -> (std::path::PathBuf, Engine) {
    let root = build_homes(tmp);
    let engine = open(tmp, &root);
    engine.set_merge_for_test(false);
    (root, engine)
}

/// **An ingest reaches the items it names by `mosaica_id` and by value where they are when it
/// applies.** The batch is held after its handler resolved it; an edit moves both items, a fold
/// removes the entities they left, and a new item takes a freed entity. Released, the batch is
/// accepted, edits both items where they are and a third nothing moved, and leaves the new item
/// alone, before and after a restart.
#[test]
fn an_ingest_reaches_its_items_where_edits_and_folds_have_moved_them() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, engine) = open_homes(tmp.path());
    let engine = Arc::new(engine);
    let made = send(
        &engine,
        "made",
        "s0",
        vec![
            create(9_101, (701.0, 702.0)),
            create(9_102, (703.0, 704.0)),
            create(9_104, (709.0, 710.0)),
        ],
    );
    let (a, b, c) = (made[0], made[1], made[2]);
    publish_buffered(&engine);
    send(
        &engine,
        "first",
        "s0",
        vec![rescore(Some(a), None, 1), rescore(None, Some(9_102), 2)],
    );
    publish_buffered(&engine);
    let named = [entity_of(&engine, a), entity_of(&engine, b)];

    engine.hold_next_write_check_for_test();
    let held = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            ingest_to(
                &engine,
                "late",
                "s0",
                vec![
                    rescore(Some(a), None, 777),
                    rescore(None, Some(9_102), 888),
                    rescore(Some(c), None, 999),
                ],
            )
        })
    };
    wait_until("the batch holds", Duration::from_secs(30), || {
        engine.write_check_is_holding_for_test()
    });
    send(
        &engine,
        "moved",
        "s0",
        vec![rescore(Some(a), None, 10), rescore(None, Some(9_102), 11)],
    );
    publish_buffered(&engine);
    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create(9_103, (705.0, 706.0))])[0];
    assert!(
        named.contains(&entity_of(&engine, z)),
        "the new item takes an entity the batch named"
    );
    publish_buffered(&engine);
    engine.release_write_check_for_test();

    let receipt = held
        .join()
        .unwrap()
        .expect("the batch is accepted where its items are");
    assert_eq!(receipt.mosaica_ids, vec![Some(a), Some(b), Some(c)]);
    assert_eq!((receipt.edited, receipt.refused.len()), (3, 0));
    publish_buffered(&engine);
    let check = |engine: &Engine, after: &str| {
        assert_eq!(
            score(engine, a),
            Some(ScalarOut::I32(777)),
            "a, after {after}"
        );
        assert_eq!(
            score(engine, b),
            Some(ScalarOut::I32(888)),
            "b, after {after}"
        );
        assert_eq!(
            score(engine, c),
            Some(ScalarOut::I32(999)),
            "c, after {after}"
        );
        assert_eq!(score(engine, z), None, "z holds no score, after {after}");
    };
    check(&engine, "the batch");
    let engine = Arc::into_inner(engine).expect("the batch's thread has ended");
    drop(engine);
    let engine = open(tmp.path(), &root);
    check(&engine, "a restart");
}

/// **A row naming a value nobody held when it was resolved reaches the item given the value
/// before the batch applies.** The batch only addresses items, so the row names no item where it
/// is resolved; held there, a new item takes the value, and the released batch edits it.
#[test]
fn an_ingest_row_by_value_reaches_the_item_given_the_value_before_it_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, engine) = open_homes(tmp.path());
    let engine = Arc::new(engine);

    engine.hold_next_write_check_for_test();
    let held = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            ingest_to(&engine, "late", "s0", vec![rescore(None, Some(9_151), 555)])
        })
    };
    wait_until("the batch holds", Duration::from_secs(30), || {
        engine.write_check_is_holding_for_test()
    });
    let holder = send(&engine, "holder", "s0", vec![create(9_151, (707.0, 708.0))])[0];
    engine.release_write_check_for_test();

    let receipt = held.join().unwrap().expect("the batch is accepted");
    assert_eq!(receipt.mosaica_ids, vec![Some(holder)]);
    assert_eq!((receipt.edited, receipt.refused.len()), (1, 0));
    publish_buffered(&engine);
    assert_eq!(score(&engine, holder), Some(ScalarOut::I32(555)));
    let engine = Arc::into_inner(engine).expect("the batch's thread has ended");
    drop(engine);
    let engine = open(tmp.path(), &root);
    assert_eq!(
        score(&engine, holder),
        Some(ScalarOut::I32(555)),
        "after a restart"
    );
}

/// **A change by value reaches the item holding the value when the change applies.** A
/// suppression names an item by its unique value; before it applies, that item is deleted and the
/// value given to a new item. The suppression is accepted and in force on the new item, before
/// and after a restart.
#[test]
fn a_change_by_value_reaches_the_values_holder_when_it_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, engine) = open_homes(tmp.path());
    let old = send(&engine, "old", "s0", vec![create(9_201, (711.0, 712.0))])[0];
    publish_buffered(&engine);
    let names = engine
        .names(by_ident(&[9_201]), &WriteScope::All, true)
        .unwrap();

    engine
        .accept_change(entity_of(&engine, old), ChangeOp::Delete)
        .unwrap();
    let new = send(&engine, "new", "s0", vec![create(9_201, (713.0, 714.0))])[0];
    publish_buffered(&engine);
    assert!(
        is_served(&engine, &full_coverage_credential(), new),
        "the new holder is served"
    );

    let refused = engine
        .accept_changes_named(names, vec![ChangeOp::Suppress], false)
        .expect("the suppression is accepted");
    assert_eq!(refused, Vec::new(), "the row names the value's holder");
    assert!(
        !is_served(&engine, &full_coverage_credential(), new),
        "the new holder is suppressed"
    );
    drop(engine);
    let engine = open(tmp.path(), &root);
    assert!(
        !is_served(&engine, &full_coverage_credential(), new),
        "the new holder is suppressed after a restart"
    );
}

/// The names of `table`, as one artifact's `members`.
fn member_names(engine: &Engine, table: AddressTable) -> MemberNames {
    let rows = table.rows;
    let names = engine.names(table, &WriteScope::All, false).unwrap();
    let list = NamedList {
        artifact: 0,
        set: MemberSet::Members,
        rows: 0..rows,
    };
    MemberNames::new(names, vec![list], false)
}

fn artifact(key: &str) -> IncomingArtifact {
    IncomingArtifact::from_entities(Some(key.into()), Vec::new())
}

/// **A publication's member named by value reaches the item holding the value when the
/// publication applies.** The holder named is deleted and the value given to a new item before
/// the publication applies; the new item is the member, and nothing is refused, before and after
/// a restart.
#[test]
fn a_publications_member_by_value_reaches_the_values_holder_when_it_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, engine) = open_homes(tmp.path());
    engine.register_layer(label_layer()).unwrap();
    let made = send(
        &engine,
        "made",
        "s0",
        vec![create(9_301, (721.0, 722.0)), create(9_302, (723.0, 724.0))],
    );
    publish_buffered(&engine);
    let names = member_names(&engine, by_ident(&[9_301, 9_302]));

    engine
        .accept_change(entity_of(&engine, made[0]), ChangeOp::Delete)
        .unwrap();
    let new = send(&engine, "new", "s0", vec![create(9_301, (725.0, 726.0))])[0];
    publish_buffered(&engine);

    let published = engine
        .put_artifacts_named(LAYER.into(), 0, vec![artifact("v")], names)
        .expect("the publication is accepted");
    assert_eq!((published.joined, published.refused.len()), (2, 0));
    let mut expected = vec![
        entity_of(&engine, new).raw() as u32,
        entity_of(&engine, made[1]).raw() as u32,
    ];
    expected.sort_unstable();
    assert_eq!(members(&engine, 0), expected, "the value's holder now");
    drop(engine);
    let engine = open(tmp.path(), &root);
    assert_eq!(members(&engine, 0), expected, "after a restart");
}

/// **A growth's member moved by an edit and a fold reaches the item where it is.** The member is
/// named; an edit moves it, a fold removes the entity it left, and a new item takes that entity.
/// The growth joins the item where it is and not the new item, before and after a restart.
#[test]
fn a_growths_member_moved_by_an_edit_and_a_fold_reaches_the_item_where_it_is() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, engine) = open_homes(tmp.path());
    engine.register_layer(label_layer()).unwrap();
    let x = send(&engine, "x", "s0", vec![create(9_501, (741.0, 742.0))])[0];
    publish_buffered(&engine);
    send(&engine, "x1", "s0", vec![rescore(Some(x), None, 1)]);
    publish_buffered(&engine);
    let named_entity = entity_of(&engine, x);
    engine
        .publish_artifacts(LAYER.into(), 0, vec![artifact("g")])
        .unwrap();
    let names = member_names(&engine, by_id(&[x]));

    send(&engine, "x2", "s0", vec![rescore(Some(x), None, 2)]);
    publish_buffered(&engine);
    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create(9_502, (743.0, 744.0))])[0];
    assert_eq!(
        entity_of(&engine, z),
        named_entity,
        "the new item takes the entity the growth named"
    );
    publish_buffered(&engine);

    let grown = engine
        .grow_memberships_named(
            LAYER.into(),
            0,
            vec![IncomingGrowth::from_entities("g".into(), Vec::new())],
            names,
        )
        .expect("the growth is accepted");
    assert_eq!((grown.grown[0].joined, grown.refused.len()), (1, 0));
    let expected = vec![entity_of(&engine, x).raw() as u32];
    assert_eq!(members(&engine, 0), expected, "x where it is, and not z");
    drop(engine);
    let engine = open(tmp.path(), &root);
    assert_eq!(members(&engine, 0), expected, "after a restart");
}
