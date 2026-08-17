//! The cut: which of a treed level's *passing* artifacts are the ones drawn.
//!
//! **Nothing here decides visibility, and that separation is the whole safety argument.** A node's
//! verdict is its own masked count against its own criterion, taken by
//! [`ArtifactView::verdict`](crate::artifacts::ArtifactView::verdict) with no input from its
//! lineage and none from the viewport
//! ([decision 0080](../../../docs/decisions/0080-the-frontier-is-a-per-artifact-test.md)). This
//! module runs *after* that, over the set that already passed, and its only question is which of
//! two artifacts a viewer may see is the one that belongs on the map. Serving fewer is always
//! sound; serving one that failed is not expressible here, because a failing node never enters.
//!
//! ## A budget is not a disclosure control
//!
//! [`ArtifactCut::budget`] is what the client can draw, and it is the single largest correctness
//! risk in this stage precisely because it *looks* like a control. It is not: a shallower cut
//! serves strictly less, and a deeper one serves only nodes that already passed against `M_auth`.
//! §8.4's maximum depth **is** a control and is fixed against `M_auth` for that reason — if you
//! find yourself reasoning about what a budget hides, you have confused the two.
//!
//! **It is met by serving ancestors instead of their descendants, never by sampling**
//! ([decision 0083](../../../docs/decisions/0083-the-frontier-is-a-request-time-budget.md)).
//! Dropping half the nodes gives a wrong map rather than half a map: the regions behind the dropped
//! nodes read as empty, which is the one thing a density map must never say by accident.
//!
//! ## One depth for the whole tree
//!
//! ⊘ **A budget resolves to one depth across every branch, and the general case is unspecified.**
//! A per-branch depth is the honest answer for an unbalanced tree — a dense branch could be cut
//! shallow while a sparse one stays deep — and it is deliberately not built: it changes what "a
//! cut" means, and the agreement property below is written for the single-depth form.
//!
//! **Two budgets agree on every artifact both return.** [`cut_at`] is a per-artifact predicate
//! given a depth, so an artifact served under a shallow cut and a deep one is the same artifact
//! with the same masked count — a client that widens its budget mid-pan never sees a node change
//! its meaning, only nodes appear beside it.

use std::collections::BTreeMap;

/// One treed level's lineage, as the cut needs it: a parent per ordinal.
///
/// **Built from the parent pointers alone**, which is the only durable direction — the child
/// direction is this relation read the other way and is derived here, per request, rather than
/// stored. That is what keeps a deletion from having to rewrite two copies of one fact.
pub struct Lineage {
    /// `parent[ordinal]`, `None` at a root and at an ordinal the level does not hold.
    parent: BTreeMap<u32, u32>,
}

impl Lineage {
    /// Build from `(ordinal, parent)` pairs — every artifact of the level, not only the passing
    /// ones. **An ancestor that failed its own criterion is still an ancestor**: it cannot be
    /// served, but a cut that did not know it was there would mistake its two children for
    /// siblings of a different parent and keep both when one covers the other.
    pub fn new(pairs: impl IntoIterator<Item = (u32, Option<u32>)>) -> Self {
        Lineage {
            parent: pairs
                .into_iter()
                .filter_map(|(ordinal, parent)| parent.map(|p| (ordinal, p)))
                .collect(),
        }
    }

    /// True where no artifact of the level names a parent — the flat case, which every layer
    /// before this stage is, and which the cut short-circuits entirely.
    pub fn is_flat(&self) -> bool {
        self.parent.is_empty()
    }

    /// The depth of `ordinal`, the root being 0.
    ///
    /// Bounded by the level's own size, so a cycle that reached durable state — the build refuses
    /// one, and a hand-written WAL is not a build — terminates rather than hanging a request.
    pub fn depth(&self, ordinal: u32) -> u32 {
        let mut depth = 0;
        let mut node = ordinal;
        while let Some(&parent) = self.parent.get(&node) {
            depth += 1;
            if depth as usize > self.parent.len() {
                break;
            }
            node = parent;
        }
        depth
    }

    /// Whether `ancestor` is a strict ancestor of `node`.
    fn is_ancestor_of(&self, ancestor: u32, node: u32) -> bool {
        let mut at = node;
        for _ in 0..=self.parent.len() {
            match self.parent.get(&at) {
                Some(&parent) if parent == ancestor => return true,
                Some(&parent) => at = parent,
                None => return false,
            }
        }
        false
    }

    /// `node` and every ancestor of it, nearest first.
    fn chain(&self, node: u32) -> Vec<u32> {
        let mut chain = vec![node];
        let mut at = node;
        while let Some(&parent) = self.parent.get(&at) {
            if chain.len() > self.parent.len() {
                break;
            }
            chain.push(parent);
            at = parent;
        }
        chain
    }
}

