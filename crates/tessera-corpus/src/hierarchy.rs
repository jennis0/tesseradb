//! The treed kind of the generated corpus: a layer whose artifacts stand in a parent/child tree.
//!
//! [The flat kind](crate::artifacts) answers which entities artifact `a` holds and which artifacts
//! hold entity `e`. A tree needs two more, closed form for the same reason: a node's children, and
//! a node's ancestors. The tree is index arithmetic over the ordinal space, so a parent is a
//! division and a child list is a multiplication.
//!
//! # The children do not exhaust their parent
//!
//! HDBSCAN's condensed tree loses points at a split: 20-25% of a parent's members fall out as
//! noise and join no child, measured on the arXiv corpus. A node's interval is
//! split [`COVERAGE_NUM`]/[`COVERAGE_DEN`] to its children and the remainder is stray: members the
//! parent holds and no child does.
//!
//! # Cases the generator always includes
//!
//! - The root holds every entity.
//! - Stray members at every internal node.
//! - A chain deep enough for a budget to matter: depth is `log_BRANCH(count)`.
//! - Leaves at unequal depths, because the last internal node's children run past `count`.

use crate::artifacts::layer_salt;
use crate::{keyed, salt, Corpus, Grant};

/// Children per internal node.
pub const BRANCH: u64 = 3;

/// The fraction of a parent's interval its children divide between them: 75%, leaving 25% stray.
pub const COVERAGE_NUM: u64 = 3;
pub const COVERAGE_DEN: u64 = 4;

const SALT_T_COUNT: u64 = salt(b"trd-cnt ");

impl Corpus {
    /// The parent of node `a`, or `None` at the root.
    pub fn artifact_parent(&self, a: u64) -> Option<u64> {
        (a > 0).then(|| (a - 1) / BRANCH)
    }

    /// The children of node `a` that exist in a level of `count` artifacts, ascending. Empty at a
    /// leaf.
    pub fn artifact_children(&self, count: u64, a: u64) -> Vec<u64> {
        (1..=BRANCH)
            .map(|i| a * BRANCH + i)
            .take_while(|&c| c < count)
            .collect()
    }

    /// Every ancestor of node `a`, nearest first, ending at the root. Empty at the root itself.
    pub fn artifact_ancestors(&self, a: u64) -> Vec<u64> {
        let mut chain = Vec::new();
        let mut node = a;
        while let Some(parent) = self.artifact_parent(node) {
            chain.push(parent);
            node = parent;
        }
        chain
    }

    /// The depth of node `a`, the root being 0.
    pub fn artifact_depth(&self, a: u64) -> u32 {
        self.artifact_ancestors(a).len() as u32
    }

    /// The members of node `a` in a treed level of `count` artifacts, as the half-open interval
    /// `[lo, hi)` over entity space.
    pub fn treed_interval(&self, count: u64, a: u64) -> (u64, u64) {
        if count == 0 || a >= count {
            return (0, 0);
        }
        // Descend from the root along `a`'s own path, narrowing the interval one level at a time.
        let mut span = (0, self.n());
        let mut chain = self.artifact_ancestors(a);
        chain.reverse();
        chain.push(a);
        for pair in chain.windows(2) {
            let (parent, child) = (pair[0], pair[1]);
            let index = child - parent * BRANCH - 1;
            span = child_span(span, index);
        }
        span
    }

    /// The members of node `a`, enumerated. Ascending, and a subset of its parent's.
    pub fn treed_members(&self, count: u64, a: u64) -> Vec<u64> {
        let (lo, hi) = self.treed_interval(count, a);
        (lo..hi).collect()
    }

    /// The members of `a` that no child of `a` holds: the stray the split lost.
    pub fn treed_stray(&self, count: u64, a: u64) -> Vec<u64> {
        let (lo, hi) = self.treed_interval(count, a);
        let children = self.artifact_children(count, a);
        if children.is_empty() {
            return (lo..hi).collect();
        }
        let covered_hi = children
            .iter()
            .map(|&c| self.treed_interval(count, c).1)
            .max()
            .unwrap_or(lo);
        (covered_hi..hi).collect()
    }

    /// Every node of a `count`-artifact level holding entity `e`, root first. `O(BRANCH × depth)`.
    pub fn treed_holding(&self, count: u64, e: u64) -> Vec<u64> {
        if count == 0 || e >= self.n() {
            return Vec::new();
        }
        let mut holders = vec![0];
        let mut span = (0, self.n());
        loop {
            let node = holders[holders.len() - 1];
            let next = (0u64..)
                .zip(self.artifact_children(count, node))
                .map(|(index, child)| (child, child_span(span, index)))
                .find(|&(_, (lo, hi))| e >= lo && e < hi);
            match next {
                Some((child, child_span)) => {
                    holders.push(child);
                    span = child_span;
                }
                None => return holders,
            }
        }
    }

    /// How many nodes this level's tree holds. Scaled `n / 10_000`, more gently than the flat and
    /// partition population, because a node's membership includes every descendant's.
    pub fn treed_count(&self, layer: u64) -> u64 {
        let scaled = self.n() / 10_000;
        let by_layer = keyed(self.seed(), SALT_T_COUNT ^ layer_salt(layer, 0), layer) % 4;
        scaled.saturating_sub(by_layer).max(BRANCH + 2)
    }

