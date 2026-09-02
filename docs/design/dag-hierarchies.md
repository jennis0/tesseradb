# A child may name several parents — design

**Date:** 2026-09-01
**Status:** Normative (r3) for the `dag` hierarchy kind and for the rule that a withheld artifact
is not in the viewer's tree. Reviewed once under two lenses and ruled by decision 0117 (2026-09-01); the
rulings are folded into [`configuration.md`](configuration.md) (the `hierarchy` row and the kinds
table), [`artifacts-from-points.md`](artifacts-from-points.md) §4, [`annotations.md`](annotations.md)
§6 r9, [`contracts.md`](contracts.md) §3.2 r71 and `architecture.md` C29 r57. **Built 2026-09-01** on
the server side — the kind, the record, the build and ingest sides (branch `dag/store`), and the cut
over the viewer's tree, the membership column's rank and `parent_ids` on the wire (branch
`dag/engine`), both reviewed and merged; the TypeScript client follows on `dag/client`. ⊘ No
corpus declares the kind yet — rung 3's is the first, and `places/containment` the second.

## 1. What this answers

Rung 3 of the ingest campaign is MedCPT / PubMed with MeSH as its artifact layer
([`../ingest-campaign.md`](../ingest-campaign.md) §4). The owner ruled on 2026-09-01 that its
artifacts are keyed by **descriptor** and that a descriptor may name several parents, which makes
the layer a directed acyclic graph rather than a tree. Rung 1's unbuilt third layer,
`places/containment` from GeoNames' `hierarchy.txt`, has the same shape for the same reason: a
feature genuinely sits under two parents. Today both refuse — a child naming two parents is *the
data is not the tree the layer declared* (`artifacts-from-points.md` §4), and that refusal is right
for a tree and wrong for a graph that was declared as one.

The campaign tracker named two surface changes. **One of them dissolves on inspection** (§2), and
this document is the other.

## 2. Multi-membership needs no new surface

The tracker says a member source is one row per point, so no levelled kind can say *this article
is in ten concepts*. That is not what the surface says. `[layer.members]` is **one row per
`(artifact, entity)`** (`configuration.md` §8, `artifacts-from-points.md` §2), and the build's
`read_members` attaches each row to its artifact's membership with no per-entity uniqueness anywhere
on the path: a scalar key at level 0, ten rows for one article, is ten memberships. A membership is
a bitmap per artifact, so a point in ten of them is the point in ten bitmaps, which is what a flat
layer's multi-membership has always been. What the tracker was describing is the **point-source
list column** — one row per point whose list is read as a lineage under `nested` — and a lineage is
one path, not ten.

So rung 3's `prepare.py` **explodes** the `m` field into one row per `(article, descriptor)`, and
the reader takes it today under every kind. The only question the explosion leaves is the one §8
puts to the owner: whether the rows carry the explicit assignments only, or their ancestor closure.

Measured on chunk 18 (940,707 articles; `probes/2026-09-01-mesh-dag/`):

| | per indexed article, mean / median / max | rows, chunk 18 | rows, extrapolated to 3.6×10⁷ |
|---|---|---|---|
| explicit descriptors | 9.96 / 10 / 46 | 8,132,820 | ≈ 3.1×10⁸ |
| with ancestor closure | 55.58 / 54 / 240 | 45,375,723 | **≈ 1.7×10⁹** |

The closure is 5.58× the explicit rows rather than the 8× a mean ancestor set of 7 would give,
because an article's descriptors share ancestors. The extrapolation is from one chunk, and the
indexed fraction runs from 100% at chunk 0 to 37.5% at chunk 37, so the corpus figure is a weighted
average the newest chunks pull down. 29 articles of chunk 18 lose every descriptor to the vintage
mismatch and are in no artifact.

## 3. The declaration: `kind = "dag"`

```toml
[[layer]]
name       = "mesh/descriptors"
hierarchy  = { kind = "dag", prune_children = true }
```

A fifth value beside `flat`, `nested`, `stacked` and `tiered`. It is `nested` in every respect but
one: every artifact sits at level 0, `[[layer.levels]]` is refused, edges run within the level, the
edges are **roll-up** rather than information (decision 0087),
a list key column is read as a lineage, and a budget climbs the edges. The one difference is that
**a child may hold several parents**, and a second parent arriving for a child is recorded rather
than refused.

The four-kinds table gains a row:

| `kind` | Lineage | `[[layer.levels]]` |
|---|---|---|
| `dag` | a directed acyclic graph in the edges, every artifact at level 0 | **refused**, as for `nested` |

