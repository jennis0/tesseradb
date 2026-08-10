//! **Is a text column's residency anonymous memory, or reclaimable page cache?**
//!
//! The lifecycle campaign measured a 365 MB `utf8` value column residing 362 MB at open where a
//! 203 MB `i64` column resided 98 KB, and read that as the mapping failing for text. But it sampled
//! `VmRSS`, which counts *file-backed* resident pages as well as anonymous ones — so it cannot
//! distinguish the two outcomes that matter:
//!
//! - **Anonymous**: the reader copied the column into the heap. Mapping bought nothing, residency is
//!   a function of what the schema declares, and the design's §8 claim is broken for text.
//! - **File-backed and clean**: the mapping is working exactly as designed and something merely
//!   *touched* every page — Arrow validating UTF-8 across the values buffer at decode. The pages are
//!   evictable under pressure, which is what mapping is for, and the cost is a pass over the bytes
//!   rather than memory that cannot be reclaimed.
//!
//! The remedy differs completely between those, and one of them needs no remedy at all. `RssAnon`
//! and `RssFile` from `/proc/self/status` separate them directly.

use std::time::Instant;

use tessera_filter::{write_value_column, Codes, ValueColumn};

/// One `/proc/self/status` size field, in bytes.
fn status_kb(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("linux");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .expect("a number")
                .parse()
                .expect("kb");
            return kb * 1024;
        }
    }
    0
}

fn sample() -> (u64, u64) {
    (status_kb("RssAnon:"), status_kb("RssFile:"))
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(20_000_000);

    let dir = std::env::temp_dir().join("tessera-textresident");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let values = dir.join("values.arrow");
    let presence = dir.join("presence.roaring");

    // Surname-shaped, as the campaign's column was: a small stem vocabulary with a numeric tail.
    if !values.exists() {
        const STEMS: [&str; 8] = [
            "anderson", "andrews", "bergstrom", "castellano", "delacroix", "fitzgerald",
            "kowalski", "nakamura",
        ];
        let codes = Codes::text((0..n).map(|i| format!("{}-{:06}", STEMS[i % 8], i % 1_000_000)));
        write_value_column(&values, &presence, &codes, None).expect("write");
    }
    let bytes = std::fs::metadata(&values).expect("stat").len();

    println!("n,file_bytes,stage,rss_anon_delta,rss_file_delta,ms");

    let (a0, f0) = sample();
    let t = Instant::now();
    let column = ValueColumn::open(&values, None, true).expect("open mapped");
    let open_ms = t.elapsed().as_secs_f64() * 1000.0;
    let (a1, f1) = sample();
    println!(
        "{n},{bytes},open-mapped,{},{},{open_ms:.1}",
        a1.saturating_sub(a0),
        f1.saturating_sub(f0)
    );

    // A scan of 1% of the column: if the open already resided everything, this adds nothing, which
    // is itself the tell.
    let mut candidate = croaring::Bitmap::new();
    candidate.add_range(0u32..(n as u32 / 100));
    candidate.run_optimize();
    let t = Instant::now();
    let hits = column.scan_text_prefix(&candidate, "anderson");
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    std::hint::black_box(hits.cardinality());
    let (a2, f2) = sample();
    println!(
        "{n},{bytes},after-1pct-scan,{},{},{scan_ms:.1}",
        a2.saturating_sub(a0),
        f2.saturating_sub(f0)
    );

    eprintln!("artefacts in {} — delete when done", dir.display());
}
