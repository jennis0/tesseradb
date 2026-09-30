//! Times bottom-up authorise over a shared label DAG, against today's union of term postings.
//!
//! Usage: labeldag [A100k|A500k|B|C ...]   (default: all four)
//! Environment: LABELDAG_ITEMS_A, LABELDAG_ITEMS_B, LABELDAG_ITEMS_C scale the corpora.

mod dag;
mod expr;

use croaring::Bitmap;
use dag::{Builder, Dag, PassStats, Scratch, Up, LEAF, NONE, OR};
use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use rustc_hash::FxHashSet;
use std::time::Instant;

const RANDOM_CREDENTIALS: usize = 25;
const REPS_PER_RANDOM: usize = 8;
const REPS_WORST: usize = 100;
const SIZES: [usize; 3] = [10, 100, 1000];

struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    fn new(n: usize, s: f64) -> Self {
        let mut acc = 0.0;
        let cdf = (1..=n)
            .map(|i| {
                acc += 1.0 / (i as f64).powf(s);
                acc
            })
            .collect();
        Zipf { cdf }
    }
    fn sample(&self, rng: &mut StdRng) -> usize {
        let u = rng.gen::<f64>() * self.cdf.last().unwrap();
        self.cdf.partition_point(|&c| c < u).min(self.cdf.len() - 1)
    }
    fn weight(&self, i: usize) -> f64 {
        self.cdf[i] - if i == 0 { 0.0 } else { self.cdf[i - 1] }
    }
    /// `k` distinct indices by rejection; for small `k` only.
    fn distinct(&self, k: usize, rng: &mut StdRng) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(k);
        while out.len() < k {
            let x = self.sample(rng);
            if !out.contains(&x) {
                out.push(x);
            }
        }
        out
    }
    /// `k` distinct indices weighted by popularity (Efraimidis–Spirakis), for any `k`.
    fn weighted_without_replacement(&self, k: usize, rng: &mut StdRng) -> Vec<usize> {
        let mut keyed: Vec<(f64, usize)> = (0..self.cdf.len())
            .map(|i| (rng.gen::<f64>().ln() / self.weight(i), i))
            .collect();
        let k = k.min(keyed.len());
        let nth = k.saturating_sub(1);
        keyed.select_nth_unstable_by(nth, |a, b| {
            b.0.partial_cmp(&a.0).unwrap()
        });
        keyed.truncate(k);
        keyed.into_iter().map(|(_, i)| i).collect()
    }
}

/// A generated expression: its printed form with operands shuffled, and a canonical form used
/// to keep generated labels distinct.
struct G {
    p: String,
    c: String,
    compound: bool,
}

fn t(name: String) -> G {
    G { p: name.clone(), c: name, compound: false }
}

fn op(sep: &str, mut parts: Vec<G>, rng: &mut StdRng) -> G {
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    let wrap = |g: &G, s: &String| if g.compound { format!("({s})") } else { s.clone() };
    let mut canon: Vec<String> = parts.iter().map(|g| wrap(g, &g.c)).collect();
    canon.sort();
    parts.shuffle(rng);
    let printed: Vec<String> = parts.iter().map(|g| wrap(g, &g.p)).collect();
    G { p: printed.join(sep), c: canon.join(sep), compound: true }
}

fn and(parts: Vec<G>, rng: &mut StdRng) -> G {
    op("&", parts, rng)
}
fn or(parts: Vec<G>, rng: &mut StdRng) -> G {
    op("|", parts, rng)
}

struct Corpus {
    name: String,
    /// Labels in the order they are written: one per distinct label for A and C, one per item
    /// for B.
    labels: Vec<String>,
    /// Items carried by each entry of `labels`.
    items_per_entry: Vec<u32>,
    /// Credentials drawn to hit labels, per credential size.
    credentials: Vec<(usize, Vec<Vec<String>>)>,
    /// Whether today's scheme can express every label, and so a baseline is comparable.
    baseline: bool,
}

fn zipf_counts(n: usize, total: u64, s: f64, rng: &mut StdRng) -> Vec<u32> {
    let z = Zipf::new(n, s);
    let sum = *z.cdf.last().unwrap();
    let mut ranks: Vec<usize> = (0..n).collect();
    ranks.shuffle(rng);
    ranks
        .iter()
        .map(|&r| ((total as f64 * z.weight(r) / sum).round() as u32).max(1))
        .collect()
}

const CLS: usize = 5;
const CMP: usize = 20;
const TEAM: usize = 600;
const REL: usize = 200;
const PROJ: usize = 175;

