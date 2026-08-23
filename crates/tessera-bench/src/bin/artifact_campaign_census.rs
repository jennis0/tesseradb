//! `tessera corpus artifact-census`, taking its grant from a **file** and writing to one.
//!
//! **Why this exists, and why it is not a second statement of the corpus.** The census verb takes
//! its principal as `--grant`, a comma-separated argv string. At the campaign's term width a
//! principal seeing 93.75% of the corpus holds 131 072 terms — about 900 KB of descriptors — and
//! Linux caps a single argument at `MAX_ARG_STRLEN` (128 KB), so the verb refuses every grant above
//! roughly 13 000 terms with `Argument list too long`. That is a property of the process boundary,
//! not of the census.
//!
//! So this binary reads the same grant from a file and calls the **same** `tessera-corpus` methods
//! the CLI verb calls — `flat_artifact_census`, `partition_artifact_census`,
//! `boundary_artifact_census`, `treed_artifact_census`. There is one statement of the corpus and
//! this is a second door to it; a transcription of any of those rules here would be exactly the
//! failure `check-layers.sh` guards.
//!
//! Output is an Arrow IPC stream of `(artifact, count)` ascending, non-empty artifacts only — the
//! verb's own schema, written to a path rather than to stdout because a 10⁷-artifact census is
//! hundreds of megabytes and a pipe nobody drains is a wedge.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use arrow::array::{ArrayRef, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use clap::{Parser, ValueEnum};
use tessera_corpus::materialise::{BOUNDARY_LAYER, FIXTURE_LEVEL, FLAT_LAYER, PARTITION_LAYER, TREED_LAYER};
use tessera_corpus::{Corpus, Grant};
use tessera_spatial::Bounds;

const GRID: f64 = 65536.0;

#[derive(Clone, Copy, ValueEnum)]
enum Arm {
    Flat,
    Partition,
    Boundary,
    Treed,
}

#[derive(Parser)]
#[command(about = "The artifact census over a grant too large for an argv string")]
struct Args {
    #[arg(long)]
    seed: u64,
    #[arg(long)]
    n: u64,
    #[arg(long)]
    terms_per_level: u32,
    #[arg(long)]
    arm: Arm,
    /// A file holding the grant in the catalogue's own encoding: comma-separated decimal term
    /// descriptors, exactly what `--grant` would carry.
    #[arg(long)]
    grant_file: PathBuf,
    /// Where the `(artifact, count)` Arrow IPC stream goes.
    #[arg(long)]
    out: PathBuf,
}

fn main() {
    let args = Args::parse();
    let extent = Bounds {
        x_min: 0.0,
        x_max: GRID,
        y_min: 0.0,
        y_max: GRID,
    };
    let corpus = Corpus::with_terms_per_level(args.seed, args.n, extent, args.terms_per_level)
        .expect("the campaign's extent and term width");
    let encoded = std::fs::read_to_string(&args.grant_file).expect("the grant file");
    let grant = Grant::parse_bounded(encoded.trim(), corpus.term_space())
        .expect("the grant names terms inside this corpus's space");

    let counts = match args.arm {
        Arm::Flat => corpus.flat_artifact_census(FLAT_LAYER, FIXTURE_LEVEL, &grant),
        Arm::Partition => corpus.partition_artifact_census(PARTITION_LAYER, &grant),
        Arm::Boundary => corpus.boundary_artifact_census(BOUNDARY_LAYER, FIXTURE_LEVEL, &grant),
        Arm::Treed => corpus.treed_artifact_census(TREED_LAYER, &grant),
    };

    let schema = Arc::new(Schema::new(vec![
        Field::new("artifact", DataType::UInt64, false),
        Field::new("count", DataType::UInt64, false),
    ]));
    let file = File::create(&args.out).expect("the census output");
    let mut writer = StreamWriter::try_new(file, &schema).expect("an IPC stream writer");
    for chunk in counts.chunks(1 << 16) {
        let artifacts = UInt64Array::from_iter_values(chunk.iter().map(|(a, _)| *a));
        let totals = UInt64Array::from_iter_values(chunk.iter().map(|(_, c)| *c));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(artifacts) as ArrayRef, Arc::new(totals)],
        )
        .expect("two columns of one chunk's length");
        writer.write(&batch).expect("writing a census chunk");
    }
    writer.finish().expect("closing the census stream");
    println!("census: {} non-empty artifact(s) -> {}", counts.len(), args.out.display());
}
