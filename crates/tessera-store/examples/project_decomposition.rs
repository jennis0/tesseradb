//! Where `Permutation::project`'s seconds go, and which rewrites remove them.
//!
//! The corpus quotes **10.7 s** for a cold row projection at 10⁹ (`probes/2026-07-30-1e9-rebuild/`)
//! and **4 550 ms** for the primitive alone (`probes/2026-08-04-refresh-ladder/`). Neither says
//! *which* stage the time is in, and the second is measured over a permutation `refresh_probe`
//! builds as the **identity** map — entity `e` at row `e`. A build orders rows by
//! `(morton, tessera_id)`, uncorrelated with entity-issue order, so a real slot array scatters.
//! Under the identity map the gathered rows come out already sorted and `par_sort_unstable` — a
//! run-detecting pdqsort — charges almost nothing, which makes that figure an *underestimate* of
//! the stage this probe exists to size.
//!
//! **Every figure here is single-threaded work, not wall clock.** Under concurrent sessions the
//! machine is already saturated, so spreading one projection across cores buys throughput nothing;
//! only removing work counts. Two of the candidates below are included precisely because they fail
//! that test.
//!
//! ## The four stages as built
//!
//! ```text
//! S1  mask.to_vec()             decode the entity mask into a Vec<u32>
//! S2  par_chunks gather         slots[e] lookup, sentinel filter
//! S3  par_sort_unstable         global comparison sort of the row array
//! S4  Bitmap::of(&rows)         roaring_bitmap_add_many
//! ```
//!
//! ## The candidates
//!
//! - **A — fused decode+gather** (S1+S2). Range-split the entity space and seek each range with
//!   `reset_at_or_after`. Included as a *negative* result: per-element iterator stepping loses to
//!   croaring's bulk `to_vec` plus a tight loop.
//! - **C — parallel bitmap build** (S4). Split the sorted array at container boundaries and build
//!   the parts concurrently. Included as a negative result: pure parallelism, no work removed.
//! - **B — partitioned build** (S3+S4). Radix-partition on the high 16 bits — which *is* the
//!   Roaring container key — and stamp each container into a dense bit array. No comparison sort.
//! - **E — fused one-pass** (all four). Decode in bulk into a reusable window, look up, and write
//!   each row *straight into its bucket*. The row array is written once rather than four times and
//!   the entity list never exists. Buckets are row ranges sized so the bit array each one stamps
//!   stays in L2 while the write cursors stay in L1.
//! - **F — E, emitting containers directly.** Same first pass; the second hands croaring finished
//!   containers in the portable format instead of re-expanding them to `u32` for `add_many`. The
//!   encoder here **mirrors `tessera-filter::pack::Sink`**, which already does exactly this in the
//!   filter write path and carries the reviewability argument for it. Landing F means lifting that
//!   module to a crate `tessera-store` may depend on — not writing a second encoder.
//!
//! Also measured: what the kernel is told about the 4 GB slot mapping. It is the one large mapping
//! in the tree carrying no `madvise` at all.
//!
//! Run:
//! ```text
//! cargo run --release --example project_decomposition -p tessera-store -- \
//!     [--entities N] [--grant F] [--shape scattered|identity] [--reps N] [--dir PATH]
//! ```
//! At 10⁹ the permutation file is 4 GB on disk and mapped; the process peaks around 11 GB.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;

use croaring::{Bitmap, Portable};
use memmap2::Mmap;
use rayon::prelude::*;

use tessera_store::write::PermutationWriter;
use tessera_store::Permutation;
use tessera_types::{EntityId, ROW_ABSENT};

/// `permutation.bin`'s header: `"TSPM"` ‖ u16 version ‖ u16 reserved ‖ u64 bound (R4).
const HEADER_LEN: usize = 16;
const DEFAULT_ENTITIES: u64 = 1_000_000_000;
const DEFAULT_GRANT: f64 = 0.25;

// ---------------------------------------------------------------------------------------------
// Fixture: a scattered bijection on [0, n), without materialising one.
// ---------------------------------------------------------------------------------------------

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A balanced Feistel permutation on `[0, 4^half_bits)`, cycle-walked down to `[0, n)`.
///
/// A bijection on the larger domain, restricted by walking, is a bijection on `[0, n)` — which is
/// what `permutation.bin` requires (`Permutation::validate_rows` rejects anything else). This
/// stands in for the `(morton, tessera_id)` order a real build produces: what matters to every
/// stage below is only that the map is uncorrelated with entity order, not which order it is.
struct Shuffle {
    half_bits: u32,
    mask: u32,
    n: u64,
}

