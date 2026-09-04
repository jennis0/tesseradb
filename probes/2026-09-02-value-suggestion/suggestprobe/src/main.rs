//! Value-suggestion probe: index residency (arm 1), keystroke latency on the probe route (arm 2),
//! and the per-session visible-set lever priced (arm 3), at 10⁵–10⁷ values over 10⁸ entities.
//!
//! Subcommands (all paths absolute; `DIR` is the fixture directory under data/ladder/):
//!   prep  <allCountries.txt> <DIR>            fold name ∪ asciiname, dedupe, shuffle → DIR/vocab.txt
//!   arm1  <DIR> <structure> <V> <key|words> [hex]   structure ∈ btree|arena|arenaref|fst|dict
//!   gen   <DIR> <V> <N> <zipf|uniform>        DIR/<dist>/values.u32 + postings.arrow
//!   arm2  <DIR> <V> <zipf|uniform>            per-value probe cost split + budgeted walks
//!   arm3  <DIR> <V> <zipf|uniform>            visible-set setup by both routes, bytes, keystroke

use croaring::{Bitmap, Portable};
use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use memmap2::Mmap;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tessera_authz::{encode_posting, PostingRef, PostingsReader, PostingsSpool};
use tessera_filter::{Access, ColumnPostings, SortedDict, SortedDictWriter};
use tessera_types::AttrLocalId;
use unicode_normalization::UnicodeNormalization;

// ------------------------------------------------------------------------------------------
// Counting allocator: heap bytes live, so a structure's residency is its allocations and not an
// RSS guess.
// ------------------------------------------------------------------------------------------

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if new >= l.size() {
            LIVE.fetch_add(new - l.size(), Ordering::Relaxed);
        } else {
            LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
        }
        System.realloc(p, l, new)
    }
}

#[global_allocator]
static A: Counting = Counting;

fn heap() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn proc_status(field: &str) -> f64 {
    let mut s = String::new();
    File::open("/proc/self/status").unwrap().read_to_string(&mut s).unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let kb: f64 = rest.trim().trim_start_matches(':').trim().split_whitespace().next().unwrap().parse().unwrap();
            return kb * 1024.0;
        }
    }
    0.0
}
fn rss_mb() -> f64 {
    proc_status("VmRSS") / 1e6
}
fn hwm_mb() -> f64 {
    proc_status("VmHWM") / 1e6
}

// ------------------------------------------------------------------------------------------
// RNG (splitmix64) and a percentile helper.
// ------------------------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Stats {
    n: usize,
    median_us: f64,
    p99_us: f64,
    mean_us: f64,
    max_us: f64,
}
fn stats(samples: &mut Vec<f64>) -> Stats {
    if samples.is_empty() {
        return Stats { n: 0, median_us: 0.0, p99_us: 0.0, mean_us: 0.0, max_us: 0.0 };
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let idx = |q: f64| samples[((n as f64 - 1.0) * q).round() as usize];
    Stats {
        n,
        median_us: idx(0.5) / 1e3,
        p99_us: idx(0.99) / 1e3,
        mean_us: samples.iter().sum::<f64>() / n as f64 / 1e3,
        max_us: samples[n - 1] / 1e3,
    }
}
impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} median {:.2} us  p99 {:.2} us  mean {:.2} us  max {:.1} us",
            self.n, self.median_us, self.p99_us, self.mean_us, self.max_us
        )
    }
}

fn median3<F: FnMut() -> f64>(mut f: F) -> f64 {
    let mut v = [f(), f(), f()];
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[1]
}

// ------------------------------------------------------------------------------------------
// The fold (design §4): NFKC, case fold, whitespace collapsed to one space and trimmed.
// Case folding is `to_lowercase`, which is Unicode lowercase mapping rather than default case
// folding (they differ on a handful of characters, e.g. ß, final sigma); noted in the README.
// ------------------------------------------------------------------------------------------

fn fold(s: &str, out: &mut String) {
    out.clear();
    let mut pending_space = false;
    for c in s.nfkc() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for l in c.to_lowercase() {
            out.push(l);
        }
    }
}

/// Byte offsets of every word start after the first (design §4: a transition into a letter or
/// digit from anything else).
fn word_starts(s: &str, out: &mut Vec<u16>) {
    out.clear();
    let mut prev_alnum = true; // position 0 is the key entry itself, never a word-start entry
    for (i, c) in s.char_indices() {
        let alnum = c.is_alphanumeric();
        if alnum && !prev_alnum && i > 0 {
            out.push(i as u16);
        }
        prev_alnum = alnum;
    }
}

// ------------------------------------------------------------------------------------------
// Keys: one arena + offsets, in file (= shuffled) order.
// ------------------------------------------------------------------------------------------

struct Keys {
    arena: Vec<u8>,
    offsets: Vec<u32>,
}
impl Keys {
    fn new() -> Self {
        Keys { arena: Vec::new(), offsets: vec![0] }
    }
    fn push(&mut self, k: &str) {
        self.arena.extend_from_slice(k.as_bytes());
        self.offsets.push(self.arena.len() as u32);
    }
    fn len(&self) -> usize {
        self.offsets.len() - 1
    }
    fn get(&self, i: usize) -> &str {
        unsafe { std::str::from_utf8_unchecked(&self.arena[self.offsets[i] as usize..self.offsets[i + 1] as usize]) }
    }
}

fn load_vocab(dir: &Path, v: usize) -> Keys {
    let f = BufReader::with_capacity(1 << 20, File::open(dir.join("vocab.txt")).unwrap());
    let mut keys = Keys::new();
    for line in f.lines().take(v) {
        keys.push(&line.unwrap());
    }
    assert_eq!(keys.len(), v, "vocab.txt holds fewer than {v} keys");
    keys
}

