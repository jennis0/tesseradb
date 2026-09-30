//! The hash-consed label DAG and the bottom-up authorise pass.

use crate::expr::{normalise, parse, Expr, ParseError};
use rustc_hash::{FxHashMap, FxHasher};
use std::hash::Hasher;

pub const LEAF: u8 = 0;
pub const AND: u8 = 1;
pub const OR: u8 = 2;
pub const NONE: u32 = u32::MAX;

/// Accepts labels one at a time and interns each into the shared DAG. Nodes are appended, so the
/// children arena is already in CSR order.
#[derive(Default)]
pub struct Builder {
    term_ids: FxHashMap<Box<[u8]>, u32>,
    term_leaf: Vec<u32>,
    kind: Vec<u8>,
    child_off: Vec<u32>,
    edges: Vec<u32>,
    /// Hash of (kind, children) to the newest node with it; older ones chain through `chain`.
    cons: FxHashMap<u64, u32>,
    chain: Vec<u32>,
    root_label: Vec<u32>,
    label_root: Vec<u32>,
}

impl Builder {
    pub fn new() -> Self {
        Self { child_off: vec![0], ..Default::default() }
    }

    fn term(&mut self, t: &[u8]) -> u32 {
        if let Some(&id) = self.term_ids.get(t) {
            return id;
        }
        let id = self.term_leaf.len() as u32;
        self.term_ids.insert(t.into(), id);
        self.term_leaf.push(NONE);
        id
    }

    fn push_node(&mut self, kind: u8, children: &[u32], hash: u64) -> u32 {
        let id = self.kind.len() as u32;
        self.kind.push(kind);
        self.edges.extend_from_slice(children);
        self.child_off.push(self.edges.len() as u32);
        self.root_label.push(NONE);
        let prev = self.cons.insert(hash, id);
        self.chain.push(prev.unwrap_or(NONE));
        id
    }

    fn leaf(&mut self, t: u32) -> u32 {
        let l = self.term_leaf[t as usize];
        if l != NONE {
            return l;
        }
        let id = self.kind.len() as u32;
        self.kind.push(LEAF);
        self.child_off.push(self.edges.len() as u32);
        self.root_label.push(NONE);
        self.chain.push(NONE);
        self.term_leaf[t as usize] = id;
        id
    }

    fn intern(&mut self, e: &Expr) -> u32 {
        let (kind, v) = match e {
            Expr::Term(t) => return self.leaf(*t),
            Expr::And(v) => (AND, v),
            Expr::Or(v) => (OR, v),
        };
        let mut ids: Vec<u32> = v.iter().map(|c| self.intern(c)).collect();
        ids.sort_unstable();
        let mut h = FxHasher::default();
        h.write_u8(kind);
        for &i in &ids {
            h.write_u32(i);
        }
        let hash = h.finish();
        let mut cand = self.cons.get(&hash).copied().unwrap_or(NONE);
        while cand != NONE {
            let c = cand as usize;
            let (s, e) = (self.child_off[c] as usize, self.child_off[c + 1] as usize);
            if self.kind[c] == kind && self.edges[s..e] == ids[..] {
                return cand;
            }
            cand = self.chain[c];
        }
        self.push_node(kind, &ids, hash)
    }

    /// Parses, normalises and interns one label; returns its label id, shared with any earlier
    /// label that normalises to the same expression.
    pub fn add(&mut self, label: &[u8]) -> Result<u32, ParseError> {
        let e = {
            let mut f = |t: &[u8]| self.term(t);
            parse(label, &mut f)?
        };
        let root = self.intern(&normalise(e));
        let l = self.root_label[root as usize];
        if l != NONE {
            return Ok(l);
        }
        let l = self.label_root.len() as u32;
        self.root_label[root as usize] = l;
        self.label_root.push(root);
        Ok(l)
    }

    /// Bytes held by the hash-consing index, which ingest needs and authorise does not.
    pub fn cons_bytes(&self) -> usize {
        // hashbrown: one control byte plus the (u64, u32) slot, padded to 16, per bucket.
        self.cons.capacity() * (16 + 1) + self.chain.capacity() * 4
    }

    /// Freezes into the form authorise reads: children and parents both in CSR.
    pub fn freeze(self) -> Dag {
        let up = Up::new(&self.child_off, &self.edges, &|_, _| true, self.root_label);
        // Children are interned before their parents, so ids are already in topological order.
        let mut height = vec![0u8; self.kind.len()];
        for p in 0..self.kind.len() {
            let (s, e) = (self.child_off[p] as usize, self.child_off[p + 1] as usize);
            if let Some(h) = self.edges[s..e].iter().map(|&c| height[c as usize]).max() {
                height[p] = h + 1;
            }
        }
        Dag {
            height,
            term_ids: self.term_ids,
            term_leaf: self.term_leaf,
            kind: self.kind,
            child_off: self.child_off,
            edges: self.edges,
            up,
            label_root: self.label_root,
        }
    }
}

