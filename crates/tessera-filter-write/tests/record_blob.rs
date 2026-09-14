//! The record blob's write half against its read half (`records-and-search.md` §3): whole-row
//! blocks, has-row rank addressing, and — most of the file — the fail-closed refusals of review
//! B6. The corruption cases doctor the artefact on disk and assert the reader refuses with the
//! typed error rather than serving a neighbour's row, because that substitution is the one
//! failure a file digest cannot catch.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use croaring::{Bitmap, Portable};

use tessera_filter::{
    Access, RecordBlob, RecordError, RecordField, RecordFieldRef, RecordValue, RECORD_BLOCKS_FILE,
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
/// row is exactly 23 bytes — a one-byte length and 11 + 11 of fields — and block cutting is
/// arithmetic the test can state.
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

const ROW_BYTES: usize = 23;

/// Owned fields as the writer takes them.
fn borrowed(fields: &[RecordField]) -> Vec<RecordFieldRef<'_>> {
    fields
        .iter()
        .map(|f| RecordFieldRef {
            tag: f.tag,
            value: f.value.as_ref().expect("the fixture carries no list"),
        })
        .collect()
}

/// One owned row, pushed.
fn push(writer: &mut RecordBlobWriter, entity: u32, fields: &[RecordField]) -> std::io::Result<()> {
    writer.push_row(entity, &borrowed(fields))
}

/// Entities deliberately not dense from zero: rank is not entity, and a reader that conflated
/// them would fail here first.
fn entity_of_rank(rank: u32) -> u32 {
    rank * 7 + 3
}

fn write_fixture(dir: &Path, rows: u32, target: usize) -> Paths {
    std::fs::create_dir_all(dir).expect("the fixture's directory");
    let p = paths(dir);
    let mut writer = RecordBlobWriter::create(&p.blocks, &p.hasrow, &p.directory, target)
        .expect("create the writer");
    for rank in 0..rows {
        let entity = entity_of_rank(rank);
        push(&mut writer, entity, &fields_for(entity)).expect("push a row");
    }
    writer.finish().expect("finish");
    p
}

/// Mixed-type rows round-trip through real blocks, and the block boundaries hold: with 30-byte
/// rows and a 90-byte target the writer must cut 3 rows per block, and the first and last row of
/// **A blob opened for a sequential walk alone reads its rows and refuses everything the bitmap
/// answers.** The build's extent readers open this way and walk the rows; the five members that
/// take their answer from the has-row bitmap — including `self_check`, whose whole contract is
/// that it checks everything — must say so rather than answer a narrower question.
#[test]
fn a_rows_only_blob_walks_its_rows_and_refuses_the_bitmap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 10, 3 * ROW_BYTES);
    let blob = RecordBlob::open_rows_only(&p.blocks, &p.directory, Access::Read)
        .expect("the blob opens without its has-row file");

    // The directory's own arithmetic still stands, and the walk still yields every row with the
    // entity the block header and the gaps give it.
    assert_eq!(blob.block_count(), 4, "10 rows at 3 per block");
    assert_eq!(blob.rows(), 10, "the row count comes from the directory");
    let mut walked = Vec::new();
    blob.for_each_row(&mut |entity, fields| {
        assert_eq!(fields, fields_for(entity));
        walked.push(entity);
        Ok(())
    })
    .expect("the rows walk");
    assert_eq!(
        walked,
        (0..10).map(entity_of_rank).collect::<Vec<_>>(),
        "every row, in entity order, from the blocks alone"
    );

    // And the five that need the bitmap refuse rather than answer.
    let refuses = |what: &str, e: Option<RecordError>| {
        let message = format!("{}", e.expect(what));
        assert!(
            message.contains("opened for a sequential walk alone"),
            "{what} should name the open that has no bitmap, got: {message}"
        );
    };
    refuses("hasrow refuses", blob.hasrow().err());
    refuses("has_row refuses", blob.has_row(0).err());
    refuses("fields_of refuses", blob.fields_of(0).err());
    refuses(
        "for_each_row_in refuses",
        blob.for_each_row_in(&Bitmap::new(), &mut |_, _| Ok(()))
            .err(),
    );
    refuses("self_check refuses", blob.self_check().err());
}

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

