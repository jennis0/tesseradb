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
//! ## The cut is taken over the viewer's tree
//!
//! **A withheld node is not in the viewer's tree** ([decision 0117](../../../docs/decisions/0117-a-child-may-name-several-parents.md) E,
//! `dag-hierarchies.md` §6). The structure a cut is taken over is the passing nodes, with an edge
//! wherever one passing node is the nearest passing ancestor of another on some path, and depth
//! counted in passing nodes only. A node the viewer may not see occupies no rung and the climb
//! passes through it, so the served set at every depth is identical to the world in which that
//! node never existed — which is what C29 promises. A cut that counted withheld nodes would let a
//! viewer sweeping budgets learn that a coarser artifact sits between two they were served.
//!
//! The stored lineage still holds every node of the level, because it is a property of the level
//! and not of the viewer; the per-request plan is what reads only the passing ones.
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
//!
//! ## A child may name several parents
//!
//! A `dag` layer's lineage is a directed acyclic graph (`dag-hierarchies.md` §3), and this module
//! reads it as one: parents are a list per node, **depth is the longest path from a root** (§5),
//! and the cut's rule is the tree rule with *its lineage* read as *each of its lineages* (§6). The
//! served count is then not monotone in depth — two parents at depth 1 are replaced by their one
//! shared child at depth 2 — so a budget on a DAG reads every depth's count rather than bisecting.

/// One treed level's lineage, as the cut needs it: the parents of each ordinal.
///
/// **Built from the parent lists alone**, which is the only durable direction — the child
/// direction is this relation read the other way and is derived here, per generation, rather than
/// stored. That is what keeps a deletion from having to rewrite two copies of one fact.
///
/// **Indexed by ordinal, because that is how a level is stored.** A level is a dense vector with
/// holes, so the lineage is one too: both directions are CSR over the ordinal space, building
/// them is a counting sort with a handful of allocations, and a lookup is an index rather than a
/// tree descent. This is on the request path for every treed layer, beside a per-artifact loop
/// that already walks the whole level — so it has to cost a fraction of that loop rather than a
/// multiple of it, which a per-node map insert would have been.
#[derive(Debug)]
pub struct Lineage {
    /// The parent direction as a CSR: `parents[parent_at[u]..parent_at[u + 1]]`, ascending and
    /// without duplicates — a duplicate edge is one edge, whichever spelling stated it
    /// (`dag-hierarchies.md` §4). A tree's lists are at most one long. `parent_at` spans every
    /// ordinal that names a parent or is named as one; an ordinal past its end has no parents.
    parent_at: Vec<u32>,
    parents: Vec<u32>,
    /// `depth[ordinal]`: **the longest path from a root**, the root being 0 (`dag-hierarchies.md`
    /// §5) — resolved once for the whole level when the lineage is built, rather than per request.
    ///
    /// Longest rather than shortest because it is the only choice under which every edge descends:
    /// `depth(parent) < depth(child)` for each parent. That is what makes it a topological order,
    /// and the topological order is what the cut's plan sweeps in. **It is not the depth the cut
    /// serves at** — that one is counted in passing nodes, per request — but a request that
    /// recomputed the order would be redoing generation work: it was measured at ~57 ms of the
    /// cut's ~240 at a 10⁷-artifact level, which is why the lineage itself is held per generation
    /// rather than rebuilt on every request, the larger version of the same observation.
    depth: Vec<u32>,
    /// How many edges the level holds. **The cycle bound** for the walks that count steps rather
    /// than marking nodes: a chain cannot be longer than the number of edges that exist.
    edges: usize,
    /// The child direction, as a CSR: `children[child_at[u]..child_at[u + 1]]`.
    ///
    /// **The parent direction alone cannot be walked downward**, and a budgeted cut wants to go
    /// downward: it settles on a shallow depth, so a walk from the roots that stops when the count
    /// exceeds the budget touches a thousand nodes where a sweep of the level touches ten million.
    /// Like [`Lineage::depth`] this is a property of the tree and not of the viewer.
    ///
    /// **Built on first use, and that is not premature.** It costs about as much as the depth table
    /// — ~105 ms at a level of ten million — and only the downward route reads it. A viewer narrow
    /// enough that the sweep is already cheap would otherwise pay for a structure their request
    /// never touches, which measured as a regression from 107 ms to 241 ms before this was made
    /// lazy.
    child_index: std::sync::OnceLock<(Vec<u32>, Vec<u32>)>,
    /// The ordinals with no parent, ascending — where the downward walk starts. An ordinal a child
    /// names that the level does not hold is a root here too: the walk requires every root to
    /// pass, and such a phantom never does, so the walk declines rather than answering over a
    /// lineage it cannot reach downward.
    roots: Vec<u32>,
    /// Whether this is a `dag` layer's lineage — a declaration, never inferred from the edges
    /// ([decision 0087](../../../docs/decisions/0087-cross-level-edges-are-information-not-rollup.md)).
    /// The downward walk declines on one, and the budget search reads every depth's count.
    dag: bool,
}

impl Lineage {
    /// Build a **tree's** lineage from `(ordinal, parents)` pairs — every artifact of the level,
    /// not only the passing ones, since the level's edges are a property of the level. A parent
    /// list is anything that iterates ordinals: an `Option<u32>` is a list of at most one.
    ///
    /// The plan reads only the passing nodes through this (see the module doc); the whole level
    /// is held so that one lineage serves every viewer.
    pub fn new<P>(pairs: impl IntoIterator<Item = (u32, P)>) -> Self
    where
        P: IntoIterator<Item = u32>,
    {
        Self::build(pairs, false)
    }

    /// Build a **`dag`** layer's lineage: the same shape, with a child free to name several
    /// parents and the flag the walks read.
    pub fn dag<P>(pairs: impl IntoIterator<Item = (u32, P)>) -> Self
    where
        P: IntoIterator<Item = u32>,
    {
        Self::build(pairs, true)
    }

    fn build<P>(pairs: impl IntoIterator<Item = (u32, P)>, dag: bool) -> Self
    where
        P: IntoIterator<Item = u32>,
    {
        // The edge list, `(child, parent)`, then a counting sort into the CSR. A self-edge is
        // refused at both entry points (`dag-hierarchies.md` §4); one arriving anyway is dropped
        // here rather than read as a cycle of length one.
        let mut edge_list: Vec<(u32, u32)> = Vec::new();
        let mut span = 0usize;
        for (ordinal, of) in pairs {
            for parent in of {
                if parent == ordinal {
                    continue;
                }
                span = span.max(ordinal as usize + 1).max(parent as usize + 1);
                edge_list.push((ordinal, parent));
            }
        }
        let mut parent_at = vec![0u32; span + 1];
        for &(child, _) in &edge_list {
            parent_at[child as usize + 1] += 1;
        }
        for slot in 1..=span {
            parent_at[slot] += parent_at[slot - 1];
        }
        let mut cursor = parent_at.clone();
        let mut parents = vec![0u32; edge_list.len()];
        for &(child, parent) in &edge_list {
            parents[cursor[child as usize] as usize] = parent;
            cursor[child as usize] += 1;
        }
        drop(edge_list);
        // Ascending and deduplicated per node. The lists are one long on a tree and a handful on
        // a DAG, so this is a pass over the edges and not a sort of them.
        let mut edges = 0usize;
        let mut write = 0usize;
        let mut read_at = vec![0u32; span + 1];
        for node in 0..span {
            let (lo, hi) = (parent_at[node] as usize, parent_at[node + 1] as usize);
            parents[lo..hi].sort_unstable();
            read_at[node] = write as u32;
            let mut last: Option<u32> = None;
            for i in lo..hi {
                let p = parents[i];
                if last != Some(p) {
                    parents[write] = p;
                    write += 1;
                    edges += 1;
                    last = Some(p);
                }
            }
        }
        read_at[span] = write as u32;
        parents.truncate(write);
        let parent_at = read_at;

        let depth = Self::depths(&parent_at, &parents);
        let roots = (0..span as u32)
            .filter(|&n| parent_at[n as usize] == parent_at[n as usize + 1])
            .collect();
        Lineage {
            parent_at,
            parents,
            depth,
            edges,
            child_index: std::sync::OnceLock::new(),
            roots,
            dag,
        }
    }

