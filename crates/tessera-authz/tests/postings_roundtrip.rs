//! Round-trip test for CSR postings: tagged records in a single Arrow IPC file,
//! `posting.arrow` — record ordinal = term_id (contracts §2.4).

use tessera_authz::{write_postings, PostingRef, PostingsReader};
use tessera_types::TermId;

#[test]
fn round_trip_three_terms() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("postings.arrow");

    // Term 0: singleton -> tag 0 (small array)
    // Term 1: 1..=1000 -> tag 1 (roaring, exceeds default threshold of 32)
    // Term 2: empty -> tag 0, zero entries
    let singleton: Vec<u32> = vec![5];
    let large: Vec<u32> = (1..=1000u32).collect();
    let empty: Vec<u32> = vec![];

    let per_term = vec![singleton.clone(), large.clone(), empty.clone()];

    write_postings(&path, &per_term, 32).unwrap();

    let reader = PostingsReader::open(&path, false).unwrap();
    assert_eq!(reader.term_count(), 3);

    match reader
        .posting(TermId::new(0))
        .unwrap()
        .expect("the term is present in this file")
    {
        PostingRef::Array(bytes) => {
            assert_eq!(bytes.len(), 4, "one u32 LE entry");
            let v = u32::from_le_bytes(bytes.try_into().unwrap());
            assert_eq!(v, 5);
        }
        PostingRef::Roaring(_) => panic!("term 0 should be tag 0 (array)"),
    };

    match reader
        .posting(TermId::new(1))
        .unwrap()
        .expect("the term is present in this file")
    {
        PostingRef::Roaring(bm) => {
            let collected: Vec<u32> = bm.iter().collect();
            assert_eq!(collected, large);
        }
        PostingRef::Array(_) => panic!("term 1 should be tag 1 (roaring)"),
    };

    match reader
        .posting(TermId::new(2))
        .unwrap()
        .expect("the term is present in this file")
    {
        PostingRef::Array(bytes) => {
            assert_eq!(bytes.len(), 0, "empty term has zero entries");
        }
        PostingRef::Roaring(_) => panic!("term 2 should be tag 0 (array)"),
    };

    // Byte-level cross-check via an independent Arrow reader (not `PostingsReader`): the
    // singleton (tag 0) record must be exactly tag ‖ u32 LE 5, and record 1's first byte must
    // be the tag-1 marker.
    let file = std::fs::File::open(&path).unwrap();
    let mut arrow_reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
    let batch = arrow_reader.next().unwrap().unwrap();
    let array = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::LargeBinaryArray>()
        .unwrap();
    assert_eq!(
        array.value(0),
        &[0u8, 5, 0, 0, 0],
        "term 0: tag 0 ‖ u32 LE 5"
    );
    assert_eq!(array.value(1)[0], 1u8, "term 1: tag byte must be 1");
}

#[test]
fn open_with_mmap() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("postings.arrow");
    let per_term = vec![vec![1u32, 2, 3], (1..=50u32).collect::<Vec<u32>>()];
    write_postings(&path, &per_term, 32).unwrap();

    let reader = PostingsReader::open(&path, true).unwrap();
    assert_eq!(reader.term_count(), 2);
    match reader
        .posting(TermId::new(0))
        .unwrap()
        .expect("the term is present in this file")
    {
        PostingRef::Array(bytes) => assert_eq!(bytes.len(), 12),
        PostingRef::Roaring(_) => panic!("expected array"),
    };
    match reader
        .posting(TermId::new(1))
        .unwrap()
        .expect("the term is present in this file")
    {
        PostingRef::Roaring(bm) => assert_eq!(bm.cardinality(), 50),
        PostingRef::Array(_) => panic!("expected roaring"),
    };
}
