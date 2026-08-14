//! What the *decodable* front-coded dictionary costs, against the non-decodable floor
//! `2026-08-12-keyword-and-list-storage` measured.
//!
//! That campaign's `front_coded()` charges one byte for the shared-prefix length and the suffix
//! bytes — no suffix length, no restart offsets — so what it reports cannot be decoded as stored.
//! records-and-search §4.3 carries the gap as a model: "the specified format below costs ~1–2 B/key
//! more, which moves the unique-identifier headline from 2.9× to ~2.2–2.3× and the modelled range
//! across the three shapes to ~2.2–3.9×". This replaces that model with the shipped writer's own
//! output over the same three real columns at the same three scales.
//!
//! It also chooses the restart interval, which the design left open: the interval trades a restart
//! offset and one un-elided key per block against `K/2` decodes per lookup, and both halves are
//! measured here rather than argued.
//!
//! Usage:
//!     cargo run --release --manifest-path probes/2026-08-13-keyword-dict/dictbytes/Cargo.toml -- \
//!         --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json

use std::io::{BufRead, BufReader};
use std::time::Instant;

use tessera_filter::{SortedDict, SortedDictWriter};

/// The restart intervals compared. 16 is LevelDB's; the neighbours bound the trade either side.
const INTERVALS: [u32; 4] = [8, 16, 32, 64];

