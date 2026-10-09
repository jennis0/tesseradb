//! **What a view's field tallies cost to hold and compose, by their shape**: a file of `lists` key
//! lists by `fields` fields, each tally full (eight extremes on each side, a sum spanning a few
//! words), written, read back as an open does, and composed for one field over every list as a
//! grant satisfying them all composes it.
//!
//! ```text
//! field_tallies_shape <scratch dir> <lists>x<fields>[:float]...
//! ```

use std::time::Instant;

use tessera_store::field_tallies::{
    read, write, ExactSum, FieldTallies, FieldTally, Sum, TallyMerge, RESERVE,
};
use tessera_types::scalar::Number;

fn tally(seed: u64, float: bool) -> FieldTally {
    let value = |k: u64| match float {
        true => Number::Float((seed * 31 + k) as f64 * 0.37),
        false => Number::Int(i128::from(seed * 31 + k)),
    };
    let sum = match float {
        true => {
            let mut exact = ExactSum::default();
            for k in 0..1_000 {
                exact.add_float((seed * 1_000 + k) as f64 * 0.37);
            }
            Sum::Float(exact.compact())
        }
        false => Sum::Int(i128::from(seed) * 1_000_000),
    };
    FieldTally {
        rows: 1_000,
        none: 10,
        count: 990,
        sum,
        low: (0..RESERVE as u64).map(|k| (value(k), k as u32)).collect(),
        high: (0..RESERVE as u64)
            .map(|k| (value(1_000 - k), (1_000 - k) as u32))
            .collect(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scratch = std::path::PathBuf::from(&args[1]);
    for shape in &args[2..] {
        let (dims, float) = match shape.split_once(':') {
            Some((dims, _)) => (dims, true),
            None => (shape.as_str(), false),
        };
        let (lists, fields) = dims.split_once('x').unwrap();
        let (lists, fields): (usize, usize) = (lists.parse().unwrap(), fields.parse().unwrap());
        let tallies = FieldTallies {
            fields: (0..fields).map(|f| (format!("f{f}"), float)).collect(),
            lists: (0..lists as u32).map(|l| vec![l]).collect(),
            tallies: (0..lists as u64)
                .map(|l| (0..fields).map(|_| tally(l, float)).collect())
                .collect(),
        };
        let path = scratch.join(format!("shape-{lists}x{fields}.bin"));
        write(&path, &tallies).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let runs = 20;
        let started = Instant::now();
        for _ in 0..runs {
            assert!(read(&path).unwrap().is_some());
        }
        let open_ms = started.elapsed().as_secs_f64() * 1e3 / f64::from(runs);
        let started = Instant::now();
        for _ in 0..runs {
            let mut merge = TallyMerge::of(FieldTally::default(), RESERVE);
            for per_list in tallies.tallies.iter().filter(|t| !t.is_empty()) {
                merge.add(&per_list[0]);
            }
            assert!(merge.finish().count > 0 || fields == 0);
        }
        let compose_ms = started.elapsed().as_secs_f64() * 1e3 / f64::from(runs);
        println!(
            "{shape}: {size} bytes, open {open_ms:.2} ms, compose one field over every list \
             {compose_ms:.3} ms"
        );
        std::fs::remove_file(&path).unwrap();
    }
}
