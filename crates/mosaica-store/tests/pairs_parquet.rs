//! `PairsParquetWriter`'s two producers agree byte for byte.
//!
//! The writer moved into this crate when compaction's pass 2 became its second producer (see
//! `src/pairs.rs`). This is the guard that the move, and the iterator entry point the sweep needs,
//! left the file it produces unchanged.

use croaring::Bitmap;

use tessera_store::PairsParquetWriter;

/// **`push_iter` produces exactly what `push_run` would from the same entities, batch boundary
/// included.** `push_iter` exists so the term sweep can drive this writer from a `croaring::Bitmap`
/// accumulator without collecting it into a `Vec<u32>` first (the memory bound the sweep exists to
/// avoid — see `tessera_authz::term_sweep`'s module doc); this is the check that the shortcut
/// costs nothing in the file it produces. One term's count (70,000) is chosen to exceed
/// `PairsParquetWriter::BATCH` (65,536), so the comparison exercises an internal flush mid-term,
/// not just the single-batch case.
///
/// **Mutation:** have `push_iter` flush on every call instead of at the batch boundary, or drop
/// the `term_id` from one iteration — either changes the row count or the batch layout, and this
/// test compares full file bytes, not just row content.
#[test]
fn push_iter_matches_push_run_byte_for_byte() {
    let dir = tempfile::TempDir::new().unwrap();
    let terms: Vec<(u32, Vec<u32>)> = vec![
        (0, vec![]),
        (1, vec![7, 8, 9]),
        (2, (0..70_000u32).collect()),
    ];

    let run_path = dir.path().join("via-push-run.parquet");
    let mut run_writer = PairsParquetWriter::create(&run_path).unwrap();
    for (term, entities) in &terms {
        run_writer.push_run(*term, entities).unwrap();
    }
    run_writer.finish().unwrap();

    let iter_path = dir.path().join("via-push-iter.parquet");
    let mut iter_writer = PairsParquetWriter::create(&iter_path).unwrap();
    for (term, entities) in &terms {
        let bitmap = Bitmap::of(entities);
        iter_writer.push_iter(*term, bitmap.iter()).unwrap();
    }
    iter_writer.finish().unwrap();

    assert_eq!(
        std::fs::read(&run_path).unwrap(),
        std::fs::read(&iter_path).unwrap()
    );
}