fn gen_hex32(n: usize) -> Keys {
    let mut keys = Keys::new();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut rng = Rng(7);
    let mut key = [0u8; 32];
    for _ in 0..n {
        let (h1, h2) = (rng.next(), rng.next());
        for (j, b) in h1.to_be_bytes().iter().chain(h2.to_be_bytes().iter()).enumerate() {
            key[j * 2] = HEX[(b >> 4) as usize];
            key[j * 2 + 1] = HEX[(b & 15) as usize];
        }
        keys.push(std::str::from_utf8(&key).unwrap());
    }
    keys
}

/// Sorted entries `(string, kind, position)`; kind 0 = key, 1 = word start. `position` is the
/// value's dense position — its rank in folded key order (the design's "dictionary position").
struct Entries<'k> {
    keys: &'k Keys,
    /// Folded-key rank → key index in `keys`.
    rank_to_key: Vec<u32>,
    /// (key index, byte offset of the entry within the key, kind) sorted by entry string.
    items: Vec<(u32, u16, u8)>,
}
impl<'k> Entries<'k> {
    fn build(keys: &'k Keys, words: bool) -> Self {
        let n = keys.len();
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|&a, &b| keys.get(a as usize).as_bytes().cmp(keys.get(b as usize).as_bytes()));
        // dedupe folded duplicates: keep the first of a run
        order.dedup_by(|a, b| keys.get(*a as usize) == keys.get(*b as usize));
        let mut items: Vec<(u32, u16, u8)> = Vec::with_capacity(n * 2);
        let mut ws = Vec::new();
        for &k in &order {
            items.push((k, 0, 0));
            if words {
                word_starts(keys.get(k as usize), &mut ws);
                for &w in &ws {
                    items.push((k, w, 1));
                }
            }
        }
        if words {
            items.sort_unstable_by(|a, b| {
                let sa = &keys.get(a.0 as usize).as_bytes()[a.1 as usize..];
                let sb = &keys.get(b.0 as usize).as_bytes()[b.1 as usize..];
                sa.cmp(sb).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0))
            });
        }
        Entries { keys, rank_to_key: order, items }
    }
    fn str_of(&self, i: usize) -> &str {
        let (k, off, _) = self.items[i];
        &self.keys.get(k as usize)[off as usize..]
    }
    fn len(&self) -> usize {
        self.items.len()
    }
    fn distinct_values(&self) -> usize {
        self.rank_to_key.len()
    }
}

fn prefix_upper(p: &[u8]) -> Vec<u8> {
    let mut u = p.to_vec();
    while let Some(last) = u.pop() {
        if last < 0xFF {
            u.push(last + 1);
            return u;
        }
    }
    u
}

/// Random prefixes of `chars` characters drawn from real keys (keys shorter than that are
/// redrawn).
fn sample_prefixes(keys: &Keys, chars: usize, n: usize, seed: u64) -> Vec<String> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let k = keys.get(rng.below(keys.len()));
        if k.chars().count() >= chars {
            let end = k.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(k.len());
            out.push(k[..end].to_string());
        }
    }
    out
}

// ------------------------------------------------------------------------------------------
// Arm 1 structures. Each answers a prefix with an entry range `[lo, hi)`.
// ------------------------------------------------------------------------------------------

/// (a) the existing shape: `BTreeMap<String, u32>`. Word-start duplicates of one string take a
/// `\0<pos>` suffix so the map stays one entry per (string, value).
struct BTree {
    map: BTreeMap<String, u32>,
}
impl BTree {
    fn build(e: &Entries) -> Self {
        let mut map = BTreeMap::new();
        let mut prev: Option<&str> = None;
        for i in 0..e.len() {
            let s = e.str_of(i);
            let key = if prev == Some(s) { format!("{s}\0{}", i) } else { s.to_string() };
            map.insert(key, i as u32);
            prev = Some(s);
        }
        BTree { map }
    }
    /// Positions the iterator at the range start and reads the first entry — what a walk needs
    /// to begin. A `BTreeMap` has no rank, so `hi` is not computed.
    fn lookup(&self, p: &str) -> (usize, usize) {
        match self.map.range::<str, _>((std::ops::Bound::Included(p), std::ops::Bound::Unbounded)).next() {
            Some((k, &v)) if k.as_bytes().starts_with(p.as_bytes()) => (v as usize, v as usize + 1),
            _ => (0, 0),
        }
    }
}

