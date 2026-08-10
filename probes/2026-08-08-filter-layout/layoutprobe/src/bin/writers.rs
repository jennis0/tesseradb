//! What producing a filter column costs in memory, whole against streamed.
//!
//! Four arms, **one per process** — a peak is a high-water mark and does not come back down, so two
//! arms in one process report the larger of them twice:
//!
//! | arm | what it does |
//! |---|---|
//! | `values-whole` | materialise the column as one `Codes`, then `write_value_column` |
//! | `values-stream` | push it to `ValueColumnWriter` in 64 Ki chunks |
//! | `postings-whole` | group into `BTreeMap<code, Vec<entity>>`, then `write_delta_tier_at` |
//! | `postings-band` | counting pass, then a flat buffer per band through `KeyedPostingsSpool` |
//!
//! The writers are the shipped ones in every arm. What the postings arms *reproduce* rather than
//! call is the emit loop around them — `write_filter_postings` is private to the build — so
//! `postings-whole` is the grouping the build did before this change and `postings-band` is the
//! banded scatter it does now, both against the real writer. The values arms call the shipped API
//! end to end.
//!
//! The values are generated from a mixing function rather than read from an array, so nothing the
//! probe holds for its own convenience appears in the measurement. That is also why the postings
//! arms generate twice: the real caller reads a column both passes see, and holding one here would
//! add a term neither arm's construction is responsible for.
//!
//! Two peaks are reported, and the distinction is the point. `VmHWM` is the process's high-water
//! resident set — but **assembly maps the spool and the IPC copy touches every page of it**, so a
//! streaming writer's high-water still includes the finished column once, as file-backed page
//! cache. Those pages are clean and reclaimable; the heap the whole-column construction holds is
//! not. So `RssAnon` is sampled alongside — per chunk and per band, which is where each arm's
//! working set actually peaks — and that is the number the memory plan is written against.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use tessera_authz::{write_delta_tier_at, KeyedPostingsSpool};
use tessera_filter::{write_value_column, Codes, ColumnKind, ValueColumnWriter};

/// A field of `/proc/self/status`, in bytes.
fn status_bytes(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("linux");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .expect("a value")
                .parse()
                .expect("kB");
            return kb * 1024;
        }
    }
    panic!("no {field} in /proc/self/status");
}

fn peak_bytes() -> u64 {
    status_bytes("VmHWM:")
}

/// Anonymous (heap and stack) resident bytes — no file-backed page counts here.
fn anon_bytes() -> u64 {
    status_bytes("RssAnon:")
}

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The code entity `e` carries. Scattered across the 32-bit space, as `vocabulary` mints them, and
/// every entity carries one — the fully covered column §6.2 prices.
#[inline]
fn code_of(e: u64, vocab: u64) -> u32 {
    (splitmix(e) % vocab) as u32 * 7 + 11
}

