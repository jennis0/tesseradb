//! **An entity id a fold freed carries nothing of the item that held it.** An edit moves an item
//! to a new entity; the fold that removes the old entity's rows frees its id, and the allocator
//! issues it again before any id from the high-water. A deleted item's number is freed by the fold
//! that removes its last entity, one tenancy higher, so the item that takes it has a `mosaica_id`
//! of its own and the deleted item's names nothing. The item that takes an id must hold only what
//! its own rows say: no value, label, unique value, membership or content of the item that held
//! the id before, in any home, through a flush, a merge, a restart and a fold.
//!
//! The freed id is the edited item's middle entity: its first is its number, which stays its
//! item's while the item exists, and its last is the one it holds. A suppressed item's old
//! entities lose their suppression at the fold that removes their rows, while the item stays
//! suppressed through the entity it holds, so the item that takes one is not suppressed. So does
//! an entity an edit made and a deletion removed before any flush placed it. An id a record the log
//! kept names is not issued when the service restarts, since the replay has applied that record to
//! it; nor does an edit resolved to the entity before it was freed reach the item that took it.

mod common;
mod homes;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use homes::fixture::*;
use homes::Home;
use mosaica_engine::filter::{Endpoint, FilterOperand, Scalar};
use mosaica_engine::{
    AcceptError, AddressTable, Engine, IngestReceipt, IngestRequest, ItemOut, NamedItems,
    NamedTargets, ScalarOut, Session,
};
use mosaica_lifecycle::membership::{IncomingArtifact, IncomingContent};
use mosaica_lifecycle::wal::{ChangeOp, WalScalar};
use mosaica_lifecycle::IngestRow;
use mosaica_types::{AttrLocalId, EntityId, MosaicaId};

/// The source of the items the test edits twice: `X` visibly, `W` under a suppression.
const W: u64 = 20;

/// Send `rows` to `view` as the batch `batch`, whose body hash is its name.
fn ingest(
    engine: &Engine,
    batch: &str,
    view: &str,
    rows: Vec<IngestRow>,
) -> Result<IngestReceipt, AcceptError> {
    let mut body_hash = [0u8; 32];
    body_hash[..batch.len()].copy_from_slice(batch.as_bytes());
    engine.ingest(IngestRequest {
        batch_id: batch.to_string(),
        body_hash,
        view: Some(view.to_string()),
        rows,
        artifacts: Default::default(),
        strict: false,
        mosaica_id_column: false,
    })
}

/// [`ingest`], accepted: the `mosaica_id` of each row.
fn send(engine: &Engine, batch: &str, view: &str, rows: Vec<IngestRow>) -> Vec<MosaicaId> {
    ingest(engine, batch, view, rows)
        .unwrap_or_else(|e| panic!("{batch} is accepted: {e}"))
        .mosaica_ids
        .into_iter()
        .map(|id| id.expect("an accepted row has a mosaica_id"))
        .collect()
}

/// A row giving the item `tid` names a new score.
fn rescore(tid: MosaicaId, score: i32) -> IngestRow {
    let mut row = IngestRow {
        mosaica_id: Some(tid),
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
        mosaica_id: None,
        labels: Some(vec![label.as_bytes().to_vec()]),
        position: Some(at),
        scalars: vec![WalScalar::Null; DECLARED],
        scoped: vec![WalScalar::Null; 2],
        omitted: (0..DECLARED + 2).collect(),
    }
}

fn entity_of(engine: &Engine, tid: MosaicaId) -> EntityId {
    items_of(engine, &[tid])[0]
        .unwrap_or_else(|| panic!("{tid:?} names an item"))
}

/// The newest side-manifest's freed ids, free or held, and its retired numbers.
fn pool(root: &Path) -> (croaring::Bitmap, croaring::Bitmap) {
    let bundle = mosaica_store::open_bundle(root).expect("the bundle opens");
    let record = &bundle.only_shard().record;
    let freed = record
        .held_entities
        .iter()
        .filter_map(|held| held.entities.entities())
        .chain(record.free_entities.entities())
        .fold(croaring::Bitmap::new(), |all, ids| all.or(ids));
    let retired = record.retired_numbers.entities().unwrap().clone();
    (freed, retired)
}

fn holds_id(set: &croaring::Bitmap, entity: EntityId) -> bool {
    set.contains(entity.raw() as u32)
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
        mosaica_id: None,
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
        scoped: vec![WalScalar::F32(Z_HEAT), WalScalar::Utf8("memo zmemo".into())],
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
            mosaica_engine::ViewportArtifactsRequest::new(view, 0, WHOLE, usize::MAX),
        )
        .expect("a viewport over the whole map")
        .artifacts()
        .into_iter()
        .find(|a| a.key.as_deref() == Some(key))
        .map(|a| a.masked_count)
}

/// What became of `x`, whose id or number `z` holds.
#[derive(Clone, Copy)]
enum Was {
    /// Edited until its score was this, and served.
    Edited(i32),
    Deleted,
}

/// What `/control/changes` resolves `tid` to.
fn name_one(engine: &Engine, tid: MosaicaId) -> NamedItems {
    engine
        .name_items(&AddressTable {
            rows: 1,
            mosaica_id: Some(vec![Some(tid)]),
            columns: Vec::new(),
        })
        .unwrap()
}

