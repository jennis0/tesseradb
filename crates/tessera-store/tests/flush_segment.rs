//! What a flush writes (§3.1, §3.6).

use std::path::Path;

use tessera_store::flush::{write_flush_segment, FlushInput, FlushOutput, FlushRow};
use tessera_store::manifest::Quantisation;
use tessera_store::{open_bundle, ExternalIdSidecar, MortonSlice};
use tessera_types::{EntityId, IdentityKey, ROW_ABSENT};

mod fixture;
use fixture::{build_bundle, PARTITION, VIEW};

const KEY_HEX: &str = "0123456789abcdef0123456789abcdef";

fn unit_quantisation() -> Quantisation {
    Quantisation {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

fn row(entity: u64, external_id: Option<&[u8]>, x: f64, y: f64) -> FlushRow {
    FlushRow {
        entity_id: EntityId::new(entity),
        external_id: external_id.map(|id| id.to_vec()),
        x,
        y,
        scalars: vec![],
    }
}

fn flush(prefix_dir: &Path, seg_id: &str, rows: Vec<FlushRow>, row_base: u32) -> FlushOutput {
    let key = IdentityKey::from_hex(KEY_HEX).unwrap();
    write_flush_segment(
        prefix_dir,
        PARTITION,
        VIEW,
        FlushInput {
            incarnation: 0,
            seg_id,
            rows,
            quantisation: unit_quantisation(),
            identity_key: &key,
            shard_id: 0,
            scalar_schema: &[],
            row_base,
        }, &[],
    )
    .unwrap()
}

/// A segment is internally Morton-sorted **against the same global quantisation** every other
/// segment used, or `tile_ranges`' binary search is wrong for it: a tile resolves to one
/// contiguous range per segment, and a segment coded against different bounds answers that search
/// with rows from the wrong cells.
#[test]
fn a_flush_segment_is_morton_sorted_against_the_global_quantisation() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let prefix = dir.path().join("v00000");

    // Deliberately scattered, so an unsorted writer would be visible.
    let rows = vec![
        row(50, Some(b"e-50"), 0.9, 0.1),
        row(51, Some(b"e-51"), 0.1, 0.9),
        row(52, Some(b"e-52"), 0.5, 0.5),
        row(53, Some(b"e-53"), 0.05, 0.05),
    ];
    let out = flush(&prefix, "seg-flush", rows, 50);

    let codes = MortonSlice::load(&prefix.join(format!(
        "partitions/{PARTITION}/views/{VIEW}/segments/seg-flush/morton.u32"
    )))
    .unwrap();
    assert!(codes.u32().windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(codes.len(), 4);
    assert_eq!(out.segment.row_count, 4);
}

/// **The watermark is `entity_hi + 1`, and the name says the consequence rather than the
/// arithmetic.** Composition treats entities at or above the watermark as buffer-resident, so a
/// watermark of `entity_hi` would leave the highest flushed entity out of the fragment *and* out
/// of the buffer: invisible, with a row, for ever.
#[test]
fn the_highest_flushed_entity_is_not_left_invisible_by_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let out = flush(
        &dir.path().join("v00000"),
        "seg-flush",
        vec![row(50, None, 0.1, 0.1), row(51, None, 0.2, 0.2)],
        50,
    );
    assert_eq!(out.segment.entity_hi, 51);
    assert_eq!(out.watermark, 52, "one past the highest flushed entity");
}

/// The reverse external-id direction survives, or `/v1/items` answers a typed error for ever for
/// every post-build item once its WAL region is reclaimed (§3.6). Both directions are asserted
/// against the files themselves, not against a cached map.
#[test]
fn a_flushed_entity_resolves_in_both_external_id_directions() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let prefix = dir.path().join("v00000");

    let out = flush(
        &prefix,
        "seg-flush",
        vec![
            row(50, Some(b"zeta"), 0.1, 0.1),
            row(51, None, 0.2, 0.2),
            row(52, Some(b"alpha"), 0.3, 0.3),
        ],
        50,
    );

    // Forward, through the **ordinary sidecar reader**, against a manifest naming the flush's own
    // extent. That is the assertion worth making: the reader verifies the extent's digest and its
    // sortedness before answering, so a flush that wrote an unsorted or mis-digested extent is
    // refused there rather than silently resolving to the wrong entity here.
    let extent = out.locator_extent.clone().expect("the rows bind, so the flush writes a locator");
    let bundle = open_bundle(dir.path()).unwrap();
    let mut manifest = bundle.partitions[PARTITION].manifest.clone();
    manifest.external_id_runs = vec![extent.external_id_run.clone()];
    manifest.files.extend(out.files.clone());
    let sidecar =
        ExternalIdSidecar::deferred_from_manifest(&bundle.manifest, &manifest, &prefix).unwrap();
    assert_eq!(
        sidecar.resolve(b"alpha").unwrap(),
        Some(EntityId::new(52)),
        "the extent must resolve an id a flush created"
    );
    assert_eq!(sidecar.resolve(b"nope").unwrap(), None);

    // Reverse, through the locator extent: entity → ordinal into that same extent, dense over the
    // segment's entity range, sentinel where an item carried no external id.
    let locator = std::fs::read(prefix.join(&extent.path)).unwrap();
    let slots: Vec<u32> = locator
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(slots.len(), 3, "dense over [entity_lo, entity_hi]");
    assert_eq!(extent.entity_lo, 50);
    assert_eq!(extent.entity_hi, 52);

    // "alpha" sorts before "zeta", so entity 52 is ordinal 0 and entity 50 is ordinal 1.
    assert_eq!(slots[0], 1, "entity 50 -> 'zeta', the second id");
    assert_eq!(slots[1], ROW_ABSENT, "entity 51 carried no external id");
    assert_eq!(slots[2], 0, "entity 52 -> 'alpha', the first id");
}