/// (b) a sorted string arena with `u32` offsets, two binary searches per prefix.
struct Arena {
    bytes: Vec<u8>,
    offsets: Vec<u32>,
    positions: Vec<u32>,
}
impl Arena {
    fn build(e: &Entries) -> Self {
        let mut a = Arena { bytes: Vec::new(), offsets: Vec::with_capacity(e.len() + 1), positions: Vec::with_capacity(e.len()) };
        a.offsets.push(0);
        // rank of a key index = its position in rank_to_key
        let mut rank_of = vec![0u32; e.keys.len()];
        for (r, &k) in e.rank_to_key.iter().enumerate() {
            rank_of[k as usize] = r as u32;
        }
        for i in 0..e.len() {
            a.bytes.extend_from_slice(e.str_of(i).as_bytes());
            a.offsets.push(a.bytes.len() as u32);
            a.positions.push(rank_of[e.items[i].0 as usize]);
        }
        a
    }
    fn at(&self, i: usize) -> &[u8] {
        &self.bytes[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
    fn len(&self) -> usize {
        self.positions.len()
    }
    fn lookup(&self, p: &str) -> (usize, usize) {
        let p = p.as_bytes();
        let lo = partition(self.len(), |i| self.at(i) < p);
        let hi = partition(self.len(), |i| self.at(i) < p || self.at(i).starts_with(p));
        (lo, hi)
    }
    fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.offsets.capacity() * 4 + self.positions.capacity() * 4
    }
}

fn partition<F: Fn(usize) -> bool>(n: usize, pred: F) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// (b′) the arena with word-start entries stored as references into the key arena rather than as
/// copied suffixes: 8 bytes per entry over the key bytes, no string duplication.
struct ArenaRef {
    keys: Vec<u8>,
    key_offsets: Vec<u32>, // by rank
    items: Vec<(u32, u16, u16)>, // (rank, byte offset, kind)
}
impl ArenaRef {
    fn build(e: &Entries) -> Self {
        let mut keys = Vec::new();
        let mut key_offsets = vec![0u32];
        let mut rank_of = vec![0u32; e.keys.len()];
        for (r, &k) in e.rank_to_key.iter().enumerate() {
            rank_of[k as usize] = r as u32;
            keys.extend_from_slice(e.keys.get(k as usize).as_bytes());
            key_offsets.push(keys.len() as u32);
        }
        let items = e.items.iter().map(|&(k, off, kind)| (rank_of[k as usize], off, kind as u16)).collect();
        ArenaRef { keys, key_offsets, items }
    }
    fn at(&self, i: usize) -> &[u8] {
        let (r, off, _) = self.items[i];
        &self.keys[self.key_offsets[r as usize] as usize + off as usize..self.key_offsets[r as usize + 1] as usize]
    }
    fn lookup(&self, p: &str) -> (usize, usize) {
        let p = p.as_bytes();
        let n = self.items.len();
        let lo = partition(n, |i| self.at(i) < p);
        let hi = partition(n, |i| self.at(i) < p || self.at(i).starts_with(p));
        (lo, hi)
    }
    fn heap_bytes(&self) -> usize {
        self.keys.capacity() + self.key_offsets.capacity() * 4 + self.items.capacity() * 8
    }
}

/// (c) an FST over the distinct entry strings, value = start offset into a sorted `positions`
/// array (so duplicates of one string — word starts shared by many values — are a run). For
/// key-only entries the value is the dense position itself and no side array is needed.
struct Fst {
    map: Map<Mmap>,
    positions: Vec<u32>, // empty for key-only
    file_bytes: u64,
    total: usize,
}
impl Fst {
    fn build(e: &Entries, path: &Path, words: bool) -> Self {
        let mut rank_of = vec![0u32; e.keys.len()];
        for (r, &k) in e.rank_to_key.iter().enumerate() {
            rank_of[k as usize] = r as u32;
        }
        let mut b = MapBuilder::new(BufWriter::with_capacity(1 << 20, File::create(path).unwrap())).unwrap();
        let mut positions = Vec::new();
        let mut prev: Option<&[u8]> = None;
        for i in 0..e.len() {
            let s = e.str_of(i).as_bytes();
            let pos = rank_of[e.items[i].0 as usize];
            if words {
                if prev != Some(s) {
                    b.insert(s, i as u64).unwrap();
                }
                positions.push(pos);
            } else {
                b.insert(s, pos as u64).unwrap();
            }
            prev = Some(s);
        }
        b.finish().unwrap();
        let file_bytes = std::fs::metadata(path).unwrap().len();
        let map = Map::new(unsafe { Mmap::map(&File::open(path).unwrap()).unwrap() }).unwrap();
        Fst { map, positions, file_bytes, total: e.len() }
    }
    fn lookup(&self, p: &str) -> (usize, usize) {
        let mut s = self.map.range().ge(p.as_bytes()).into_stream();
        let lo = match s.next() {
            Some((k, v)) if k.starts_with(p.as_bytes()) => v as usize,
            _ => return (0, 0),
        };
        let up = prefix_upper(p.as_bytes());
        let mut s = self.map.range().ge(&up).into_stream();
        let hi = match s.next() {
            Some((_, v)) => v as usize,
            None => self.total,
        };
        (lo, hi)
    }
}

/// (d) the repository's front-coded block dictionary (`SortedDictWriter`, K = 16) over the
/// distinct entry strings, plus — for word-start entries — a `starts` array (ordinal → first
/// entry) and the `positions` array, as the FST has.
struct Dict {
    dict: SortedDict,
    starts: Vec<u32>,
    positions: Vec<u32>,
    file_bytes: u64,
}
impl Dict {
    fn build(e: &Entries, path: &Path, words: bool) -> Self {
        let mut rank_of = vec![0u32; e.keys.len()];
        for (r, &k) in e.rank_to_key.iter().enumerate() {
            rank_of[k as usize] = r as u32;
        }
        let mut w = SortedDictWriter::new(BufWriter::with_capacity(1 << 20, File::create(path).unwrap())).unwrap();
        let mut starts = Vec::new();
        let mut positions = Vec::new();
        let mut prev: Option<&str> = None;
        for i in 0..e.len() {
            let s = e.str_of(i);
            if prev != Some(s) {
                w.push(s).unwrap();
                if words {
                    starts.push(i as u32);
                }
            }
            if words {
                positions.push(rank_of[e.items[i].0 as usize]);
            }
            prev = Some(s);
        }
        if words {
            starts.push(e.len() as u32);
        }
        let st = w.finish().unwrap();
        let dict = SortedDict::open(path, Access::Mapped).unwrap();
        Dict { dict, starts, positions, file_bytes: st.bytes }
    }
    fn lookup(&self, p: &str) -> (usize, usize) {
        let r = self.dict.prefix_range(p).unwrap();
        if self.starts.is_empty() {
            (r.start as usize, r.end as usize)
        } else {
            (self.starts[r.start as usize] as usize, self.starts[r.end as usize] as usize)
        }
    }
}

fn bench_prefixes<F: Fn(&str) -> (usize, usize)>(keys: &Keys, lookup: F, label: &str) {
    for &chars in &[1usize, 2, 3, 4, 8] {
        let prefixes = sample_prefixes(keys, chars, 2000, 1000 + chars as u64);
        // warm pass
        let mut sum = 0usize;
        for p in &prefixes {
            let (lo, hi) = lookup(p);
            sum += hi - lo;
        }
        let mut samples = Vec::with_capacity(prefixes.len());
        let mut widths = Vec::with_capacity(prefixes.len());
        for p in &prefixes {
            let t = Instant::now();
            let (lo, hi) = lookup(p);
            samples.push(t.elapsed().as_nanos() as f64);
            widths.push((hi - lo) as f64);
            sum += hi - lo;
        }
        let st = stats(&mut samples);
        widths.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "    {label} prefix {chars} chars: {st}  | range width median {} p99 {} (checksum {sum})",
            widths[widths.len() / 2],
            widths[(widths.len() as f64 * 0.99) as usize]
        );
    }
}