/// `x`'s `mosaica_id` names nothing: no card, no entity, and no item to a write that names it.
fn names_nothing(engine: &Engine, x: MosaicaId, after: &str) {
    for credential in [full_coverage_credential(), subset_credential()] {
        let session = engine.authorise(&credential).unwrap();
        assert!(
            engine.item(&session, x).unwrap().is_none(),
            "x has a card, after {after}"
        );
    }
    assert_eq!(
        items_of(engine, &[x]),
        vec![None],
        "x names an entity, after {after}"
    );
    let named = name_one(engine, x);
    assert!(
        named.verdicts[0].is_err(),
        "a change naming x names an item, after {after}: {:?}",
        named.verdicts[0]
    );
}

/// `z` holds the freed id and carries its own values and nothing of `x`, which held it before, in
/// any home; `x` carries its own, or names nothing where it was deleted. Checked through what each
/// principal is served: `x` carries `0` alone and `z` carries `1` alone, so each principal sees
/// one of them.
fn check(engine: &Engine, x: MosaicaId, z: MosaicaId, was: Was, after: &str) {
    let seen = Seen::new(engine, x, z, was, after);
    for home in Home::ALL {
        match home {
            Home::Row => seen.views(),
            Home::RenderColumn | Home::RenderPresence => seen.rendered_score(*home),
            Home::ValueColumn => seen.names_alone(
                "score",
                exactly(f64::from(seen.x_score)),
                exactly(f64::from(Z_SCORE)),
            ),
            Home::CategoryPostings => seen.band_postings(),
            Home::KeywordDictionary => seen.names_alone(
                "tag",
                FilterOperand::TextEquals(tag_of(X)),
                FilterOperand::TextEquals("tag-z".into()),
            ),
            Home::TextIndex => seen.names_alone(
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
            Home::RecordBlob => seen.record(),
            Home::TermPostings => seen.labels(),
            Home::UniqueIndex => seen.names_alone(
                "ident",
                FilterOperand::NumIn(vec![Scalar::Int(i128::from(ident_of(X)))]),
                FilterOperand::NumIn(vec![Scalar::Int(i128::from(Z_IDENT))]),
            ),
            Home::EditedItems => seen.edited_items(),
            Home::Membership => seen.membership(),
            Home::GeneratingSet => seen.generated_content(),
            // Served to the subset principal at `Home::Row`, so no suppression stands against it.
            Home::Suppression => {}
            Home::ScopedValue => seen.scoped_heat(),
            Home::ScopedProse => seen.names_alone(
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

/// What [`check`] reads after one step: `x` through the principal holding `0`, and `z` and its
/// card through the one holding `1`.
struct Seen<'a> {
    engine: &'a Engine,
    full: Session,
    subset: Session,
    card: ItemOut,
    x: MosaicaId,
    z: MosaicaId,
    /// The score `x` holds, or held before it was deleted.
    x_score: i32,
    /// Whether `x` is served.
    x_live: bool,
    after: &'a str,
}

impl<'a> Seen<'a> {
    fn new(engine: &'a Engine, x: MosaicaId, z: MosaicaId, was: Was, after: &'a str) -> Self {
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        let subset = engine.authorise(&subset_credential()).unwrap();
        let (x_score, x_live) = match was {
            Was::Edited(score) => (score, true),
            Was::Deleted => {
                names_nothing(engine, x, after);
                (score_of(X), false)
            }
        };
        let card = engine
            .item(&subset, z)
            .unwrap()
            .unwrap_or_else(|| panic!("z has a card, after {after}"));
        Seen {
            engine,
            full,
            subset,
            card,
            x,
            z,
            x_score,
            x_live,
            after,
        }
    }

    fn field(&self, name: &str) -> Option<ScalarOut> {
        self.card
            .fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.clone())
    }

    /// A value of `x` in `column` names `x` alone for the principal who sees it and never `z` for
    /// the one who sees `z`, and a value of `z` names `z` alone for the one who sees it.
    fn names_alone(&self, column: &str, of_x: FilterOperand, of_z: FilterOperand) {
        let (engine, after) = (self.engine, self.after);
        let expected = if self.x_live {
            BTreeSet::from([self.x.raw()])
        } else {
            BTreeSet::new()
        };
        assert_eq!(
            served(engine, &self.full, Q1, leaf(column, of_x.clone())),
            expected,
            "{column} of x names x alone, or nothing once x is deleted, in {Q1}, after {after}"
        );
        assert!(
            !served(engine, &self.subset, Q1, leaf(column, of_x)).contains(&self.z.raw()),
            "{column} of x names z in {Q1}, after {after}"
        );
        assert_eq!(
            served(engine, &self.subset, Q1, leaf(column, of_z)),
            BTreeSet::from([self.z.raw()]),
            "{column} of z names z alone in {Q1}, after {after}"
        );
    }

    fn views(&self) {
        assert_eq!(
            self.card
                .views
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            vec![Q1],
            "Home::Row: z's views, after {}",
            self.after
        );
        assert!(served(self.engine, &self.subset, Q1, None).contains(&self.z.raw()));
    }

    fn rendered_score(&self, home: Home) {
        let z = self.z.raw();
        // Served first, so the projection a fold's refresh may be building is built.
        assert!(served(self.engine, &self.subset, Q1, None).contains(&z));
        let out = self
            .engine
            .viewport(
                &self.subset,
                mosaica_engine::ViewportRequest::new(Q1, 0, WHOLE, 10_000),
            )
            .unwrap();
        let at = out
            .points
            .mosaica_ids
            .iter()
            .position(|t| *t == z)
            .expect("q1 serves z");
        let score = out.scalar_names.iter().position(|n| n == "score").unwrap();
        let mosaica_engine::ColumnBuf::I32(scores) = &out.points.scalars[score].values else {
            panic!("score renders as i32")
        };
        assert!(
            out.points.scalars[score].is_present(at) && scores[at] == Z_SCORE,
            "{home:?}: z renders its own score, after {}",
            self.after
        );
    }

    fn band_postings(&self) {
        let (engine, x, z, after) = (self.engine, self.x.raw(), self.z.raw(), self.after);
        let band = |code: u8| {
            leaf(
                "band",
                FilterOperand::Equals(AttrLocalId::new(u32::from(code))),
            )
        };
        assert_eq!(
            served(engine, &self.full, Q1, band(band_code(X))).contains(&x),
            self.x_live,
            "Home::CategoryPostings: x's band names x while it is served, after {after}"
        );
        for session in [&self.full, &self.subset] {
            assert!(
                !served(engine, session, Q1, band(band_code(X))).contains(&z),
                "Home::CategoryPostings: x's band names z, after {after}"
            );
        }
        assert!(
            served(engine, &self.subset, Q1, band(Z_BAND)).contains(&z),
            "Home::CategoryPostings: z's band names z, after {after}"
        );
        assert!(
            !served(engine, &self.full, Q1, band(Z_BAND)).contains(&z),
            "Home::CategoryPostings: z's band shows z without its label, after {after}"
        );
    }

    fn record(&self) {
        assert_eq!(
            (
                self.field("note"),
                self.field("prose"),
                self.field("tag"),
                self.field("band"),
                self.field("ident")
            ),
            (
                Some(ScalarOut::Utf8("note-z".into())),
                Some(ScalarOut::Utf8("zeta pzq".into())),
                Some(ScalarOut::Utf8("tag-z".into())),
                Some(ScalarOut::Utf8("high".into())),
                Some(ScalarOut::U64(Z_IDENT)),
            ),
            "Home::RecordBlob: z's stored values, after {}",
            self.after
        );
    }

    fn labels(&self) {
        let (engine, x, z, after) = (self.engine, self.x.raw(), self.z.raw(), self.after);
        let seen_by_full = served(engine, &self.full, Q1, None);
        let seen_by_subset = served(engine, &self.subset, Q1, None);
        assert!(
            seen_by_full.contains(&x) == self.x_live && !seen_by_full.contains(&z),
            "Home::TermPostings: the full principal sees x while it is served, and not z, after \
             {after}"
        );
        assert!(
            seen_by_subset.contains(&z) && !seen_by_subset.contains(&x),
            "Home::TermPostings: the subset principal sees z and not x, after {after}"
        );
        assert!(
            engine.item(&self.full, self.z).unwrap().is_none(),
            "Home::TermPostings: z has a card without its label, after {after}"
        );
        assert_eq!(
            self.card.labels,
            vec!["1".to_string()],
            "z's labels, after {after}"
        );
    }

    /// `z` was created on the freed id, so the id is its number: the fold that freed it dropped
    /// the edited-items pairs naming the id, or `z` would answer `x`'s id or resolve to an entity
    /// of `x`.
    fn edited_items(&self) {
        let (engine, x, z) = (self.engine, self.x, self.z);
        let entity = entity_of(engine, z);
        assert_eq!(
            engine.mosaica_id_of(entity).unwrap(),
            z,
            "Home::EditedItems: z's entity answers another mosaica_id, after {}",
            self.after
        );
        assert_ne!(x, z, "x and z are one identifier");
        if self.x_live {
            assert_ne!(entity_of(engine, x), entity, "x and z name one entity");
        }
    }

    /// `t0` holds every built item, `x` among them and `W` suppressed: the sixteen `q1` holds and
    /// `W` not one. Its content is generated from `x`, so once `x` is deleted the fold drops the
    /// content and `t0` is withheld. `z` is a member of `tz` alone.
    fn membership(&self) {
        let (engine, after) = (self.engine, self.after);
        assert_eq!(
            (count_in(engine, Q1, "t0"), count_in(engine, Q1, "tz")),
            (
                self.x_live
                    .then_some(QUARTERS[0].1.end - QUARTERS[0].1.start),
                Some(1)
            ),
            "Home::Membership: t0 and tz in q1, after {after}"
        );
        let entity = entity_of(engine, self.z).raw() as u32;
        let holding = engine
            .level_memberships_for_test(LAYER, 0)
            .into_iter()
            .filter(|(_, members, _)| members.contains(&entity))
            .count();
        assert_eq!(
            holding, 1,
            "Home::Membership: z's entity is a member of tz alone, after {after}"
        );
    }

    /// Once `x` is deleted, the content generated from it is dropped, and `z` on its number does
    /// not bring it back for a principal who sees `z` and `Y`.
    fn generated_content(&self) {
        let credential = if self.x_live {
            full_coverage_credential()
        } else {
            both_credential()
        };
        let t0 = artifacts_of(self.engine, &credential)
            .into_iter()
            .find(|a| a.key.as_deref() == Some("t0"))
            .map(|a| a.content);
        assert_eq!(
            t0,
            self.x_live.then(|| vec![CONTENT.to_string()]),
            "Home::GeneratingSet: the content generated from x, after {}",
            self.after
        );
    }

    fn scoped_heat(&self) {
        assert_eq!(
            self.card
                .scoped
                .iter()
                .find(|s| s.name == "heat")
                .map(|s| s.values.clone()),
            Some(vec![("q1".to_string(), ScalarOut::F32(Z_HEAT))]),
            "Home::ScopedValue: z's heat, after {}",
            self.after
        );
        self.names_alone(
            "heat@quarter:q1",
            exactly(f64::from(heat(0, X))),
            exactly(f64::from(Z_HEAT)),
        );
    }
}

/// Publish `tz`, an artifact of [`LAYER`] whose one member and generating set is `z`'s entity.
fn publish_tz(engine: &Engine, z: EntityId) {
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("tz".into()),
                vec![z],
                vec![IncomingContent::new(vec!["z's".to_string()], vec![z])],
            )],
        )
        .expect("an artifact over z publishes");
    tick(engine);
}