/// **A blob of many blocks is the same bytes every time it is written.** Sealed blocks compress
/// on a small pool and are written back in seal order, so which worker finishes first must not
/// reach `blocks.bin`. Two hundred blocks is enough for the workers to interleave; the two runs
/// agree byte for byte and both read back as the rows that went in.
///
/// Mutation killed: writing a block as its compressor returns it rather than in seal order.
#[test]
fn many_blocks_compress_to_the_same_bytes_every_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rows = 200 * 4;
    let first = write_fixture(&dir.path().join("a"), rows, 4 * ROW_BYTES);
    let second = write_fixture(&dir.path().join("b"), rows, 4 * ROW_BYTES);
    for (a, b) in [
        (&first.blocks, &second.blocks),
        (&first.hasrow, &second.hasrow),
        (&first.directory, &second.directory),
    ] {
        assert_eq!(
            std::fs::read(a).expect("read"),
            std::fs::read(b).expect("read"),
            "two runs of the same rows wrote different bytes"
        );
    }
    let blob = open(&first).expect("opens");
    assert_eq!(blob.block_count(), 200, "{rows} rows at 4 per block");
    blob.self_check().expect("the artefact is self-consistent");
    for rank in 0..rows {
        let entity = entity_of_rank(rank);
        assert_eq!(
            blob.fields_of(entity).expect("read"),
            Some(fields_for(entity)),
            "rank {rank}"
        );
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
    push(&mut writer, 1, &fields_for(1)).expect("a small row");
    writer
        .push_row(2, &borrowed(std::slice::from_ref(&huge)))
        .expect("the oversize row");
    push(&mut writer, 3, &fields_for(3)).expect("another small row");
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
    assert!(!blob
        .has_row(4)
        .expect("a blob opened whole holds its has-row bitmap"));
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
    push(&mut writer, 5, &fields_for(5)).expect("in order");
    assert!(push(&mut writer, 5, &fields_for(5)).is_err(), "a repeat");
    assert!(
        push(&mut writer, 4, &fields_for(4)).is_err(),
        "a regression"
    );
}

/// The directory's five columns, lifted into plain vectors, doctored, and written back — the
/// corruption a file digest would catch in a bundle but the reader must also refuse on its own.
struct Directory {
    compressed_offset: Vec<u64>,
    compressed_len: Vec<u64>,
    uncompressed_len: Vec<u32>,
    first_rank: Vec<u32>,
    row_count: Vec<u32>,
}

fn read_directory(path: &Path) -> Directory {
    let reader = arrow::ipc::reader::FileReader::try_new(File::open(path).expect("open"), None)
        .expect("a readable directory");
    let mut out = Directory {
        compressed_offset: Vec::new(),
        compressed_len: Vec::new(),
        uncompressed_len: Vec::new(),
        first_rank: Vec::new(),
        row_count: Vec::new(),
    };
    for batch in reader {
        let batch = batch.expect("batch");
        let co: &UInt64Array = batch.column(0).as_any().downcast_ref().expect("u64");
        let cl: &UInt64Array = batch.column(1).as_any().downcast_ref().expect("u64");
        let ul: &UInt32Array = batch.column(2).as_any().downcast_ref().expect("u32");
        let fr: &UInt32Array = batch.column(3).as_any().downcast_ref().expect("u32");
        let rc: &UInt32Array = batch.column(4).as_any().downcast_ref().expect("u32");
        for i in 0..batch.num_rows() {
            out.compressed_offset.push(co.value(i));
            out.compressed_len.push(cl.value(i));
            out.uncompressed_len.push(ul.value(i));
            out.first_rank.push(fr.value(i));
            out.row_count.push(rc.value(i));
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
        Field::new("row_count", DataType::UInt32, false),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(dir.compressed_offset.clone())),
        Arc::new(UInt64Array::from(dir.compressed_len.clone())),
        Arc::new(UInt32Array::from(dir.uncompressed_len.clone())),
        Arc::new(UInt32Array::from(dir.first_rank.clone())),
        Arc::new(UInt32Array::from(dir.row_count.clone())),
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

/// One block's uncompressed bytes, mutated and written back with the directory corrected — the
/// corruption a digest over `blocks.bin` would catch in a bundle, but which the reader must also
/// refuse on its own.
fn doctor_block(p: &Paths, block: usize, doctor: impl FnOnce(&mut Vec<u8>)) {
    let mut dir = read_directory(&p.directory);
    let bytes = std::fs::read(&p.blocks).expect("read blocks");
    let at = dir.compressed_offset[block] as usize;
    let len = dir.compressed_len[block] as usize;
    let mut plain = zstd::bulk::decompress(
        &bytes[at..at + len],
        dir.uncompressed_len[block] as usize * 4 + 1024,
    )
    .expect("decompress");
    doctor(&mut plain);
    let recompressed = zstd::bulk::compress(&plain, 3).expect("compress");

    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..at]);
    out.extend_from_slice(&recompressed);
    out.extend_from_slice(&bytes[at + len..]);
    std::fs::write(&p.blocks, &out).expect("write blocks");

    dir.compressed_len[block] = recompressed.len() as u64;
    dir.uncompressed_len[block] = plain.len() as u32;
    let mut cursor = 0u64;
    for i in 0..dir.compressed_offset.len() {
        dir.compressed_offset[i] = cursor;
        cursor += dir.compressed_len[i];
    }
    write_directory(&p.directory, &dir);
}

