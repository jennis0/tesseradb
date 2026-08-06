//! `PairsParquetWriter::push_iter` and the wiring between compaction's term sweep
//! (`tessera_authz::term_sweep`, pass 2) and `terms/pairs.parquet` (contracts §2.4).
//!
//! The sweep itself — the union, the subtraction, the tag boundary — is `tessera-authz`'s to test
//! (`crates/tessera-authz/tests/term_sweep.rs`); this file covers only what could not be tested
//! there: the two things that live on this side of the crate boundary because this crate holds the
//! Parquet writer and `tessera-authz` may not depend on it (`scripts/check-layers.sh`).

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, UInt32Array, UInt64Array};
use arrow::record_batch::RecordBatch;
use croaring::Bitmap;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use tessera_authz::{
    sweep_term_postings, write_delta_tier, write_postings, DeltaTier, PostingRef, PostingsReader,
    PostingsSpool,
};
use tessera_store::PairsParquetWriter;
use tessera_types::TermId;

const THRESHOLD: u32 = 4;

fn read_parquet(path: &Path) -> Vec<RecordBatch> {
    ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap()
        .map(|b| b.unwrap())
        .collect()
}

/// `(term_id, entity_id)` rows from a `pairs.parquet`-shaped file, in file order.
fn read_pairs(path: &Path) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    for batch in read_parquet(path) {
        let entities = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let terms = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            out.push((terms.value(i), entities.value(i)));
        }
    }
    out
}

fn entities_of(posting: PostingRef<'_>) -> Vec<u32> {
    match posting {
        PostingRef::Roaring(view) => view.iter().collect(),
        PostingRef::Array(bytes) => bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    }
}

/// **End to end: the term sweep drives `PairsParquetWriter` through `push_iter`, and the resulting
/// `pairs.parquet` agrees with the swept `postings.arrow` about every folded deletion** — the
/// "done when" property named directly in issue #76, checked against a real Parquet file rather
/// than the sweep's in-memory callback argument.
///
/// Term 0 is base-only; term 1 is absent from the base file and carried only by a tier (so
/// `base.posting(1)` answers `None`, not empty) — the two categories that read through different
/// branches inside the sweep. Entity 3 is tombstoned out of term 0.
///
/// **Mutation:** feed `push_iter` the pre-subtraction union instead of the sweep's own
/// post-subtraction bitmap, and entity 3 would still appear in `pairs.parquet` while it is already
/// gone from `postings.arrow` — exactly the disagreement the I1 mask differential exists to catch.
#[test]
fn pairs_parquet_agrees_with_swept_postings_about_a_folded_deletion() {
    let dir = tempfile::TempDir::new().unwrap();

    let base_path = dir.path().join("base-postings.arrow");
    write_postings(&base_path, &[vec![1, 2, 3, 4, 5]], THRESHOLD).unwrap();
    let base = PostingsReader::open(&base_path, false).unwrap();

    let tier_path = dir.path().join("tier.arrow");
    write_delta_tier(&tier_path, &[(TermId::new(1), vec![10, 11, 12])], THRESHOLD).unwrap();
    let tier = Arc::new(DeltaTier::open(&tier_path).unwrap());

    let tombstones = Bitmap::of(&[3]);

    let spool_path = dir.path().join("sweep.spool");
    let postings_path = dir.path().join("postings.arrow");
    let pairs_path = dir.path().join("pairs.parquet");

    let mut spool = PostingsSpool::create(&spool_path).unwrap();
    let mut pairs_writer = PairsParquetWriter::create(&pairs_path).unwrap();
    sweep_term_postings(
        2,
        &base,
        &[tier],
        &tombstones,
        THRESHOLD,
        &mut spool,
        |term, bitmap| {
            pairs_writer
                .push_iter(term.raw(), bitmap.iter())
                .map_err(|e| std::io::Error::other(e.to_string()))
        },
    )
    .unwrap();
    spool.finish(&postings_path).unwrap();
    pairs_writer.finish().unwrap();

    let reader = PostingsReader::open(&postings_path, false).unwrap();
    let posting_entities = |t: u32| entities_of(reader.posting(TermId::new(t)).unwrap().unwrap());
    assert_eq!(
        posting_entities(0),
        vec![1, 2, 4, 5],
        "entity 3 is folded out"
    );
    assert_eq!(posting_entities(1), vec![10, 11, 12]);

    let pairs = read_pairs(&pairs_path);
    assert_eq!(
        pairs,
        vec![(0, 1), (0, 2), (0, 4), (0, 5), (1, 10), (1, 11), (1, 12)],
        "pairs.parquet must be (term, entity)-sorted and must not carry the folded entity"
    );

    // Restated as an explicit agreement check, term by term, rather than trusting the literal
    // above alone: every entity `postings.arrow` names for a term is named by exactly the same
    // set of rows in `pairs.parquet`, and nothing else is.
    for t in 0..2u32 {
        let from_postings = posting_entities(t);
        let from_pairs: Vec<u64> = pairs
            .iter()
            .filter(|(term, _)| *term == t)
            .map(|(_, e)| *e)
            .collect();
        let from_postings_as_u64: Vec<u64> = from_postings.iter().map(|&e| e as u64).collect();
        assert_eq!(
            from_pairs, from_postings_as_u64,
            "term {t}: pairs.parquet and postings.arrow disagree"
        );
    }
    assert!(
        !pairs.iter().any(|(_, e)| *e == 3),
        "the tombstoned entity must not survive in pairs.parquet under any term"
    );
}
