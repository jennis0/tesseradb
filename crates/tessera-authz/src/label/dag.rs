//! The shared expression DAG over compound labels.
//!
//! A leaf is a term id and an inner node is an AND or an OR over its children. Nodes are
//! hash-consed on their kind and their sorted child ids, so a subexpression that several labels
//! share is one node. Children are interned before their parent and nodes are only appended, so
//! labels can be added while the service runs. Each node keeps a list of its parents, which the
//! bottom-up pass follows. The lists are threaded through the edge arrays, so adding a node
//! allocates nothing per node beyond its edges.

use std::hash::Hasher;

use rustc_hash::{FxHashMap, FxHasher};
use tessera_types::{LabelId, TermId};

use super::Expr;

const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    Leaf(TermId),
    And,
    Or,
}

pub(super) struct Dag {
    node: Vec<Node>,
    /// Node `n`'s children are `child[child_off[n]..child_off[n + 1]]`.
    child_off: Vec<u32>,
    child: Vec<u32>,
    /// Per edge: the parent it leads to, and the next edge from the same child.
    edge_parent: Vec<u32>,
    next_parent: Vec<u32>,
    /// Per node: its first edge upwards, or `NONE`.
    first_parent: Vec<u32>,
    /// Per node: the label whose root it is, or `NONE`.
    root_label: Vec<u32>,
    leaf: FxHashMap<TermId, u32>,
    /// A hash of (kind, children) to the newest inner node with it. Older nodes with the same hash
    /// follow through `chain`.
    cons: FxHashMap<u64, u32>,
    chain: Vec<u32>,
}

/// Per-caller state for [`super::Labels::authorise`], reused across passes so that a pass costs
/// what it visits and never clears the whole array. It grows with the DAG.
#[derive(Debug, Default)]
pub struct Scratch {
    epoch: u32,
    stamp: Vec<u32>,
    count: Vec<u32>,
    queue: Vec<u32>,
}

impl Scratch {
    fn begin(&mut self, nodes: usize) {
        if self.stamp.len() < nodes {
            self.stamp.resize(nodes, 0);
            self.count.resize(nodes, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.fill(0);
            self.epoch = 1;
        }
        self.queue.clear();
    }

    /// Records that one more child of `n` is true, and whether `n` has just become true.
    fn arrive(&mut self, n: u32, needed: u32) -> bool {
        let n = n as usize;
        if self.stamp[n] != self.epoch {
            self.stamp[n] = self.epoch;
            self.count[n] = 0;
        }
        self.count[n] += 1;
        self.count[n] == needed
    }
}

fn index(n: usize) -> u32 {
    u32::try_from(n)
        .ok()
        .filter(|&n| n != NONE)
        .expect("the label DAG holds fewer than 2^32 - 1 nodes and edges")
}

impl Dag {
    pub(super) fn new() -> Self {
        Dag {
            node: Vec::new(),
            child_off: vec![0],
            child: Vec::new(),
            edge_parent: Vec::new(),
            next_parent: Vec::new(),
            first_parent: Vec::new(),
            root_label: Vec::new(),
            leaf: FxHashMap::default(),
            cons: FxHashMap::default(),
            chain: Vec::new(),
        }
    }

    fn children(&self, n: u32) -> &[u32] {
        let n = n as usize;
        &self.child[self.child_off[n] as usize..self.child_off[n + 1] as usize]
    }

    /// The children of `n` that must be true for `n` to be true.
    fn needed(&self, n: u32) -> u32 {
        match self.node[n as usize] {
            Node::And => self.children(n).len() as u32,
            Node::Leaf(_) | Node::Or => 1,
        }
    }

    /// Interns a normalised expression, calling `term` for the id of each term, and returns its
    /// root.
    pub(super) fn intern(&mut self, e: &Expr, term: &mut impl FnMut(&str) -> TermId) -> u32 {
        let (kind, operands) = match e {
            Expr::Term(t) => return self.leaf(term(t)),
            Expr::And(v) => (Node::And, v),
            Expr::Or(v) => (Node::Or, v),
        };
        let mut children: Vec<u32> = operands.iter().map(|c| self.intern(c, term)).collect();
        children.sort_unstable();
        let mut h = FxHasher::default();
        h.write_u8(u8::from(kind == Node::And));
        children.iter().for_each(|&c| h.write_u32(c));
        let hash = h.finish();
        match self.find(kind, &children, hash) {
            Some(n) => n,
            None => self.push(kind, &children, Some(hash)),
        }
    }

    fn find(&self, kind: Node, children: &[u32], hash: u64) -> Option<u32> {
        let mut candidate = self.cons.get(&hash).copied().unwrap_or(NONE);
        while candidate != NONE {
            let c = candidate as usize;
            if self.node[c] == kind && self.children(candidate) == children {
                return Some(candidate);
            }
            candidate = self.chain[c];
        }
        None
    }

    fn leaf(&mut self, term: TermId) -> u32 {
        match self.leaf.get(&term) {
            Some(&n) => n,
            None => {
                let n = self.push(Node::Leaf(term), &[], None);
                self.leaf.insert(term, n);
                n
            }
        }
    }