/// **The B6 case, caught before a row is framed.** A row length doctored to swallow its
/// neighbour would put the row after next under this entity's identity — the substitution the
/// format exists to refuse. The rows of a block must tile it exactly, which is checked when the
/// block is decompressed, so the redirected read never reaches the row it was pointed at.
///
/// Mutation killed: dropping the tiling walk from `header_of`, after which rank 1 answers with
/// rank 2's fields.
#[test]
fn a_row_length_that_swallows_its_neighbour_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    // Row 0's length covers its own fields and the whole of row 1, so the walk would take row 2's
    // bytes as rank 1's row. Every row is the same width, so the arithmetic is the test's.
    doctor_block(&p, 0, |block| {
        let rows_at = block.len() - 3 * ROW_BYTES;
        assert_eq!(block[rows_at], (ROW_BYTES - 1) as u8, "row 0's length");
        block[rows_at] = (2 * ROW_BYTES - 1) as u8;
    });

    let blob = open(&p).expect("the doctored block still opens; the defect is inside it");
    let err = blob
        .fields_of(entity_of_rank(1))
        .expect_err("a redirected row refuses");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    // The whole block refuses, which is fail-closed's direction: what no read may do is answer
    // with anything but its own entity's fields.
    assert!(blob.fields_of(entity_of_rank(0)).is_err());
    assert!(blob.self_check().is_err());
}

/// **A wrong rank inside the right block.** A has-row bitmap that names a different entity at a
/// rank — the same cardinality, the same block, the same offsets, so nothing the directory or the
/// digest can see — must refuse on the block's own statement of identity. This is the failure the
/// per-row entity used to catch and the block's entity gaps catch now.
///
/// Mutation killed: dropping the `entity_at` comparison in `row_at`, or deriving the expected
/// entity from the bitmap instead of from the block, serves entity 10's row to entity 11.
#[test]
fn a_hasrow_naming_another_entity_at_a_rank_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    let bytes = std::fs::read(&p.hasrow).expect("read");
    let mut bitmap = Bitmap::try_deserialize::<Portable>(&bytes).expect("portable");
    // Rank 0 keeps its member, so the block's first-entity check still agrees; rank 1 does not.
    assert!(bitmap.remove_checked(entity_of_rank(1)), "rank 1 was there");
    bitmap.add(entity_of_rank(1) + 1);
    std::fs::write(&p.hasrow, bitmap.serialize::<Portable>()).expect("doctor");

    let blob = open(&p).expect("cardinality is unchanged, so the open-time checks pass");
    let err = blob
        .fields_of(entity_of_rank(1) + 1)
        .expect_err("the row at that rank belongs to another entity");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(err.to_string().contains("belongs to entity"), "{err}");
    // Rank 0 is unaffected and still answers as itself.
    assert_eq!(
        blob.fields_of(entity_of_rank(0)).expect("read"),
        Some(fields_for(entity_of_rank(0)))
    );
    // The walk finds it too, from the other direction: the bitmap's member and the block's gap
    // disagree at that rank.
    assert!(blob.self_check().is_err());
}

