//! **One item edited, and every home it has to be carried to.** An edit moves the item to a new
//! entity and keeps its `mosaica_id`, so every place the old entity's data lives must be written
//! again for the new one, or read through to it: its rows and positions in every view, every
//! column family, the record blob, its labels, a unique value, its layer
//! memberships, the items a content was generated from, a suppression standing against it, and
//! the group-scoped values and prose of every key it holds.
//!
//! [`Home`] is the list, shared with `deletion_reaches_every_home.rs`, and [`check`] matches it
//! exhaustively, so a home added there does not compile here until an edit is shown to carry it.
//!
//! The edited item is suppressed, edited through one view, shown again, then edited through a
//! group's view with a row carrying no coordinates, and the log is replayed before that edit is
//! flushed. Every home is checked after each step, after a fold and after a restart, through what
//! the engine serves.

mod common;
mod homes;

use std::collections::BTreeSet;
use std::path::Path;

use common::*;
use homes::fixture::*;
use homes::Home;
use mosaica_engine::filter::{Endpoint, FilterOperand, Scalar};
use mosaica_engine::{ColumnBuf, Engine, IngestRequest, ItemOut, ScalarOut, ViewportRequest};
use mosaica_lifecycle::wal::{ChangeOp, WalScalar};
use mosaica_lifecycle::IngestRow;
use mosaica_types::{AttrLocalId, EntityId, MosaicaId};

/// What the edited item should hold, and what it held at the build.
struct Expected {
    tid: MosaicaId,
    /// The entity the build gave it.
    first: EntityId,
    suppressed: bool,
    score: i32,
    heat: [f32; 2],
    /// Its card at the build, before any edit.
    built: ItemOut,
}

fn field(card: &ItemOut, name: &str) -> Option<ScalarOut> {
    card.fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.value.clone())
}

fn scoped_of(card: &ItemOut, family: &str) -> Vec<(String, ScalarOut)> {
    card.scoped
        .iter()
        .find(|s| s.name == family)
        .map(|s| s.values.clone())
        .unwrap_or_default()
}