/// Merge four new segments with the one that lists `z`'s row, fold, and restart, calling `check`
/// after each. The bundle verifies after the merge and after the restart.
fn through_merge_and_fold(tmp: &Path, root: &Path, engine: Engine, check: impl Fn(&Engine, &str)) {
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
    check(&engine, "a merge");
    mosaica_build::verify_deep(root, &mosaica_build::VerifyOpts::default())
        .expect("the merged bundle verifies");

    fold(&engine);
    check(&engine, "a second fold");
    drop(engine);
    let engine = open(tmp, root);
    check(&engine, "a restart after the second fold");
    mosaica_build::verify_deep(root, &mosaica_build::VerifyOpts::default())
        .expect("the folded bundle verifies");
}

/// Request a fold that holds after its passes, run `during` while it holds, release it, and wait
/// for it to publish.
fn fold_while<T>(engine: &Engine, during: impl FnOnce() -> T) -> T {
    let folds = engine.write_executor_stats().folds;
    engine.set_fold_paused_for_test(true);
    engine.request_fold();
    wait_until("the fold holds", Duration::from_secs(60), || {
        engine.fold_is_holding_for_test()
    });
    let answer = during();
    engine.set_fold_paused_for_test(false);
    wait_until("the fold publishes", Duration::from_secs(60), || {
        engine.write_executor_stats().folds > folds
    });
    answer
}

