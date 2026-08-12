//! **What does a mixed-field row actually compress to in the built record blob?**
//!
//! `records-and-search.md` §3 sets the block target at 256 KiB on the strength of the
//! string-storage probe's **2.44×** — which was measured on *per-column title bytes*, one field
//! type, one column, by a Python harness that never touched this format. The design says so and
//! marks the mixed-row figure **assumed**; §11 item 6 owes the measurement. This is it.
//!
//! The gap being closed is not a rounding one. A blob row is not a column: it interleaves a
//! `utf8` title with an `i64` timestamp and a `u8` count, so the compressor sees short runs of
//! dissimilar bytes where the column gave it 256 KiB of one kind. Interleaving is the reason to
//! expect *worse*; the shared context of neighbouring rows — the same `arXiv` prefixes, the same
//! licence URLs, the same journal names, run after run — is the reason to expect *better*. Which
//! wins is an empirical question about this corpus and this framing, and nothing before this
//! answered it.
//!
//! # What is measured, and against what
//!
//! Three row shapes, every one written through [`RecordBlobWriter`] — the shipped writer, the
//! shipped 256 KiB target, the shipped zstd level and framing — over real arXiv records read from
//! the Kaggle snapshot in snapshot order, which is submission order, which is entity order
//! (`probes/dataset.md` §4.1; the string-storage probe reads the same file the same way).
//!
//! | shape | fields | why it is here |
//! |---|---|---|
//! | `title` | one `utf8` | **the control**: the probe's own condition, through this format |
//! | `mixed` | 8 `utf8` + 6 fixed-width | the shape §3 assumes about |
//! | `mixed-abstract` | `mixed` + the abstract | a `text` field's values live in the blob too (§3) |
//!
//! The control is what makes the comparison honest. The probe's 2.44× was measured on raw title
//! bytes with a different compressor invocation and no row framing at all, so quoting `mixed`
//! against 2.44 directly would compare two things that differ in more than the row shape. Running
//! `title` through *this* writer isolates the one variable the design's assumption is about.
//!
//! Two ratios are reported per shape, because the design's sentence and the design's budget want
//! different ones:
//!
//! - **format ratio** = framed row bytes ÷ `blocks.bin`. What the compressor achieves on what it
//!   is handed. This is the number comparable to the probe's 2.44×.
//! - **B/entity, all three files** = (`blocks.bin` + `hasrow.roaring` + `directory.arrow`) ÷
//!   entities. What the blob costs a corpus, addressing included — the number §3's storage
//!   arithmetic is actually made of, and always the larger of the two burdens.
//!
//! Random single-row read latency is measured too, against §3's quoted 169 µs — the other figure
//! the 256 KiB choice rests on, and the one that decides whether drill-down is an interaction.
//!
//! **Not measured here**: the coalesce and fold rewrite rates (§11 item 7), and any scale past the
//! real corpus — arXiv has 2.42M records and this reads a prefix of them, so nothing here speaks
//! to 10⁹ block behaviour beyond the block target being scale-free by construction.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin record_blob_ratio -- \
//!     --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json \
//!     [--limit 2400000] [--reads 2000]
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

use tessera_filter::{Access, RecordBlob, RecordField, RecordValue, RECORD_BLOCK_TARGET};
use tessera_filter_write::RecordBlobWriter;

/// One snapshot record, reduced to the fields a blob row would carry. Owned `String`s: the
/// snapshot is read once and the rows are built three times.
struct Paper {
    id: String,
    submitter: String,
    title: String,
    comments: String,
    journal_ref: String,
    doi: String,
    license: String,
    categories: String,
    abstract_: String,
    /// `versions[0].created`, as microseconds since the epoch. `i64::MIN` marks unparsable, and
    /// such a row simply omits the field — which is what a blob row does with an absent value.
    created_us: i64,
    authors: u8,
    category_count: u8,
    year: u16,
}

/// Tags are positions in `declared_scalars`; this bin declares its own order, which is all a
/// standalone measurement needs — nothing here opens a manifest.
const TAG_ID: u16 = 0;
const TAG_SUBMITTER: u16 = 1;
const TAG_TITLE: u16 = 2;
const TAG_COMMENTS: u16 = 3;
const TAG_JOURNAL: u16 = 4;
const TAG_DOI: u16 = 5;
const TAG_LICENSE: u16 = 6;
const TAG_CATEGORIES: u16 = 7;
const TAG_CREATED: u16 = 8;
const TAG_AUTHORS: u16 = 9;
const TAG_CATCOUNT: u16 = 10;
const TAG_YEAR: u16 = 11;
const TAG_HAS_DOI: u16 = 12;
const TAG_RECENCY: u16 = 13;
const TAG_ABSTRACT: u16 = 14;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Title,
    Mixed,
    MixedAbstract,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Shape::Title => "title",
            Shape::Mixed => "mixed",
            Shape::MixedAbstract => "mixed-abstract",
        }
    }
}