fn arm1(dir: &Path, structure: &str, v: usize, words: bool, hex: bool) {
    let t = Instant::now();
    let keys = if hex { gen_hex32(v) } else { load_vocab(dir, v) };
    println!(
        "== arm1 {structure} V={v} entries={} hex={hex}  (keys loaded in {:.1}s, {:.1} MB of key bytes, avg {:.1} B/key)",
        if words { "key+wordstart" } else { "key" },
        t.elapsed().as_secs_f64(),
        keys.arena.len() as f64 / 1e6,
        keys.arena.len() as f64 / v as f64
    );
    let t = Instant::now();
    let e = Entries::build(&keys, words);
    println!(
        "  entries: {} over {} distinct folded values ({:.2} entries/value), sort+derive {:.2}s",
        e.len(),
        e.distinct_values(),
        e.len() as f64 / e.distinct_values() as f64,
        t.elapsed().as_secs_f64()
    );
    let heap0 = heap();
    println!("  before build: heap {:.1} MB, RSS {:.0} MB", heap0 as f64 / 1e6, rss_mb());
    let file = dir.join(format!("arm1-{structure}-{v}-{}.bin", if words { "w" } else { "k" }));

    // Build three times, report the median build time; keep the last for lookups.
    let mut builds = Vec::new();
    macro_rules! run {
        ($build:expr, $bytes:expr, $lookup:expr) => {{
            let mut last = None;
            for _ in 0..3 {
                drop(last.take());
                let h0 = heap();
                let t = Instant::now();
                let s = $build;
                builds.push(t.elapsed().as_secs_f64());
                let h = heap() - h0;
                let (file_b, extra) = $bytes(&s);
                println!(
                    "  build {:.2}s  heap growth {:.1} MB  file {:.1} MB  side arrays {:.1} MB  RSS now {:.0} MB  peak RSS {:.0} MB",
                    builds.last().unwrap(),
                    h as f64 / 1e6,
                    file_b as f64 / 1e6,
                    extra as f64 / 1e6,
                    rss_mb(),
                    hwm_mb()
                );
                last = Some(s);
            }
            let s = last.unwrap();
            builds.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!("  build median of 3: {:.2}s", builds[1]);
            bench_prefixes(&keys, |p| $lookup(&s, p), structure);
        }};
    }
    match structure {
        "btree" => run!(BTree::build(&e), |_s: &BTree| (0u64, 0usize), |s: &BTree, p: &str| s.lookup(p)),
        "arena" => run!(Arena::build(&e), |s: &Arena| (0u64, s.heap_bytes()), |s: &Arena, p: &str| s.lookup(p)),
        "arenaref" => run!(ArenaRef::build(&e), |s: &ArenaRef| (0u64, s.heap_bytes()), |s: &ArenaRef, p: &str| s.lookup(p)),
        "fst" => run!(
            Fst::build(&e, &file, words),
            |s: &Fst| (s.file_bytes, s.positions.capacity() * 4),
            |s: &Fst, p: &str| s.lookup(p)
        ),
        "dict" => run!(
            Dict::build(&e, &file, words),
            |s: &Dict| (s.file_bytes, s.starts.capacity() * 4 + s.positions.capacity() * 4),
            |s: &Dict, p: &str| s.lookup(p)
        ),
        other => panic!("unknown structure {other}"),
    }
    std::fs::remove_file(&file).ok();
    println!("  end: heap {:.1} MB, RSS {:.0} MB, peak RSS {:.0} MB", heap() as f64 / 1e6, rss_mb(), hwm_mb());
}

// ------------------------------------------------------------------------------------------
// prep: fold name ∪ asciiname, dedupe, shuffle.
// ------------------------------------------------------------------------------------------

