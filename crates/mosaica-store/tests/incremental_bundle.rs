//! A generation is constructed incrementally, never by reopening the bundle (§1.2).
//!
//! `open_bundle` maps every file afresh and `Permutation::load` re-pays an O(bound)
//! `validate_rows`, so re-opening per flush would cost more than the flush it followed. §1.4's
//! drain-depth cost model rests on the same property from the other side: consecutive generations
//! share their base geometry rather than being whole distinct bundles, which is what makes the
//! marginal cost of a drain entry roughly one flush segment.

use std::sync::Arc;

use mosaica_store::permutation::SegmentExtent;
use mosaica_store::read::PublishedManifest;
use mosaica_store::{open_bundle, Bundle};
use mosaica_types::{EntityId, RowId};

mod fixture;
use fixture::{build_bundle, flush_segment, next_manifest, PARTITION, SHARD, VIEW};

/// The claim §1.4's drain-depth cost model rests on: consecutive generations share their base
/// geometry — the mapped `permutation.bin` and every existing segment's mmaps — rather than being
/// whole distinct bundles.
#[test]
fn an_incremental_bundle_shares_its_base_mappings() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base = Arc::new(open_bundle(dir.path()).unwrap());

    let (seg, extent) = flush_segment(dir.path(), &base, 50, 3);
    let next = base
        .with_segment(
            SHARD,
            VIEW,
            seg,
            extent,
            PublishedManifest {
                manifest: next_manifest(&base, 3),
                n: &base.partitions[PARTITION].segments_n + 1,
                refs: Default::default(),
            },
        )
        .unwrap();

    let a = &base.only_shard().views[VIEW];
    let b = &next.only_shard().views[VIEW];
    assert!(
        Arc::ptr_eq(a.row_space.base(), b.row_space.base()),
        "the base permutation was re-opened, not shared"
    );
    assert!(
        Arc::ptr_eq(&a.segments[0], &b.segments[0]),
        "the build segment's mmaps were re-opened, not shared"
    );
    assert_eq!(b.segments.len(), 2, "and the new segment was appended");
}

/// The extent joins row space, so the flushed entities resolve — and every entity below the build
/// bound still resolves exactly as it did.
#[test]
fn a_flushed_entity_resolves_and_the_base_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base = Arc::new(open_bundle(dir.path()).unwrap());

    let (seg, extent) = flush_segment(dir.path(), &base, 50, 3);
    let next = base
        .with_segment(
            SHARD,
            VIEW,
            seg,
            extent,
            PublishedManifest {
                manifest: next_manifest(&base, 3),
                n: &base.partitions[PARTITION].segments_n + 1,
                refs: Default::default(),
            },
        )
        .unwrap();

    let before = &base.only_shard().views[VIEW].row_space;
    let after = &next.only_shard().views[VIEW].row_space;

    for e in 0..50u64 {
        assert_eq!(
            after.row_of(EntityId::new(e)),
            before.row_of(EntityId::new(e)),
            "entity {e} moved"
        );
    }
    // The flushed entities occupy exactly the rows the extent claims — which rows *individually*
    // is decided by the segment's Morton sort, not by entity order, so the set is what is asserted.
    let mut rows: Vec<u32> = (50..53u64)
        .map(|e| {
            after
                .row_of(EntityId::new(e))
                .unwrap_or_else(|| panic!("entity {e} has no row"))
                .raw()
        })
        .collect();
    rows.sort_unstable();
    assert_eq!(rows, vec![50, 51, 52]);

    assert_eq!(
        after.row_of(EntityId::new(53)),
        None,
        "and no more than that"
    );
    assert_eq!(
        before.row_of(EntityId::new(50)),
        None,
        "the base is untouched"
    );
}

