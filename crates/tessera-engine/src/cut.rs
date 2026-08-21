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

/// One treed level's lineage, as the cut needs it: a parent per ordinal.
///
/// **Built from the parent pointers alone**, which is the only durable direction — the child
/// direction is this relation read the other way and is derived here, per request, rather than
/// stored. That is what keeps a deletion from having to rewrite two copies of one fact.
///
/// **Indexed by ordinal, because that is how a level is stored.** A level is a dense vector with
/// holes, so the lineage is one too: building it is a linear fill with a single allocation, and a
/// lookup is an index rather than a tree descent. This is on the request path for every treed
/// layer, beside a per-artifact loop that already walks the whole level — so it has to cost a
/// fraction of that loop rather than a multiple of it, which a per-node map insert would have been.
pub struct Lineage {
    /// `parent[ordinal]`, `None` at a root and at an ordinal the level does not hold.
    parent: Vec<Option<u32>>,
    /// `depth[ordinal]`, the root being 0 — resolved once for the whole level when the lineage is
    /// built, rather than per request.
    ///
    /// **Depth is a property of the tree and not of the viewer**, so a request that recomputes it
    /// is redoing generation work. It was measured at ~57 ms of the cut's ~240 at a 10⁷-artifact
    /// level, and it belongs here for the same reason the parent map does — which is also why the
    /// lineage itself wants holding per generation rather than rebuilding on every request, the
    /// larger version of the same observation.
    depth: Vec<u32>,
    /// How many ordinals name a parent. **The cycle bound**, and tighter than the vector's length:
    /// a chain cannot be longer than the number of edges that exist.
    edges: usize,
}

impl Lineage {
    /// Build from `(ordinal, parent)` pairs — every artifact of the level, not only the passing
    /// ones. **An ancestor that failed its own criterion is still an ancestor**: it cannot be
    /// served, but a cut that did not know it was there would mistake its two children for
    /// siblings of a different parent and keep both when one covers the other.
    pub fn new(pairs: impl IntoIterator<Item = (u32, Option<u32>)>) -> Self {
        let mut parent: Vec<Option<u32>> = Vec::new();
        let mut edges = 0;
        for (ordinal, of) in pairs {
            if of.is_none() {
                continue;
            }
            let idx = ordinal as usize;
            if parent.len() <= idx {
                parent.resize(idx + 1, None);
            }
            parent[idx] = of;
            edges += 1;
        }
        let depth = Self::depths(&parent, edges);
        Lineage {
            parent,
            depth,
            edges,
        }
    }

    /// Every ordinal's depth, resolved in one pass with the ancestors memoised.
    ///
    /// **Ancestors are shared, so depth is the quantity most worth memoising.** A balanced tree's
    /// lineages all pass through the same handful of nodes near the root, and computing each node's
    /// depth by walking to the root would re-walk that spine once per descendant — the depth of the
    /// tree multiplied by the number of nodes, for an answer that never changes.
    fn depths(parent: &[Option<u32>], edges: usize) -> Vec<u32> {
        const UNKNOWN: u32 = u32::MAX;
        let mut known = vec![UNKNOWN; parent.len()];
        let mut stack: Vec<u32> = Vec::new();
        for node in 0..parent.len() as u32 {
            if known[node as usize] != UNKNOWN {
                continue;
            }
            stack.clear();
            let mut at = node;
            // Climb to the first node whose depth is already known, or to a root. The cycle guard
            // is the edge count, as everywhere else here: the build refuses a cycle, and a
            // hand-written WAL is not a build.
            let base = loop {
                match parent.get(at as usize).copied().flatten() {
                    None => {
                        known[at as usize] = 0;
                        break 0;
                    }
                    Some(up) => {
                        if stack.len() > edges {
                            known[at as usize] = 0;
                            break 0;
                        }
                        stack.push(at);
                        if known.get(up as usize).copied().unwrap_or(UNKNOWN) != UNKNOWN {
                            break known[up as usize];
                        }
                        if up as usize >= known.len() {
                            known[at as usize] = 0;
                            break 0;
                        }
                        at = up;
                    }
                }
            };
            // Unwind, deepest last: each node is one below the one above it.
            let mut d = base;
            for &n in stack.iter().rev() {
                d += 1;
                known[n as usize] = d;
            }
        }
        known
    }

