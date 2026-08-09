//! Can text `contains` be brought inside the budget, and by which of the levers `textdecomp`
//! licenses?
//!
//! The decomposition says the binding term is the **byte-scan loop itself** — first-byte search
//! plus verification is ~70% of the contiguous cell and over half of the scattered one — with the
//! two random cache lines (offsets, value bytes) the second term on a scattered candidate and the
//! traversal a rounding error. So the levers, in the order the decomposition ranks them: a better
//! search loop; then a layout that cuts a random line; and nothing that only touches the
//! traversal.
//!
//! Arms, all asserted bitmap-equal to the shipped scan before a timing is printed:
//!
//! - `shipped` — `ValueColumn::scan_text_contains`, the baseline.
//! - `memmem-value` — `memchr::memmem::Finder` built once per scan, `find` per value. Kept as the
//!   obvious option even though a first pass measured it *slower* than the scalar loop on
//!   ~14-byte values: the finder's per-call dispatch costs more than it saves on a haystack that
//!   fits in two words. The negative result is the point.
//! - `memmem-concat` — a contiguous slot run's value bytes are one contiguous region, so search
//!   the *region* with one `find_iter` and map hits back to slots through the offsets, discarding
//!   matches that straddle a value boundary. The search runs at SIMD throughput and the per-value
//!   dispatch disappears; single-slot runs (a scattered candidate) fall back per value.
//! - `swar-value` — the shipped loop's shape with the first-byte scan done eight bytes at a time
//!   (the standard SWAR zero-byte trick) and a scalar tail. No new storage, no dependency; tests
//!   how much of the gap a better inner loop closes on its own.
//! - `bloom8` — a 1-byte per-value **byte** bloom consulted before anything else. Expected
//!   worthless — every value draws from the same small alphabet — and measured to show it.
//! - `tri16` — a 2-byte per-value **trigram** bloom (each of a value's ~12 trigrams hashed to one
//!   of 16 bits, needle passes if all its trigram bits are present). A needle shorter than 3
//!   bytes degenerates to "consult nothing". SWAR verify on pass.
//! - `desc64` — the layout option: one `u64` per value packing `offset:36 | len:8 | trigram
//!   bloom:16`, replacing the offsets array byte-for-byte (8 B/value either way; values ≤ 255
//!   bytes, longer ones would need an escape). A scattered candidate's reject path touches
//!   **one** random line instead of two — length reject and bloom reject read nothing else — and
//!   only a bloom pass touches the value bytes, SWAR-verified.
//! - `desc64+tri32` — desc64 plus a second-stage 4-byte trigram bloom (independent hash)
//!   consulted only by desc64 survivors, pricing what a wider summary buys: the 16-bit bloom in
//!   the descriptor is ~53% dense at ~12 trigrams per value, so a single-trigram needle passes
//!   half of everything; 32 more bits take the joint single-trigram false-positive rate to ~17%
//!   at +4 B/value.
//!
//! A first pass verified prefilter hits through `memmem::Finder` and the finder's call overhead
//! cancelled the prefilter's win on scattered candidates (`qzx` scattered measured *worse* than
//! shipped, 120–135 ns against 94); every prefiltered arm therefore verifies with the SWAR loop.
//!
//! Needles cover the three shapes that price differently: `-000` (arm 6's needle, ~10⁻³ of
//! values), `erg` (interior of two stems, ~25% of values — verification-heavy, the adversarial
//! case for any prefilter), `qzx` (absent from the corpus — a prefilter's best case, and the
//! shape where its false-positive rate is nakedly visible). A `stats` mode prints the
//! trigram-vocabulary counts a postings estimate needs, instead of building postings arm 9
//! already prices at ~2 B per posting entry when scattered. A `lite` mode drops the bloom8/tri16
//! arms so the 10⁹ confirmation fits in RAM beside the flat column.

use std::hint::black_box;
use std::time::Instant;

use croaring::Bitmap;
use memchr::memmem;
use tessera_filter::{Codes, ValueColumn};

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `textscan`'s generator, unchanged.
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

fn for_each_run(bitmap: &Bitmap, mut f: impl FnMut(u32, u32)) {
    let mut cursor = bitmap.cursor();
    let mut buf = [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; 64];
    loop {
        let n = cursor.read_many_ranges(&mut buf);
        if n == 0 {
            return;
        }
        for r in &buf[..n] {
            f(r.start, r.last);
        }
    }
}