fn prep(tsv: &Path, dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let t = Instant::now();
    let f = BufReader::with_capacity(1 << 22, File::open(tsv).unwrap());
    let mut set: std::collections::HashSet<Box<str>> = std::collections::HashSet::with_capacity(12_000_000);
    let mut buf = String::new();
    let mut rows = 0usize;
    let mut raw = 0usize;
    for line in f.lines() {
        let line = line.unwrap();
        rows += 1;
        for col in line.split('\t').skip(1).take(2) {
            raw += 1;
            fold(col, &mut buf);
            if buf.is_empty() {
                continue;
            }
            if !set.contains(buf.as_str()) {
                set.insert(buf.as_str().into());
            }
        }
    }
    println!("prep: {rows} rows, {raw} raw strings, {} distinct folded, {:.1}s", set.len(), t.elapsed().as_secs_f64());
    let mut all: Vec<Box<str>> = set.into_iter().collect();
    all.sort_unstable();
    let mut rng = Rng(2026_09_02);
    for i in (1..all.len()).rev() {
        let j = rng.below(i + 1);
        all.swap(i, j);
    }
    let mut w = BufWriter::with_capacity(1 << 22, File::create(dir.join("vocab.txt")).unwrap());
    for k in &all {
        w.write_all(k.as_bytes()).unwrap();
        w.write_all(b"\n").unwrap();
    }
    w.flush().unwrap();
    println!("prep: wrote {} keys to {}", all.len(), dir.join("vocab.txt").display());
}

// ------------------------------------------------------------------------------------------
// gen: value column + postings for V values over N entities.
// ------------------------------------------------------------------------------------------

const SMALL_TERM_THRESHOLD: u32 = 32;
const ZIPF_S: f64 = 1.0;
const EMPTY_FRACTION: f64 = 0.02;

fn gen(dir: &Path, v: usize, n: usize, dist: &str) {
    let out = dir.join(dist);
    std::fs::create_dir_all(&out).unwrap();
    let t0 = Instant::now();
    let mut rng = Rng(11);
    // member count per value
    let mut counts: Vec<u32> = vec![0; v];
    match dist {
        "zipf" => {
            // Zipf(s=1) over popularity ranks, a floor of one member per value (the row that
            // minted it), and a random 2% of values emptied (the shape a suppressed or deleted
            // value leaves: a record with no members). Ranks are scattered over positions by a
            // random permutation so popularity is uncorrelated with lexical order.
            let mut perm: Vec<u32> = (0..v as u32).collect();
            for i in (1..v).rev() {
                let j = rng.below(i + 1);
                perm.swap(i, j);
            }
            let empties = (v as f64 * EMPTY_FRACTION) as usize;
            let live = v - empties;
            let h: f64 = (1..=live).map(|r| 1.0 / (r as f64).powf(ZIPF_S)).sum();
            let spare = n - live;
            let mut assigned = 0usize;
            for r in 1..=live {
                let c = 1 + ((spare as f64) / (r as f64).powf(ZIPF_S) / h).floor() as usize;
                counts[perm[r - 1] as usize] = c as u32;
                assigned += c;
            }
            // remainder onto rank 1
            counts[perm[0] as usize] += (n - assigned) as u32;
            println!("gen zipf: s={ZIPF_S}, {live} live values (floor 1), {empties} empty, head count {}, H={h:.2}", counts[perm[0] as usize]);
        }
        "uniform" => {
            for _ in 0..n {
                counts[rng.below(v)] += 1;
            }
            let empties = counts.iter().filter(|&&c| c == 0).count();
            println!("gen uniform: {n} entities over {v} values, max {} empty {empties}", counts.iter().max().unwrap());
        }
        other => panic!("unknown dist {other}"),
    }
    // value column: expand counts then shuffle
    let mut vals: Vec<u32> = Vec::with_capacity(n);
    for (val, &c) in counts.iter().enumerate() {
        for _ in 0..c {
            vals.push(val as u32);
        }
    }
    assert_eq!(vals.len(), n);
    for i in (1..n).rev() {
        let j = rng.below(i + 1);
        vals.swap(i, j);
    }
    {
        let mut w = BufWriter::with_capacity(1 << 22, File::create(out.join("values.u32")).unwrap());
        for x in &vals {
            w.write_all(&x.to_le_bytes()).unwrap();
        }
        w.flush().unwrap();
    }
    println!("gen: value column written ({:.1}s)", t0.elapsed().as_secs_f64());
    // bucket entities by value (ascending within a value by construction)
    let mut starts: Vec<u32> = vec![0; v + 1];
    let mut acc = 0u64;
    for i in 0..v {
        starts[i] = acc as u32;
        acc += counts[i] as u64;
    }
    starts[v] = acc as u32;
    let mut fill = starts.clone();
    let mut ents: Vec<u32> = vec![0; n];
    for (e, &val) in vals.iter().enumerate() {
        ents[fill[val as usize] as usize] = e as u32;
        fill[val as usize] += 1;
    }
    drop(vals);
    let t = Instant::now();
    let spool_path = out.join("postings.spool");
    let mut spool = PostingsSpool::create(&spool_path).unwrap();
    let mut tag1 = 0usize;
    let mut bytes = 0usize;
    for val in 0..v {
        let slice = &ents[starts[val] as usize..starts[val + 1] as usize];
        let rec = encode_posting(val, slice, SMALL_TERM_THRESHOLD).unwrap();
        if rec[0] == 1 {
            tag1 += 1;
        }
        bytes += rec.len();
        spool.append(&rec).unwrap();
    }
    spool.finish(&out.join("postings.arrow")).unwrap();
    println!(
        "gen: postings.arrow written: {v} records, {tag1} tag-1 (Roaring), {} record bytes, {:.1}s; file {:.1} MB; total {:.1}s",
        bytes,
        t.elapsed().as_secs_f64(),
        std::fs::metadata(out.join("postings.arrow")).unwrap().len() as f64 / 1e6,
        t0.elapsed().as_secs_f64()
    );
}