    /// The parent of `ordinal`, or `None` at a root and at an ordinal this level does not hold.
    fn parent_of(&self, ordinal: u32) -> Option<u32> {
        self.parent.get(ordinal as usize).copied().flatten()
    }

    /// True where no artifact of the level names a parent — the flat case, which every layer
    /// before this stage is, and which the cut short-circuits entirely.
    pub fn is_flat(&self) -> bool {
        self.edges == 0
    }

    /// The depth of `ordinal`, the root being 0. A lookup: see [`Lineage::depth`].
    ///
    /// An ordinal the level does not hold is a root, which is what it is in every other reading
    /// here too.
    pub fn depth(&self, ordinal: u32) -> u32 {
        self.depth.get(ordinal as usize).copied().unwrap_or(0)
    }

    /// `node` and every ancestor of it, nearest first. Bounded for a cycle's sake.
    ///
    /// **Only the reference implementation uses this now.** The serving path climbs without
    /// materialising a chain per node — allocating one was the cost the plan was rewritten to
    /// remove — but the obvious form the plan is checked against is written with it, and reads the
    /// way the rule is stated.
    #[cfg(test)]
    fn chain(&self, node: u32) -> Vec<u32> {
        let mut chain = vec![node];
        let mut at = node;
        while let Some(parent) = self.parent_of(at) {
            if chain.len() > self.edges {
                break;
            }
            chain.push(parent);
            at = parent;
        }
        chain
    }
}

/// One request's cut, resolved once and then asked for any depth.
///
/// **Every node this cut can serve is served over exactly one contiguous range of depths**, and
/// that single observation is the whole structure. A frontier node's lineage picks, at depth *d*,
/// the deepest passing ancestor at or above *d* — so as *d* grows the pick walks *down* the lineage
/// and each node on it is the pick over one interval and never again. Taking the union across the
/// lineages that share a node leaves an interval, because they share its lower end.
///
/// So the plan is one interval per servable node — `[from, until)` — computed in two sweeps of the
/// level, and every question the budget search asks is answered from it:
///
/// - **how many does depth *d* serve** is a lookup in a per-depth count, accumulated from the
///   intervals with a difference array. The bisection needs no evaluation at all.
/// - **which artifacts does depth *d* serve** is one filtered pass, in ascending ordinal order by
///   construction — so the result needs no sort and no dedup.
///
/// **This replaces materialising a lineage per frontier node**, which was the shape that made the
/// cut the largest term in a whole-corpus request. At a 10⁷-artifact level with everything passing
/// there are ~4.4 million frontier nodes whose lineages are ~15 deep and almost entirely shared:
/// 67 million entries, half a gigabyte, to answer for a few thousand. Measured at **852 ms of the
/// cut's 975 ms** in `Vec<Vec<_>>` form, and **326 ms** flattened into one buffer — against a
/// frontier walk of 12 ms and side tables of 2 ms, so the storage was the whole cost and not the
/// arithmetic.
struct Plan {
    /// The servable nodes, **ascending** — which is what lets [`Plan::serve_at`] return an
    /// ascending, deduplicated result without sorting one.
    nodes: Vec<u32>,
    /// Parallel to `nodes`: the first depth at which this node is served.
    ///
    /// Its own depth, except where it has no passing ancestor at all — then it is what its lineage
    /// falls back to at every shallower cut, which is the rule that keeps a branch whose ancestors
    /// were suppressed, deleted or below their bar on the map rather than blanking its region.
    from: Vec<u32>,
    /// Parallel to `nodes`: the first depth at which this node is **no longer** served, because a
    /// passing node beneath it has become reachable in some lineage that was picking this one.
    /// `u32::MAX` for a frontier node, which is picked at every depth from its own downward.
    until: Vec<u32>,
    /// `counts[d]` is `serve_at(d).len()`, for `d` in `0..=deepest`.
    counts: Vec<u32>,
}