**Why a kind value and not a key on `nested`.** The kind is what every reader switches on — which
positions a list carries, which adjacencies are edges, what a build verifies, what a registry's
`parent_ref` accepts, whether a cut climbs — and it is declared and never inferred from the edges
(0087). A `parents = "several"` key beside `kind = "nested"` would be a second axis every one of
those readers has to consult, for a distinction that is a shape distinction: a tree and a DAG are
different structures, and the values name structures. It is one more value in an enum that already
has four.

**Why not a `tiered` variant.** A descriptor with several tree numbers sits at several depths —
`Respiratory Tract Neoplasms` is at depth 3 under one branch and depth 5 under another — and a
levelled kind places each artifact at exactly one level. The polyhierarchy is within a level or it
is nothing, which is why the DAG is `nested`'s cousin and not `tiered`'s. A tiered layer whose
data names two coarser parents stays refused: a ward in two districts is a data error on a ladder.

**Why not `polyhierarchy`.** The other values name shapes and this one would name a library
science term, which is the argument that renamed `administrative` to `tiered` (0087).

## 4. Edges: two spellings, both may name several

An edge has two spellings today, and both are kept: the `parent` column on an artifact row, and
the adjacency of a lineage list on a member row. Under `dag`:

- **`parent` on the artifact row may be a list** — `list<utf8>` or `list<int>` on the rule an
  integer key already takes — or a scalar, which is a list of one. A row's grain stays one row per
  artifact: the build's refusal of one key on two rows stands, so several parents are several
  entries in one cell rather than several rows.
- **A lineage naming a second parent for a child adds the edge, where the edge is being created.**
  This is the `two_parents` refusal — in the build's `record_lineage` and `apply_lineage`, in the
  ingest route's batch check and in `mint_records`' cross-window check — turned into an insertion
  when the layer's kind is `dag`, and left exactly as it is for `nested` and `tiered`. A point in ten
  concepts is ten member rows each carrying one lineage; each lineage is one path from a root, and
  the union of the paths is the graph. **Edges are settled at the publication that creates the
  artifact**, at both entry points: the build folds every row before applying, so it sees the whole
  graph, and a mint sees the batch and the window that mint it. A lineage naming a parent for a child
  the layer *already holds* is the growth rule below, on a DAG exactly as on a tree — so a
  list-spelled graph ingested in batches keeps only the edges each child arrived with, which is what
  a tree has always done and not a divergence [decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md)
  forbids. Rung 3 spells its edges on the roster's `parent` column, which is complete at
  publication, and that is the spelling to prefer for any DAG ingested rather than built.
- **A duplicate edge is one edge**, whichever spelling stated it and however many rows did.
- **A self-edge refuses, and so does a cycle.** The build has the check (`detect_cycles`, a
  single-parent chain walk that becomes a depth-first search over parent lists). **Ingest has none
  today** — the registry's `check_edge` accepts A→B and B→A minted in one batch, and the engine's
  depth walks then read the loop as depth 0 by an edge-count guard, a latent gap on a tree and the
  defining property on a DAG. So `mint_records` gains the check over the window's minted edges, and
  that set is sufficient: a growth never adds lineage, so an edge into an existing artifact cannot
  close a cycle, and every cycle is among the artifacts one window mints.
- **A parent must exist before an edge into it** (`annotation-representation.md` §5.0.4), for every
  parent: a batch minting a child under two new parents mints both first. **A growth adds members
  and never lineage** (`artifacts-from-points.md` §6) is unchanged, and applies per parent — a
  growth naming a parent an existing child does not hold is reported and the memberships still
  land, the edge not added.

Containment is verified per edge exactly as today: a child holding a member no parent of it holds is
**reported and published**, never refused. A concept need only be a subset of each of its parents,
and §8 is about whether rung 3's data makes it one.

## 5. Depth is the longest path from a root

A tree gives every node one depth. A DAG gives it one depth per path, and the cut needs one number.
**Depth is the longest path from any root**, computed in one topological pass where `Lineage::depths`
climbs today. Longest rather than shortest because it is the only choice under which **every edge
descends**: `depth(parent) < depth(child)` for each parent, so a cut at depth *d* that serves a
child never sits beneath an unserved parent shallower than it, and *deepest passing at or above d*
keeps its meaning. Under shortest-path depth a child reachable from a root in one step sits at
depth 1 while another of its parents sits at depth 7, and a cut at 3 serves the child and climbs
from it to a parent it would have to place below.

