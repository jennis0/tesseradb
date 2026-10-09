//! **What a view's field tallies cost to derive as the corpus grows**: an entity's rank in the
//! entity-terms layer, by croaring's own rank and by the block rank table the reader uses, and the
//! whole tally pass of `mosaica_store::field_tallies::derive` over every row.
//!
//! The corpus is synthetic: entities `0..n·7/6` less every seventh, so the has-row bitmap is bitset
//! containers across its whole span; each entity carrying one or two of 64 keys, which makes about
//! 250 distinct key lists; and two fields held per entity, an integer and a float, each computed
//! from the entity. Row `r` is entity `r + r / 6`.
//!
//! ```text
//! field_tallies_scale <scratch dir> <rows>...
//! ```
//!
//! Each scale's entity-terms layer is written under the scratch directory and removed after it.

use std::time::Instant;

use mosaica_store::entity_terms::EntityTerms;
use mosaica_store::field_tallies::{derive, TallyField, TallySource};
use mosaica_types::scalar::Number;

fn entity_of_row(row: u32) -> u32 {
    row + row / 6
}

fn terms_of(entity: u32, out: &mut Vec<u32>) {
    out.clear();
    let a = entity.wrapping_mul(2_654_435_761) % 64;
    out.push(a);
    if entity.is_multiple_of(3) {
        let b = (a + 1 + entity % 5) % 64;
        if b > a {
            out.push(b);
        } else {
            out.insert(0, b);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scratch = std::path::PathBuf::from(&args[1]);
    for rows in args[2..].iter().map(|n| n.parse::<u32>().unwrap()) {
        let dir = scratch.join(format!("terms-{rows}"));
        std::fs::create_dir_all(&dir).unwrap();
        let started = Instant::now();
        let mut writer = mosaica_store::EntityTermsWriter::create(&dir).unwrap();
        let mut keys = Vec::new();
        for row in 0..rows {
            let entity = entity_of_row(row);
            terms_of(entity, &mut keys);
            writer.push(entity, &keys).unwrap();
        }
        writer.finish().unwrap();
        let written = started.elapsed().as_secs_f64();
        let terms = EntityTerms::open_dir(&dir).unwrap();

        // A spread of entities: each one's list read whole, then its rank alone by the block rank
        // table and by croaring's rank, over the has-row bitmap the layer was written with.
        let probes: Vec<u32> = (0..1_000_000u32)
            .map(|i| entity_of_row(((u64::from(i) * 2_654_435_761) % u64::from(rows)) as u32))
            .collect();
        let mut out = Vec::new();
        let started = Instant::now();
        let mut held = 0usize;
        for &entity in &probes {
            terms.terms_into(entity, &mut out).unwrap();
            held += out.len();
        }
        let lookup_ns = started.elapsed().as_nanos() as f64 / probes.len() as f64;
        let mut dense = croaring::Bitmap::new();
        dense.add_range(0..entity_of_row(rows - 1) + 1);
        for e in (6..=entity_of_row(rows - 1)).step_by(7) {
            dense.remove(e);
        }
        // As a writer leaves it: bitset containers, each holding its cardinality.
        dense.run_optimize();
        let table = mosaica_roaring::BlockRanks::of(&dense);
        let started = Instant::now();
        let ranked: u64 = probes
            .iter()
            .map(|&e| table.rank_of(&dense, e).unwrap())
            .sum();
        let table_ns = started.elapsed().as_nanos() as f64 / probes.len() as f64;
        let few = &probes[..10_000];
        let started = Instant::now();
        let croaring: u64 = few.iter().map(|&e| dense.rank(e)).sum();
        let croaring_ns = started.elapsed().as_nanos() as f64 / few.len() as f64;

        // The whole pass, two held fields, visited in entity order as a build and a fold visit
        // them.
        fn over(
            numbers: fn(u32) -> Option<Number>,
        ) -> impl Fn(&croaring::Bitmap, &mut dyn FnMut(u32, Number)) + Sync {
            move |entities, f| {
                for entity in entities.iter() {
                    if let Some(value) = numbers(entity) {
                        f(entity, value);
                    }
                }
            }
        }
        let int = over(|e| (e % 11 != 0).then(|| Number::Int(i128::from(e % 1_000))));
        let float = over(|e| (e % 13 != 0).then(|| Number::Float(f64::from(e) * 0.5)));
        let fields = [
            TallyField {
                name: "int".to_string(),
                float: false,
                source: TallySource::Held(&int),
            },
            TallyField {
                name: "float".to_string(),
                float: true,
                source: TallySource::Held(&float),
            },
        ];
        let row_of = |entity: u32| (entity % 7 != 6).then(|| entity - entity / 7);
        let lists = |span: std::ops::Range<u32>, f: &mut dyn FnMut(u32, &[u32])| {
            terms
                .for_each_in(span, f)
                .map_err(|e| std::io::Error::other(e.to_string()))
        };
        let path = scratch.join(format!("tallies-{rows}.bin"));
        let bound = u64::from(entity_of_row(rows - 1)) + 1;
        let started = Instant::now();
        let tallies = derive(bound, None, &row_of, &lists, &fields, &path).unwrap();
        assert_eq!(
            tallies.tallies.iter().map(|t| t[0].rows).sum::<u64>(),
            u64::from(rows)
        );
        let pass = started.elapsed().as_secs_f64();
        let size = std::fs::metadata(&path).unwrap().len();
        println!(
            "rows {rows}: layer written {written:.1} s; a list read {lookup_ns:.0} ns; rank \
             {table_ns:.0} ns by the table, {croaring_ns:.0} ns by croaring; pass {pass:.2} s, \
             {:.0} ns a row, {} lists, {size} bytes ({held} {ranked} {croaring})",
            pass * 1e9 / f64::from(rows),
            tallies.lists.len(),
        );
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&path).unwrap();
    }
}
