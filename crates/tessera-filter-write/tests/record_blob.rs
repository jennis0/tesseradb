//! The record blob's write half against its read half (`records-and-search.md` §3): whole-row
//! blocks, has-row rank addressing, and — most of the file — the fail-closed refusals of review
//! B6. The corruption cases doctor the artefact on disk and assert the reader refuses with the
//! typed error rather than serving a neighbour's row, because that substitution is the one
//! failure the digest cannot catch.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, LargeListArray, UInt32Array, UInt64Array};
use arrow::buffer::{OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use croaring::{Bitmap, Portable};

use tessera_filter::{
    Access, RecordBlob, RecordError, RecordField, RecordValue, RECORD_BLOCKS_FILE,
    RECORD_DIRECTORY_FILE, RECORD_HASROW_FILE,
};
use tessera_filter_write::RecordBlobWriter;

struct Paths {
    blocks: PathBuf,
    hasrow: PathBuf,
    directory: PathBuf,
}

fn paths(dir: &Path) -> Paths {
    Paths {
        blocks: dir.join(RECORD_BLOCKS_FILE),
        hasrow: dir.join(RECORD_HASROW_FILE),
        directory: dir.join(RECORD_DIRECTORY_FILE),
    }
}

fn open(p: &Paths) -> Result<RecordBlob, RecordError> {
    RecordBlob::open(&p.blocks, &p.hasrow, &p.directory, Access::Read)
}

/// The fixture's generation function: entity `e` carries a `u64` and a four-byte string, so every
/// row is exactly 30 bytes — 8 header + 11 + 11 — and block cutting is arithmetic the test can
/// state.
fn fields_for(e: u32) -> Vec<RecordField> {
    vec![
        RecordField {
            tag: 0,
            value: RecordValue::U64(u64::from(e) * 3),
        },
        RecordField {
            tag: 1,
            value: RecordValue::Utf8(format!("{:04}", e % 10_000)),
        },
    ]
}

const ROW_BYTES: usize = 30;

/// Entities deliberately not dense from zero: rank is not entity, and a reader that conflated
/// them would fail here first.
fn entity_of_rank(rank: u32) -> u32 {
    rank * 7 + 3
}

fn write_fixture(dir: &Path, rows: u32, target: usize) -> Paths {
    let p = paths(dir);
    let mut writer = RecordBlobWriter::create(&p.blocks, &p.hasrow, &p.directory, target)
        .expect("create the writer");
    for rank in 0..rows {
        let entity = entity_of_rank(rank);
        writer
            .push_row(entity, &fields_for(entity))
            .expect("push a row");
    }
    writer.finish().expect("finish");
    p
}

/// Mixed-type rows round-trip through real blocks, and the block boundaries hold: with 30-byte
/// rows and a 90-byte target the writer must cut 3 rows per block, and the first and last row of
/// every block — the B6 catalogue's boundary cases — read back as their own entities.
#[test]
fn rows_round_trip_across_block_boundaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 10, 3 * ROW_BYTES);
    let blob = open(&p).expect("the blob opens");
    assert_eq!(blob.block_count(), 4, "10 rows at 3 per block");
    assert_eq!(blob.rows(), 10);
    blob.self_check().expect("the artefact is self-consistent");

    for rank in 0..10 {
        let entity = entity_of_rank(rank);
        let fields = blob
            .fields_of(entity)
            .expect("a well-formed read")
            .expect("the entity has a row");
        assert_eq!(fields, fields_for(entity), "rank {rank}");
    }
    // The boundary ranks by the cutting arithmetic: first and last of each of the four blocks.
    for boundary_rank in [0u32, 2, 3, 5, 6, 8, 9] {
        let entity = entity_of_rank(boundary_rank);
        assert!(blob.fields_of(entity).expect("read").is_some());
    }
}

