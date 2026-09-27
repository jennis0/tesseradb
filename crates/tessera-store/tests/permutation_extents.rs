//! Row space as a base permutation plus an ordered extent list (§2.1).
//!
//! The properties here are the ones a flush and a merge rest on: dispatch by entity range,
//! projection as a union over disjoint parts, and a collapse that moves no row.

use std::sync::Arc;

use tessera_store::permutation::{RowSpace, SegmentExtent};
use tessera_store::write::write_permutation;
use tessera_store::Permutation;
use tessera_types::{EntityId, RowId};

/// A base covering `[0, rows.len())`, entity *i* at row `rows[i]`.
fn base_of(rows: &[u32]) -> RowSpace {
    // Leaked so the mapped file outlives the borrow; a test process is the whole lifetime.
    let dir = Box::leak(Box::new(tempfile::TempDir::new().unwrap()));
    let path = dir.path().join("permutation.bin");
    let mut in_row_order = vec![EntityId::new(0); rows.len()];
    for (entity, &row) in rows.iter().enumerate() {
        in_row_order[row as usize] = EntityId::new(entity as u64);
    }
    write_permutation(&path, &in_row_order, rows.len() as u64).unwrap();
    RowSpace::new(
        Arc::new(Permutation::load(&path).unwrap()),
        rows.len() as u32,
    )
}

fn extent(lo: u64, hi: u64, seg_id: &str, row_base: u32, rows: &[u32]) -> SegmentExtent {
    SegmentExtent {
        entity_lo: lo,
        entity_hi: hi,
        seg_id: seg_id.to_string(),
        row_base,
        rows: rows.to_vec(),
        below: Vec::new(),
    }
}

/// Below the build bound the base decides; above it, the extent list does.
#[test]
fn row_of_dispatches_base_then_extents() {
    let space = base_of(&[0, 1, 2])
        .with_extent(extent(3, 4, "s1", 3, &[0, 1]))
        .unwrap();
    assert_eq!(space.row_of(EntityId::new(1)), Some(RowId::new(1)));
    assert_eq!(space.row_of(EntityId::new(3)), Some(RowId::new(3)));
    assert_eq!(space.row_of(EntityId::new(4)), Some(RowId::new(4)));
    assert_eq!(space.row_of(EntityId::new(9)), None);
}

/// The property the flush patch rests on: an extent's rows are disjoint from every existing
/// row, so projecting the whole is projecting the parts unioned.
#[test]
fn projecting_the_whole_equals_the_union_of_the_parts() {
    let space = base_of(&[0, 1, 2])
        .with_extent(extent(3, 4, "s1", 3, &[0, 1]))
        .unwrap();
    let mask = croaring::Bitmap::of(&[1, 3, 4]);

    let mut expected = base_of(&[0, 1, 2]).project(&mask);
    expected.or_inplace(&space.project_extents_from(&mask, 0));

    assert_eq!(space.project(&mask), expected);
}

/// **The complement route answers the base's part and the extents are the same term above it.**
///
/// `RowSpace::project` is the base's contribution unioned with every extent's own. The complement
/// route replaces the first of those two and nothing else, so a projection built that way must be
/// the projection the walk builds. Over a mask reaching into both parts, over the whole of both,
/// and over a narrow one.
#[test]
fn the_complement_route_under_the_extents_projects_what_the_walk_projects() {
    let space = base_of(&[2, 0, 1, 4, 3])
        .with_extent(extent(5, 7, "s1", 5, &[0, 1, 2]))
        .unwrap();
    space
        .base()
        .validate_rows(5)
        .expect("the base is a bijection onto [0, 5)");

    for (mask, what) in [
        (croaring::Bitmap::of(&[0, 1, 2, 3, 5, 6, 7]), "all but one"),
        (
            croaring::Bitmap::of(&[0, 1, 2, 3, 4, 5, 6, 7]),
            "everything",
        ),
        (croaring::Bitmap::of(&[1, 6]), "one entity in each part"),
        (croaring::Bitmap::of(&[6, 7]), "the extent alone"),
    ] {
        let mut rows = space
            .project_complement_base(&mask)
            .expect("the base records the row count it is a bijection onto");
        rows.or_inplace(&space.project_extents_from(&mask, 0));
        assert_eq!(
            rows,
            space.project(&mask),
            "{what}: the complement route under the extents lost or gained a row"
        );
    }
}

