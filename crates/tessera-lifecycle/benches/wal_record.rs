//! What one `/control/ingest` batch costs to put in the log, over a GeoNames-shaped row: eight
//! category codes, four numbers, a name, a handful of label descriptors and an external id.
//!
//! The append is `postcard::to_allocvec` plus a framed write, and the figure it is measured
//! against is ~80 ms for a 10,000-row batch — the window close's largest single stage. Three
//! arms separate the two things that could be: `encode_allocvec` is the path as it stands,
//! `encode_reused` writes into a buffer that is already big enough, and `decode` is what a
//! restart pays per batch.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tessera_lifecycle::wal::{WalRecord, WalRow, WalScalar};
use tessera_types::EntityId;

/// A row on GeoNames' declaration: `feature_class`, `feature_code`, `country`, `admin1..4` and
/// `timezone` as category codes, then `population`, `elevation`, `dem`, `modification_date` and
/// the `name`. `dem` is absent often enough in the source to be worth carrying as one.
fn row(i: u64) -> WalRow {
    WalRow {
        external_id: Some(format!("{}", 2_000_000 + i).into_bytes()),
        entity_id: EntityId::new(i),
        view: "world".to_owned(),
        join: false,
        // The label terms: a country and its admin levels, as descriptor bytes.
        descriptors: vec![
            b"country:GB".to_vec(),
            b"admin1:GB.ENG".to_vec(),
            b"admin2:GB.ENG.H9".to_vec(),
        ],
        x: -0.127_758 + i as f64 * 1e-6,
        y: 51.507_351 - i as f64 * 1e-6,
        scalars: vec![
            WalScalar::U32(3),
            WalScalar::U32(147),
            WalScalar::U32(826),
            WalScalar::U32(12),
            WalScalar::U32(430),
            WalScalar::U32(1_204),
            WalScalar::Null,
            WalScalar::U32(31),
            WalScalar::I64(8_908_081),
            WalScalar::I16(11),
            WalScalar::I16(24),
            WalScalar::TimestampUs(1_735_689_600_000_000),
            WalScalar::Utf8(format!("Greater London {i}")),
        ],
        scoped: Vec::new(),
    }
}

fn batch(rows: usize) -> WalRecord {
    WalRecord::IngestBatch {
        batch_id: "geonames-holdout-000017".to_owned(),
        body_hash: [7u8; 32],
        rows: (0..rows as u64).map(row).collect(),
    }
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_ingest_record");
    for rows in [1_000usize, 10_000] {
        let record = batch(rows);
        let encoded = postcard::to_allocvec(&record).expect("a record encodes");
        group.throughput(criterion::Throughput::Elements(rows as u64));

        group.bench_with_input(BenchmarkId::new("encode_allocvec", rows), &record, |b, r| {
            b.iter(|| postcard::to_allocvec(r).expect("a record encodes"))
        });

        // The same encoding with the growth taken out: how much of the append is the `Vec`
        // doubling its way up to the batch's size, and how much is the encoding itself.
        let mut buffer = vec![0u8; encoded.len() * 2];
        group.bench_with_input(BenchmarkId::new("encode_reused", rows), &record, |b, r| {
            b.iter(|| postcard::to_slice(r, &mut buffer).expect("a record encodes").len())
        });

        group.bench_with_input(BenchmarkId::new("decode", rows), &encoded, |b, bytes| {
            b.iter(|| postcard::from_bytes::<WalRecord>(bytes).expect("a record decodes"))
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