/// The blob row for one paper under one shape. An empty string is treated as an **absent** field
/// and omitted — that is what the format means by absence, and carrying `""` instead would both
/// inflate the row and hand the compressor a run of identical framing bytes it would not see in
/// a real artefact.
fn row_for(paper: &Paper, shape: Shape, fields: &mut Vec<RecordField>) {
    fields.clear();
    let push_str = |tag: u16, s: &str, fields: &mut Vec<RecordField>| {
        if !s.is_empty() {
            fields.push(RecordField {
                tag,
                value: RecordValue::Utf8(s.to_string()),
            });
        }
    };
    if shape == Shape::Title {
        push_str(TAG_TITLE, &paper.title, fields);
        // A row with no fields cannot be encoded, and a paper with no title has no blob row at
        // all — exactly the has-row absence the format is built around.
        return;
    }
    push_str(TAG_ID, &paper.id, fields);
    push_str(TAG_SUBMITTER, &paper.submitter, fields);
    push_str(TAG_TITLE, &paper.title, fields);
    push_str(TAG_COMMENTS, &paper.comments, fields);
    push_str(TAG_JOURNAL, &paper.journal_ref, fields);
    push_str(TAG_DOI, &paper.doi, fields);
    push_str(TAG_LICENSE, &paper.license, fields);
    push_str(TAG_CATEGORIES, &paper.categories, fields);
    if shape == Shape::MixedAbstract {
        push_str(TAG_ABSTRACT, &paper.abstract_, fields);
    }
    if paper.created_us != i64::MIN {
        fields.push(RecordField {
            tag: TAG_CREATED,
            value: RecordValue::TimestampUs(paper.created_us),
        });
    }
    fields.push(RecordField {
        tag: TAG_AUTHORS,
        value: RecordValue::U8(paper.authors),
    });
    fields.push(RecordField {
        tag: TAG_CATCOUNT,
        value: RecordValue::U8(paper.category_count),
    });
    fields.push(RecordField {
        tag: TAG_YEAR,
        value: RecordValue::U16(paper.year),
    });
    fields.push(RecordField {
        tag: TAG_HAS_DOI,
        value: RecordValue::Bool(!paper.doi.is_empty()),
    });
    // Derived, not sourced — years from 1991, the corpus's first submission. It exists so the
    // `f32` path is exercised at all and carries nothing the timestamp does not; stated here
    // rather than left to be inferred.
    fields.push(RecordField {
        tag: TAG_RECENCY,
        value: RecordValue::F32((paper.year as f32) - 1991.0),
    });
}

/// The snapshot's `versions[0].created` — an RFC-2822-ish `"Mon, 2 Apr 2007 19:18:42 GMT"` — as
/// microseconds since the epoch, and the year beside it. Parsed by hand: this bin has no date
/// dependency and needs only a monotone, realistically-distributed `i64`, which the fields it
/// reads give exactly.
fn parse_created(s: &str) -> (i64, u16) {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts: Vec<&str> = s.split_whitespace().collect();
    // "Mon," day month year hh:mm:ss zone
    if parts.len() < 5 {
        return (i64::MIN, 0);
    }
    let day: i64 = parts[1].parse().unwrap_or(0);
    let month = MONTHS.iter().position(|m| *m == parts[2]).unwrap_or(0) as i64;
    let year: i64 = parts[3].parse().unwrap_or(0);
    if year == 0 {
        return (i64::MIN, 0);
    }
    let (h, m, sec) = parts
        .get(4)
        .map(|t| {
            let mut it = t.split(':');
            (
                it.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
                it.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
                it.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0, 0));
    // Days from 1970 by the civil-from-days inverse, no leap-second pretence: this is a plausible
    // timestamp, not a calendar library.
    let y = if month <= 1 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 10) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3_600 + m * 60 + sec;
    (secs * 1_000_000, year as u16)
}