    fn push(&mut self, kind: Node, children: &[u32], hash: Option<u64>) -> u32 {
        let id = index(self.node.len());
        self.node.push(kind);
        for &c in children {
            let edge = index(self.child.len());
            self.child.push(c);
            self.edge_parent.push(id);
            self.next_parent.push(self.first_parent[c as usize]);
            self.first_parent[c as usize] = edge;
        }
        self.child_off.push(index(self.child.len()));
        self.first_parent.push(NONE);
        self.root_label.push(NONE);
        let older = hash.and_then(|h| self.cons.insert(h, id));
        self.chain.push(older.unwrap_or(NONE));
        id
    }

    pub(super) fn root_label(&self, n: u32) -> Option<LabelId> {
        let label = self.root_label[n as usize];
        (label != NONE).then_some(LabelId::new(label))
    }

    pub(super) fn set_root_label(&mut self, n: u32, label: LabelId) {
        self.root_label[n as usize] = label.raw();
    }

    /// Marks the leaves of `held` true and propagates upwards. An OR becomes true with its first
    /// true child and an AND when its count reaches its number of children. Appends the label of
    /// every root that becomes true. Only nodes reachable from `held` are visited.
    pub(super) fn authorise(&self, held: &[TermId], s: &mut Scratch, out: &mut Vec<LabelId>) {
        s.begin(self.node.len());
        for &leaf in held.iter().filter_map(|t| self.leaf.get(t)) {
            if s.arrive(leaf, 1) {
                s.queue.push(leaf);
            }
        }
        let mut next = 0;
        while let Some(&n) = s.queue.get(next) {
            next += 1;
            out.extend(self.root_label(n));
            let mut edge = self.first_parent[n as usize];
            while edge != NONE {
                let parent = self.edge_parent[edge as usize];
                if s.arrive(parent, self.needed(parent)) {
                    s.queue.push(parent);
                }
                edge = self.next_parent[edge as usize];
            }
        }
    }

    /// Top-down evaluation of the expression rooted at `n`.
    pub(super) fn eval(&self, n: u32, held: &impl Fn(TermId) -> bool) -> bool {
        match self.node[n as usize] {
            Node::Leaf(t) => held(t),
            Node::And => self.children(n).iter().all(|&c| self.eval(c, held)),
            Node::Or => self.children(n).iter().any(|&c| self.eval(c, held)),
        }
    }

    /// Appends to `out` held terms whose conjunction satisfies the expression rooted at `n`, and
    /// returns whether there are such terms. On `false`, `out` may hold terms of a partial attempt.
    pub(super) fn witness(
        &self,
        n: u32,
        held: &impl Fn(TermId) -> bool,
        out: &mut Vec<TermId>,
    ) -> bool {
        match self.node[n as usize] {
            Node::Leaf(t) => {
                out.push(t);
                held(t)
            }
            Node::And => self.children(n).iter().all(|&c| self.witness(c, held, out)),
            Node::Or => {
                let mark = out.len();
                self.children(n).iter().any(|&c| {
                    out.truncate(mark);
                    self.witness(c, held, out)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{Label, DEFAULT_MAX_NODES};

    fn intern(dag: &mut Dag, text: &str) -> u32 {
        let label = Label::parse(text, DEFAULT_MAX_NODES).unwrap();
        let mut term = |t: &str| TermId::new(u32::from(t.as_bytes()[0]));
        dag.intern(label.expr().unwrap(), &mut term)
    }

    #[test]
    fn a_pass_reaches_only_roots_whose_expression_is_true() {
        let mut dag = Dag::new();
        let roots = ["s&(a|b)", "a&e", "(s&e)|(a&b)"].map(|t| intern(&mut dag, t));
        for (i, &root) in roots.iter().enumerate() {
            dag.set_root_label(root, LabelId::new(i as u32));
        }
        let held = |terms: &str| {
            terms
                .bytes()
                .map(|b| TermId::new(u32::from(b)))
                .collect::<Vec<_>>()
        };
        let mut scratch = Scratch::default();
        for (terms, expected) in [
            ("sa", vec![0]),
            ("ae", vec![1]),
            ("se", vec![2]),
            ("ab", vec![2]),
        ] {
            let mut out = Vec::new();
            dag.authorise(&held(terms), &mut scratch, &mut out);
            assert_eq!(
                out,
                expected.into_iter().map(LabelId::new).collect::<Vec<_>>(),
                "{terms}"
            );
        }
    }

    #[test]
    fn the_scratch_survives_its_epoch_wrapping() {
        let mut dag = Dag::new();
        let root = intern(&mut dag, "a&b");
        dag.set_root_label(root, LabelId::new(0));
        let mut scratch = Scratch {
            epoch: u32::MAX,
            ..Scratch::default()
        };
        let mut out = Vec::new();
        dag.authorise(&[TermId::new(u32::from(b'a'))], &mut scratch, &mut out);
        dag.authorise(&[TermId::new(u32::from(b'b'))], &mut scratch, &mut out);
        assert!(out.is_empty());
    }
}
