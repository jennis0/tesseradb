//! Compaction's pass 1 (compaction §3, "Pass 1 — row space"): a k-way merge over every live
//! segment, tombstone-skipping, feeding [`SegmentWriter`] and a mapped `permutation.bin`.
//!
//! Self-contained rather than sharing `tests/fixture`: pass 1 never touches a whole bundle (no
//! `MANIFEST.json`, no `CURRENT`, no side-manifest) — its only inputs are bare segment directories
//! — so building one directly here is less machinery than routing through `fixture::build_bundle`.

use std::fs;
use std::path::{Path, PathBuf};

use croaring::Bitmap;

use tessera_spatial::fixed32;
use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_store::read::{ColumnsRef, MortonSlice};
use tessera_store::write::write_segment;
use tessera_store::{fold_row_space, FoldRowSpaceSpec, FoldSegmentInput, Permutation, RowToEntity};
use tessera_types::{EntityId, IdentityKey, RowId};

fn key() -> IdentityKey {
    IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key")
}

/// Write one live input segment covering `entities`, whose coordinates are a function of
/// `stride` — chosen, as in `merge_execution.rs`'s fixture, so that two segments' points
/// **interleave** in Morton order rather than landing in disjoint regions. That is what makes
/// this fixture exercise the "inputs are the whole row space, not an adjacent window" property
/// pass 1 is built for: an entity-ordered scan of the inputs would not reproduce the output order,
/// only the heap over `(morton, tessera_id)` would.
fn write_input(dir: &Path, seg_id: &str, entities: &[u64], stride: u64) -> FoldSegmentInput {
    let k = key();
    let mut items: Vec<TilerItem> = entities
        .iter()
        .map(|&e| TilerItem {
            tessera_id: k.forward(0, EntityId::new(e)).expect("entity fits u32"),
            qx: fixed32(((e * stride) % 97) as f64 / 97.0, 0.0, 1.0),
            qy: fixed32(((e * 53) % 89) as f64 / 89.0, 0.0, 1.0),
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = entities.iter().copied().map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    let seg_dir = dir.join(seg_id);
    fs::create_dir_all(&seg_dir).expect("mkdir");
    write_segment(&seg_dir, &items, &codes, &[]).expect("write_segment");

    FoldSegmentInput {
        seg_id: seg_id.to_string(),
        dir: seg_dir,
    }
}

/// **A declared column an input lacks fails the fold unless a declaration since the inputs
/// explains it** (`ingest.md` §6.3). The inputs carry no scalars; a fold declaring one and naming
/// none lawful is a torn bundle and refuses naming the column, since writing the rows as absent
/// would blank the column and then reclaim the input. The same fold with the column named as
/// declared since the inputs writes it at its placeholder with every row absent.
#[test]
fn a_column_an_input_lacks_fails_the_fold_unless_declared_since_the_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let input = write_input(dir.path(), "in-a", &[1, 2, 3], 7);
    let schema = vec![(
        "citations".to_string(),
        tessera_spatial::tiler::ScalarType::U64,
    )];
    let identity_key = key();
    let no_tombstones = Bitmap::new();
    let spec = |absent_ok: &'static [String]| FoldRowSpaceSpec {
        inputs: std::slice::from_ref(&input),
        identity_key: &identity_key,
        shard_id: 0,
        scalar_schema: &schema,
        absent_ok,
        tombstones: &no_tombstones,
        permutation_bound: 8,
    };
    let torn = fold_row_space(
        &dir.path().join("torn-seg"),
        &dir.path().join("torn.bin"),
        &dir.path().join("torn-rows.u32"),
        spec(&[]),
    );
    match torn {
        Err(e) => assert!(e.to_string().contains("citations"), "{e}"),
        Ok(_) => panic!("a column no declaration explains refuses the fold"),
    }

    let lawful: &'static [String] = Box::leak(vec!["citations".to_string()].into_boxed_slice());
    fold_row_space(
        &dir.path().join("out-seg"),
        &dir.path().join("permutation.bin"),
        &dir.path().join(tessera_store::ROW_ENTITY_FILE),
        spec(lawful),
    )
    .expect("a column declared since the inputs folds as absence");
    let cols = ColumnsRef::load(&dir.path().join("out-seg").join("columns.arrow")).unwrap();
    assert!(cols.scalar("citations").is_some());
    let presence = cols.presence("citations");
    assert!((0..3u32).all(|row| !presence.contains(row)));
}

fn tombstones(entities: &[u64]) -> Bitmap {
    Bitmap::of(&entities.iter().map(|&e| e as u32).collect::<Vec<u32>>())
}