/// Pull one string field out of a snapshot line. `serde_json` full-parses each line; at 2.4M
/// lines that is the dominant cost of this bin and it is setup, never timed.
fn read_snapshot(path: &PathBuf, limit: usize) -> std::io::Result<Vec<Paper>> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut out = Vec::with_capacity(limit.min(4_000_000));
    for line in reader.lines() {
        let line = line?;
        if out.len() >= limit {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let s = |k: &str| -> String {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let categories = s("categories");
        let created = v
            .get("versions")
            .and_then(|x| x.as_array())
            .and_then(|a| a.first())
            .and_then(|x| x.get("created"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let (created_us, year) = parse_created(created);
        let authors = v
            .get("authors_parsed")
            .and_then(|x| x.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
            .min(255) as u8;
        out.push(Paper {
            id: s("id"),
            submitter: s("submitter"),
            title: s("title"),
            comments: s("comments"),
            journal_ref: s("journal-ref"),
            doi: s("doi"),
            license: s("license"),
            category_count: categories.split_whitespace().count().min(255) as u8,
            categories,
            abstract_: s("abstract"),
            created_us,
            authors,
            year,
        });
    }
    Ok(out)
}

/// splitmix64 — the repo's own mask-independent hash, used here only to pick read targets.
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut snapshot: Option<PathBuf> = None;
    let mut limit = 2_400_000usize;
    let mut reads = 2_000usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--snapshot" => snapshot = args.next().map(PathBuf::from),
            "--limit" => limit = args.next().and_then(|v| v.parse().ok()).unwrap_or(limit),
            "--reads" => reads = args.next().and_then(|v| v.parse().ok()).unwrap_or(reads),
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    let Some(snapshot) = snapshot else {
        return Err("--snapshot <arxiv-metadata-oai-snapshot.json> is required".into());
    };

    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — the read latencies are not meaningful. Use --release.");
    }

    eprintln!("reading {} ...", snapshot.display());
    let t0 = Instant::now();
    let papers = read_snapshot(&snapshot, limit)?;
    eprintln!(
        "read {} records in {:.1} s (snapshot order = submission order = entity order)",
        papers.len(),
        t0.elapsed().as_secs_f64()
    );

    let dir = std::env::temp_dir().join(format!("tessera-record-ratio-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    println!(
        "\nblock target {} KiB (RECORD_BLOCK_TARGET), zstd level as shipped, {} entities\n",
        RECORD_BLOCK_TARGET / 1024,
        papers.len()
    );
    println!(
        "{:<15} {:>9} {:>10} {:>10} {:>8} {:>9} {:>9} {:>8} {:>10}",
        "shape", "rows", "src B/e", "framed B/e", "blocks", "fmt ratio", "src ratio", "B/e all", "us/read"
    );

    for shape in [Shape::Title, Shape::Mixed, Shape::MixedAbstract] {
        let blocks = dir.join(format!("{}-blocks.bin", shape.name()));
        let hasrow = dir.join(format!("{}-hasrow.roaring", shape.name()));
        let directory = dir.join(format!("{}-directory.arrow", shape.name()));

        let mut writer =
            RecordBlobWriter::create(&blocks, &hasrow, &directory, RECORD_BLOCK_TARGET)?;
        let mut fields: Vec<RecordField> = Vec::with_capacity(16);
        // The bytes the *source* holds for the fields this shape carries — the denominator the
        // probe's own ratio used, before any framing. `framed` is what the writer handed zstd.
        let mut src_bytes = 0u64;
        let mut framed_bytes = 0u64;
        let mut rows = 0u64;
        let mut encoded = Vec::new();
        for (entity, paper) in papers.iter().enumerate() {
            row_for(paper, shape, &mut fields);
            if fields.is_empty() {
                continue;
            }
            src_bytes += fields
                .iter()
                .map(|f| match &f.value {
                    RecordValue::Utf8(s) => s.len() as u64,
                    RecordValue::Bool(_) | RecordValue::U8(_) => 1,
                    RecordValue::U16(_) => 2,
                    RecordValue::F32(_) => 4,
                    _ => 8,
                })
                .sum::<u64>();
            encoded.clear();
            tessera_filter::encode_row(entity as u32, &fields, &mut encoded)?;
            framed_bytes += encoded.len() as u64;
            writer.push_row(entity as u32, &fields)?;
            rows += 1;
        }
        writer.finish()?;

        let blocks_bytes = std::fs::metadata(&blocks)?.len();
        let hasrow_bytes = std::fs::metadata(&hasrow)?.len();
        let dir_bytes = std::fs::metadata(&directory)?.len();
        let all = blocks_bytes + hasrow_bytes + dir_bytes;

        // Random single-row reads through the shipped reader — one block read and decompress
        // each, which is §3's 169 µs claim. `Access::Read` is the ordinary read path.
        let blob = RecordBlob::open(&blocks, &hasrow, &directory, Access::Read)?;
        let mut hit = 0u64;
        let t = Instant::now();
        for i in 0..reads {
            let entity = (mix64(i as u64) % papers.len() as u64) as u32;
            if blob.fields_of(entity)?.is_some() {
                hit += 1;
            }
        }
        let us_per_read = t.elapsed().as_secs_f64() * 1e6 / reads as f64;

        println!(
            "{:<15} {:>9} {:>10.2} {:>10.2} {:>8} {:>9.2} {:>9.2} {:>8.2} {:>10.1}",
            shape.name(),
            rows,
            src_bytes as f64 / rows as f64,
            framed_bytes as f64 / rows as f64,
            blob.block_count(),
            framed_bytes as f64 / blocks_bytes as f64,
            src_bytes as f64 / blocks_bytes as f64,
            all as f64 / rows as f64,
            us_per_read,
        );
        eprintln!(
            "  {}: blocks {} B, hasrow {} B, directory {} B, {} of {reads} reads hit a row",
            shape.name(),
            blocks_bytes,
            hasrow_bytes,
            dir_bytes,
            hit,
        );
        drop(blob);
        let _ = std::fs::remove_file(&blocks);
        let _ = std::fs::remove_file(&hasrow);
        let _ = std::fs::remove_file(&directory);
    }

    println!(
        "\nfmt ratio = framed row bytes / blocks.bin — the figure comparable to the string-storage\n\
         probe's 2.44x. src ratio = source value bytes / blocks.bin, framing counted as a cost.\n\
         B/e all = (blocks + hasrow + directory) / entities: what the corpus pays."
    );
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}