/// `byte_contains` with the first-byte scan taken eight bytes at a time: broadcast the needle's
/// first byte, XOR, and locate zero bytes with the standard SWAR mask. Candidate positions then
/// verify exactly as the scalar loop does. Same answer as the shipped `byte_contains` on every
/// input; only the search order over first-byte hits differs, and both stop at the first match.
#[inline]
fn swar_contains(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    let first = needle[0];
    let last_start = hay.len() - n;
    let pat = (first as u64).wrapping_mul(0x0101_0101_0101_0101);
    let mut i = 0usize;
    while i + 8 <= last_start + 1 {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap());
        let x = w ^ pat;
        let mut m = x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080;
        while m != 0 {
            let j = i + (m.trailing_zeros() >> 3) as usize;
            if &hay[j..j + n] == needle {
                return true;
            }
            m &= m - 1;
        }
        i += 8;
    }
    while i <= last_start {
        if hay[i] == first && &hay[i..i + n] == needle {
            return true;
        }
        i += 1;
    }
    false
}

#[inline]
fn tri_bit16(t: &[u8]) -> u16 {
    let key = t[0] as u64 | (t[1] as u64) << 8 | (t[2] as u64) << 16;
    1u16 << (splitmix(key) % 16)
}

#[inline]
fn tri_mask16(v: &[u8]) -> u16 {
    let mut m = 0u16;
    for w in v.windows(3) {
        m |= tri_bit16(w);
    }
    m
}

/// Second-stage 32-bit trigram bloom, hashed independently of the descriptor's 16-bit one so the
/// two rejects compose.
#[inline]
fn tri_bit32(t: &[u8]) -> u32 {
    let key = t[0] as u64 | (t[1] as u64) << 8 | (t[2] as u64) << 16;
    1u32 << (splitmix(key ^ 0xA5A5_5A5A) % 32)
}

#[inline]
fn tri_mask32(v: &[u8]) -> u32 {
    let mut m = 0u32;
    for w in v.windows(3) {
        m |= tri_bit32(w);
    }
    m
}

#[inline]
fn byte_mask8(v: &[u8]) -> u8 {
    let mut m = 0u8;
    for &b in v {
        m |= 1u8 << (splitmix(b as u64) % 8);
    }
    m
}

fn time_arm(
    n: u64,
    label: &str,
    needle: &str,
    cand: (&str, &Bitmap),
    expect: &Bitmap,
    mut f: impl FnMut() -> Bitmap,
) {
    let warm = f();
    assert_eq!(&warm, expect, "{label} disagrees with the shipped scan");
    let t = Instant::now();
    let hits = black_box(f());
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let c = cand.1.cardinality();
    println!(
        "{n},{label},{needle},{},{c},{ms:.3},{:.2},{}",
        cand.0,
        ms * 1e6 / c as f64,
        hits.cardinality()
    );
}

