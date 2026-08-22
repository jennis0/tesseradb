//! Sustained point ingest into a running server, carrying the attribute column and a membership.
//!
//! **The wire body comes from the generator, not from a transcription of it.**
//! `Corpus::ingest_batch` builds `/control/ingest`'s shape for a range of items — external id,
//! geometry, access label and every declared scalar — and this binary adds the two columns the
//! campaign needs beside it and that the generator's own batch does not carry:
//!
//! - **`partition`**, the attribute-predicate layer's value column. The generator writes it into
//!   `points.parquet` and not into `ingest_batch`, so an ingested point would otherwise carry no
//!   value for the one column a predicate layer reads — and the freshness claim under test is
//!   exactly *a point ingested with value v counts at its flush*.
//! - **a column named for an enumerated layer**, carrying the key that point joins
//!   (`artifacts-from-points.md` §6.2 — a point may name its artifacts on the wire as it may in a
//!   file). One constant key per run, because the question is whether one artifact's masked count
//!   moves by the number of members that arrived, and a spread across artifacts would only make
//!   the arithmetic harder to state.
//!
//! The access label may be overridden so every ingested point is visible to the principal doing the
//! checking: at a million terms an item's own two terms are seen by essentially nobody, and a
//! freshness check whose new members are invisible to the observer passes vacuously.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{ArrayRef, StringBuilder, UInt32Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use clap::Parser;
use tessera_corpus::materialise::PARTITION_LAYER;
use tessera_corpus::Corpus;
use tessera_spatial::Bounds;

const GRID: f64 = 65536.0;

#[derive(Parser)]
#[command(about = "Sustained ingest of generator points into a running server")]
struct Args {
    #[arg(long)]
    seed: u64,
    /// The corpus's built size — ingest starts above it, so every row is new.
    #[arg(long)]
    n: u64,
    #[arg(long, default_value_t = 65_536)]
    terms_per_level: u32,
    /// The control plane's base URL, e.g. `http://127.0.0.1:45721`.
    #[arg(long)]
    control: String,
    /// The operator credential the control plane is behind.
    #[arg(long)]
    credential: String,
    /// How many rows to ingest in total.
    #[arg(long, default_value_t = 10_000)]
    rows: u64,
    /// Rows per batch.
    #[arg(long, default_value_t = 500)]
    batch_rows: u64,
    /// Stop after this many seconds however many rows have gone (0 = no cap).
    #[arg(long, default_value_t = 0.0)]
    seconds: f64,
    /// The first entity id to ingest. Defaults to `n` — the built prefix's end.
    #[arg(long)]
    start: Option<u64>,
    /// Override every row's access label with this comma-separated term list, so the checking
    /// principal can see what arrives.
    #[arg(long)]
    access: Option<String>,
    /// Give every row this `partition` value instead of the one the generator would compute.
    #[arg(long)]
    partition_value: Option<u32>,
    /// Also carry a column named for this layer.
    #[arg(long)]
    membership_layer: Option<String>,
    /// The key that column carries on every row.
    #[arg(long)]
    membership_key: Option<String>,
    /// A prefix for the `x-tessera-batch-id` header, so two runs against one server do not collide
    /// on the idempotency horizon.
    #[arg(long, default_value = "campaign")]
    batch_prefix: String,
}

/// `Corpus::ingest_batch` with the campaign's extra columns appended.
fn body_for(args: &Args, corpus: &Corpus, range: std::ops::Range<u64>) -> Vec<u8> {
    let base = corpus.ingest_batch(range.clone());
    let rows = base.num_rows();
    let mut fields: Vec<Field> = base
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    let mut columns: Vec<ArrayRef> = base.columns().to_vec();

    // The access override replaces the generator's own label in place, so the column order the
    // declaration fixes is untouched.
    if let Some(access) = &args.access {
        let index = fields
            .iter()
            .position(|f| f.name() == "access")
            .expect("the generator's ingest batch carries an access column");
        let mut builder = StringBuilder::new();
        for _ in 0..rows {
            builder.append_value(access);
        }
        columns[index] = Arc::new(builder.finish());
    }

    let mut partition = UInt32Builder::with_capacity(rows);
    for e in range.clone() {
        partition.append_value(
            args.partition_value
                .unwrap_or_else(|| corpus.partition_artifact_of(PARTITION_LAYER, e) as u32),
        );
    }
    fields.push(Field::new("partition", DataType::UInt32, false));
    columns.push(Arc::new(partition.finish()));

    if let (Some(layer), Some(key)) = (&args.membership_layer, &args.membership_key) {
        let mut builder = StringBuilder::new();
        for _ in 0..rows {
            builder.append_value(key);
        }
        fields.push(Field::new(layer, DataType::Utf8, true));
        columns.push(Arc::new(builder.finish()));
    }

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).expect("the widened batch");
    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &schema).expect("an IPC writer");
        writer.write(&batch).expect("writing the batch");
        writer.finish().expect("closing the stream");
    }
    buffer
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
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .expect("an http client");

    let start = args.start.unwrap_or(args.n);
    let deadline = (args.seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(args.seconds));
    let began = Instant::now();
    let mut sent = 0u64;
    let mut accepted = 0u64;
    let mut minted = 0u64;
    let mut batches = 0u64;
    let mut refusals: Vec<String> = Vec::new();
    let mut latencies: Vec<f64> = Vec::new();

    let mut cursor = start;
    while sent < args.rows {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                break;
            }
        }
        let end = (cursor + args.batch_rows).min(start + args.rows);
        let body = body_for(&args, &corpus, cursor..end);
        let batch_id = format!("{}-{}-{}", args.batch_prefix, start, batches);
        let at = Instant::now();
        let response = client
            .post(format!("{}/control/ingest", args.control))
            .bearer_auth(&args.credential)
            .header("x-tessera-batch-id", &batch_id)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .expect("the ingest request");
        latencies.push(at.elapsed().as_secs_f64());
        let status = response.status().as_u16();
        let text = response.text().unwrap_or_default();
        if status == 200 {
            accepted += end - cursor;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                minted += json.get("minted").and_then(|m| m.as_u64()).unwrap_or(0);
            }
        } else if refusals.len() < 5 {
            refusals.push(format!("{status}: {}", &text[..text.len().min(300)]));
        }
        sent += end - cursor;
        cursor = end;
        batches += 1;
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| -> f64 {
        if latencies.is_empty() {
            f64::NAN
        } else {
            latencies[((q * latencies.len() as f64).ceil() as usize).saturating_sub(1).min(latencies.len() - 1)]
        }
    };
    let elapsed = began.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "rows_sent": sent,
            "rows_accepted": accepted,
            "batches": batches,
            "minted": minted,
            "seconds": elapsed,
            "rows_per_second": if elapsed > 0.0 { sent as f64 / elapsed } else { 0.0 },
            "batch_p50_ms": pick(0.5) * 1000.0,
            "batch_p99_ms": pick(0.99) * 1000.0,
            "batch_max_ms": latencies.last().copied().unwrap_or(f64::NAN) * 1000.0,
            "refusals": refusals,
        })
    );
}