#[test]
fn a_freed_id_carries_nothing_of_the_item_that_held_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let map = source_to_new_map(&root, "v00000");
    let x = engine.mosaica_id_of(EntityId::new(map[&X])).unwrap();
    let w = engine.mosaica_id_of(EntityId::new(map[&W])).unwrap();

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
    assert_eq!(
        entities[1], suppressed,
        "z2 takes the id w left: {entities:?}"
    );
    assert_eq!(
        engine.allocator_high_water(),
        high_water + 1,
        "two of the three came from the freed ids"
    );
    let z = made[0];
    publish_buffered(&engine);
    let unsuppressed = |engine: &Engine, after: &str| {
        let subset = engine.authorise(&subset_credential()).unwrap();
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        assert!(
            served(engine, &subset, Q1, None).contains(&made[1].raw()),
            "z2 is served on the id a suppressed entity left, after {after}"
        );
        assert!(
            !served(engine, &full, "s0", None).contains(&w.raw()),
            "w stays suppressed, after {after}"
        );
        assert_eq!(engine.overlay_depth(), 1, "after {after}");
    };
    unsuppressed(&engine, "the flush that placed z2");
    publish_tz(&engine, freed);
    check(&engine, x, z, Was::Edited(502), "the flush that placed z");

    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, x, z, Was::Edited(502), "a restart");
    unsuppressed(&engine, "a restart");
    through_merge_and_fold(tmp.path(), &root, engine, |engine, after| {
        check(engine, x, z, Was::Edited(502), after)
    });
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
    let x = engine.mosaica_id_of(EntityId::new(map[&X])).unwrap();
    send(&engine, "x1", "s0", vec![rescore(x, 501)]);
    publish_buffered(&engine);
    let freed = entity_of(&engine, x);
    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);

    // The fold holds after its passes while a row is buffered, and publishes with it unflushed.
    let pinned = fold_while(&engine, || {
        send(&engine, "pin", "s0", vec![create("0", (5.0, 5.0))])
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
    assert!(
        made.iter().all(|t| seen.contains(&t.raw())),
        "every new item is served"
    );
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
    let w = engine.mosaica_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    publish_buffered(&engine);
    let middle = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    publish_buffered(&engine);
    let holds = entity_of(&engine, w);

    fold_while(&engine, || {
        send(&engine, "pin", "s0", vec![create("0", (5.0, 5.0))])
    });

    let check = |engine: &Engine, after: &str| {
        let overlay = Arc::clone(&engine.generation().only_shard().overlay);
        assert!(
            !overlay.is_suppressed(number) && !overlay.is_suppressed(middle),
            "the entities the fold removed hold no suppression, after {after}"
        );
        assert!(
            overlay.is_suppressed(holds),
            "w's own entity does, after {after}"
        );
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
    use mosaica_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        mosaica_engine::EngineConfig {
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
    let w = engine.mosaica_id_of(number).unwrap();
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
        let overlay = Arc::clone(&engine.generation().only_shard().overlay);
        assert!(
            overlay.is_deleted(number) && overlay.is_deleted(middle),
            "the old entities stay deleted, after {after}"
        );
        assert!(
            overlay.is_suppressed(holds),
            "w's own entity is suppressed, after {after}"
        );
        for credential in [full_coverage_credential(), subset_credential()] {
            let session = engine.authorise(&credential).unwrap();
            assert!(
                !served(engine, &session, "s0", None).contains(&w.raw()),
                "w stays hidden, after {after}"
            );
            assert!(
                engine.item(&session, w).unwrap().is_none(),
                "w has no card, after {after}"
            );
        }
    };
    hidden(&engine, "the discarded fold");
    drop(engine);
    let engine = open(tmp.path(), &root);
    hidden(&engine, "a restart");
    let overlay = Arc::clone(&engine.generation().only_shard().overlay);
    assert!(
        !overlay.is_suppressed(number) && !overlay.is_suppressed(middle),
        "the restart replayed the unsuppression the discarded fold logged"
    );

    fold(&engine);
    let overlay = Arc::clone(&engine.generation().only_shard().overlay);
    assert!(
        !overlay.touches(number) && !overlay.touches(middle),
        "the next fold removes the old entities and their deny records"
    );
    assert!(overlay.is_suppressed(holds));
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(
        !served(&engine, &full, "s0", None).contains(&w.raw()),
        "w stays suppressed"
    );
}

/// **An edit's new entity deleted before its flush is freed without its suppression.** A
/// suppressed item is edited, and deleted before a flush places the entity the edit gave it. The
/// fold removes both entities and drops both suppressions; it frees the new entity, which is no
/// item's number, at tenancy 0, and the item's number at tenancy 1, its last entity gone. The items
/// that take them are served, and the deleted item's `mosaica_id` names nothing.
#[test]
fn a_new_entity_deleted_before_its_flush_is_freed_without_its_suppression() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&W]);
    let w = engine.mosaica_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    let unflushed = entity_of(&engine, w);
    engine.accept_change(unflushed, ChangeOp::Delete).unwrap();
    assert_eq!(
        engine.overlay_depth(),
        2,
        "both entities deleted and suppressed"
    );

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
    assert_eq!(
        entities,
        vec![unflushed, number],
        "the first new item takes the id at tenancy 0, the second w's number at tenancy 1"
    );
    assert_eq!(tenancy_of(made[1]), 1);
    assert_ne!(made[1], w, "w's number is issued under another mosaica_id");
    assert_eq!(engine.allocator_high_water(), high_water);
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let seen = served(&engine, &subset, Q1, None);
    assert!(
        made.iter().all(|t| seen.contains(&t.raw())),
        "the item on the freed id is served"
    );
    names_nothing(&engine, w, "its number was issued again");
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
    let w = engine.mosaica_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    let unflushed = entity_of(&engine, w);
    engine.accept_change(unflushed, ChangeOp::Delete).unwrap();
    drop(engine);

    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    fold(&engine);
    publish_buffered(&engine);
    assert_eq!(
        engine.overlay_depth(),
        0,
        "the fold removes both entities and their denies"
    );
    let made = send(&engine, "new", Q1, vec![create("1", (901.0, 902.0))]);
    assert_eq!(
        entity_of(&engine, made[0]),
        unflushed,
        "the new item takes the freed id"
    );
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
    let w = engine.mosaica_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Suppress).unwrap();
    send(&engine, "w1", "s0", vec![rescore(w, 601)]);
    let middle = entity_of(&engine, w);
    send(&engine, "w2", "s0", vec![rescore(w, 602)]);
    let holds = entity_of(&engine, w);
    publish_buffered(&engine);

    fold(&engine);
    publish_buffered(&engine);
    let overlay = Arc::clone(&engine.generation().only_shard().overlay);
    assert!(!overlay.touches(number) && !overlay.touches(middle));
    assert!(
        overlay.is_suppressed(holds),
        "w stays suppressed through the entity it holds"
    );
    let made = send(&engine, "new", Q1, vec![create("1", (901.0, 902.0))]);
    assert_eq!(
        entity_of(&engine, made[0]),
        middle,
        "the new item takes the middle entity's id"
    );
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(
        served(&engine, &subset, Q1, None).contains(&made[0].raw()),
        "z is served"
    );
    assert!(
        !served(&engine, &full, "s0", None).contains(&w.raw()),
        "w stays hidden"
    );
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
        std::thread::spawn(move || ingest(&engine, "late", "s0", vec![rescore(x, 777)]))
    };
    wait_until("the edit holds", Duration::from_secs(30), || {
        engine.write_check_is_holding_for_test()
    });

    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);
    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create("1", (901.0, 902.0))])[0];
    assert_eq!(
        entity_of(&engine, z),
        resolved,
        "the new item takes the freed id"
    );
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
    assert_eq!(
        score,
        Some(ScalarOut::I32(expected)),
        "x holds its own score"
    );
}

