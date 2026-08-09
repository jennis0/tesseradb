//! Can the unselective result be built without paying per-value insertion — and what does the
//! branchless predicate buy once the result representation stops charging for it?
//!
//! Arm 7 left a measured decomposition and a refusal. The unselective cost splits ~40% mispredicted
//! branches / ~30% `add_many` / ~30% buffer traffic, and branchless collection was refused because
//! it fixed only the first term: built into the shipped scan it cost the selective arms 1.5–2.1×
//! to buy 1.3–1.6× on the unselective ones. The note carried forward was that branchless becomes
//! attractive **alongside a cheaper result representation**, because only the combination reaches
//! the 0.5–1 s filter budget. This arm measures that combination.
//!
//! The combination is bit-packing: evaluate the predicate per value into a 1024-word block —
//! `word |= (pred as u64) << bit`, no branch, autovectorisable — and the packed words *are* the
//! result's 2¹⁶-entity Roaring bitset container. That kills all three terms at once: no branch, no
//! per-value `add_many`, and the only buffer written is the 8 KB the container is. The blocker is
//! handing a finished container to croaring, which exposes no container-level API; two routes are
//! measured:
//!
//! - **portable** — assemble the CRoaring portable serialization by hand (cookie 12346, `(key,
//!   card−1)` descriptors, offsets, then array/bitset payloads back to back) and pass it to
//!   `Bitmap::try_deserialize::<Portable>`. Fast in proportion to how little croaring then does —
//!   a bitset payload is memcpy'd — and **wrong-mask-shaped if buggy**, so every arm here is
//!   asserted equal to the shipped scan's result before any timing is reported.
//! - **saferange** — stay on croaring's public API: a block over half full goes in as `add_range`
//!   plus `remove_many` of the (fewer) misses; under half, `add_many` of the hits. No format
//!   knowledge, at the cost of extracting set or cleared bits back out of the words.
//!
//! `count` is the floor: the packed loop with nothing built. The gap between it and each route is
//! that route's whole overhead. `shipped` is `ValueColumn::scan_range` — the code as it stands,
//! the number the others must beat.
//!
//! The candidate here is contiguous (whole corpus, and a 25% principal), which is the shape the
//! unselective gap was measured on. A scattered candidate cannot bit-pack a block it holds three
//! entities of; an integration would keep the shipped per-run path for short runs and pack only
//! runs spanning a block, so the scattered arms of `realscan` price that case already.

use std::time::Instant;

use croaring::{Bitmap, Portable};
use tessera_filter::{Codes, Endpoint, Scalar, ValueColumn};

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const BLOCK: usize = 1 << 16;
const WORDS: usize = BLOCK / 64;
/// CRoaring's array/bitset threshold: a container holding more than this many values is a bitset.
const ARRAY_MAX: u32 = 4096;

/// Pack `pred` over `vals` into `words`, branchlessly. `vals.len() <= BLOCK`.
#[inline]
fn pack(vals: &[u8], bound: u8, words: &mut [u64; WORDS]) -> u32 {
    words.fill(0);
    for (i, &v) in vals.iter().enumerate() {
        words[i >> 6] |= u64::from(v < bound) << (i & 63);
    }
    words.iter().map(|w| w.count_ones()).sum()
}

/// `v < bound` for eight bytes at once, one result bit per byte — SWAR, no lanes wider than a
/// register. Requires every byte and the bound below 0x80, which this probe's 0..100 value domain
/// gives; a general `u8` column needs the two-sided form (one more op per word) and the other
/// widths need their own lane arithmetic, so this arm prices the *approach*, not a shipped kernel.
///
/// The two constants are the standard tricks: `x + (0x80 - bound)` sets bit 7 of a byte exactly
/// when the byte is ≥ bound (no inter-byte carry, both operands below 0x80), and the multiply
/// gathers the eight bit-7s into the top byte, one bit per source byte, low byte first.
#[inline]
fn mask8_lt(x: u64, bound: u8) -> u64 {
    let add = (0x80 - u64::from(bound)) * 0x0101_0101_0101_0101;
    let ge = x.wrapping_add(add) & 0x8080_8080_8080_8080;
    let lt = ge ^ 0x8080_8080_8080_8080;
    ((lt >> 7).wrapping_mul(0x0102_0408_1020_4080)) >> 56
}

/// [`pack`] with the SWAR kernel: 64 values per word in eight 8-byte gulps.
#[inline]
fn pack_swar(vals: &[u8], bound: u8, words: &mut [u64; WORDS]) -> u32 {
    words.fill(0);
    let mut card = 0u32;
    let (chunks, tail) = vals.split_at(vals.len() & !63);
    for (wi, chunk) in chunks.chunks_exact(64).enumerate() {
        let mut w = 0u64;
        for (gi, gulp) in chunk.chunks_exact(8).enumerate() {
            let x = u64::from_le_bytes(gulp.try_into().unwrap());
            w |= mask8_lt(x, bound) << (gi * 8);
        }
        words[wi] = w;
        card += w.count_ones();
    }
    let base = chunks.len();
    for (i, &v) in tail.iter().enumerate() {
        let bit = u64::from(v < bound) << ((base + i) & 63);
        words[(base + i) >> 6] |= bit;
        card += bit.count_ones();
    }
    card
}