// ------------------------------------------------------------------------------------------
// Shared by arms 2 and 3: the fixture opened as the reader opens it, and candidates.
// ------------------------------------------------------------------------------------------

struct Fixture {
    column: ColumnPostings,
    reader: PostingsReader,
    values: Mmap,
    n: usize,
    members: Vec<u32>,
}

fn open_fixture(dir: &Path, dist: &str, v: usize) -> Fixture {
    let out = dir.join(dist);
    let path = out.join("postings.arrow");
    // Startup cost: PostingsReader::open validates every record (round-trips each payload).
    let mmap_s = median3(|| {
        let t = Instant::now();
        let r = PostingsReader::open(&path, true).unwrap();
        let s = t.elapsed().as_secs_f64();
        drop(r);
        s
    });
    let read_s = median3(|| {
        let t = Instant::now();
        let r = PostingsReader::open(&path, false).unwrap();
        let s = t.elapsed().as_secs_f64();
        drop(r);
        s
    });
    println!(
        "open: PostingsReader::open({} records) median of 3: mmap {:.3}s, read {:.3}s (page-cache warm)",
        v, mmap_s, read_s
    );
    let column = ColumnPostings::open(&path, true).unwrap();
    let reader = PostingsReader::open(&path, true).unwrap();
    assert_eq!(reader.term_count() as usize, v);
    let values = unsafe { Mmap::map(&File::open(out.join("values.u32")).unwrap()).unwrap() };
    let n = values.len() / 4;
    let t = Instant::now();
    let mut members = vec![0u32; v];
    for i in 0..v {
        members[i] = match reader.posting_at(i as u32).unwrap().unwrap() {
            PostingRef::Array(b) => (b.len() / 4) as u32,
            PostingRef::Roaring(view) => view.cardinality() as u32,
        };
    }
    let empties = members.iter().filter(|&&m| m == 0).count();
    let tag1 = members.iter().filter(|&&m| m > SMALL_TERM_THRESHOLD).count();
    println!(
        "fixture: N={n} V={v}; member counts read in {:.2}s; {empties} empty, {tag1} Roaring, max {}",
        t.elapsed().as_secs_f64(),
        members.iter().max().unwrap()
    );
    Fixture { column, reader, values, n, members }
}

fn value_at(f: &Fixture, e: u32) -> u32 {
    let i = e as usize * 4;
    u32::from_le_bytes(f.values[i..i + 4].try_into().unwrap())
}

const SPARSITIES: [f64; 4] = [1e-4, 1e-3, 1e-2, 1e-1];

fn candidate(n: usize, frac: f64, contiguous: bool, seed: u64) -> Bitmap {
    let k = (n as f64 * frac) as usize;
    let mut rng = Rng(seed);
    if contiguous {
        let start = rng.below(n - k) as u32;
        Bitmap::from_range(start..start + k as u32)
    } else {
        let mut b = Bitmap::new();
        let mut buf = Vec::with_capacity(k + 1000);
        for e in 0..n as u32 {
            if rng.unit() < frac {
                buf.push(e);
            }
        }
        b.add_many(&buf);
        b
    }
}

fn shape_name(contiguous: bool) -> &'static str {
    if contiguous { "contiguous" } else { "scattered" }
}

/// The probe: `members(v) ∩ candidate ≠ ∅` by `ColumnPostings::narrow` (allocates only the
/// intersection).
#[inline(never)]
fn probe(f: &Fixture, pos: u32, cand: &Bitmap) -> bool {
    !f.column.narrow(AttrLocalId::new(pos), cand).unwrap().is_empty()
}

/// The same predicate through `Bitmap::intersect` on the mapped view — a boolean that
/// short-circuits on the first common container and allocates nothing.
#[inline(never)]
fn probe_intersect(f: &Fixture, pos: u32, cand: &Bitmap) -> bool {
    match f.reader.posting_at(pos).unwrap() {
        None => false,
        Some(PostingRef::Array(b)) => b.chunks_exact(4).any(|c| cand.contains(u32::from_le_bytes(c.try_into().unwrap()))),
        Some(PostingRef::Roaring(view)) => cand.intersect(&view),
    }
}

// ------------------------------------------------------------------------------------------
// Arm 2.
// ------------------------------------------------------------------------------------------

