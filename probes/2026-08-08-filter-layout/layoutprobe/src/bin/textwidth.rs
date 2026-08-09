//! What halving the text offsets is worth — the "next thing to try" arm 6 recorded and did not take.
//!
//! A text scan streams two 8-byte offsets and the value's bytes per candidate entity — ~22 bytes
//! for a surname-shaped column against a `u32` column's 4 — and arm 6 measured it at memory
//! bandwidth for that shape, so the only lever left is fewer bytes. `i32` offsets take a quarter of
//! the offset traffic back (~a fifth of the total for this shape). Both walks here are the same
//! reimplemented loop over the same bytes, differing **only** in the offset type, so the delta is
//! the width and nothing else. This does not measure the shipped code — the shipped column has no
//! `i32` offset path to measure — so it prices the change, not the integration.
//!
//! The capacity limit is real and per column: Arrow `Utf8` offsets are `i32`, capping a column's
//! concatenated bytes at 2 GiB, which a 10⁹-entity column passes at two bytes a value
//! (`filter-index.md` §2.5). So a narrow-offset column is a per-column build choice where the bytes
//! fit, never the format; the reader keeps both paths.

use std::time::Instant;

use croaring::Bitmap;

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn value_for(e: u64) -> String {
    const STEMS: [&str; 8] = [
        "anderson", "andrews", "andrade", "bergstrom", "bergman", "castellano", "castleton",
        "delacroix",
    ];
    let h = splitmix(e);
    format!("{}-{:05}", STEMS[(h % 8) as usize], h % 100_000)
}

/// The candidate-run walk over `(bytes, offsets)`, generic in the offset width. The body mirrors
/// `ValueColumn::walk_text`'s offset-pair loop; the run structure is a contiguous candidate's.
fn walk<O: Copy + TryInto<usize>>(
    lo: u32,
    hi: u32,
    bytes: &[u8],
    offsets: &[O],
    mut pred: impl FnMut(&[u8]) -> bool,
    hits: &mut Vec<u32>,
) where
    <O as TryInto<usize>>::Error: std::fmt::Debug,
{
    hits.clear();
    for (i, w) in offsets[lo as usize..=hi as usize].windows(2).enumerate() {
        let (a, b) = (w[0].try_into().unwrap(), w[1].try_into().unwrap());
        if pred(&bytes[a..b]) {
            hits.push(lo + i as u32);
        }
    }
}

#[inline]
fn byte_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if haystack.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    haystack[..=last_start]
        .iter()
        .enumerate()
        .any(|(i, &b)| b == first && &haystack[i..i + needle.len()] == needle)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(100_000_000);

    let mut bytes: Vec<u8> = Vec::new();
    let mut off64: Vec<i64> = vec![0];
    for e in 0..n {
        bytes.extend_from_slice(value_for(e).as_bytes());
        off64.push(bytes.len() as i64);
    }
    assert!(bytes.len() < i32::MAX as usize, "fixture must fit i32 offsets");
    let off32: Vec<i32> = off64.iter().map(|&o| o as i32).collect();

    // A 25% contiguous candidate — the broad shape the per-candidate constants are quoted at.
    let (clo, chi) = (0u32, (n / 4) as u32);
    let mut cand = Bitmap::new();
    cand.add_range(clo..chi);

    let hit = value_for(7);
    let needle_eq = hit.as_bytes();
    let needle_sub = &hit.as_bytes()[2..7];

    println!("n,predicate,width,ms,ns_per_candidate,hits");
    let mut hits: Vec<u32> = Vec::new();
    let mut expect_eq = 0usize;
    let mut expect_sub = 0usize;
    for (pname, expected) in [("eq", &mut expect_eq), ("contains", &mut expect_sub)] {
        for width in ["i64", "i32"] {
            let go = |hits: &mut Vec<u32>| match (pname, width) {
                ("eq", "i64") => walk(clo, chi, &bytes, &off64, |v| v == needle_eq, hits),
                ("eq", "i32") => walk(clo, chi, &bytes, &off32, |v| v == needle_eq, hits),
                ("contains", "i64") => {
                    walk(clo, chi, &bytes, &off64, |v| byte_contains(v, needle_sub), hits)
                }
                _ => walk(clo, chi, &bytes, &off32, |v| byte_contains(v, needle_sub), hits),
            };
            go(&mut hits);
            if width == "i64" {
                *expected = hits.len();
            } else {
                assert_eq!(hits.len(), *expected, "widths disagreed");
            }
            let t = Instant::now();
            go(&mut hits);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let ns = ms * 1e6 / f64::from(chi - clo);
            println!("{n},{pname},{width},{ms:.1},{ns:.2},{}", hits.len());
        }
    }
}