impl Shuffle {
    fn new(n: u64) -> Self {
        let mut half_bits = 1u32;
        while (1u64 << (2 * half_bits)) < n {
            half_bits += 1;
        }
        Shuffle {
            half_bits,
            mask: (1u32 << half_bits) - 1,
            n,
        }
    }

    fn round(&self, i: u64, r: u32) -> u32 {
        (splitmix64(r as u64 ^ (0x5DEE_CE66_D000_0000u64.wrapping_mul(i + 1))) >> 40) as u32
            & self.mask
    }

    fn feistel(&self, x: u64) -> u64 {
        let mut l = ((x >> self.half_bits) as u32) & self.mask;
        let mut r = (x as u32) & self.mask;
        for i in 0..4u64 {
            let nl = r;
            r = l ^ self.round(i, r);
            l = nl;
        }
        ((l as u64) << self.half_bits) | (r as u64)
    }

    /// The Feistel run backwards: each round restores `r_prev` from `l`, then `l_prev` from `r`.
    fn feistel_inv(&self, y: u64) -> u64 {
        let mut l = ((y >> self.half_bits) as u32) & self.mask;
        let mut r = (y as u32) & self.mask;
        for i in (0..4u64).rev() {
            let r_prev = l;
            l = r ^ self.round(i, r_prev);
            r = r_prev;
        }
        ((l as u64) << self.half_bits) | (r as u64)
    }

    fn at(&self, x: u64) -> u64 {
        let mut v = self.feistel(x);
        while v >= self.n {
            v = self.feistel(v);
        }
        v
    }

    /// The inverse of [`Self::at`], and the reason the fixture can be written sequentially.
    ///
    /// Cycle-walking inverts symmetrically: walking *forward* from `x` skips the values of the
    /// larger domain that fall outside `[0, n)`, so walking *backward* from `y` skips exactly the
    /// same ones in the same order. Writing `perm[e] = at_inverse(e)` for ascending `e` therefore
    /// produces the identical file to scattering `perm[at(r)] = r` over ascending `r` — but as a
    /// sequential 4 GB stream rather than 10⁹ random writes, which is minutes against seconds.
    fn at_inverse(&self, y: u64) -> u64 {
        let mut x = self.feistel_inv(y);
        while x >= self.n {
            x = self.feistel_inv(x);
        }
        x
    }
}

/// Map the slot array of a `permutation.bin`.
///
/// Read directly rather than through `Permutation`, which owns no accessor for its slots —
/// deliberately, since I4 makes it the only *dispatch* into row space. Nothing here is a production
/// path: these are algorithms being sized, and the one that wins gets written inside
/// `permutation.rs` where the dispatch stays.
fn map_slots(path: &Path) -> (Mmap, usize) {
    let file = File::open(path).expect("the fixture opens");
    // SAFETY: read-only for the lifetime of the mapping; nothing in this probe writes it.
    let mmap = unsafe { Mmap::map(&file) }.expect("the fixture maps");
    let bound = u64::from_le_bytes(mmap[8..16].try_into().expect("8-byte view")) as usize;
    assert_eq!(mmap.len(), HEADER_LEN + bound * 4, "fixture length");
    (mmap, bound)
}

fn slots_of(mmap: &Mmap, bound: usize) -> &[u32] {
    let bytes = &mmap[HEADER_LEN..];
    // SAFETY: HEADER_LEN is 16, a multiple of 4, over a page-aligned base, so the cast is aligned;
    // the length was checked in `map_slots`.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, bound) }
}

// ---------------------------------------------------------------------------------------------
// Baseline — `Permutation::project`'s body, split at its four stage boundaries.
// ---------------------------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Stages {
    decode: f64,
    gather: f64,
    sort: f64,
    build: f64,
}

impl Stages {
    fn total(&self) -> f64 {
        self.decode + self.gather + self.sort + self.build
    }
}

fn chunking(len: usize) -> usize {
    let threads = rayon::current_num_threads().max(1);
    (len / (threads * 8)).max(1)
}

