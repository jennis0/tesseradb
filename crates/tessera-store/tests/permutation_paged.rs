//! The paged `permutation.bin`: the round trip, the sparsity it exists for, and the refusals that
//! keep its encoding canonical (contracts §2.6, `views.md` §8).
//!
//! **The subject is the representation, not the mapping.** `Permutation::row_of` answered the same
//! questions before the paging landed, so a test that only asks it would pass against the flat
//! array it replaced. What is asserted here is that a view sparse in entity space *stores* what it
//! occupies — the file's size, page by page — and that a file departing from the canonical
//! encoding is refused rather than read generously.

use std::fs;
use std::path::Path;

use tessera_store::write::{write_permutation, PagePlan, PermutationWriter};
use tessera_store::Permutation;
use tessera_types::EntityId;

/// The 24-byte header, a `u32` per page of directory, zero padding to a 4 KiB boundary.
const PAYLOAD_ALIGN: usize = 4096;
const PAGE_ENTRIES: u64 = 1 << 16;
const PAGE_BYTES: usize = PAGE_ENTRIES as usize * 4;

fn payload_start(page_count: usize) -> usize {
    (24 + page_count * 4).div_ceil(PAYLOAD_ALIGN) * PAYLOAD_ALIGN
}

/// What the flat array this replaced would have cost for the same bound: a 16-byte header and a
/// `u32` per entity id.
fn flat_size(bound: u64) -> usize {
    16 + bound as usize * 4
}

fn write(dir: &Path, name: &str, entities: &[u64], bound: u64) -> std::path::PathBuf {
    let path = dir.join(name);
    let row_order: Vec<EntityId> = entities.iter().copied().map(EntityId::new).collect();
    write_permutation(&path, &row_order, bound).expect("the permutation writes");
    path
}

/// Every entity the view holds reads back at its row, and every other entity in `bound` reads as
/// absent — whether its page is present or not.
fn assert_round_trip(path: &Path, entities: &[u64], bound: u64) {
    let perm = Permutation::load(path).expect("the permutation loads");
    assert_eq!(perm.bound(), bound);
    for (row, entity) in entities.iter().enumerate() {
        assert_eq!(
            perm.row_of(EntityId::new(*entity)).map(|r| r.raw()),
            Some(row as u32),
            "entity {entity} must read back at row {row}"
        );
    }
    perm.validate_rows(entities.len() as u32)
        .expect("the mapping is a bijection onto its rows");
}

#[test]
fn a_dense_view_has_every_page_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 3 * PAGE_ENTRIES;
    let entities: Vec<u64> = (0..bound).collect();
    let path = write(dir.path(), "dense.bin", &entities, bound);

    assert_round_trip(&path, &entities, bound);
    let perm = Permutation::load(&path).expect("loads");
    assert_eq!(perm.page_count(), 3);
    assert_eq!(
        perm.present_pages(),
        3,
        "a dense view is the degenerate case: every page present"
    );
    // The flat array plus a directory and its padding — the paging costs a dense view one 4 KiB
    // block and nothing else.
    let len = fs::metadata(&path).expect("stat").len() as usize;
    assert_eq!(len, payload_start(3) + 3 * PAGE_BYTES);
    assert!(
        len < flat_size(bound) + PAYLOAD_ALIGN,
        "a dense view must not pay materially more than the flat array: {len} against {}",
        flat_size(bound)
    );
}

#[test]
fn a_view_holding_one_page_of_a_wide_entity_space_stores_one_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A hundred entities at the top of a 64-page entity space: one page occupied, 63 absent.
    let bound = 64 * PAGE_ENTRIES;
    let entities: Vec<u64> = (63 * PAGE_ENTRIES..63 * PAGE_ENTRIES + 100).collect();
    let path = write(dir.path(), "one-page.bin", &entities, bound);

    assert_round_trip(&path, &entities, bound);
    let perm = Permutation::load(&path).expect("loads");
    assert_eq!(perm.page_count(), 64);
    assert_eq!(perm.present_pages(), 1);

    let len = fs::metadata(&path).expect("stat").len() as usize;
    assert_eq!(len, payload_start(64) + PAGE_BYTES);
    assert!(
        len * 60 < flat_size(bound),
        "one page of sixty-four must cost far less than the flat array: {len} against {}",
        flat_size(bound)
    );
    // An entity in an absent page is absent, not row 0 — the same answer the sentinel gives, from
    // a page that was never written.
    assert!(perm.row_of(EntityId::new(0)).is_none());
    assert!(perm.row_of(EntityId::new(PAGE_ENTRIES * 7 + 3)).is_none());
}