    /// How many ordinals the parent table spans — every ordinal that names or is named as a
    /// parent. An ordinal at or past this has no parents and no children.
    fn span(&self) -> usize {
        self.parent_at.len() - 1
    }

    /// The parents of `ordinal`, ascending; empty at a root and at an ordinal this level does
    /// not hold.
    pub fn parents_of(&self, ordinal: u32) -> &[u32] {
        let at = ordinal as usize;
        if at >= self.span() {
            return &[];
        }
        &self.parents[self.parent_at[at] as usize..self.parent_at[at + 1] as usize]
    }

    /// The child direction as a CSR — one counting pass and one fill over the parent CSR.
    fn children_of(parent_at: &[u32], parents: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let span = parent_at.len() - 1;
        let mut child_at = vec![0u32; span + 1];
        for &up in parents {
            child_at[up as usize + 1] += 1;
        }
        for slot in 1..=span {
            child_at[slot] += child_at[slot - 1];
        }
        let mut cursor = child_at.clone();
        let mut children = vec![0u32; parents.len()];
        for node in 0..span {
            for &up in &parents[parent_at[node] as usize..parent_at[node + 1] as usize] {
                children[cursor[up as usize] as usize] = node as u32;
                cursor[up as usize] += 1;
            }
        }
        (child_at, children)
    }

    /// The children of `ordinal`, ascending — building the index on the first call.
    fn children_of_node(&self, ordinal: u32) -> &[u32] {
        let (child_at, children) = self
            .child_index
            .get_or_init(|| Self::children_of(&self.parent_at, &self.parents));
        let at = ordinal as usize;
        if at + 1 >= child_at.len() {
            return &[];
        }
        &children[child_at[at] as usize..child_at[at + 1] as usize]
    }

    /// Every ordinal's depth — the longest path from a root — resolved in one pass with the
    /// ancestors memoised.
    ///
    /// **Ancestors are shared, so depth is the quantity most worth memoising.** A balanced tree's
    /// lineages all pass through the same handful of nodes near the root, and computing each node's
    /// depth by walking to the root would re-walk that spine once per descendant — the depth of the
    /// tree multiplied by the number of nodes, for an answer that never changes.
    ///
    /// A depth-first pass over the parent lists, iterative so a ten-million-node chain does not
    /// recurse ten million deep. The cycle guard is the in-progress mark: the build and the mint
    /// refuse a cycle, and a hand-written WAL is not a build, so a parent still in progress when
    /// its child is resolved contributes nothing rather than looping.
    fn depths(parent_at: &[u32], parents: &[u32]) -> Vec<u32> {
        const UNKNOWN: u32 = u32::MAX;
        const PENDING: u32 = u32::MAX - 1;
        let span = parent_at.len() - 1;
        let parents_of =
            |node: usize| &parents[parent_at[node] as usize..parent_at[node + 1] as usize];
        let mut known = vec![UNKNOWN; span];
        let mut stack: Vec<u32> = Vec::new();
        for node in 0..span as u32 {
            if known[node as usize] != UNKNOWN {
                continue;
            }
            stack.push(node);
            while let Some(&at) = stack.last() {
                let a = at as usize;
                match known[a] {
                    UNKNOWN => {
                        known[a] = PENDING;
                        for &up in parents_of(a) {
                            if known[up as usize] == UNKNOWN {
                                stack.push(up);
                            }
                        }
                    }
                    PENDING => {
                        // Every parent pushed above this node has been resolved and popped; one
                        // still pending is beneath it on the stack, which is a cycle.
                        let mut d = 0u32;
                        for &up in parents_of(a) {
                            let k = known[up as usize];
                            if k < PENDING {
                                d = d.max(k + 1);
                            }
                        }
                        known[a] = d;
                        stack.pop();
                    }
                    _ => {
                        stack.pop();
                    }
                }
            }
        }
        known
    }

    /// The ancestor-or-self of `ordinal` with the greatest `key`, among those `key` holds for —
    /// the search the per-point membership column makes from a point's leaf for the **deepest
    /// served** artifact above it, then the lowest identifier (`client-components.md` §5.10,
    /// `dag-hierarchies.md` §6). Every path is searched, so on a DAG a served ancestor on a
    /// second path is found; on a tree the search is the chain.
    ///
    /// `scratch` is the caller's buffer, so a resolver asking once per point allocates once. It
    /// doubles as the visited set — the ancestor set of one node is small, and a linear
    /// membership test over it is cheaper than a marker over the level — which is also what
    /// terminates the search over a malformed level's cycle.
    pub fn select_ancestor<K: Ord>(
        &self,
        ordinal: u32,
        scratch: &mut Vec<u32>,
        key: impl Fn(u32) -> Option<K>,
    ) -> Option<(u32, K)> {
        scratch.clear();
        scratch.push(ordinal);
        let mut best: Option<(u32, K)> = None;
        let mut next = 0usize;
        while next < scratch.len() {
            let at = scratch[next];
            next += 1;
            if let Some(k) = key(at) {
                if best.as_ref().is_none_or(|(_, held)| k > *held) {
                    best = Some((at, k));
                }
            }
            for &up in self.parents_of(at) {
                if !scratch.contains(&up) {
                    scratch.push(up);
                }
            }
        }
        best
    }

    /// True where no artifact of the level names a parent — the flat case, which every layer
    /// before this stage is, and which the cut short-circuits entirely.
    pub fn is_flat(&self) -> bool {
        self.edges == 0
    }

    /// The depth of `ordinal` — the longest path from a root, the root being 0. A lookup: see
    /// [`Lineage::depth`].
    ///
    /// An ordinal the level does not hold is a root, which is what it is in every other reading
    /// here too.
    pub fn depth(&self, ordinal: u32) -> u32 {
        self.depth.get(ordinal as usize).copied().unwrap_or(0)
    }

    /// Whether this is a `dag` layer's lineage.
    pub fn is_dag(&self) -> bool {
        self.dag
    }
}

/// One [`Lineage`] per `(layer, level)`, rebuilt when that level moves.
///
/// **A level's parent pointers depend on neither the mask nor the viewport**, so deriving them on
/// every request is generation work charged to a request. At a level of ten million it is ~96 ms —
/// larger than everything the cut itself now costs — and it is the same fact for the depth table
/// and the child index the lineage carries beside them.
///
/// Keyed on the level's version and nothing else: unlike a row-space projection this holds no view
/// and no prefix, because a parent is an ordinal in the same level whichever view is served, and a
/// fold renumbers rows rather than ordinals. The level's own version therefore carries the whole
/// of the invalidation, and it reaches: the two writes that can move a parent pointer are a
/// publication and the fold's retire, and both bump it.
///
/// **A growth rebuilds this and cannot have moved an edge** — it unions members and touches
/// nothing else about a record — so that is a rebuild the shared version buys and the lineage does
/// not need. One version per level is one thing to keep in step; a second counter for structure
/// alone would be two, and what argues for it is a cost rather than a correctness case.
/// A level of a layer — what a lineage is held against.
type LevelOf = (String, u32);