fn arm2(dir: &Path, v: usize, dist: &str) {
    println!("== arm2 {dist} V={v}");
    let f = open_fixture(dir, dist, v);
    let keys = load_vocab(dir, v);
    let t = Instant::now();
    let e = Entries::build(&keys, false);
    let arena = Arena::build(&e);
    println!("index: key-only arena over {} values built in {:.2}s", arena.len(), t.elapsed().as_secs_f64());
    let dense = arena.len(); // dense positions == folded ranks; the postings hold V records
    assert!(dense <= v);

    let bucket = |m: u32| -> &'static str {
        match m {
            0 => "0",
            1..=32 => "1-32 (tag 0)",
            33..=1_000 => "33-1e3",
            1_001..=100_000 => "1e3-1e5",
            _ => ">1e5",
        }
    };
    let buckets = ["0", "1-32 (tag 0)", "33-1e3", "1e3-1e5", ">1e5"];

    for &frac in &SPARSITIES {
        for &contiguous in &[true, false] {
            let cand = candidate(f.n, frac, contiguous, 99);
            println!("-- candidate {frac:.0e} {} |cand|={} ({} containers)", shape_name(contiguous), cand.cardinality(), cand.statistics().n_containers);

            // ---- per-value probe cost, split by outcome and by member-count bucket ----------
            let mut rng = Rng(5);
            let mut sample: Vec<u32> = (0..6000).map(|_| rng.below(dense) as u32).collect();
            // make sure the head is represented: the 300 largest values
            let mut by_size: Vec<u32> = (0..dense as u32).collect();
            by_size.select_nth_unstable_by(300, |&a, &b| f.members[b as usize].cmp(&f.members[a as usize]));
            sample.extend_from_slice(&by_size[..300]);
            let mut class_samples: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            let mut class_samples_i: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            let mut bucket_samples: Vec<(&str, f64, bool)> = Vec::new();
            for &pos in &sample {
                probe(&f, pos, &cand); // warm the pages this record touches
                let t = Instant::now();
                let visible = probe(&f, pos, &cand);
                let ns = t.elapsed().as_nanos() as f64;
                let t = Instant::now();
                let visible_i = probe_intersect(&f, pos, &cand);
                let ns_i = t.elapsed().as_nanos() as f64;
                assert_eq!(visible, visible_i);
                let m = f.members[pos as usize];
                let class = if m == 0 { 0 } else if !visible { 1 } else { 2 };
                class_samples[class].push(ns);
                class_samples_i[class].push(ns_i);
                bucket_samples.push((bucket(m), ns, visible));
            }
            let names = ["hidden, no members", "hidden, members disjoint", "visible"];
            for c in 0..3 {
                println!("   probe[{}] narrow:    {}", names[c], stats(&mut class_samples[c]));
                println!("   probe[{}] intersect: {}", names[c], stats(&mut class_samples_i[c]));
            }
            for b in buckets {
                for vis in [false, true] {
                    let mut s: Vec<f64> = bucket_samples.iter().filter(|x| x.0 == b && x.2 == vis).map(|x| x.1).collect();
                    if !s.is_empty() {
                        println!("   probe by members {b:>12} {}: {}", if vis { "visible" } else { "hidden " }, stats(&mut s));
                    }
                }
            }

            // ---- budgeted walk ------------------------------------------------------------
            for &chars in &[1usize, 2, 3, 4] {
                let prefixes = sample_prefixes(&keys, chars, 100, 7000 + chars as u64);
                for &budget in &[1_000usize, 10_000, 100_000] {
                    let mut lat = Vec::new();
                    let mut spent = 0usize;
                    let mut filled = 0usize;
                    let mut exhausted = 0usize;
                    let mut examined_total = 0usize;
                    let mut emitted_total = 0usize;
                    for p in &prefixes {
                        let t = Instant::now();
                        let (lo, hi) = arena.lookup(p);
                        let mut emitted = 0usize;
                        let mut examined = 0usize;
                        let mut i = lo;
                        while i < hi && emitted < 20 && examined < budget {
                            let pos = arena.positions[i];
                            examined += 1;
                            if probe(&f, pos, &cand) {
                                emitted += 1;
                            }
                            i += 1;
                        }
                        lat.push(t.elapsed().as_nanos() as f64);
                        if emitted == 20 {
                            filled += 1;
                        } else if examined >= budget {
                            spent += 1;
                        } else {
                            exhausted += 1;
                        }
                        examined_total += examined;
                        emitted_total += emitted;
                    }
                    let st = stats(&mut lat);
                    println!(
                        "   walk prefix {chars} budget {budget:>6}: median {:.2} ms  p99 {:.2} ms  max {:.2} ms | page filled {filled}/{} budget spent {spent} range exhausted {exhausted} | mean examined {:.0} mean emitted {:.1}",
                        st.median_us / 1e3,
                        st.p99_us / 1e3,
                        st.max_us / 1e3,
                        prefixes.len(),
                        examined_total as f64 / prefixes.len() as f64,
                        emitted_total as f64 / prefixes.len() as f64
                    );
                }
            }
        }
    }
    println!("end: RSS {:.0} MB peak {:.0} MB heap {:.0} MB", rss_mb(), hwm_mb(), heap() as f64 / 1e6);
}

// ------------------------------------------------------------------------------------------
// Arm 3.
// ------------------------------------------------------------------------------------------

