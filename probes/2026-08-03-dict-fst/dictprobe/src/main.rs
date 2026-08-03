//! Dict representation probe: FST vs FxHashMap at surnames-config scale.
//!
//! Namespaces:
//!   decimal  — decimal strings of 0..N (the probe bundles' actual descriptor bytes)
//!   surname  — real surname descriptors x replica suffix ("{s}@r{r}"), the realistic shape
//!   hex32    — 32 random hex chars per key, the incompressible worst case
//!
//! Per run: FST build time / file size / mmap-open + lookup latency (hit and miss), then
//! FxHashMap<Box<[u8]>, u32> build time / resident growth / lookup latency.

use fst::{Map, MapBuilder};
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::{BufWriter, Read};
use std::time::Instant;

fn rss_gb() -> f64 {
    let mut s = String::new();
    File::open("/proc/self/statm").unwrap().read_to_string(&mut s).unwrap();
    let pages: f64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096.0 / 1e9
}

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// One arena of key bytes + (offset, len) spans, in "intern order" (ordinal = index).
struct Keys {
    arena: Vec<u8>,
    spans: Vec<(u64, u32)>,
}

impl Keys {
    fn get(&self, i: usize) -> &[u8] {
        let (off, len) = self.spans[i];
        &self.arena[off as usize..off as usize + len as usize]
    }
    fn len(&self) -> usize {
        self.spans.len()
    }
    fn push(&mut self, key: &[u8]) {
        self.spans.push((self.arena.len() as u64, key.len() as u32));
        self.arena.extend_from_slice(key);
    }
}

fn decimal(mut v: usize, buf: &mut [u8; 20]) -> usize {
    let mut i = 20;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    i
}

fn gen_decimal(n: usize) -> Keys {
    let mut k = Keys { arena: Vec::with_capacity(n * 9), spans: Vec::with_capacity(n) };
    let mut buf = [0u8; 20];
    for i in 0..n {
        let s = decimal(i, &mut buf);
        k.push(&buf[s..]);
    }
    k
}

fn gen_surname(n: usize, surnames: &[&str]) -> Keys {
    let mut k = Keys { arena: Vec::with_capacity(n * 14), spans: Vec::with_capacity(n) };
    let mut buf = [0u8; 20];
    let mut i = 0usize;
    let mut r = 0usize;
    'outer: loop {
        let s0 = decimal(r, &mut buf);
        let suffix = &buf[s0..];
        for s in surnames {
            if i == n {
                break 'outer;
            }
            let (off, base) = (k.arena.len() as u64, s.as_bytes());
            k.arena.extend_from_slice(base);
            k.arena.extend_from_slice(b"@r");
            k.arena.extend_from_slice(suffix);
            k.spans.push((off, (base.len() + 2 + suffix.len()) as u32));
            i += 1;
        }
        r += 1;
    }
    k
}

fn gen_hex32(n: usize) -> Keys {
    let mut k = Keys { arena: Vec::with_capacity(n * 32), spans: Vec::with_capacity(n) };
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for i in 0..n {
        // two xorshift outputs per key -> 128 bits, hex-encoded
        let mut a = XorShift((0x9E3779B97F4A7C15u64.wrapping_mul(i as u64 + 1)) | 1);
        let (h1, h2) = (a.next(), a.next());
        let mut key = [0u8; 32];
        for (j, b) in h1.to_be_bytes().iter().chain(h2.to_be_bytes().iter()).enumerate() {
            key[j * 2] = HEX[(b >> 4) as usize];
            key[j * 2 + 1] = HEX[(b & 15) as usize];
        }
        k.push(&key);
    }
    k
}

/// Ordinal assignment: identity models intern order correlated with the namespace's own
/// structure; "shuf" scatters ordinals through a non-linear bijection, modelling intern order
/// uncorrelated with lexicographic order — the pessimistic end for FST output sharing, which is
/// where the per-key cost actually lives. An affine permutation is NOT sufficient here: it
/// decomposes additively over a cross-product key language and compresses spuriously (measured:
/// 0.27 B/key affine vs 6.74 B/key fmix32 on the surname namespace at 1.17e8).
fn value_of(i: usize, shuffle: bool) -> u64 {
    if !shuffle {
        return i as u64;
    }
    // murmur3 fmix32: bijective on u32, genuinely non-linear — an affine permutation still
    // decomposes additively over a cross-product key language and compresses spuriously.
    let mut x = i as u32;
    x ^= x >> 16;
    x = x.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 13;
    x = x.wrapping_mul(0xC2B2_AE35);
    x ^= x >> 16;
    x as u64
}