fn main() {
    let mut snapshot = String::new();
    let mut scales = vec![250_000usize, 1_000_000, 2_400_000];
    let mut limit = 2_400_000usize;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--snapshot" => {
                snapshot = args[i + 1].clone();
                i += 2;
            }
            "--scales" => {
                scales = args[i + 1].split(',').map(|s| s.parse().unwrap()).collect();
                i += 2;
            }
            "--limit" => {
                limit = args[i + 1].parse().unwrap();
                i += 2;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(!snapshot.is_empty(), "--snapshot is required");
    let path = if let Some(rest) = snapshot.strip_prefix("~/") {
        format!("{}/{rest}", std::env::var("HOME").unwrap())
    } else {
        snapshot
    };

    // Snapshot order = submission order = entity order (`probes/dataset.md` §5 rule 1), so a scale
    // is a prefix, exactly as the parent campaign took it.
    let file = std::fs::File::open(&path).expect("the arXiv snapshot");
    let mut ids = Vec::with_capacity(limit);
    let mut submitters = Vec::with_capacity(limit);
    let mut dois = Vec::with_capacity(limit);
    for line in BufReader::with_capacity(1 << 20, file).lines() {
        if ids.len() >= limit {
            break;
        }
        let line = line.expect("a readable line");
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let field = |name: &str| -> String {
            record
                .get(name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        ids.push(field("id"));
        submitters.push(field("submitter"));
        dois.push(field("doi"));
    }
    println!(
        "read {} records (snapshot order = entity order)\n",
        ids.len()
    );

    println!("### bytes per key: the shipped format against the probe's non-decodable floor\n");
    println!(
        "{:>10} {:>10} {:>10} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "column", "scale", "distinct", "floor", "K=8", "K=16", "K=32", "K=64", "K16-fl"
    );
    for &n in &scales {
        if n > ids.len() {
            continue;
        }
        for (label, values) in [("id", &ids), ("submitter", &submitters), ("doi", &dois)] {
            per_key(label, values, n);
        }
    }

    println!(
        "\n### the whole DICT+C layout, B per present entity (dictionary at K=16 + one u32)\n"
    );
    println!(
        "{:>10} {:>10} {:>8} {:>8} {:>8} {:>8} {:>7} {:>7}",
        "column", "scale", "flat", "floor+C", "DICT+C", "dictB/k", "floor×", "real×"
    );
    for &n in &scales {
        if n > ids.len() {
            continue;
        }
        for (label, values) in [("id", &ids), ("submitter", &submitters), ("doi", &dois)] {
            whole_layout(label, values, n);
        }
    }

    println!("\n### lookup cost by restart interval, at the largest scale\n");
    println!(
        "{:>10} {:>10} {:>4} {:>12} {:>12} {:>12} {:>12}",
        "column", "distinct", "K", "resolve ns", "miss ns", "key_of ns", "walk ns/key"
    );
    let n = *scales.iter().filter(|&&s| s <= ids.len()).max().unwrap();
    for (label, values) in [("id", &ids), ("submitter", &submitters), ("doi", &dois)] {
        timings(label, values, n);
    }
}

/// The distinct, sorted keys of `values[..n]`, absence dropped.
fn keys_of(values: &[String], n: usize) -> Vec<String> {
    let mut keys: Vec<String> = values[..n]
        .iter()
        .filter(|v| !v.is_empty())
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

fn present_count(values: &[String], n: usize) -> usize {
    values[..n].iter().filter(|v| !v.is_empty()).count()
}

/// The parent campaign's `front_coded()`, transcribed exactly: one byte for the shared-prefix
/// length plus the suffix's bytes, the prefix measured in **characters** and capped at 255, and
/// nothing for suffix lengths or restarts. Not decodable as stored — that is the point of it.
fn probe_floor(keys: &[String]) -> u64 {
    let mut total = 0u64;
    let mut prev: Vec<char> = Vec::new();
    for key in keys {
        let cur: Vec<char> = key.chars().collect();
        let mut j = 0;
        while j < cur.len().min(prev.len()).min(255) && cur[j] == prev[j] {
            j += 1;
        }
        total += 1 + cur[j..].iter().map(|c| c.len_utf8() as u64).sum::<u64>();
        prev = cur;
    }
    total
}

fn build(keys: &[String], interval: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = SortedDictWriter::with_restart_interval(&mut out, interval).unwrap();
    for key in keys {
        writer.push(key).unwrap();
    }
    let stats = writer.finish().unwrap();
    assert_eq!(stats.bytes, out.len() as u64);
    out
}

fn per_key(label: &str, values: &[String], n: usize) {
    let keys = keys_of(values, n);
    if keys.is_empty() {
        return;
    }
    let k = keys.len() as f64;
    let floor = probe_floor(&keys) as f64 / k;
    let sizes: Vec<f64> = INTERVALS
        .iter()
        .map(|&i| build(&keys, i).len() as f64 / k)
        .collect();
    println!(
        "{label:>10} {n:>10} {:>10} {floor:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>7.2}",
        keys.len(),
        sizes[0],
        sizes[1],
        sizes[2],
        sizes[3],
        sizes[1] - floor
    );
}

fn whole_layout(label: &str, values: &[String], n: usize) {
    let keys = keys_of(values, n);
    if keys.is_empty() {
        return;
    }
    let present = present_count(values, n) as f64;
    // FLAT is the shipped `utf8` column: value bytes plus an i64 offset each.
    let flat = values[..n]
        .iter()
        .filter(|v| !v.is_empty())
        .map(|v| v.len() as u64)
        .sum::<u64>() as f64
        + 8.0 * present;
    let dict = build(&keys, 16).len() as f64;
    let floor = probe_floor(&keys) as f64;
    // DICT+C: the dictionary plus one u32 ordinal per present entity. Presence is common to both
    // layouts and reported separately by the parent campaign, so it is excluded here too.
    let real = dict + 4.0 * present;
    let modelled = floor + 4.0 * present;
    println!(
        "{label:>10} {n:>10} {:>8.1} {:>8.1} {:>8.1} {:>8.2} {:>7.2} {:>7.2}",
        flat / present,
        modelled / present,
        real / present,
        dict / keys.len() as f64,
        flat / modelled,
        flat / real
    );
}

fn timings(label: &str, values: &[String], n: usize) {
    let keys = keys_of(values, n);
    if keys.is_empty() {
        return;
    }
    // A deterministic scatter over the key space, so a lookup is a random access rather than a
    // sequential one — which is what both `contains` routes actually do.
    let sample: Vec<&String> = (0..10_000)
        .map(|i| &keys[(i * 7919) % keys.len()])
        .collect();
    let misses: Vec<String> = sample.iter().map(|k| format!("{k}~absent")).collect();

    for interval in INTERVALS {
        let bytes = build(&keys, interval);
        let dict = SortedDict::from_vec(bytes).unwrap();

        let t = Instant::now();
        let mut hits = 0u64;
        for key in &sample {
            if dict.resolve(key).unwrap().is_some() {
                hits += 1;
            }
        }
        let resolve_ns = t.elapsed().as_nanos() as f64 / sample.len() as f64;
        assert_eq!(hits, sample.len() as u64, "every sampled key is present");

        let t = Instant::now();
        let mut found = 0u64;
        for key in &misses {
            if dict.resolve(key).unwrap().is_some() {
                found += 1;
            }
        }
        let miss_ns = t.elapsed().as_nanos() as f64 / misses.len() as f64;
        assert_eq!(found, 0);

        let mut scratch = Vec::new();
        let t = Instant::now();
        let mut bytes_seen = 0u64;
        for i in 0..sample.len() {
            let ordinal = ((i * 7919) % keys.len()) as u32;
            bytes_seen += dict.key_of(ordinal, &mut scratch).unwrap().len() as u64;
        }
        let key_of_ns = t.elapsed().as_nanos() as f64 / sample.len() as f64;
        assert!(bytes_seen > 0);

        let t = Instant::now();
        let mut walked = 0u64;
        dict.walk(|_, key| walked += key.len() as u64).unwrap();
        let walk_ns = t.elapsed().as_nanos() as f64 / keys.len() as f64;
        assert!(walked > 0);

        println!(
            "{label:>10} {:>10} {interval:>4} {resolve_ns:>12.1} {miss_ns:>12.1} \
             {key_of_ns:>12.1} {walk_ns:>12.2}",
            keys.len()
        );
    }
}