/// The by-hand portable buffer: one container per key, assembled back to front from parts.
struct PortableBuilder {
    keys: Vec<u16>,
    cards: Vec<u32>,
    starts: Vec<u32>,
    payload: Vec<u8>,
}

impl PortableBuilder {
    fn new(max_containers: usize) -> Self {
        PortableBuilder {
            keys: Vec::with_capacity(max_containers),
            cards: Vec::with_capacity(max_containers),
            starts: Vec::with_capacity(max_containers),
            payload: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.cards.clear();
        self.starts.clear();
        self.payload.clear();
    }

    /// Add the container for `key` from packed words. `card` must be `words`' popcount, non-zero.
    fn push(&mut self, key: u16, card: u32, words: &[u64; WORDS]) {
        self.keys.push(key);
        self.cards.push(card);
        self.starts.push(self.payload.len() as u32);
        if card > ARRAY_MAX {
            for w in words {
                self.payload.extend_from_slice(&w.to_le_bytes());
            }
        } else {
            for (wi, &w0) in words.iter().enumerate() {
                let mut w = w0;
                while w != 0 {
                    let low = (wi as u32) * 64 + w.trailing_zeros();
                    self.payload.extend_from_slice(&(low as u16).to_le_bytes());
                    w &= w - 1;
                }
            }
        }
    }

    /// Assemble the stream: cookie, count, descriptors, offsets, payloads.
    fn assemble(&self, out: &mut Vec<u8>) {
        out.clear();
        let size = self.keys.len() as u32;
        out.extend_from_slice(&12346u32.to_le_bytes()); // SERIAL_COOKIE_NO_RUNCONTAINER
        out.extend_from_slice(&size.to_le_bytes());
        for (k, c) in self.keys.iter().zip(&self.cards) {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&((c - 1) as u16).to_le_bytes());
        }
        let base = 8 + 8 * size;
        for s in &self.starts {
            out.extend_from_slice(&(base + s).to_le_bytes());
        }
        out.extend_from_slice(&self.payload);
    }
}