fn baseline(slots: &[u32], mask: &Bitmap) -> (Bitmap, Stages) {
    let mut st = Stages::default();

    let t = Instant::now();
    let entities: Vec<u32> = mask.to_vec();
    st.decode = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let chunk_len = chunking(entities.len());
    let per_chunk: Vec<Vec<u32>> = entities
        .par_chunks(chunk_len)
        .map(|chunk| {
            let mut local = Vec::with_capacity(chunk.len());
            local.extend(
                chunk
                    .iter()
                    .filter_map(|&entity| slots.get(entity as usize).copied())
                    .filter(|&slot| slot != ROW_ABSENT),
            );
            local
        })
        .collect();
    drop(entities);
    let total_rows: usize = per_chunk.iter().map(Vec::len).sum();
    let mut rows: Vec<u32> = Vec::with_capacity(total_rows);
    rows.extend(per_chunk.into_iter().flatten());
    st.gather = t.elapsed().as_secs_f64();

    let t = Instant::now();
    rows.par_sort_unstable();
    st.sort = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let out = Bitmap::of(&rows);
    st.build = t.elapsed().as_secs_f64();

    (out, st)
}

// ---------------------------------------------------------------------------------------------
// Candidate A — fused decode + gather by range seek. A negative result.
// ---------------------------------------------------------------------------------------------

fn fused_gather(slots: &[u32], mask: &Bitmap, bound: u64) -> Vec<u32> {
    let ranges = (rayon::current_num_threads().max(1) * 8) as u64;
    let span = bound.div_ceil(ranges).max(1);

    let per_range: Vec<Vec<u32>> = (0..ranges)
        .into_par_iter()
        .map(|i| {
            let lo = i * span;
            if lo >= bound {
                return Vec::new();
            }
            let hi = (lo + span).min(bound);
            let mut it = mask.iter();
            it.reset_at_or_after(lo as u32);
            let mut local = Vec::new();
            for entity in it {
                if entity as u64 >= hi {
                    break;
                }
                if let Some(&slot) = slots.get(entity as usize) {
                    if slot != ROW_ABSENT {
                        local.push(slot);
                    }
                }
            }
            local
        })
        .collect();

    let total: usize = per_range.iter().map(Vec::len).sum();
    let mut rows = Vec::with_capacity(total);
    rows.extend(per_range.into_iter().flatten());
    rows
}

// ---------------------------------------------------------------------------------------------
// Candidate C — keep the sort, build the bitmap in parallel. A negative result.
// ---------------------------------------------------------------------------------------------

fn parallel_build_sorted(rows: &[u32]) -> Bitmap {
    if rows.is_empty() {
        return Bitmap::new();
    }
    let parts = (rayon::current_num_threads().max(1) * 4).max(1);
    let target = rows.len().div_ceil(parts).max(1);

    let mut bounds = vec![0usize];
    let mut at = target;
    while at < rows.len() {
        let key = rows[at] >> 16;
        let mut j = at;
        while j < rows.len() && (rows[j] >> 16) == key {
            j += 1;
        }
        if j >= rows.len() {
            break;
        }
        bounds.push(j);
        at = j + target;
    }
    bounds.push(rows.len());

    let built: Vec<Bitmap> = bounds
        .windows(2)
        .collect::<Vec<_>>()
        .par_iter()
        .map(|w| Bitmap::of(&rows[w[0]..w[1]]))
        .collect();
    let refs: Vec<&Bitmap> = built.iter().collect();
    Bitmap::fast_or(&refs)
}

// ---------------------------------------------------------------------------------------------
// Candidate B — partition by container key; no comparison sort.
// ---------------------------------------------------------------------------------------------