/// Every home of the edited item, through what the engine serves.
fn check(engine: &Engine, expected: &Expected, after: &str) {
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let subset = engine.authorise(&subset_credential()).unwrap();
    let tid = expected.tid;
    let card = engine.item(&full, tid).unwrap();
    for home in Home::ALL {
        match home {
            // Readable whether or not the item may be seen.
            Home::EditedItems => {
                let entity = engine.resolve_mosaica_ids(&[tid]).unwrap()[0].unwrap_or_else(|| {
                    panic!("Home::EditedItems: the mosaica_id names nothing, after {after}")
                });
                assert_eq!(
                    engine.mosaica_id_of(entity).unwrap(),
                    tid,
                    "Home::EditedItems: the entity it names answers another mosaica_id, after {after}"
                );
            }
            Home::Suppression => {
                if expected.suppressed {
                    assert!(
                        card.is_none(),
                        "Home::Suppression: a suppressed item has a card, after {after}"
                    );
                    for view in VIEWS {
                        assert!(
                            !served(engine, &full, view, None).contains(&tid.raw()),
                            "Home::Suppression: {view} serves a suppressed item, after {after}"
                        );
                    }
                } else {
                    assert!(
                        card.is_some(),
                        "Home::Suppression: a shown item has no card, after {after}"
                    );
                }
            }
            // No other home is observable while the suppression stands.
            _ if expected.suppressed => {}
            Home::Row => {
                let card = card.as_ref().unwrap();
                assert_eq!(
                    card.views, expected.built.views,
                    "Home::Row: the item's views and positions, after {after}"
                );
                for view in VIEWS {
                    assert!(
                        served(engine, &full, view, None).contains(&tid.raw()),
                        "Home::Row: {view} does not serve the item, after {after}"
                    );
                }
            }
            Home::RenderColumn | Home::RenderPresence => {
                let out = engine
                    .viewport(&full, ViewportRequest::new("s0", 0, WHOLE, 10_000))
                    .unwrap();
                let at = out
                    .points
                    .mosaica_ids
                    .iter()
                    .position(|t| *t == tid.raw())
                    .expect("s0 serves the item");
                let column = |name: &str| {
                    let i = out.scalar_names.iter().position(|n| n == name).unwrap();
                    &out.points.scalars[i]
                };
                let score = column("score");
                let band = column("band");
                match home {
                    Home::RenderColumn => {
                        let ColumnBuf::U8(bands) = &band.values else {
                            panic!("band renders as u8")
                        };
                        let ColumnBuf::I32(scores) = &score.values else {
                            panic!("score renders as i32")
                        };
                        assert_eq!(
                            (bands[at], scores[at]),
                            (band_code(X), expected.score),
                            "Home::RenderColumn: the rendered row, after {after}"
                        );
                    }
                    _ => assert!(
                        score.is_present(at),
                        "Home::RenderPresence: the rendered score is marked absent, after {after}"
                    ),
                }
            }
            Home::ValueColumn => {
                let exactly = |v: i32| FilterOperand::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Int(i128::from(v)),
                        inclusive: true,
                    }),
                    hi: Some(Endpoint {
                        value: Scalar::Int(i128::from(v)),
                        inclusive: true,
                    }),
                };
                assert_eq!(
                    served(engine, &full, "s0", leaf("score", exactly(expected.score))),
                    BTreeSet::from([tid.raw()]),
                    "Home::ValueColumn: the score column, after {after}"
                );
            }
            Home::CategoryPostings => assert!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf(
                        "band",
                        FilterOperand::Equals(AttrLocalId::new(u32::from(band_code(X))))
                    )
                )
                .contains(&tid.raw()),
                "Home::CategoryPostings: the band postings, after {after}"
            ),
            Home::KeywordDictionary => assert_eq!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf("tag", FilterOperand::TextEquals(tag_of(X)))
                ),
                BTreeSet::from([tid.raw()]),
                "Home::KeywordDictionary: the tag, after {after}"
            ),
            Home::TextIndex => assert_eq!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf(
                        "prose",
                        FilterOperand::Match {
                            query: format!("p{X}q"),
                            minimum: None
                        }
                    )
                ),
                BTreeSet::from([tid.raw()]),
                "Home::TextIndex: the prose's words, after {after}"
            ),
            Home::RecordBlob => {
                let card = card.as_ref().unwrap();
                assert_eq!(
                    (field(card, "note"), field(card, "prose")),
                    (
                        Some(ScalarOut::Utf8(note_of(X))),
                        Some(ScalarOut::Utf8(prose_of(X)))
                    ),
                    "Home::RecordBlob: the stored note and prose, after {after}"
                );
                for name in ["band", "tag", "ident"] {
                    assert_eq!(
                        field(card, name),
                        field(&expected.built, name),
                        "Home::RecordBlob: {name} on the card, after {after}"
                    );
                }
            }
            Home::TermPostings => {
                assert!(
                    served(engine, &full, "s0", None).contains(&tid.raw()),
                    "Home::TermPostings: a principal holding its label cannot see it, after {after}"
                );
                assert!(
                    !served(engine, &subset, "s0", None).contains(&tid.raw()),
                    "Home::TermPostings: a principal without its label sees it, after {after}"
                );
            }
            Home::UniqueIndex => assert_eq!(
                served(
                    engine,
                    &full,
                    "s0",
                    leaf(
                        "ident",
                        FilterOperand::NumIn(vec![Scalar::Int(i128::from(ident_of(X)))])
                    )
                ),
                BTreeSet::from([tid.raw()]),
                "Home::UniqueIndex: the item's unique value, after {after}"
            ),
            Home::Membership => {
                let t0 = artifacts_of(engine, &full_coverage_credential())
                    .into_iter()
                    .find(|a| a.key.as_deref() == Some("t0"))
                    .expect("the artifact is served");
                assert_eq!(
                    t0.masked_count, N,
                    "Home::Membership: the artifact counts every item, after {after}"
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
                    "Home::GeneratingSet: the content generated from the item, after {after}"
                );
            }
            Home::ScopedValue => {
                let card = card.as_ref().unwrap();
                assert_eq!(
                    scoped_of(card, "heat"),
                    vec![
                        ("q1".to_string(), ScalarOut::F32(expected.heat[0])),
                        ("q2".to_string(), ScalarOut::F32(expected.heat[1])),
                    ],
                    "Home::ScopedValue: heat per key, after {after}"
                );
                for (slot, (key, _)) in QUARTERS.iter().enumerate() {
                    let value = f64::from(expected.heat[slot]);
                    let exactly = FilterOperand::Range {
                        lo: Some(Endpoint {
                            value: Scalar::Float(value),
                            inclusive: true,
                        }),
                        hi: Some(Endpoint {
                            value: Scalar::Float(value),
                            inclusive: true,
                        }),
                    };
                    assert!(
                        served(
                            engine,
                            &full,
                            &format!("quarter:{key}"),
                            leaf(&format!("heat@quarter:{key}"), exactly)
                        )
                        .contains(&tid.raw()),
                        "Home::ScopedValue: heat under {key}, after {after}"
                    );
                }
            }
            Home::ScopedProse => {
                for (slot, (key, _)) in QUARTERS.iter().enumerate() {
                    let word = format!("m{slot}n{X}x");
                    assert_eq!(
                        served(
                            engine,
                            &full,
                            &format!("quarter:{key}"),
                            leaf(
                                &format!("memo@quarter:{key}"),
                                FilterOperand::Match {
                                    query: word,
                                    minimum: None
                                }
                            )
                        ),
                        BTreeSet::from([tid.raw()]),
                        "Home::ScopedProse: memo under {key}, after {after}"
                    );
                }
            }
        }
    }
}