/// Trigram-vocabulary statistics for pricing a postings route without building one: distinct
/// trigrams corpus-wide, and total posting entries (Σ over values of distinct-trigrams-in-value).
fn stats(n: u64) {
    use std::collections::HashMap;
    let mut vocab: HashMap<u32, u64> = HashMap::new();
    let mut entries: u64 = 0;
    let mut local = [0u32; 32];
    for e in 0..n {
        let v = value_for(e);
        let b = v.as_bytes();
        let mut k = 0usize;
        for w in b.windows(3) {
            let t = w[0] as u32 | (w[1] as u32) << 8 | (w[2] as u32) << 16;
            if !local[..k].contains(&t) {
                local[k] = t;
                k += 1;
            }
        }
        entries += k as u64;
        for &t in &local[..k] {
            *vocab.entry(t).or_insert(0) += 1;
        }
    }
    println!(
        "stats,n={n},distinct_trigrams={},posting_entries={entries},entries_per_value={:.2}",
        vocab.len(),
        entries as f64 / n as f64
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(100_000_000);
    let mode = args.get(2).map(String::as_str).unwrap_or("");
    if mode == "stats" {
        stats(n);
        return;
    }
    if mode == "fuzz" {
        // `swar_contains` against `str::contains` on shapes the timed arms do not exercise:
        // empty needles, needles longer than the value, matches at every alignment, and values
        // spanning the 8-byte SWAR boundary.
        let alphabet = b"ab0-";
        for seed in 0..200_000u64 {
            let h = splitmix(seed);
            let vlen = (h % 24) as usize;
            let nlen = (splitmix(h) % 6) as usize;
            let v: Vec<u8> = (0..vlen)
                .map(|i| alphabet[(splitmix(h ^ i as u64) % 4) as usize])
                .collect();
            let nd: Vec<u8> = (0..nlen)
                .map(|i| alphabet[(splitmix(!h ^ i as u64) % 4) as usize])
                .collect();
            let expect = std::str::from_utf8(&v)
                .unwrap()
                .contains(std::str::from_utf8(&nd).unwrap());
            assert_eq!(
                swar_contains(&v, &nd),
                expect,
                "swar_contains({v:?}, {nd:?})"
            );
        }
        println!("fuzz,ok,200000");
        return;
    }
    let lite = mode == "lite";

    println!("n,arm,needle,candidate,candidate_entities,ms,ns_per_candidate,hits");

    let cands = candidates(n);
    let needles = ["-000", "erg", "qzx"];

    // Shipped baselines first; the column is then dropped so the peak stays near one column.
    let mut expected: Vec<Vec<Bitmap>> = Vec::new(); // [needle][candidate]
    {
        let text = ValueColumn::universal(Codes::text((0..n).map(value_for)));
        for needle in &needles {
            let mut per_cand = Vec::new();
            for c in &cands {
                let warm = text.scan_text_contains(&c.1, needle);
                let t = Instant::now();
                let hits = black_box(text.scan_text_contains(&c.1, needle));
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(warm, hits);
                let cc = c.1.cardinality();
                println!(
                    "{n},shipped,{needle},{},{cc},{ms:.3},{:.2},{}",
                    c.0,
                    ms * 1e6 / cc as f64,
                    hits.cardinality()
                );
                per_cand.push(hits);
            }
            expected.push(per_cand);
        }
    }

    // The flat column and the summary structures the arms price.
    // Reserved up front: at 10⁹ a doubling realloc's transient copy would not fit beside the rest.
    let mut bytes: Vec<u8> = Vec::with_capacity(n as usize * 16);
    let mut offsets: Vec<i64> = Vec::with_capacity(n as usize + 1);
    offsets.push(0);
    for e in 0..n {
        bytes.extend_from_slice(value_for(e).as_bytes());
        offsets.push(bytes.len() as i64);
    }
    let (bloom8, tri16): (Vec<u8>, Vec<u16>) = if lite {
        (Vec::new(), Vec::new())
    } else {
        (
            (0..n as usize)
                .map(|k| byte_mask8(&bytes[offsets[k] as usize..offsets[k + 1] as usize]))
                .collect(),
            (0..n as usize)
                .map(|k| tri_mask16(&bytes[offsets[k] as usize..offsets[k + 1] as usize]))
                .collect(),
        )
    };
    let tri32: Vec<u32> = (0..n as usize)
        .map(|k| tri_mask32(&bytes[offsets[k] as usize..offsets[k + 1] as usize]))
        .collect();
    // offset:36 | len:8 | bloom:16 — one u64 per value, byte-for-byte what the offsets array cost.
    assert!(bytes.len() < (1usize << 36), "desc64 offset field");
    let desc64: Vec<u64> = (0..n as usize)
        .map(|k| {
            let lo = offsets[k] as u64;
            let len = (offsets[k + 1] - offsets[k]) as u64;
            assert!(len <= 255, "desc64 len field");
            let bloom = tri_mask16(&bytes[lo as usize..(lo + len) as usize]) as u64;
            lo | len << 36 | bloom << 48
        })
        .collect();

    for (ni, needle) in needles.iter().enumerate() {
        let nb = needle.as_bytes();
        let finder = memmem::Finder::new(nb);
        let need16: u16 = if nb.len() >= 3 { tri_mask16(nb) } else { 0 };
        let need32: u32 = if nb.len() >= 3 { tri_mask32(nb) } else { 0 };
        let need8: u8 = byte_mask8(nb);

        for (ci, c) in cands.iter().enumerate() {
            let cd = (c.0, &c.1);
            let expect = &expected[ni][ci];

            time_arm(n, "memmem-value", needle, cd, expect, || {
                let mut hits: Vec<u32> = Vec::new();
                for_each_run(&c.1, |s, l| {
                    for e in s..=l {
                        let lo = offsets[e as usize] as usize;
                        let hi = offsets[e as usize + 1] as usize;
                        if finder.find(&bytes[lo..hi]).is_some() {
                            hits.push(e);
                        }
                    }
                });
                let mut bm = Bitmap::new();
                bm.add_many(&hits);
                bm
            });

            time_arm(n, "memmem-concat", needle, cd, expect, || {
                let mut hits: Vec<u32> = Vec::new();
                for_each_run(&c.1, |s, l| {
                    if s == l {
                        let lo = offsets[s as usize] as usize;
                        let hi = offsets[s as usize + 1] as usize;
                        if swar_contains(&bytes[lo..hi], nb) {
                            hits.push(s);
                        }
                        return;
                    }
                    let base = offsets[s as usize] as usize;
                    let region = &bytes[base..offsets[l as usize + 1] as usize];
                    let mut slot = s as usize;
                    let mut last_hit = u32::MAX;
                    for pos in finder.find_iter(region) {
                        let abs = base + pos;
                        // Matches ascend, so the owning slot only moves forward.
                        while offsets[slot + 1] as usize <= abs {
                            slot += 1;
                        }
                        // A match straddling a value boundary is not a match in any value.
                        if abs + nb.len() <= offsets[slot + 1] as usize && slot as u32 != last_hit {
                            hits.push(slot as u32);
                            last_hit = slot as u32;
                        }
                    }
                });
                let mut bm = Bitmap::new();
                bm.add_many(&hits);
                bm
            });

            time_arm(n, "swar-value", needle, cd, expect, || {
                let mut hits: Vec<u32> = Vec::new();
                for_each_run(&c.1, |s, l| {
                    for e in s..=l {
                        let lo = offsets[e as usize] as usize;
                        let hi = offsets[e as usize + 1] as usize;
                        if swar_contains(&bytes[lo..hi], nb) {
                            hits.push(e);
                        }
                    }
                });
                let mut bm = Bitmap::new();
                bm.add_many(&hits);
                bm
            });

            if !lite {
                time_arm(n, "bloom8", needle, cd, expect, || {
                    let mut hits: Vec<u32> = Vec::new();
                    for_each_run(&c.1, |s, l| {
                        for e in s..=l {
                            if bloom8[e as usize] & need8 != need8 {
                                continue;
                            }
                            let lo = offsets[e as usize] as usize;
                            let hi = offsets[e as usize + 1] as usize;
                            if swar_contains(&bytes[lo..hi], nb) {
                                hits.push(e);
                            }
                        }
                    });
                    let mut bm = Bitmap::new();
                    bm.add_many(&hits);
                    bm
                });

                time_arm(n, "tri16", needle, cd, expect, || {
                    let mut hits: Vec<u32> = Vec::new();
                    for_each_run(&c.1, |s, l| {
                        for e in s..=l {
                            if tri16[e as usize] & need16 != need16 {
                                continue;
                            }
                            let lo = offsets[e as usize] as usize;
                            let hi = offsets[e as usize + 1] as usize;
                            if swar_contains(&bytes[lo..hi], nb) {
                                hits.push(e);
                            }
                        }
                    });
                    let mut bm = Bitmap::new();
                    bm.add_many(&hits);
                    bm
                });
            }

            time_arm(n, "desc64", needle, cd, expect, || {
                let need = need16 as u64;
                let nlen = nb.len() as u64;
                let mut hits: Vec<u32> = Vec::new();
                for_each_run(&c.1, |s, l| {
                    for e in s..=l {
                        let d = desc64[e as usize];
                        let len = d >> 36 & 0xFF;
                        if len < nlen || d >> 48 & need != need {
                            continue;
                        }
                        let lo = (d & 0xF_FFFF_FFFF) as usize;
                        if swar_contains(&bytes[lo..lo + len as usize], nb) {
                            hits.push(e);
                        }
                    }
                });
                let mut bm = Bitmap::new();
                bm.add_many(&hits);
                bm
            });

            time_arm(n, "desc64+tri32", needle, cd, expect, || {
                let need = need16 as u64;
                let nlen = nb.len() as u64;
                let mut hits: Vec<u32> = Vec::new();
                for_each_run(&c.1, |s, l| {
                    for e in s..=l {
                        let d = desc64[e as usize];
                        let len = d >> 36 & 0xFF;
                        if len < nlen || d >> 48 & need != need {
                            continue;
                        }
                        if tri32[e as usize] & need32 != need32 {
                            continue;
                        }
                        let lo = (d & 0xF_FFFF_FFFF) as usize;
                        if swar_contains(&bytes[lo..lo + len as usize], nb) {
                            hits.push(e);
                        }
                    }
                });
                let mut bm = Bitmap::new();
                bm.add_many(&hits);
                bm
            });
        }
    }
}