fn partitioned_build(rows: &[u32], max_row: u32) -> Bitmap {
    if rows.is_empty() {
        return Bitmap::new();
    }
    let nbuckets = (max_row >> 16) as usize + 1;
    let chunk_len = chunking(rows.len());
    let chunks: Vec<&[u32]> = rows.chunks(chunk_len).collect();

    let hist: Vec<Vec<u32>> = chunks
        .par_iter()
        .map(|c| {
            let mut h = vec![0u32; nbuckets];
            for &r in c.iter() {
                h[(r >> 16) as usize] += 1;
            }
            h
        })
        .collect();

    let locals: Vec<(Vec<u16>, Vec<u32>)> = chunks
        .par_iter()
        .zip(hist.par_iter())
        .map(|(c, h)| {
            let mut start = vec![0u32; nbuckets + 1];
            for b in 0..nbuckets {
                start[b + 1] = start[b] + h[b];
            }
            let mut cursor = start[..nbuckets].to_vec();
            let mut buf = vec![0u16; c.len()];
            for &r in c.iter() {
                let b = (r >> 16) as usize;
                buf[cursor[b] as usize] = r as u16;
                cursor[b] += 1;
            }
            (buf, start)
        })
        .collect();

    let groups = (rayon::current_num_threads().max(1) * 4).max(1);
    let per_group = nbuckets.div_ceil(groups).max(1);

    let built: Vec<Bitmap> = (0..nbuckets)
        .step_by(per_group)
        .collect::<Vec<_>>()
        .par_iter()
        .map(|&glo| {
            let ghi = (glo + per_group).min(nbuckets);
            let mut stamp = vec![0u64; 1024];
            let mut sorted: Vec<u32> = Vec::new();
            for b in glo..ghi {
                let count: usize = locals
                    .iter()
                    .map(|(_, start)| (start[b + 1] - start[b]) as usize)
                    .sum();
                if count == 0 {
                    continue;
                }
                stamp.iter_mut().for_each(|w| *w = 0);
                for (buf, start) in locals.iter() {
                    for &low in &buf[start[b] as usize..start[b + 1] as usize] {
                        stamp[(low >> 6) as usize] |= 1u64 << (low & 63);
                    }
                }
                let base = (b as u32) << 16;
                sorted.reserve(count);
                for (w, &word) in stamp.iter().enumerate() {
                    let mut word = word;
                    while word != 0 {
                        let bit = word.trailing_zeros();
                        sorted.push(base | ((w as u32) << 6) | bit);
                        word &= word - 1;
                    }
                }
            }
            Bitmap::of(&sorted)
        })
        .collect();

    let refs: Vec<&Bitmap> = built.iter().collect();
    Bitmap::fast_or(&refs)
}

// ---------------------------------------------------------------------------------------------
// Candidates E and F — one pass over the permutation.
// ---------------------------------------------------------------------------------------------

/// Rows per bucket, and the two constraints that fix it.
///
/// A bucket is a **row range**, not a container key, and its width is the only free parameter. Too
/// wide and the bit array it stamps falls out of L2, making every stamp a last-level miss; too
/// narrow and there are so many live write cursors that the *append* side misses instead. 2²² rows
/// is a 512 KB bit array — L2-resident — and at 10⁹ leaves 239 buckets, whose cursors and tails
/// together stay inside L1. A bucket spans exactly 64 Roaring containers, so the container boundary
/// always falls inside one and never across two.
const BUCKET_SHIFT: u32 = 22;
/// 64-bit words in one Roaring container — croaring's block width.
const WORDS: usize = (1 << 16) / 64;
/// croaring's array/bitset threshold: a container holding more than this is stored as a bitset.
const ARRAY_MAX: u32 = 4096;
/// Containers staged before a stream is handed to croaring, bounding the transient buffer.
const STAGE: usize = 128;
/// The portable format's cookie for a stream with no run containers.
const COOKIE_NO_RUN: u32 = 12346;

/// Decode, look up and bucket in one pass.
///
/// The baseline writes the row array four times over: `to_vec` materialises the entity list, the
/// gather writes the rows, the sort rewrites them, and `Bitmap::of` reads them back. Candidate B
/// removes the sort but still writes the rows, re-reads them for a histogram, and re-reads them to
/// scatter. Here the entity list never exists — `read_many` decodes into a reusable window — and a
/// row is written exactly once, into its bucket, straight out of the slot lookup.
fn bucketise(slots: &[u32], mask: &Bitmap, row_bound: u32) -> Vec<Vec<u32>> {
    let nbuckets = ((row_bound >> BUCKET_SHIFT) + 1) as usize;
    // Rows spread near-uniformly across row space by construction (the permutation is uncorrelated
    // with entity order), so a mean-plus-a-quarter reservation absorbs the variation without a
    // histogram pass. A bucket that beats it grows once; nothing is wrong if it does.
    let expected = (mask.cardinality() as usize / nbuckets.max(1)) * 5 / 4;
    let mut buckets: Vec<Vec<u32>> = (0..nbuckets)
        .map(|_| Vec::with_capacity(expected))
        .collect();

    let mut window = [0u32; 8192];
    let mut cursor = mask.cursor();
    loop {
        let n = cursor.read_many(&mut window);
        if n == 0 {
            break;
        }
        for &entity in &window[..n] {
            if let Some(&slot) = slots.get(entity as usize) {
                if slot != ROW_ABSENT {
                    // `slot < row_bound` holds for every non-sentinel slot of a valid permutation
                    // (`Permutation::validate_rows` establishes it once per view at bundle open),
                    // so the shift cannot address past the ends.
                    buckets[(slot >> BUCKET_SHIFT) as usize].push(slot);
                }
            }
        }
    }
    buckets
}