/// A held lineage and the level version it was derived from.
type Held = (u64, std::sync::Arc<Lineage>);

#[derive(Debug, Default)]
pub struct Lineages {
    cached: std::sync::Mutex<std::collections::BTreeMap<LevelOf, Held>>,
    /// How many lineages this has built since the engine opened — the cadence, counted, for the
    /// reason [`crate::artifacts::ArtifactProjections`]'s own counter carries.
    builds: std::sync::atomic::AtomicU64,
}

impl Lineages {
    pub fn new() -> Self {
        Self::default()
    }

    /// See [`Self::builds`].
    pub fn builds(&self) -> u64 {
        self.builds.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many lineages are held — the gauge beside [`Self::builds`].
    pub fn held(&self) -> usize {
        self.cached.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Drop every lineage held for one layer, when the layer is dropped — see
    /// [`crate::artifacts::ArtifactProjections::forget`], which this is the other half of and
    /// which carries the argument for both.
    pub fn forget(&self, layer: &str) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, _), _| held != layer);
    }

    /// This level's lineage at `level_version`, building it if what is held is stale.
    ///
    /// **The version and the build must come from one reading of the store**, which is the
    /// caller's obligation rather than this module's: `cut` knows nothing about an artifact store
    /// and is the better for it. The serving path takes both inside one `with_artifacts`.
    ///
    /// **The build runs outside this cache's lock**, as [`crate::artifacts::ArtifactProjections`]'s
    /// does and for the same reason: two threads racing one key both build, from the same level
    /// version, so the two results are equal and the waste is one derivation rather than a wrong
    /// answer.
    pub fn get_or_build<F>(
        &self,
        layer: &str,
        level: u32,
        level_version: u64,
        build: F,
    ) -> std::sync::Arc<Lineage>
    where
        F: FnOnce() -> Lineage,
    {
        let key = (layer.to_string(), level);
        if let Some((held, lineage)) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if *held == level_version {
                return std::sync::Arc::clone(lineage);
            }
        }
        let lineage = std::sync::Arc::new(build());
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (level_version, std::sync::Arc::clone(&lineage)));
        lineage
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
/// **Depth here is counted in passing nodes** (decision 0117 E): a node's rung is the longest
/// path to it through the passing nodes alone, so a withheld ancestor occupies no rung and the
/// climb passes through it to the nearest passing ancestor on each path. The stored depth is read
/// only as the order the sweeps run in.
///
/// So the plan is one interval per passing node — `[from, until)` — computed in three sweeps of
/// the on-chain nodes, and every question the budget search asks is answered from it:
///
/// - **how many does depth *d* serve** is a lookup in a per-depth count, accumulated from the
///   intervals with a difference array. The search needs no evaluation at all.
/// - **which artifacts does depth *d* serve** is one filtered pass over the passing nodes of
///   rung at most *d*, and never over the level.
///
/// **This replaces materialising a lineage per frontier node**, which was the shape that made the
/// cut the largest term in a whole-corpus request. At a 10⁷-artifact level with everything passing
/// there are ~4.4 million frontier nodes whose lineages are ~15 deep and almost entirely shared:
/// 67 million entries, half a gigabyte, to answer for a few thousand. Measured at **852 ms of the
/// cut's 975 ms** in `Vec<Vec<_>>` form, and **326 ms** flattened into one buffer — against a
/// frontier walk of 12 ms and side tables of 2 ms, so the storage was the whole cost and not the
/// arithmetic.
struct Plan {
    /// The passing nodes of each **rung** — passing-depth — ascending within a rung, so a cut at
    /// depth *d* reads `by_rung[0..=d]` and never touches the rest of the level.
    ///
    /// **This is what keeps the answer proportional to itself.** A budget forces a shallow cut, so
    /// the served set is the top of the tree: 729 artifacts out of ten million in the measured case.
    /// An earlier revision built one interval per *servable* node — three arrays of ten million,
    /// 120 MB — and then scanned all of them to pick those 729, which was 83 ms of the cut's 262.
    by_rung: Vec<Vec<u32>>,
    /// Per ordinal, the first depth at which it is **no longer** served, because a passing node
    /// beneath it has become reachable in some lineage that was picking it. `u32::MAX` for a
    /// frontier node, which is picked at every depth from its own downward.
    until: Vec<u32>,
    /// `counts[d]` is `serve_at(d).len()`, for `d` in `0..=deepest`.
    counts: Vec<u32>,
}