fn bench_lookups<F: Fn(&[u8]) -> Option<u64>>(keys: &Keys, lookup: F, label: &str) {
    let mut rng = XorShift(42);
    let rounds = 1_000_000usize;
    // hits
    let t = Instant::now();
    let mut sum = 0u64;
    for _ in 0..rounds {
        let i = (rng.next() % keys.len() as u64) as usize;
        sum += lookup(keys.get(i)).expect("present key must resolve");
    }
    let hit_ns = t.elapsed().as_nanos() as f64 / rounds as f64;
    // misses: present key + 0xFF suffix byte (never a valid key in any namespace here)
    let mut miss_buf = Vec::with_capacity(80);
    let t = Instant::now();
    let mut misses = 0usize;
    for _ in 0..rounds {
        let i = (rng.next() % keys.len() as u64) as usize;
        miss_buf.clear();
        miss_buf.extend_from_slice(keys.get(i));
        miss_buf.push(0xFF);
        if lookup(&miss_buf).is_none() {
            misses += 1;
        }
    }
    let miss_ns = t.elapsed().as_nanos() as f64 / rounds as f64;
    assert_eq!(misses, rounds, "miss keys must all miss");
    println!("    {label}: hit {hit_ns:.0} ns, miss {miss_ns:.0} ns  (checksum {sum})");
}

fn run(name: &str, keys: Keys, fst_path: &str, shuffle: bool) {
    let n = keys.len();
    let key_bytes = keys.arena.len();
    println!(
        "== {name}  n={n}  key bytes total {:.2} GB (avg {:.1} B/key)",
        key_bytes as f64 / 1e9,
        key_bytes as f64 / n as f64
    );

    // ---- sort spans lexicographically for FST construction --------------------------------
    let t = Instant::now();
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_unstable_by(|&a, &b| keys.get(a as usize).cmp(keys.get(b as usize)));
    println!("  sort: {:.1}s", t.elapsed().as_secs_f64());

    // ---- FST build ------------------------------------------------------------------------
    let t = Instant::now();
    let mut builder = MapBuilder::new(BufWriter::new(File::create(fst_path).unwrap())).unwrap();
    for &i in &order {
        // value = ordinal in intern order, exactly the TermId a Dict would answer
        builder.insert(keys.get(i as usize), value_of(i as usize, shuffle)).unwrap();
    }
    builder.finish().unwrap();
    let build_s = t.elapsed().as_secs_f64();
    let fst_size = std::fs::metadata(fst_path).unwrap().len();
    println!(
        "  fst: build {:.1}s, file {:.3} GB = {:.2} B/key",
        build_s,
        fst_size as f64 / 1e9,
        fst_size as f64 / n as f64
    );
    drop(order);

    // ---- FST open + lookups ---------------------------------------------------------------
    let rss0 = rss_gb();
    let t = Instant::now();
    let mmap = unsafe { Mmap::map(&File::open(fst_path).unwrap()).unwrap() };
    let map = Map::new(mmap).unwrap();
    println!("  fst: open {:.3} ms", t.elapsed().as_secs_f64() * 1e3);
    bench_lookups(&keys, |k| map.get(k), "fst lookups (cold pages)");
    bench_lookups(&keys, |k| map.get(k), "fst lookups (warm)");
    println!("  fst: resident growth after lookups {:.2} GB", rss_gb() - rss0);
    drop(map);

    // ---- FxHashMap ------------------------------------------------------------------------
    let rss0 = rss_gb();
    let t = Instant::now();
    let mut hm: FxHashMap<Box<[u8]>, u32> = FxHashMap::default();
    hm.reserve(n);
    for i in 0..n {
        hm.insert(keys.get(i).to_vec().into_boxed_slice(), value_of(i, shuffle) as u32);
    }
    println!(
        "  fxhashmap: build (== open cost every restart) {:.1}s, resident growth {:.2} GB",
        t.elapsed().as_secs_f64(),
        rss_gb() - rss0
    );
    bench_lookups(&keys, |k| hm.get(k).map(|&v| v as u64), "map lookups");
    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let namespace = args.get(1).map(String::as_str).unwrap_or("decimal");
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10_000_000);
    let shuffle = args.get(3).map(String::as_str) == Some("shuf");
    let scratch = std::env::var("PROBE_DIR").unwrap_or_else(|_| ".".into());
    let fst_path = format!("{scratch}/{namespace}-{n}.fst");

    let keys = match namespace {
        "decimal" => gen_decimal(n),
        "surname" => {
            let text = std::fs::read_to_string(format!("{scratch}/surnames.txt")).unwrap();
            let surnames: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            gen_surname(n, &surnames)
        }
        "hex32" => gen_hex32(n),
        other => panic!("unknown namespace {other}"),
    };
    run(
        &format!("{namespace}{}", if shuffle { " (shuffled ordinals)" } else { "" }),
        keys,
        &fst_path,
        shuffle,
    );
    std::fs::remove_file(&fst_path).ok();
}