fn fold(
    root: &Path,
    inputs: &[FoldSegmentInput],
    tombstones: &Bitmap,
    bound: u64,
) -> (tessera_store::FoldRowSpaceOutput, PathBuf, PathBuf) {
    let output_dir = root.join("out-seg");
    let permutation_path = root.join("permutation.bin");
    let row_entity_path = root.join(tessera_store::ROW_ENTITY_FILE);
    let out = fold_row_space(
        &output_dir,
        &permutation_path,
        &row_entity_path,
        FoldRowSpaceSpec {
            inputs,
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &[],
            absent_ok: &[],
            tombstones,
            permutation_bound: bound,
        },
    )
    .expect("fold_row_space executes");
    (out, output_dir, permutation_path)
}

/// Every input row, as `(entity, tessera_id, morton, residual)`, read straight off the mapped
/// files — never through `unsplit32`, so a test bug here cannot mask a dequantise-requantise
/// regression the way comparing decoded coordinates would.
fn read_input_rows(input: &FoldSegmentInput) -> Vec<(u64, u64, u32, u32)> {
    let codes = MortonSlice::load(&input.dir.join("morton.u32")).unwrap();
    let cols = ColumnsRef::load(&input.dir.join("columns.arrow")).unwrap();
    let k = key();
    (0..codes.u32().len())
        .map(|row| {
            let tessera_id = cols.tessera_id()[row];
            let (_, entity) = k.invert(tessera_types::TesseraId::new(tessera_id));
            (
                entity.raw(),
                tessera_id,
                codes.u32()[row],
                cols.residual()[row],
            )
        })
        .collect()
}

fn read_output_rows(output_dir: &Path) -> Vec<(u64, u32, u32)> {
    let codes = MortonSlice::load(&output_dir.join("morton.u32")).unwrap();
    let cols = ColumnsRef::load(&output_dir.join("columns.arrow")).unwrap();
    (0..codes.u32().len())
        .map(|row| {
            (
                cols.tessera_id()[row],
                codes.u32()[row],
                cols.residual()[row],
            )
        })
        .collect()
}

/// **Tombstoned rows are dropped, everything else survives, and Morton order holds across the
/// merge of every live segment — not just one adjacent window.**
///
/// Two segments interleave in Morton space (see [`write_input`]), so this fails if the fold
/// degenerates into "concatenate the inputs" (wrong order) or "walk them in entity order" (also
/// wrong order — pass 1 has no such concept). It also fails if the tombstone filter is applied
/// against the wrong entity (e.g. the row index rather than the inverted entity id), which would
/// drop or keep the wrong rows.
#[test]
fn fold_drops_tombstoned_rows_and_keeps_the_rest_in_morton_order() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = write_input(dir.path(), "seg-a", &(100..110).collect::<Vec<_>>(), 7);
    let b = write_input(dir.path(), "seg-b", &(200..208).collect::<Vec<_>>(), 31);

    let dead = tombstones(&[103, 105, 202]);
    let (out, output_dir, _) = fold(dir.path(), &[a, b], &dead, 1000);

    assert_eq!(out.row_count, 10 + 8 - 3);

    let rows = read_output_rows(&output_dir);
    assert_eq!(rows.len(), out.row_count as usize);
    assert!(
        rows.windows(2)
            .all(|w| (w[0].1, w[0].0) <= (w[1].1, w[1].0)),
        "output rows must be (morton, tessera_id) ascending"
    );

    let k = key();
    let surviving_entities: Vec<u64> = rows
        .iter()
        .map(|&(tid, _, _)| k.invert(tessera_types::TesseraId::new(tid)).1.raw())
        .collect();
    for e in (100..110).chain(200..208) {
        let dropped = [103u64, 105, 202].contains(&e);
        assert_eq!(
            surviving_entities.contains(&e),
            !dropped,
            "entity {e}: dropped={dropped}, but presence in the output disagreed"
        );
    }
}

/// **Byte-exact through the code, never through coordinates**, asserted as a multiset over every
/// surviving input row against the output.
///
/// Order is deliberately discarded before comparing (both sides sorted) — pass 1's job is to
/// reorder into `(morton, tessera_id)` order, so the useful assertion is that no triple was
/// altered, not that it kept its input position. A dequantise-then-requantise round trip (reading
/// a row via `unsplit32`/`split32` instead of carrying `(morton, residual)` straight through)
/// would perturb a residual by up to a cell and show up here as a multiset mismatch, even though
/// row counts, entity membership and sort order would all still look right.
#[test]
fn every_surviving_points_triple_matches_its_input_exactly() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = write_input(dir.path(), "seg-a", &(100..112).collect::<Vec<_>>(), 7);
    let b = write_input(dir.path(), "seg-b", &(200..209).collect::<Vec<_>>(), 31);

    let dead = tombstones(&[104, 201, 208]);

    let mut expected: Vec<(u64, u32, u32)> = Vec::new();
    for input in [&a, &b] {
        for (entity, tessera_id, morton, residual) in read_input_rows(input) {
            if !dead.contains(entity as u32) {
                expected.push((tessera_id, morton, residual));
            }
        }
    }
    expected.sort_unstable();

    let (out, output_dir, _) = fold(dir.path(), &[a, b], &dead, 1000);
    let mut actual = read_output_rows(&output_dir);
    actual.sort_unstable();

    assert_eq!(out.row_count as usize, expected.len());
    assert_eq!(expected, actual);
}

