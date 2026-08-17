//! The treed arm of the generated corpus: a layer whose artifacts stand in a parent/child tree.
//!
//! The [flat arm](crate::artifacts) answers *which entities does artifact a hold* and *which
//! artifacts hold entity e*. A treed layer needs two more, and they must be closed form for the
//! same reason: **a node's children, and a node's ancestors**. Both are here in constant time —
//! the tree is index arithmetic over the ordinal space, so a parent is a division and a child list
//! is a multiplication.
//!
//! # The children do not exhaust their parent, by construction
//!
//! HDBSCAN's condensed tree is the fixture Stage 5 turns on, and its defining property is that a
//! split loses points: 20–25% of a parent's members fall out as noise rather than joining any
//! child (measured on the arXiv corpus). **A generator producing a covering hierarchy would test
//! none of the machinery that property exists to exercise** — a rollup that unions children and
//! calls it the parent passes every covering fixture and is wrong on every real one.
//!
//! So a node's interval is split [`COVERAGE_NUM`]/[`COVERAGE_DEN`] to its children and the
//! remainder is **stray**: members the parent holds and no child does. The fraction is chosen at
//! the measured value rather than a round number, so a construction that assumed exhaustion fails
//! visibly rather than marginally.
//!
//! # What is deliberately planted
//!
//! - **The root holds every entity**, so the whole corpus is one node's membership — the case a
//!   proportional criterion divides by and an absolute one never reaches;
//! - **stray members at every internal node**, which is the non-covering case above;
//! - **a chain deep enough for a budget to bite** — depth is `log_BRANCH(count)`, so a level with
//!   a few thousand artifacts is six or seven deep;
//! - **leaves at unequal depths**, because the last internal node's children run past `count` and
//!   are simply absent: an unbalanced tree is what makes a single-depth cut visibly wrong, which
//!   is the check Stage 5 owes.

use crate::Corpus;

/// Children per internal node. Three rather than two: a binary tree makes "the middle child" and
/// "the only other child" the same case, and a fan-out that is not a power of two keeps the
/// arithmetic from agreeing with a bit shift by accident.
pub const BRANCH: u64 = 3;

/// The fraction of a parent's interval its children divide between them — 75%, leaving 25% stray.
/// The measured noise fraction at an HDBSCAN split is 20–25%, so this is its pessimistic end.
pub const COVERAGE_NUM: u64 = 3;
pub const COVERAGE_DEN: u64 = 4;

impl Corpus {
    /// The parent of node `a`, or `None` at the root.
    ///
    /// Closed form: the tree is laid out breadth-first over the ordinal space, so a node's parent
    /// is one division. No table, no walk, and independent of the corpus size — which is what
    /// keeps the whole arm prefix-stable.
    pub fn artifact_parent(&self, a: u64) -> Option<u64> {
        (a > 0).then(|| (a - 1) / BRANCH)
    }

    /// The children of node `a` that exist in a level of `count` artifacts, ascending.
    ///
    /// Empty at a leaf, and **short at the last internal node** — its later children run past
    /// `count`. That is the planted unbalanced case: a level whose leaves sit at two depths.
    pub fn artifact_children(&self, count: u64, a: u64) -> Vec<u64> {
        (1..=BRANCH)
            .map(|i| a * BRANCH + i)
            .take_while(|&c| c < count)
            .collect()
    }

    /// Every ancestor of node `a`, nearest first, ending at the root. Empty at the root itself.
    ///
    /// The reverse direction the census needs: a walk of `log_BRANCH(a)` divisions rather than a
    /// scan, so it costs the same at 10⁹ as at 10³.
    pub fn artifact_ancestors(&self, a: u64) -> Vec<u64> {
        let mut chain = Vec::new();
        let mut node = a;
        while let Some(parent) = self.artifact_parent(node) {
            chain.push(parent);
            node = parent;
        }
        chain
    }

