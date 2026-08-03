//! I1 composition tests over a 10k(+5)-entity in-memory-ish fixture.
//!
//! Fixture: an identity permutation over `[0, 10_005)` (rows == entity ids, so row-space
//! assertions can be read directly against entity ids), two granted terms (0, 1) whose postings
//! cover entities `[0, 510)` — the base fragment — and entity ids `[10_000, 10_005)` reserved,
//! unpermutted-in-the-bundle-sense but *do* have rows in this synthetic permutation, standing in
//! for buffered entities. They are exercised against a synthetic permutation even though real
//! buffered items never have a row, so that the branch that would handle one is covered.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, FrozenFragment, PostingsReader};
use tessera_engine::compose::{compose, visible_to, EffectiveMask, RowProjection};
use tessera_lifecycle::{ChangeOp, IngestBuffer, Overlay, PredicateChange};
use tessera_store::write::write_permutation;
use tessera_store::{Permutation, RowSpace};
use tessera_types::{EntityId, TermId};

/// A predicate change over already-resolved term ids. Its descriptors are stand-ins — nothing here
/// resolves them — but they are stated anyway, because `PredicateChange` exists precisely so the
/// two halves cannot be set independently.
fn evaluate(terms: &[u32]) -> PredicateChange {
    PredicateChange {
        descriptors: terms.iter().map(|t| t.to_string().into_bytes()).collect(),
        terms: terms.iter().map(|t| TermId::new(*t)).collect(),
    }
}

const UNIVERSE: u32 = 10_000;
const BUFFER_EXT: u32 = 5;
const BOUND: u64 = (UNIVERSE + BUFFER_EXT) as u64;
const WATERMARK: u64 = UNIVERSE as u64;
const SMALL_TERM_THRESHOLD: u32 = 32;

// Fixture entity ids (documented at point of use below in each test).
const SUPPRESS_IN: u64 = 5;
const EVAL_NARROW: u64 = 6;
const EVAL_KEEP: u64 = 7;
const DELETE_BEATS_EVAL: u64 = 8;
const CROSS1_DSU: u64 = 10; // delete -> suppress -> unsuppress
const CROSS2_SDU: u64 = 11; // suppress -> delete -> unsuppress
const SUPPRESS_OUT: u64 = 9000; // outside the fragment
const EVAL_WIDEN: u64 = 9001; // outside the fragment
const BUFFERED_PASS: u64 = 10_000;
const BUFFERED_FAIL: u64 = 10_001;

const SATISFIED_TERM_A: u32 = 0; // a real granted/postings term
const SATISFIED_TERM_MARKER: u32 = 99; // satisfied but carries no postings — pure evaluate marker
const UNSATISFIED_TERM: u32 = 77;

struct Fixture {
    _temp: TempDir,
    perm: RowSpace,
    postings: PostingsReader,
    satisfied: FxHashSet<TermId>,
    base: Arc<RowProjection>,
    /// Every entity id genuinely inside the base fragment (brute-force, for cross-checks).
    fragment_entities: HashSet<u32>,
}

fn build_fixture() -> Fixture {
    let temp = TempDir::new().unwrap();

    // Term 0: entities [0, 500). Term 1: entities [500, 510). Both granted.
    let per_term: Vec<Vec<u32>> = vec![(0..500).collect(), (500..510).collect()];

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &per_term, SMALL_TERM_THRESHOLD).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();

    let granted: Vec<TermId> = vec![TermId::new(0), TermId::new(1)];
    let cache_dir = temp.path().join("cache");
    let cache = FragmentCache::new(&cache_dir, [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&granted, [3u8; 32], 0, &postings, &[], WATERMARK)
        .unwrap();

    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..BOUND).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, BOUND).unwrap();
    let perm = RowSpace::new(
        Arc::new(Permutation::load(&perm_path).unwrap()),
        BOUND as u32,
    );

    let base = Arc::new(RowProjection::new(&fragment, &perm));

    let mut fragment_entities: HashSet<u32> = HashSet::new();
    for t in per_term {
        fragment_entities.extend(t);
    }

    // `satisfied` includes the granted postings terms plus a marker term used only by
    // `evaluate_terms` entries in the tests below — evaluate never touches postings, so this
    // term need not (and does not) appear in any posting.
    let satisfied: FxHashSet<TermId> = [
        TermId::new(SATISFIED_TERM_A),
        TermId::new(1),
        TermId::new(SATISFIED_TERM_MARKER),
    ]
    .into_iter()
    .collect();

    Fixture {
        _temp: temp,
        perm,
        postings,
        satisfied,
        base,
        fragment_entities,
    }
}