/// **`permutation.bin` maps every surviving entity to its new row, and every entity that lost or
/// never had a row reads back as absent — never row 0.**
///
/// Checked two ways: through `Permutation::row_of` (the only legal read path, I4), and by reading
/// the sentinel bytes directly, since `Permutation::row_of` conflates "never had a row" with a
/// hand-broken reader that zero-fills instead of `0xFF`-filling — both would return `None` here on
/// a correctly *sized* file, but only the byte check catches a writer that forgot the fill
/// (`PermutationWriter::create`'s whole reason to exist over a plain zeroed file).
///
/// Also asserts the row-id-shift property compaction §6 exists for: entity 109's row in the
/// output is **not** its position in either input, because two earlier rows (100..109's tombstoned
/// members) were dropped ahead of it in `(morton, tessera_id)` order — an implementation that
/// scattered using an input row index instead of an emitted-row counter would misplace it.
#[test]
fn permutation_maps_survivors_and_marks_dropped_and_unknown_entities_absent() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = write_input(dir.path(), "seg-a", &(100..110).collect::<Vec<_>>(), 7);
    let b = write_input(dir.path(), "seg-b", &(200..208).collect::<Vec<_>>(), 31);

    let dead = tombstones(&[101, 103, 205]);
    let bound = 1000u64;
    let (out, output_dir, perm_path) = fold(dir.path(), &[a, b], &dead, bound);

    let rows = read_output_rows(&output_dir);
    let k = key();

    let permutation = Permutation::load(&perm_path).expect("permutation.bin loads");
    assert_eq!(permutation.bound(), bound);

    // Every surviving entity: `row_of` answers the row where its tessera_id actually landed.
    for e in (100..110).chain(200..208) {
        if [101u64, 103, 205].contains(&e) {
            continue;
        }
        let tessera_id = k.forward(0, EntityId::new(e)).unwrap();
        let expected_row = rows
            .iter()
            .position(|&(tid, _, _)| tid == tessera_id.raw())
            .unwrap_or_else(|| panic!("entity {e} must survive and appear in the output"));
        assert_eq!(
            permutation
                .row_of(EntityId::new(e))
                .map(|r| r.raw() as usize),
            Some(expected_row),
            "entity {e}'s permutation entry must match where it actually landed"
        );
    }

    // Every tombstoned entity, and one never named by any input (`500`), read as absent.
    for e in [101u64, 103, 205, 500] {
        assert_eq!(
            permutation.row_of(EntityId::new(e)),
            None,
            "entity {e} must read as absent, not row 0"
        );
    }
    assert_eq!(out.row_count as usize, rows.len());

    // The `0xFF` sentinel, directly: read entity 500's raw slot bytes (never named by any input
    // or tombstone) and confirm the writer's fill, not merely that some reader interprets it as
    // absent. Format per `tessera_store::permutation`: a 24-byte header, a `u32` per page of
    // directory, zero padding to a 4 KiB boundary, then the present pages of 2¹⁶ slots each. This
    // fold's bound is 1000, so there is one page and it is present.
    let bytes = fs::read(&perm_path).expect("read permutation.bin");
    let at = 4096 + 500 * 4;
    let slot = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!(
        slot, 0xFFFF_FFFF,
        "an absent slot must hold the 0xFF sentinel, never zero"
    );
}

/// **A tombstone naming an entity with no row anywhere in the live segments costs nothing.** Pass
/// 1 must not error, and must not drop anything else, when `D₀` mentions an id this fold's inputs
/// never carried — the ordinary case for most of `D₀` on any real deployment, since a deletion is
/// named in the overlay long before (or long after, if it targets a not-yet-flushed buffer entity)
/// the fold that eventually executes it.
#[test]
fn a_tombstone_naming_an_entity_with_no_row_is_harmless() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = write_input(dir.path(), "seg-a", &(100..106).collect::<Vec<_>>(), 7);

    let dead = tombstones(&[9_999]);
    let (out, output_dir, _) = fold(dir.path(), &[a], &dead, 20_000);

    assert_eq!(
        out.row_count, 6,
        "no input row names the tombstoned entity, so none is dropped"
    );
    let rows = read_output_rows(&output_dir);
    assert_eq!(rows.len(), 6);
}