fn corpus_a(n_labels: usize, items: u64, seed: u64) -> Corpus {
    let mut rng = StdRng::seed_from_u64(seed);
    let cls_w = [0.35, 0.30, 0.20, 0.10, 0.05];
    let cmp = Zipf::new(CMP, 1.0);
    let team = Zipf::new(TEAM, 1.0);
    let rel = Zipf::new(REL, 1.0);
    let proj = Zipf::new(PROJ, 1.0);
    let pick_cls = |rng: &mut StdRng| {
        let u: f64 = rng.gen();
        let mut acc = 0.0;
        for (i, w) in cls_w.iter().enumerate() {
            acc += w;
            if u < acc {
                return t(format!("cls:{i}"));
            }
        }
        t("cls:4".into())
    };
    let names = |z: &Zipf, pre: &str, k: usize, rng: &mut StdRng| -> Vec<G> {
        z.distinct(k, rng).into_iter().map(|i| t(format!("{pre}:{i}"))).collect()
    };
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut labels = Vec::with_capacity(n_labels);
    while labels.len() < n_labels {
        let shape: f64 = rng.gen();
        let g = if shape < 0.30 {
            let k = rng.gen_range(1..=6);
            let ts = names(&team, "team", k, &mut rng);
            let o = or(ts, &mut rng);
            and(vec![pick_cls(&mut rng), o], &mut rng)
        } else if shape < 0.50 {
            let (a, b) = (rng.gen_range(1..=4), rng.gen_range(1..=3));
            let ts = or(names(&team, "team", a, &mut rng), &mut rng);
            let rs = or(names(&rel, "rel", b, &mut rng), &mut rng);
            and(vec![pick_cls(&mut rng), ts, rs], &mut rng)
        } else if shape < 0.62 {
            let c = rng.gen_range(1..=3);
            let mut parts = names(&cmp, "cmp", c, &mut rng);
            let k = rng.gen_range(1..=5);
            parts.push(or(names(&team, "team", k, &mut rng), &mut rng));
            parts.push(pick_cls(&mut rng));
            and(parts, &mut rng)
        } else if shape < 0.72 {
            let k = rng.gen_range(1..=2);
            let l = and(
                vec![pick_cls(&mut rng), or(names(&team, "team", k, &mut rng), &mut rng)],
                &mut rng,
            );
            let k = rng.gen_range(1..=2);
            let r = and(
                vec![
                    t(format!("proj:{}", proj.sample(&mut rng))),
                    or(names(&rel, "rel", k, &mut rng), &mut rng),
                ],
                &mut rng,
            );
            or(vec![l, r], &mut rng)
        } else if shape < 0.82 {
            let k = rng.gen_range(2..=30);
            let rs = or(names(&rel, "rel", k, &mut rng), &mut rng);
            and(vec![pick_cls(&mut rng), rs], &mut rng)
        } else if shape < 0.92 {
            let k = rng.gen_range(1..=3);
            let ts = team.distinct(k, &mut rng);
            let rs = rel.distinct(k, &mut rng);
            let mut alts = vec![t(format!("proj:{}", proj.sample(&mut rng)))];
            for (a, b) in ts.into_iter().zip(rs) {
                alts.push(and(vec![t(format!("team:{a}")), t(format!("rel:{b}"))], &mut rng));
            }
            let o = or(alts, &mut rng);
            and(vec![pick_cls(&mut rng), o], &mut rng)
        } else if shape < 0.97 {
            let k = rng.gen_range(2..=20);
            let ts = team.distinct(k, &mut rng);
            let rs = rel.distinct(k, &mut rng);
            let mut alts = Vec::new();
            for (a, b) in ts.into_iter().zip(rs) {
                let c = pick_cls(&mut rng);
                alts.push(and(vec![c, t(format!("team:{a}")), t(format!("rel:{b}"))], &mut rng));
            }
            or(alts, &mut rng)
        } else {
            let c = rng.gen_range(0..=2);
            let mut parts = names(&cmp, "cmp", c, &mut rng);
            let a = rng.gen_range(5..=15);
            let b = rng.gen_range(10..=40);
            parts.push(or(names(&team, "team", a, &mut rng), &mut rng));
            parts.push(or(names(&rel, "rel", b, &mut rng), &mut rng));
            parts.push(pick_cls(&mut rng));
            and(parts, &mut rng)
        };
        if seen.insert(g.c) {
            labels.push(g.p);
        }
    }
    let items_per_entry = zipf_counts(n_labels, items, 1.0, &mut rng);

    // A person: every classification up to their own, a few compartments, then teams, regions
    // and projects by popularity.
    let mut credentials = Vec::new();
    for &k in &SIZES {
        let mut creds = Vec::new();
        for _ in 0..RANDOM_CREDENTIALS {
            let level = rng.gen_range(0..CLS);
            let mut c: Vec<String> = (0..=level).map(|i| format!("cls:{i}")).collect();
            let n_cmp = (k / 10).clamp(1, CMP);
            c.extend(cmp.weighted_without_replacement(n_cmp, &mut rng).iter().map(|i| format!("cmp:{i}")));
            let rest = k.saturating_sub(c.len());
            let n_team = (rest / 2).min(TEAM);
            let n_rel = (rest * 3 / 10).min(REL);
            let n_proj = (rest - n_team - n_rel).min(PROJ);
            c.extend(team.weighted_without_replacement(n_team, &mut rng).iter().map(|i| format!("team:{i}")));
            c.extend(rel.weighted_without_replacement(n_rel, &mut rng).iter().map(|i| format!("rel:{i}")));
            c.extend(proj.weighted_without_replacement(n_proj, &mut rng).iter().map(|i| format!("proj:{i}")));
            // Past a category's size the remainder goes to whichever still has room.
            let mut fill = (0..TEAM).map(|i| format!("team:{i}"))
                .chain((0..REL).map(|i| format!("rel:{i}")))
                .chain((0..PROJ).map(|i| format!("proj:{i}")))
                .chain((0..CMP).map(|i| format!("cmp:{i}")))
                .chain((0..CLS).map(|i| format!("cls:{i}")));
            let mut have: FxHashSet<String> = c.iter().cloned().collect();
            while c.len() < k {
                match fill.next() {
                    Some(x) if have.insert(x.clone()) => c.push(x),
                    Some(_) => {}
                    None => break,
                }
            }
            creds.push(c);
        }
        credentials.push((k, creds));
    }
    Corpus {
        name: format!("A{}k", n_labels / 1000),
        labels,
        items_per_entry,
        credentials,
        baseline: false,
    }
}

