//! What a merge emits (flush §5.1, §5.2).

mod fixture;

use std::path::Path;

use fixture::{build_bundle, PARTITION, SLICE};
use tessera_spatial::tiler::ScalarType;
use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::Quantisation;
use tessera_store::merge::{execute_merge, MergeInput, MergeSpec};
use tessera_store::read::{ColumnsRef, MortonSlice};
use tessera_types::{EntityId, IdentityKey};

fn key() -> IdentityKey {
    IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key")
}

fn quantisation() -> Quantisation {
    Quantisation {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// Write one segment of `count` entities from `entity_lo`, whose coordinates are a function of the
/// entity id — `stride` chosen so two segments' points **interleave** in Morton order rather than
/// falling into disjoint regions, which is what makes the merge's sort do real work.
fn segment(root: &Path, seg_id: &str, entity_lo: u64, count: u64, stride: u64) -> MergeInput {
    let rows: Vec<FlushRow> = (entity_lo..entity_lo + count)
        .map(|e| FlushRow {
            entity_id: EntityId::new(e),
            external_id: Some(format!("ext-{e}").into_bytes()),
            x: (((e * stride) % 97) as f32) / 97.0,
            y: (((e * 53) % 89) as f32) / 89.0,
            scalars: vec![],
        })
        .collect();
    write_flush_segment(
        &root.join("v00000"),
        PARTITION,
        SLICE,
        FlushInput {
            seg_id,
            rows,
            quantisation: quantisation(),
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &[],
            row_base: 0,
        },
    )
    .expect("the input segment writes");
    MergeInput {
        seg_id: seg_id.to_string(),
        entity_lo,
        entity_hi: entity_lo + count - 1,
    }
}

fn merge(root: &Path, inputs: &[MergeInput]) -> tessera_store::flush::FlushOutput {
    let schema: Vec<(String, ScalarType)> = vec![];
    execute_merge(
        &root.join("v00000"),
        PARTITION,
        SLICE,
        MergeSpec {
            seg_id: "merged-1",
            inputs,
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &schema,
            row_base: 0,
        },
    )
    .expect("the merge executes")
}

fn seg_dir(root: &Path, seg_id: &str) -> std::path::PathBuf {
    root.join("v00000/partitions")
        .join(PARTITION)
        .join("slices")
        .join(SLICE)
        .join("segments")
        .join(seg_id)
}

/// **Dropping rows is a fold, and folds belong to compaction.** A merge that reclaimed a
/// tombstoned row would be doing invariant-bearing work from a layer that must not.
#[test]
fn a_merge_emits_exactly_as_many_rows_as_it_consumed() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 10, 7);
    let b = segment(dir.path(), "in-b", 200, 7, 11);

    let out = merge(dir.path(), &[a, b]);
    assert_eq!(out.segment.row_count, 17);
    assert_eq!(out.segment.entity_lo, 100);
    assert_eq!(out.segment.entity_hi, 206);
}

/// The Morton sort **is** the tile index — a segment that is not internally sorted breaks
/// `tile_ranges`' binary search outright — so sorting is not optional and cannot be skipped under
/// pressure, unlike Lucene's re-rank decorator (arch §11.3).
#[test]
fn a_merged_segment_is_morton_sorted() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 20, 7);
    let b = segment(dir.path(), "in-b", 200, 20, 31);

    merge(dir.path(), &[a, b]);
    let codes = MortonSlice::load(&seg_dir(dir.path(), "merged-1").join("morton.u32")).unwrap();
    assert!(codes.u32().windows(2).all(|w| w[0] <= w[1]));
}