/// A merge substitutes rather than appends: the consumed segments leave the list and the merged
/// one takes their place, at their `row_base` and with their row count.
#[test]
fn a_merge_substitutes_the_consumed_segments() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base = Arc::new(open_bundle(dir.path()).unwrap());

    let (s1, e1) = flush_segment(dir.path(), &base, 50, 2);
    let one = base
        .with_segment(
            SHARD,
            VIEW,
            s1,
            e1,
            PublishedManifest {
                manifest: next_manifest(&base, 2),
                n: &base.partitions[PARTITION].segments_n + 1,
                refs: Default::default(),
            },
        )
        .unwrap();
    let (s2, e2) = flush_segment(dir.path(), &one, 52, 2);
    let two = one
        .with_segment(
            SHARD,
            VIEW,
            s2,
            e2,
            PublishedManifest {
                manifest: next_manifest(&one, 2),
                n: &one.partitions[PARTITION].segments_n + 1,
                refs: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(two.only_shard().views[VIEW].segments.len(), 3);

    let consumed: Vec<String> = two.only_shard().views[VIEW].segments[1..]
        .iter()
        .map(|s| s.seg_id.clone())
        .collect();
    let (merged_seg, merged_extent) = flush_segment(dir.path(), &base, 50, 4);
    let after = two
        .with_merged(
            SHARD,
            VIEW,
            &consumed,
            merged_seg,
            SegmentExtent {
                entity_lo: 50,
                entity_hi: 53,
                seg_id: merged_extent.seg_id.clone(),
                row_base: 50,
                rows: vec![0, 1, 2, 3],
                below: Vec::new(),
            },
            PublishedManifest {
                manifest: next_manifest(&two, 4),
                n: &two.partitions[PARTITION].segments_n + 1,
                refs: Default::default(),
            },
        )
        .unwrap();

    let view = &after.only_shard().views[VIEW];
    assert_eq!(view.segments.len(), 2, "two segments collapsed into one");
    assert_eq!(view.row_space.extent_count(), 1);
    for e in 50..=53u64 {
        assert_eq!(
            view.row_space.row_of(EntityId::new(e)),
            Some(RowId::new(e as u32)),
            "entity {e} moved under a row-count-preserving merge"
        );
    }
    assert!(
        Arc::ptr_eq(&two.only_shard().views[VIEW].segments[0], &view.segments[0]),
        "the untouched build segment is still shared"
    );
}

/// **ABA safety, as an `Err` rather than a panic.** A merge planned against a generation that has
/// since been superseded names `seg_id`s that are no longer present, and `seg_id`s are never
/// reused (contracts §2.1) — so absence is proof the inputs are gone, and the publication is
/// discarded.
#[test]
fn a_merge_whose_inputs_are_absent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base = Arc::new(open_bundle(dir.path()).unwrap());

    let (seg, extent) = flush_segment(dir.path(), &base, 50, 3);
    let err = base.with_merged(
        SHARD,
        VIEW,
        &["seg-that-never-existed".to_string()],
        seg,
        extent,
        PublishedManifest {
            manifest: next_manifest(&base, 3),
            n: &base.partitions[PARTITION].segments_n + 1,
            refs: Default::default(),
        },
    );
    assert!(
        err.is_err(),
        "a merge whose inputs are gone must not publish"
    );
}

/// An unknown partition or view is a caller error, not a silent no-op that publishes a
/// generation missing the segment it was told to add.
#[test]
fn an_unknown_view_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base = Arc::new(open_bundle(dir.path()).unwrap());

    let (seg, extent) = flush_segment(dir.path(), &base, 50, 3);
    assert!(base
        .with_segment(
            SHARD,
            "no-such-view",
            seg,
            extent,
            PublishedManifest {
                manifest: next_manifest(&base, 3),
                n: 1,
                refs: Default::default(),
            },
        )
        .is_err());
}

/// A bundle with no extents is what `mosaica build` writes, and `with_segment` must be the only
/// thing that changes it. Nothing here calls `open_bundle` twice.
#[test]
fn the_original_generation_is_unchanged_by_a_publication() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base: Arc<Bundle> = Arc::new(open_bundle(dir.path()).unwrap());

    let (seg, extent) = flush_segment(dir.path(), &base, 50, 3);
    let _next = base
        .with_segment(
            SHARD,
            VIEW,
            seg,
            extent,
            PublishedManifest {
                manifest: next_manifest(&base, 3),
                n: &base.partitions[PARTITION].segments_n + 1,
                refs: Default::default(),
            },
        )
        .unwrap();

    let view = &base.only_shard().views[VIEW];
    assert_eq!(view.segments.len(), 1);
    assert_eq!(view.row_space.extent_count(), 0);
    assert_eq!(view.row_space.total_rows(), 50);
}

/// **A publication may name a shard the bundle does not serve yet**, which is served from then on
/// with every declared view empty; one that leaves out a served shard is refused.
#[test]
fn a_publication_opens_a_shard_it_names_and_may_not_drop_a_served_one() {
    let dir = tempfile::tempdir().unwrap();
    build_bundle(dir.path(), 50);
    let base = Arc::new(open_bundle(dir.path()).unwrap());
    let one = mosaica_types::ShardId::new(1).unwrap();

    let mut manifest = next_manifest(&base, 0);
    fixture::add_shard_one(&mut manifest);
    let next = base
        .with_manifest(
            PARTITION,
            PublishedManifest {
                manifest,
                n: 1,
                refs: Default::default(),
            },
        )
        .expect("a new shard is opened");
    let opened = next.shard(one).expect("shard 1 is served");
    assert_eq!(
        opened.views[VIEW].segments.len(),
        0,
        "its declared view is empty"
    );
    assert_eq!(next.partitions[PARTITION].next_shard_id, 2);
    assert!(Arc::ptr_eq(
        &base.only_shard().views[VIEW].segments[0],
        &next.shard(mosaica_types::ShardId::ZERO).unwrap().views[VIEW].segments[0]
    ));

    let mut without = next.partitions[PARTITION].side_manifest();
    without.shards.records.remove(0);
    assert!(next
        .with_manifest(
            PARTITION,
            PublishedManifest {
                manifest: without,
                n: 2,
                refs: Default::default(),
            },
        )
        .is_err());
}