/// A row larger than the target gets an oversized block of its own — the target is a target, not
/// a cap (records §3) — and its neighbours still read back from their own blocks.
#[test]
fn an_oversize_row_gets_a_block_of_its_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let mut writer =
        RecordBlobWriter::create(&p.blocks, &p.hasrow, &p.directory, 64).expect("create");
    let huge = RecordField {
        tag: 0,
        value: RecordValue::Utf8("x".repeat(500)),
    };
    writer.push_row(1, &fields_for(1)).expect("a small row");
    writer
        .push_row(2, std::slice::from_ref(&huge))
        .expect("the oversize row");
    writer
        .push_row(3, &fields_for(3))
        .expect("another small row");
    writer.finish().expect("finish");

    let blob = open(&p).expect("the blob opens");
    assert_eq!(
        blob.block_count(),
        3,
        "small / oversized / small — the oversize row shares with nobody"
    );
    blob.self_check().expect("self-consistent");
    assert_eq!(blob.fields_of(2).expect("read"), Some(vec![huge]));
    assert_eq!(blob.fields_of(1).expect("read"), Some(fields_for(1)));
    assert_eq!(blob.fields_of(3).expect("read"), Some(fields_for(3)));
}

/// An entity with no blob-resident value is absent from has-row and answers `None` — a legitimate
/// absence, distinct from every refusal in this file.
#[test]
fn an_entity_with_no_row_answers_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 4, 1024);
    let blob = open(&p).expect("the blob opens");
    // Entity 4 sits between ranks 0 (entity 3) and 1 (entity 10) and has no row.
    assert_eq!(blob.fields_of(4).expect("a clean read"), None);
    assert!(!blob.has_row(4));
}

/// A blob with declared columns but no values anywhere is three well-formed files, not an
/// absence: the open rule demands them whenever the schema has a blob-resident column.
#[test]
fn an_empty_blob_opens_and_answers_absence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    RecordBlobWriter::create(&p.blocks, &p.hasrow, &p.directory, 1024)
        .expect("create")
        .finish()
        .expect("finish with no rows");
    let blob = open(&p).expect("an empty blob opens");
    assert_eq!(blob.block_count(), 0);
    assert_eq!(blob.rows(), 0);
    blob.self_check().expect("self-consistent");
    assert_eq!(blob.fields_of(0).expect("read"), None);
}

/// Rows arrive in strictly ascending entity order; a repeat or a regression refuses rather than
/// sorts, because a sort would paper over a broken producer (I9).
#[test]
fn out_of_order_rows_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let mut writer =
        RecordBlobWriter::create(&p.blocks, &p.hasrow, &p.directory, 1024).expect("create");
    writer.push_row(5, &fields_for(5)).expect("in order");
    assert!(writer.push_row(5, &fields_for(5)).is_err(), "a repeat");
    assert!(writer.push_row(4, &fields_for(4)).is_err(), "a regression");
}

/// The directory's five columns, lifted into plain vectors, doctored, and written back — the
/// corruption a digest would catch in a bundle but the reader must also refuse on its own.
struct Directory {
    compressed_offset: Vec<u64>,
    compressed_len: Vec<u64>,
    uncompressed_len: Vec<u32>,
    first_rank: Vec<u32>,
    row_offsets: Vec<Vec<u32>>,
}