/// Values pushed per chunk, matching the build's own.
const CHUNK: usize = 1 << 16;
/// Entity ids the banded emit holds in flight, matching the build's own: 4 B each.
const BAND_ROWS: usize = 1 << 26;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arm = args.get(1).map(String::as_str).unwrap_or("values-stream");
    let n: u64 = args
        .get(2)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(200_000_000);
    let vocab: u64 = args
        .get(3)
        .map(|s| s.parse().expect("vocab"))
        .unwrap_or(1000);

    let dir = std::env::temp_dir().join("tessera-writers-probe");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let values_path = dir.join("values.arrow");
    let presence_path = dir.join("presence.roaring");
    let postings_path = dir.join("postings.arrow");
    let spool_path = dir.join("postings.spool");

    let before = peak_bytes();
    let mut anon_peak = anon_bytes();
    let watch = |a: &mut u64| *a = (*a).max(anon_bytes());
    let t = Instant::now();
    match arm {
        "values-whole" => {
            let held: Vec<u32> = (0..n).map(|e| code_of(e, vocab)).collect();
            watch(&mut anon_peak);
            write_value_column(&values_path, &presence_path, &Codes::U32(held.into()), None)
                .expect("write");
        }
        "values-stream" => {
            let mut w = ValueColumnWriter::create(&values_path, &presence_path, ColumnKind::U32)
                .expect("create");
            let mut held: Vec<u32> = Vec::with_capacity(CHUNK);
            for e in 0..n {
                held.push(code_of(e, vocab));
                if held.len() == CHUNK {
                    w.push(&Codes::U32(std::mem::take(&mut held).into()))
                        .expect("push");
                    held.reserve(CHUNK);
                    watch(&mut anon_peak);
                }
            }
            if !held.is_empty() {
                w.push(&Codes::U32(held.into())).expect("push");
            }
            w.finish(None).expect("finish");
        }
        "postings-whole" => {
            let mut by_code: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            for e in 0..n {
                by_code.entry(code_of(e, vocab)).or_default().push(e as u32);
            }
            watch(&mut anon_peak);
            let entries: Vec<(u32, Vec<u32>)> = by_code.into_iter().collect();
            watch(&mut anon_peak);
            write_delta_tier_at(&postings_path, &entries, 32).expect("write");
        }
        "postings-band" => {
            let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
            for e in 0..n {
                *counts.entry(code_of(e, vocab)).or_insert(0) += 1;
            }
            let codes: Vec<u32> = counts.keys().copied().collect();
            let rows: Vec<u32> = counts.values().copied().collect();
            drop(counts);

            let budget = BAND_ROWS.max(rows.iter().copied().max().unwrap_or(0) as usize);
            let mut bands: Vec<(usize, usize)> = Vec::new();
            let (mut lo, mut acc) = (0usize, 0usize);
            for (i, &r) in rows.iter().enumerate() {
                if acc + r as usize > budget && i > lo {
                    bands.push((lo, i));
                    lo = i;
                    acc = 0;
                }
                acc += r as usize;
            }
            bands.push((lo, codes.len()));
            eprintln!("{} band(s)", bands.len());

            let mut spool = KeyedPostingsSpool::create(&spool_path, 32).expect("create");
            for (lo, hi) in bands {
                let keys = &codes[lo..hi];
                let mut offsets: Vec<u64> = Vec::with_capacity(hi - lo + 1);
                let mut total = 0u64;
                offsets.push(0);
                for &r in &rows[lo..hi] {
                    total += r as u64;
                    offsets.push(total);
                }
                let mut flat: Vec<u32> = vec![0; total as usize];
                let mut cursor: Vec<u64> = offsets[..hi - lo].to_vec();
                for e in 0..n {
                    let Ok(local) = keys.binary_search(&code_of(e, vocab)) else {
                        continue;
                    };
                    assert!(cursor[local] < offsets[local + 1], "overflow");
                    flat[cursor[local] as usize] = e as u32;
                    cursor[local] += 1;
                }
                watch(&mut anon_peak);
                for (local, slot) in cursor.iter().enumerate() {
                    assert_eq!(*slot, offsets[local + 1], "short fill");
                }
                for local in 0..(hi - lo) {
                    spool
                        .append(
                            keys[local],
                            &flat[offsets[local] as usize..offsets[local + 1] as usize],
                        )
                        .expect("append");
                }
            }
            spool.finish(&postings_path).expect("finish");
        }
        other => panic!("unknown arm {other}"),
    }
    let elapsed = t.elapsed().as_secs_f64();
    let peak = peak_bytes();

    let file = |p: &PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    watch(&mut anon_peak);
    println!("arm,n,vocab,peak_rss_mb,peak_anon_mb,peak_before_mb,seconds,output_bytes");
    println!(
        "{arm},{n},{vocab},{:.0},{:.0},{:.0},{elapsed:.1},{}",
        peak as f64 / 1e6,
        anon_peak as f64 / 1e6,
        before as f64 / 1e6,
        file(&values_path) + file(&postings_path)
    );
    eprintln!("artefacts left in {} — delete when done", dir.display());
}