/// **A has-row bitmap that renames a block's first entity.** Removing rank 0's member and adding
/// one that sorts below rank 1's leaves every later rank naming the entity it named before, so no
/// row read past the first would notice. The block states its first entity, and the bitmap's
/// rank-0 member must be it.
///
/// Mutation killed: dropping the `first_entity` comparison in `header_of` — after which a read of
/// rank 1 answers normally out of a blob whose rank space has shifted under it.
#[test]
fn a_hasrow_renaming_a_blocks_first_entity_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    let bytes = std::fs::read(&p.hasrow).expect("read");
    let mut bitmap = Bitmap::try_deserialize::<Portable>(&bytes).expect("portable");
    assert!(bitmap.remove_checked(entity_of_rank(0)), "rank 0 was there");
    bitmap.add(entity_of_rank(0) + 1);
    assert!(
        entity_of_rank(0) + 1 < entity_of_rank(1),
        "rank 1 is unmoved"
    );
    std::fs::write(&p.hasrow, bitmap.serialize::<Portable>()).expect("doctor");

    let blob = open(&p).expect("cardinality is unchanged, so the open-time checks pass");
    // Rank 1 still names entity 10 and its row still belongs to entity 10; only the block's
    // first entity disagrees, and that is what must refuse.
    let err = blob
        .fields_of(entity_of_rank(1))
        .expect_err("the bitmap and the block disagree about rank 0");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(err.to_string().contains("first entity"), "{err}");
    assert!(blob.self_check().is_err());
}

/// **A wrong block.** Two blocks' bytes swapped in `blocks.bin`, their compressed lengths kept, so
/// every open-time check still passes: the block a rank resolves to now holds another block's
/// rows. The block states its own first rank, so the read refuses rather than answering out of
/// bytes that belong to other entities.
///
/// Mutation killed: dropping the `first_rank` comparison in `header_of`.
#[test]
fn a_block_holding_another_blocks_rows_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Two blocks of three rows each, written identically so their compressed frames are the same
    // length and the directory still tiles after the swap.
    let p = write_fixture(dir.path(), 6, 3 * ROW_BYTES);
    let blob = open(&p).expect("opens");
    assert_eq!(blob.block_count(), 2, "6 rows at 3 per block");
    let (lo_len, hi_len) = {
        let d = read_directory(&p.directory);
        (d.compressed_len[0] as usize, d.compressed_len[1] as usize)
    };
    let bytes = std::fs::read(&p.blocks).expect("read");
    let mut swapped = Vec::with_capacity(bytes.len());
    swapped.extend_from_slice(&bytes[lo_len..lo_len + hi_len]);
    swapped.extend_from_slice(&bytes[..lo_len]);
    std::fs::write(&p.blocks, &swapped).expect("swap");
    doctor_directory(&p.directory, |d| {
        d.compressed_len.swap(0, 1);
        d.uncompressed_len.swap(0, 1);
        d.row_count.swap(0, 1);
        d.compressed_offset[1] = d.compressed_len[0];
    });

    let blob = open(&p).expect("the directory still tiles blocks.bin exactly");
    let err = blob
        .fields_of(entity_of_rank(0))
        .expect_err("block 0 now holds block 1's rows");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(err.to_string().contains("first rank"), "{err}");
    assert!(blob.self_check().is_err());
}

/// **A block that states the wrong row count.** The header's first word alone is doctored, the
/// gaps and the rows left as they were, so nothing about the block's bytes has moved and only the
/// count the block claims disagrees with the count the directory's first ranks imply. The block is
/// refused before a row is framed: the count is what bounds the walk, so a block read against the
/// wrong one would stop short of its own rows or run past them.
///
/// Mutation killed: dropping the `row_count` comparison in `header_of`.
#[test]
fn a_block_that_states_the_wrong_row_count_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    doctor_block(&p, 0, |block| {
        assert_eq!(
            u32::from_le_bytes(block[..4].try_into().expect("four bytes")),
            3,
            "the block holds three rows"
        );
        block[..4].copy_from_slice(&2u32.to_le_bytes());
    });

    let blob = open(&p).expect("opens; the directory and the bitmap are untouched");
    let err = blob
        .fields_of(entity_of_rank(0))
        .expect_err("the block and the directory count different rows");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(
        err.to_string().contains("rows where the directory"),
        "{err}"
    );
    assert!(blob.self_check().is_err());
}