fn read_directory(path: &Path) -> Directory {
    let reader = arrow::ipc::reader::FileReader::try_new(File::open(path).expect("open"), None)
        .expect("a readable directory");
    let mut out = Directory {
        compressed_offset: Vec::new(),
        compressed_len: Vec::new(),
        uncompressed_len: Vec::new(),
        first_rank: Vec::new(),
        row_offsets: Vec::new(),
    };
    for batch in reader {
        let batch = batch.expect("batch");
        let co: &UInt64Array = batch.column(0).as_any().downcast_ref().expect("u64");
        let cl: &UInt64Array = batch.column(1).as_any().downcast_ref().expect("u64");
        let ul: &UInt32Array = batch.column(2).as_any().downcast_ref().expect("u32");
        let fr: &UInt32Array = batch.column(3).as_any().downcast_ref().expect("u32");
        let lists: &LargeListArray = batch.column(4).as_any().downcast_ref().expect("list");
        for i in 0..batch.num_rows() {
            out.compressed_offset.push(co.value(i));
            out.compressed_len.push(cl.value(i));
            out.uncompressed_len.push(ul.value(i));
            out.first_rank.push(fr.value(i));
            let row = lists.value(i);
            let row: &UInt32Array = row.as_any().downcast_ref().expect("u32 items");
            out.row_offsets.push(row.values().to_vec());
        }
    }
    out
}

fn write_directory(path: &Path, dir: &Directory) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("compressed_offset", DataType::UInt64, false),
        Field::new("compressed_len", DataType::UInt64, false),
        Field::new("uncompressed_len", DataType::UInt32, false),
        Field::new("first_rank", DataType::UInt32, false),
        Field::new(
            "row_offsets",
            DataType::LargeList(Arc::new(Field::new("item", DataType::UInt32, false))),
            false,
        ),
    ]));
    let mut offsets: Vec<i64> = vec![0];
    let mut flat: Vec<u32> = Vec::new();
    for block in &dir.row_offsets {
        flat.extend_from_slice(block);
        offsets.push(flat.len() as i64);
    }
    let lists = LargeListArray::new(
        Arc::new(Field::new("item", DataType::UInt32, false)),
        OffsetBuffer::new(ScalarBuffer::from(offsets)),
        Arc::new(UInt32Array::from(flat)),
        None,
    );
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(dir.compressed_offset.clone())),
        Arc::new(UInt64Array::from(dir.compressed_len.clone())),
        Arc::new(UInt32Array::from(dir.uncompressed_len.clone())),
        Arc::new(UInt32Array::from(dir.first_rank.clone())),
        Arc::new(lists),
    ];
    let batch = RecordBatch::try_new(schema.clone(), columns).expect("batch");
    let file = File::create(path).expect("create");
    let mut writer = arrow::ipc::writer::FileWriter::try_new(file, &schema).expect("writer");
    writer.write(&batch).expect("write");
    writer.finish().expect("finish");
}

fn doctor_directory(path: &Path, doctor: impl FnOnce(&mut Directory)) {
    let mut dir = read_directory(path);
    doctor(&mut dir);
    write_directory(path, &dir);
}

/// **The B6 case.** A directory offset redirected at another entity's row must refuse on the
/// discriminant — the typed error, never the neighbour's fields. This is the defect the digest
/// cannot catch when a build or fold writes a consistent-looking wrong directory.
#[test]
fn a_redirected_offset_refuses_and_never_serves_the_neighbour() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    // Rank 1 (entity 10) now points at rank 0's row (entity 3), inside the same block.
    doctor_directory(&p.directory, |d| d.row_offsets[0][1] = d.row_offsets[0][0]);

    let blob = open(&p).expect("the doctored directory still opens; the defect is per-row");
    let err = blob
        .fields_of(entity_of_rank(1))
        .expect_err("a redirected row refuses");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(err.to_string().contains("discriminant"), "{err}");
    // The neighbour whose row was stolen either answers as itself or refuses on the tiling check
    // — the directory is inconsistent, and refusing more than the minimum is fail-closed's
    // direction. What it must never do is answer with anything but its own fields.
    match blob.fields_of(entity_of_rank(0)) {
        Ok(fields) => assert_eq!(fields, Some(fields_for(entity_of_rank(0)))),
        Err(e) => assert!(matches!(e, RecordError::Malformed(_)), "{e}"),
    }
    // And the exhaustive check finds the inconsistency the single read found.
    assert!(blob.self_check().is_err());
}