fn edit(engine: &Engine, batch: &str, view: &str, row: IngestRow) {
    let receipt = engine
        .ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash: {
                let mut hash = [0u8; 32];
                hash[..batch.len()].copy_from_slice(batch.as_bytes());
                hash
            },
            view: Some(view.to_string()),
            rows: vec![row],
            artifacts: Default::default(),
            strict: false,
            mosaica_id_column: false,
        })
        .expect("the edit is accepted");
    assert_eq!(receipt.edited, 1, "{batch} edits the item: {receipt:?}");
}

/// A row naming the item by its `mosaica_id` and carrying only what `set` gives it.
fn naming(tid: MosaicaId, set: impl FnOnce(&mut IngestRow)) -> IngestRow {
    let mut row = IngestRow {
        mosaica_id: Some(tid),
        labels: None,
        position: None,
        scalars: vec![WalScalar::Null; DECLARED],
        scoped: vec![WalScalar::Null; 2],
        omitted: (0..DECLARED + 2).collect(),
    };
    set(&mut row);
    row
}

#[test]
fn an_edit_carries_every_home() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    publish(&engine, &root);

    let first = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    let tid = engine.mosaica_id_of(first).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let mut expected = Expected {
        tid,
        first,
        suppressed: false,
        score: score_of(X),
        heat: [heat(0, X), heat(1, X)],
        built: engine
            .item(&full, tid)
            .unwrap()
            .expect("the built item has a card"),
    };
    check(&engine, &expected, "the build");

    // Suppressed, then edited through `s0`: the suppression goes with the item.
    engine.accept_change(first, ChangeOp::Suppress).unwrap();
    expected.suppressed = true;
    expected.score = 555;
    edit(
        &engine,
        "score",
        "s0",
        naming(tid, |row| {
            row.scalars[SCORE_AT] = WalScalar::I32(expected.score);
            row.omitted.retain(|at| *at != SCORE_AT);
        }),
    );
    check(&engine, &expected, "an edit of the suppressed item");
    publish_buffered(&engine);
    check(&engine, &expected, "its flush");
    let moved = engine.resolve_mosaica_ids(&[tid]).unwrap()[0].unwrap();
    assert_ne!(moved, expected.first, "the edit gave the item a new entity");
    engine.accept_change(moved, ChangeOp::Unsuppress).unwrap();
    expected.suppressed = false;
    check(&engine, &expected, "the suppression lifted");

    // Edited through a group's view with no coordinates, and the log replayed before the flush.
    expected.heat[0] = 99.5;
    edit(
        &engine,
        "heat",
        "quarter:q1",
        naming(tid, |row| {
            row.scoped[0] = WalScalar::F32(expected.heat[0]);
            row.omitted.retain(|at| *at != DECLARED);
        }),
    );
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    for view in VIEWS {
        assert!(
            !served(&engine, &full, view, None).contains(&tid.raw()),
            "an edited item is placed again by its flush, not before: {view}"
        );
    }
    drop(engine);
    engine = open(tmp.path(), &root);
    publish_buffered(&engine);
    check(&engine, &expected, "a restart before the edit's flush");
    // Two edits, each giving the item an entity with a row in its three views; the first entity
    // is deleted and still on disc until the fold.
    let verified = mosaica_build::verify_deep(&root, &mosaica_build::VerifyOpts::default())
        .expect("the edited items agree with the rows");
    assert_eq!((verified.edited_pairs, verified.edited_rows), (2, 6));

    fold(&engine);
    check(&engine, &expected, "a fold");
    let verified = mosaica_build::verify_deep(&root, &mosaica_build::VerifyOpts::default())
        .expect("the folded edited items agree with the rows");
    assert_eq!((verified.edited_pairs, verified.edited_rows), (1, 3));
    drop(engine);
    let engine = open(tmp.path(), &root);
    check(&engine, &expected, "a restart after the fold");
}

