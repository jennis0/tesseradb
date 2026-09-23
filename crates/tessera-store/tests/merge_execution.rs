//! What a merge emits (write-path §7).

mod fixture;

use std::path::Path;

use fixture::{build_bundle, PARTITION, VIEW};
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
            x: (((e * stride) % 97) as f64) / 97.0,
            y: (((e * 53) % 89) as f64) / 89.0,
            scalars: vec![],
        })
        .collect();
    write_flush_segment(
        &root.join("v00000"),
        PARTITION,
        VIEW,
        FlushInput {
            incarnation: 0,
            seg_id,
            rows,
            quantisation: quantisation(),
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &[],
            row_base: 0,
        }, &[],
    )
    .expect("the input segment writes");
    MergeInput {
        seg_id: seg_id.to_string(),
        entity_lo,
        entity_hi: entity_lo + count - 1,
    }
}

fn merge(root: &Path, inputs: &[MergeInput]) -> tessera_store::merge::MergeOutput {
    let schema: Vec<(String, ScalarType)> = vec![];
    execute_merge(
        &root.join("v00000"),
        PARTITION,
        VIEW,
        MergeSpec {
            incarnation: 0,
            seg_id: "merged-1",
            inputs,
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &schema,
            absent_ok: &[],
            row_base: 0,
        },
    )
    .expect("the merge executes")
}

fn seg_dir(root: &Path, seg_id: &str) -> std::path::PathBuf {
    root.join("v00000/partitions")
        .join(PARTITION)
        .join("views")
        .join(VIEW)
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
/// pressure, unlike the Lucene re-ranking this policy was drawn from (arch §11.3).
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
        VIEW,
        MergeSpec {
            incarnation: 0,
            seg_id: "merged-1",
            inputs: &[a, b],
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &schema,
            absent_ok: &[],
            row_base: 0,
        },
    );
    assert!(err.is_err());
}

/// **A declared scalar column an input lacks, and no declaration explains, fails the merge**
/// rather than being dropped or blanked.
///
/// Dropping it shifts every later scalar up a position, so the merged segment carries every value
/// under the wrong column's name — right count, right types, wrong data, and no error anywhere.
/// Blanking it writes the placeholder into every row and marks it absent, which is worse in the
/// one way that matters: the merge then reclaims the input the values were in. The inputs here
/// are written with no scalars while the merge declares one and names none lawful, which is the
/// shape a stepped-down or hand-repaired bundle presents.
///
/// **Mutation:** drop the `absent_ok` test in `gather_scalars` and this merge succeeds, silently.
#[test]
fn a_missing_scalar_column_fails_the_merge_rather_than_shifting_the_rest() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 5, 7);
    let b = segment(dir.path(), "in-b", 200, 5, 31);

    let schema = vec![("citations".to_string(), ScalarType::U64)];
    let err = execute_merge(
        &dir.path().join("v00000"),
        PARTITION,
        VIEW,
        MergeSpec {
            incarnation: 0,
            seg_id: "merged-1",
            inputs: &[a, b],
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &schema,
            absent_ok: &[],
            row_base: 0,
        },
    )
    .expect_err("a segment missing a declared column must not merge");
    assert!(
        err.to_string().contains("citations"),
        "the refusal must name the column: {err}"
    );
}

/// **A declared scalar column an input lawfully lacks is absent in every row of that input**,
/// never dropped and never a held zero (`ingest.md` §6.3).
///
/// The inputs here are written with no scalars while the merge declares one and names it as
/// declared since the inputs were written, which is the shape a column declared at a running
/// service presents to a merge of segments written before it: the merged segment carries the
/// column at its placeholder, and its presence bitmap leaves every one of those rows out, so a
/// reader takes the absence and not the zero.
///
/// **Mutation:** drop the schema check in the merge's presence pass and every row reads as
/// carrying a zero.
#[test]
fn a_column_declared_since_the_inputs_is_absent_in_every_row_rather_than_shifting_the_rest() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 5, 7);
    let b = segment(dir.path(), "in-b", 200, 5, 31);

    let schema = vec![("citations".to_string(), ScalarType::U64)];
    let lawful = vec!["citations".to_string()];
    let out = execute_merge(
        &dir.path().join("v00000"),
        PARTITION,
        VIEW,
        MergeSpec {
            incarnation: 0,
            seg_id: "merged-1",
            inputs: &[a, b],
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &schema,
            absent_ok: &lawful,
            row_base: 0,
        },
    )
    .expect("a segment lacking a declared column merges, the column absent in its rows");
    assert_eq!(out.segment.row_count, 10);
    let d = seg_dir(dir.path(), "merged-1");
    let cols = ColumnsRef::load(&d.join("columns.arrow")).unwrap();
    assert!(
        cols.scalar("citations").is_some(),
        "the merged segment carries the declared column"
    );
    let presence = cols.presence("citations");
    assert!(
        presence.bitmap().is_some(),
        "a presence bitmap is written, because some row is absent"
    );
    assert!(
        (0..10u32).all(|row| !presence.contains(row)),
        "every row came from an input without the column, so every row is absent"
    );
    assert_eq!(
        cols.tessera_id().len(),
        10,
        "the fixed columns keep every row"
    );
}