const USERS: usize = 1_000_000;
const GROUPS: usize = 50_000;

fn corpus_b(items: usize, seed: u64) -> Corpus {
    let mut rng = StdRng::seed_from_u64(seed);
    let users = Zipf::new(USERS, 0.9);
    let groups = Zipf::new(GROUPS, 1.0);
    let mut labels: Vec<String> = Vec::with_capacity(items);
    for i in 0..items {
        if i > 0 && rng.gen_bool(0.05) {
            let j = rng.gen_range(0..i);
            let again = labels[j].clone();
            labels.push(again);
            continue;
        }
        let k = rng.gen_range(2..=8);
        let mut ps: Vec<String> = Vec::with_capacity(k);
        while ps.len() < k {
            let p = if rng.gen_bool(0.7) {
                format!("user:{}", users.sample(&mut rng))
            } else {
                format!("group:{}", groups.sample(&mut rng))
            };
            if !ps.contains(&p) {
                ps.push(p);
            }
        }
        labels.push(ps.join("|"));
    }
    let mut credentials = Vec::new();
    for &k in &SIZES {
        let mut creds = Vec::new();
        for _ in 0..RANDOM_CREDENTIALS {
            let mut c = vec![format!("user:{}", users.sample(&mut rng))];
            c.extend(
                groups
                    .weighted_without_replacement(k - 1, &mut rng)
                    .iter()
                    .map(|g| format!("group:{g}")),
            );
            creds.push(c);
        }
        credentials.push((k, creds));
    }
    Corpus {
        name: "B".into(),
        labels,
        items_per_entry: vec![1; items],
        credentials,
        baseline: true,
    }
}

fn corpus_c(n_labels: usize, items: u64, seed: u64) -> Corpus {
    let mut rng = StdRng::seed_from_u64(seed);
    let labels: Vec<String> = (0..n_labels).map(|i| format!("t:{i}")).collect();
    let items_per_entry = zipf_counts(n_labels, items, 1.0, &mut rng);
    // Credentials weighted by how many items carry each term, so they hit.
    let pop = {
        let mut acc = 0.0;
        Zipf {
            cdf: items_per_entry
                .iter()
                .map(|&c| {
                    acc += c as f64;
                    acc
                })
                .collect(),
        }
    };
    let mut credentials = Vec::new();
    for &k in &SIZES {
        let creds = (0..RANDOM_CREDENTIALS)
            .map(|_| pop.weighted_without_replacement(k, &mut rng).iter().map(|&i| format!("t:{i}")).collect())
            .collect();
        credentials.push((k, creds));
    }
    Corpus { name: "C".into(), labels, items_per_entry, credentials, baseline: true }
}