#[test]
fn holes_across_many_pages_cost_only_the_pages_occupied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 40 * PAGE_ENTRIES;
    // Pages 0, 3, 17 and 39 — the last of them the page the bound ends in.
    let entities: Vec<u64> = [0u64, 3, 17, 39]
        .iter()
        .flat_map(|page| (0..5).map(move |i| page * PAGE_ENTRIES + i * 1000))
        .collect();
    let path = write(dir.path(), "holes.bin", &entities, bound);

    assert_round_trip(&path, &entities, bound);
    let perm = Permutation::load(&path).expect("loads");
    assert_eq!(perm.present_pages(), 4);
    assert_eq!(
        fs::metadata(&path).expect("stat").len() as usize,
        payload_start(40) + 4 * PAGE_BYTES
    );
    // A hole *inside* a present page is the sentinel, and a hole between present pages is an
    // absent page. Both answer `None`, which is the property the two levels must not disagree on.
    assert!(perm.row_of(EntityId::new(1)).is_none());
    assert!(perm.row_of(EntityId::new(PAGE_ENTRIES + 1)).is_none());
}

#[test]
fn an_empty_view_holds_no_pages_at_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A view of a 10-page entity space that holds nothing — a group's quarter with no points in
    // it, which is an ordinary state and not an error.
    let bound = 10 * PAGE_ENTRIES;
    let path = write(dir.path(), "empty.bin", &[], bound);

    let perm = Permutation::load(&path).expect("an empty view still loads");
    assert_eq!(perm.bound(), bound);
    assert_eq!(perm.present_pages(), 0);
    assert!(perm.row_of(EntityId::new(0)).is_none());
    assert!(perm.row_of(EntityId::new(bound - 1)).is_none());
    perm.validate_rows(0)
        .expect("no rows, no bijection to break");
    assert_eq!(
        fs::metadata(&path).expect("stat").len() as usize,
        payload_start(10),
        "the header, the directory and its padding — no payload"
    );
    assert!(perm.project(&croaring::Bitmap::of(&[0, 5, 99])).is_empty());
}

#[test]
fn a_view_with_no_entity_space_at_all_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(dir.path(), "zero-bound.bin", &[], 0);
    let perm = Permutation::load(&path).expect("a zero bound loads");
    assert_eq!(perm.bound(), 0);
    assert_eq!(perm.page_count(), 0);
    assert!(perm.row_of(EntityId::new(0)).is_none());
}

/// The two entities either side of a page boundary — the off-by-one the two-level index invites.
#[test]
fn the_last_entity_of_a_page_and_the_first_of_the_next() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 3 * PAGE_ENTRIES;
    let entities = vec![PAGE_ENTRIES - 1, PAGE_ENTRIES, 2 * PAGE_ENTRIES - 1];
    let path = write(dir.path(), "boundary.bin", &entities, bound);

    assert_round_trip(&path, &entities, bound);
    let perm = Permutation::load(&path).expect("loads");
    assert_eq!(
        perm.present_pages(),
        2,
        "the three entities occupy pages 0 and 1, and not page 2"
    );
    assert!(perm.row_of(EntityId::new(2 * PAGE_ENTRIES)).is_none());
    assert!(perm.row_of(EntityId::new(bound - 1)).is_none());
    assert!(
        perm.row_of(EntityId::new(bound)).is_none(),
        "an entity at the bound is out of this permutation, not a wrapped index"
    );
}

/// A bound that is not a whole number of pages: the last page's tail names entities that cannot
/// exist, and must be sentinel.
#[test]
fn a_bound_inside_a_page_leaves_the_tail_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = PAGE_ENTRIES + 10;
    let entities = vec![PAGE_ENTRIES + 2, 7];
    let path = write(dir.path(), "ragged.bin", &entities, bound);

    assert_round_trip(&path, &entities, bound);
    let perm = Permutation::load(&path).expect("loads");
    assert_eq!(perm.page_count(), 2);
    assert_eq!(perm.present_pages(), 2);
    assert!(perm.row_of(EntityId::new(bound - 1)).is_none());
}

/// **The projection agrees with `row_of` across present and absent pages alike.**
///
/// The two paths through the directory are separate code — `project` caches the page across a
/// mask's ascending walk, `row_of` resolves one — so a mask spanning both kinds of page is what
/// stops the cache serving one page's rows under another page's entities.
#[test]
fn a_projection_agrees_with_row_of_across_the_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 8 * PAGE_ENTRIES;
    let entities: Vec<u64> = [0u64, 2, 5]
        .iter()
        .flat_map(|page| (0..300).map(move |i| page * PAGE_ENTRIES + i * 37))
        .collect();
    let path = write(dir.path(), "project.bin", &entities, bound);
    let perm = Permutation::load(&path).expect("loads");

    // Every entity of every page, present or absent, at a stride that lands on and off the rows.
    let mask: Vec<u32> = (0..bound).step_by(37).map(|e| e as u32).collect();
    let projected = perm.project(&croaring::Bitmap::of(&mask));
    let expected: Vec<u32> = mask
        .iter()
        .filter_map(|&e| perm.row_of(EntityId::new(e as u64)).map(|r| r.raw()))
        .collect();
    assert_eq!(projected, croaring::Bitmap::of(&expected));
    assert!(!expected.is_empty(), "the fixture must project something");
}