/// The value of a node's passing-depth slot while no passing ancestor has been found on any path.
const NO_PASSING_ABOVE: u32 = u32::MAX;

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
        // rung — is an array index.
        let span = (sorted.last().map_or(0, |&n| n as usize + 1)).max(lineage.span());
        let mut is_passing = vec![false; span];
        for &n in sorted {
            is_passing[n as usize] = true;
        }

        // The frontier: a passing node is covered exactly when some *other* passing node names it
        // among its ancestors. Climbing from each passing node marks that in one pass — and the
        // climb **expands each node's parents once**, because everything above a node an earlier
        // climb reached was marked then. That is what keeps the total linear in the edges rather
        // than multiplying the passing set by the depth: sibling lineages share a spine, and
        // without the memo every one of them re-walks it to the root. On a DAG the climb is a
        // search over every path, which the same memo bounds — and which needs no cycle guard,
        // since a node is expanded at most once.
        let mut covered = vec![false; span];
        let mut climbed = vec![false; span];
        let mut stack: Vec<u32> = Vec::new();
        for &node in sorted {
            stack.push(node);
            while let Some(at) = stack.pop() {
                for &up in lineage.parents_of(at) {
                    let u = up as usize;
                    if is_passing[u] {
                        covered[u] = true;
                    }
                    if !climbed[u] {
                        climbed[u] = true;
                        stack.push(up);
                    }
                }
            }
        }

        // The heads: the nodes whose lineages the cut picks from — and at an unbounded depth, the
        // answer itself, since every lineage's deepest entry is its own head.
        //
        // **Marked, not collected.** Materialising them cost 34 ms of the pass at a level of ten
        // million: six and a half million ordinals into a 27 MB vector, to answer a question that
        // is one array read wherever it is asked.
        let mut is_head = vec![false; span];
        for &n in sorted {
            if !prune || !covered[n as usize] {
                is_head[n as usize] = true;
            }
        }

        // **Everything on some lineage is `climbed ∪ passing`, so there is no second climb.** Every
        // passing node is an ancestor-or-self of a head — descend through passing descendants and
        // the descent ends at a node with none, which is a head — so the passing set is on-chain
        // outright; and an ancestor of a head is an ancestor of a passing node, which is what the
        // climb marked.
        //
        // ⊘ **Collecting them as a list during the climb was tried and reverted.** It removes the
        // scan below, and measured *worse* — 40 ms against 23 at a level where two thousand pass —
        // because the scan reads `depth` in ordinal order, which is the order it is stored in,
        // where a list reads it at random. The scan is not the floor; the side tables are.
        let mut on_chain = climbed;
        for &n in sorted {
            on_chain[n as usize] = true;
        }

        // The on-chain nodes bucketed by **stored** depth — the topological order the two sweeps
        // below run in, since the longest-path depth increases strictly along every edge
        // (`dag-hierarchies.md` §5). A counting sort, since a level's depth is small and bounded
        // by its own edge count. The deepest head bounds it: an on-chain node is an
        // ancestor-or-self of a head and so no deeper than one.
        //
        // ⊘ **Pre-sizing the buckets was tried and reverted.** A counting pass ahead of the fill
        // removes the reallocation, and measured *worse*: 193 ms against 198 at full passing, and
        // 32 ms against 23 at a level where only a few thousand pass, because the second sequential
        // scan of ten million costs more than the growth it avoids.
        let mut deepest = 0u32;
        for &n in sorted {
            if is_head[n as usize] {
                deepest = deepest.max(lineage.depth(n));
            }
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

        // **Shallowest first: how deep is this node in the viewer's tree?** One slot per node
        // carries, for a passing node, its rung — one more than the deepest rung among the
        // nearest passing ancestors on any path, or zero where there is none on any path — and,
        // for a withheld node, that same maximum handed through unchanged, which is what makes a
        // withheld node occupy no rung. A node with no passing ancestor on any path is a root of
        // the viewer's tree and served from depth zero: that is the fallback rule of
        // `annotations.md` §6, and under this counting it is not a special case.
        let mut rung = vec![NO_PASSING_ABOVE; span];
        let mut deepest_rung = 0u32;
        for level in &by_depth {
            for &node in level {
                let n = node as usize;
                let mut above = NO_PASSING_ABOVE;
                for &up in lineage.parents_of(node) {
                    let r = rung[up as usize];
                    if r != NO_PASSING_ABOVE && (above == NO_PASSING_ABOVE || r > above) {
                        above = r;
                    }
                }
                rung[n] = if is_passing[n] {
                    let r = if above == NO_PASSING_ABOVE {
                        0
                    } else {
                        above + 1
                    };
                    deepest_rung = deepest_rung.max(r);
                    r
                } else {
                    above
                };
            }
        }

        // **Deepest first: when does a node stop being anybody's pick?** When a passing node
        // beneath it becomes reachable — so the answer is the *latest* such rung over the lineages
        // running through it, which is a max accumulated into every parent. A withheld node hands
        // its own maximum upward, so the next passing node below is found through it.
        let mut until = vec![0u32; span];
        for level in by_depth.iter().rev() {
            for &node in level {
                let n = node as usize;
                let contribution = if is_passing[n] { rung[n] } else { until[n] };
                for &up in lineage.parents_of(node) {
                    let at = &mut until[up as usize];
                    *at = (*at).max(contribution);
                }
            }
        }

        // The intervals, and the per-depth counts accumulated from them into a difference array —
        // so the budget search is answered without materialising a single cut, and without an
        // array over the ordinal space to scan for it afterwards. A head is picked at every depth
        // from its own downward, so its interval is open at the top; written into `until` rather
        // than carried beside it, because the sweep that filled `until` has already run and
        // nothing below reads the old value.
        let mut by_rung: Vec<Vec<u32>> = vec![Vec::new(); deepest_rung as usize + 1];
        let mut delta = vec![0i64; deepest_rung as usize + 2];
        for &node in sorted {
            let n = node as usize;
            let lo = rung[n];
            if is_head[n] {
                until[n] = u32::MAX;
            }
            let hi = until[n];
            if lo >= hi {
                continue;
            }
            by_rung[lo as usize].push(node);
            delta[lo as usize] += 1;
            if (hi as usize) < delta.len() {
                delta[hi as usize] -= 1;
            }
        }
        let mut counts = Vec::with_capacity(deepest_rung as usize + 1);
        let mut running = 0i64;
        for step in delta.iter().take(deepest_rung as usize + 1) {
            running += step;
            counts.push(running as u32);
        }

        Plan {
            by_rung,
            until,
            counts,
        }
    }

    /// The deepest rung any passing node sits at — the top of the budget search's range.
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
    /// Each lineage contributes the deepest passing node at or above `depth`. A node with no
    /// passing ancestor on any path sits at rung zero, so it is served at every depth until a
    /// passing descendant takes over — which is what keeps a branch whose ancestors were all
    /// suppressed, deleted or below their own bar on the map rather than blanking its region.
    ///
    /// **Ascending and deduplicated by construction** — see [`cut_at`] for why that is a
    /// contract: the buckets are read shallow-first, so the merge happens here, over the served
    /// set the budget bounds and never over the level.
    fn serve_at(&self, depth: u32) -> Vec<u32> {
        // Clamped, as [`Plan::count_at`] is: an unbounded depth means *the deepest cut there is*,
        // and a head's interval is open at the top. Comparing against `u32::MAX` itself would
        // exclude exactly the nodes an unbounded depth is meant to serve.
        let depth = depth.min(self.deepest());
        // **Only the rungs a cut here can reach.** Every node served at `depth` sits at or above
        // it, so the deeper buckets are never read: at a budget that settles on depth six over a
        // ten-million-artifact level, that is a thousand nodes examined rather than ten million.
        let mut served: Vec<u32> = Vec::new();
        for level in self.by_rung.iter().take(depth as usize + 1) {
            for &node in level {
                if depth < self.until[node as usize] {
                    served.push(node);
                }
            }
        }
        served.sort_unstable();
        served
    }
}

/// **The budgeted cut, walked down from the roots instead of swept across the level.**
///
/// A budget settles on a shallow depth — a thousand artifacts is depth six in a three-way tree — so
/// the answer lives in the top of the tree while the general plan in [`Plan`] sweeps all ten million
/// nodes to find it. This walks depths from the roots and stops at the first one the budget cannot
/// hold, touching `O(budget × branching)` nodes.
///
/// # When it applies, and why the guard is what it is
///
/// It runs while **every node it has seen passes**, and gives up otherwise. That is not a
/// simplification of the rule but the condition under which the rule collapses:
///
/// - a node above the cut has a passing child, so some lineage through it reaches a passing node at
///   or above the cut, so a deeper node represents it and it is **not** served;
/// - a node *at* the cut has only strict descendants below it, so no lineage through it reaches a
///   passing node at or above the cut, and it **is** served;
/// - every node has a passing parent, so none is its lineage's fallback except a root, which is
///   served only at depth zero.
///
/// So the served set is exactly *the nodes at that depth*, and the count is how many there are.
/// Under `prune_children = false` every passing node is its own lineage's deepest entry, so the
/// served set is instead everything at or above the cut — the other branch below. And with every
/// node above the cut passing, the stored depth and the rung in the viewer's tree agree there, so
/// counting depth in passing nodes changes nothing this walk reads.
///
/// **This is the whole-corpus principal's case**, which is the one that bounds the system: a viewer
/// who can see everything passes everything, so nothing above the cut fails and the walk never
/// stops early. A viewer who cannot is exactly a viewer for whom the general plan's cost is already
/// proportional to the little they can see. What the guard gives up on is a mask that is broad
/// **and** fragmented — many artifacts passing, with failures scattered near the top — and there
/// the answer is the sweep, at the cost it has always had.
///
/// **It declines on a `dag` lineage** (`dag-hierarchies.md` §6): the walk reasons in depths and a
/// node with two parents is reached twice, and the sweep is the general route with no case yet for
/// teaching the walk the graph.
fn cut_top_down(lineage: &Lineage, passing: &[u32], budget: u32, prune: bool) -> Option<Vec<u32>> {
    if lineage.dag {
        return None;
    }
    // **Attempted only where it can succeed.** The walk needs every node above the cut to pass, so
    // a level most of which fails will decline at its first step — after paying to build the child
    // index. Half is a heuristic and nothing rests on it: both routes return the same cut, and this
    // only decides which one runs.
    if (passing.len() as u64) * 2 < lineage.span() as u64 {
        return None;
    }
    let holds = |n: u32| passing.binary_search(&n).is_ok();
    if !passing.windows(2).all(|w| w[0] < w[1]) {
        // The lookup above is a binary search, so an unordered `passing` would answer wrongly rather
        // than slowly. The serving path always hands this over ascending; a caller that does not
        // gets the sweep.
        return None;
    }

    // Depth zero is every ordinal with no parent — which includes the tail above the parent table,
    // since an ordinal it does not mention has no parent to name.
    let named = lineage.span() as u32;
    let span = passing.last().map_or(named, |&n| n + 1).max(named);
    let mut roots = lineage.roots.clone();
    roots.extend(named..span);
    if roots.is_empty() || !roots.iter().all(|&n| holds(n)) {
        // A root that fails puts its whole subtree's lineages into the general rule — every node
        // below it may be its own fallback — which is the sweep's business, not this walk's.
        return None;
    }
    let mut level: Vec<u32> = roots;

    let mut served = level.clone();
    let mut above: Vec<u32> = if prune { Vec::new() } else { level.clone() };
    // **The leaves already passed, and they stay.** A node with no children has no passing
    // descendant, so it is its own lineage's deepest entry — a head — and a deeper cut has nothing
    // to replace it with. A walk that swapped each depth for the next wholesale would drop the
    // shallow branches of an unbalanced tree and blank their regions, which is the failure the
    // budget exists to avoid rather than one it may cause.
    let mut leaves: Vec<u32> = Vec::new();
    // The walk is bounded twice over: by the budget, which stops it as soon as a depth is too wide,
    // and by the level's own depth, which the edge count bounds.
    for _ in 0..=lineage.edges {
        let mut next: Vec<u32> = Vec::new();
        for &node in &level {
            let kids = lineage.children_of_node(node);
            if kids.is_empty() {
                leaves.push(node);
            }
            next.extend_from_slice(kids);
        }
        if next.is_empty() {
            // The tree ran out before the budget did, so the deepest cut is the one in hand.
            return Some(finish(served));
        }
        // **The guard.** A failure anywhere at or below the frontier puts the rule back into its
        // general form, which this walk does not implement.
        if !next.iter().all(|&n| holds(n)) {
            return None;
        }
        let candidate: Vec<u32> = if prune {
            let mut all = leaves.clone();
            all.extend_from_slice(&next);
            all
        } else {
            let mut all = above.clone();
            all.extend_from_slice(&next);
            all
        };
        if candidate.len() as u32 > budget {
            return Some(finish(served));
        }
        if !prune {
            above.extend_from_slice(&next);
        }
        served = candidate;
        level = next;
    }
    Some(finish(served))
}