/// **One writer, two producers — and the k-way merge's bytes are the sort's bytes** (write-path §7).
///
/// `execute_merge` streams its inputs through a heap into `SegmentWriter` rather than decoding them
/// into `TilerItem`s and re-sorting, because the old shape peaked at a measured 4.4–4.9× its
/// inputs' bytes and that multiplier — not the policy — is why decision 0049 could not raise
/// `max_merged_segment_bytes`. A substitution is only safe if the output does not move, and "it
/// merges" is not that claim: this compares **both segment files byte for byte** against what
/// `write_segment` emits from the same rows concatenated and sorted, which is what the merge did
/// before.
///
/// The fixture interleaves deliberately (see [`segment`]'s `stride`), so the two producers
/// genuinely disagree about input order and agree only about output order.
///
/// **Mutations this kills:** dropping the `tessera_id` component of the heap key (ties inside one
/// Morton cell then order by input, not by identity); reading the residual from the wrong cursor;
/// spooling a column little-endian where arrow reads it native; emitting the extent's ordinal
/// rather than the emission ordinal.
#[test]
fn the_k_way_merge_emits_exactly_what_a_concatenate_and_sort_would() {
    use tessera_spatial::tiler::TilerItem;
    use tessera_spatial::unsplit32;
    use tessera_store::write::write_segment;
    use tessera_types::{MortonCode, TesseraId};

    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let inputs = [
        segment(dir.path(), "in-a", 100, 23, 7),
        segment(dir.path(), "in-b", 200, 19, 31),
        segment(dir.path(), "in-c", 300, 17, 11),
    ];

    // What the superseded path did: read every input's rows, concatenate, sort by
    // `(morton, tessera_id)`, write through the one segment writer.
    let mut items: Vec<TilerItem> = Vec::new();
    let mut entity_ids: Vec<tessera_types::EntityId> = Vec::new();
    for input in &inputs {
        let d = seg_dir(dir.path(), &input.seg_id);
        let codes = MortonSlice::load(&d.join("morton.u32")).unwrap();
        let cols = ColumnsRef::load(&d.join("columns.arrow")).unwrap();
        for row in 0..codes.u32().len() {
            let tessera_id = TesseraId::new(cols.tessera_id()[row]);
            let (qx, qy) = unsplit32(MortonCode::new(codes.u32()[row]), cols.residual()[row]);
            items.push(TilerItem {
                tessera_id,
                qx,
                qy,
                scalars: vec![],
            });
            entity_ids.push(key().invert(tessera_id).1);
        }
    }
    let expected_dir = dir.path().join("expected");
    std::fs::create_dir_all(&expected_dir).unwrap();
    let codes = tessera_spatial::sort_batch(&mut items, &mut entity_ids);
    write_segment(&expected_dir, &items, &codes, &[]).expect("the reference segment writes");

    merge(dir.path(), &inputs);
    let merged = seg_dir(dir.path(), "merged-1");
    for name in ["morton.u32", "cuts.u32", "columns.arrow"] {
        assert_eq!(
            std::fs::read(merged.join(name)).unwrap(),
            std::fs::read(expected_dir.join(name)).unwrap(),
            "{name}: the k-way merge and the sort must produce the same bytes, not merely the \
             same rows"
        );
    }
}

/// **The spools do not survive the merge that wrote them.**
///
/// `SegmentWriter` streams each column to a file beside its output and maps it back at `finish`.
/// At the fold's scale those spools are the corpus (compaction §3, ~12 GB at 10⁹), and a filled
/// device takes the whole write path down with it (write-path §1.3) — so they are unlinked by a
/// destructor rather than by a line at the bottom of the happy path.
///
/// **Mutation:** replace `SpoolGuard`'s `Drop` with a `remove_file` at the end of `finish` and this
/// still passes; delete either and it fails. What it pins is that a published segment directory
/// holds exactly the files the manifest names.
#[test]
fn a_merged_segment_directory_holds_no_spool_files() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);
    let a = segment(dir.path(), "in-a", 100, 6, 7);
    let b = segment(dir.path(), "in-b", 200, 5, 31);

    merge(dir.path(), &[a, b]);
    let mut names: Vec<String> = std::fs::read_dir(seg_dir(dir.path(), "merged-1"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["columns.arrow", "cuts.u32", "morton.u32"],
        "the segment directory must hold exactly what the manifest names"
    );
}