/// Stamp each bucket into a bit array and read it back ascending into `add_many`.
fn fused_bucketed(slots: &[u32], mask: &Bitmap, row_bound: u32) -> Bitmap {
    let buckets = bucketise(slots, mask, row_bound);
    let mut stamp = vec![0u64; (1usize << BUCKET_SHIFT) / 64];
    let mut ascending: Vec<u32> = Vec::new();
    let mut out = Bitmap::new();
    for (b, rows) in buckets.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let base = (b as u32) << BUCKET_SHIFT;
        stamp.iter_mut().for_each(|w| *w = 0);
        for &row in rows.iter() {
            let offset = row - base;
            stamp[(offset >> 6) as usize] |= 1u64 << (offset & 63);
        }
        ascending.clear();
        ascending.reserve(rows.len());
        for (w, &word) in stamp.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let bit = word.trailing_zeros();
                ascending.push(base | ((w as u32) << 6) | bit);
                word &= word - 1;
            }
        }
        out.add_many(&ascending);
    }
    out
}

/// Mirror of `tessera-filter::pack::Sink` — the portable-format encoder that already exists in the
/// filter write path, reproduced here only because it is `pub(crate)` there.
///
/// Faithful to it in the two places that decide correctness: the payload encoding is chosen from
/// the cardinality (bitset above `ARRAY_MAX`, sorted `u16` at or below), because the deserializer
/// chooses how to *read* it the same way; and the offset table is written unconditionally, which is
/// what the no-run cookie requires. A stream croaring refuses falls back to per-entity insertion
/// rather than yielding a partial mask.
struct Sink {
    keys: Vec<u16>,
    cards: Vec<u32>,
    starts: Vec<u32>,
    payload: Vec<u8>,
    stream: Vec<u8>,
    out: Bitmap,
}

impl Sink {
    fn new() -> Self {
        Sink {
            keys: Vec::with_capacity(STAGE),
            cards: Vec::with_capacity(STAGE),
            starts: Vec::with_capacity(STAGE),
            payload: Vec::new(),
            stream: Vec::new(),
            out: Bitmap::new(),
        }
    }

    fn push_block(&mut self, key: u16, card: u32, words: &[u64]) {
        if card == 0 {
            return;
        }
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
        if self.keys.len() == STAGE {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        self.stream.clear();
        let size = self.keys.len() as u32;
        self.stream.extend_from_slice(&COOKIE_NO_RUN.to_le_bytes());
        self.stream.extend_from_slice(&size.to_le_bytes());
        for (key, card) in self.keys.iter().zip(&self.cards) {
            self.stream.extend_from_slice(&key.to_le_bytes());
            self.stream
                .extend_from_slice(&((card - 1) as u16).to_le_bytes());
        }
        let base = 8 + 8 * size;
        for start in &self.starts {
            self.stream.extend_from_slice(&(base + start).to_le_bytes());
        }
        self.stream.extend_from_slice(&self.payload);

        match Bitmap::try_deserialize::<Portable>(&self.stream) {
            Some(bitmap) => self.out.or_inplace(&bitmap),
            None => panic!("the packed stream must be a valid portable bitmap"),
        }
        self.keys.clear();
        self.cards.clear();
        self.starts.clear();
        self.payload.clear();
    }

    fn finish(mut self) -> Bitmap {
        self.flush();
        self.out
    }
}

/// Candidate G — candidate F, but through the crate the projection actually links against.
///
/// Isolates the crate boundary from the algorithm: same buckets, same stamp, same emit order.
fn fused_packed_shared(slots: &[u32], mask: &Bitmap, row_bound: u32) -> Bitmap {
    let buckets = bucketise(slots, mask, row_bound);
    let mut stamp = vec![0u64; (1usize << BUCKET_SHIFT) / 64];
    let mut sink = tessera_roaring::Sink::new();
    for (b, rows) in buckets.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let base = (b as u32) << BUCKET_SHIFT;
        stamp.iter_mut().for_each(|w| *w = 0);
        for &row in rows.iter() {
            let offset = row - base;
            stamp[(offset >> 6) as usize] |= 1u64 << (offset & 63);
        }
        for (blk, words) in stamp.as_chunks::<WORDS>().0.iter().enumerate() {
            let card: u32 = words.iter().map(|w| w.count_ones()).sum();
            if card == 0 {
                continue;
            }
            let key = u16::try_from((base >> 16) + blk as u32).expect("container key fits u16");
            sink.push_block(key, card, words);
        }
    }
    sink.finish()
}