/// The upward edges a pass follows, and what a node that becomes true reports (`NONE` for
/// nothing): a label id, or a clause's posting.
pub struct Up {
    pub off: Vec<u32>,
    pub list: Vec<u32>,
    pub hit: Vec<u32>,
}

impl Up {
    /// Parents from the children CSR, keeping only the edges (parent, child) `keep` accepts.
    pub fn new(child_off: &[u32], edges: &[u32], keep: &dyn Fn(usize, u32) -> bool, hit: Vec<u32>) -> Up {
        let n = child_off.len() - 1;
        let mut off = vec![0u32; n + 1];
        for p in 0..n {
            for &c in &edges[child_off[p] as usize..child_off[p + 1] as usize] {
                if keep(p, c) {
                    off[c as usize + 1] += 1;
                }
            }
        }
        for i in 0..n {
            off[i + 1] += off[i];
        }
        let mut fill = off.clone();
        let mut list = vec![0u32; off[n] as usize];
        for p in 0..n {
            for &c in &edges[child_off[p] as usize..child_off[p + 1] as usize] {
                if keep(p, c) {
                    list[fill[c as usize] as usize] = p as u32;
                    fill[c as usize] += 1;
                }
            }
        }
        Up { off, list, hit }
    }

    pub fn bytes(&self) -> usize {
        4 * (self.off.len() + self.list.len() + self.hit.len())
    }
}

pub struct Dag {
    /// 0 for a leaf, otherwise one more than the highest child.
    pub height: Vec<u8>,
    pub term_ids: FxHashMap<Box<[u8]>, u32>,
    pub term_leaf: Vec<u32>,
    pub kind: Vec<u8>,
    pub child_off: Vec<u32>,
    pub edges: Vec<u32>,
    /// Every parent edge; a node reports its label id if it is a label's root.
    pub up: Up,
    pub label_root: Vec<u32>,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PassStats {
    pub nodes_visited: u64,
    pub nodes_true: u64,
    pub edges: u64,
    pub labels_true: u64,
}

/// Per-worker state for the pass: 8 bytes per node, reused across passes so that a pass costs
/// what it visits and never clears the whole array.
pub struct Scratch {
    epoch: u32,
    stamp: Vec<u32>,
    count: Vec<u32>,
    queue: Vec<u32>,
    truth: Vec<u32>,
    buckets: Vec<Vec<u32>>,
    pending: Vec<Vec<u32>>,
}

impl Dag {
    pub fn nodes(&self) -> usize {
        self.kind.len()
    }

    pub fn labels(&self) -> usize {
        self.label_root.len()
    }

    pub fn children(&self, n: u32) -> &[u32] {
        &self.edges[self.child_off[n as usize] as usize..self.child_off[n as usize + 1] as usize]
    }

    pub fn leaf_of(&self, term: &[u8]) -> Option<u32> {
        let t = *self.term_ids.get(term)?;
        let l = self.term_leaf[t as usize];
        (l != NONE).then_some(l)
    }

    /// Bytes of what authorise reads, excluding the term dictionary.
    pub fn graph_bytes(&self) -> usize {
        2 * self.kind.len()
            + self.up.bytes()
            + 4 * (self.child_off.len()
                + self.edges.len()
                + self.label_root.len()
                + self.term_leaf.len())
    }

    /// Bytes of the term dictionary: key bytes, box headers, and hash slots.
    pub fn dict_bytes(&self) -> usize {
        let keys: usize = self.term_ids.keys().map(|k| k.len()).sum();
        keys + self.term_ids.capacity() * (16 + 4 + 4 + 1)
    }

    pub fn scratch(&self) -> Scratch {
        let n = self.nodes();
        let h = self.height.iter().copied().max().unwrap_or(0) as usize + 1;
        Scratch {
            epoch: 0,
            stamp: vec![0; n],
            count: vec![0; n],
            queue: Vec::new(),
            truth: vec![0; n],
            buckets: vec![Vec::new(); h],
            pending: vec![Vec::new(); h],
        }
    }

