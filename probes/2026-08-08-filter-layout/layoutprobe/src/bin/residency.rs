//! What a generation pays to *open* its filter columns — resident bytes and wall time, mapped
//! against read.
//!
//! Every declared filter column is opened at once, at generation open, and held for the process
//! lifetime. The scan arms measure what a filter costs once the column is in hand; this one
//! measures what holding it costs before any filter arrives, which is the term a deployment pays
//! whether or not anyone filters.
//!
//! RSS is read from `/proc/self/statm` (Linux), which counts *resident* pages — so a mapped column
//! whose pages have not been touched does not appear in it, and one that has been scanned does.
//! That is the quantity of interest: not virtual size, which mapping trivially inflates, but the
//! memory the process is actually holding.

use std::time::Instant;

use tessera_filter::{write_value_column, Codes, ValueColumn};

/// Resident set size in bytes, from `/proc/self/statm`'s second field (resident pages).
fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("linux");
    let resident: u64 = statm
        .split_whitespace()
        .nth(1)
        .expect("statm has a resident field")
        .parse()
        .expect("a page count");
    resident * 4096
}

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args.get(1).map(|s| s.parse().expect("n")).unwrap_or(100_000_000);
    let columns: usize = args.get(2).map(|s| s.parse().expect("columns")).unwrap_or(8);

    let dir = std::env::temp_dir().join("tessera-residency-probe");
    std::fs::create_dir_all(&dir).expect("temp dir");

    // Written once and reused across both arms, so the two open the identical bytes.
    let paths: Vec<std::path::PathBuf> = (0..columns)
        .map(|c| {
            let path = dir.join(format!("values-{c}.arrow"));
            if !path.exists() {
                let values: Vec<u32> = (0..n)
                    .map(|e| (splitmix(e ^ c as u64) % 1_000) as u32)
                    .collect();
                write_value_column(
                    &path,
                    &dir.join("unused.roaring"),
                    &Codes::U32(values.into()),
                    None,
                )
                .expect("write");
            }
            path
        })
        .collect();

    println!("mode,n,columns,open_ms,rss_after_open_mb,rss_after_one_scan_mb,scan_cold_ms,scan_warm_ms");

    // **One arm per process.** Running both in one would let the read arm's freed pages sit in the
    // allocator's arena and flatter the mapped arm's baseline; the caller invokes this twice.
    let only = args.get(3).map(String::as_str);
    for mmap in [false, true] {
        match only {
            Some("read") if mmap => continue,
            Some("mmap") if !mmap => continue,
            _ => {}
        }
        let baseline = rss_bytes();
        let t = Instant::now();
        let held: Vec<ValueColumn> = paths
            .iter()
            .map(|p| ValueColumn::open(p, None, mmap).expect("open"))
            .collect();
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        let after_open = rss_bytes().saturating_sub(baseline);

        // One filter over one column, which is what a first request does. The mapped arm should
        // fault in only the pages that column's scan touches.
        let mut candidate = croaring::Bitmap::new();
        candidate.add_range(0u32..(n / 100) as u32);
        candidate.run_optimize();
        let t = Instant::now();
        let hits = held[0].scan_eq(&candidate, tessera_types::AttrLocalId::new(42));
        std::hint::black_box(hits.cardinality());
        // Cold: the mapped arm pays its page faults here, the read arm paid them at open.
        let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
        let after_scan = rss_bytes().saturating_sub(baseline);
        let t = Instant::now();
        std::hint::black_box(
            held[0]
                .scan_eq(&candidate, tessera_types::AttrLocalId::new(42))
                .cardinality(),
        );
        let warm_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!(
            "{},{n},{columns},{open_ms:.1},{:.0},{:.0},{cold_ms:.2},{warm_ms:.2}",
            if mmap { "mmap" } else { "read" },
            after_open as f64 / 1e6,
            after_scan as f64 / 1e6,
        );
        drop(held);
        if !mmap {
            // The read arm's buffers are freed here, but the allocator may keep the arena — so the
            // mmap arm's baseline is retaken above rather than assumed to be the same.
            std::hint::black_box(&paths);
        }
    }

    eprintln!("artefacts left in {} — delete when done", dir.display());
}