fn rss_bytes() -> u64 {
    status_bytes("VmRSS:")
}

fn status_bytes(key: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn pct(v: &mut [u64], p: f64) -> u64 {
    v.sort_unstable();
    let i = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[i.clamp(1, v.len()) - 1]
}

fn med(v: &mut [u64]) -> u64 {
    pct(v, 50.0)
}

fn us(ns: u64) -> String {
    let u = ns as f64 / 1000.0;
    if u >= 100.0 {
        format!("{u:.0}")
    } else if u >= 10.0 {
        format!("{u:.1}")
    } else {
        format!("{u:.2}")
    }
}

struct Timed {
    bitmap: Bitmap,
    stats: PassStats,
    prop_ns: u64,
    union_ns: u64,
}

fn pass(dag: &Dag, up: &Up, watched: bool, s: &mut Scratch, cred: &[Vec<u8>], out: &mut Vec<u32>) -> PassStats {
    let leaves: Vec<u32> = cred.iter().filter_map(|c| dag.leaf_of(c)).collect();
    out.clear();
    if watched {
        dag.propagate_watched(up, s, &leaves, out)
    } else {
        dag.propagate(up, s, &leaves, out)
    }
}

/// The design as written: propagate to label roots, then union each true label's run of items.
#[allow(clippy::too_many_arguments)]
fn authorise_labels(
    dag: &Dag,
    up: &Up,
    watched: bool,
    s: &mut Scratch,
    starts: &[u32],
    cred: &[Vec<u8>],
    labels: &mut Vec<u32>,
    singles: &mut Vec<u32>,
) -> Timed {
    let t0 = Instant::now();
    let stats = pass(dag, up, watched, s, cred, labels);
    let t1 = Instant::now();
    // Label ids in order, so that adjacent runs coalesce. A Roaring bitmap orders them faster
    // than sorting the vector did.
    let order = Bitmap::of(labels);
    let mut bitmap = Bitmap::new();
    singles.clear();
    let (mut rs, mut re) = (0u32, 0u32);
    let flush = |rs: u32, re: u32, bm: &mut Bitmap, singles: &mut Vec<u32>| {
        if re - rs >= 32 {
            bm.add_range(rs..re);
        } else {
            singles.extend(rs..re);
        }
    };
    for l in order.iter() {
        let (s, e) = (starts[l as usize], starts[l as usize + 1]);
        if s == re {
            re = e;
        } else {
            flush(rs, re, &mut bitmap, singles);
            rs = s;
            re = e;
        }
    }
    flush(rs, re, &mut bitmap, singles);
    bitmap.add_many(singles);
    let t2 = Instant::now();
    Timed { bitmap, stats, prop_ns: (t1 - t0).as_nanos() as u64, union_ns: (t2 - t1).as_nanos() as u64 }
}

/// The variant: each item is posted under every top-level disjunct (clause) of its label, and a
/// pass propagates only as far as the clauses.
struct Clauses {
    up: Up,
    watch: Up,
    postings: Vec<Bitmap>,
    entries: u64,
    bytes: usize,
}

fn clauses(dag: &Dag, starts: &[u32]) -> Clauses {
    let n = dag.nodes();
    let mut idx = vec![NONE; n];
    let mut postings: Vec<Bitmap> = Vec::new();
    let mut entries = 0u64;
    for (l, &root) in dag.label_root.iter().enumerate() {
        let cs: &[u32] = if dag.kind[root as usize] == OR {
            dag.children(root)
        } else {
            std::slice::from_ref(&dag.label_root[l])
        };
        for &c in cs {
            if idx[c as usize] == NONE {
                idx[c as usize] = postings.len() as u32;
                postings.push(Bitmap::new());
            }
            postings[idx[c as usize] as usize].add_range(starts[l]..starts[l + 1]);
            entries += (starts[l + 1] - starts[l]) as u64;
        }
    }
    let mut needed = vec![false; n];
    let mut stack: Vec<u32> = (0..n as u32).filter(|&c| idx[c as usize] != NONE).collect();
    for &c in &stack {
        needed[c as usize] = true;
    }
    while let Some(x) = stack.pop() {
        for &ch in dag.children(x) {
            if !needed[ch as usize] {
                needed[ch as usize] = true;
                stack.push(ch);
            }
        }
    }
    let up = Up::new(&dag.child_off, &dag.edges, &|p, _| needed[p], idx.clone());
    let watch = dag.watch(&|p| needed[p], idx);
    for b in postings.iter_mut() {
        b.run_optimize();
    }
    let bytes = up.bytes()
        + postings.iter().map(|b| b.get_serialized_size_in_bytes::<croaring::Portable>()).sum::<usize>();
    Clauses { up, watch, postings, entries, bytes }
}

fn authorise_clauses(
    dag: &Dag,
    cl: &Clauses,
    watched: bool,
    s: &mut Scratch,
    cred: &[Vec<u8>],
    hits: &mut Vec<u32>,
) -> Timed {
    let t0 = Instant::now();
    let up = if watched { &cl.watch } else { &cl.up };
    let stats = pass(dag, up, watched, s, cred, hits);
    let t1 = Instant::now();
    let refs: Vec<&Bitmap> = hits.iter().map(|&i| &cl.postings[i as usize]).collect();
    let bitmap = Bitmap::fast_or(&refs);
    let t2 = Instant::now();
    Timed { bitmap, stats, prop_ns: (t1 - t0).as_nanos() as u64, union_ns: (t2 - t1).as_nanos() as u64 }
}

/// Today's scheme: one posting per term, and the union of the held terms' postings. All of it
/// is reported as union.
fn baseline(dag: &Dag, postings: &[Bitmap], cred: &[Vec<u8>]) -> Timed {
    let t0 = Instant::now();
    let refs: Vec<&Bitmap> = cred
        .iter()
        .filter_map(|c| dag.term_ids.get(c.as_slice()))
        .map(|&t| &postings[t as usize])
        .collect();
    let bitmap = Bitmap::fast_or(&refs);
    let stats = PassStats { labels_true: refs.len() as u64, ..Default::default() };
    Timed { bitmap, stats, prop_ns: 0, union_ns: t0.elapsed().as_nanos() as u64 }
}

#[derive(Default)]
struct Samples(Vec<u64>);

impl Samples {
    fn push(&mut self, x: u64) {
        self.0.push(x)
    }
    fn med(&self) -> u64 {
        med(&mut self.0.clone())
    }
    fn mp(&self) -> String {
        let mut v = self.0.clone();
        format!("{} / {}", us(med(&mut v)), us(pct(&mut v, 99.0)))
    }
}

fn run(c: Corpus) {
    println!("\n## Corpus {}\n", c.name);
    let n_items: u64 = c.items_per_entry.iter().map(|&x| x as u64).sum();
    let bytes: Vec<&[u8]> = c.labels.iter().map(|s| s.as_bytes()).collect();

    // Build: parse, normalise and intern every label, then freeze.
    let rss0 = rss_bytes();
    let t0 = Instant::now();
    let mut b = Builder::new();
    let mut entry_label = Vec::with_capacity(bytes.len());
    for l in &bytes {
        entry_label.push(b.add(l).expect("generated labels parse"));
    }
    let t_add = t0.elapsed();
    let rss_building = rss_bytes();
    let cons = b.cons_bytes();
    let t1 = Instant::now();
    let dag = b.freeze();
    let t_freeze = t1.elapsed();

    let n_labels = dag.labels();
    let n_nodes = dag.nodes();
    let n_leaves = dag.kind.iter().filter(|&&k| k == LEAF).count();
    let graph = dag.graph_bytes();
    let dict = dag.dict_bytes();
    let postings_bytes = 4 * (n_labels + 1);

    // Postings: items sorted by label id, so label l is items starts[l]..starts[l+1].
    let mut per_label = vec![0u64; n_labels];
    for (e, &l) in entry_label.iter().enumerate() {
        per_label[l as usize] += c.items_per_entry[e] as u64;
    }
    drop(entry_label);
    let mut starts = Vec::with_capacity(n_labels + 1);
    let mut acc = 0u64;
    starts.push(0u32);
    for &x in &per_label {
        acc += x;
        starts.push(acc as u32);
    }

    // Nodes per label, and how many labels and items mention each leaf.
    let mut stamp = vec![u32::MAX; n_nodes];
    let mut mention = vec![(0u32, 0u64); n_nodes];
    let mut sizes: Vec<u64> = Vec::with_capacity(n_labels);
    let mut stack = Vec::new();
    for (l, &root) in dag.label_root.iter().enumerate() {
        let mut count = 0u64;
        stack.clear();
        stack.push(root);
        stamp[root as usize] = l as u32;
        while let Some(n) = stack.pop() {
            count += 1;
            if dag.kind[n as usize] == LEAF {
                mention[n as usize].0 += 1;
                mention[n as usize].1 += per_label[l];
            }
            for &ch in dag.children(n) {
                if stamp[ch as usize] != l as u32 {
                    stamp[ch as usize] = l as u32;
                    stack.push(ch);
                }
            }
        }
        sizes.push(count);
    }
    drop(stamp);

    println!(
        "labels written {}, distinct labels {}, items {}, terms {}, nodes {} ({} leaves, {} inner), edges {}",
        bytes.len(), n_labels, n_items, dag.term_ids.len(), n_nodes, n_leaves, n_nodes - n_leaves, dag.edges.len()
    );
    println!(
        "build: add {:.2} s ({:.0} ns per label written), freeze {:.3} s",
        t_add.as_secs_f64(),
        t_add.as_nanos() as f64 / bytes.len() as f64,
        t_freeze.as_secs_f64()
    );
    println!(
        "memory (accounted): graph {} B ({:.1} B/node, {:.1} B/label); term dictionary {} B; label postings offsets {} B; hash-cons index {} B ({:.1} B/node); scratch per worker {} B",
        graph, graph as f64 / n_nodes as f64, graph as f64 / n_labels as f64, dict, postings_bytes, cons,
        cons as f64 / n_nodes as f64, 12 * n_nodes
    );
    println!(
        "memory (RSS): +{:.1} MB after adding every label, before freezing",
        (rss_building.saturating_sub(rss0)) as f64 / 1e6,
    );
    {
        let mut s = sizes.clone();
        let mut hist = [0u64; 9];
        for &x in &s {
            let b = match x {
                1 => 0,
                2..=3 => 1,
                4..=7 => 2,
                8..=15 => 3,
                16..=31 => 4,
                32..=63 => 5,
                64..=127 => 6,
                128..=255 => 7,
                _ => 8,
            };
            hist[b] += 1;
        }
        println!(
            "nodes per label: p50 {} p90 {} p99 {} p99.9 {} max {}; histogram 1:{} 2-3:{} 4-7:{} 8-15:{} 16-31:{} 32-63:{} 64-127:{} 128-255:{} 256+:{}",
            pct(&mut s, 50.0), pct(&mut s, 90.0), pct(&mut s, 99.0), pct(&mut s, 99.9), s.last().unwrap(),
            hist[0], hist[1], hist[2], hist[3], hist[4], hist[5], hist[6], hist[7], hist[8]
        );
    }

    let t = Instant::now();
    let cl = clauses(&dag, &starts);
    println!(
        "clause variant: {} clause postings, {} entries ({:.2} per item), {} B (postings serialised, plus upward edges), built in {:.2} s; upward edges {} of {}",
        cl.postings.len(), cl.entries, cl.entries as f64 / n_items as f64, cl.bytes, t.elapsed().as_secs_f64(),
        cl.up.list.len(), dag.up.list.len()
    );

    let postings: Vec<Bitmap> = if c.baseline {
        let t = Instant::now();
        let mut leaf_term = vec![u32::MAX; n_nodes];
        for (term, &leaf) in dag.term_leaf.iter().enumerate() {
            if leaf != NONE {
                leaf_term[leaf as usize] = term as u32;
            }
        }
        let mut out: Vec<Bitmap> = (0..dag.term_leaf.len()).map(|_| Bitmap::new()).collect();
        for (l, &root) in dag.label_root.iter().enumerate() {
            let leaves: &[u32] = if dag.kind[root as usize] == LEAF {
                std::slice::from_ref(&dag.label_root[l])
            } else {
                dag.children(root)
            };
            for &leaf in leaves {
                assert_eq!(dag.kind[leaf as usize], LEAF, "the baseline needs flat disjunctions");
                out[leaf_term[leaf as usize] as usize].add_range(starts[l]..starts[l + 1]);
            }
        }
        for b in out.iter_mut() {
            b.run_optimize();
        }
        let bytes: usize = out.iter().map(|b| b.get_serialized_size_in_bytes::<croaring::Portable>()).sum();
        println!("baseline postings: {} terms, {} B serialised, built in {:.2} s", out.len(), bytes, t.elapsed().as_secs_f64());
        out
    } else {
        Vec::new()
    };
    println!(
        "memory (RSS): {:.1} MB in total with every structure built; peak so far {:.1} MB",
        rss_bytes() as f64 / 1e6,
        status_bytes("VmHWM:") as f64 / 1e6
    );

    // The worst credential of each size: the terms the most labels mention, then the most items.
    let mut by_mention: Vec<((u32, u64), u32)> = dag
        .term_leaf
        .iter()
        .enumerate()
        .filter(|(_, &l)| l != NONE)
        .map(|(t, &l)| (mention[l as usize], t as u32))
        .collect();
    by_mention.sort_unstable_by(|a, b| b.cmp(a));
    let mut term_name = vec![Vec::new(); dag.term_leaf.len()];
    for (k, &v) in dag.term_ids.iter() {
        term_name[v as usize] = k.to_vec();
    }

    let mut scratch = dag.scratch();
    let label_watch = dag.watch(&|_| true, dag.up.hit.clone());
    let mut buf = Vec::new();
    let mut singles = Vec::new();
    let designs: &[&str] = if c.baseline { &["L", "LW", "C", "CW", "T"] } else { &["L", "LW", "C", "CW"] };

    println!(
        "\n| corpus | credential | terms | design | propagate µs med / p99 | union µs med / p99 | total µs med / p99 | nodes visited | hits | authorised items |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (k, creds) in &c.credentials {
        let worst: Vec<Vec<u8>> = by_mention.iter().take(*k).map(|&(_, t)| term_name[t as usize].clone()).collect();
        let drawn: Vec<Vec<Vec<u8>>> =
            creds.iter().map(|c| c.iter().map(|s| s.as_bytes().to_vec()).collect()).collect();
        for (kind, set, reps) in [("drawn", drawn, REPS_PER_RANDOM), ("worst", vec![worst], REPS_WORST)] {
            let mut samples: Vec<[Samples; 6]> = designs.iter().map(|_| Default::default()).collect();
            let mut one = |d: &str, cred: &[Vec<u8>], buf: &mut Vec<u32>| -> Timed {
                match d {
                    "L" => authorise_labels(&dag, &dag.up, false, &mut scratch, &starts, cred, buf, &mut singles),
                    "LW" => authorise_labels(&dag, &label_watch, true, &mut scratch, &starts, cred, buf, &mut singles),
                    "C" => authorise_clauses(&dag, &cl, false, &mut scratch, cred, buf),
                    "CW" => authorise_clauses(&dag, &cl, true, &mut scratch, cred, buf),
                    _ => baseline(&dag, &postings, cred),
                }
            };
            for (ci, cred) in set.iter().enumerate() {
                // One untimed pass of each warms the caches and checks the answers agree.
                let first = one("L", cred, &mut buf);
                if ci < 3 {
                    let held: FxHashSet<u32> = cred.iter().filter_map(|x| dag.leaf_of(x)).collect();
                    let expect: Vec<u32> = (0..n_labels as u32)
                        .filter(|&l| dag.eval(dag.label_root[l as usize], &|n| held.contains(&n)))
                        .collect();
                    let mut got = buf.clone();
                    got.sort_unstable();
                    assert_eq!(got, expect, "propagation disagrees with top-down evaluation");
                    let sum: u64 = expect.iter().map(|&l| per_label[l as usize]).sum();
                    assert_eq!(first.bitmap.cardinality(), sum);
                }
                for d in &designs[1..] {
                    assert!(one(d, cred, &mut buf).bitmap == first.bitmap, "design {d} disagrees");
                }
                for _ in 0..reps {
                    for (di, d) in designs.iter().enumerate() {
                        let r = one(d, cred, &mut buf);
                        let s = &mut samples[di];
                        s[0].push(r.prop_ns);
                        s[1].push(r.union_ns);
                        s[2].push(r.prop_ns + r.union_ns);
                        s[3].push(r.stats.nodes_visited);
                        s[4].push(r.stats.labels_true);
                        s[5].push(r.bitmap.cardinality());
                    }
                }
            }
            for (di, d) in designs.iter().enumerate() {
                let s = &samples[di];
                let prop = if *d == "T" { "n/a".to_string() } else { s[0].mp() };
                println!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    c.name, kind, k, d, prop, s[1].mp(), s[2].mp(), s[3].med(), s[4].med(), s[5].med()
                );
            }
        }
    }
}