fn e(id: u64) -> EntityId {
    EntityId::new(id)
}

/// Rebuild the same fragment via the cache (cache hit, not a rebuild) — the exact `&FrozenFragment`
/// every test composes against, exposed separately so `visible_to` tests can also get one without
/// threading it through the fixture's lifetime.
fn fragment_for(fx: &Fixture) -> Arc<FrozenFragment> {
    let cache_dir = fx._temp.path().join("cache");
    let cache = FragmentCache::new(&cache_dir, [1u8; 32], [2u8; 32]);
    let granted: Vec<TermId> = vec![TermId::new(0), TermId::new(1)];
    cache
        .get_or_build(&granted, [3u8; 32], 0, &fx.postings, &[], WATERMARK)
        .unwrap()
}

/// Build a `FrozenFragment` handle and compose against `overlay`/`buffer`. Kept as a free
/// function so every test composes through the exact same call.
fn compose_with(fx: &Fixture, overlay: &Overlay, buffer: &IngestBuffer) -> EffectiveMask {
    let fragment = fragment_for(fx);

    compose(
        &fragment,
        &fx.satisfied,
        overlay,
        buffer,
        Arc::clone(&fx.base),
        &fx.perm,
    )
}

fn full_range() -> Range<u32> {
    0..(BOUND as u32)
}

#[test]
fn a_no_overlay_or_buffer_matches_raw_projection() {
    let fx = build_fixture();
    let overlay = Overlay::new();
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);

    assert_eq!(
        mask.count_range(full_range()),
        fx.base.bitmap().cardinality()
    );
    for entity in 0..BOUND as u32 {
        assert_eq!(
            mask.contains_row(entity),
            fx.base.bitmap().contains(entity),
            "entity {entity}"
        );
    }
    assert!(mask.check_structural_invariants());
}