/// The artifacts of `passing` that a cut at `depth` serves.
///
/// Each node of the frontier — a passing node with no passing node beneath it — is replaced by the
/// **deepest passing node at or above `depth` in its own lineage**. Where a parent and a child both
/// passed, that rule keeps the child at a deep cut and the parent at a shallow one; where only the
/// child passed, it keeps the child at every cut.
///
/// **A node with no passing ancestor to climb to is served where it stands.** That is not a
/// relaxation of the budget, it is the whole reason the climb is expressed over *passing* nodes
/// rather than over depths: a lineage's ancestors are suppressed, deleted or below their own bar
/// often enough that a depth-shaped climb would blank the region under every one of them and call
/// it a smaller map. Serving more than the budget asked is a client's problem; serving nothing
/// where the viewer is entitled to something is the failure the budget exists around.
pub fn cut_at(lineage: &Lineage, passing: &[u32], depth: u32) -> Vec<u32> {
    if lineage.is_flat() {
        return passing.to_vec();
    }
    let frontier = passing
        .iter()
        .copied()
        .filter(|&a| !passing.iter().any(|&b| b != a && lineage.is_ancestor_of(a, b)));

    let mut served: Vec<u32> = frontier
        .map(|a| {
            let chain = lineage.chain(a);
            // Nearest first, so the first that fits is the deepest that fits.
            chain
                .iter()
                .copied()
                .find(|&n| passing.contains(&n) && lineage.depth(n) <= depth)
                // No passing ancestor is shallow enough — climb as far as the lineage allows and
                // stop there rather than dropping the branch.
                .unwrap_or_else(|| {
                    chain
                        .iter()
                        .copied()
                        .rfind(|n| passing.contains(n))
                        .unwrap_or(a)
                })
        })
        .collect();
    served.sort_unstable();
    served.dedup();
    served
}