/// **A value scoped to a key goes where the item has a row under that key.** An item in no view of
/// the key is sent its value with its position there, which adds it; a row carrying the value alone
/// is refused, since no row would hold it.
#[test]
fn a_scoped_value_needs_a_row_under_its_key() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    // Held by `q2` alone.
    let item = EntityId::new(source_to_new_map(&root, "v00000")[&20]);
    let tid = engine.mosaica_id_of(item).unwrap();
    let send = |batch: &str, position: Option<(f64, f64)>| {
        let row = naming(tid, |row| {
            row.scoped[0] = WalScalar::F32(12.5);
            row.omitted.retain(|at| *at != DECLARED);
            row.position = position;
        });
        let mut body_hash = [0u8; 32];
        body_hash[..batch.len()].copy_from_slice(batch.as_bytes());
        engine.ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash,
            view: Some("quarter:q1".to_string()),
            rows: vec![row],
            artifacts: Default::default(),
            strict: false,
            mosaica_id_column: false,
        })
    };
    let refused = send("alone", None);
    assert!(
        matches!(refused, Err(mosaica_engine::AcceptError::Contract(_))),
        "a scoped value with no row to hold it is refused: {refused:?}"
    );
    assert_eq!(engine.buffered_items(), 0, "and nothing is written");

    let added = send("placed", Some(position(20))).expect("sent with a position, it is taken");
    assert_eq!(added.added, 1, "{added:?}");
    publish_buffered(&engine);
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    let card = engine
        .item(&full, tid)
        .unwrap()
        .expect("the item has a card");
    assert_eq!(
        scoped_of(&card, "heat"),
        vec![
            ("q1".to_string(), ScalarOut::F32(12.5)),
            ("q2".to_string(), ScalarOut::F32(heat(1, 20))),
        ]
    );
}