    /// The depth of node `a` — the root being 0. What a cut's budget resolves against.
    pub fn artifact_depth(&self, a: u64) -> u32 {
        self.artifact_ancestors(a).len() as u32
    }

    /// The members of node `a` in a treed level of `count` artifacts, as the half-open interval
    /// `[lo, hi)` over entity space.
    ///
    /// **An interval rather than the flat arm's interval-plus-scatter**, because containment is the
    /// property this arm exists to plant and a scatter would make it a set operation rather than a
    /// comparison. The flat arm keeps the pathological membership shapes; this one keeps the
    /// lineage.
    pub fn treed_interval(&self, count: u64, a: u64) -> (u64, u64) {
        if count == 0 || a >= count {
            return (0, 0);
        }
        // Descend from the root along `a`'s own path, narrowing the interval one level at a time.
        // The path is recovered from the ancestor chain, which is arithmetic — so this is
        // `O(depth)` and touches no state.
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

    /// The members of node `a`, enumerated. Ascending, and a subset of its parent's — the
    /// containment the build's verification checks and the rollup argument rests on.
    pub fn treed_members(&self, count: u64, a: u64) -> Vec<u64> {
        let (lo, hi) = self.treed_interval(count, a);
        (lo..hi).collect()
    }

    /// The members of `a` that **no child of `a` holds** — the stray the split lost.
    ///
    /// Non-empty at every internal node, which is the property a covering hierarchy would not have
    /// and a construction assuming one gets wrong.
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

    /// Every node of a `count`-artifact level holding entity `e`, root first.
    ///
    /// The closed-form reverse direction. A descent rather than a scan: at each level at most one
    /// child can contain `e`, since the children's intervals are disjoint, so the walk is
    /// `O(BRANCH × depth)`.
    pub fn treed_holding(&self, count: u64, e: u64) -> Vec<u64> {
        if count == 0 || e >= self.n() {
            return Vec::new();
        }
        let mut holders = vec![0];
        let mut node = 0;
        loop {
            let next = self
                .artifact_children(count, node)
                .into_iter()
                .find(|&c| {
                    let (lo, hi) = self.treed_interval(count, c);
                    e >= lo && e < hi
                });
            match next {
                Some(child) => {
                    holders.push(child);
                    node = child;
                }
                None => return holders,
            }
        }
    }
}

/// The `index`-th child's span of a parent's `[lo, hi)`.
///
/// The children divide [`COVERAGE_NUM`]/[`COVERAGE_DEN`] of the parent contiguously from `lo`; the
/// tail is the parent's alone. Contiguous rather than interleaved so a child's interval is a
/// comparison against its parent's, which is what makes the containment check cheap enough to run
/// over every edge of a 10⁷-artifact level.
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
    use tessera_spatial::Bounds;

    fn corpus(n: u64) -> Corpus {
        Corpus::new(
            0x5EED,
            n,
            Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
        )
        .unwrap()
    }

    /// **The property the whole arm rests on**: a child's members are a subset of its parent's.
    /// Containment is what makes rollup sound under an absolute criterion, and the build's
    /// verification reports every edge that breaks it — so a generator that broke it would have the
    /// check reporting a defect in the system for a defect in itself.
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

    /// **And they do not exhaust it**, which is the case a planted balanced tree never produces
    /// and every real condensed tree does.
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

    /// The two directions are the same relation — the flat arm's census property, for lineage.
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
            assert_eq!(got, *holders, "the reverse direction disagrees at entity {e}");
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

    /// The tree is deep enough for a budget to bite, and its leaves sit at more than one depth —
    /// the unbalanced case a single-depth cut is visibly wrong for.
    ///
    /// **The count is not a complete tree, deliberately.** 1 + 3 + 9 + 27 = 40 is exactly full at
    /// depth 3, so a level of 40 is balanced and tests none of this; 30 leaves the last internal
    /// node's children running past the end, which is where the two leaf depths come from.
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
}