/// **The empty tombstone set is a faithful re-emission**: every input row survives, none is
/// reordered incorrectly, and the row count is exactly the sum of the inputs'. This is the
/// baseline the drop-related tests above are deviations from — if this one fails, a failure in
/// any of them could be the fixture rather than the fold.
#[test]
fn the_empty_tombstone_set_is_a_faithful_reemission() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = write_input(dir.path(), "seg-a", &(100..115).collect::<Vec<_>>(), 7);
    let b = write_input(dir.path(), "seg-b", &(300..309).collect::<Vec<_>>(), 41);

    let mut expected: Vec<(u64, u32, u32)> = Vec::new();
    for input in [&a, &b] {
        for (_, tessera_id, morton, residual) in read_input_rows(input) {
            expected.push((tessera_id, morton, residual));
        }
    }
    expected.sort_unstable();

    let empty = Bitmap::new();
    let (out, output_dir, perm_path) = fold(dir.path(), &[a, b], &empty, 1000);

    assert_eq!(out.row_count as usize, expected.len());
    let mut actual = read_output_rows(&output_dir);
    actual.sort_unstable();
    assert_eq!(expected, actual);

    let permutation = Permutation::load(&perm_path).expect("permutation.bin loads");
    for e in (100..115).chain(300..309) {
        assert!(
            permutation.row_of(EntityId::new(e)).is_some(),
            "entity {e} must have a row when nothing was tombstoned"
        );
    }
}

/// **The fold's two outputs invert one another.** Pass 1 writes `permutation.bin` and
/// `row-entity.u32` from the same loop, and nothing downstream checks that they agree:
/// `RowSpace::with_row_entity` stores the table with no validation, not even a length check. So
/// the agreement is asserted here, over a fold's own production of both, rather than over a
/// hand-supplied row order (which is what `tests/row_entity.rs` covers).
///
/// The harm is in the widening direction. `RowSpace::entity_of` is how the filtered viewport's
/// per-tile crossing route decides which of a viewport's rows belong to a filter's verdict set;
/// that set is a subset of `M_auth`, so a table misaligned with the permutation admits rows whose
/// true entity is not in it. The route is chosen by a cost ratio, so the same request answers
/// correctly on one shape and wrongly on another.
///
/// Mutations this kills: pushing the entity above the tombstone `continue`, so the table gains one
/// entry per *input* row rather than per emitted row — every row after the first tombstone is
/// attributed to the wrong entity and the table claims more rows than the segment holds; and any
/// off-by-one, reordering or duplicate in either structure.
#[test]
fn the_fold_writes_a_row_entity_table_that_inverts_its_permutation() {
    let dir = tempfile::TempDir::new().unwrap();
    let a = write_input(dir.path(), "seg-a", &(100..110).collect::<Vec<_>>(), 7);
    let b = write_input(dir.path(), "seg-b", &(200..208).collect::<Vec<_>>(), 31);

    let dead = tombstones(&[101, 103, 205]);
    let (out, _output_dir, perm_path) = fold(dir.path(), &[a, b], &dead, 1000);
    let table_path = dir.path().join(tessera_store::ROW_ENTITY_FILE);

    let permutation = Permutation::load(&perm_path).expect("permutation.bin loads");
    let table = RowToEntity::load(&table_path).expect("row-entity.u32 loads");

    // The fixture must actually drop rows, or the two structures agree for the trivial reason
    // that nothing was skipped between them and this test proves nothing.
    assert_eq!(out.row_count, 15, "18 input rows less three tombstoned");
    assert_eq!(
        table.row_count(),
        out.row_count,
        "the table must describe exactly the rows the segment holds"
    );

    for row in 0..out.row_count {
        let row = RowId::new(row);
        let entity = table
            .entity_of(row)
            .unwrap_or_else(|| panic!("row {} has no entity in the table", row.raw()));
        assert_eq!(
            permutation.row_of(entity),
            Some(row),
            "the table sends row {} to entity {}, which the permutation does not send back",
            row.raw(),
            entity.raw()
        );
        assert!(
            !dead.contains(entity.raw() as u32),
            "row {} is attributed to tombstoned entity {}",
            row.raw(),
            entity.raw()
        );
    }

    // And the other direction, so a table that merely covers a subset of the rows cannot pass:
    // every surviving entity is named by exactly the row the permutation gives it.
    for e in (100..110u64).chain(200..208) {
        let entity = EntityId::new(e);
        match permutation.row_of(entity) {
            Some(row) => assert_eq!(
                table.entity_of(row),
                Some(entity),
                "entity {e} holds row {} in the permutation but not in the table",
                row.raw()
            ),
            None => assert!(
                dead.contains(e as u32),
                "entity {e} lost its row without being tombstoned"
            ),
        }
    }
}
