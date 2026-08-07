//! **How much does a per-value membership set cost, and how contiguous is it?**
//!
//! A global category legend answers, per value, "can this principal see any member" — and, when
//! counts arrive, "how many". Both are `members(v) ∩ M_auth` over an entity-space bitmap per
//! `(column, code)`. Whether that is a modest artefact or a serious one is decided by something
//! neither the design nor the minting memo measured: **how contiguous category membership is in
//! entity space.**
//!
//! It is not obviously good. Entity ids are assigned in *signature-sorted* order (§11.1) — by
//! auth labels — so a category's members are contiguous only insofar as the category correlates
//! with the label set. The repo's cost model is that bitmap operations cost O(containers
//! touched), not O(cardinality), and the probes measured up to ~130× on union cost with
//! cardinality held constant. Contiguity therefore decides both the artefact's size and the
//! legend's latency, and nothing in the corpus predicts it.
//!
//! What this reports, per category value and in total:
//!
//! - **cardinality, containers, and members per container** — the direct read on contiguity.
//!   65,536 per container is a perfectly packed run; 1 is a fully scattered set.
//! - **serialised bytes**, against the `columns.arrow` tail the same data already costs — so the
//!   figure is comparable rather than absolute.
//! - **legend latency**: the whole per-principal evaluation, every value intersected against one
//!   mask, across principals of varying width and shape.
//!
//! **The threshold is a session, not a frame.** Per-viewport category counts are deferred to the
//! filter contract (§8.2, #43); the legend is served as metadata behind a cache keyed by
//! `(auth fingerprint, generation, overlay version)`, so this runs about once per session rather
//! than once per pan. A figure that would be unacceptable on the viewport path may be fine here.
//!
//! **What this does not measure**, so nobody mistakes it for the whole answer: the composed
//! verdict. §3.3 requires visibility to be evaluated against the fragment *with the overlay
//! applied*, because a suppression never touches postings (Rule S) and a fragment alone still
//! contains suppressed items. This unions raw postings — the cost shape, not the correctness
//! shape. Composition adds an `andnot` per generation rather than per value, so it does not move
//! the conclusion, but these figures are a floor.
//!
//! Self-contained, like its sibling bins: `tessera-bench` has no lib target, so the three helpers
//! it would otherwise borrow from `corpus`/`metrics`/`postings` are inlined below.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin category_membership -- <bundle root> [column]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use croaring::{Bitmap, Portable};

use tessera_authz::{PostingRef, PostingsReader};
use tessera_store::manifest::Manifest;
use tessera_store::permutation::Permutation;
use tessera_store::read::{ColumnsRef, ScalarSlice};
use tessera_types::{EntityId, TermId};

/// One term's postings as an owned bitmap — `tessera_bench::postings::to_bitmap`, inlined.
/// The tag-0 path copies, so this is setup-only and never appears in a timed loop.
fn to_bitmap(postings: &PostingsReader, term: TermId) -> Bitmap {
    let Some(posting) = postings.posting(term).expect("posting") else {
        return Bitmap::new();
    };
    match posting {
        PostingRef::Roaring(view) => (*view).clone(),
        PostingRef::Array(bytes) => {
            let mut values: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            values.sort_unstable();
            let mut bitmap = Bitmap::new();
            bitmap.add_many(&values);
            bitmap
        }
    }
}