On MeSH the longest path is 17 where no tree number is deeper than 12, because a longest path hops
between a descriptor's positions; 47.5% of descriptors have a longest depth that differs from their
shortest, by up to 14 (§8's table). That is the shape, not a defect: a concept that is a leaf in one
branch and an interior node in another is deep by the branch that contains more.

**This is load-bearing for §6, not a preference.** The sweep's plan is computed in order of
decreasing depth as its reverse topological order, and that order is valid only because
longest-path depth increases strictly along every edge; under shortest-path depth a child can sit
above one of its parents and the pass reads it before its parent is settled.

`rung` on the wire is this depth recomputed over the response's own forest — the same definition a
treed layer has (contracts §3.2 r41), but a change in the code: `response_depth` is a memoised
single-parent chain and becomes a longest-path pass over the served links.

## 6. The cut on a DAG

**The rule is `annotations.md` §6's, stated over paths.** A frontier node — passing, with no passing
descendant — is represented at depth *d* by the deepest passing node at or above *d* on each of its
root paths, and the served set at *d* is the union over frontier nodes and paths. A node is served
at *d* iff it passes, its depth is at most *d*, and some downward path from it reaches a frontier
node through no passing node of depth at most *d*; a node with no passing ancestor on any path is a
fallback and is served at every depth shallower than its own. Every node the cut withholds is
covered by an ancestor it served **on at least one path**, which is the property a budget rests on:
a node with a passing ancestor on one path and none on another is not a fallback, and above its own
depth it is covered on the first path and by nothing on the second.

That is the tree rule with *its lineage* read as *each of its lineages*, and the sweep's `Plan`
computes it in one pass in reverse topological order: `until[n]` becomes the maximum over children
of the next passing node's depth rather than the value one child supplies. `Lineage::new` takes
`(ordinal, parent)` pairs and no kind, so it gains the parent lists and a flag the top-down walk
reads; the two tests asserting the served count is monotone in depth become tree-only tests. Under
`prune_children = false` every passing node is its own deepest entry, so the served set at *d* is
the passing nodes of depth at most *d* plus the fallbacks, unchanged.

**Two things change, and both are consequences of the shape rather than choices.**

- **The served count is not monotone in depth.** On a tree a deeper cut only separates nodes that
  shared a representative, so the count never falls and the deepest depth that fits a budget is
  found by bisection. On a DAG two parents at depth 1 are replaced by their one shared child at
  depth 2: counts 1, 2, 1. So `cut` takes the deepest depth whose count fits by **reading every
  depth's count**, which `Plan::counts` already holds for every depth. That is a scan over the graph's
  longest path — 13 on MeSH — where a bisection would have read four, over an array that is already
  in hand. *Deepest that fits* stays the right knob: the depth-2 cut above is strictly finer than
  the depth-1 cut and fits where it did not.
- **The top-down walk declines on a DAG.** `cut_top_down` reasons in depths and `children_of_node`,
  and a node with two parents is reached twice; it could be made to work and there is no case for
  it yet, so on a `dag` layer it returns `None` and the sweep answers. The sweep is the general
  route and was the only route until the walk was added for the ten-million-artifact level.

**A concept served under two parents is drawn twice, and that is the honest coarsening.** A cut at
depth 1 in the example above serves both parents, each of which contains the child's points, so the
child's region is inside two drawn hulls. That is what a polyhierarchy *is* — one concept at two
positions — and it is exactly the duplication the owner's ruling accepted at the client rather than
the build, where keying by tree number would have made it two artifacts with two counts and nothing
on the wire to say they are one thing. Here it is one artifact with one count wherever it appears.

**Nothing about a verdict or a count moves.** Every artifact is still tested on its own masked
count against its own criterion with no input from its lineage ([decision 0080](../decisions/0080-the-frontier-is-a-per-artifact-test.md)),
the number beside a served artifact is its own declared membership (`annotations.md` §3), and roll-up
is substitution of a parent for its children and never a sum (0087). A DAG adds edges and changes
none of those.

**A withheld node is not in the viewer's tree** (owner ruling, 2026-09-01). The structure a cut is
taken over is the **passing nodes**, with an edge wherever one passing node is the nearest passing
ancestor of another on some path; depth is counted in passing nodes; a node with no passing
ancestor on any path is a root of that tree. So the served set at every depth is identical to the
world in which the withheld node never existed, which is what C29 promises and what *an artifact
withheld is indistinguishable from one that never existed* means for the cut. Substitution stays
honest because containment is transitive: the nearest passing ancestor contains everything the
withheld node did.

**The implementation did not do this before 2026-09-01, on a tree either.** A lineage was built
from every record of the level, deliberately — *an ancestor that failed its own criterion is still an
ancestor* — so a withheld node still occupies a rung and the climb passes through it. Tree example:
R→X→{C1, C2} and R→D, X withheld, everything else passing. Counts by depth are 1, 2, 3 with X and
1, 3 without, so a budget of 2 serves {R, D} in one world and {R} in the other, and a viewer
sweeping budgets learns that a coarser artifact sits between R and C1 which they may not see. A DAG
widened it — a withheld *sibling path* moved the cut while the served parent was fully visible, and
longest-path depth counted withheld nodes on the longer path — which is how the review found it.
Under the rule above neither input exists. The fix is in the same pass, and is built: the plan is a
per-request sweep of the level against the viewer's passing set, and counting depth in passing
nodes is one more value in that pass, not a per-request structure; the per-generation lineage keeps
the parent lists and the child index, and its cached depth is only the sweep's order. Measured at
10⁷ treed artifacts, two thirds passing: the lineage build fell from 96 to 78 ms and the dense
budgeted sweep rose from 173 to 218 ms, the whole of the rise being the one extra shallowest-first
pass over passing nodes this rule requires. The
top-down walk is unaffected, running only where everything above the cut passes. Treating a
withheld node as a wall instead — its descendants unable to climb past it, served as fallbacks — was
considered and is wrong twice: the output still differs from the world without the node, and the
budget is exceeded.

**The membership column** (`membership:<layer>`, contracts §3.2 item 3) names the deepest served
artifact of the layer holding the point, **deepest by the response-local rung** — the review found
the stored depth would rank two incomparable served artifacts by withheld nodes between them, the
channel ruling E closes; the rung is computed over the served set before the column is resolved. On a tree that is unique; on a DAG and on a flat layer with
multi-membership two served artifacts may hold the point at one depth. The artifact-major route
iterates a `HashMap` of the served set and takes *the ordinal if it is deeper than what it holds*,
so **the tie is decided by hash order today, on a flat multi-membership layer as much as on a
DAG** — the rule is owed now and not only for this kind. **A tie is broken by the lowest
`tessera_id`** — the key the frame already carries, rather than an ordinal (C8) — and the row-major
route's `Lineage::nearest`, a single-chain climb, becomes a search over the served ancestor set for
the deepest, then the lowest identifier, so the two routes agree, which
`crates/tessera-engine/tests/membership_column.rs` asserts. It discloses nothing: both artifacts
are in the response and both hold the point.

## 7. The wire and the client

**`parent_id: uint64?` becomes `parent_ids: list<uint64>`** on the artifacts frame, for every layer
kind — empty for a root, for a flat artifact, and for a parent the response withheld. A tree serves a
list of at most one. C29's control is unchanged and applies per entry: **an entry is present only
where that parent is in the same response**, resolved against the response's membership after it is
settled, so a withheld parent is absent from the list identically to a root having none. A viewer
learns a relation between artifacts they were already served, and nothing about one they were not.
The entries are ascending by `tessera_id`, so a client that wants one parent takes the first and
gets the same one every time.

This is a rename and a type change on a frame column. **`api_version` does not move** (owner
ruling, 2026-09-01): nothing has launched, so the client is changed in the same commit and the
conformance comparator's schema with it. No shim: `parent_id` is not kept beside the list.

**The client** reads `parentIds: bigint[]` where it reads `parentId`. Four consumers:

- The artifact list (`artifact-list.ts`) builds a tree from parent links and **deduplicates by
  identifier today**, so under a DAG a node appears once, under whichever parent the walk reached
  first. ⊘ Whether it should instead appear under each served parent, and how the row says *also
  under X*, is the client-components work's, not this document's.
- The card's children are the served artifacts naming it among their parents.
- The colour lookup (`lut.ts`, `SessionArtifactTable.resolve`) walks parent links to the nearest
  colourable ancestor; it walks the first entry. Exact-only colouring (decision 0099) is unchanged: the
  point wears the colour of an artifact the wire said holds it.
- The response-local depth `explorer.ts` and the deck layer compute from links is the server's
  `rung`, as it already is.

**The bundle** persists one parent per artifact twice: as `ParentRef` in the WAL row (positional
under postcard) and as a hand-rolled tag-plus-level-plus-ordinal in the record blob
(`tessera_lifecycle::membership`, decision 0077), which is the encoding `BUNDLE_FORMAT` actually
guards. Both become lists, and `BUNDLE_FORMAT` moves from 4 to 5 so a stale local artifact refuses
rather than misreads. The engine's `ArtifactRecords::parents` and `Lineage::parent` become
CSR in the parent direction, beside the child direction that already is.

## 8. Rung 3's layer, and one question the data forces

Keyed by lowercased descriptor name; edges from `mtrees2025.bin`: D is a parent of E where a tree
number of E has as its dotted prefix a tree number D owns, deduplicated. Measured
(`probes/2026-09-01-mesh-dag/`, 2026-09-01):

| | |
|---|---|
| descriptors (nodes) | 30,954 |
| edges, deduplicated | 42,287 |
| roots | 110 — five of the 115 root tree positions belong to descriptors that also sit under another |
| descriptors with more than one parent | 9,286 (30.0%) |
| maximum parents | 6 |
| self-edges | 0 |
| cycles of length ≥ 2 | 0 — acyclic, by Tarjan over the whole graph |
| longest path (depth by §5) | 17; the deepest tree number is 12 |

**MeSH indexing assigns the most specific heading and not its ancestors.** An article indexed under
`Lung Neoplasms` is not indexed under `Neoplasms`, so with explicit assignments only, a parent's
declared membership does *not* contain its children's, and containment is reported violated on
essentially every edge. Two ways to build the layer:

- **(a) Close the membership upward in `prepare.py`**: each article's rows are its descriptors and
  every ancestor of each. Containment then holds by construction, roll-up substitutes a parent that
  genuinely contains its children, a child's masked count never exceeds its parent's, and the
  frontier's guarantee that a passing child never sits beneath a failing parent holds. The cost is
  rows — the second line of §2's table — and a parent whose members are the union of everything
  beneath it, which is what a subject heading's population *is*.
- **(b) Explicit assignments only.** Cheap, and honest about what the NLM wrote. Containment is
  reported violated throughout; roll-up serves `Neoplasms` in place of `Lung Neoplasms` with a
  count that excludes it, which is a count of a different thing; and an ancestor with few direct
  assignments fails a floor its children pass, so the cut shows leaves with no ancestor to climb
  to. No disclosure either way — every artifact passed its own test — but the map under a budget
  is misleading in exactly the way 0087 says substitution must not be.

**Recommendation: (a), knowing its price.** The rows are what a hierarchy whose edges mean what a
budget assumes costs. The price with the right denominators: ≈1.7×10⁹ membership entries against
the 5.07×10⁸ rung 2's spill took, 3.4×; no single artifact larger than rung 2's taxonomy root,
which held most of 7.4×10⁷ places, since a closed MeSH root cannot exceed the ≈3.1×10⁷ indexed
articles. The spill bounds only the build's peak; the term that grows without a bound is the
bundle's resident membership and the publish record, linear in entries. That is rung 3's scaling
finding rather than a reason to choose (b), and the campaign exists to have it; if it does not fit,
the fallback is (b) with the containment report stated beside every figure, not a smaller closure.

**The closure is emitted as a set per article.** Entries are counted, not deduplicated, so a
repeated `(article, ancestor)` row — which a naive walk over ten descriptors' shared ancestors
produces — would move a declared count. The probe deduplicated; `prepare.py` must.

`places/containment` at rung 1 is the same kind over `hierarchy.txt`, with the closure question
absent: a feature's containment membership is spatial and already nests.

## 9. Owner rulings sought

- **(A) The declaration.** `kind = "dag"` as §3. **Ruled 2026-09-01: yes.**
- **(B) Depth is the longest path** (§5), and the cut takes the deepest depth that fits by reading
  every depth's count (§6). **Ruled 2026-09-01: yes.**
- **(C) The wire.** `parent_ids: list<uint64>` replacing `parent_id` for every kind, the client
  changed in the same commit, `api_version` not moving (§7). **Ruled 2026-09-01: yes.**
- **(D) Rung 3's membership is the ancestor closure** (§8 option a). **Ruled 2026-09-01: yes.**
- **(E) A withheld node is not in the viewer's tree** (§6). **Ruled 2026-09-01**: the cut is taken
  over the passing nodes and the edges they induce, on a tree as on a DAG; the implementation is
  corrected rather than the channel registered.

## 10. Not in scope

- **Per-branch depth** — a budget resolving to different depths in different branches
  (`annotations.md` §6, ⊘ unspecified). Unchanged by this; a DAG does not make it harder or easier.
- **How a `nested` or `dag` layer bounds its response at low zoom** — the artifact-response-volume
  finding (`ingest-campaign.md` §6, an owner call outstanding). A DAG layer has no levels and so no
  zoom map, exactly as the Overture boundary layer has none.
- **The client's presentation of one artifact under several parents** — the components work's.
- **`tiered` with several parents.** Refused as today; nothing in either rung wants it.