/// What `/control/changes` resolves `tid` to: its entity, and the generation it was named in.
fn named(engine: &Engine, tid: MosaicaId) -> (EntityId, NamedTargets) {
    let named = engine
        .name_targets(&AddressTable {
            rows: 1,
            mosaica_id: Some(vec![Some(tid)]),
            columns: Vec::new(),
        })
        .unwrap();
    let Ok(target) = named.verdicts[0] else {
        panic!("{tid:?} names an item: {:?}", named.verdicts[0]);
    };
    (target.target().entity(), named)
}

/// Apply `op` to the item `named` names, as resolved in the generation it was named in.
fn change_named(engine: &Engine, named: &NamedTargets, op: ChangeOp) -> Result<(), AcceptError> {
    let target = named.verdicts[0].clone().expect("the row names an item");
    engine.accept_changes_at(vec![(target, op)], named.at)
}

/// An item in `s0` alone, as a new item will be, edited once and flushed, so the entity it holds
/// is one an edit gave it and a fold frees once it is gone. It holds no unique value, which would
/// name it where it moves.
fn edited_item(engine: &Engine) -> MosaicaId {
    let x = send(engine, "x", "s0", vec![create("0", (700.0, 701.0))])[0];
    publish_buffered(engine);
    send(engine, "x1", "s0", vec![rescore(x, 501)]);
    publish_buffered(engine);
    x
}