/// The cost of writing one long label, by shape and size, into an empty DAG and into one that
/// already holds `base`.
fn limit_bench() {
    println!("\n## Cost of one label by size\n");
    println!("| shape | operands | nodes | bytes | add µs med / max (empty DAG) | add µs med / max (DAG of 100k labels) | evaluate µs med |");
    println!("|---|---|---|---|---|---|---|");
    let base = corpus_a(100_000, 100_000, 9);
    let mut rng = StdRng::seed_from_u64(10);
    for m in [16usize, 64, 256, 1024, 4096, 16384] {
        let wide: Vec<String> = (0..m).map(|i| format!("rel:{i}")).collect();
        let pairs: Vec<String> = (0..m).map(|i| format!("(cls:1&cmp:2&team:{i})")).collect();
        let mut shuffled = pairs.clone();
        shuffled.shuffle(&mut rng);
        for (shape, text) in [("OR of terms", wide.join("|")), ("OR of 3-term ANDs sharing two terms", shuffled.join("|"))] {
            let bytes = text.len();
            let (mut empty, mut full, mut eval) = (Samples::default(), Samples::default(), Samples::default());
            let mut nodes = 0;
            for _ in 0..9 {
                let mut b = Builder::new();
                let t = Instant::now();
                b.add(text.as_bytes()).unwrap();
                empty.push(t.elapsed().as_nanos() as u64);
                let dag = b.freeze();
                nodes = dag.nodes();
                let root = dag.label_root[0];
                let t = Instant::now();
                let r = dag.eval(root, &|n| n % 7 == 0);
                eval.push(t.elapsed().as_nanos() as u64);
                std::hint::black_box(r);
            }
            let mut b = Builder::new();
            for l in &base.labels {
                b.add(l.as_bytes()).unwrap();
            }
            for _ in 0..9 {
                // A fresh text each time, so the label is new to the DAG.
                let text = format!("{text}|rel:x{}", rng.gen::<u32>());
                let t = Instant::now();
                b.add(text.as_bytes()).unwrap();
                full.push(t.elapsed().as_nanos() as u64);
            }
            let max = |s: &Samples| us(*s.0.iter().max().unwrap());
            println!(
                "| {shape} | {m} | {nodes} | {bytes} | {} / {} | {} / {} | {} |",
                us(empty.med()), max(&empty), us(full.med()), max(&full), us(eval.med())
            );
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let want = |n: &str| args.is_empty() || args.iter().any(|a| a == n);
    if args.iter().any(|a| a == "limit") {
        limit_bench();
        return;
    }
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let items_a = env("LABELDAG_ITEMS_A", 100_000_000);
    let items_b = env("LABELDAG_ITEMS_B", 10_000_000) as usize;
    let items_c = env("LABELDAG_ITEMS_C", 100_000_000);
    println!(
        "samples: drawn = {RANDOM_CREDENTIALS} credentials x {REPS_PER_RANDOM} passes; worst = 1 credential x {REPS_WORST} passes; single thread"
    );
    if want("A100k") {
        let t = Instant::now();
        let c = corpus_a(100_000, items_a, 1);
        eprintln!("generated A100k in {:.1} s", t.elapsed().as_secs_f64());
        run(c);
    }
    if want("A500k") {
        let c = corpus_a(500_000, items_a, 2);
        run(c);
    }
    if want("B") {
        let t = Instant::now();
        let c = corpus_b(items_b, 3);
        eprintln!("generated B in {:.1} s", t.elapsed().as_secs_f64());
        run(c);
    }
    if want("C") {
        let c = corpus_c(100_000, items_c, 4);
        run(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_subexpressions_are_one_node() {
        let mut b = Builder::new();
        let l1 = b.add(b"secret&(team_a|team_b)").unwrap();
        let l2 = b.add(b"(team_b|team_a)&secret").unwrap();
        b.add(b"team_a&eu").unwrap();
        let l4 = b.add(b"team_a").unwrap();
        let l5 = b.add(b"team_a|(team_a&eu)").unwrap();
        assert_eq!(l1, l2);
        assert_eq!(l4, l5);
        let dag = b.freeze();
        assert_eq!(dag.labels(), 3);
        // Leaves secret, team_a, team_b, eu; inner OR, AND, AND.
        assert_eq!(dag.nodes(), 7);
    }

    #[test]
    fn propagation_matches_evaluation() {
        let mut rng = StdRng::seed_from_u64(11);
        let c = corpus_a(2_000, 10_000, 5);
        let mut b = Builder::new();
        for l in &c.labels {
            b.add(l.as_bytes()).unwrap();
        }
        let dag = b.freeze();
        let mut s = dag.scratch();
        let watch = dag.watch(&|_| true, dag.up.hit.clone());
        let mut out = Vec::new();
        let leaves: Vec<u32> = (0..dag.nodes() as u32).filter(|&n| dag.kind[n as usize] == LEAF).collect();
        for _ in 0..200 {
            let k = rng.gen_range(0..leaves.len());
            let held: FxHashSet<u32> = leaves.choose_multiple(&mut rng, k).copied().collect();
            let hv: Vec<u32> = held.iter().copied().collect();
            out.clear();
            dag.propagate(&dag.up, &mut s, &hv, &mut out);
            let mut w = Vec::new();
            dag.propagate_watched(&watch, &mut s, &hv, &mut w);
            w.sort_unstable();
            out.sort_unstable();
            let expect: Vec<u32> = (0..dag.labels() as u32)
                .filter(|&l| dag.eval(dag.label_root[l as usize], &|n| held.contains(&n)))
                .collect();
            assert_eq!(out, expect);
            assert_eq!(w, expect);
        }
    }
}
