//! The two families the scan arms never measured: **strings** and **category set-membership**.
//!
//! Arm 4 measured `scan_eq` over a `u32` code column, which is a category's *equality* case and
//! nothing else. This one covers what a real filter surface actually issues:
//!
//! - **text** — `eq`, `in`, `prefix`, `contains`, whose inner loop is offset arithmetic and a byte
//!   comparison rather than an integer compare, and whose values are variable-length;
//! - **category `in`** — a *k*-element set, whose per-element test is a lookup rather than a
//!   compare, at the widths a category is actually stored in.
//!
//! Same candidate shapes as the other arms so the numbers are comparable: 1% contiguous, 25% broad,
//! 1% scattered. `ns_per_candidate` is the constant to compare — the absolute ms differ with the
//! candidate's size.
//!
//! **The needle is chosen so the predicate is not trivially false.** A predicate that matches
//! nothing can be faster than one that matches — not through any early exit, which the design
//! forbids, but because a failing byte comparison exits at the first differing byte. Both a
//! matching and a non-matching needle are timed for `eq`, so the spread is visible rather than
//! assumed away.

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

/// A surname-shaped value: a small vocabulary of stems with a numeric tail, so values share long
/// common prefixes. That is the adversarial case for a byte comparison — a scan that exits on the
/// first differing byte gets no help from the first several — and it is also what a real string
/// column looks like.
fn value_for(e: u64) -> String {
    const STEMS: [&str; 8] = [
        "anderson", "andrews", "andrade", "bergstrom", "bergman", "castellano", "castleton",
        "delacroix",
    ];
    let h = splitmix(e);
    format!("{}-{:05}", STEMS[(h % 8) as usize], h % 100_000)
}

fn candidates(n: u64) -> Vec<(&'static str, Bitmap)> {
    let mut contiguous = Bitmap::new();
    contiguous.add_range((n / 3) as u32..(n / 3 + n / 100) as u32);
    contiguous.run_optimize();

    let mut broad = Bitmap::new();
    broad.add_range(0u32..(n / 4) as u32);
    broad.run_optimize();

    let mut scattered = Bitmap::new();
    let mut v: Vec<u32> = Vec::new();
    for e in 0..n {
        if splitmix(e ^ 0x5EED) % 100 == 0 {
            v.push(e as u32);
        }
    }
    scattered.add_many(&v);
    scattered.run_optimize();

    vec![
        ("sparse-contiguous-1pct", contiguous),
        ("broad-25pct", broad),
        ("sparse-scattered-1pct", scattered),
    ]
}

fn time(label: &str, n: u64, family: &str, cand: (&str, &Bitmap), f: impl Fn() -> Bitmap) {
    let _ = f(); // one untimed pass, so the measurement is warm
    let t = Instant::now();
    let hits = f();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let c = cand.1.cardinality();
    println!(
        "{n},{family},{label},{},{c},{ms:.2},{:.2},{}",
        cand.0,
        ms * 1e6 / c as f64,
        hits.cardinality()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(100_000_000);

    println!("n,family,predicate,candidate,candidate_entities,ms,ns_per_candidate,hits");

    let cands = candidates(n);

    // ---- text ----------------------------------------------------------------------------
    {
        let text = ValueColumn::universal(Codes::text((0..n).map(value_for)));
        // A value that exists: whatever entity 7 carries. `miss` shares its stem, so the comparison
        // cannot exit on the first byte.
        let hit = value_for(7);
        let miss = format!("{}-XXXXX", &hit[..hit.len() - 6]);
        let prefix_long = &hit[..hit.len() - 6]; // the whole stem: ~1/8 of the corpus
        let prefix_short = &hit[..3]; // three bytes: several stems
        let needles: Vec<String> = (0..5).map(|k| value_for(k * 1_000 + 7)).collect();

        for c in &cands {
            let cd = (c.0, &c.1);
            time("text-eq-hit", n, "text", cd, || text.scan_text_eq(&c.1, &hit));
            time("text-eq-miss", n, "text", cd, || {
                text.scan_text_eq(&c.1, &miss)
            });
            time("text-in-5", n, "text", cd, || {
                text.scan_text_in(&c.1, &needles)
            });
            time("text-prefix-stem", n, "text", cd, || {
                text.scan_text_prefix(&c.1, prefix_long)
            });
            time("text-prefix-3", n, "text", cd, || {
                text.scan_text_prefix(&c.1, prefix_short)
            });
            time("text-contains", n, "text", cd, || {
                text.scan_text_contains(&c.1, "-000")
            });
        }
    }

    // ---- category set membership ---------------------------------------------------------
    for (width, column) in [
        (
            "u8",
            ValueColumn::universal(Codes::U8(
                (0..n)
                    .map(|e| (splitmix(e) % 200) as u8)
                    .collect::<Vec<_>>()
                    .into(),
            )),
        ),
        (
            "u16",
            ValueColumn::universal(Codes::U16(
                (0..n)
                    .map(|e| (splitmix(e) % 50_000) as u16)
                    .collect::<Vec<_>>()
                    .into(),
            )),
        ),
        (
            "u32",
            ValueColumn::universal(Codes::U32(
                (0..n)
                    .map(|e| (splitmix(e) % 1_000_000) as u32)
                    .collect::<Vec<_>>()
                    .into(),
            )),
        ),
    ] {
        let domain: u32 = match width {
            "u8" => 200,
            "u16" => 50_000,
            _ => 1_000_000,
        };
        for c in &cands {
            let cd = (c.0, &c.1);
            time(&format!("{width}-eq"), n, "category", cd, || {
                column.scan_eq(&c.1, AttrLocalId::new(42))
            });
            for k in [2usize, 8, 32] {
                let set: Vec<AttrLocalId> = (0..k)
                    .map(|i| AttrLocalId::new((i as u32 * 7 + 3) % domain))
                    .collect();
                time(&format!("{width}-in-{k}"), n, "category", cd, || {
                    column.scan_in(&c.1, &set)
                });
            }
        }
    }
}