/// Ascending and deduplicated, which every cut promises — see [`cut_at`].
fn finish(mut served: Vec<u32>) -> Vec<u32> {
    served.sort_unstable();
    served.dedup();
    served
}

/// The artifacts of `passing` that a cut at `depth` serves.
///
/// Each node of the frontier — a passing node with no passing node beneath it — is replaced by the
/// **deepest passing node at or above `depth` on each of its root paths** through the passing
/// nodes, depth counted in passing nodes (`dag-hierarchies.md` §6). Where a parent and a child
/// both passed, that rule keeps the child at a deep cut and the parent at a shallow one; where
/// only the child passed, it keeps the child at every cut, since the parent occupies no rung.
///
/// **A node with no passing ancestor to climb to is a root of the viewer's tree and served where
/// it stands.** That is not a relaxation of the budget, it is the whole reason the climb is
/// expressed over *passing* nodes rather than over stored depths: a lineage's ancestors are
/// suppressed, deleted or below their own bar often enough that a depth-shaped climb would blank
/// the region under every one of them and call it a smaller map. Serving more than the budget
/// asked is a client's problem; serving nothing where the viewer is entitled to something is the
/// failure the budget exists around.
///
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
/// the viewer may see, and the ones it does not return are covered by an ancestor it did on at
/// least one path.
///
/// **On a tree the search is a bisection, which the count's monotonicity licenses.** A deeper cut
/// moves each frontier node's representative down its own lineage and never up, and two nodes
/// that shared a representative can only separate — so the served count is non-decreasing in
/// depth and the deepest depth that fits can be found in `log(depth)` evaluations rather than by
/// walking every depth. That property is worth stating because the bisection is wrong without it.
///
/// **On a DAG it is a scan, because the count is not monotone**: two parents at depth 1 are
/// replaced by their one shared child at depth 2, so the counts run 1, 2, 1. Every depth's count
/// is already in the plan, so the scan is one array read per depth over the graph's longest path
/// — 17 on MeSH — and *deepest that fits* stays the right knob, the depth-2 cut above being
/// strictly finer than the depth-1 cut and fitting where it did not (`dag-hierarchies.md` §6).
///
/// **A budget that not even the roots fit is served anyway**, at depth 0. The alternative is
/// dropping nodes to reach the number, which is the sampling decision 0083 forbids: a map missing
/// arbitrary roots claims those regions are empty.
/// Ascending and deduplicated, as [`cut_at`] is and for the same reason.
pub fn cut(lineage: &Lineage, passing: &[u32], budget: Option<u32>, prune: bool) -> Vec<u32> {
    if lineage.is_flat() {
        return ascending(passing);
    }
    let Some(budget) = budget else {
        return Plan::new(lineage, passing, prune).serve_at(u32::MAX);
    };
    // **The downward walk first, where it applies.** It answers in the top of the tree rather than
    // across the level; where its guard does not hold it declines and the sweep below runs
    // unchanged. Both return the same cut, which the tests assert against the obvious rule over
    // random trees rather than against each other.
    if let Some(served) = cut_top_down(lineage, passing, budget, prune) {
        return served;
    }
    let plan = Plan::new(lineage, passing, prune);
    if plan.count_at(u32::MAX) <= budget {
        return plan.serve_at(u32::MAX);
    }
    // **The search evaluates a count, not a cut.** Every depth's served total is already in the
    // plan, so either search is a handful of array reads and only the depth it settles on is ever
    // built.
    let best = if lineage.dag {
        (0..=plan.deepest())
            .rev()
            .find(|&d| plan.count_at(d) <= budget)
            .unwrap_or(0)
    } else {
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
        best
    };
    plan.serve_at(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

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

    /// The stored depth is the longest path from the root over every node of the level — the
    /// order the sweeps run in — and a node whose ancestors all failed still knows its own. What
    /// the cut serves at is a different number: see the withheld-node tests below.
    #[test]
    fn stored_depth_is_measured_over_every_node() {
        let lineage = chain();
        assert_eq!(lineage.depth(0), 0);
        assert_eq!(lineage.depth(1), 1);
        assert_eq!(lineage.depth(3), 2);
        assert_eq!(lineage.depth(4), 3);
    }

    // ---------------------------------------------------------------------------------------
    // A withheld node is not in the viewer's tree (decision 0117 E)
    // ---------------------------------------------------------------------------------------

    /// **The design's example.** `R → X → {C1, C2}` and `R → D`, with `X` withheld. Counted over
    /// every node the depths run 1, 2, 3 and a budget of 2 serves `{R, D}`; counted over the
    /// passing nodes they run 1, 3 and it serves `{R}` — which is what the world without `X`
    /// serves, so a viewer sweeping budgets learns nothing about `X`.
    #[test]
    fn a_withheld_node_occupies_no_rung() {
        const R: u32 = 0;
        const X: u32 = 1;
        const C1: u32 = 2;
        const C2: u32 = 3;
        const D: u32 = 4;
        let with_x = Lineage::new([
            (R, None),
            (X, Some(R)),
            (C1, Some(X)),
            (C2, Some(X)),
            (D, Some(R)),
        ]);
        let passing = [R, C1, C2, D];
        assert_eq!(cut(&with_x, &passing, Some(2), true), vec![R]);
        assert_eq!(cut_at(&with_x, &passing, 0, true), vec![R]);
        assert_eq!(cut_at(&with_x, &passing, 1, true), vec![C1, C2, D]);
        assert_eq!(cut_at(&with_x, &passing, 2, true), vec![C1, C2, D]);

        // The same, on the level where X never existed.
        let without_x = Lineage::new([(R, None), (C1, Some(R)), (C2, Some(R)), (D, Some(R))]);
        for budget in 1..=5u32 {
            assert_eq!(
                cut(&with_x, &passing, Some(budget), true),
                cut(&without_x, &passing, Some(budget), true),
                "budget {budget}"
            );
        }
    }

    /// A DAG widens the same channel two ways — a withheld sibling path, and a longest-path depth
    /// that counts withheld nodes on the longer path — and the rule closes both: `R → A → C` and
    /// `R → X → Y → C`, `X` and `Y` withheld, must cut exactly as the viewer's tree does, which
    /// is `R → A → C` with the withheld path contracted to the edge `R → C` — `R` being the
    /// nearest passing ancestor of `C` on that path, and containing everything `Y` did.
    #[test]
    fn a_withheld_path_does_not_deepen_a_dag_node() {
        const R: u32 = 0;
        const A: u32 = 1;
        const X: u32 = 2;
        const Y: u32 = 3;
        const C: u32 = 4;
        let dag = Lineage::dag([
            (R, vec![]),
            (A, vec![R]),
            (X, vec![R]),
            (Y, vec![X]),
            (C, vec![A, Y]),
        ]);
        assert_eq!(dag.depth(C), 3, "the stored depth is the longest path");
        let passing = [R, A, C];
        let contracted = Lineage::dag([(R, vec![]), (A, vec![R]), (C, vec![A, R])]);
        for depth in 0..4u32 {
            assert_eq!(
                cut_at(&dag, &passing, depth, true),
                cut_at(&contracted, &passing, depth, true),
                "depth {depth}"
            );
        }
        // C sits at rung 2 in the viewer's tree, not 3: a cut at 2 already reaches it. At 1 the
        // path through A picks A and the contracted path picks R.
        assert_eq!(cut_at(&dag, &passing, 2, true), vec![C]);
        assert_eq!(cut_at(&dag, &passing, 1, true), vec![R, A]);
        assert_eq!(cut_at(&dag, &passing, 0, true), vec![R]);
    }

    /// **The served set at every depth is the served set over the level with the withheld nodes
    /// removed** — the rule stated as an equality and swept over random trees and random DAGs,
    /// every depth and every budget, both pruning modes. The level without the withheld nodes is
    /// built with an edge wherever one passing node is the nearest passing ancestor of another on
    /// some path, which is the viewer's tree by its definition.
    #[test]
    fn the_cut_is_the_cut_over_the_level_without_the_withheld_nodes() {
        for (seed, dag) in [(0x0E17u64, false), (0xDA6Du64, true)] {
            let mut next = stream(seed);
            for case in 0..300 {
                let n = 1 + (next() % 40) as u32;
                let (lineage, parents) = random_lineage(&mut next, n, dag);
                let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
                if passing.is_empty() {
                    continue;
                }
                let is_passing = |o: u32| passing.binary_search(&o).is_ok();
                let induced = induced_parents(&parents, &is_passing);
                let pairs = passing
                    .iter()
                    .map(|&o| (o, induced[&o].iter().copied().collect::<Vec<u32>>()));
                let without = if dag {
                    Lineage::dag(pairs)
                } else {
                    Lineage::new(pairs)
                };
                for prune in [true, false] {
                    for depth in 0..8u32 {
                        assert_eq!(
                            cut_at(&lineage, &passing, depth, prune),
                            cut_at(&without, &passing, depth, prune),
                            "case {case} (dag {dag}, prune {prune}): depth {depth} differs from \
                             the level without the withheld nodes"
                        );
                    }
                    for budget in 1..=8u32 {
                        assert_eq!(
                            cut(&lineage, &passing, Some(budget), prune),
                            cut(&without, &passing, Some(budget), prune),
                            "case {case} (dag {dag}, prune {prune}): budget {budget} differs \
                             from the level without the withheld nodes"
                        );
                    }
                    assert_eq!(
                        cut(&lineage, &passing, None, prune),
                        cut(&without, &passing, None, prune),
                        "case {case} (dag {dag}, prune {prune}): the unbudgeted cuts differ"
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // The optimised plan against an obviously-correct reference
    // ---------------------------------------------------------------------------------------

    /// The parents each node has as a list, the form the reference reads.
    type Parents = BTreeMap<u32, Vec<u32>>;

    /// The nearest passing ancestors of every passing node, on every path: the edges of the
    /// viewer's tree. Recursion through withheld nodes, memoised over them.
    fn induced_parents(
        parents: &Parents,
        is_passing: &dyn Fn(u32) -> bool,
    ) -> BTreeMap<u32, BTreeSet<u32>> {
        fn above(
            node: u32,
            parents: &Parents,
            is_passing: &dyn Fn(u32) -> bool,
            memo: &mut BTreeMap<u32, BTreeSet<u32>>,
        ) -> BTreeSet<u32> {
            if let Some(known) = memo.get(&node) {
                return known.clone();
            }
            let mut out = BTreeSet::new();
            for &up in parents.get(&node).map_or(&[][..], Vec::as_slice) {
                if is_passing(up) {
                    out.insert(up);
                } else {
                    out.extend(above(up, parents, is_passing, memo));
                }
            }
            memo.insert(node, out.clone());
            out
        }
        let mut memo = BTreeMap::new();
        parents
            .keys()
            .copied()
            .filter(|&o| is_passing(o))
            .map(|o| {
                let set = above(o, parents, is_passing, &mut memo);
                (o, set)
            })
            .collect()
    }

    /// The cut, written the slow, obvious way, straight from `dag-hierarchies.md` §6: a frontier
    /// node is represented at depth *d* by the deepest passing node at or above *d* on **each of
    /// its root paths** through the passing nodes, depth counted in passing nodes, and the served
    /// set is the union. **It is the specification of what [`Plan`] computes**, and it exists
    /// because the fast form's structure — three sweeps in stored-depth order, a difference
    /// array, a search over counts — is far enough from the rule it implements that reading it is
    /// not a proof. Exponential in the paths, which is fine at forty nodes.
    fn reference_cut_at(parents: &Parents, passing: &[u32], depth: u32, prune: bool) -> Vec<u32> {
        let is_passing = |o: u32| passing.contains(&o);
        let induced = induced_parents(parents, &is_passing);
        // Rung: the longest path to the node through the passing nodes.
        fn rung(node: u32, induced: &BTreeMap<u32, BTreeSet<u32>>) -> u32 {
            induced[&node]
                .iter()
                .map(|&up| rung(up, induced) + 1)
                .max()
                .unwrap_or(0)
        }
        // Every root path of a node, as lists ending at the node.
        fn paths(node: u32, induced: &BTreeMap<u32, BTreeSet<u32>>) -> Vec<Vec<u32>> {
            let ups = &induced[&node];
            if ups.is_empty() {
                return vec![vec![node]];
            }
            let mut out = Vec::new();
            for &up in ups {
                for mut path in paths(up, induced) {
                    path.push(node);
                    out.push(path);
                }
            }
            out
        }
        let has_passing_descendant = |node: u32| {
            passing
                .iter()
                .any(|&o| o != node && induced_ancestors(o, &induced).contains(&node))
        };
        let heads: Vec<u32> = passing
            .iter()
            .copied()
            .filter(|&o| !prune || !has_passing_descendant(o))
            .collect();
        let mut served = BTreeSet::new();
        for head in heads {
            for path in paths(head, &induced) {
                let pick = path
                    .iter()
                    .copied()
                    .filter(|&o| rung(o, &induced) <= depth)
                    .max_by_key(|&o| rung(o, &induced))
                    .expect("a root sits at rung 0");
                served.insert(pick);
            }
        }
        served.into_iter().collect()
    }

    /// Every ancestor of `node` in the viewer's tree.
    fn induced_ancestors(node: u32, induced: &BTreeMap<u32, BTreeSet<u32>>) -> BTreeSet<u32> {
        let mut out = BTreeSet::new();
        let mut stack = vec![node];
        while let Some(at) = stack.pop() {
            for &up in &induced[&at] {
                if out.insert(up) {
                    stack.push(up);
                }
            }
        }
        out
    }

    /// The budget search, walked rather than bisected: the deepest depth whose count fits.
    fn reference_cut(
        parents: &Parents,
        passing: &[u32],
        budget: Option<u32>,
        prune: bool,
    ) -> Vec<u32> {
        let full = reference_cut_at(parents, passing, u32::MAX, prune);
        let Some(budget) = budget else {
            return full;
        };
        if full.len() as u32 <= budget {
            return full;
        }
        for depth in (0..passing.len() as u32).rev() {
            let candidate = reference_cut_at(parents, passing, depth, prune);
            if candidate.len() as u32 <= budget {
                return candidate;
            }
        }
        reference_cut_at(parents, passing, 0, prune)
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
    /// zero. With `dag`, a node may draw a second parent the same way, and the two may coincide —
    /// a duplicate edge is one edge.
    fn random_lineage(next: &mut impl FnMut() -> u64, n: u32, dag: bool) -> (Lineage, Parents) {
        let parents: Parents = (0..n)
            .map(|ordinal| {
                let mut ups = Vec::new();
                if ordinal > 0 && !next().is_multiple_of(4) {
                    ups.push((next() % ordinal as u64) as u32);
                    if dag && next().is_multiple_of(2) {
                        ups.push((next() % ordinal as u64) as u32);
                    }
                }
                ups.sort_unstable();
                ups.dedup();
                (ordinal, ups)
            })
            .collect();
        let pairs = parents.iter().map(|(&o, ups)| (o, ups.clone()));
        let lineage = if dag {
            Lineage::dag(pairs)
        } else {
            Lineage::new(pairs)
        };
        (lineage, parents)
    }

    /// **The fast plan and the obvious rule agree, over random trees, at every depth and every
    /// budget.** An optimisation that changed an answer would show up here rather than in the one
    /// hand-built tree the other tests use.
    #[test]
    fn the_plan_agrees_with_the_obvious_rule_over_random_trees() {
        let mut next = stream(0x5EED);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let (lineage, parents) = random_lineage(&mut next, n, false);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            for depth in 0..8u32 {
                assert_eq!(
                    cut_at(&lineage, &passing, depth, true),
                    reference_cut_at(&parents, &passing, depth, true),
                    "case {case}: the plan and the rule disagree at depth {depth}"
                );
            }
            for budget in 1..=8u32 {
                assert_eq!(
                    cut(&lineage, &passing, Some(budget), true),
                    reference_cut(&parents, &passing, Some(budget), true),
                    "case {case}: the bisection and the walk disagree at budget {budget}"
                );
            }
            assert_eq!(
                cut(&lineage, &passing, None, true),
                reference_cut(&parents, &passing, None, true),
                "case {case}: the unbudgeted cuts disagree"
            );
        }
    }

    /// **And over random DAGs**, where the rule is stated over every root path and the budget
    /// search reads every depth: the plan's one pass in stored-depth order against the path
    /// enumeration, both pruning modes.
    #[test]
    fn the_plan_agrees_with_the_obvious_rule_over_random_dags() {
        let mut next = stream(0xDA6);
        let mut multi_parented = 0usize;
        for case in 0..200 {
            let n = 1 + (next() % 30) as u32;
            let (lineage, parents) = random_lineage(&mut next, n, true);
            multi_parented += parents.values().filter(|ups| ups.len() > 1).count();
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            for prune in [true, false] {
                for depth in 0..8u32 {
                    assert_eq!(
                        cut_at(&lineage, &passing, depth, prune),
                        reference_cut_at(&parents, &passing, depth, prune),
                        "case {case} (prune {prune}): the plan and the rule disagree at depth {depth}"
                    );
                }
                for budget in 1..=8u32 {
                    assert_eq!(
                        cut(&lineage, &passing, Some(budget), prune),
                        reference_cut(&parents, &passing, Some(budget), prune),
                        "case {case} (prune {prune}): the scan and the walk disagree at budget {budget}"
                    );
                }
                assert_eq!(
                    cut(&lineage, &passing, None, prune),
                    reference_cut(&parents, &passing, None, prune),
                    "case {case} (prune {prune}): the unbudgeted cuts disagree"
                );
            }
        }
        assert!(
            multi_parented > 200,
            "only {multi_parented} nodes drew two parents, so the DAG cases are nearly trees"
        );
    }

    /// **On a DAG the served count is not monotone in depth** — two parents at depth 1 replaced by
    /// their one shared child at depth 2 — which is why the budget search reads every depth
    /// rather than bisecting. The design's counts, 1, 2, 1, on the design's shape.
    #[test]
    fn a_dag_budget_takes_the_deepest_depth_that_fits_by_reading_every_count() {
        // 0 → {1, 2} → 3
        let diamond = Lineage::dag([(0, vec![]), (1, vec![0]), (2, vec![0]), (3, vec![1, 2])]);
        let passing = [0, 1, 2, 3];
        assert_eq!(cut_at(&diamond, &passing, 0, true), vec![0]);
        assert_eq!(cut_at(&diamond, &passing, 1, true), vec![1, 2]);
        assert_eq!(cut_at(&diamond, &passing, 2, true), vec![3]);
        // A budget of 1 fits at depth 0 and at depth 2; the deepest that fits is 2, which is
        // strictly finer. A bisection over 1, 2, 1 could settle on 0.
        assert_eq!(cut(&diamond, &passing, Some(1), true), vec![3]);
        assert_eq!(cut(&diamond, &passing, Some(2), true), vec![3]);
        assert!(
            cut_top_down(&diamond, &passing, 2, true).is_none(),
            "the downward walk declines on a dag lineage"
        );
    }

    /// A duplicate edge is one edge and a self-edge is none (`dag-hierarchies.md` §4): neither
    /// changes a depth or a cut.
    #[test]
    fn a_duplicate_edge_is_one_edge_and_a_self_edge_is_none() {
        let clean = Lineage::dag([(0, vec![]), (1, vec![0]), (2, vec![0, 1])]);
        let noisy = Lineage::dag([(0, vec![0]), (1, vec![0, 0]), (2, vec![1, 0, 1])]);
        assert_eq!(noisy.edges, clean.edges);
        assert_eq!(noisy.parents_of(2), &[0, 1]);
        for o in 0..3 {
            assert_eq!(noisy.depth(o), clean.depth(o));
        }
        assert_eq!(
            cut(&noisy, &[0, 1, 2], Some(1), true),
            cut(&clean, &[0, 1, 2], Some(1), true)
        );
    }

    /// **The downward walk is actually taken**, which the tests above do not establish: they assert
    /// that `cut` answers correctly, and it would do that with the walk deleted.
    #[test]
    fn the_downward_walk_answers_where_every_artifact_passes() {
        let mut next = stream(0xC0FFEE);
        let mut taken = 0;
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let (lineage, parents) = random_lineage(&mut next, n, false);
            // Every artifact passes — the whole-corpus principal, and the case that bounds the
            // system, since a viewer who can see everything withholds nothing from the cut.
            let passing: Vec<u32> = (0..n).collect();
            for budget in 1..=8u32 {
                let walked = cut_top_down(&lineage, &passing, budget, true);
                if let Some(served) = walked {
                    taken += 1;
                    assert_eq!(
                        served,
                        reference_cut(&parents, &passing, Some(budget), true),
                        "case {case}: the walk and the rule disagree at budget {budget}"
                    );
                }
            }
        }
        assert!(
            taken > 100,
            "the walk declined almost everything ({taken} taken), so the agreement above is vacuous"
        );
    }

    /// **And it declines rather than answering differently.** A mask that fails artifacts near the
    /// top puts every node below them into the general rule — each may be a root of the viewer's
    /// tree — which the walk does not implement and must not guess at.
    #[test]
    fn the_downward_walk_declines_a_fragmented_mask() {
        let wide = Lineage::new([
            (0, None),
            (1, Some(0)),
            (2, Some(0)),
            (3, Some(1)),
            (4, Some(1)),
            (5, Some(2)),
            (6, Some(2)),
        ]);
        let parents: Parents = [
            (0, vec![]),
            (1, vec![0]),
            (2, vec![0]),
            (3, vec![1]),
            (4, vec![1]),
            (5, vec![2]),
            (6, vec![2]),
        ]
        .into_iter()
        .collect();
        // The root passes and one of its children does not, so a subtree below the failure has no
        // passing ancestor and is served where it stands at every cut.
        let fragmented = [0u32, 2, 3, 4, 5, 6];
        assert!(
            cut_top_down(&wide, &fragmented, 2, true).is_none(),
            "a failure above the cut must send the request to the sweep"
        );
        // And the answer that comes back is still the rule's.
        assert_eq!(
            cut(&wide, &fragmented, Some(2), true),
            reference_cut(&parents, &fragmented, Some(2), true)
        );

        // A failing **root** is the same story one level up.
        let no_root = [1u32, 3, 4];
        assert!(cut_top_down(&wide, &no_root, 2, true).is_none());
        assert_eq!(
            cut(&wide, &no_root, Some(2), true),
            reference_cut(&parents, &no_root, Some(2), true)
        );
    }

    /// The walk keeps a shallow leaf that a deeper cut has nothing to replace — the failure the
    /// first revision had, where each depth was swapped for the next wholesale and an unbalanced
    /// tree's short branches were dropped.
    #[test]
    fn the_downward_walk_keeps_a_leaf_shallower_than_the_cut() {
        // 0 ─┬─ 1 (a leaf at depth 1)
        //    └─ 2 ─┬─ 3
        //          └─ 4
        let lopsided = Lineage::new([
            (0, None),
            (1, Some(0)),
            (2, Some(0)),
            (3, Some(2)),
            (4, Some(2)),
        ]);
        let parents: Parents = [
            (0, vec![]),
            (1, vec![0]),
            (2, vec![0]),
            (3, vec![2]),
            (4, vec![2]),
        ]
        .into_iter()
        .collect();
        let all = [0u32, 1, 2, 3, 4];
        let served = cut_top_down(&lopsided, &all, 3, true).expect("every artifact passes");
        assert_eq!(
            served,
            vec![1, 3, 4],
            "the depth-1 leaf stays beside the depth-2 pair"
        );
        assert_eq!(served, reference_cut(&parents, &all, Some(3), true));
    }

    /// **On a tree the served count does not decrease with depth**, which is the property the
    /// budget's bisection rests on and is false for any rule that let a deeper cut merge two nodes
    /// into one. Asserted over random trees rather than argued; on a DAG it is false by the
    /// shape, which is the diamond test above.
    #[test]
    fn the_served_count_is_monotone_in_depth_on_a_tree() {
        let mut next = stream(0xC0FFEE);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let (lineage, _) = random_lineage(&mut next, n, false);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            let mut previous = 0;
            for depth in 0..10u32 {
                let count = cut_at(&lineage, &passing, depth, true).len();
                assert!(
                    count >= previous,
                    "case {case}: the count fell from {previous} to {count} at depth {depth}, so \
                     the budget's bisection would settle on the wrong cut"
                );
                previous = count;
            }
        }
    }

    /// Every artifact a cut serves is one that passed — the property that makes the whole module
    /// unable to disclose, whatever it gets wrong about which node to draw. Trees and DAGs alike.
    #[test]
    fn a_cut_never_serves_an_artifact_that_did_not_pass() {
        for dag in [false, true] {
            let mut next = stream(0xBEEF);
            for _ in 0..200 {
                let n = 1 + (next() % 40) as u32;
                let (lineage, _) = random_lineage(&mut next, n, dag);
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
                // And a cut is never empty while something passed: a blank map is the failure
                // mode the climb-to-a-passing-ancestor rule exists to prevent.
                assert!(!cut(&lineage, &passing, Some(1), true).is_empty());
            }
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

    /// The search from a leaf for the best-keyed ancestor-or-self visits every path of a DAG and
    /// picks by the key: the deepest, then the lowest identifier, as the membership column asks.
    #[test]
    fn select_ancestor_searches_every_path_and_picks_by_key() {
        // 0 → 1 → 3, 0 → 2 → 3; 4 a leaf under 3.
        let dag = Lineage::dag([
            (0, vec![]),
            (1, vec![0]),
            (2, vec![0]),
            (3, vec![1, 2]),
            (4, vec![3]),
        ]);
        let mut scratch = Vec::new();
        // Served: 1 and 2 at the same depth — the lower identifier wins; 4 is not served.
        let ids: BTreeMap<u32, u64> = [(1, 90), (2, 40), (0, 5)].into_iter().collect();
        let key = |o: u32| ids.get(&o).map(|&id| (dag.depth(o), std::cmp::Reverse(id)));
        let (picked, _) = dag.select_ancestor(4, &mut scratch, key).unwrap();
        assert_eq!(picked, 2, "deepest served, then the lowest identifier");
        assert_eq!(
            dag.select_ancestor(4, &mut scratch, |o| ids.get(&o).filter(|_| o == 0).copied()),
            Some((0, 5))
        );
        assert_eq!(dag.select_ancestor(4, &mut scratch, |_| None::<u32>), None);
        // Self is a candidate.
        assert_eq!(
            dag.select_ancestor(2, &mut scratch, key).map(|(o, _)| o),
            Some(2)
        );
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

    /// The unpruned cut is monotone in depth on a tree too, which the budget's bisection needs in
    /// both modes.
    #[test]
    fn the_unpruned_count_is_monotone_in_depth_on_a_tree() {
        let mut next = stream(0xD00D);
        for case in 0..200 {
            let n = 1 + (next() % 40) as u32;
            let (lineage, _) = random_lineage(&mut next, n, false);
            let passing: Vec<u32> = (0..n).filter(|_| !next().is_multiple_of(3)).collect();
            if passing.is_empty() {
                continue;
            }
            let mut previous = 0;
            for depth in 0..10u32 {
                let count = cut_at(&lineage, &passing, depth, false).len();
                assert!(
                    count >= previous,
                    "case {case}: the count fell at depth {depth}"
                );
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
        for dag in [false, true] {
            let mut next = stream(0xFEED);
            for case in 0..200 {
                let n = 1 + (next() % 40) as u32;
                let (lineage, _) = random_lineage(&mut next, n, dag);
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
                            "case {case} (dag {dag}): at depth {depth} pruning served {served}, \
                             which the unpruned cut did not"
                        );
                    }
                }
            }
        }
    }
}