    /// The upward edges for `propagate_watched`: every edge into an OR, and into an AND only
    /// from its sentinel, the child with the fewest parents. Edges into parents `keep` refuses
    /// are dropped.
    pub fn watch(&self, keep: &dyn Fn(usize) -> bool, hit: Vec<u32>) -> Up {
        let fan_in = |c: u32| self.up.off[c as usize + 1] - self.up.off[c as usize];
        let sentinel: Vec<u32> = (0..self.nodes() as u32)
            .map(|p| {
                if self.kind[p as usize] != AND {
                    return NONE;
                }
                *self.children(p).iter().min_by_key(|&&c| (fan_in(c), c)).unwrap()
            })
            .collect();
        Up::new(
            &self.child_off,
            &self.edges,
            &|p, c| keep(p) && (self.kind[p] != AND || sentinel[p] == c),
            hit,
        )
    }

    fn next_epoch(s: &mut Scratch) -> u32 {
        s.epoch = s.epoch.wrapping_add(1);
        if s.epoch == 0 {
            s.stamp.fill(0);
            s.truth.fill(0);
            s.epoch = 1;
        }
        s.epoch
    }

    /// As `propagate`, with each AND reached only through its sentinel and then checked against
    /// all its children. Nodes are settled in order of height, so every child of an AND is
    /// settled before the AND is checked.
    pub fn propagate_watched(&self, up: &Up, s: &mut Scratch, leaves: &[u32], out: &mut Vec<u32>) -> PassStats {
        let epoch = Self::next_epoch(s);
        let mut st = PassStats::default();
        for &l in leaves {
            if s.stamp[l as usize] != epoch {
                s.stamp[l as usize] = epoch;
                s.truth[l as usize] = epoch;
                s.buckets[0].push(l);
                st.nodes_visited += 1;
            }
        }
        for h in 0..s.buckets.len() {
            let mut pend = std::mem::take(&mut s.pending[h]);
            for &p in &pend {
                if self.children(p).iter().all(|&c| s.truth[c as usize] == epoch) {
                    s.truth[p as usize] = epoch;
                    s.buckets[h].push(p);
                }
            }
            pend.clear();
            s.pending[h] = pend;
            let mut b = std::mem::take(&mut s.buckets[h]);
            for &n in &b {
                st.nodes_true += 1;
                let l = up.hit[n as usize];
                if l != NONE {
                    out.push(l);
                }
                for &p in &up.list[up.off[n as usize] as usize..up.off[n as usize + 1] as usize] {
                    st.edges += 1;
                    let pu = p as usize;
                    if s.stamp[pu] == epoch {
                        continue;
                    }
                    s.stamp[pu] = epoch;
                    st.nodes_visited += 1;
                    let ph = self.height[pu] as usize;
                    if self.kind[pu] == OR {
                        s.truth[pu] = epoch;
                        s.buckets[ph].push(p);
                    } else {
                        s.pending[ph].push(p);
                    }
                }
            }
            b.clear();
            s.buckets[h] = b;
        }
        st.labels_true = out.len() as u64;
        st
    }

    /// Marks `leaves` true and propagates along `up`. Appends `up.hit` of every node that
    /// becomes true, where it is not `NONE`.
    pub fn propagate(&self, up: &Up, s: &mut Scratch, leaves: &[u32], out: &mut Vec<u32>) -> PassStats {
        let epoch = Self::next_epoch(s);
        let mut st = PassStats::default();
        s.queue.clear();
        for &l in leaves {
            if s.stamp[l as usize] != epoch {
                s.stamp[l as usize] = epoch;
                s.queue.push(l);
            }
        }
        st.nodes_visited = s.queue.len() as u64;
        let mut i = 0;
        while i < s.queue.len() {
            let n = s.queue[i];
            i += 1;
            let l = up.hit[n as usize];
            if l != NONE {
                out.push(l);
            }
            for &p in &up.list[up.off[n as usize] as usize..up.off[n as usize + 1] as usize] {
                st.edges += 1;
                let pu = p as usize;
                if s.stamp[pu] != epoch {
                    s.stamp[pu] = epoch;
                    s.count[pu] = 0;
                    st.nodes_visited += 1;
                }
                s.count[pu] += 1;
                let need = if self.kind[pu] == AND {
                    self.child_off[pu + 1] - self.child_off[pu]
                } else {
                    1
                };
                if s.count[pu] == need {
                    s.queue.push(p);
                }
            }
        }
        st.nodes_true = s.queue.len() as u64;
        st.labels_true = out.len() as u64;
        st
    }

    /// Top-down evaluation of one node, used only to check `propagate`.
    pub fn eval(&self, n: u32, held: &dyn Fn(u32) -> bool) -> bool {
        match self.kind[n as usize] {
            LEAF => held(n),
            AND => self.children(n).iter().all(|&c| self.eval(c, held)),
            _ => self.children(n).iter().any(|&c| self.eval(c, held)),
        }
    }
}
