//! Does the category accelerator reach the **unselective** case arm 2 never asked about — and
//! what does the residual channel arm 2 left unmeasured actually measure?
//!
//! Arm 2 measured a *single* value's posting intersected against a candidate and found 0.13–49.5 ms
//! where the scan cost 785–5,271 ms. But every needle share it tried was a selective predicate; the
//! binding gap arm 7 then found is an **unselective** one — a tick-box `in` naming values that
//! together cover a quarter or half of the corpus. There the scan's cost is the *result's* size,
//! and the open question is whether `fast_or` over k postings followed by one intersection stays
//! container-priced or degenerates toward the corpus's 2,885 ms union warning.
//!
//! Two vocabulary shapes bracket it, as in arm 2: **correlated** (each value's members contiguous —
//! run containers, the shape signature sorting produces when the attribute follows the label set)
//! and **scattered** (uniform — every posting touches every container, the 1.01× end).
//!
//! The second question is the one arm 2's hidden-value section explicitly left open: a *scattered*
//! posting whose containers the candidate meets while **no bits match**. The intersection's work
//! there is container-proportional while the result is empty, so it is where a residual timing
//! channel against per-point-attributes §3.8 would live. `hidden-scattered` builds exactly that
//! pair — candidate = 25% contiguous with the value's members removed — and times it against a
//! value with no members at all.

use std::time::Instant;

use croaring::Bitmap;
use tessera_filter::{Codes, ValueColumn};
use tessera_types::AttrLocalId;

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const VALUES: u64 = 100;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(1_000_000_000);

    println!("n,shape,candidate,k,form,ms,hits");

    for shape in ["correlated", "scattered"] {
        // The column: 100 values, each covering 1% of the corpus, laid out per shape.
        let code_of = |e: u64| -> u8 {
            match shape {
                "correlated" => (e / (n / VALUES)).min(VALUES - 1) as u8,
                _ => (splitmix(e) % VALUES) as u8,
            }
        };
        let raw: Vec<u8> = (0..n).map(code_of).collect();

        // The derived postings, one per value — what the fold would build from the column.
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); VALUES as usize];
        for (e, &c) in raw.iter().enumerate() {
            members[c as usize].push(e as u32);
        }
        let postings: Vec<Bitmap> = members
            .iter()
            .map(|m| {
                let mut b = Bitmap::of(m);
                b.run_optimize();
                b
            })
            .collect();
        drop(members);
        let total_posting_bytes: u64 = postings
            .iter()
            .map(|p| p.get_serialized_size_in_bytes::<croaring::Portable>() as u64)
            .sum();
        eprintln!(
            "# {shape}: {VALUES} postings, {total_posting_bytes} serialized bytes total \
             (column is {n} bytes at u8)"
        );
        let column = ValueColumn::universal(Codes::U8(raw.into()));

        let mut broad = Bitmap::new();
        broad.add_range(0u32..(n / 4) as u32);
        broad.run_optimize();
        let mut all = Bitmap::new();
        all.add_range(0u32..n as u32);
        all.run_optimize();

        for (cname, cand) in [("broad-25pct", &broad), ("all", &all)] {
            // k values covering k% of the corpus: the tick-box shape, at the unselective end.
            for k in [5u32, 25, 50, 100] {
                let codes: Vec<AttrLocalId> = (0..k).map(AttrLocalId::new).collect();

                let run_scan = || column.scan_in(cand, &codes);
                let expected = run_scan();
                let t = Instant::now();
                let got = run_scan();
                let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
                assert!(got == expected);

                // Union then intersect — the ordering the built reader forces: postings resolve
                // over the whole corpus and meet the candidate afterwards (filter-surface §5).
                let run_union = || {
                    let refs: Vec<&Bitmap> = (0..k as usize).map(|i| &postings[i]).collect();
                    let mut u = Bitmap::fast_or(&refs);
                    u.and_inplace(cand);
                    u
                };
                let got = run_union();
                assert!(got == expected, "union+intersect disagreed with the scan");
                let t = Instant::now();
                let got = run_union();
                let union_ms = t.elapsed().as_secs_f64() * 1000.0;
                assert!(got == expected);

                for (form, ms) in [("scan", scan_ms), ("union-intersect", union_ms)] {
                    println!("{n},{shape},{cname},{k},{form},{ms:.2},{}", expected.cardinality());
                }
            }
        }

        // The unmeasured residual: containers meet, no bits match. Scattered shape only — a
        // correlated hidden value was measured flat in arm 2, and a scattered one has members in
        // every container, so this is the pair that could differ.
        if shape == "scattered" {
            let hidden = &postings[0]; // 1% of the corpus, uniformly scattered
            let mut disjoint = broad.clone();
            disjoint.andnot_inplace(hidden);
            let empty = Bitmap::new();

            let run_hidden = || disjoint.and(hidden);
            let run_absent = || disjoint.and(&empty);
            let _ = (run_hidden(), run_absent());
            let t = Instant::now();
            let got = run_hidden();
            let hid_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got.is_empty());
            let t = Instant::now();
            let got = run_absent();
            let abs_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got.is_empty());
            println!("{n},{shape},hidden-scattered,1,intersect-hidden,{hid_ms:.3},0");
            println!("{n},{shape},hidden-scattered,1,intersect-absent,{abs_ms:.3},0");
        }
    }
}