/// The cut this request serves: the deepest one that fits `budget`, or the full frontier if none
/// was asked for.
///
/// **The search is over depths and never over artifacts.** Taking the deepest cut that fits is what
/// makes the budget a resolution knob rather than a selection: every node the cut returns is one
/// the viewer may see, and the ones it does not return are covered by an ancestor it did.
///
/// **A budget that not even the roots fit is served anyway**, at depth 0. The alternative is
/// dropping nodes to reach the number, which is the sampling decision 0083 forbids: a map missing
/// arbitrary roots claims those regions are empty.
pub fn cut(lineage: &Lineage, passing: &[u32], budget: Option<u32>) -> Vec<u32> {
    let full = cut_at(lineage, passing, u32::MAX);
    let Some(budget) = budget else {
        return full;
    };
    if full.len() as u32 <= budget {
        return full;
    }
    let deepest = passing.iter().map(|&a| lineage.depth(a)).max().unwrap_or(0);
    // Downward from the full depth: the first cut that fits is the deepest one that does, since a
    // shallower cut serves a subset of a deeper one's *coverage* and never more nodes.
    for depth in (0..deepest).rev() {
        let candidate = cut_at(lineage, passing, depth);
        if candidate.len() as u32 <= budget {
            return candidate;
        }
    }
    cut_at(lineage, passing, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-deep chain off one root, plus a second child of the root:
    ///
    /// ```text
    /// 0 ── 1 ── 3 ── 4
    ///  └── 2
    /// ```
    fn chain() -> Lineage {
        Lineage::new([
            (0, None),
            (1, Some(0)),
            (2, Some(0)),
            (3, Some(1)),
            (4, Some(3)),
        ])
    }

    /// A layer with no edges is served exactly as it was before this stage existed — the flat case
    /// is not a degenerate tree, it is a short circuit.
    #[test]
    fn a_flat_level_is_served_whole() {
        let flat = Lineage::new([(0, None), (1, None), (2, None)]);
        assert!(flat.is_flat());
        assert_eq!(cut(&flat, &[0, 1, 2], None), vec![0, 1, 2]);
        // Even against a budget: there is no lineage to cut along, so there is nothing to trade.
        assert_eq!(cut(&flat, &[0, 1, 2], Some(1)), vec![0, 1, 2]);
    }

    /// **The frontier: where a parent and a child both pass, the child is what is drawn.** Serving
    /// both would draw the parent's shape over the child's, and the parent's masked count beside a
    /// count it contains.
    #[test]
    fn a_passing_child_replaces_its_passing_parent() {
        let lineage = chain();
        assert_eq!(cut(&lineage, &[0, 1, 3], None), vec![3]);
        assert_eq!(cut(&lineage, &[0, 2], None), vec![2]);
    }

    /// **A hole in the lineage does not stop the child being served**, which is the proportional
    /// criterion's normal state: a parent at 5% of 10 000 fails a 10% rule while its child at 50%
    /// of 200 passes it, the child a strict subset throughout. No disclosure follows — each node
    /// passed its own test — and a cut that required an ancestor would blank the child's region.
    #[test]
    fn a_child_whose_parent_failed_is_still_served() {
        let lineage = chain();
        // 1 is absent from `passing`: it failed its own criterion. 3 is beneath it and passed.
        assert_eq!(cut(&lineage, &[0, 3], None), vec![3]);
        assert_eq!(cut(&lineage, &[3], None), vec![3]);
    }

    /// A budget is met by serving ancestors, and the cut taken is the deepest one that fits.
    #[test]
    fn a_budget_is_met_by_climbing_rather_than_dropping() {
        let lineage = chain();
        let passing = [0, 1, 2, 3, 4];
        // No budget: the frontier is the two leaves.
        assert_eq!(cut(&lineage, &passing, None), vec![2, 4]);
        // One artifact: only the root fits.
        assert_eq!(cut(&lineage, &passing, Some(1)), vec![0]);
        // Two: the full frontier already fits, so nothing is traded away — a budget buys depth
        // and never costs it.
        assert_eq!(cut(&lineage, &passing, Some(2)), vec![2, 4]);
    }

    /// A wider tree, where the budget genuinely bites and the depth it settles on is visible.
    ///
    /// ```text
    /// 0 ─┬─ 1 ─┬─ 3
    ///    │     └─ 4
    ///    └─ 2 ─┬─ 5
    ///          └─ 6
    /// ```
    #[test]
    fn the_budget_settles_on_the_deepest_depth_that_fits() {
        let wide = Lineage::new([
            (0, None),
            (1, Some(0)),
            (2, Some(0)),
            (3, Some(1)),
            (4, Some(1)),
            (5, Some(2)),
            (6, Some(2)),
        ]);
        let passing = [0, 1, 2, 3, 4, 5, 6];
        assert_eq!(cut(&wide, &passing, None), vec![3, 4, 5, 6]);
        // Four leaves do not fit in three, so the cut climbs to depth 1 and serves two nodes —
        // fewer than the budget, because a depth is what it can trade and not a count.
        assert_eq!(cut(&wide, &passing, Some(3)), vec![1, 2]);
        assert_eq!(cut(&wide, &passing, Some(1)), vec![0]);
    }

    /// **Two budgets agree on every artifact both return** — the property a client widening its
    /// budget mid-pan depends on. A node is never served meaning one thing at one budget and
    /// another at the next; the deeper cut adds nodes beside the ones the shallow cut kept.
    #[test]
    fn two_budgets_agree_on_every_artifact_both_return() {
        let lineage = chain();
        let passing = [0, 1, 2, 3, 4];
        for shallow in 1..=5u32 {
            for deep in shallow..=5 {
                let a = cut(&lineage, &passing, Some(shallow));
                let b = cut(&lineage, &passing, Some(deep));
                for ordinal in &a {
                    if b.contains(ordinal) {
                        // Present in both, and it is the same artifact: the cut chooses which
                        // ordinals are served and never what one of them means.
                        assert!(passing.contains(ordinal));
                    }
                }
            }
        }
    }

    /// A budget below the root count is served anyway rather than sampled down to it. Dropping
    /// roots would claim their regions are empty.
    #[test]
    fn a_budget_smaller_than_the_roots_serves_the_roots() {
        let two_roots = Lineage::new([(0, None), (1, None), (2, Some(0)), (3, Some(1))]);
        assert_eq!(cut(&two_roots, &[0, 1, 2, 3], Some(1)), vec![0, 1]);
    }

    /// **A branch with no passing ancestor is served where it stands rather than blanked.**
    ///
    /// This is the defect the integration test caught: the climb was expressed over *depths*, so a
    /// budget that resolved to depth 0 against a suppressed root served nothing at all — a blank
    /// map, from a viewer entitled to two artifacts, with no error anywhere. The climb is over
    /// passing nodes for that reason.
    #[test]
    fn a_branch_whose_ancestors_all_failed_is_served_where_it_stands() {
        let lineage = chain();
        // 0 and 1 failed — suppressed, deleted, or below their own bar; the cut cannot tell and
        // does not need to.
        assert_eq!(cut(&lineage, &[2, 3], Some(1)), vec![2, 3]);
        assert_eq!(
            cut(&lineage, &[2, 4], Some(1)),
            vec![2, 4],
            "a budget resolving below every passing node still serves them"
        );
    }

    /// Depth is measured from the root, and a node whose ancestors all failed still knows its own.
    #[test]
    fn depth_is_measured_through_failing_ancestors() {
        let lineage = chain();
        assert_eq!(lineage.depth(0), 0);
        assert_eq!(lineage.depth(1), 1);
        assert_eq!(lineage.depth(3), 2);
        assert_eq!(lineage.depth(4), 3);
    }
}