    /// The treed census: one O(n) pass, per-node visible counts. Every entity is counted once per
    /// ancestor it has, so a node's masked count is never less than the sum any one child reports.
    pub fn treed_artifact_census(&self, layer: u64, grant: &Grant) -> Vec<(u64, u64)> {
        let count = self.treed_count(layer);
        self.bucket_census(grant, |e| self.treed_holding(count, e))
    }
}

/// The `index`-th child's span of a parent's `[lo, hi)`: the children divide
/// [`COVERAGE_NUM`]/[`COVERAGE_DEN`] of the parent contiguously from `lo`; the tail is the
/// parent's alone.
fn child_span((lo, hi): (u64, u64), index: u64) -> (u64, u64) {
    let span = hi - lo;
    let covered = span * COVERAGE_NUM / COVERAGE_DEN;
    let each = covered / BRANCH;
    let start = lo + index * each;
    (start.min(hi), (start + each).min(hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{assert_census_counts_visible_members, corpus};

    /// A child's members are a subset of its parent's.
    #[test]
    fn a_childs_members_are_a_subset_of_its_parents() {
        let c = corpus(10_000);
        let count = 40;
        for a in 1..count {
            let parent = c.artifact_parent(a).expect("only the root has none");
            let (child_lo, child_hi) = c.treed_interval(count, a);
            let (parent_lo, parent_hi) = c.treed_interval(count, parent);
            assert!(
                child_lo >= parent_lo && child_hi <= parent_hi,
                "node {a} [{child_lo},{child_hi}) escapes its parent {parent} \
                 [{parent_lo},{parent_hi})"
            );
        }
    }

    /// And they do not exhaust it.
    #[test]
    fn every_internal_node_keeps_members_no_child_holds() {
        let c = corpus(10_000);
        let count = 40;
        for a in 0..count {
            if c.artifact_children(count, a).is_empty() {
                continue;
            }
            assert!(
                !c.treed_stray(count, a).is_empty(),
                "node {a}'s children exhaust it, so the non-covering case is untested"
            );
        }
    }

    /// `treed_holding` is the inverse of `treed_members` for every entity.
    #[test]
    fn the_two_directions_agree_over_every_entity() {
        let c = corpus(4_000);
        let count = 40;
        let mut expected: Vec<Vec<u64>> = vec![Vec::new(); c.n() as usize];
        for a in 0..count {
            for e in c.treed_members(count, a) {
                expected[e as usize].push(a);
            }
        }
        for (e, holders) in expected.iter_mut().enumerate() {
            holders.sort_unstable();
            let mut got = c.treed_holding(count, e as u64);
            got.sort_unstable();
            assert_eq!(
                got, *holders,
                "the reverse direction disagrees at entity {e}"
            );
        }
    }

    /// A node's ancestors and its children are inverses of each other.
    #[test]
    fn the_lineage_directions_agree() {
        let c = corpus(1_000);
        let count = 40;
        for a in 0..count {
            for child in c.artifact_children(count, a) {
                assert_eq!(
                    c.artifact_parent(child),
                    Some(a),
                    "node {child} is a child of {a} but does not name it as parent"
                );
                assert!(
                    c.artifact_ancestors(child).contains(&a),
                    "node {child}'s ancestor chain omits {a}"
                );
            }
        }
    }

    /// The tree is deep enough, and its leaves sit at more than one depth.
    #[test]
    fn the_tree_is_deep_and_unbalanced() {
        let c = corpus(10_000);
        let count = 30;
        let depths: Vec<u32> = (0..count)
            .filter(|&a| c.artifact_children(count, a).is_empty())
            .map(|a| c.artifact_depth(a))
            .collect();
        assert!(
            depths.iter().max().unwrap() >= &3,
            "the tree is too shallow for a budget to bite"
        );
        assert!(
            depths.iter().min() != depths.iter().max(),
            "every leaf sits at one depth, so the unbalanced case is untested"
        );
    }

    /// Prefix stability: the lineage is index arithmetic, so it does not move with the corpus size.
    #[test]
    fn the_lineage_does_not_depend_on_the_corpus_size() {
        let small = corpus(1_000);
        let large = corpus(100_000);
        for a in 0..40 {
            assert_eq!(small.artifact_parent(a), large.artifact_parent(a));
            assert_eq!(
                small.artifact_children(40, a),
                large.artifact_children(40, a)
            );
        }
    }

    /// Every node counts its descendants' members too, so the root's count is the number of
    /// items the grant sees in the whole corpus.
    #[test]
    fn the_census_counts_each_nodes_visible_members() {
        let c = corpus(20_000);
        let grant = Grant::parse("0,1,2,3").unwrap();
        let count = c.treed_count(8);
        let census = c.treed_artifact_census(8, &grant);
        assert_eq!(
            census[0],
            (0, (0..c.n()).filter(|e| c.visible(*e, &grant)).count() as u64)
        );
        assert_census_counts_visible_members(&c, &grant, census, 0..count, |a| {
            c.treed_members(count, a)
        });
    }
}
