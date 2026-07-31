//! Byte-identity tests for the streaming writers: `DictStreamWriter` against `DictWriter`, and
//! `PostingsSpool` against `write_posting_records`. Byte-for-byte equality is the contract —
//! the streaming writers replace the buffered ones in the build pipeline, and bundles must not
//! change by a byte.

use std::fs;
use tempfile::TempDir;
use tessera_authz::{
    encode_posting, write_posting_records, Dict, DictStreamWriter, DictWriter, PostingsSpool,
};

/// Distinct descriptors of varied lengths, including one empty descriptor. Distinctness is the
/// streaming writer's caller contract; without it `DictWriter` would deduplicate and the two
/// outputs would legitimately differ.
fn descriptors(n: usize) -> Vec<Vec<u8>> {
    let mut out = vec![Vec::new()];
    for i in 0..n.saturating_sub(1) {
        let mut d = format!("descriptor-{i:05}-").into_bytes();
        d.extend(std::iter::repeat_n(b'x', i % 13));
        out.push(d);
    }
    out
}

// DictWriter writes a single `terms-0.dict` extent unconditionally — there is no extent-split
// boundary to cover; if a split rule is ever introduced there, DictStreamWriter must replicate
// it and this test must gain a case that crosses the boundary.
#[test]
fn dict_stream_writer_matches_dict_writer_bytes() {
    let descriptors = descriptors(1000);

    let buffered_dir = TempDir::new().unwrap();
    let mut buffered = DictWriter::new(buffered_dir.path());
    for (i, d) in descriptors.iter().enumerate() {
        assert_eq!(buffered.intern(d).raw(), i as u32);
    }
    let buffered_paths = buffered.finish().unwrap();

    let streamed_dir = TempDir::new().unwrap();
    let mut streamed = DictStreamWriter::new(streamed_dir.path());
    for (i, d) in descriptors.iter().enumerate() {
        assert_eq!(streamed.append(d).raw(), i as u32);
    }
    assert_eq!(streamed.len(), descriptors.len() as u32);
    let streamed_paths = streamed.finish().unwrap();

    assert_eq!(buffered_paths.len(), 1);
    assert_eq!(streamed_paths.len(), 1);
    assert_eq!(
        streamed_paths[0].file_name(),
        buffered_paths[0].file_name(),
        "extent naming must match"
    );
    assert_eq!(
        fs::read(&streamed_paths[0]).unwrap(),
        fs::read(&buffered_paths[0]).unwrap(),
        "extent bytes must be identical"
    );

    // The streamed extent must load and resolve ids exactly as assigned.
    let dict = Dict::load(&streamed_paths).unwrap();
    assert_eq!(dict.len(), descriptors.len() as u32);
    assert_eq!(dict.lookup(&descriptors[7]).unwrap().raw(), 7);
}

#[test]
fn dict_stream_writer_matches_dict_writer_bytes_when_empty() {
    let buffered_dir = TempDir::new().unwrap();
    let buffered_paths = DictWriter::new(buffered_dir.path()).finish().unwrap();

    let streamed_dir = TempDir::new().unwrap();
    let streamed = DictStreamWriter::new(streamed_dir.path());
    assert!(streamed.is_empty());
    let streamed_paths = streamed.finish().unwrap();

    assert_eq!(
        fs::read(&streamed_paths[0]).unwrap(),
        fs::read(&buffered_paths[0]).unwrap(),
        "an empty dictionary must still write an identical (empty) extent"
    );
}

/// A few hundred encoded records straddling the tag-0/tag-1 threshold, including empty entity
/// lists (a one-byte, tag-only record).
fn posting_records(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|t| {
            let count = (t * 7) % 90;
            let entities: Vec<u32> = (0..count as u32).map(|k| k * 3 + t as u32).collect();
            encode_posting(t, &entities, 32).unwrap()
        })
        .collect()
}

fn assert_spool_matches_buffered(records: &[Vec<u8>]) {
    let temp = TempDir::new().unwrap();

    let buffered_path = temp.path().join("buffered.arrow");
    write_posting_records(&buffered_path, records).unwrap();

    let spool_path = temp.path().join("postings.spool");
    let streamed_path = temp.path().join("streamed.arrow");
    let mut spool = PostingsSpool::create(&spool_path).unwrap();
    for record in records {
        spool.append(record).unwrap();
    }
    spool.finish(&streamed_path).unwrap();

    assert!(
        !spool_path.exists(),
        "spool file must be deleted after a successful write"
    );
    assert_eq!(
        fs::read(&streamed_path).unwrap(),
        fs::read(&buffered_path).unwrap(),
        "postings.arrow bytes must be identical for {} record(s)",
        records.len()
    );
}

#[test]
fn postings_spool_matches_write_posting_records_bytes() {
    assert_spool_matches_buffered(&posting_records(300));
}

#[test]
fn postings_spool_matches_for_zero_records() {
    assert_spool_matches_buffered(&[]);
}

#[test]
fn postings_spool_matches_for_a_single_record() {
    assert_spool_matches_buffered(&posting_records(1));
}