/// **A view dropped while an edit waits in the same commit window** does not strand the edit.
/// The edit is committed first, its own row in the dropped view gives way to its row in another
/// view, and the item is served there with what the edit changed, before and after a restart.
#[test]
fn a_view_dropped_behind_an_edit_in_one_window_keeps_the_item() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    let first = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    let tid = engine.mosaica_id_of(first).unwrap();

    engine.set_work_pass_paused_for_test(true);
    let enqueued = engine.work_enqueued_for_test();
    std::thread::scope(|scope| {
        let edited = scope.spawn(|| {
            edit(
                &engine,
                "heat",
                "quarter:q1",
                naming(tid, |row| {
                    row.scoped[0] = WalScalar::F32(99.5);
                    row.omitted.retain(|at| *at != DECLARED);
                }),
            )
        });
        wait_until(
            "the edit is queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued,
        );
        let dropped = scope.spawn(|| {
            engine
                .drop_view("quarter".to_string(), "q1".to_string())
                .expect("the drop is accepted")
        });
        wait_until(
            "the drop is queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued + 1,
        );
        engine.set_work_pass_paused_for_test(false);
        edited.join().unwrap();
        dropped.join().unwrap();
    });
    publish_buffered(&engine);
    for pass in ["the flush", "a restart"] {
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        let card = engine
            .item(&full, tid)
            .unwrap()
            .unwrap_or_else(|| panic!("the edited item has a card after {pass}"));
        let views: Vec<&str> = card.views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(views, ["quarter:q2", "s0"], "after {pass}");
        assert!(
            served(&engine, &full, "s0", None).contains(&tid.raw()),
            "s0 serves the item after {pass}"
        );
        assert_eq!(
            engine.generation().buffer.oldest_wal_pos(),
            None,
            "nothing buffered holds the log after {pass}"
        );
        drop(engine);
        engine = open(tmp.path(), &root);
        publish_buffered(&engine);
    }
}

/// **A growth and a publication naming an item, queued behind an edit of it**, reach the item
/// where the edit moved it: the item joins the artifact grown, and a content generated from it
/// is served, before and after the fold that retires the entity they named.
#[test]
fn a_growth_and_a_publication_queued_behind_an_edit_follow_the_item() {
    use mosaica_lifecycle::membership::IncomingContent;
    use mosaica_lifecycle::{IncomingArtifact, IncomingGrowth};
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    let map = source_to_new_map(&root, "v00000");
    let entity = |s: u64| EntityId::new(map[&s]);
    let tid = engine.mosaica_id_of(entity(X)).unwrap();
    let mut grown = label_layer();
    grown.name = "topics/b".into();
    engine.register_layer(grown).unwrap();
    engine
        .publish_artifacts(
            "topics/b".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("g0".into()),
                [entity(Y)],
                vec![IncomingContent::new(vec!["grown".to_string()], [entity(Y)])],
            )],
        )
        .unwrap();
    tick(&engine);

    engine.set_work_pass_paused_for_test(true);
    let enqueued = engine.work_enqueued_for_test();
    std::thread::scope(|scope| {
        let edited = scope.spawn(|| {
            edit(
                &engine,
                "score",
                "s0",
                naming(tid, |row| {
                    row.scalars[SCORE_AT] = WalScalar::I32(555);
                    row.omitted.retain(|at| *at != SCORE_AT);
                }),
            )
        });
        wait_until(
            "the edit is queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued,
        );
        let growth = scope.spawn(|| {
            engine
                .grow_memberships(
                    "topics/b".into(),
                    0,
                    vec![IncomingGrowth::from_entities("g0".into(), [entity(X)])],
                )
                .expect("the growth is accepted")
        });
        let publication = scope.spawn(|| {
            engine
                .publish_artifacts(
                    "topics/b".into(),
                    0,
                    vec![IncomingArtifact::with_content(
                        Some("g1".into()),
                        [entity(X), entity(Y)],
                        vec![IncomingContent::new(
                            vec!["published".to_string()],
                            [entity(X)],
                        )],
                    )],
                )
                .expect("the publication is accepted")
        });
        wait_until(
            "both are queued",
            std::time::Duration::from_secs(30),
            || engine.work_enqueued_for_test() > enqueued + 2,
        );
        engine.set_work_pass_paused_for_test(false);
        edited.join().unwrap();
        growth.join().unwrap();
        publication.join().unwrap();
    });
    publish_buffered(&engine);
    assert_ne!(
        engine.resolve_mosaica_ids(&[tid]).unwrap()[0],
        Some(entity(X)),
        "the edit moved the item"
    );
    for after in ["the flush", "the fold"] {
        let served = artifacts_of(&engine, &full_coverage_credential());
        let artifact = |key: &str| {
            served
                .iter()
                .find(|a| a.key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("{key} is served after {after}"))
        };
        assert_eq!(
            artifact("g0").masked_count,
            2,
            "the grown artifact holds the item after {after}"
        );
        assert_eq!(
            artifact("g1").masked_count,
            2,
            "the published artifact holds it after {after}"
        );
        assert_eq!(
            artifact("g1").content,
            vec!["published".to_string()],
            "the content generated from the item is served after {after}"
        );
        fold(&engine);
    }
}