/// An offset past its block's bytes refuses by bounds, not by reading whatever lies there.
#[test]
fn an_out_of_bounds_offset_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    doctor_directory(&p.directory, |d| d.row_offsets[0][2] = 60_000);
    let blob = open(&p).expect("opens; the defect is per-row");
    let err = blob
        .fields_of(entity_of_rank(2))
        .expect_err("an out-of-bounds offset refuses");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(blob.self_check().is_err());
}

/// A length the block's bytes contradict refuses at the decompress, whichever direction it lies.
#[test]
fn a_wrong_uncompressed_length_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    doctor_directory(&p.directory, |d| d.uncompressed_len[0] += 1);
    let blob = open(&p).expect("opens; the defect is per-block");
    let err = blob
        .fields_of(entity_of_rank(0))
        .expect_err("a wrong length refuses");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
}

/// A directory that disagrees with `blocks.bin`'s length — the file truncated, or carrying bytes
/// no block claims — refuses at open, before any request reads through it.
#[test]
fn a_truncated_or_padded_blocks_file_refuses_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 6, 64);

    let whole = std::fs::read(&p.blocks).expect("read blocks");
    std::fs::write(&p.blocks, &whole[..whole.len() - 1]).expect("truncate");
    let err = open(&p).expect_err("a short blocks.bin refuses at open");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");

    let mut padded = whole.clone();
    padded.push(0);
    std::fs::write(&p.blocks, &padded).expect("pad");
    let err = open(&p).expect_err("unclaimed bytes refuse at open");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");

    std::fs::write(&p.blocks, &whole).expect("restore");
    open(&p).expect("the restored file opens again");
}

/// A corrupt zstd frame refuses at the read — the block is unaddressable, not silently empty.
#[test]
fn a_corrupt_block_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    let mut bytes = std::fs::read(&p.blocks).expect("read");
    bytes[0] ^= 0xFF; // the frame's magic — every read of this block now fails
    std::fs::write(&p.blocks, &bytes).expect("corrupt");

    let blob = open(&p).expect("length checks pass; the corruption is inside the frame");
    let err = blob
        .fields_of(entity_of_rank(0))
        .expect_err("a corrupt block refuses");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(blob.self_check().is_err());
}

/// A has-row bitmap that disagrees with the directory's row count refuses at open: the rank
/// addressing would pair every later entity with the wrong row, so neither file is trusted alone.
#[test]
fn a_hasrow_directory_disagreement_refuses_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    let bytes = std::fs::read(&p.hasrow).expect("read");
    let mut bitmap = Bitmap::try_deserialize::<Portable>(&bytes).expect("portable");
    bitmap.add(1_000_000);
    std::fs::write(&p.hasrow, bitmap.serialize::<Portable>()).expect("doctor");
    let err = open(&p).expect_err("the disagreement refuses at open");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
}

/// A missing file refuses at open — records §7's rule, "never 'those entities have no record'".
#[test]
fn a_missing_file_refuses_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    std::fs::remove_file(&p.directory).expect("remove");
    assert!(open(&p).is_err());
}