#[test]
fn b_suppress_visible_entity_drops_count_and_visibility() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(SUPPRESS_IN as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Suppress, None);
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);

    assert_eq!(
        mask.count_range(full_range()),
        fx.base.bitmap().cardinality() - 1
    );
    assert!(!mask.contains_row(SUPPRESS_IN as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn c_unsuppress_restores_it() {
    let fx = build_fixture();

    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Suppress, None);
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Unsuppress, None);
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);

    assert_eq!(
        mask.count_range(full_range()),
        fx.base.bitmap().cardinality()
    );
    assert!(mask.contains_row(SUPPRESS_IN as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn d_evaluate_excludes_when_terms_no_longer_intersect() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(EVAL_NARROW as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(
        e(EVAL_NARROW),
        ChangeOp::Predicate,
        Some(evaluate(&[UNSATISFIED_TERM])),
    );
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(!mask.contains_row(EVAL_NARROW as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn d_evaluate_keeps_when_terms_still_intersect() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(EVAL_KEEP as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(
        e(EVAL_KEEP),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(mask.contains_row(EVAL_KEEP as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn d2_evaluate_widening_includes_entity_outside_fragment() {
    let fx = build_fixture();
    assert!(!fx.fragment_entities.contains(&(EVAL_WIDEN as u32)));
    assert!(!fx.base.bitmap().contains(EVAL_WIDEN as u32));

    let mut overlay = Overlay::new();
    overlay.apply(
        e(EVAL_WIDEN),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(mask.contains_row(EVAL_WIDEN as u32));
    assert_eq!(
        mask.count_range(full_range()),
        fx.base.bitmap().cardinality() + 1
    );
    assert!(mask.check_structural_invariants());
}

#[test]
fn e_buffered_entity_included_iff_terms_intersect() {
    let fx = build_fixture();

    let mut buffer = IngestBuffer::new();
    insert_buffered(
        &mut buffer,
        BUFFERED_PASS,
        vec![TermId::new(SATISFIED_TERM_A)],
    );
    insert_buffered(
        &mut buffer,
        BUFFERED_FAIL,
        vec![TermId::new(UNSATISFIED_TERM)],
    );
    let overlay = Overlay::new();

    let mask = compose_with(&fx, &overlay, &buffer);

    assert!(mask.contains_row(BUFFERED_PASS as u32));
    assert!(!mask.contains_row(BUFFERED_FAIL as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn f_deny_beats_evaluate() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(DELETE_BEATS_EVAL as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(e(DELETE_BEATS_EVAL), ChangeOp::Delete, None);
    overlay.apply(
        e(DELETE_BEATS_EVAL),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(!mask.contains_row(DELETE_BEATS_EVAL as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn f2_out_of_fragment_deny_is_a_byte_for_byte_no_op() {
    let fx = build_fixture();
    assert!(!fx.fragment_entities.contains(&(SUPPRESS_OUT as u32)));
    assert!(!fx.base.bitmap().contains(SUPPRESS_OUT as u32));

    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_OUT), ChangeOp::Suppress, None);
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);

    // Every count identical to case (a): the `minus ⊆ base` clamp means denying an entity the
    // fragment never contained changes nothing.
    assert_eq!(
        mask.count_range(full_range()),
        fx.base.bitmap().cardinality()
    );
    for entity in 0..BOUND as u32 {
        assert_eq!(mask.contains_row(entity), fx.base.bitmap().contains(entity));
    }
    assert!(mask.check_structural_invariants());
}

#[test]
fn f3_cross_cause_delete_suppress_unsuppress_stays_excluded() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(CROSS1_DSU as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(e(CROSS1_DSU), ChangeOp::Delete, None);
    overlay.apply(e(CROSS1_DSU), ChangeOp::Suppress, None);
    overlay.apply(e(CROSS1_DSU), ChangeOp::Unsuppress, None);
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(!mask.contains_row(CROSS1_DSU as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn f3_cross_cause_suppress_delete_unsuppress_stays_excluded() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(CROSS2_SDU as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(e(CROSS2_SDU), ChangeOp::Suppress, None);
    overlay.apply(e(CROSS2_SDU), ChangeOp::Delete, None);
    overlay.apply(e(CROSS2_SDU), ChangeOp::Unsuppress, None);
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(!mask.contains_row(CROSS2_SDU as u32));
    assert!(mask.check_structural_invariants());
}

#[test]
fn g_count_range_matches_brute_force_rows_in_range() {
    let fx = build_fixture();

    // A composite scenario exercising every rule at once.
    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Suppress, None);
    overlay.apply(
        e(EVAL_NARROW),
        ChangeOp::Predicate,
        Some(evaluate(&[UNSATISFIED_TERM])),
    );
    overlay.apply(
        e(EVAL_WIDEN),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    overlay.apply(e(CROSS1_DSU), ChangeOp::Delete, None);
    overlay.apply(e(CROSS1_DSU), ChangeOp::Suppress, None);
    overlay.apply(e(CROSS1_DSU), ChangeOp::Unsuppress, None);
    overlay.apply(e(SUPPRESS_OUT), ChangeOp::Suppress, None);

    let mut buffer = IngestBuffer::new();
    insert_buffered(
        &mut buffer,
        BUFFERED_PASS,
        vec![TermId::new(SATISFIED_TERM_A)],
    );
    insert_buffered(
        &mut buffer,
        BUFFERED_FAIL,
        vec![TermId::new(UNSATISFIED_TERM)],
    );

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(mask.check_structural_invariants());

    let mut rng = StdRng::seed_from_u64(42);
    for _ in 0..200 {
        let a = rng.gen_range(0..BOUND as u32);
        let b = rng.gen_range(0..BOUND as u32);
        let r = a.min(b)..a.max(b) + 1;

        let expected = mask.rows_in_range(r.clone()).iter().count() as u64;
        assert_eq!(mask.count_range(r.clone()), expected, "range {r:?}");
    }
}

/// The run decode (`for_each_visible_run`) flattens to exactly `rows_in_range`, on **both**
/// routes: the diffs-empty cursor walk of `base`, and the diffs-present fallback through the
/// materialised bitmap. `diffs_are_empty` is the route predicate, so asserting it per mask pins
/// which route each half of this test actually exercised.
#[test]
fn g2_visible_runs_flatten_to_rows_in_range_on_both_routes() {
    let fx = build_fixture();

    // Route 1: no overlay, no buffer — diffs empty, the cursor walks `base` directly.
    let empty_mask = compose_with(&fx, &Overlay::new(), &IngestBuffer::new());
    assert!(empty_mask.diffs_are_empty(), "route predicate: base walk");

    // Route 2: the same composite scenario as `g_...` — non-empty minus AND plus, so the
    // fallback must include `plus` rows the base cursor could never see.
    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Suppress, None);
    overlay.apply(
        e(EVAL_WIDEN),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    let diff_mask = compose_with(&fx, &overlay, &IngestBuffer::new());
    assert!(!diff_mask.diffs_are_empty(), "route predicate: fallback");

    let mut rng = StdRng::seed_from_u64(0xB9);
    for mask in [&empty_mask, &diff_mask] {
        for _ in 0..200 {
            let a = rng.gen_range(0..BOUND as u32);
            let b = rng.gen_range(0..BOUND as u32);
            let r = a.min(b)..a.max(b) + 1;

            let mut flat: Vec<u32> = Vec::new();
            let mut prev_end: u32 = 0;
            mask.for_each_visible_run(r.clone(), |run| {
                assert!(run.start < run.end, "empty run emitted for {r:?}");
                assert!(
                    flat.is_empty() || run.start > prev_end,
                    "runs not ascending/disjoint for {r:?}"
                );
                prev_end = run.end;
                flat.extend(run);
            });
            let expected = mask.rows_in_range(r.clone()).to_vec();
            assert_eq!(flat, expected, "range {r:?}");
        }
    }

    // The plus row is genuinely reachable only through the fallback: prove the scenario keeps
    // exercising the property the fallback exists for.
    let widen_row = EVAL_WIDEN as u32;
    let mut saw_widen = false;
    diff_mask.for_each_visible_run(widen_row..widen_row + 1, |run| {
        saw_widen = saw_widen || (run.start..run.end).contains(&widen_row);
    });
    assert!(
        saw_widen,
        "the widened (plus) row must be yielded by the fallback route"
    );
}

#[test]
fn h_structural_invariants_hold_pervasively() {
    let fx = build_fixture();

    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Suppress, None);
    overlay.apply(e(SUPPRESS_OUT), ChangeOp::Suppress, None);
    overlay.apply(
        e(EVAL_WIDEN),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    overlay.apply(e(CROSS2_SDU), ChangeOp::Suppress, None);
    overlay.apply(e(CROSS2_SDU), ChangeOp::Delete, None);
    overlay.apply(e(CROSS2_SDU), ChangeOp::Unsuppress, None);

    let mut buffer = IngestBuffer::new();
    insert_buffered(
        &mut buffer,
        BUFFERED_PASS,
        vec![TermId::new(SATISFIED_TERM_A)],
    );

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(mask.check_structural_invariants());
}

/// Insert a buffered item directly with already-resolved `terms`, bypassing descriptor
/// resolution (irrelevant to these tests — see [`tessera_lifecycle::IngestBuffer::insert_row_with_terms`]).
fn insert_buffered(buffer: &mut IngestBuffer, entity: u64, terms: Vec<TermId>) {
    use tessera_lifecycle::WalRow;

    let row = WalRow {
        external_id: Some(entity.to_le_bytes().to_vec()),
        entity_id: e(entity),
        slice: "s0".to_string(),
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: Vec::new(),
    };
    buffer.insert_row_with_terms(&row, terms);
}

/// Restart-replay. Two entities are established purely through the WAL (ids
/// `10_002`/`10_003`, inside the synthetic permutation's buffered-with-a-row range), then driven
/// through the two cross-cause sequences that matter — `delete → suppress →
/// unsuppress` and `delete → predicate` (with a term the session satisfies) — all *through the
/// WAL*, not by calling `Overlay::apply` directly. The WAL handle is dropped and reopened (the
/// "restart"), so the `Overlay`/`IngestBuffer` this test composes against are rebuilt from a
/// fresh replay of on-disk bytes, exactly as a real process restart would rebuild them.
#[test]
fn step3_restart_replay_survives_cross_cause_sequences() {
    use tessera_lifecycle::wal::{Wal, WalRecord, WalRow};
    use tessera_lifecycle::{replay, ChangeOp};

    const ENTITY_X: u64 = 10_002;
    const ENTITY_Y: u64 = 10_003;
    let ext_x = b"external-x".to_vec();
    let ext_y = b"external-y".to_vec();

    let wal_dir = TempDir::new().unwrap();
    let wal_path = wal_dir.path().join("wal.log");

    {
        let (mut wal, _initial) = Wal::open(&wal_path).unwrap();
        wal.append(&WalRecord::IngestBatch {
            batch_id: "b0".to_string(),
            body_hash: [0u8; 32],
            rows: vec![
                WalRow {
                    external_id: Some(ext_x.clone()),
                    entity_id: e(ENTITY_X),
                    slice: "s0".to_string(),
                    descriptors: vec![b"term-x".to_vec()],
                    x: 0.0,
                    y: 0.0,
                    scalars: Vec::new(),
                },
                WalRow {
                    external_id: Some(ext_y.clone()),
                    entity_id: e(ENTITY_Y),
                    slice: "s0".to_string(),
                    descriptors: vec![b"term-y".to_vec()],
                    x: 0.0,
                    y: 0.0,
                    scalars: Vec::new(),
                },
            ],
        })
        .unwrap();

        // delete X -> suppress X -> unsuppress X (must stay excluded: delete is terminal).
        wal.append(&WalRecord::Change {
            external_id: ext_x.clone(),
            op: ChangeOp::Delete,
            descriptors: None,
        })
        .unwrap();
        wal.append(&WalRecord::Change {
            external_id: ext_x.clone(),
            op: ChangeOp::Suppress,
            descriptors: None,
        })
        .unwrap();
        wal.append(&WalRecord::Change {
            external_id: ext_x.clone(),
            op: ChangeOp::Unsuppress,
            descriptors: None,
        })
        .unwrap();

        // delete Y -> predicate Y granting a term the session satisfies (must stay excluded:
        // predicate never clears deny flags).
        wal.append(&WalRecord::Change {
            external_id: ext_y.clone(),
            op: ChangeOp::Delete,
            descriptors: None,
        })
        .unwrap();
        wal.append(&WalRecord::Change {
            external_id: ext_y.clone(),
            op: ChangeOp::Predicate,
            descriptors: Some(vec![b"satisfied-term".to_vec()]),
        })
        .unwrap();

        wal.fsync().unwrap();
        // `wal` (and its file handle) drops here — the simulated crash/restart boundary.
    }

    // A one-descriptor dictionary: `b"satisfied-term"` resolves to `TermId(0)`, which is exactly
    // `SATISFIED_TERM_A` in the fixture's `satisfied` set below — so if the deny didn't hold,
    // Y's predicate change would otherwise rescue it.
    let dict_dir = TempDir::new().unwrap();
    let mut dict_writer = tessera_authz::DictWriter::new(dict_dir.path());
    dict_writer.intern(b"satisfied-term");
    let dict_paths = dict_writer.finish().unwrap();
    let dict = tessera_authz::Dict::load(&dict_paths).unwrap();

    // Reopen: fresh replay from disk, not the in-memory `Overlay`/`IngestBuffer` above.
    let (_wal, records) = Wal::open(&wal_path).unwrap();
    let (overlay, buffer, _established, _resolver) = replay(&records, &dict, |_external_id| {
        Ok::<_, std::convert::Infallible>(None)
    })
    .unwrap();

    let x_entry = overlay.get(e(ENTITY_X)).expect("X has an overlay entry");
    assert!(x_entry.deleted, "delete must survive replay");
    assert!(
        !x_entry.suppressed,
        "unsuppress clears suppressed only, and does so across replay too"
    );

    let y_entry = overlay.get(e(ENTITY_Y)).expect("Y has an overlay entry");
    assert!(y_entry.deleted, "delete must survive replay");
    assert_eq!(
        y_entry.evaluate_terms(),
        Some(&[TermId::new(0)][..]),
        "predicate's granted term must also survive replay"
    );
    assert!(buffer.contains(e(ENTITY_X)));
    assert!(buffer.contains(e(ENTITY_Y)));

    // Compose against the Step 1 fixture (its `satisfied` set already contains `TermId(0)`) and
    // confirm both entities are excluded from the effective mask despite Y's satisfied predicate.
    let fx = build_fixture();
    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(!mask.contains_row(ENTITY_X as u32));
    assert!(!mask.contains_row(ENTITY_Y as u32));
    assert!(mask.check_structural_invariants());
}

/// Review finding #1, end to end: an unsatisfied `predicate` excludes an in-fragment entity; a
/// later `predicate` change carrying no descriptors (`terms: None`, representable per R5's
/// optional `access`) must not fall back to the fragment's original (included) verdict.
#[test]
fn predicate_with_no_terms_does_not_reopen_a_prior_evaluate_exclusion() {
    let fx = build_fixture();
    assert!(fx.fragment_entities.contains(&(EVAL_NARROW as u32)));
    assert!(fx.base.bitmap().contains(EVAL_NARROW as u32));

    let mut overlay = Overlay::new();
    overlay.apply(
        e(EVAL_NARROW),
        ChangeOp::Predicate,
        Some(evaluate(&[UNSATISFIED_TERM])),
    );
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(
        !mask.contains_row(EVAL_NARROW as u32),
        "first predicate must exclude the entity"
    );

    // A second, descriptor-less predicate change must not restore visibility.
    overlay.apply(e(EVAL_NARROW), ChangeOp::Predicate, None);
    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(
        !mask.contains_row(EVAL_NARROW as u32),
        "a descriptor-less predicate must not re-expose an entity a prior predicate excluded"
    );
    assert!(mask.check_structural_invariants());
}

/// Review finding #2, end to end: a term id from `DescriptorResolver`'s in-memory extension
/// (top-of-`u32`-range, per the fix in `tessera_lifecycle::buffer`) must never satisfy a
/// session's `satisfied` set built the ordinary way (from real, small dictionary ordinals) — the
/// "unsatisfiable until the next build" property is exercised here, not just asserted in prose.
#[test]
fn extension_only_term_never_passes_compose() {
    let fx = build_fixture();

    // A genuinely novel descriptor, resolved against an otherwise-empty dictionary, lands at
    // `u32::MAX` (top of the extension range) — nowhere near any id in `fx.satisfied`.
    let dict_dir = TempDir::new().unwrap();
    let dict_writer = tessera_authz::DictWriter::new(dict_dir.path());
    let dict_paths = dict_writer.finish().unwrap();
    let dict = tessera_authz::Dict::load(&dict_paths).unwrap();
    let mut resolver = tessera_lifecycle::DescriptorResolver::new(&dict);
    let extension_term = resolver.resolve(b"never-built-yet");
    assert!(!fx.satisfied.contains(&extension_term));

    // Outside the fragment, so any pass would show up as `plus`.
    assert!(!fx.fragment_entities.contains(&(EVAL_WIDEN as u32)));

    let mut overlay = Overlay::new();
    overlay.apply(
        e(EVAL_WIDEN),
        ChangeOp::Predicate,
        Some(tessera_lifecycle::overlay::resolve(
            &[b"never-built-yet".to_vec()],
            &mut resolver,
        )),
    );
    let buffer = IngestBuffer::new();

    let mask = compose_with(&fx, &overlay, &buffer);
    assert!(
        !mask.contains_row(EVAL_WIDEN as u32),
        "an extension-only term must never intersect a session's satisfied set"
    );
    assert_eq!(
        mask.count_range(full_range()),
        fx.base.bitmap().cardinality()
    );
    assert!(mask.check_structural_invariants());
}

/// The equivalence `visible_to` rests on, asserted rather than argued. For a
/// fixture exercising every precedence branch — deleted, suppressed, evaluate pass, evaluate
/// fail, a neutral overlay entry (delete → suppress → unsuppress leaves an entry present but not
/// currently suppressed), a deny outside the fragment (no-op), buffered pass/fail, and plain
/// fragment membership — `visible_to` must agree with `compose(...).contains_row(row_of(entity))`
/// for every entity that has a row. If `verdict` was correctly factored out of `compose` (rather
/// than transcribed a second time), this is what proves the factoring did not change `compose`'s
/// behaviour — two independent transcriptions of the precedence rule is exactly how a suppression
/// stops suppressing (lifecycle §3, caught twice in review).
#[test]
fn visible_to_agrees_with_compose_over_every_precedence_case() {
    let fx = build_fixture();

    let mut overlay = Overlay::new();
    overlay.apply(e(SUPPRESS_IN), ChangeOp::Suppress, None);
    overlay.apply(
        e(EVAL_NARROW),
        ChangeOp::Predicate,
        Some(evaluate(&[UNSATISFIED_TERM])),
    );
    overlay.apply(
        e(EVAL_KEEP),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    overlay.apply(
        e(EVAL_WIDEN),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    overlay.apply(e(DELETE_BEATS_EVAL), ChangeOp::Delete, None);
    overlay.apply(
        e(DELETE_BEATS_EVAL),
        ChangeOp::Predicate,
        Some(evaluate(&[SATISFIED_TERM_MARKER])),
    );
    overlay.apply(e(CROSS1_DSU), ChangeOp::Delete, None);
    overlay.apply(e(CROSS1_DSU), ChangeOp::Suppress, None);
    overlay.apply(e(CROSS1_DSU), ChangeOp::Unsuppress, None);
    overlay.apply(e(SUPPRESS_OUT), ChangeOp::Suppress, None);

    let mut buffer = IngestBuffer::new();
    insert_buffered(
        &mut buffer,
        BUFFERED_PASS,
        vec![TermId::new(SATISFIED_TERM_A)],
    );
    insert_buffered(
        &mut buffer,
        BUFFERED_FAIL,
        vec![TermId::new(UNSATISFIED_TERM)],
    );

    let fragment = fragment_for(&fx);
    let mask = compose(
        &fragment,
        &fx.satisfied,
        &overlay,
        &buffer,
        Arc::clone(&fx.base),
        &fx.perm,
    );
    assert!(mask.check_structural_invariants());

    for entity in 0..BOUND as u32 {
        let Some(row) = fx.perm.row_of(EntityId::new(entity as u64)) else {
            continue;
        };
        let expected = mask.contains_row(row.raw());
        let got = visible_to(
            &fragment,
            &fx.satisfied,
            &overlay,
            &buffer,
            EntityId::new(entity as u64),
        );
        assert_eq!(got, expected, "entity {entity}");
    }
}