/// Whether `tid` is served in `s0` to the principal `credential` authorises.
fn is_served(engine: &Engine, credential: &[u8], tid: MosaicaId) -> bool {
    let session = engine.authorise(credential).unwrap();
    served(engine, &session, "s0", None).contains(&tid.raw())
}

/// **A deletion resolved before its item moved and its entity was freed reaches the item where it
/// is now, and not the item that took the entity.** The deletion is named; an edit moves the item,
/// a fold frees the entity it named, and a new item takes it. Submitted with the generation it was
/// named in, the deletion is accepted: the item is gone and the new item is served, before and
/// after a restart.
#[test]
fn a_deletion_resolved_before_its_entity_was_freed_reaches_the_item_and_not_the_new_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let x = edited_item(&engine);
    let (resolved, named) = named(&engine, x);

    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);
    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create("1", (901.0, 902.0))])[0];
    assert_eq!(
        entity_of(&engine, z),
        resolved,
        "the new item takes the freed id"
    );
    publish_buffered(&engine);

    change_named(&engine, &named, ChangeOp::Delete).expect("the deletion is accepted");
    let check = |engine: &Engine, after: &str| {
        assert!(
            is_served(engine, &subset_credential(), z),
            "z is served, after {after}"
        );
        assert!(
            !is_served(engine, &full_coverage_credential(), x),
            "x is deleted, after {after}"
        );
        names_nothing(engine, x, after);
    };
    check(&engine, "the deletion");
    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, "a restart");
}

/// **A suppression resolved before its item was deleted does not reach the item that took the
/// entity.** No edit follows the suppression's naming: another request deletes the item, a fold
/// frees its entity, and a new item takes it. Submitted with the generation it was named in, the
/// suppression is accepted and reaches nothing, since its item is gone, and the new item is
/// served, before and after a restart.
#[test]
fn a_suppression_resolved_before_its_item_was_deleted_does_not_reach_the_new_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let x = edited_item(&engine);
    let (resolved, named) = named(&engine, x);

    engine.accept_change(resolved, ChangeOp::Delete).unwrap();
    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create("1", (901.0, 902.0))])[0];
    assert_eq!(
        entity_of(&engine, z),
        resolved,
        "the new item takes the freed id"
    );
    publish_buffered(&engine);

    change_named(&engine, &named, ChangeOp::Suppress).expect("the suppression is accepted");
    assert!(is_served(&engine, &subset_credential(), z), "z is served");
    drop(engine);
    engine = open(tmp.path(), &root);
    assert!(
        is_served(&engine, &subset_credential(), z),
        "z is served after a restart"
    );
}

/// **A suppression is accepted however often its item moves while it is on its way.** The
/// suppression is named; then, three times, an edit moves the item and a fold frees the entity it
/// left, so the names the suppression carries are stale each time, and a new item takes the
/// entity it named. Submitted with the generation it was named in, the suppression is accepted
/// and in force for the next request: the item is served to no one and the new item is served,
/// before and after a restart.
#[test]
fn a_suppression_whose_item_keeps_moving_is_accepted_and_in_force_where_the_item_is() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let x = edited_item(&engine);
    let (resolved, named) = named(&engine, x);

    for round in 0..3 {
        send(
            &engine,
            &format!("x{}", round + 2),
            "s0",
            vec![rescore(x, 502 + round)],
        );
        publish_buffered(&engine);
        fold(&engine);
        publish_buffered(&engine);
    }
    let z = send(&engine, "z", "s0", vec![create("1", (901.0, 902.0))])[0];
    assert_eq!(
        entity_of(&engine, z),
        resolved,
        "the new item takes the entity the suppression named"
    );
    publish_buffered(&engine);

    change_named(&engine, &named, ChangeOp::Suppress).expect("the suppression is accepted");
    let check = |engine: &Engine, after: &str| {
        assert!(
            !is_served(engine, &full_coverage_credential(), x),
            "x is suppressed, after {after}"
        );
        let full = engine.authorise(&full_coverage_credential()).unwrap();
        assert!(
            engine.item(&full, x).unwrap().is_none(),
            "x has no card, after {after}"
        );
        assert!(
            is_served(engine, &subset_credential(), z),
            "z is served, after {after}"
        );
    };
    check(&engine, "the suppression");
    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, "a restart");
}