/// Candidate F — candidate E, handing croaring finished containers.
///
/// The bit array a bucket stamps *is* 64 container payloads laid end to end, so the second pass
/// costs a popcount per container and a memcpy, against `add_many`'s insertion per row.
fn fused_packed(slots: &[u32], mask: &Bitmap, row_bound: u32) -> Bitmap {
    let buckets = bucketise(slots, mask, row_bound);
    let mut stamp = vec![0u64; (1usize << BUCKET_SHIFT) / 64];
    let mut sink = Sink::new();
    for (b, rows) in buckets.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let base = (b as u32) << BUCKET_SHIFT;
        stamp.iter_mut().for_each(|w| *w = 0);
        for &row in rows.iter() {
            let offset = row - base;
            stamp[(offset >> 6) as usize] |= 1u64 << (offset & 63);
        }
        for (blk, words) in stamp.as_chunks::<WORDS>().0.iter().enumerate() {
            let card: u32 = words.iter().map(|w| w.count_ones()).sum();
            if card == 0 {
                continue;
            }
            let key = u16::try_from((base >> 16) + blk as u32).expect("container key fits u16");
            sink.push_block(key, card, words);
        }
    }
    sink.finish()
}

// ---------------------------------------------------------------------------------------------
// What the kernel is told about the slot mapping.
// ---------------------------------------------------------------------------------------------

/// `permutation.bin` is mapped with **no `madvise` at all** — the only large mapping in the tree
/// that isn't advised. The gather walks it end to end, so the first walk after a mapping is
/// established pays one minor fault per 4 KB page: a million of them at 10⁹.
///
/// Each arm re-maps the file, so page tables start empty while the page *cache* stays warm. That
/// isolates the fault-and-TLB cost, which the advice changes, from the disk read, which it does
/// not. `MADV_SEQUENTIAL` is measured because it is what the rest of the tree uses and would be the
/// obvious thing to copy — included to show it is the wrong hint here, not as a candidate: its
/// drop-behind frees each page just after it is read, which is the opposite of the residency a
/// table re-walked by every session needs.
fn advice_sweep(path: &Path, entities: u64) {
    use memmap2::Advice;
    let arms: [(&str, Option<Advice>); 4] = [
        ("none (today)", None),
        ("MADV_WILLNEED", Some(Advice::WillNeed)),
        ("MADV_HUGEPAGE", Some(Advice::HugePage)),
        ("MADV_SEQUENTIAL", Some(Advice::Sequential)),
    ];
    for (label, advice) in arms {
        let (mmap, bound) = map_slots(path);
        let t = Instant::now();
        if let Some(advice) = advice {
            if mmap.advise(advice).is_err() {
                println!("{label:<24} advise refused by the kernel");
                continue;
            }
        }
        let advise_ms = t.elapsed().as_secs_f64() * 1e3;

        let slots = slots_of(&mmap, bound);
        let t = Instant::now();
        // One slot per 4 KB page — what a wide grant's gather does to every page of the table.
        let mut acc = 0u64;
        let mut i = 0usize;
        while i < slots.len() {
            acc = acc.wrapping_add(slots[i] as u64);
            i += 1024;
        }
        let walk_ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "{label:<24} advise {advise_ms:>8.1} ms   walk {walk_ms:>8.1} ms   ({:.1} GB, sum {acc})",
            entities as f64 * 4.0 / 1e9
        );
    }
}

// ---------------------------------------------------------------------------------------------

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    s[s.len() / 2]
}