/// Every file written is named and digested, or the loader refuses to open one the manifest never
/// vouched for — the second half of the read protocol's verification.
#[test]
fn every_file_written_is_named_and_digested() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let prefix = dir.path().join("v00000");
    let out = flush(
        &prefix,
        "seg-flush",
        vec![row(50, Some(b"a"), 0.1, 0.1)],
        50,
    );

    let seg_dir = prefix.join(format!(
        "partitions/{PARTITION}/views/{VIEW}/segments/seg-flush"
    ));
    let mut on_disk: Vec<String> = std::fs::read_dir(&seg_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut named: Vec<String> = out
        .files
        .keys()
        .map(|k| k.rsplit('/').next().unwrap().to_string())
        .collect();
    named.sort();
    assert_eq!(
        on_disk, named,
        "a file written but not named is unverifiable"
    );

    for (rel, digest) in &out.files {
        let bytes = std::fs::read(prefix.join(rel)).unwrap();
        assert_eq!(digest.size, bytes.len() as u64, "{rel}");
    }
}

/// The extent places every flushed entity at `row_base + its position in the sorted order`, which
/// is what makes `RowSpace::with_extent` accept it as a continuation of row space.
#[test]
fn the_extent_addresses_exactly_the_rows_the_segment_holds() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let out = flush(
        &dir.path().join("v00000"),
        "seg-flush",
        vec![
            row(50, None, 0.9, 0.9),
            row(51, None, 0.1, 0.1),
            row(52, None, 0.5, 0.5),
        ],
        50,
    );

    assert_eq!(out.extent.row_base, 50);
    assert_eq!(out.extent.rows.len(), 3);
    let mut rows = out.extent.rows.clone();
    rows.sort_unstable();
    assert_eq!(rows, vec![0, 1, 2], "a bijection onto the segment's rows");
}

/// Rows must arrive ascending by entity id — I9's monotone allocation is what makes the extent
/// dense, and an out-of-order batch would silently mis-address every row after the inversion.
#[test]
fn unordered_rows_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let key = IdentityKey::from_hex(KEY_HEX).unwrap();
    let result = write_flush_segment(
        &dir.path().join("v00000"),
        PARTITION,
        VIEW,
        FlushInput {
            incarnation: 0,
            seg_id: "seg-bad",
            rows: vec![row(52, None, 0.1, 0.1), row(50, None, 0.2, 0.2)],
            quantisation: unit_quantisation(),
            identity_key: &key,
            shard_id: 0,
            scalar_schema: &[],
            row_base: 50,
        }, &[],
    );
    assert!(result.is_err());
}