/// **A deletion resolved before an edit moved its item reaches the item where it is now.**
#[test]
fn a_deletion_resolved_before_an_edit_reaches_the_item_where_it_is_now() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let x = edited_item(&engine);
    let (resolved, named) = named(&engine, x);

    send(&engine, "x2", "s0", vec![rescore(x, 502)]);
    publish_buffered(&engine);
    assert_ne!(entity_of(&engine, x), resolved, "the edit moved x");
    change_named(&engine, &named, ChangeOp::Delete).unwrap();
    publish_buffered(&engine);
    assert!(
        !is_served(&engine, &full_coverage_credential(), x),
        "x is deleted"
    );
    names_nothing(&engine, x, "the deletion");
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
        let tid = engine.mosaica_id_of(EntityId::new(map[&source])).unwrap();
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
    assert!(
        freed.contains(&taken),
        "the first new item takes a freed id"
    );
    drop(engine);
    engine = open(tmp.path(), &root);
    assert_eq!(
        entity_of(&engine, first),
        taken,
        "the item keeps it across the restart"
    );

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

/// **A deleted item's number is issued again at the next tenancy and carries nothing.** `x` is
/// deleted, and the fold that removes its rows frees its number at tenancy 1. `z`, the next new
/// item, takes it under a `mosaica_id` of its own. `x`'s `mosaica_id` names nothing on every route,
/// and `z` holds only what its own rows say in every home, through a flush, a restart, a merge and a
/// second fold.
#[test]
fn a_deleted_items_number_is_issued_again_and_carries_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish(&engine, &root);
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    let x = engine.mosaica_id_of(number).unwrap();
    engine.accept_change(number, ChangeOp::Delete).unwrap();
    let high_water = engine.allocator_high_water();
    fold(&engine);
    publish_buffered(&engine);

    let z = send(&engine, "new", Q1, vec![create_z()])[0];
    assert_eq!(entity_of(&engine, z), number, "z takes x's number");
    assert_eq!((tenancy_of(x), tenancy_of(z)), (0, 1));
    assert_eq!(
        engine.allocator_high_water(),
        high_water,
        "z took no id from the high-water"
    );
    publish_buffered(&engine);
    publish_tz(&engine, number);
    check(&engine, x, z, Was::Deleted, "the flush that placed z");

    // A row addressing `x` names no item, and edits nothing.
    let receipt =
        ingest(&engine, "stale", Q1, vec![rescore(x, 777)]).expect("the batch is answered");
    assert_eq!(
        (receipt.mosaica_ids, receipt.refused.len(), receipt.edited),
        (vec![None], 1, 0),
        "a row naming x is refused"
    );
    publish_buffered(&engine);
    check(&engine, x, z, Was::Deleted, "a row naming x");

    drop(engine);
    engine = open(tmp.path(), &root);
    check(&engine, x, z, Was::Deleted, "a restart");
    through_merge_and_fold(tmp.path(), &root, engine, |engine, after| {
        check(engine, x, z, Was::Deleted, after)
    });
}

/// **An item edited, then deleted while a fold is in flight, keeps its number until the fold that
/// removes its last entity.** The fold in flight removes the item's number, which the edit left,
/// and not the entity the item holds, whose deletion came after the fold planned: the number is
/// then neither free nor held. The next fold removes that entity, frees it at tenancy 0 and the
/// number at tenancy 1, and the item that takes the number resolves to itself, not to the entity
/// the edited-items map paired with the number.
#[test]
fn an_item_deleted_while_a_fold_is_in_flight_keeps_its_number_until_the_next_fold() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let x = edited_item(&engine);
    let number = test_key().invert(x).unwrap().1;
    let holds = entity_of(&engine, x);
    assert_ne!(holds, number, "the edit moved x");

    fold_while(&engine, || {
        engine.accept_change(holds, ChangeOp::Delete).unwrap();
    });
    publish_buffered(&engine);
    let (freed, _) = pool(&root);
    assert!(
        !holds_id(&freed, number) && !holds_id(&freed, holds),
        "the number and the entity x holds are neither free nor held"
    );
    names_nothing(&engine, x, "the fold in flight");
    let early = send(
        &engine,
        "a",
        "s0",
        vec![create("1", (901.0, 902.0)), create("1", (903.0, 904.0))],
    );
    let entities: Vec<EntityId> = early.iter().map(|t| entity_of(&engine, *t)).collect();
    assert!(
        !entities.contains(&number) && !entities.contains(&holds),
        "no new item takes x's number or its entity: {entities:?}"
    );
    publish_buffered(&engine);

    fold(&engine);
    publish_buffered(&engine);
    let made = send(
        &engine,
        "b",
        "s0",
        vec![create("1", (905.0, 906.0)), create("1", (907.0, 908.0))],
    );
    let entities: Vec<EntityId> = made.iter().map(|t| entity_of(&engine, *t)).collect();
    assert_eq!(
        entities,
        vec![holds, number],
        "the entity x held is issued at tenancy 0, then x's number at tenancy 1"
    );
    let z = made[1];
    assert_eq!(tenancy_of(z), 1);
    assert_eq!(
        engine.mosaica_id_of(number).unwrap(),
        z,
        "z's number answers z's mosaica_id"
    );
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    let card = engine.item(&subset, z).unwrap().expect("z is served");
    assert!(card.fields.is_empty(), "z carries {:?}", card.fields);
    assert!(served(&engine, &subset, "s0", None).contains(&z.raw()));
    assert_eq!(
        entity_of(&engine, z),
        number,
        "z resolves to its own entity"
    );
    names_nothing(&engine, x, "its number was issued again");
}