/// Merge is row-count preserving, so collapsing adjacent extents moves no later `row_base`.
#[test]
fn collapsing_adjacent_extents_preserves_every_row_id() {
    let space = base_of(&[0, 1, 2])
        .with_extent(extent(3, 3, "s1", 3, &[0]))
        .unwrap()
        .with_extent(extent(4, 5, "s2", 4, &[0, 1]))
        .unwrap()
        .with_extent(extent(6, 6, "s3", 6, &[0]))
        .unwrap();

    let merged = extent(3, 5, "s4", 3, &[0, 1, 2]);
    let after = space
        .collapsing(&["s1".into(), "s2".into()], merged)
        .expect("inputs are present and adjacent");

    assert_eq!(after.extent_count(), 2);
    for e in 3..=6u64 {
        assert_eq!(
            after.row_of(EntityId::new(e)),
            space.row_of(EntityId::new(e)),
            "entity {e}"
        );
    }
    assert_eq!(after.total_rows(), space.total_rows());
}

/// ABA safety: a merge whose inputs are gone publishes nothing.
#[test]
fn collapsing_refuses_when_an_input_seg_id_is_absent() {
    let space = base_of(&[0])
        .with_extent(extent(1, 1, "s1", 1, &[0]))
        .unwrap();
    assert!(space
        .collapsing(&["s-gone".into()], extent(1, 1, "s9", 1, &[0]))
        .is_none());
}

/// A bundle out of `tessera build` carries no extents, and must project byte-for-byte what the
/// bare permutation did — the guarantee that makes this change landable ahead of any flush.
#[test]
fn an_extent_free_row_space_projects_exactly_what_the_base_does() {
    let space = base_of(&[2, 0, 1, 4, 3]);
    let mask = croaring::Bitmap::of(&[0, 2, 3, 4, 99]);
    assert_eq!(
        space.project(&mask).serialize::<croaring::Portable>(),
        space
            .base()
            .project(&mask)
            .serialize::<croaring::Portable>()
    );
    assert_eq!(space.extent_count(), 0);
    assert_eq!(space.total_rows(), 5);
}

/// An extent that does not begin exactly where row space currently ends is corruption, not a
/// state to tolerate: every later `row_base` would be wrong by the gap.
#[test]
fn an_extent_is_refused_unless_it_continues_row_space_exactly() {
    let space = base_of(&[0, 1, 2]);
    assert!(space.with_extent(extent(3, 3, "s1", 4, &[0])).is_none());
    assert!(space.with_extent(extent(2, 2, "s1", 3, &[0])).is_none());
    assert!(space.with_extent(extent(3, 3, "s1", 3, &[1])).is_none());
}

/// A base over `bound` entities where `in_row_order[row]` holds each row, and every other entity
/// below `bound` has none: what a fold writes after removing a deleted entity's rows.
fn base_holding(bound: u64, in_row_order: &[u64]) -> RowSpace {
    let dir = Box::leak(Box::new(tempfile::TempDir::new().unwrap()));
    let path = dir.path().join("permutation.bin");
    let entities: Vec<EntityId> = in_row_order.iter().map(|&e| EntityId::new(e)).collect();
    write_permutation(&path, &entities, bound).unwrap();
    RowSpace::new(
        Arc::new(Permutation::load(&path).unwrap()),
        in_row_order.len() as u32,
    )
}

/// The extent a flush writes over `entities`, in row order, into `space`.
fn flushed(space: &RowSpace, seg_id: &str, entities: &[u64]) -> SegmentExtent {
    let span = SegmentExtent::flush_span(space.entity_floor(), entities.iter().copied());
    SegmentExtent::from_rows(
        seg_id,
        space.total_rows() as u32,
        span,
        entities.iter().copied(),
    )
    .expect("one row per entity")
}