/// **A block that does not tile.** A byte appended to a block's rows section: every row is still
/// as long as it says, but the section is a byte longer than the rows account for. The block's
/// rows must end where the block does, so it refuses before a row is framed.
#[test]
fn a_block_longer_than_its_rows_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    doctor_block(&p, 0, |block| block.push(0));

    let blob = open(&p).expect("opens; the defect is inside the block");
    let err = blob
        .fields_of(entity_of_rank(2))
        .expect_err("the rows do not account for the block");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(err.to_string().contains("whole rows"), "{err}");
    assert!(blob.self_check().is_err());
}

/// **A row that does not fill its extent.** One row's string length shortened by a byte, which
/// moves neither the row's own length nor the section's, so the block still tiles. The field walk
/// has to consume the row exactly, and a byte left over is what refuses.
///
/// Mutation killed: ending the field loop while fewer than three bytes remain, which would
/// tolerate a trailing byte and serve the short row as if it were whole.
#[test]
fn a_row_that_does_not_fill_its_extent_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    // Row 1 is [len][tag 0, kind u64, 8 bytes][tag 1, kind utf8, len u32, 4 bytes]; the string's
    // length prefix sits 23 + 1 + 3 + 8 + 3 bytes into the rows section.
    doctor_block(&p, 0, |block| {
        let rows_at = block.len() - 3 * ROW_BYTES;
        let len_at = rows_at + ROW_BYTES + 1 + 3 + 8 + 3;
        assert_eq!(block[len_at], 4, "the string length prefix");
        block[len_at] = 3;
    });

    let blob = open(&p).expect("opens; the row lengths and the section length are untouched");
    let err = blob
        .fields_of(entity_of_rank(1))
        .expect_err("a row that leaves a byte over refuses");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    // Its neighbours are unaffected: the defect is one row's, and the block still tiles.
    assert_eq!(
        blob.fields_of(entity_of_rank(2)).expect("read"),
        Some(fields_for(entity_of_rank(2)))
    );
    assert!(blob.self_check().is_err());
}

/// **A truncated block.** A block's rows section cut short: the directory's last offsets now fall
/// outside it. The bounds check refuses before any byte is framed.
#[test]
fn a_block_cut_short_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    doctor_block(&p, 0, |block| {
        block.truncate(block.len() - 5);
    });

    let blob = open(&p).expect("opens; the defect is inside the block");
    let err = blob
        .fields_of(entity_of_rank(2))
        .expect_err("the last row runs past the block");
    assert!(matches!(err, RecordError::Malformed(_)), "{err}");
    assert!(blob.self_check().is_err());
}

/// A row length running past its block's bytes refuses by bounds, not by reading whatever lies
/// there.
#[test]
fn a_row_length_past_the_block_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = write_fixture(dir.path(), 3, 1024);
    // The last row's length raised to a two-byte varint far past the block. Every length in this
    // fixture is one byte, so raising one widens the row and the block stops tiling either way;
    // what this asserts is that the bounds refuse rather than a slice panicking.
    doctor_block(&p, 0, |block| {
        let rows_at = block.len() - 3 * ROW_BYTES;
        let last = rows_at + 2 * ROW_BYTES;
        block[last] = 0xd0;
        block.insert(last + 1, 0x0f);
    });
    let blob = open(&p).expect("opens; the defect is inside the block");
    let err = blob
        .fields_of(entity_of_rank(2))
        .expect_err("a length past the block refuses");
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
            push(&mut writer, e, &fields_for(e)).expect("push");
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

    let stack = RecordStack::open(Some(&base), &[extent_a.clone(), extent_b], Access::Mapped)
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
    assert!(
        refused.is_err(),
        "a truncated extent refuses the whole stack"
    );
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