/// Walk a contiguous candidate `[lo, hi)` in blocks aligned to 2¹⁶ entity boundaries.
fn for_each_block(lo: u32, hi: u32, mut f: impl FnMut(u32, u32)) {
    let mut at = lo;
    while at < hi {
        let next = ((u64::from(at >> 16) + 1) << 16).min(u64::from(hi)) as u32;
        f(at, next);
        at = next;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(1_000_000_000);

    let values: Vec<u8> = (0..n).map(|e| (splitmix(e) % 100) as u8).collect();
    let column = ValueColumn::universal(Codes::U8(values.clone().into()));

    println!("n,candidate,selectivity_pct,form,ms,hits");

    for (cname, clo, chi) in [("all", 0u32, n as u32), ("broad-25pct", 0u32, (n / 4) as u32)] {
        let mut cand = Bitmap::new();
        cand.add_range(clo..chi);
        cand.run_optimize();

        for sel in [1u8, 5, 25, 50, 75, 100] {
            let bound = Endpoint {
                value: Scalar::Int(i128::from(sel)),
                inclusive: false,
            };

            // --- shipped: the baseline, and the answer every other arm must equal.
            let run_shipped = || column.scan_range(&cand, None, Some(bound));
            let expected = run_shipped();
            let t = Instant::now();
            let got = run_shipped();
            let shipped_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got == expected);

            // --- count: the packed loop's floor, nothing built.
            let run_count = || {
                let mut words = [0u64; WORDS];
                let mut total: u64 = 0;
                for_each_block(clo, chi, |lo, hi| {
                    total += u64::from(pack(
                        &values[lo as usize..hi as usize],
                        sel,
                        &mut words,
                    ));
                });
                total
            };
            let c = run_count();
            assert_eq!(c, expected.cardinality());
            let t = Instant::now();
            let c = run_count();
            let count_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(c, expected.cardinality());

            // --- portable: pack, assemble by hand, deserialize.
            let max_containers = ((chi - clo) as usize >> 16) + 1;
            let mut builder = PortableBuilder::new(max_containers);
            let mut stream: Vec<u8> = Vec::new();
            let mut run_portable = || {
                builder.clear();
                let mut words = [0u64; WORDS];
                for_each_block(clo, chi, |lo, hi| {
                    let card = pack(&values[lo as usize..hi as usize], sel, &mut words);
                    if card != 0 {
                        builder.push((lo >> 16) as u16, card, &words);
                    }
                });
                builder.assemble(&mut stream);
                Bitmap::try_deserialize::<Portable>(&stream).expect("self-assembled stream")
            };
            let got = run_portable();
            assert!(got == expected, "portable arm disagreed with the shipped scan");
            let t = Instant::now();
            let got = run_portable();
            let portable_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got == expected);

            // --- count-swar: the floor once the predicate stops being one byte per iteration.
            let run_count_swar = || {
                let mut words = [0u64; WORDS];
                let mut total: u64 = 0;
                for_each_block(clo, chi, |lo, hi| {
                    total += u64::from(pack_swar(
                        &values[lo as usize..hi as usize],
                        sel,
                        &mut words,
                    ));
                });
                total
            };
            let c = run_count_swar();
            assert_eq!(c, expected.cardinality());
            let t = Instant::now();
            let c = run_count_swar();
            let count_swar_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(c, expected.cardinality());

            // --- portable-swar: the fast kernel feeding the by-hand stream.
            let mut run_portable_swar = || {
                builder.clear();
                let mut words = [0u64; WORDS];
                for_each_block(clo, chi, |lo, hi| {
                    let card = pack_swar(&values[lo as usize..hi as usize], sel, &mut words);
                    if card != 0 {
                        builder.push((lo >> 16) as u16, card, &words);
                    }
                });
                builder.assemble(&mut stream);
                Bitmap::try_deserialize::<Portable>(&stream).expect("self-assembled stream")
            };
            let got = run_portable_swar();
            assert!(got == expected, "portable-swar arm disagreed with the shipped scan");
            let t = Instant::now();
            let got = run_portable_swar();
            let portable_swar_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got == expected);

            // --- portable-chunked: the same stream, flushed every 128 containers into the result
            // by `or_inplace` — which appends disjoint containers by copy — so the transient buffer
            // is ~1 MB however large the result, where the one-shot stream is the whole result
            // serialised (125 MB at half of 10⁹). This is the shape an integration would ship.
            let mut run_chunked = || {
                let mut out = Bitmap::new();
                builder.clear();
                let mut words = [0u64; WORDS];
                for_each_block(clo, chi, |lo, hi| {
                    let card = pack_swar(&values[lo as usize..hi as usize], sel, &mut words);
                    if card != 0 {
                        builder.push((lo >> 16) as u16, card, &words);
                    }
                    if builder.keys.len() == 128 {
                        builder.assemble(&mut stream);
                        out |= Bitmap::try_deserialize::<Portable>(&stream)
                            .expect("self-assembled stream");
                        builder.clear();
                    }
                });
                if !builder.keys.is_empty() {
                    builder.assemble(&mut stream);
                    out |=
                        Bitmap::try_deserialize::<Portable>(&stream).expect("self-assembled stream");
                }
                out
            };
            let got = run_chunked();
            assert!(got == expected, "portable-chunked arm disagreed with the shipped scan");
            let t = Instant::now();
            let got = run_chunked();
            let chunked_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got == expected);

            // --- saferange: pack, then croaring's public API only.
            let mut ones: Vec<u32> = Vec::with_capacity(BLOCK);
            let mut run_safe = || {
                let mut words = [0u64; WORDS];
                let mut out = Bitmap::new();
                for_each_block(clo, chi, |lo, hi| {
                    let len = hi - lo;
                    let card = pack(&values[lo as usize..hi as usize], sel, &mut words);
                    if card == 0 {
                        return;
                    }
                    if card == len {
                        out.add_range(lo..hi);
                        return;
                    }
                    ones.clear();
                    if card * 2 > len {
                        // Majority set: add the whole block, remove the misses.
                        for (wi, &w0) in words[..(len as usize + 63) / 64].iter().enumerate() {
                            let mut w = !w0;
                            // Mask tail bits past the block's length in its last word.
                            if (wi + 1) * 64 > len as usize {
                                w &= (1u64 << (len as usize - wi * 64)) - 1;
                            }
                            while w != 0 {
                                ones.push(lo + (wi as u32) * 64 + w.trailing_zeros());
                                w &= w - 1;
                            }
                        }
                        out.add_range(lo..hi);
                        out.remove_many(&ones);
                    } else {
                        for (wi, &w0) in words.iter().enumerate() {
                            let mut w = w0;
                            while w != 0 {
                                ones.push(lo + (wi as u32) * 64 + w.trailing_zeros());
                                w &= w - 1;
                            }
                        }
                        out.add_many(&ones);
                    }
                });
                out.run_optimize();
                out
            };
            let got = run_safe();
            assert!(got == expected, "saferange arm disagreed with the shipped scan");
            let t = Instant::now();
            let got = run_safe();
            let safe_ms = t.elapsed().as_secs_f64() * 1000.0;
            assert!(got == expected);

            for (form, ms) in [
                ("shipped", shipped_ms),
                ("count", count_ms),
                ("portable", portable_ms),
                ("count-swar", count_swar_ms),
                ("portable-swar", portable_swar_ms),
                ("portable-chunked", chunked_ms),
                ("saferange", safe_ms),
            ] {
                println!("{n},{cname},{sel},{form},{ms:.1},{}", expected.cardinality());
            }
        }
    }
}