/// **An item deleted before its first flush has its number freed by the fold that retires its
/// deletion.** It has no rows, so the fold removes nothing of it but the deletion and frees its
/// number at tenancy 1, which the next new item takes.
#[test]
fn an_item_deleted_before_its_first_flush_has_its_number_freed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let y = send(&engine, "y", "s0", vec![create("0", (700.0, 701.0))])[0];
    let number = entity_of(&engine, y);
    engine.accept_change(number, ChangeOp::Delete).unwrap();
    let high_water = engine.allocator_high_water();
    fold(&engine);
    publish_buffered(&engine);

    let z = send(&engine, "z", "s0", vec![create("1", (901.0, 902.0))])[0];
    assert_eq!(entity_of(&engine, z), number, "z takes y's number");
    assert_eq!(tenancy_of(z), 1);
    assert_eq!(engine.allocator_high_water(), high_water);
    publish_buffered(&engine);
    let subset = engine.authorise(&subset_credential()).unwrap();
    assert!(served(&engine, &subset, "s0", None).contains(&z.raw()));
    let card = engine.item(&subset, z).unwrap().expect("z is served");
    assert!(card.fields.is_empty(), "z carries {:?}", card.fields);
    names_nothing(&engine, y, "its number was issued again");
}

/// **A number at the highest tenancy is retired by the fold that removes it and never issued
/// again.** `x`'s number is put at tenancy 4,095 in the built bundle's index. The fold that removes
/// `x` records the number as retired and frees nothing; no new item takes it, after a restart and a
/// second fold either.
#[test]
fn a_number_at_the_highest_tenancy_is_retired_and_never_issued_again() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let number = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    set_tenancy(&root, number, 4095);
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let x = engine.mosaica_id_of(number).unwrap();
    assert_eq!(tenancy_of(x), 4095);
    engine.accept_change(number, ChangeOp::Delete).unwrap();
    fold(&engine);
    publish_buffered(&engine);

    let check_retired = |engine: &Engine, after: &str| {
        let (freed, retired) = pool(&root);
        assert!(
            holds_id(&retired, number) && !holds_id(&freed, number),
            "x's number is retired and neither free nor held, after {after}"
        );
        names_nothing(engine, x, after);
        let made = send(
            engine,
            &format!("after {after}"),
            "s0",
            (0..3)
                .map(|i| create("1", (40.0 + f64::from(i), 41.0)))
                .collect(),
        );
        let entities: Vec<EntityId> = made.iter().map(|t| entity_of(engine, *t)).collect();
        assert!(
            !entities.contains(&number),
            "a new item takes a retired number, after {after}: {entities:?}"
        );
        publish_buffered(engine);
    };
    check_retired(&engine, "the fold");
    drop(engine);
    engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    check_retired(&engine, "a restart");
    fold(&engine);
    publish_buffered(&engine);
    check_retired(&engine, "a second fold");
    mosaica_build::verify_deep(&root, &mosaica_build::VerifyOpts::default())
        .expect("the bundle verifies");
}

/// **A restart before the log rotates past a freeing fold issues no id twice.** A row buffered
/// while the fold is in flight keeps the deletion of `y` in the log past the fold's publication,
/// so the restart replays it and `y`'s number is neither free nor held. The next fold frees it
/// again, one tenancy higher, and the item that takes it has an identifier no other item has had.
#[test]
fn a_restart_before_the_rotation_issues_no_number_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let mut engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let y = send(&engine, "y", "s0", vec![create("0", (700.0, 701.0))])[0];
    publish_buffered(&engine);
    let number = entity_of(&engine, y);
    engine.accept_change(number, ChangeOp::Delete).unwrap();

    let pinned = fold_while(&engine, || {
        send(&engine, "pin", "s0", vec![create("0", (5.0, 5.0))])[0]
    });
    assert!(
        holds_id(&pool(&root).0, number),
        "the fold holds y's number back"
    );

    drop(engine);
    engine = open(tmp.path(), &root);
    engine.set_merge_for_test(false);
    publish_buffered(&engine);
    let mut issued = vec![y, pinned];
    let made = send(
        &engine,
        "a",
        "s0",
        vec![create("1", (40.0, 41.0)), create("1", (42.0, 41.0))],
    );
    let entities: Vec<EntityId> = made.iter().map(|t| entity_of(&engine, *t)).collect();
    assert!(
        !entities.contains(&number),
        "a number whose deletion the replay restored is not issued: {entities:?}"
    );
    issued.extend(made);
    publish_buffered(&engine);

    fold(&engine);
    publish_buffered(&engine);
    let z = send(&engine, "z", "s0", vec![create("1", (44.0, 41.0))])[0];
    assert_eq!(entity_of(&engine, z), number, "z takes y's number");
    assert_eq!(
        tenancy_of(z),
        2,
        "the fold after the restart frees it one tenancy higher again"
    );
    issued.push(z);
    publish_buffered(&engine);

    assert_eq!(
        issued.iter().collect::<BTreeSet<_>>().len(),
        issued.len(),
        "no identifier is issued twice"
    );
    let entities = items_of(&engine, &issued);
    assert_eq!(entities[0], None, "y names nothing");
    let held: Vec<EntityId> = entities[1..].iter().map(|e| e.expect("an item")).collect();
    assert_eq!(
        held.iter().collect::<BTreeSet<_>>().len(),
        held.len(),
        "no two identifiers name one entity"
    );
    let subset = engine.authorise(&subset_credential()).unwrap();
    assert!(served(&engine, &subset, "s0", None).contains(&z.raw()));
}