impl Plan {
    /// `prune` is the layer's `prune_children`: with it, only the frontier contributes — where a
    /// parent and a child both pass, the child is the finer statement and the parent would draw
    /// over it. Without it **every passing artifact contributes**, so a client receives the whole
    /// visible tree and can nest it, or filter to one subtree while drawing the rest.
    ///
    /// **Neither is safer than the other**, which is unusual enough here to state: every artifact
    /// in `passing` cleared its own criterion independently, so pruning serves strictly less and
    /// not pruning reveals nothing beyond what each artifact's own presence already does. The
    /// choice is the layer's, on rendering grounds.
    fn new(lineage: &Lineage, passing: &[u32], prune: bool) -> Self {
        // **Borrowed where it is already in order**, which is the case the serving path always
        // produces: it collects ordinals by scanning the level, so they arrive strictly ascending.
        // Copying forty megabytes to sort what is sorted is pure waste, and the check that avoids
        // it is one sequential pass.
        let owned: Vec<u32>;
        let sorted: &[u32] = if passing.windows(2).all(|w| w[0] < w[1]) {
            passing
        } else {
            let mut v = passing.to_vec();
            v.sort_unstable();
            v.dedup();
            owned = v;
            &owned
        };

        // **Ordinal-indexed side tables, not sorted lookups.** A level is a dense ordinal space, so
        // every question this pass asks of a node — is it passing, is it covered, what is its
        // depth — is an array index.
        let span = sorted
            .last()
            .copied()
            .unwrap_or(0)
            .max(lineage.parent.len().saturating_sub(1) as u32) as usize
            + 1;
        let mut is_passing = vec![false; span];
        for &n in sorted {
            is_passing[n as usize] = true;
        }

        // The frontier: a passing node is covered exactly when some *other* passing node names it
        // among its ancestors. Walking up from each passing node marks that in one pass — and the
        // walk **stops at the first node an earlier walk already climbed through**, because
        // everything above it was marked then. That is what keeps the total linear in the passing
        // set rather than multiplying it by the depth: sibling lineages share a spine, and without
        // the memo every one of them re-walks it to the root.
        let mut covered = vec![false; span];
        let mut climbed = vec![false; span];
        for &node in sorted {
            let mut at = node;
            for _ in 0..=lineage.edges {
                let Some(parent) = lineage.parent_of(at) else {
                    break;
                };
                if is_passing[parent as usize] {
                    covered[parent as usize] = true;
                }
                if climbed[parent as usize] {
                    break;
                }
                climbed[parent as usize] = true;
                at = parent;
            }
        }

        // The heads: the nodes whose lineages the cut picks from. Ascending, because `sorted` is —
        // and this *is* the answer at an unbounded depth, since every lineage's deepest entry is
        // its own head.
        let heads: Vec<u32> = sorted
            .iter()
            .copied()
            .filter(|&n| !prune || !covered[n as usize])
            .collect();
        let mut is_head = vec![false; span];
        for &h in &heads {
            is_head[h as usize] = true;
        }

        // **Everything on some lineage is `climbed ∪ passing`, so there is no second climb.** Every
        // passing node is an ancestor-or-self of a head — descend through passing descendants and
        // the descent ends at a node with none, which is a head — so the passing set is on-chain
        // outright; and an ancestor of a head is an ancestor of a passing node, which is what
        // `climbed` already marked. The two sets are therefore equal, and the walk that used to
        // recompute one of them was a second pass over the spine for an answer already held.
        let mut on_chain = climbed;
        for &n in sorted {
            on_chain[n as usize] = true;
        }

        // Depths, and the on-chain nodes bucketed by depth in the same pass — a counting sort,
        // since a level's depth is small and bounded by its own edge count. Both sweeps below need
        // one order or the other.
        let mut deepest = 0u32;
        for &head in &heads {
            deepest = deepest.max(lineage.depth(head));
        }
        let mut by_depth: Vec<Vec<u32>> = vec![Vec::new(); deepest as usize + 1];
        for node in 0..span as u32 {
            if on_chain[node as usize] {
                let d = lineage.depth(node);
                if (d as usize) < by_depth.len() {
                    by_depth[d as usize].push(node);
                }
            }
        }

        // **Deepest first: when does a node stop being anybody's pick?** When a passing node
        // beneath it becomes reachable — so the answer is the *latest* such depth over the lineages
        // running through it, which is a max accumulated into the parent.
        let mut until = vec![0u32; span];
        for depth in (0..by_depth.len()).rev() {
            for &node in &by_depth[depth] {
                let contribution = if is_passing[node as usize] {
                    lineage.depth(node)
                } else {
                    until[node as usize]
                };
                if let Some(parent) = lineage.parent_of(node) {
                    let at = &mut until[parent as usize];
                    *at = (*at).max(contribution);
                }
            }
        }

        // **Shallowest first: does this node have a passing ancestor at all?** Where it has none it
        // is its lineage's fallback, so it is served from depth zero rather than from its own.
        let mut has_passing_ancestor = vec![false; span];
        for level in by_depth.iter() {
            for &node in level {
                if let Some(parent) = lineage.parent_of(node) {
                    has_passing_ancestor[node as usize] =
                        is_passing[parent as usize] || has_passing_ancestor[parent as usize];
                }
            }
        }

        // Every passing node on a lineage is servable at *some* depth, not only the heads — a
        // parent is what its children's lineages fall back to. Sizing these to the head count
        // instead grows them through several reallocations at a level where the difference is a
        // hundred megabytes.
        let servable = sorted.len().max(1);
        let mut nodes = Vec::with_capacity(servable);
        let mut from = Vec::with_capacity(servable);
        let mut until_of = Vec::with_capacity(servable);
        let mut delta = vec![0i64; deepest as usize + 2];
        for node in 0..span as u32 {
            if !on_chain[node as usize] || !is_passing[node as usize] {
                continue;
            }
            let lo = if has_passing_ancestor[node as usize] {
                lineage.depth(node)
            } else {
                0
            };
            let hi = if is_head[node as usize] {
                u32::MAX
            } else {
                until[node as usize]
            };
            if lo >= hi {
                continue;
            }
            nodes.push(node);
            from.push(lo);
            until_of.push(hi);
            delta[lo as usize] += 1;
            if (hi as usize) < delta.len() {
                delta[hi as usize] -= 1;
            }
        }

        let mut counts = Vec::with_capacity(deepest as usize + 1);
        let mut running = 0i64;
        for step in delta.iter().take(deepest as usize + 1) {
            running += step;
            counts.push(running as u32);
        }

        Plan {
            nodes,
            from,
            until: until_of,
            counts,
        }
    }