fn arm3(dir: &Path, v: usize, dist: &str) {
    println!("== arm3 {dist} V={v}");
    let f = open_fixture(dir, dist, v);
    let keys = load_vocab(dir, v);
    let e = Entries::build(&keys, false);
    let arena = Arena::build(&e);
    let dense = arena.len();

    for &frac in &SPARSITIES {
        for &contiguous in &[true, false] {
            let cand = candidate(f.n, frac, contiguous, 99);
            println!("-- candidate {frac:.0e} {} |cand|={}", shape_name(contiguous), cand.cardinality());

            // (i) one pass over the u32 value column under the candidate
            let mut vis_col = Bitmap::new();
            let s_col = median3(|| {
                let t = Instant::now();
                let mut b = Bitmap::new();
                for e in cand.iter() {
                    b.add(value_at(&f, e));
                }
                let s = t.elapsed().as_secs_f64();
                vis_col = b;
                s
            });
            let mut words = vec![0u64; (v + 63) / 64];
            let s_col_bits = median3(|| {
                let t = Instant::now();
                for w in words.iter_mut() {
                    *w = 0;
                }
                for e in cand.iter() {
                    let val = value_at(&f, e) as usize;
                    words[val / 64] |= 1 << (val % 64);
                }
                t.elapsed().as_secs_f64()
            });
            // (i′) the same pass with the visible values collected then added in bulk
            let s_col_bulk = median3(|| {
                let t = Instant::now();
                let mut buf: Vec<u32> = cand.iter().map(|e| value_at(&f, e)).collect();
                buf.sort_unstable();
                buf.dedup();
                let mut b = Bitmap::new();
                b.add_many(&buf);
                let s = t.elapsed().as_secs_f64();
                assert_eq!(b, vis_col);
                s
            });
            println!(
                "   setup (i) value column pass: roaring add {:.1} ms | bitset {:.1} ms | collect+sort+add_many {:.1} ms  → {} visible values",
                s_col * 1e3,
                s_col_bits * 1e3,
                s_col_bulk * 1e3,
                vis_col.cardinality()
            );

            // (ii) one pass over all V postings
            let mut vis_post = Bitmap::new();
            let s_post_narrow = median3(|| {
                let t = Instant::now();
                let mut b = Bitmap::new();
                for pos in 0..v as u32 {
                    if probe(&f, pos, &cand) {
                        b.add(pos);
                    }
                }
                let s = t.elapsed().as_secs_f64();
                vis_post = b;
                s
            });
            assert_eq!(vis_post, vis_col, "the two routes must agree");
            let s_post_intersect = median3(|| {
                let t = Instant::now();
                let mut b = Bitmap::new();
                for pos in 0..v as u32 {
                    if probe_intersect(&f, pos, &cand) {
                        b.add(pos);
                    }
                }
                let s = t.elapsed().as_secs_f64();
                assert_eq!(b, vis_col);
                s
            });
            println!(
                "   setup (ii) all-postings pass: narrow {:.2} s | intersect {:.2} s   ({:.0} ns / {:.0} ns per value)",
                s_post_narrow,
                s_post_intersect,
                s_post_narrow * 1e9 / v as f64,
                s_post_intersect * 1e9 / v as f64
            );
            let mut opt = vis_col.clone();
            opt.run_optimize();
            println!(
                "   visible set bytes: roaring portable {:.1} KB (run-optimised {:.1} KB) | V bits {:.1} KB | 4V bits {:.1} KB  (density {:.3}%)",
                vis_col.get_serialized_size_in_bytes::<Portable>() as f64 / 1e3,
                opt.get_serialized_size_in_bytes::<Portable>() as f64 / 1e3,
                v as f64 / 8.0 / 1e3,
                4.0 * v as f64 / 8.0 / 1e3,
                100.0 * vis_col.cardinality() as f64 / v as f64
            );

            // per-keystroke: iterate visible bits inside the prefix range
            for &chars in &[1usize, 2, 3, 4] {
                let prefixes = sample_prefixes(&keys, chars, 1000, 7000 + chars as u64);
                let mut lat = Vec::new();
                let mut lat_bits = Vec::new();
                let mut emitted_total = 0usize;
                let mut filled = 0usize;
                let mut it = vis_col.iter();
                for p in &prefixes {
                    let t = Instant::now();
                    let (lo, hi) = arena.lookup(p);
                    // positions in the arena are ranks; for key-only they are ascending with i
                    let (plo, phi) = if lo < hi { (arena.positions[lo], arena.positions[hi - 1] + 1) } else { (0, 0) };
                    it.reset_at_or_after(plo);
                    let mut emitted = 0usize;
                    while let Some(pos) = it.next() {
                        if pos >= phi || emitted == 20 {
                            break;
                        }
                        emitted += 1;
                    }
                    lat.push(t.elapsed().as_nanos() as f64);
                    emitted_total += emitted;
                    if emitted == 20 {
                        filled += 1;
                    }
                    // the plain bitset: scan words
                    let t = Instant::now();
                    let (lo, hi) = arena.lookup(p);
                    let (plo, phi) = if lo < hi { (arena.positions[lo] as usize, arena.positions[hi - 1] as usize + 1) } else { (0, 0) };
                    let mut emitted = 0usize;
                    let mut pos = plo;
                    while pos < phi && emitted < 20 {
                        let w = words[pos / 64] >> (pos % 64);
                        if w == 0 {
                            pos = (pos / 64 + 1) * 64;
                            continue;
                        }
                        pos += w.trailing_zeros() as usize;
                        if pos < phi {
                            emitted += 1;
                        }
                        pos += 1;
                    }
                    lat_bits.push(t.elapsed().as_nanos() as f64);
                }
                println!(
                    "   keystroke prefix {chars}: roaring reset_at_or_after {} | bitset scan median {:.2} us p99 {:.2} us | page filled {filled}/{} mean emitted {:.1}",
                    stats(&mut lat),
                    { let s = stats(&mut lat_bits); s.median_us },
                    stats(&mut lat_bits).p99_us,
                    prefixes.len(),
                    emitted_total as f64 / prefixes.len() as f64
                );
            }
        }
    }
    let _ = dense;
    println!("end: RSS {:.0} MB peak {:.0} MB heap {:.0} MB", rss_mb(), hwm_mb(), heap() as f64 / 1e6);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let dir = PathBuf::from(args.get(2).expect("DIR"));
    println!("cmd: {}", args.join(" "));
    match cmd {
        "prep" => prep(&PathBuf::from(&args[2]), &PathBuf::from(&args[3])),
        "arm1" => {
            let structure = &args[3];
            let v: usize = args[4].parse().unwrap();
            let words = args[5] == "words";
            let hex = args.get(6).map(String::as_str) == Some("hex");
            arm1(&dir, structure, v, words, hex);
        }
        "gen" => gen(&dir, args[3].parse().unwrap(), args[4].parse().unwrap(), &args[5]),
        "arm2" => arm2(&dir, args[3].parse().unwrap(), &args[4]),
        "arm3" => arm3(&dir, args[3].parse().unwrap(), &args[4]),
        other => panic!("unknown command {other}"),
    }
}