/// Every file under `from`, copied to `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The newest side manifest of `root`'s partition, and its path.
fn side_manifest(root: &Path) -> (std::path::PathBuf, serde_json::Value) {
    let partition = root.join(current_prefix(root)).join("partitions/default");
    let newest = std::fs::read_dir(&partition)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let n: u64 = name
                .strip_prefix("SEGMENTS-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            Some(n)
        })
        .max()
        .unwrap();
    let path = partition.join(format!("SEGMENTS-{newest}.json"));
    let manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    (path, manifest)
}

/// **A bundle whose edited items disagree with themselves or with its rows is refused.** An
/// edited item's flushed rows list their entities and the map holds its pairs both ways; a map
/// missing one direction, a segment whose moved rows are not recorded, and a listed file that is
/// missing are each refused, by `verify --deep` or at open.
#[test]
fn verify_refuses_edited_items_that_disagree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    let tid = engine
        .mosaica_id_of(EntityId::new(source_to_new_map(&root, "v00000")[&X]))
        .unwrap();
    edit(
        &engine,
        "score",
        "s0",
        naming(tid, |row| {
            row.scalars[SCORE_AT] = WalScalar::I32(555);
            row.omitted.retain(|at| *at != SCORE_AT);
        }),
    );
    publish_buffered(&engine);
    drop(engine);
    let verify =
        |root: &Path| mosaica_build::verify_deep(root, &mosaica_build::VerifyOpts::default());
    let verified = verify(&root).expect("the edited bundle verifies");
    assert_eq!((verified.edited_pairs, verified.edited_rows), (1, 3));

    // One direction of the map dropped.
    let one_way = tmp.path().join("one-way");
    copy_tree(&root, &one_way);
    let (path, mut manifest) = side_manifest(&one_way);
    manifest["edited_items"]["by_entity"]["live"] = serde_json::json!([]);
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(verify(&one_way).is_err(), "a map held one way is refused");

    // The moved rows' entities no longer recorded.
    let unrecorded = tmp.path().join("unrecorded");
    copy_tree(&root, &unrecorded);
    let (path, mut manifest) = side_manifest(&unrecorded);
    let files = manifest["files"].as_object_mut().unwrap();
    let listed: Vec<String> = files
        .keys()
        .filter(|rel| rel.ends_with(mosaica_store::edited::EDITED_ROWS_FILE))
        .cloned()
        .collect();
    assert_eq!(listed.len(), 3, "one list per view the item holds a row in");
    for rel in &listed {
        files.remove(rel);
    }
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(
        verify(&unrecorded).is_err(),
        "rows whose entity is not recorded are refused"
    );

    // A listed file that is missing.
    let missing = tmp.path().join("missing");
    copy_tree(&root, &missing);
    std::fs::remove_file(missing.join(current_prefix(&missing)).join(&listed[0])).unwrap();
    assert!(
        mosaica_store::read::open_bundle(&missing).is_err(),
        "a listed edited-rows file that is missing refuses the open"
    );
}

