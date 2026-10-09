//! **What does a walk over a whole record blob cost at 10⁸ rows?**
//!
//! Writes one blob through the shipped writer — one `u64` field a row, the shape of an integer id
//! column — over a has-row bitmap of long runs with short gaps, which is what a dense corpus's
//! entity space looks like, and times the three reads a whole-blob pass can take: the addressed
//! walk over every entity (`for_each_row_in`, which `verify --deep`'s unique check and the
//! engine's column reads take), the same walk over a sparse set, and the sequential cursor
//! (`self_check`).
//!
//! ```text
//! cargo run --release -p mosaica-bench --bin record_walk_scale -- --dir <scratch> [--rows N]
//! ```
//!
//! The blob is kept under `--dir` and reused by a later run with the same `--rows`, so two builds
//! of the reader can be timed over one blob.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use croaring::Bitmap;
use mosaica_filter::{Access, RecordBlob, RecordFieldRef, RecordValue, RecordValueRef};
use mosaica_filter_write::RecordBlobWriter;

#[derive(Parser)]
struct Args {
    /// Where the blob is written, or found from an earlier run.
    #[arg(long)]
    dir: PathBuf,
    /// Rows in the blob.
    #[arg(long, default_value_t = 100_000_000)]
    rows: u64,
    /// The sparse walk takes one present entity in this many.
    #[arg(long, default_value_t = 1_000)]
    stride: u64,
    /// Skip the addressed walk over every entity, for a reader too slow to finish it.
    #[arg(long)]
    skip_full: bool,
}

/// A deterministic stream of run and gap lengths.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

fn status_mib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |kib| kib >> 10)
}

fn main() {
    let args = Args::parse();
    let dir = args.dir.join(format!("record-walk-{}", args.rows));
    let blocks = dir.join("blocks.bin");
    let hasrow = dir.join("hasrow.roaring");
    let directory = dir.join("directory.arrow");

    if !directory.exists() {
        std::fs::create_dir_all(&dir).expect("create the blob directory");
        let t = Instant::now();
        let mut writer = RecordBlobWriter::create(
            &blocks,
            &hasrow,
            &directory,
            mosaica_filter::RECORD_BLOCK_TARGET,
        )
        .expect("create the writer");
        let mut lcg = Lcg(7);
        let mut entity = 0u32;
        let mut written = 0u64;
        while written < args.rows {
            let run = (1_000 + lcg.below(200_000)).min(args.rows - written);
            for _ in 0..run {
                let value = u64::from(entity) * 7;
                writer
                    .push_row(
                        entity,
                        &[RecordFieldRef {
                            tag: 0,
                            value: RecordValueRef::U64(value),
                        }],
                    )
                    .expect("push a row");
                entity += 1;
            }
            written += run;
            entity += 1 + lcg.below(2_000) as u32;
        }
        writer.finish().expect("finish the blob");
        println!("wrote {written} rows in {:.2?}", t.elapsed());
    }

    let blob = RecordBlob::open(&blocks, &hasrow, &directory, Access::MappedSequential)
        .expect("open the blob");
    let rows = blob.hasrow().expect("a has-row bitmap").cardinality();
    let bound = blob.hasrow().unwrap().maximum().map_or(0, |m| m + 1);
    println!(
        "{rows} rows over {bound} entities, {} blocks",
        blob.block_count()
    );

    let mut sum = 0u64;
    let mut visit = |_: u32, fields: Vec<mosaica_filter::RecordField>| {
        if let Some(RecordValue::U64(v)) = fields.first().map(|f| f.value.clone()) {
            sum = sum.wrapping_add(v);
        }
        Ok(())
    };

    if !args.skip_full {
        let t = Instant::now();
        blob.for_each_row_in(&Bitmap::from_range(0..bound), &mut visit)
            .expect("the addressed walk");
        println!("addressed walk, every entity: {:.2?}", t.elapsed());
    }

    let sparse: Bitmap = blob
        .hasrow()
        .unwrap()
        .iter()
        .step_by(args.stride as usize)
        .collect();
    let t = Instant::now();
    blob.for_each_row_in(&sparse, &mut visit)
        .expect("the sparse walk");
    println!(
        "addressed walk, {} entities (one in {}): {:.2?}",
        sparse.cardinality(),
        args.stride,
        t.elapsed()
    );

    let t = Instant::now();
    blob.self_check().expect("the sequential walk");
    println!("sequential walk (self_check): {:.2?}", t.elapsed());
    println!("checksum {sum}; peak RSS {} MiB", status_mib("VmHWM:"));
}
