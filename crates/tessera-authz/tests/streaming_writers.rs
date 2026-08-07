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

/// **`encode_posting_bitmap` and `encode_posting` agree byte for byte, across the tag boundary.**
///
/// The bitmap encoder is compaction's pass 2 primitive: the fold's term sweep works in Roaring
/// throughout (union the tiers, subtract the tombstones) and must not materialise a `Vec<u32>` to
/// encode the result — the widest term is a measured 125.12 MB as portable Roaring against 2 GB as
/// `u32`s at 10⁹, per term, on a pass that visits every term in the dictionary.
///
/// Being a *second producer* rather than a second format is the whole property, so this sweeps the
/// tag rule's boundary explicitly: cardinalities either side of `small_term_threshold` take
/// different arms (tag 0 raw `u32` LEs, tag 1 run-optimised portable Roaring), and both arms must
/// match. The empty set and the singleton are included because they are the cases where a
/// cardinality comparison is easiest to get off by one.
///
/// **Mutation:** drop the `run_optimize` from the bitmap arm, or compare `<` rather than `<=`
/// against the threshold, and the tag-1 or boundary cases stop matching.
#[test]
fn the_bitmap_and_slice_encoders_agree_byte_for_byte() {
    use croaring::Bitmap;
    use tessera_authz::encode_posting_bitmap;

    const THRESHOLD: u32 = 8;

    // Sets chosen around the threshold, plus a run-heavy one (where `run_optimize` actually fires)
    // and a scattered one (where it does not).
    let cases: Vec<Vec<u32>> = vec![
        vec![],
        vec![7],
        (0..THRESHOLD).collect(),     // exactly at the threshold: tag 0
        (0..THRESHOLD + 1).collect(), // one past it: tag 1
        (0..5_000u32).collect(),      // one long run
        (0..5_000u32).map(|i| i * 977).collect(), // scattered, no runs
        vec![0, u32::MAX / 2, u32::MAX - 1], // sparse across the whole space
    ];

    for entities in cases {
        let from_slice = encode_posting(0, &entities, THRESHOLD)
            .expect("the slice encoder accepts sorted input");
        let from_bitmap = encode_posting_bitmap(&Bitmap::of(&entities), THRESHOLD)
            .expect("the bitmap encoder accepts any bitmap");
        assert_eq!(
            from_slice,
            from_bitmap,
            "the two encoders disagree at cardinality {} (tag {})",
            entities.len(),
            from_slice.first().copied().unwrap_or(255)
        );
    }
}