    /// The deepest depth any lineage reaches — the top of the budget search's range.
    fn deepest(&self) -> u32 {
        self.counts.len().saturating_sub(1) as u32
    }

    /// How many artifacts a cut at `depth` serves, without building the set.
    fn count_at(&self, depth: u32) -> u32 {
        let at = (depth as usize).min(self.counts.len().saturating_sub(1));
        self.counts.get(at).copied().unwrap_or(0)
    }

    /// The artifacts a cut at `depth` serves.
    ///
    /// Each lineage contributes the deepest passing node at or above `depth`. **A node with no
    /// passing ancestor shallow enough contributes itself**, which is what keeps a branch whose
    /// ancestors were all suppressed, deleted or below their own bar on the map rather than
    /// blanking its region — and it is why `from` is zero for such a node rather than its depth.
    ///
    /// **Ascending and deduplicated by construction**: `nodes` is built by an ordinal scan, so no
    /// sort runs here at all.
    fn serve_at(&self, depth: u32) -> Vec<u32> {
        // Clamped, as [`Plan::count_at`] is: an unbounded depth means *the deepest cut there is*,
        // and a frontier node's interval is open at the top. Comparing against `u32::MAX` itself
        // would exclude exactly the nodes an unbounded depth is meant to serve.
        let depth = depth.min(self.deepest());
        let mut served = Vec::new();
        for i in 0..self.nodes.len() {
            if self.from[i] <= depth && depth < self.until[i] {
                served.push(self.nodes[i]);
            }
        }
        served
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
/// **The result is ascending and deduplicated**, on every path including the flat short circuit.
/// That is a contract rather than an accident: the serving path tests each candidate against the
/// served set by binary search, and a caller that passed its ordinals in some other order would
/// otherwise get a silently short response rather than an error.
pub fn cut_at(lineage: &Lineage, passing: &[u32], depth: u32, prune: bool) -> Vec<u32> {
    if lineage.is_flat() {
        return ascending(passing);
    }
    Plan::new(lineage, passing, prune).serve_at(depth)
}

/// `passing`, ascending and deduplicated — the flat case's whole answer.
fn ascending(passing: &[u32]) -> Vec<u32> {
    let mut out = passing.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// The cut this request serves: the deepest one that fits `budget`, or the full frontier if none
/// was asked for.
///
/// **The search is over depths and never over artifacts.** Taking the deepest cut that fits is what
/// makes the budget a resolution knob rather than a selection: every node the cut returns is one
/// the viewer may see, and the ones it does not return are covered by an ancestor it did.
///
/// **The search is a bisection, which the count's monotonicity licenses.** A deeper cut moves each
/// frontier node's representative down its own lineage and never up, and two nodes that shared a
/// representative can only separate — so the served count is non-decreasing in depth and the
/// deepest depth that fits can be found in `log(depth)` evaluations rather than by walking every
/// depth. That property is worth stating because the bisection is wrong without it.
///
/// **A budget that not even the roots fit is served anyway**, at depth 0. The alternative is
/// dropping nodes to reach the number, which is the sampling decision 0083 forbids: a map missing
/// arbitrary roots claims those regions are empty.
/// Ascending and deduplicated, as [`cut_at`] is and for the same reason.
pub fn cut(lineage: &Lineage, passing: &[u32], budget: Option<u32>, prune: bool) -> Vec<u32> {
    if lineage.is_flat() {
        return ascending(passing);
    }
    let plan = Plan::new(lineage, passing, prune);
    let Some(budget) = budget else {
        return plan.serve_at(u32::MAX);
    };
    if plan.count_at(u32::MAX) <= budget {
        return plan.serve_at(u32::MAX);
    }
    // **The search evaluates a count, not a cut.** Every depth's served total is already in the
    // plan, so the bisection is a handful of array reads and only the depth it settles on is ever
    // built. The bisection itself stays, rather than a scan, because the monotonicity argument
    // above is what licenses either and the search range is the level's depth.
    let mut best = 0;
    let (mut lo, mut hi) = (0u32, plan.deepest());
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        if plan.count_at(mid) <= budget {
            best = mid;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    plan.serve_at(best)
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
        assert_eq!(cut(&flat, &[0, 1, 2], None, true), vec![0, 1, 2]);
        // Even against a budget: there is no lineage to cut along, so there is nothing to trade.
        assert_eq!(cut(&flat, &[0, 1, 2], Some(1), true), vec![0, 1, 2]);
    }

    /// **The frontier: where a parent and a child both pass, the child is what is drawn.** Serving
    /// both would draw the parent's shape over the child's, and the parent's masked count beside a
    /// count it contains.
    #[test]
    fn a_passing_child_replaces_its_passing_parent() {
        let lineage = chain();
        assert_eq!(cut(&lineage, &[0, 1, 3], None, true), vec![3]);
        assert_eq!(cut(&lineage, &[0, 2], None, true), vec![2]);
    }

    /// **A hole in the lineage does not stop the child being served**, which is the proportional
    /// criterion's normal state: a parent at 5% of 10 000 fails a 10% rule while its child at 50%
    /// of 200 passes it, the child a strict subset throughout. No disclosure follows — each node
    /// passed its own test — and a cut that required an ancestor would blank the child's region.
    #[test]
    fn a_child_whose_parent_failed_is_still_served() {
        let lineage = chain();
        // 1 is absent from `passing`: it failed its own criterion. 3 is beneath it and passed.
        assert_eq!(cut(&lineage, &[0, 3], None, true), vec![3]);
        assert_eq!(cut(&lineage, &[3], None, true), vec![3]);
    }

    /// A budget is met by serving ancestors, and the cut taken is the deepest one that fits.
    #[test]
    fn a_budget_is_met_by_climbing_rather_than_dropping() {
        let lineage = chain();
        let passing = [0, 1, 2, 3, 4];
        // No budget: the frontier is the two leaves.
        assert_eq!(cut(&lineage, &passing, None, true), vec![2, 4]);
        // One artifact: only the root fits.
        assert_eq!(cut(&lineage, &passing, Some(1), true), vec![0]);
        // Two: the full frontier already fits, so nothing is traded away — a budget buys depth
        // and never costs it.
        assert_eq!(cut(&lineage, &passing, Some(2), true), vec![2, 4]);
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
        assert_eq!(cut(&wide, &passing, None, true), vec![3, 4, 5, 6]);
        // Four leaves do not fit in three, so the cut climbs to depth 1 and serves two nodes —
        // fewer than the budget, because a depth is what it can trade and not a count.
        assert_eq!(cut(&wide, &passing, Some(3), true), vec![1, 2]);
        assert_eq!(cut(&wide, &passing, Some(1), true), vec![0]);
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
                let a = cut(&lineage, &passing, Some(shallow), true);
                let b = cut(&lineage, &passing, Some(deep), true);
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
        assert_eq!(cut(&two_roots, &[0, 1, 2, 3], Some(1), true), vec![0, 1]);
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
        assert_eq!(cut(&lineage, &[2, 3], Some(1), true), vec![2, 3]);
        assert_eq!(
            cut(&lineage, &[2, 4], Some(1), true),
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

    // ---------------------------------------------------------------------------------------
    // The optimised plan against an obviously-correct reference
    // ---------------------------------------------------------------------------------------

    /// The cut, written the slow, obvious way: quadratic frontier, linear membership, a walk per
    /// lookup. **It is the specification of what [`Plan`] computes**, and it exists because the
    /// fast form's structure — a prebuilt chain per frontier node, a binary search per depth, a
    /// bisection over depths — is far enough from the rule it implements that reading it is not a
    /// proof.
    fn reference_cut_at(lineage: &Lineage, passing: &[u32], depth: u32) -> Vec<u32> {
        if lineage.is_flat() {
            return passing.to_vec();
        }
        let is_ancestor = |ancestor: u32, node: u32| {
            lineage.chain(node).iter().skip(1).any(|&n| n == ancestor)
        };
        let mut served: Vec<u32> = passing
            .iter()
            .copied()
            .filter(|&a| !passing.iter().any(|&b| b != a && is_ancestor(a, b)))
            .map(|a| {
                let chain = lineage.chain(a);
                chain
                    .iter()
                    .copied()
                    .find(|&n| passing.contains(&n) && lineage.depth(n) <= depth)
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

    /// The budget search, walked rather than bisected.
    fn reference_cut(lineage: &Lineage, passing: &[u32], budget: Option<u32>) -> Vec<u32> {
        let full = reference_cut_at(lineage, passing, u32::MAX);
        let Some(budget) = budget else {
            return full;
        };
        if full.len() as u32 <= budget {
            return full;
        }
        let deepest = passing.iter().map(|&a| lineage.depth(a)).max().unwrap_or(0);
        for depth in (0..deepest).rev() {
            let candidate = reference_cut_at(lineage, passing, depth);
            if candidate.len() as u32 <= budget {
                return candidate;
            }
        }
        reference_cut_at(lineage, passing, 0)
    }

    /// A deterministic pseudo-random stream — SplitMix64, the same finaliser the corpus generator
    /// uses. Deterministic so a failure here is a failure anyone can reproduce from the seed.
    fn stream(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    /// A random forest over `n` ordinals: each node's parent is drawn from the nodes before it, or
    /// it is a root. Ordinal order therefore agrees with lineage order, which is what a level
    /// published in one batch produces, and roots occur at every position rather than only at
    /// zero.
    fn random_lineage(next: &mut impl FnMut() -> u64, n: u32) -> Lineage {
        let pairs: Vec<(u32, Option<u32>)> = (0..n)
            .map(|ordinal| {
                if ordinal == 0 || next().is_multiple_of(4) {
                    (ordinal, None)
                } else {
                    (ordinal, Some((next() % ordinal as u64) as u32))
                }
            })
            .collect();
        Lineage::new(pairs)
    }

    /// **The fast plan and the obvious rule agree, over random trees, at every depth and every
    /// budget.** An optimisation that changed an answer would show up here rather than in the one
    /// hand-built tree the other tests use.
    #[test]
    fn the_plan_agrees_with_the_obvious_rule_over_random_trees() {
        let mut next = stream(0x5EED);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let lineage = random_lineage(&mut next, n);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            for depth in 0..8u32 {
                assert_eq!(
                    cut_at(&lineage, &passing, depth, true),
                    reference_cut_at(&lineage, &passing, depth),
                    "case {case}: the plan and the rule disagree at depth {depth}"
                );
            }
            for budget in 1..=8u32 {
                assert_eq!(
                    cut(&lineage, &passing, Some(budget), true),
                    reference_cut(&lineage, &passing, Some(budget)),
                    "case {case}: the bisection and the walk disagree at budget {budget}"
                );
            }
            assert_eq!(
                cut(&lineage, &passing, None, true),
                reference_cut(&lineage, &passing, None),
                "case {case}: the unbudgeted cuts disagree"
            );
        }
    }

    /// **The served count does not decrease with depth**, which is the property the budget's
    /// bisection rests on and is false for any rule that let a deeper cut merge two nodes into
    /// one. Asserted over the same random trees rather than argued.
    #[test]
    fn the_served_count_is_monotone_in_depth() {
        let mut next = stream(0xC0FFEE);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let lineage = random_lineage(&mut next, n);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            let mut previous = 0;
            for depth in 0..10u32 {
                let count = cut_at(&lineage, &passing, depth, true).len();
                assert!(
                    count >= previous,
                    "case {case}: the count fell from {previous} to {count} at depth {depth}, so                      the budget's bisection would settle on the wrong cut"
                );
                previous = count;
            }
        }
    }

    /// Every artifact a cut serves is one that passed — the property that makes the whole module
    /// unable to disclose, whatever it gets wrong about which node to draw.
    #[test]
    fn a_cut_never_serves_an_artifact_that_did_not_pass() {
        let mut next = stream(0xBEEF);
        for _ in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let lineage = random_lineage(&mut next, n);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            for budget in [None, Some(1), Some(3), Some(10)] {
                for served in cut(&lineage, &passing, budget, true) {
                    assert!(
                        passing.contains(&served),
                        "the cut served {served}, which never passed its own criterion"
                    );
                }
            }
            // And a cut is never empty while something passed: a blank map is the failure mode
            // the climb-to-a-passing-ancestor rule exists to prevent.
            assert!(!cut(&lineage, &passing, Some(1), true).is_empty());
        }
    }

    /// **Every cut comes back ascending and deduplicated**, including the flat short circuit and
    /// including a caller who did not sort. The serving path binary-searches this, so an unsorted
    /// return would drop artifacts from the response with nothing raising an error.
    #[test]
    fn every_cut_is_ascending_and_deduplicated() {
        let unsorted = [4, 2, 2, 0];
        let flat = Lineage::new([(0, None), (2, None), (4, None)]);
        assert_eq!(cut(&flat, &unsorted, None, true), vec![0, 2, 4]);
        assert_eq!(cut_at(&flat, &unsorted, 0, true), vec![0, 2, 4]);

        let lineage = chain();
        for budget in [None, Some(1), Some(2), Some(9)] {
            let served = cut(&lineage, &unsorted, budget, true);
            assert!(
                served.windows(2).all(|w| w[0] < w[1]),
                "a cut came back out of order or with a duplicate: {served:?}"
            );
        }
    }

    // ---------------------------------------------------------------------------------------
    // `prune_children = false`: the whole visible tree, not its frontier
    // ---------------------------------------------------------------------------------------

    /// **Unpruned, a passing parent is served beside its passing child**, and that is the default
    /// the layer declaration carries. It is what lets a client nest what it draws, or filter to one
    /// subtree while still drawing the rest of the map — neither of which is possible from a
    /// frontier, where the ancestors have already been dropped.
    #[test]
    fn without_pruning_every_passing_artifact_is_served() {
        let lineage = chain();
        let passing = [0, 1, 2, 3, 4];
        assert_eq!(cut(&lineage, &passing, None, false), vec![0, 1, 2, 3, 4]);
        // Pruned, the same input is the two leaves.
        assert_eq!(cut(&lineage, &passing, None, true), vec![2, 4]);
    }

    /// A hole in the lineage does not become a root: an unpruned cut serves what passed and no
    /// more, so a child whose parent failed is still served and the parent still is not.
    #[test]
    fn without_pruning_a_failing_ancestor_is_still_absent() {
        let lineage = chain();
        assert_eq!(cut(&lineage, &[2, 3, 4], None, false), vec![2, 3, 4]);
    }

    /// **A budget still bites without pruning**, and it climbs the same way. The difference is only
    /// which artifacts are candidates to be moved, not what a depth means.
    #[test]
    fn a_budget_climbs_the_same_way_without_pruning() {
        let lineage = chain();
        let passing = [0, 1, 2, 3, 4];
        assert_eq!(cut(&lineage, &passing, Some(1), false), vec![0]);
        assert_eq!(cut(&lineage, &passing, Some(3), false), vec![0, 1, 2]);
    }

    /// The unpruned cut is monotone in depth too, which the budget's bisection needs in both modes.
    #[test]
    fn the_unpruned_count_is_monotone_in_depth() {
        let mut next = stream(0xD00D);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let lineage = random_lineage(&mut next, n);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            let mut previous = 0;
            for depth in 0..10u32 {
                let count = cut_at(&lineage, &passing, depth, false).len();
                assert!(count >= previous, "case {case}: the count fell at depth {depth}");
                previous = count;
            }
        }
    }

    /// **At one depth, the pruned cut is a subset of the unpruned one.** That is the safety
    /// relationship between the two modes: both map a node to the same representative, and pruning
    /// simply maps fewer of them, so turning it on can never surface something turning it off
    /// would have withheld.
    ///
    /// **Under a budget the two are incomparable, and that is not a defect.** A budget is a count,
    /// and the unpruned cut spends more nodes per depth — so it settles shallower to fit the same
    /// number, and the pruned cut can hold a deep node the unpruned one has already replaced with
    /// an ancestor. Both answers are correct for what was asked; they are answers to different
    /// questions.
    #[test]
    fn at_one_depth_pruning_serves_a_subset_of_not_pruning() {
        let mut next = stream(0xFEED);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let lineage = random_lineage(&mut next, n);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            for depth in [0u32, 1, 3, u32::MAX] {
                let pruned = cut_at(&lineage, &passing, depth, true);
                let whole = cut_at(&lineage, &passing, depth, false);
                for served in &pruned {
                    assert!(
                        whole.contains(served),
                        "case {case}: at depth {depth} pruning served {served}, which the \
                         unpruned cut did not"
                    );
                }
            }
        }
    }
}