/// The layered stack answers from whichever layer holds the entity, and disjointness (I9) is
/// what the construction rests on: a base and two extents with interleaved entity ranges each
/// answer their own rows, an entity in no layer is an ordinary absence, and a corrupt extent
/// refuses the whole stack rather than downgrading to the layers that opened.
#[test]
fn a_stack_of_disjoint_layers_answers_each_from_its_own() {
    use tessera_filter::{RecordExtentPaths, RecordStack};

    let dir = tempfile::tempdir().expect("tempdir");
    // Base: ranks 0..8 (entities 3,10,17,...). Extents: two flushes over higher entities that
    // interleave with each other but never with the base or one another.
    let base = dir.path().join("base");
    std::fs::create_dir_all(&base).expect("mkdir");
    write_fixture(&base, 8, 90);

    let write_extent = |name: &str, entities: &[u32]| -> RecordExtentPaths {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).expect("mkdir");
        let p = paths(&d);
        let mut writer =
            RecordBlobWriter::create(&p.blocks, &p.hasrow, &p.directory, 90).expect("create");
        for &e in entities {
            writer.push_row(e, &fields_for(e)).expect("push");
        }
        writer.finish().expect("finish");
        RecordExtentPaths {
            blocks: p.blocks,
            hasrow: p.hasrow,
            directory: p.directory,
        }
    };
    let extent_a = write_extent("flush-a", &[1_000, 1_004, 1_010]);
    let extent_b = write_extent("flush-b", &[1_001, 1_002, 1_020]);

    let stack = RecordStack::open(
        Some(&base),
        &[extent_a.clone(), extent_b],
        Access::Mapped,
    )
    .expect("open the stack");

    for entity in [3u32, 52, 1_000, 1_010, 1_001, 1_020] {
        let fields = stack
            .fields_of(entity)
            .expect("read")
            .unwrap_or_else(|| panic!("entity {entity} has a row in exactly one layer"));
        assert_eq!(fields, fields_for(entity), "entity {entity}");
    }
    assert_eq!(
        stack.fields_of(999).expect("read"),
        None,
        "an entity in no layer is an ordinary absence"
    );
    stack.self_check().expect("every layer self-checks");

    // A corrupt layer refuses the stack at open — fail-closed, never a downgrade.
    let bytes = std::fs::read(&extent_a.blocks).expect("read blocks");
    std::fs::write(&extent_a.blocks, &bytes[..bytes.len() - 1]).expect("truncate");
    let refused = RecordStack::open(Some(&base), &[extent_a], Access::Mapped);
    assert!(refused.is_err(), "a truncated extent refuses the whole stack");
}

/// **The set read and the single read agree, row for row, and the set read decompresses each block
/// once.** The whole point of `for_each_row_in` is that a caller wanting many rows stops paying a
/// decompress per row; the risk it introduces is an addressing one, since it holds a block's bytes
/// across several rows and could serve one row's offsets against another block's bytes.
///
/// Ten rows at three per block, so the wanted set spans block boundaries in both directions: it
/// asks for both rows of one block and skips a block entirely.
#[test]
fn a_set_read_agrees_with_the_single_reads_it_replaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 10, 3 * ROW_BYTES);
    let blob = open(&p).expect("the blob opens");
    assert_eq!(blob.block_count(), 4, "10 rows at 3 per block");

    // Ranks 0, 1 (block 0), 4 (block 1), 7 and 8 (block 2) — block 3 is skipped, and one entity
    // that has no row at all is asked for.
    let mut wanted = Bitmap::new();
    for rank in [0u32, 1, 4, 7, 8] {
        wanted.add(entity_of_rank(rank));
    }
    wanted.add(entity_of_rank(0) + 1);

    let mut got: Vec<(u32, Vec<RecordField>)> = Vec::new();
    blob.for_each_row_in(&wanted, &mut |entity, fields| {
        got.push((entity, fields));
        Ok(())
    })
    .expect("a well-formed set read");

    let want: Vec<(u32, Vec<RecordField>)> = [0u32, 1, 4, 7, 8]
        .iter()
        .map(|rank| {
            let entity = entity_of_rank(*rank);
            (entity, fields_for(entity))
        })
        .collect();
    assert_eq!(got, want, "the entity with no row is absent, not an error");

    // Asking for everything is the whole-blob walk, in the same order.
    let mut all = Vec::new();
    blob.for_each_row_in(&Bitmap::from_range(0..u32::MAX), &mut |entity, fields| {
        all.push((entity, fields));
        Ok(())
    })
    .expect("a well-formed set read");
    let mut streamed = Vec::new();
    blob.for_each_row(&mut |entity, fields| {
        streamed.push((entity, fields));
        Ok(())
    })
    .expect("the streaming walk");
    assert_eq!(all, streamed);
}