/// Containers a bitmap touches — `tessera_bench::metrics::containers`, inlined. Roaring iteration
/// is ascending, so a change in the high half is a new container and no set is needed.
fn containers(bitmap: &Bitmap) -> u64 {
    let mut count = 0u64;
    let mut last_high: Option<u32> = None;
    for value in bitmap.iter() {
        let high = value >> 16;
        if last_high != Some(high) {
            count += 1;
            last_high = Some(high);
        }
    }
    count
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next().map(PathBuf::from) else {
        eprintln!("usage: category_membership <bundle root> [column]");
        std::process::exit(2);
    };
    let column = args
        .next()
        .unwrap_or_else(|| "primary_category".to_string());

    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT")).unwrap();
    let prefix = current["prefix"].as_str().expect("CURRENT.prefix");
    let prefix_dir = root.join(prefix);
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(prefix_dir.join("MANIFEST.json")).unwrap()).unwrap();

    // Report values by key, not by code: a table of bare integers cannot be checked against the
    // corpus by eye, and this probe's first job is to be believable.
    let key_of: BTreeMap<u32, String> = manifest
        .declared_scalars
        .iter()
        .find(|d| d.name == column)
        .and_then(|d| d.vocabulary.as_ref())
        .and_then(|name| manifest.vocabularies.iter().find(|v| &v.name == name))
        .map(|v| v.values.iter().map(|x| (x.code, x.key.clone())).collect())
        .unwrap_or_default();

    let part_dir = prefix_dir.join("partitions/default");
    let slice_dir = part_dir.join("slices/s0");
    let permutation = Permutation::load(&slice_dir.join("permutation.bin")).expect("permutation");
    let columns =
        ColumnsRef::load(&slice_dir.join("segments/seg-0/columns.arrow")).expect("columns.arrow");

    let codes: Vec<u32> = match columns.scalar(&column) {
        Some(ScalarSlice::U8(v)) => v.iter().map(|c| *c as u32).collect(),
        Some(ScalarSlice::U16(v)) => v.iter().map(|c| *c as u32).collect(),
        Some(ScalarSlice::U32(v)) => v.to_vec(),
        other => {
            eprintln!("column '{column}' is not a category width: {other:?}");
            std::process::exit(2);
        }
    };

    // **Walk entities and resolve to rows, never the reverse.** The bitmaps are entity-space
    // objects and the mask they meet is an entity-space object; building them from row ids would
    // produce sets that look identical in aggregate and are wrong per element — the same defect
    // class as this branch's source-id bug, which no aggregate check caught.
    let build_start = Instant::now();
    let mut members: BTreeMap<u32, Bitmap> = BTreeMap::new();
    let bound = permutation.bound();
    let mut placed = 0u64;
    for entity in 0..bound {
        let Some(row) = permutation.row_of(EntityId::new(entity)) else {
            continue; // no row in this slice
        };
        let code = codes[row.raw() as usize];
        // Code 0 is the *absent* sentinel: a row carrying it is a member of nothing, not a member
        // of a value called zero.
        if code != 0 {
            members.entry(code).or_default().add(entity as u32);
            placed += 1;
        }
    }
    let build_secs = build_start.elapsed().as_secs_f64();

    println!("# category membership — {} / {column}", root.display());
    println!(
        "\n{} values over {placed} members of {bound} entities, built in {build_secs:.2}s\n",
        members.len()
    );

    let mut total_bytes = 0usize;
    let mut total_containers = 0u64;
    let mut rows: Vec<(u32, u64, u64, usize)> = Vec::new();
    for (&code, bitmap) in &members {
        let mut optimised = bitmap.clone();
        // Run-encode before measuring: it is what a writer would store, and the difference it
        // makes *is* the contiguity this probe is about.
        optimised.run_optimize();
        // Portable format: the same encoding `postings.arrow` stores, so the figure is the
        // artefact's real size rather than an in-memory one.
        let bytes = optimised.get_serialized_size_in_bytes::<Portable>();
        let container_count = containers(bitmap);
        total_bytes += bytes;
        total_containers += container_count;
        rows.push((code, bitmap.cardinality(), container_count, bytes));
    }

    let column_bytes = codes.len()
        * match columns.scalar(&column) {
            Some(ScalarSlice::U8(_)) => 1,
            Some(ScalarSlice::U16(_)) => 2,
            _ => 4,
        };

    println!("## the artefact");
    println!(
        "{} containers total, {:.2} MiB run-optimised, mean {:.0} members/container",
        total_containers,
        total_bytes as f64 / (1024.0 * 1024.0),
        placed as f64 / total_containers.max(1) as f64
    );
    println!(
        "the render column is {:.2} MiB, so membership costs {:.2}x the column it indexes\n",
        column_bytes as f64 / (1024.0 * 1024.0),
        total_bytes as f64 / column_bytes.max(1) as f64
    );

    rows.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
    println!("## the ten widest values, and the five narrowest");
    println!("| value | members | containers | members/container | bytes |");
    println!("|---|---|---|---|---|");
    let narrowest: Vec<_> = rows.iter().rev().take(5).cloned().collect();
    for (code, cardinality, container_count, bytes) in
        rows.iter().take(10).chain(narrowest.iter().rev())
    {
        println!(
            "| {} | {cardinality} | {container_count} | {:.0} | {bytes} |",
            key_of
                .get(code)
                .cloned()
                .unwrap_or_else(|| code.to_string()),
            *cardinality as f64 / (*container_count).max(1) as f64
        );
    }

    // ---- legend latency --------------------------------------------------------------------
    let postings =
        PostingsReader::open(&part_dir.join("terms/postings.arrow"), true).expect("postings");
    let mut widths: Vec<(TermId, u64)> = (0..postings.term_count())
        .map(|t| {
            let term = TermId::new(t);
            (term, to_bitmap(&postings, term).cardinality())
        })
        .filter(|(_, cardinality)| *cardinality > 0)
        .collect();
    widths.sort_unstable_by_key(|w| std::cmp::Reverse(w.1));

    println!("\n## legend latency — every value intersected against one principal's mask");
    println!("| principal | terms | mask cardinality | visible values | evaluate |");
    println!("|---|---|---|---|---|");
    for (label, terms) in [
        ("tail w=1", widths.iter().rev().take(1).collect::<Vec<_>>()),
        ("tail w=8", widths.iter().rev().take(8).collect()),
        ("tail w=64", widths.iter().rev().take(64).collect()),
        ("head w=1", widths.iter().take(1).collect()),
        ("head w=8", widths.iter().take(8).collect()),
        ("head w=64", widths.iter().take(64).collect()),
    ] {
        let mut mask = Bitmap::new();
        for (term, _) in &terms {
            mask.or_inplace(&to_bitmap(&postings, *term));
        }
        mask.run_optimize();

        // `and_cardinality`, not a non-empty test: the count is the more expensive of the two and
        // the design wants both out of one structure, so timing the cheaper one would flatter it.
        let start = Instant::now();
        let mut visible = 0usize;
        for bitmap in members.values() {
            if bitmap.and_cardinality(&mask) > 0 {
                visible += 1;
            }
        }
        let elapsed = start.elapsed();

        println!(
            "| {label} | {} | {} | {visible}/{} | {:.3} ms |",
            terms.len(),
            mask.cardinality(),
            members.len(),
            elapsed.as_secs_f64() * 1000.0
        );
    }

    println!(
        "\n§3.3 predicts the intersection is **cheapest where the principal is least privileged** \
         — the opposite of I7's usual asymmetry. Read the tail rows against the head rows."
    );
}