fn report(label: &str, secs: &[f64]) -> f64 {
    let m = median(secs);
    println!("{label:<38} median {:>9.1} ms", m * 1e3);
    m
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let entities: u64 = arg(&args, "--entities")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ENTITIES);
    let grant: f64 = arg(&args, "--grant")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_GRANT);
    let shape = arg(&args, "--shape").unwrap_or_else(|| "scattered".to_string());
    let reps: usize = arg(&args, "--reps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let dir = PathBuf::from(
        arg(&args, "--dir").unwrap_or_else(|| "/tmp/tessera-project-decomp".to_string()),
    );
    std::fs::create_dir_all(&dir).expect("the probe directory");

    println!("# project_decomposition");
    println!(
        "entities={entities} grant={grant} shape={shape} reps={reps} threads={}",
        rayon::current_num_threads()
    );

    let perm_path = dir.join(format!("permutation-{shape}-{entities}.bin"));
    if !perm_path.exists() {
        // Written to a sibling and renamed on success. `PermutationWriter` sizes the file up front
        // and then *scatters* into it, so a run interrupted mid-write leaves a full-length file
        // whose unwritten slots still hold the row-absent sentinel — indistinguishable from a
        // complete fixture by any check on length or existence. That poisoned exactly one 10⁹
        // measurement here: 43% of the grant projected to nothing, and every stage downstream of
        // the gather was silently sized against a smaller row set than the header claimed.
        let tmp = perm_path.with_extension("bin.partial");
        let t = Instant::now();
        {
            let mut writer =
                PermutationWriter::create(&tmp, entities).expect("the fixture is created");
            match shape.as_str() {
                "identity" => {
                    for e in 0..entities {
                        writer
                            .set(EntityId::new(e), e as u32)
                            .expect("the slot is in bound");
                    }
                }
                _ => {
                    let sh = Shuffle::new(entities);
                    // The inverse is load-bearing for the fixture's validity, so it is checked
                    // against the forward map before 10^9 of them are trusted. A bijection that is
                    // subtly not one shows up downstream as a projection short of the grant, which
                    // the cardinality assertion below would catch — but this says so here.
                    for probe in [0u64, 1, 7, entities / 3, entities - 1] {
                        assert_eq!(
                            sh.at(sh.at_inverse(probe)),
                            probe,
                            "the Feistel inverse must undo the forward map"
                        );
                    }
                    for e in 0..entities {
                        writer
                            .set(EntityId::new(e), sh.at_inverse(e) as u32)
                            .expect("the slot is in bound");
                    }
                }
            }
            writer.finish().expect("the fixture flushes");
        }
        std::fs::rename(&tmp, &perm_path).expect("the fixture publishes");
        println!(
            "fixture: {:.1} GB in {:.1}s",
            (entities as f64 * 4.0) / 1e9,
            t.elapsed().as_secs_f64()
        );
    }

    println!("\n## slot mapping — what the kernel is told");
    advice_sweep(&perm_path, entities);

    let (mmap, bound) = map_slots(&perm_path);
    let slots = slots_of(&mmap, bound);
    // Warm the mapping: every stage below reads the whole slot array, and charging the first one
    // for a cold page cache would attribute the fault storm to whichever ran first.
    let warm: u64 = slots.par_iter().map(|&s| s as u64).sum();
    let stride = (1.0 / grant).round().max(1.0) as u32;
    let mut mask = Bitmap::from_range_with_step(0..entities as u32, stride);
    mask.run_optimize();
    println!(
        "\ngrant: {} entities, {:.2} MB serialised (warm sum {warm})",
        mask.cardinality(),
        mask.get_serialized_size_in_bytes::<Portable>() as f64 / 1e6
    );

    // ---- baseline, staged ----
    println!("\n## baseline stages");
    let mut runs: Vec<Stages> = Vec::new();
    let mut reference: Option<Bitmap> = None;
    for _ in 0..reps {
        let (out, st) = baseline(slots, &mask);
        if reference.is_none() {
            println!(
                "projection: {} rows, {:.2} MB serialised",
                out.cardinality(),
                out.get_serialized_size_in_bytes::<Portable>() as f64 / 1e6
            );
            // Both fixture shapes are *total* bijections over `[0, entities)`, so every granted
            // entity has a row and the projection must have exactly the grant's cardinality. This
            // is the check that catches a truncated fixture: without it the stages downstream of
            // the gather quietly measure a smaller row set, and the ratios still look plausible.
            assert_eq!(
                out.cardinality(),
                mask.cardinality(),
                "fixture is not a total bijection — delete it and let this run rewrite it"
            );
            reference = Some(out);
        }
        runs.push(st);
    }
    let reference = reference.expect("one run");
    let s1 = report(
        "S1 decode  mask.to_vec()",
        &runs.iter().map(|s| s.decode).collect::<Vec<_>>(),
    );
    let s2 = report(
        "S2 gather  slots lookup",
        &runs.iter().map(|s| s.gather).collect::<Vec<_>>(),
    );
    let s3 = report(
        "S3 sort    par_sort_unstable",
        &runs.iter().map(|s| s.sort).collect::<Vec<_>>(),
    );
    let s4 = report(
        "S4 build   Bitmap::of",
        &runs.iter().map(|s| s.build).collect::<Vec<_>>(),
    );
    let base_total = median(&runs.iter().map(Stages::total).collect::<Vec<_>>());
    println!("{:<38} median {:>9.1} ms", "TOTAL", base_total * 1e3);

    let row_bound = u32::try_from(entities).expect("bundle_format 1 is u32 rows");
    let mut speedups: Vec<(&str, f64)> = Vec::new();

    // Measured directly after the baseline, and before the research arms below, deliberately: this
    // is the before/after the change claims, and both halves of it should see the same process.
    // Run last it reads ~8 GB of allocator churn later and lands 25% slower, which is a property of
    // the harness rather than of the projection.
    println!("\n## candidate G — F through the shared crate (isolates the boundary)");
    let mut g = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let out = fused_packed_shared(slots, &mask, row_bound);
        g.push(t.elapsed().as_secs_f64());
        assert_eq!(out, reference, "candidate G must equal the baseline");
    }
    let g = report("G  fused one-pass + shared packer", &g);
    println!("{:<38} {:>16.2}x vs baseline total", "speedup", base_total / g);
    speedups.push(("G (F via shared crate)", base_total / g));

    println!("\n## production Permutation::project, as landed");
    let perm = Permutation::load(&perm_path).expect("the fixture loads through the real reader");
    let mut landed = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let out = perm.project(&mask);
        landed.push(t.elapsed().as_secs_f64());
        assert_eq!(out, reference, "the landed project must equal the baseline");
    }
    let landed = report("Permutation::project", &landed);
    println!("{:<38} {:>16.2}x vs baseline total", "speedup", base_total / landed);
    speedups.push(("Permutation::project (landed)", base_total / landed));


    let rows_unsorted = fused_gather(slots, &mask, entities);
    let max_row = rows_unsorted.iter().copied().max().unwrap_or(0);


    // ---- candidate A ----
    println!("\n## candidate A — fused decode+gather by range seek (replaces S1+S2)");
    let mut a = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let rows = fused_gather(slots, &mask, entities);
        a.push(t.elapsed().as_secs_f64());
        std::hint::black_box(&rows);
    }
    let a = report("A  range-seek gather", &a);
    println!("{:<38} {:>16.2}x vs S1+S2", "speedup", (s1 + s2) / a);

    // ---- candidate C ----
    println!("\n## candidate C — parallel bitmap build (replaces S4)");
    let mut sorted = rows_unsorted.clone();
    sorted.par_sort_unstable();
    let mut c = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let out = parallel_build_sorted(&sorted);
        c.push(t.elapsed().as_secs_f64());
        assert_eq!(out, reference, "candidate C must equal the baseline");
    }
    let c = report("C  parallel build over sorted", &c);
    println!("{:<38} {:>16.2}x vs S4", "speedup", s4 / c);
    drop(sorted);

    // ---- candidate B ----
    println!("\n## candidate B — partitioned build (replaces S3+S4)");
    let mut b = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let out = partitioned_build(&rows_unsorted, max_row);
        b.push(t.elapsed().as_secs_f64());
        assert_eq!(out, reference, "candidate B must equal the baseline");
    }
    let b = report("B  partition + stamp", &b);
    println!("{:<38} {:>16.2}x vs S3+S4", "speedup", (s3 + s4) / b);
    speedups.push(("B (with baseline S1+S2)", base_total / (s1 + s2 + b)));
    drop(rows_unsorted);

    // ---- candidate E ----
    println!("\n## candidate E — fused one-pass, add_many tail (replaces all four)");
    let mut e = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let out = fused_bucketed(slots, &mask, row_bound);
        e.push(t.elapsed().as_secs_f64());
        assert_eq!(out, reference, "candidate E must equal the baseline");
    }
    let e = report("E  fused one-pass", &e);
    println!("{:<38} {:>16.2}x vs baseline total", "speedup", base_total / e);
    speedups.push(("E", base_total / e));

    // ---- candidate F ----
    println!("\n## candidate F — fused one-pass, containers emitted directly");
    let mut f = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let out = fused_packed(slots, &mask, row_bound);
        f.push(t.elapsed().as_secs_f64());
        assert_eq!(out, reference, "candidate F must equal the baseline");
    }
    let f = report("F  fused one-pass + packed", &f);
    println!("{:<38} {:>16.2}x vs baseline total", "speedup", base_total / f);
    speedups.push(("F", base_total / f));

    println!("\n## summary — single-threaded work against today");
    for (label, x) in speedups {
        println!("{label:<38} {x:>8.2}x");
    }
}
