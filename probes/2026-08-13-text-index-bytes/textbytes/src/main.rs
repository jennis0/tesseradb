//! What a text column's token index costs **on disk, through the shipped writers**.
//!
//! `2026-08-12-string-storage` arm 3 put the index at 22.7–23.6 B/entity and
//! `records-and-search.md` §4.4 quotes 23.6. That figure is an *accounting* — a sum of modelled
//! per-posting costs — and a model of a format is not the format: the same gap between a floor and
//! the decodable bytes cost the dictionary campaign 2.0–2.9 B/key when it was measured. This
//! campaign replaces the model with `write_sorted_dict` and `write_postings`' own output.
//!
//! Usage (from the repository root, so `.cargo/config.toml`'s alignment pin applies):
//!     cargo run --release --manifest-path probes/2026-08-13-text-index-bytes/textbytes/Cargo.toml \
//!         -- --snapshot ~/.cache/kagglehub/.../arxiv-metadata-oai-snapshot.json

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

use tessera_analyse::Analyser;
use tessera_types::SMALL_TERM_THRESHOLD_DEFAULT;

fn main() {
    let mut snapshot = String::new();
    let mut scales = vec![250_000usize, 1_000_000, 2_400_000];
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
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(!snapshot.is_empty(), "--snapshot is required");
    let path = if let Some(rest) = snapshot.strip_prefix("~/") {
        format!("{}/{rest}", std::env::var("HOME").unwrap())
    } else {
        snapshot
    };
    let limit = *scales.iter().max().unwrap();

    let file = std::fs::File::open(&path).expect("the arXiv snapshot");
    let mut titles: Vec<String> = Vec::with_capacity(limit);
    let mut abstracts: Vec<String> = Vec::with_capacity(limit);
    for line in BufReader::with_capacity(1 << 20, file).lines() {
        if titles.len() >= limit {
            break;
        }
        let line = line.expect("a readable line");
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let field = |name: &str| {
            record
                .get(name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        titles.push(field("title"));
        abstracts.push(field("abstract"));
    }

    println!("# text index bytes on disk, through the shipped writers\n");
    println!("| column | entities | terms | flat B/e | dict B/e | postings B/e | **index B/e** | ratio |");
    println!("|---|---|---|---|---|---|---|---|");
    for (name, values) in [("title", &titles), ("abstract", &abstracts)] {
        for &scale in &scales {
            if scale > values.len() {
                continue;
            }
            report(name, &values[..scale]);
        }
    }
}

fn report(name: &str, values: &[String]) {
    let analyser = Analyser::new();
    let mut terms: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (entity, prose) in values.iter().enumerate() {
        let entity = entity as u32;
        for token in analyser.tokens(prose) {
            let postings = terms.entry(token).or_default();
            if postings.last() != Some(&entity) {
                postings.push(entity);
            }
        }
    }

    let dir = tempfile::tempdir().expect("a temp dir");
    let dict_path = dir.path().join("dict.bin");
    tessera_filter::write_sorted_dict(&dict_path, terms.keys().map(String::as_str))
        .expect("the dictionary writes");
    let per_term: Vec<Vec<u32>> = terms.values().cloned().collect();
    let postings_path = dir.path().join("postings.arrow");
    tessera_authz::postings::write_postings(&postings_path, &per_term, SMALL_TERM_THRESHOLD_DEFAULT)
        .expect("the postings write");

    let bytes = |p: &std::path::Path| std::fs::metadata(p).expect("a written file").len() as f64;
    let n = values.len() as f64;
    let dict = bytes(&dict_path) / n;
    let postings = bytes(&postings_path) / n;
    // The flat column as the retired `utf8` writer would have held it: the value bytes plus Arrow's
    // 32-bit offset per row. The column itself is deleted, so this is computed rather than written
    // — stated because every other figure in this table is a file on disk.
    let flat = (values.iter().map(|v| v.len()).sum::<usize>() as f64) / n + 4.0;
    println!(
        "| `{name}` | {} | {} | {:.2} | {:.2} | {:.2} | **{:.2}** | {:.2}× |",
        values.len(),
        terms.len(),
        flat,
        dict,
        postings,
        dict + postings,
        flat / (dict + postings),
    );
}