/// **An item edited twice, the first edit flushed and the second buffered, is found where the
/// second put it after a restart.** A growth holds the log, so the restart replays both edits;
/// the first edit's entity has its row and its pair in a run, and replay keeps no pair for it.
/// The item is served with the second edit's value under its one `mosaica_id`, a change addressed
/// by that `mosaica_id` reaches the second edit's entity, and neither earlier entity answers it.
#[test]
fn an_item_edited_twice_across_a_flush_is_found_at_its_last_entity_after_a_restart() {
    use mosaica_lifecycle::{IncomingArtifact, IncomingGrowth};
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    let map = source_to_new_map(&root, "v00000");
    let entity = |s: u64| EntityId::new(map[&s]);
    let a = entity(X);
    let tid = engine.mosaica_id_of(a).unwrap();

    let mut pinning = label_layer();
    pinning.name = "topics/b".into();
    engine.register_layer(pinning).unwrap();
    engine
        .publish_artifacts(
            "topics/b".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("g0".into()),
                [entity(Y)],
            )],
        )
        .unwrap();
    tick(&engine);
    engine
        .grow_memberships(
            "topics/b".into(),
            0,
            vec![IncomingGrowth::from_entities("g0".into(), [entity(0)])],
        )
        .unwrap();

    let set_score = |score: i32| {
        naming(tid, move |row| {
            row.scalars[SCORE_AT] = WalScalar::I32(score);
            row.omitted.retain(|at| *at != SCORE_AT);
        })
    };
    edit(&engine, "to-b", "s0", set_score(555));
    publish_buffered(&engine);
    let b = engine.resolve_mosaica_ids(&[tid]).unwrap()[0].unwrap();
    edit(&engine, "to-c", "s0", set_score(777));
    let c = engine.resolve_mosaica_ids(&[tid]).unwrap()[0].unwrap();
    assert!(
        a != b && b != c && a != c,
        "each edit gave the item a new entity"
    );
    drop(engine);

    let wal = mosaica_lifecycle::Wal::open(tmp.path().join("wal.log")).unwrap();
    let edits = wal
        .records()
        .map(|r| r.unwrap().1)
        .filter(|r| matches!(r, mosaica_lifecycle::WalRecord::IngestBatch { edits, .. } if !edits.is_empty()))
        .count();
    assert_eq!(edits, 2, "the growth held both edits in the log");
    drop(wal);

    engine = open(tmp.path(), &root);
    for pass in ["the restart", "the flush after it"] {
        assert_eq!(
            engine.resolve_mosaica_ids(&[tid]).unwrap()[0],
            Some(c),
            "the mosaica_id names the last entity after {pass}"
        );
        for earlier in [a, b] {
            assert_eq!(
                engine.resolve_mosaica_ids(&[engine.mosaica_id_of(earlier).unwrap()]).unwrap()[0],
                Some(c),
                "an earlier entity's mosaica_id is the item's, and names the last entity after {pass}"
            );
        }
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        if pass == "the flush after it" {
            let card = engine
                .item(&full, tid)
                .unwrap()
                .expect("the item has a card");
            assert_eq!(
                field(&card, "score"),
                Some(ScalarOut::I32(777)),
                "after {pass}"
            );
            assert!(served(&engine, &full, "s0", None).contains(&tid.raw()));
        }
        publish_buffered(&engine);
    }

    let target = engine.resolve_mosaica_ids(&[tid]).unwrap()[0].unwrap();
    engine.accept_change(target, ChangeOp::Suppress).unwrap();
    tick(&engine);
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(
        engine.item(&full, tid).unwrap().is_none(),
        "the suppression addressed by the mosaica_id reached the last entity"
    );
    assert!(!served(&engine, &full, "s0", None).contains(&tid.raw()));
}