/// **The scatter layout and the planned layout write the same file**, over many pages with holes
/// between them — the multi-page form of the byte-identity property
/// `segment_roundtrip::a_scattered_permutation_is_byte_identical_to_a_sequential_one` pins for one
/// page. The fold cannot know its pages up front and the build can; if the two encodings could
/// differ, the same corpus would digest differently depending on which produced it.
#[test]
fn the_scatter_and_planned_layouts_are_byte_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 20 * PAGE_ENTRIES;
    let entities: Vec<u64> = [1u64, 4, 19]
        .iter()
        .flat_map(|page| (0..50).map(move |i| page * PAGE_ENTRIES + i * 211))
        .collect();

    let planned = write(dir.path(), "planned.bin", &entities, bound);

    // The same mapping learned in an order unrelated to entity or row, which is what a
    // Morton-ordered fold pass produces.
    let scattered = dir.path().join("scattered.bin");
    let mut writer = PermutationWriter::create(&scattered, bound).expect("create");
    let mut shuffled: Vec<(usize, u64)> = entities.iter().copied().enumerate().collect();
    shuffled
        .sort_by_key(|(row, entity)| entity.wrapping_mul(2_654_435_761).wrapping_add(*row as u64));
    for (row, entity) in shuffled {
        writer.set(EntityId::new(entity), row as u32).expect("set");
    }
    writer.finish().expect("finish");

    assert_eq!(
        fs::read(&planned).unwrap(),
        fs::read(&scattered).unwrap(),
        "the two producers must write the same permutation.bin byte for byte"
    );
    assert_round_trip(&scattered, &entities, bound);
}

/// A planned writer refuses a row for an entity the plan did not declare — the fail-closed half of
/// laying the file out before the rows arrive. Silently dropping it would leave the entity absent
/// from a view that holds it.
#[test]
fn a_planned_writer_refuses_an_undeclared_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("undeclared.bin");
    let bound = 4 * PAGE_ENTRIES;
    let plan = PagePlan::of_entities(bound, [EntityId::new(5)]).expect("plan");
    let mut writer = PermutationWriter::create_planned(&path, &plan).expect("create");
    writer
        .set(EntityId::new(5), 0)
        .expect("the declared page takes its row");
    let err = writer
        .set(EntityId::new(2 * PAGE_ENTRIES), 1)
        .expect_err("an undeclared page must be refused");
    assert!(
        err.to_string().contains("page"),
        "the refusal must name the page: {err}"
    );
}

/// **The flat array's version number refuses itself.** Version 1 was `bound` slots with no
/// directory; read as version 2 its first slots would be a directory and every lookup would be
/// wrong. `bundle_format` does not move for this (owner direction), so the version field is the
/// whole of the loud refusal.
#[test]
fn a_version_one_flat_permutation_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("flat.bin");
    let bound = 64u64;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"TSPM");
    bytes.extend_from_slice(&1u16.to_le_bytes()); // version 1
    bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
    bytes.extend_from_slice(&bound.to_le_bytes());
    for entity in 0..bound {
        bytes.extend_from_slice(&(entity as u32).to_le_bytes());
    }
    fs::write(&path, &bytes).expect("write the old shape");

    let err = Permutation::load(&path).expect_err("the flat array must be refused");
    assert!(
        err.to_string().contains("version"),
        "the refusal must name the version: {err}"
    );
}

/// A directory whose slots do not ascend with page index is refused. It would be a *valid* file in
/// every other sense — every lookup in range, every page present — and would serve each page under
/// another page's rows.
#[test]
fn a_non_canonical_directory_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 2 * PAGE_ENTRIES;
    let path = write(dir.path(), "swap.bin", &[1, PAGE_ENTRIES + 1], bound);

    let mut bytes = fs::read(&path).expect("read");
    // Swap the two directory entries: page 0 -> slot 1, page 1 -> slot 0.
    bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&0u32.to_le_bytes());
    fs::write(&path, &bytes).expect("rewrite");

    let err = Permutation::load(&path).expect_err("a permuted directory must be refused");
    assert!(
        err.to_string().contains("canonical"),
        "the refusal must say the directory is not canonical: {err}"
    );
}

/// Padding is part of the encoding: a file that carries something in it is not the file the
/// digest names, and would let two producers of one mapping disagree.
#[test]
fn stuffed_padding_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = PAGE_ENTRIES;
    let path = write(dir.path(), "padding.bin", &[3], bound);
    let mut bytes = fs::read(&path).expect("read");
    bytes[100] = 0x01;
    fs::write(&path, &bytes).expect("rewrite");

    let err = Permutation::load(&path).expect_err("stuffed padding must be refused");
    assert!(
        err.to_string().contains("padding"),
        "the refusal must name the padding: {err}"
    );
}
