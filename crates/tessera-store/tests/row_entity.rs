//! `row-entity.u32` is the inverse of `permutation.bin`, and the property that matters is exactly
//! that: for every row, `entity_of(row_of(e)) == e`. A table that is merely *plausible* — right
//! length, right value range — would put one entity's filter verdict on another entity's row, and
//! a filtered viewport would answer confidently and wrongly.

use std::sync::Arc;

use tessera_store::permutation::{Permutation, RowSpace, SegmentExtent};
use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
use tessera_store::write::write_permutation;
use tessera_types::{EntityId, RowId};

/// `row_order[row] = entity`, the shape both producers hand the writer.
fn build(dir: &std::path::Path, row_order: &[u32], bound: u64) -> (Permutation, RowToEntity) {
    let perm_path = dir.join("permutation.bin");
    let entities: Vec<EntityId> = row_order.iter().map(|&e| EntityId::new(e as u64)).collect();
    write_permutation(&perm_path, &entities, bound).expect("permutation writes");

    let table_path = dir.join(ROW_ENTITY_FILE);
    write_row_entity(&table_path, row_order).expect("table writes");

    (
        Permutation::load(&perm_path).expect("permutation loads"),
        RowToEntity::load(&table_path).expect("table loads"),
    )
}

#[test]
fn the_table_inverts_the_permutation_for_every_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Deliberately not the identity: an identity table would pass a wrong implementation that
    // returned the row back as the entity.
    let row_order: Vec<u32> = vec![7, 3, 0, 9, 4, 1, 8, 2, 6, 5];
    let (permutation, table) = build(dir.path(), &row_order, 10);

    for (row, &entity) in row_order.iter().enumerate() {
        let e = EntityId::new(entity as u64);
        let r = permutation.row_of(e).expect("every entity has a row here");
        assert_eq!(
            r.raw() as usize,
            row,
            "permutation disagrees about entity {entity}"
        );
        assert_eq!(
            table.entity_of(r),
            Some(e),
            "the table does not invert the permutation at row {row}"
        );
    }
}

#[test]
fn a_row_past_the_table_is_none_rather_than_a_wrapped_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let row_order: Vec<u32> = vec![2, 0, 1];
    let (_permutation, table) = build(dir.path(), &row_order, 3);

    assert_eq!(table.row_count(), 3);
    assert_eq!(table.entity_of(RowId::new(3)), None);
    assert_eq!(table.entity_of(RowId::new(u32::MAX)), None);
}

#[test]
fn a_truncated_table_refuses_to_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(ROW_ENTITY_FILE);
    write_row_entity(&path, &[1, 0]).expect("table writes");
    // One byte short of a whole slot — the shape a partial write leaves.
    let bytes = std::fs::read(&path).expect("read");
    std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");

    assert!(
        RowToEntity::load(&path).is_err(),
        "a table that is not a whole number of slots must refuse rather than round down"
    );
}

#[test]
fn row_space_answers_across_the_base_and_its_extents() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Base: entities 0..4 at rows 0..4, shuffled.
    let row_order: Vec<u32> = vec![3, 1, 0, 2];
    let (permutation, table) = build(dir.path(), &row_order, 4);

    // An extent above the base: entities 10..=12, rows 4..7 in the extent's own order.
    // `rows[e - entity_lo]` is relative to `row_base`.
    let extent = SegmentExtent {
        entity_lo: 10,
        entity_hi: 12,
        seg_id: "s1".to_string(),
        row_base: 4,
        rows: vec![2, 0, 1],
    };

    let space = RowSpace::new(Arc::new(permutation), 4)
        .with_row_entity(Arc::new(table))
        .with_extent(extent)
        .expect("the extent continues row space");

    // Base rows resolve through the table.
    for (row, &entity) in row_order.iter().enumerate() {
        assert_eq!(
            space.entity_of(RowId::new(row as u32)),
            Some(EntityId::new(entity as u64)),
            "base row {row}"
        );
    }
    // Extent rows resolve through the derived tail: entity 10 is at extent row 2 (absolute 6),
    // entity 11 at 0 (absolute 4), entity 12 at 1 (absolute 5).
    assert_eq!(space.entity_of(RowId::new(4)), Some(EntityId::new(11)));
    assert_eq!(space.entity_of(RowId::new(5)), Some(EntityId::new(12)));
    assert_eq!(space.entity_of(RowId::new(6)), Some(EntityId::new(10)));
    assert_eq!(space.entity_of(RowId::new(7)), None, "past row space");

    // And the round trip holds in both directions across the boundary.
    for entity in [0u64, 1, 2, 3, 10, 11, 12] {
        let e = EntityId::new(entity);
        let r = space.row_of(e).expect("every entity here has a row");
        assert_eq!(
            space.entity_of(r),
            Some(e),
            "round trip for entity {entity}"
        );
    }
}

#[test]
fn a_row_space_without_a_table_answers_none_rather_than_guessing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let perm_path = dir.path().join("permutation.bin");
    let entities: Vec<EntityId> = [1u64, 0].iter().map(|&e| EntityId::new(e)).collect();
    write_permutation(&perm_path, &entities, 2).expect("permutation writes");
    let space = RowSpace::new(Arc::new(Permutation::load(&perm_path).expect("loads")), 2);

    // `None` here means "ask another way" — the caller falls back to projecting. It must never be
    // read as "this row has no entity", which would silently drop rows from a filtered viewport.
    assert_eq!(space.entity_of(RowId::new(0)), None);
    assert_eq!(space.entity_of(RowId::new(1)), None);
}