/// **Byte-exact through the code, never through coordinates.** A segment stores the code and its
/// residual, not the axes; recovering the axes as a bit permutation is what keeps a merged row's
/// position identical to its input's. Dequantise-and-re-quantise would move points by up to a
/// quantisation step, silently, on every merge.
///
/// Asserted as a multiset over `(tessera_id, code, residual)`: the *order* changes, and nothing
/// else may.
#[test]
fn a_merge_moves_no_point() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let inputs = [
        segment(dir.path(), "in-a", 100, 12, 7),
        segment(dir.path(), "in-b", 200, 9, 31),
    ];

    let mut before: Vec<(u64, u32, u32)> = Vec::new();
    for input in &inputs {
        let d = seg_dir(dir.path(), &input.seg_id);
        let codes = MortonSlice::load(&d.join("morton.u32")).unwrap();
        let cols = ColumnsRef::load(&d.join("columns.arrow")).unwrap();
        for row in 0..codes.u32().len() {
            before.push((
                cols.tessera_id()[row],
                codes.u32()[row],
                cols.residual()[row],
            ));
        }
    }
    before.sort_unstable();

    merge(dir.path(), &inputs);
    let d = seg_dir(dir.path(), "merged-1");
    let codes = MortonSlice::load(&d.join("morton.u32")).unwrap();
    let cols = ColumnsRef::load(&d.join("columns.arrow")).unwrap();
    let mut after: Vec<(u64, u32, u32)> = (0..codes.u32().len())
        .map(|row| {
            (
                cols.tessera_id()[row],
                codes.u32()[row],
                cols.residual()[row],
            )
        })
        .collect();
    after.sort_unstable();

    assert_eq!(
        before, after,
        "every point kept its identity and its exact position"
    );
}

/// The extent maps every consumed entity to its row in the merged order, over one contiguous span.
/// This is what `RowSpace` resolves through, so a wrong entry serves one entity's coordinates under
/// another's identity.
#[test]
fn the_extent_maps_every_entity_to_its_merged_row() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 6, 7);
    let b = segment(dir.path(), "in-b", 200, 5, 31);

    let out = merge(dir.path(), &[a, b]);
    let d = seg_dir(dir.path(), "merged-1");
    let cols = ColumnsRef::load(&d.join("columns.arrow")).unwrap();

    for entity in (100..106).chain(200..205) {
        let row = out.extent.rows[(entity - 100) as usize];
        assert_ne!(row, tessera_types::ROW_ABSENT, "entity {entity} has a row");
        let (shard, back) = key().invert(tessera_types::TesseraId::new(
            cols.tessera_id()[row as usize],
        ));
        assert_eq!(shard, 0);
        assert_eq!(
            back.raw(),
            entity,
            "the extent's row for {entity} carries {entity}'s identity"
        );
    }
    // The gap between the two input ranges is absent, not mapped to row 0 — which would serve
    // another entity's row for an id this segment never carried.
    assert_eq!(
        out.extent.rows[(150 - 100) as usize],
        tessera_types::ROW_ABSENT
    );
}

/// **Runs merge by caller key, because that is the only order a run has** (flush §5.2b). Unlike an
/// extent, a run cannot be ordered against its neighbours, so coalescing is a merge-sort over the
/// bytes — and the reader binary-searches the result.
#[test]
fn the_external_id_runs_coalesce_in_key_order() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 8, 7);
    let b = segment(dir.path(), "in-b", 200, 8, 31);

    merge(dir.path(), &[a, b]);
    let path = seg_dir(dir.path(), "merged-1").join("external-ids.arrow");
    let file = std::fs::File::open(&path).unwrap();
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::BinaryArray>()
            .unwrap();
        for i in 0..batch.num_rows() {
            keys.push(ids.value(i).to_vec());
        }
    }
    assert_eq!(keys.len(), 16, "every input key survives");
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "sorted bytewise, which is what the reader binary-searches"
    );
}

/// Inputs must be in ascending, non-overlapping entity order: the merged extent is one contiguous
/// span, and a window that interleaves with its neighbours is not one.
#[test]
fn out_of_order_inputs_are_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 200, 5, 7);
    let b = segment(dir.path(), "in-b", 100, 5, 31);

    let schema: Vec<(String, ScalarType)> = vec![];
    let err = execute_merge(
        &dir.path().join("v00000"),
        PARTITION,
        SLICE,
        MergeSpec {
            seg_id: "merged-1",
            inputs: &[a, b],
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &schema,
            row_base: 0,
        },
    );
    assert!(err.is_err());
}