/// **An entity a fold freed lies below the view's rows, and a flush lists its row.** It is found
/// by lookup, by projection and by the reverse direction, exactly as a row in the dense span is.
#[test]
fn an_entity_below_the_views_rows_is_listed_and_found() {
    // Entity 1's rows were removed by a fold; 0, 2 and 3 hold rows 0..3.
    let base = base_holding(4, &[0, 2, 3]);
    assert_eq!(base.row_of(EntityId::new(1)), None);
    let extent = flushed(&base, "s1", &[4, 1, 5]);
    assert_eq!((extent.entity_lo, extent.entity_hi), (4, 5));
    assert_eq!(extent.below, vec![(1, 1)]);
    let space = base.with_extent(extent).expect("the listed entity has no row");

    assert_eq!(space.row_of(EntityId::new(1)), Some(RowId::new(4)));
    assert_eq!(space.row_of(EntityId::new(4)), Some(RowId::new(3)));
    assert_eq!(space.entity_of(RowId::new(4)), Some(EntityId::new(1)));
    assert_eq!(
        space.project(&croaring::Bitmap::of(&[1, 5])),
        croaring::Bitmap::of(&[4, 5])
    );
    // A mask holding only the listed entity is below every dense span, and still projects.
    assert_eq!(
        space.project_extents_from(&croaring::Bitmap::of(&[1]), 0),
        croaring::Bitmap::of(&[4])
    );
    assert_eq!(space.total_rows(), 6);
}

/// A listed entity that already has a row in the view would alias two rows onto one entity.
#[test]
fn a_listed_entity_that_already_has_a_row_is_refused() {
    let base = base_holding(4, &[0, 2, 3]);
    let extent = flushed(&base, "s1", &[2, 4]);
    assert!(base.with_extent(extent).is_none());

    let space = base.with_extent(flushed(&base, "s1", &[1])).unwrap();
    assert!(
        space.with_extent(flushed(&space, "s2", &[1, 6])).is_none(),
        "an entity listed by an earlier extent is refused too"
    );
}

/// An extent whose every row is listed has an empty span at the view's floor. A later extent
/// continues above it, and a merge of the two moves no row.
#[test]
fn an_extent_of_listed_rows_alone_takes_an_empty_span_and_merges() {
    let base = base_holding(5, &[0, 3, 4]);
    let first = flushed(&base, "s1", &[2, 1]);
    assert_eq!((first.entity_lo, first.entity_hi), (5, 4), "empty at the floor");
    let space = base.with_extent(first).unwrap();
    assert_eq!(space.entity_floor(), 5);
    let space = space.with_extent(flushed(&space, "s2", &[6, 5])).unwrap();

    let merged = SegmentExtent::from_rows("s3", 3, (5, 6), [2, 1, 6, 5]).unwrap();
    let after = space
        .collapsing(&["s1".into(), "s2".into()], merged)
        .expect("the run is present and covers the same span and rows");
    for e in 0..=6u64 {
        assert_eq!(
            after.row_of(EntityId::new(e)),
            space.row_of(EntityId::new(e)),
            "entity {e}"
        );
    }
    let mask = croaring::Bitmap::of(&[1, 2, 5, 6]);
    assert_eq!(after.project(&mask), space.project(&mask));
}

/// A merge's span can hold an entity another extent lists: its dense slot is empty, and the lookup
/// falls through to the list.
#[test]
fn a_dense_span_with_an_empty_slot_defers_to_the_extent_that_lists_it() {
    let base = base_holding(1, &[0]);
    // Entity 2 was deleted before its flush, so its slot in s1's span is empty.
    let space = base.with_extent(flushed(&base, "s1", &[1, 3])).unwrap();
    let space = space.with_extent(flushed(&space, "s2", &[2, 4])).unwrap();
    assert_eq!(space.row_of(EntityId::new(2)), Some(RowId::new(3)));
    assert_eq!(
        space.project(&croaring::Bitmap::of(&[2, 3])),
        croaring::Bitmap::of(&[2, 3])
    );
}
