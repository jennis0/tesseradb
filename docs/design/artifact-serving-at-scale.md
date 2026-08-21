# Serving ten million artifacts — design and options

**Date:** 2026-08-21
**Status:** **Ruled, reviewed, and amended.** §9's question is answered by
[decision 0092](../decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md),
with [0093](../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md) and
[0094](../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
beside it. The adversarial review over this document and
[the selection surface](../evidence/memos/2026-08-21-artifact-layout-selection.md) has run and is
dispositioned — [the record](../evidence/memos/2026-08-21-artifact-serving-scale-review.md). One part
is built and gate-green — the cut rewrite (§6) — because it was a data-structure choice rather than a
design one. Everything else is measured and proposed, not implemented.

**Two things a reader must carry from the review.** The design had a **fail-open** in it: §4's
settled half claimed a question the structures 0093 kept do not answer, and the probe's routes did
not perform the missing test. It is amended below, and the routes that measured it were measuring
something cheaper than the design now specifies. **So every grid in §7 is superseded pending
re-measurement on the corrected probe**, and each affected table says so at its head; the ratios are
the shape of the answer and not a bound on it. The correction is fixed
[on its own track](../evidence/memos/2026-08-21-artifact-scale-plan.md).

Every figure comes from the shipped structures, measured by
[`probes/2026-08-20-artifact-serving-scale/`](../../probes/2026-08-20-artifact-serving-scale/README.md)
and [`artifact_cut_cost`](../../crates/tessera-bench/src/bin/artifact_cut_cost.rs); anything modelled
or derived says so at the claim. Extends [`annotations.md`](annotations.md) and
[`annotation-representation.md`](annotation-representation.md), which stay normative for the model.
**No option here changes what an artifact is, what is served, or what a client sees.**

## 1. The target, and the answer

**10⁷ artifacts over 10⁹ points, inside a second, on one core** — one core because a serving node
carries ten or more simultaneous viewers, so a budget met by spending the machine is not met at all.

**What the budget covers, stated before any figure is read.** Every number here is **one layer at one
level, verdict and cut only**, with the lineage held per generation. Excluded: the gather, the
record-blob reads for supplied content, the wire encoding, and the ~240 MB of `passing` tuples a wide
request allocates (§8.4). A request naming several layers pays each of them. So the figures bound the
part of the request this campaign changed, not the request.

**Yes for artifacts that are somewhere; for artifacts that are everywhere, only with a different
layout.** At the target the whole grid of principals and zooms runs **7.7–279 ms** (§7.2), against
~1 190 ms for one cell of it when this began, and every other request shape is one to
three orders of magnitude better than that. ⊘ Those grids are **superseded pending re-measurement**
— the routes that produced them omitted a test §4 now requires — so read them as the shape of the
answer and not as its value.

## 2. The one idea

The request path asks four `O(artifacts)` questions and **only one of them has the request in it**.
The design separates them by cadence and gives each the layout that suits it.

| question | mask? | viewport? | belongs |
|---|---|---|---|
| does this artifact have a visible member **in view**? | yes | yes | **request** |
| `\|membership ∩ M_auth\|` — the number served, and the criterion's input | yes | no | **token** |
| `\|G ∩ M\| == \|G\|` — containment | yes | no | **token** |
| what is this artifact's parent, and how deep is it? | no | no | **generation** |

Two consequences run through everything below.

**Cost should track what a viewer may see, and today it inverts.** At 10⁹ points a principal seeing
9.4% of the corpus costs the shipped path **1 635 ms** at whole-map zoom against 727 ms for one
seeing everything, because a narrow `M_auth` is a more fragmented row-space set and every pass is
`O(containers touched)`. ⊘ The pair once quoted here as this design's answer — 0.49 ms and 13.5 ms —
is **withdrawn as evidence for it**: both cells were measured on the **per-token** route that
[decision 0093](../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
deletes, with that route's 151 ms of session setup excluded from the figure, and the build-time route
that replaces it was never run at 10⁶ artifacts. The **direction** is what survives, and it survives
on §7.2's grid rather than on this pair: the narrow principal becomes the cheap one, because fewer
artifacts survive the term partition. Re-measurement is queued.

**The cut cannot help, and does not need to.** It runs after the verdicts by construction — the
verdict is a per-artifact question with no lineage input
([decision 0080](../decisions/0080-the-frontier-is-a-per-artifact-test.md)) — so `artifact_budget`
bounds what is *served* and never what is *evaluated*. Nothing here changes that.

## 3. What is stored

**At the generation move**, beside the row form that already exists:

- **A hierarchical row-range index.** Rows are Morton rank, so the client's tile hierarchy *is* a
  hierarchy of row ranges. Each node holds the artifacts whose whole membership lies inside it, plus
  a subtree roll-up. **4.2 MB** at 10⁷ artifacts over 10⁸ points, 25.8 MB at 10⁶ over 10⁹ and 42.3 MB
  at 10⁷ over 10⁹; **0.2–1.2 s to build** — beside a projection of the same population that costs
  147.8 s at 10⁸ points and 282–298 s at 10⁹. ⊘ *An earlier revision said 3.2 s and 138 s; neither
  appears in a committed log, and the figures here are read from `data/*.log`.* A third set, the
  artifacts too wide for any node, is held beside the tree: see **`everywhere`** in §4.
- **A per-artifact extent**, `(min_row, max_row)` — 8 bytes, **80 MB** at 10⁷.
- **The lineage, with depth** — see §6. Mask-independent, and today rebuilt per request.
- **For a layer that partitions**, a **row-addressed label column** *instead of* the per-artifact
  bitmaps (§5).

- **A containment partition** — per `(artifact, rank)`, an interned identifier naming the boolean
  expression over *terms* that decides that rank, plus a bitmap per distinct expression. It names no
  principal, so it is shared by every token. This is what replaces §8.5's per-token servable-label
  set; see §4.2 for its grain, its key and what it costs.

**Per token: nothing sized by the artifact population**
([0093](../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)), with
the one exception that ruling names — a masked-count histogram for a **row-major** layer, which is
sized by that layer's artifact count and has no other route to the whole-membership count (§5.1).
A layer declaring an existence criterion is the case that does not fully dissolve; see §4.3.

## 4. The request

1. **Walk the index top-down.** Take a whole subtree where the viewport covers a node; descend only
   where it cuts one. Cost is the viewport's *perimeter*, not the population. Out come `settled`,
   `open`, and — unconditionally into `open` — the `everywhere` set of artifacts too wide for any
   node.
2. **Compose the viewport with the mask once**, into `here = viewport ∩ M_auth`, and ask every
   candidate the same question against it: one early-exiting `Bitmap::intersect`, which stops at the
   first container that meets. For a **settled** artifact — one whose `membership ⊆ viewport` —
   that probe *is* `masked_count > 0`, exactly, because intersecting with the viewport removes
   nothing from its membership. That collapse is what containment inside a covered node buys: not a
   test skipped, but one probe answering both the request's question and the layer-wide one a
   criterion and a count need. **What the walk itself buys is the population it never probes** — an
   artifact in no node the viewport touches has no member there, so skipping it withholds nothing.
   See §4's closing note on what the blocks-per-artifact statistic does and does not say about how
   large that saving is.
3. **The `open` half takes the extent test first** (`viewport ⊇ [min,max]`, one `contains_range`),
   which settles an artifact the node walk handed back on an alignment boundary; every artifact still
   takes the probe in step 2. This is the viewport's edge, and only its edge.
4. **Containment is a lookup** against the build-time partition (§4.2), not a per-request
   `satisfied_rank` over the composed mask.
5. **The artifact's own suppression still runs live**, unconditionally — verdict step 1, and nothing
   caches above it.

**⊘ The settled half once claimed more than this, and that claim was fail-open.** An earlier
revision said a settled artifact was *"done"* on containment alone, because *"the token structure
answered"* whether it had a visible member — the servable-label set 0093 then deleted. What replaced
that structure answers `G ⊆ M_auth`, which is a question about the artifact's **generating set**, and
never `|membership ∩ M_auth| > 0`, which is a question about its **members**: an artifact all of
whose members are outside `M_auth` passes containment and would have been served, disclosing that a
grouping exists where the viewer can see nothing of it. The probe's grouped and hoisted routes
(G and H) contained the same omission and are what §7's grids measured; the equality assertion that
would have caught it covered an earlier route which did perform the test, and the fixture's 32-way
alignment of masks, generating sets and signature groups made the two sets coincide, so nothing
disagreed. The design above is the correction; the figures are superseded until the probe carries it.

**What "1.0 row blocks per artifact" says, and what it does not.** The statistic counts the
**65 536-row containers** an artifact's membership touches, while the index's finest node is
**1 024 rows** — a factor of sixty-four apart, so a layer at 1.0 containers per artifact may still
straddle a node boundary and be promoted. It is also imposed rather than observed: the `runs` arm
lays artifacts out at a stride that makes each one contiguous by construction. It therefore says the
membership is **local**, which is the property the index needs, and it does **not** say what fraction
of a population a given viewport settles. That fraction is what the walk returns per request, and no
recorded run reports it.

### 4.1 Why this is a candidate generator and not a decision

`MaskedSet::intersects_set` records that an early draft served an artifact *"wherever the box
intersected the viewport"*, which discloses the unmasked extent by panning. What is refused there is
using unmasked geometry as **the answer**. This uses it two ways and neither is that:

- **as a superset filter** — an artifact outside every node the viewport touches has no member
  there, so it cannot have a visible one. Skipping it withholds nothing.
- **as the collapse that makes one probe exact for two questions** — where `membership ⊆ viewport`,
  `membership ∩ (viewport ∩ M_auth)` and `membership ∩ M_auth` are the same set, so the request-shaped
  probe of §4 step 2 answers the mask-shaped question exactly. Containment in a covered node is a
  fact about **cost**, never an answer: every artifact served still cleared a masked test.

The geometry decides only *which question to ask*. The probe asserts the served set is identical to
the shipped loop's, ordinal for ordinal, at every mask and every zoom — and, for the row-major route,
count for count as well. ⊘ **That assertion did not cover the routes §7's grids were measured on**;
it is the gap §4 records, and closing it is the first item of the corrected probe.

### 4.2 Containment does not need a per-token structure at all

**There are a great many tokens** (owner, 2026-08-21), so anything materialised per token over the
artifact population is the wrong shape however cheap one copy is. `architecture.md` §8.5 specifies a
servable-label set on that cadence; **for containment it is not needed**, and the design says why in
`annotations.md` §4 already:

> what decides is **which** terms, never *how many* items — which is why terms are what make the
> test tractable (**I5**)

Containment is `G ⊆ M_auth`, and an entity is in `M_auth` exactly when its own visibility expression
holds for the principal's terms. So

```text
G ⊆ M_auth   ⟺   ( ⋀ vis(e) for e in G )( T )
```

— a boolean expression over terms with **nothing about the mask in it**. Compose it once at build,
canonicalise, intern: artifacts sharing an expression share an answer for every principal that will
ever exist.

**The grain is `(artifact, rank)`, and the answer is a rank rather than a boolean.** An artifact
holds **ranked contents** and is served the first entry whose generating set the viewer contains
entire ([decisions 0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)
and [0078](../decisions/0078-the-service-takes-no-opinion-on-which-variation.md)) — which
is what `satisfied_rank` returns and what the request needs. So the partition holds one expression
identifier per `(artifact, rank)`, and a request resolves it by taking the **lowest** rank whose
expression the principal satisfies. Storage multiplies by the mean content count, which is a property
of the layer and is not one in the general case.

**How many distinct expressions there are is unmeasured, and the fixture's thirty-two was an axiom
rather than a result.** The probe plants 32 signature groups and draws every generating set from
inside one, so 32 is what it can produce. The claim that the count stays small rests on generating
sets being authored **per term**, which [`annotations.md`](annotations.md) §8.1 shows as a variant an
author may adopt against label creep — *"both viewers fail the same label and both satisfy its
per-term variant"* — and not as a rule the service imposes. Nothing stops an author declaring a
generating set spanning many terms, and the measured corpus holds **54,791 distinct permission
signatures over 2.42M items** (`probes/phase0-memo.md` §2.3), which is the order the expression count
is bounded by from above rather than below. Consequences taken here: the expression identifier is
**at least a `u16`**, never a byte; and the storage figures below are the fixture's, not the design's.

⊘ **Measured at 32 synthetic expressions. The distinct-expression count over a real generating-set
population is the queued measurement**, and it is what decides both the identifier width and whether
the per-request union stays cheap.

**Its cadence and its key.** The partition is keyed `(prefix, view, store_version)` — the same
identity `ProjectionKey` carries — and is rebuilt where row forms are: in the fold's artifact pass,
and at a store-version bump. Two consequences follow from that and neither is optional. `G` **shrinks
at every fold on a permissive layer**, so the expression an artifact resolves to changes there
(`annotation-write-cycle.md` §3.4, row 2). And the projection-loss flag — a generating set that lost
members in projection can never be contained — is per view and mask-independent, so it folds into the
build rather than being asked per request. ⊘ **What the partition costs to build is unmeasured.**
The probe interned 32 expressions in 0.1 s over ten million artifacts; that is the fixture's
expression count, not a build cost for a real one.

Per request the answer is either a union of the satisfied expressions' bitmaps, or a lookup per
candidate, whichever the viewport has left smaller. **Measured against the per-token route at 10⁷
artifacts, full mask** — ⊘ *unrecorded earlier revision, re-measurement queued: these cells appear in
no committed run log, and the routes they compare both omit §4's masked test:*

| viewport | shipped | per-token *(151 ms setup, excluded)* | **build-time groups *(setup 0.1 s at 32 expressions)*** |
|---|---:|---:|---:|
| whole map | 2 929 ms | 141.9 ms | **140.0 ms** |
| 6.25% | 891 ms | 10.1 ms | **9.87 ms** |
| 0.39% | 692 ms | 0.845 ms | **0.869 ms** |
| 0.024% | 666 ms | 0.186 ms | **0.221 ms** |

**Parity, with the per-token structure deleted.** And at a narrower principal it is ahead outright —
a 9.4% mask at whole-map zoom is **4.09 ms against 13.1 ms** — because the expressions a principal
fails are never touched, where the per-token pass had to evaluate every artifact once to find that
out. The build-time route's setup is small but is **not** nothing, which the header of an earlier
revision of this table claimed.

Storage is the identifier column plus the expression bitmaps: at the 32-expression fixture, **30 MB
of bitmaps and 10 MB of identifiers, so 40 MB at 10⁷** — ⊘ superseded, since the identifier is at
least two bytes and the column is per `(artifact, rank)` rather than per artifact. It names no
principal, so one copy serves every token however many there are, which is the property that does not
depend on the count.

**Two forms of the same fact, and the route picks between them on size**, exactly as §5's two layouts
do: unioning the satisfied expressions is `O(containers)` and independent of the viewport, ~6 ms at
10⁷; testing each candidate through the column is `O(candidates)`, which at a 0.024% viewport is
three hundred of them. Taking the union unconditionally cost 626 µs at a viewport whose answer was
73 µs. The probe switches at `settled + open > max(rows / 64, 4096)`; that constant is a measured
choice and belongs in the design when this is built (selection memo §9).

⊘ **The deny correction is the acceptance test, not a refinement.** `denied = deleted ∪ suppressed`,
both of them: a deletion and a suppression each remove a member of `G` from `M_auth` whatever the
terms say, so a partition consulted alone is **fail-open for exactly the case the write cycle exists
to make safe**. The correction is evaluated **live against the overlay per request**, or applied
synchronously with the deny's acknowledgement — a refresh on any other cadence is fail-open for the
length of its window, and `annotation-write-cycle.md` §3.4 puts containment's response to a deny at
**accept**. Two further properties: an **unsuppress re-derives** rather than subtracts, because
delete → suppress → unsuppress must leave the entity deleted, and a subtraction would restore it; and
the structure this needs is an inverted index from **entity to the artifacts whose generating set
holds it**, sized by `Σ|G|`, which joins the storage tables above. That index is in tension with
`annotation-write-cycle.md` §4.5, which states the deny lane does **no** artifact work precisely to
avoid an inverted lookup on the accept path. Recorded, not resolved: reconciling the two is the build
wave's, and it is the largest open question this section leaves.

### 4.3 The masked count, which is the part that does not fully dissolve

`|membership ∩ M_auth|` is genuinely per-principal. It splits by what the layer declares:

- **No existence criterion** — the count is only the number *beside* a served artifact, so it is
  needed for what survives the cut. `O(budget)`: measured at **22–205 µs** for a thousand artifacts,
  against 596 ms for ten million. Solved — ⊘ *at a thousand.* §7.2's count row is priced at that
  budget while the fixture it was measured on serves up to 2.4 million artifacts at a shallow cut, so
  the row understates what such a request pays; §7.3's nested arm is the honest pricing of the same
  quantity.
- **An existence criterion** — the count decides whether the artifact exists for this viewer, so it
  is needed per candidate. The viewport bounds that: **0.2 ms at a 0.024% viewport and 2.8 ms at
  0.39%** over 10⁷ artifacts, but **596 ms at whole-map zoom**.

So one case remains: **a criterion layer at a wide viewport**. Three ways out, and the third is the
one worth ruling on:

- Hold the counts per token after all — which is what §8.5 buys, and what many tokens make
  expensive. It is now the *only* thing that structure would be for, and
  [0093](../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md) closes
  it for an artifact-major layer. **The one exception that ruling names is a row-major layer**, whose
  count has no other route: see §5.1.
- Keep, per artifact, a count **per signature group** — build-time and mask-independent, summed over
  the satisfied groups per request. ⊘ Not measured, and the storage is real: an artifact spanning
  thirty-two groups is ~200 bytes, so ~2 GB at 10⁷ — and thirty-two is the fixture's number, not a
  corpus's (§4.2).
- **Evaluate only the levels the request can serve from.** A whole-map request runs every level of a
  treed layer when the client can draw the coarse one; bounded to what it can serve, the wide case is
  ~10³ artifacts and the question does not arise. This changes what is *evaluated* and not what is
  served, but it reaches the request contract. ⊘ Not measured.

### 4.4 Three things the construction needs to work at all

- **A hierarchy, not a granularity.** A flat index at 65 536-row blocks is worth nothing at mid-zoom:
  the viewport is then made of tiles smaller than the block, so no block is ever fully covered and
  nothing is settled. The tile-to-block relationship moves with corpus size, so any fixed block is
  wrong at most scales.
- **The extent beside the tree.** Node boundaries are powers of two, so an artifact at an arbitrary
  offset straddles one and is promoted a level, whose node the viewport must cover sixteen times as
  much of. Without the extent the `regions` arm gets nothing at a 6.25% viewport — measured.
- **An allocation-free walk.** Building a `Bitmap` per node merely to ask whether the viewport meets
  it costs nothing at 10⁸ rows and **5×** at 10⁹, where the hierarchy is two levels deeper.
  `range_cardinality` answers both the disjoint and the covered question from one call.

⊘ **The hierarchy's own constants are the probe's**, and the first bullet's argument applies to them:
a finest node of 1 024 rows and four bits per level are what
`crates/tessera-bench/src/bin/artifact_serving_scale.rs` chose, measured at nothing else, and a fixed
granularity is exactly what that bullet says is wrong at most scales. They are carried into the build
wave's constraint list ([the selection surface](../evidence/memos/2026-08-21-artifact-layout-selection.md)
§9) rather than settled here.

## 5. Three layouts, chosen by locality

`annotation-representation.md` §2.0 names three membership *sources*. The axis that decides cost is
a different one and does not line up with them: **row-space locality**.

| shape | what has it | blocks/artifact | §3's index |
|---|---|---:|---|
| clustered | HDBSCAN, point-and-radius, a hierarchy level | **1.0** | yes, entirely |
| regional | administrative boundary, spatial predicate | **1.0–1.6** | yes, via the extent |
| scattered | attribute predicate, per-analyst selection, term-as-artifact | **96.8** | **no** |

**"Large" and "numerous" are mutually exclusive**, which is why the middle row is not a third
problem: a boundary set measures 1.6 blocks per artifact at 10⁴ and 1.0 at 10⁶, because regions must
shrink as they multiply to keep fitting the same map.

A scattered artifact touches every node, so it is never inside one and never outside one — the walk
returns it in the `everywhere` set every time, whatever the viewport. Under §3's structures such a
layer walls at ~**2×10⁵** artifacts — against 10⁷ clustered ones in the same budget.

⊘ **The axis has two arm-imposed endpoints and nothing between them.** Every recorded run sits at
1.0–1.6 blocks per artifact or at 10.0–96.8; both ends are what the `runs`/`regions` and `scattered`
generators construct rather than what a corpus was observed to do, and **no measurement exists
between 1.6 and 10**. So the axis is the right one — it is what separates the two costs by two
decades — and the *threshold* on it is not yet a number. The automatic pick's threshold in
[the selection surface](../evidence/memos/2026-08-21-artifact-layout-selection.md) §3 stays ⊘ until a
sweep brackets it.

### 5.1 The inversion that removes the wall

The scattered shapes have a property clusters do not: **a single-valued attribute predicate
partitions the corpus.** Every point carries exactly one value, so the memberships are disjoint and
the natural storage is one label per **row**, not one bitmap per artifact. Candidacy becomes one scan
of `viewport ∩ M_auth` marking labels; the count becomes one histogram over `M_auth`. Both cost
**points rather than artifacts**.

**The count is the one place 0093 admits a per-session structure**, and it is admitted because a
row-major layer has no other route to it. An artifact-major layer answers `|membership ∩ M_auth|` for
one artifact at a time, so a budget bounds the work; a row-major layer has no per-artifact membership
to intersect, and its only route is the histogram — which prices every artifact in the layer whether
the request needs one or all. Holding that histogram per `(session, layer)` is ~4 B per artifact,
**4 MB at 10⁶ and 40 MB at 10⁷**, byte-budgeted exactly as the row-projection cache is and refreshed
on the session-geometry cadence (owner ruling, 2026-08-21, recorded in 0093). Nothing else per token
is sized by the artifact population.

Measured over 10⁸ points, on the `partition` arm — the run logs are
`data/r1e8-p1e3.csv` and `r1e8-p1e4.csv`. The point is the columns, not the rows:

| viewport | artifacts | shipped | artifact-major | **row-major** |
|---|---:|---:|---:|---:|
| whole map | 10³ | 376 ms | **1.9 ms** | 333 ms |
| whole map | 10⁴ | 2 320 ms | **15.5 ms** | 332 ms |
| 6.25% | 10³ | 102 ms | 26.1 ms | **23.4 ms** |
| 6.25% | 10⁴ | 448 ms | 193 ms | **23.3 ms** |
| 0.39% | 10³ | 79.0 ms | 2.06 ms | **1.48 ms** |
| 0.39% | 10⁴ | 271 ms | 21.2 ms | **1.56 ms** |

**Ten times the artifacts and the row-major route does not move.** Its cost is ~4–5 ns per visible
row and nothing else — ⊘ ~280 ms *(modelled from that constant)* for a 6.25% viewport over 10⁹
points, however many artifacts the layer holds. The two layouts cross where you would want: row-major
is dearest at whole-map zoom, exactly where the extent test needs no scan at all. So the rule is
**take the cheaper**, and both are exact.

**The stronger argument is not speed.** At the target the artifact-major form does not fit. From the
residency campaign's measured 78.5 B per container on scattered membership, over 10⁹ rows:

| scattered artifacts | members each | artifact-major | row-major |
|---:|---:|---:|---:|
| 10⁴ | 10⁵ | 12.0 GB | **4.0 GB** |
| 10⁵ | 10⁴ | **78.5 GB** | **4.0 GB** |
| 10⁶ | 10³ | **78.5 GB** | **4.0 GB** |

⊘ Derived from the measured constant, not measured at 10⁹ — and consistent with what that campaign
saw directly, where its scattered arm at 10⁷ artifacts was OOM-killed rather than slow. The row-major
form is one `u32` per row whatever the artifact count, narrower at the `u8`/`u16` widths
`configuration.md` §1 already declares, and a mappable array rather than anonymous allocation. The
**list** form is not one `u32` per row: it holds `k` of them on average, and the recorded 4.4 GB is at
**`k ≈ 0.1`** — 10⁸ memberships over 10⁹ rows — with ⊘ ~8 GB at `k = 1` *(derived)*. So §7.2's
residency line is a figure about the fixture's `k`, not a property of the layout.

**Not a new mechanism.** `artifacts-from-points` already *reads* this layout — an integer key column,
or a list column naming the artifacts a point belongs to — and converts it into artifact-major
bitmaps on the way in. The proposal is to keep what the build was handed, **row-addressed** rather
than entity-addressed: `attrs/` holds it by entity, which is right for a filter and wrong for a
viewport, and reaching the entity form costs an `entity_of` per row — ~120 ms of Stage 6's 175 ms was
that inversion rather than the counting.

⊘ **Single-valued only.** An overlapping layer needs a list per row, which is the same inversion at a
larger constant and is not measured.

## 6. The cut — built, not proposed

A principal who can see the whole corpus passes **every** artifact, so the cut is handed all 10⁷.
`artifact_cut_cost` had never run above 10⁶, where its own note said that scale "is not an operating
point this system serves"; that is false for exactly this principal.

**Two arms, and their figures are not interchangeable.** The probe runs a **treed** arm — a balanced
three-way tree with a share of the level passing — and a **flat** arm with no edges at all, which is
every layer published before Stage 5. Each table below names its arm and the run it comes from,
because an earlier revision of this section put three rows from two different profiles in one table
and read them as a progression.

**The treed arm, at a whole-corpus principal** — the three profiles in order, each `10⁷` artifacts,
budgeted, single-threaded:

| profile | lineage | cut, budgeted | peak RSS |
|---|---:|---:|---:|
| as planned — one lineage per frontier node | 44.9 ms | **1 008 ms** | 1 078 MB |
| intervals per servable node | 87.0 ms | **188 ms** | 470 MB |
| the downward walk, warm | 96 ms | **3.05 ms** | — |

**The flat arm**, same level size and the same runs: 6.1 ms of lineage and **28 ms** of cut,
unchanged by any of the three — it has no tree to walk down, and the sweep is its only route.

**The change that produced the 3 ms is the downward walk**, and it is worth stating before the two
that came earlier, because it is the one that decides what bounds the system. A budget settles on a
**shallow** depth — a thousand artifacts is depth six in a three-way tree — so the answer lives in the
top of the tree, while every revision before this swept the whole level to find it. Walking down from
the roots and stopping at the first depth the budget cannot hold touches `O(budget × branching)`
nodes. With every node above the cut passing the rule collapses exactly: a node above the cut has a
passing child so something deeper represents it; a node *at* it has none, so nothing does; and every
node has a passing parent, so none is its lineage's fallback. The served set is the nodes at that
depth plus the leaves shallower than it. **Where a node above the cut fails, the walk declines** and
the sweep answers at the cost it always had — §7.1 says why that residual does not bound throughput.

The two earlier profiles are what made 1 008 ms into 188. The plan materialised a lineage per frontier
node: ~4.4 million of them, ~15 deep and almost entirely shared, so 67 million entries and half a
gigabyte to answer for a few thousand — instrumentation put **852 of the original 975 ms** there,
against 12 ms for the frontier walk and 2 ms for the side tables, and flattening them into one buffer
was worth 975 → 440 ms on its own. **The RSS pair belongs to that flattening**, not to the downward
walk. What replaced them follows from one observation: as the cut deepens, each lineage's pick walks
*down* it, so a node is the pick over one contiguous range of depths and never again, and the union
across the lineages sharing a node is an interval because they share its lower end. So the plan is one
`[from, until)` per servable node; every depth's served count falls out of a difference array, and the
served set emerges ascending, so nothing sorts.

Three smaller changes went with it: `on_chain` is `climbed ∪ passing`, so the second climb over the
spine disappeared; the passing set is borrowed where it already arrives ascending, which the serving
path always produces; and **depth moved to `Lineage`**, which is why that column rose as the cut fell
— the work did not grow, it moved to the object that should be held per generation.

**Serving exactly what it served before**, on both routes — checked against the reference
implementation the module is already tested against over random trees, and with a test asserting the
walk is actually taken rather than silently declining everywhere.

## 7. The numbers

**Every grid in this section is superseded pending re-measurement on the corrected probe** (§4): the
routes that produced them settle an artifact on containment without the masked probe §4 step 2 now
requires. What follows is kept because the *shape* — which request is dear, which axis moves it — is
what the design is argued from, and because a corrected run should be read against a recorded
predecessor rather than against nothing.

**Clustered, 10⁷ artifacts, single-threaded, best of three:**

| viewport | shipped | design | |
|---|---:|---:|---:|
| whole map | 2 988 ms | **31.7 ms** | 94× |
| 6.25% | 904 ms | **8.2 ms** | 111× |
| 0.39% | 725 ms | **0.96 ms** | 755× |
| 0.024% | 713 ms | **0.28 ms** | 2 547× |

Those are the **build-time-groups** route, so nothing above is amortised over a session: every row is
what one request costs on a cold token.

⊘ **The "flat in the corpus size" table is withdrawn as evidence for this design.** It compared the
**per-token** route — the one 0093 deletes — at 10⁶ artifacts over 10⁸ and 10⁹ points, with that
route's per-session setup excluded from every cell, and the build-time route that replaces it has
**never been run at 10⁶ artifacts**. The property it claimed may well hold; nothing here measures it.
Re-measurement is queued. The cells, kept so the corrected run has a predecessor: 12.9 / 1.92 / 0.45 /
0.24 ms over 10⁸ points and 13.5 / 2.05 / 0.42 / 0.18 ms over 10⁹, at whole map / 6.25% / 0.39% /
0.024%, against a shipped path of 727 / 148 / 101 / 94.8 ms at 10⁹.

**The worst request end to end** — whole corpus, whole map, treed 10⁷-artifact layer:

| | before | now |
|---|---:|---:|
| verdict pass | ~140 ms | **~32 ms** |
| lineage build *(belongs per generation)* | 45 ms | 87 ms |
| cut | ~1 010 ms | **3.05 ms** |
| **total** | **~1 195 ms** | **~131 ms, or ~35 ms once the lineage is cached** |

**10⁷ artifacts over 10⁹ points is measured directly** — §7.2 — **at 25.3 GB peak on the three grid
runs** (`grid`, `grid2`, `grid3`) and 19.8 GB on the end-to-end run that swept principal coverage
instead of viewport, which built no list column for a layer never served from one. Two runs, two
figures; an earlier revision quoted them as one. The flatness argument that stood in for the direct
run holds in the same direction: the same twelve cells over 10⁸ points measure 7.8–60.3 ms against
8.0–108.1 at ten times the corpus.

⊘ **And the 10⁹/10⁷ layer covers about a tenth of the corpus.** Both grids run `--members 10`, so ten
million artifacts hold 10⁸ member slots over 10⁹ rows. The comparison above therefore moves **coverage
and corpus size together**, and cannot separate them: a layer covering the whole row space at 10⁹ is
not measured. Within the same runs, grouped candidacy grew **34 ms to 103 ms** between 10⁸ and 10⁹
points at the same artifact count, which no argument in this document explains. A corrected run at
full coverage is queued.

### 7.1 What a request pays, stage by stage

**The worst case is what bounds the system**, since it fixes how many viewers one core carries.
10⁷ artifacts, treed layer, a principal who sees the whole corpus, single-threaded, no per-token
state.

⊘ **This table is an assembly and its totals are withdrawn.** Its verdict row is the 10⁸-point
verdict measurement and its cut row is `artifact_cut_cost`'s, two different fixtures at two different
corpus sizes added together; no run produced a total. **§7.2 and §7.3 are the measured grids** and are
what a reader should cost a request from. The rows are kept because each is a real measurement of its
own stage:

| stage | whole map | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|
| verdict — index walk, containment, viewport edge *(10⁸ points)* | 31.7 ms | 8.2 ms | 0.96 ms | 0.28 ms |
| masked count, no criterion — `O(budget = 1 000)` | 0.06 ms | 0.04 ms | 0.07 ms | 0.04 ms |
| lineage *(per generation; §8.2)* | ~96 ms | ~96 ms | ~96 ms | ~96 ms |
| **cut** *(`artifact_cut_cost`, treed 10⁷)* | **3.05 ms** | 60 ms | 43 ms | 23 ms |

**The worst request is now the cheapest**, which is the shape the whole design has been converging
on: a principal who can see everything passes everything, and passing everything is exactly what
makes both the containment groups and the downward walk collapse. The expensive request is now a
*mid-zoom* one for a principal who sees most but not all — where the viewport does not narrow much
and the walk declines.

Against **~1 190 ms** for the same request before this campaign — the same assembly of the same two
fixtures, so the ratio is sound where the totals are not. Two of the four lines got there in this
round and both were the same mistake — materialising something to look at a fraction of it:

- **The verdict was 140 ms and is 31.7**, because it collected ten million ordinals out of a bitmap
  and then **sorted** them. Both inputs are already ascending, so the sort was 110 of the 140 ms and
  bought nothing. What remains is the materialisation itself, which exists only because `cut` takes
  a slice.
- **The cut was 254 ms and is 199**, because it built one interval per *servable* node — three
  arrays of ten million, 120 MB — and scanned all of them to select the 729 a budget serves. It now
  reads only the depth buckets a cut can reach.

**The cut is no longer the bound.** It was `O(level)` — five sequential passes over the ordinal
space — and it is now `O(budget × branching)` for the principal that bounds the system, by the
downward walk §6 describes. **Treed arm, `artifact_cut_cost`, 10⁷ level:**

| 10⁷ level, passing | lineage | cut, cold | **cut, warm** |
|---:|---:|---:|---:|
| 2 838 | 148 ms | 23.6 ms | 23.1 ms |
| 44 248 | 101 ms | 44.6 ms | 42.8 ms |
| 714 286 | 95 ms | 62.7 ms | 59.8 ms |
| **10 000 000** | 96 ms | 117 ms | **3.05 ms** |

**Where a node above the cut fails, the walk declines** and the sweep answers at the cost it always
had. That is the residual: a mask that is broad **and** fragmented — many artifacts passing, with
failures scattered near the top. It is not the shape that bounds throughput, because a principal who
passes everything leaves no fragmentation at all, and a principal who fails much is one for whom the
sweep is already proportional to the little they can see.

**Cold against warm is the lineage's question, not the cut's.** The walk reads a child index that is
a property of the tree, built on first use and reused after. A lineage held per generation sees only
the warm column; today it is rebuilt per request, so the cold column is what a request pays and
§8.2 is the difference between them.

⊘ **Two things measured and reverted**, recorded so they are not re-attempted: pre-sizing the depth
buckets from a counting pass (193 ms against 198 at full passing, and **32 against 24** at a level
where a few thousand pass — the second sequential scan costs more than the growth it avoids), and
`ArtifactRows::intersects`' early exit at whole-map zoom, which loses about a tenth where every
artifact meets the viewport anyway.

⊘ **Not in this table**: the gather, the record-blob reads for supplied content, and the wire
encoding.

### 7.2 The whole picture, by artifact type and principal

**10⁹ points, 10⁷ artifacts, single-threaded.** Per-request totals in milliseconds, with the lineage
held per generation (§8.2 — without it add ~96 ms to every cell).

**Per session: nothing on an artifact-major layer.** The containment partition (§4.2) is build-time
and names no principal, so a token costs what it always did — the mask fragment — and no artifact
structure at all. That is the change this campaign made that matters most at a large token count. A
**row-major** layer is the exception 0093 names: its masked-count histogram, ~4 B per artifact
(§5.1).

**Per generation, per view:** the row form (3.6 GB, unchanged), the tile index and extents
(**42 MB + 80 MB = 122 MB** at 10⁷ over 10⁹), the containment partition (~40 MB at the 32-expression
fixture, superseded — §4.2), the lineage with depth and child index (~200 MB).

#### Clustered and regional — HDBSCAN, point-and-radius, boundaries, hierarchy levels

**Measured at the target**, 10⁷ artifacts over 10⁹ points, treed, lineage held per generation, no
per-session state, all stages on one fixture. Peak RSS 25.3 GB. Milliseconds per request.
⊘ *Superseded pending re-measurement (§4), and **several cells are per-cell minima over two routes** —
`grouped` and `hoisted` — which is a lower bound on each cell rather than any one run's grid.*

| principal sees \ viewport | 100% | 75% | 50% | 25% | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|---:|---:|---:|
| **100%** | 111.1 | **232.4** | 182.1 | 92.7 | 32.8 | 9.7 | 8.1 |
| **75%** | 211.2 | 194.7 | 146.5 | 73.0 | 27.4 | 10.0 | 8.0 |
| **50%** | 152.3 | 147.9 | 109.6 | 57.6 | 22.2 | 8.8 | 8.0 |
| **25%** | 127.2 | 125.5 | 92.4 | 51.3 | 17.9 | 8.7 | 8.0 |
| **9.4%** | 90.6 | 89.0 | 66.7 | 36.4 | 14.2 | 8.4 | 7.7 |
| **3.1%** | 63.1 | 65.7 | 50.0 | 28.2 | 12.5 | 8.3 | **7.7** |

**The worst case is a ridge just inside the grid, not a corner of it** — a full mask and a
*three-quarter* viewport, against 111 ms at the same mask and the whole map. **That cell measures
232–279 ms across the three recorded runs** (232.4 was the minimum, and is what the table above
carries), so the ridge is a range and not a number; the run-to-run spread reaches 2.3× at the wide
cells, where the sweep dominates. An earlier revision of this table sampled 100%, 6.25%, 0.39% and
0.024% and reported 108 ms as the worst request; it stepped over the peak, and **understated it by
2.1×** (owner, 2026-08-21). Both axes are swept through their middles here for that reason.

**The cliff is the downward walk's guard, and the split says so exactly.** At a full mask and the
whole map every artifact passes, the walk applies, and the cut is **2.7 ms**. Narrow the viewport to
three-quarters and 1.2 million artifacts fall out of view — so nodes above the cut fail, the walk
declines, and the sweep runs over 8.8 million passing: **133.7 ms**, fifty times as much. The verdict
barely moves across that step, 108 → 99 ms.

**The ridge was largely the fixture, and a real hierarchy moves the cost somewhere else** — see
§7.3. What follows is the record of how that was found, kept because the reasoning was wrong twice.

⊘ **The ridge is not yet explained, and the guard is not the whole of it.** Relaxing the guard was
built, tested against the reference over random trees with partial masks, and **measured worse** —
272 ms against 232 at the peak — so it is reverted. Instrumenting it says why, and the answer is
about the fixture rather than the algorithm:

**the probe's treed layer is not a hierarchy.** The parent of ordinal *o* is `(o − 1) / 3`, so the
tree's shape is the ordinal space's, while each artifact's membership sits near its *own* ordinal. A
real nested layer is the opposite — a parent's membership **contains** its children's, so a parent is
in view whenever any child is and the root is in view always. Here the artifacts near ordinal zero
are the whole top of the tree, a three-quarter viewport drops them, and every lineage below becomes
its own fallback: a shape a real hierarchy cannot produce. The relaxed walk then spends its work
bound chasing fallbacks and hands the request back anyway, so it pays for both routes.

**So every treed figure at a partial viewport in §7.2 is suspect**, and the ridge is part fixture. The
measurement that settles it is a fixture whose membership comes from the tree rather than the tree
from the ordinals — which also changes what a level costs to store, since every level of a nested
layer then covers the corpus. That is the next measurement, and until it exists the guard should stay
as it is: simple, and the better of the two on the evidence there is.

#### Scattered — attribute predicates, per-analyst selections, terms-as-artifacts

**Served row-major** — §5.1's label column where the layer partitions, a list per row where it
overlaps — at ~10 ns per visible row and **flat in the artifact count**. Measured at 10⁵ overlapping
artifacts against the best artifact-major route — ⊘ *unrecorded earlier revision, re-measurement
queued: this table is labelled 10⁷ points here and 10⁸ where the same shipped figures appear beside
the hoisting arm, and no committed log settles which:*

| viewport | artifact-major | **row-major list** |
|---|---:|---:|
| whole map | **20.2 ms** | 75.7 ms |
| 6.25% | 29.4 ms | **6.3 ms** |
| 0.39% | 36.8 ms | **0.96 ms** |
| 0.024% | 16.1 ms | **0.14 ms** |

The two cross where they should: a row-major scan is cheapest where the viewport is small, and at
whole-map zoom the extent test settles the layer without scanning anything. **No artifact shape is
left without an answer**, which is what the list column changed — it generalises the row-major
layout from partitioning layers to overlapping ones, and `artifacts-from-points` already reads it.

⊘ **At the target the numbers are derived, not measured.** The scan costs `O(k × visible rows)`, so
a 6.25% viewport over 10⁹ points is 6×10⁷ rows ≈ **600 ms**; a 0.39% one is ~40 ms.
**Mid-zoom at the full corpus is where this shape is dearest**, inverting the clustered case.

**The one direct run at the target contradicts the sentence that used to close this paragraph.**
*"Whole-map is answered artifact-major"* was written from the 10⁸-point arms, where the extent test
settles a scattered layer because its artifacts, though scattered, are bounded. At 10⁷ scattered
artifacts over 10⁹ points the grouped route measures **136 952 853 µs of candidacy — 137 seconds —
at a full mask and whole-map zoom**, and the run did not complete: the single row in
`data/e2e-r1e9-a1e7-scattered.csv` is all there is. So whole-map is **not** answered artifact-major
for a layer whose artifacts reach across the row space; the walk hands every one of them back in the
`everywhere` set, and the extent test settles nothing because no extent is narrower than the map. The
argument has to be re-posed for that case, and `everywhere` is where it lives: an artifact in that set
takes the full masked probe at every zoom, and it is the third output of the walk that no document
before this revision mentioned. The run is slow for a reason worth recording — the correctness
assertions each scan the whole population through the shipped loop, ~24 s a call at ten million
scattered artifacts — but 137 s is candidacy, not assertion.

#### What this costs to hold

| | at 10⁷ artifacts over 10⁹ points |
|---|---:|
| row form *(exists)* | 3.6 GB per view, per level |
| lineage — parents, depth, child index | ~200 MB |
| tile index and extents | 42 MB + 80 MB = **122 MB** |
| containment partition | 40 MB *(30 MB of bitmaps + 10 MB of identifiers, at 32 expressions — ⊘ superseded, §4.2)* |
| list column *(scattered layers only)* | 4.4 GB at `k ≈ 0.1`; ⊘ ~8 GB at `k = 1` *(derived)* |
| **per session** | **nothing artifact-major**; a row-major layer holds ~4 B per artifact (§5.1) |

### 7.3 A real hierarchy, and where the cost actually is

The fixture above lays a tree over the ordinal space while giving each artifact a membership near its
own ordinal, so the two are unrelated — a viewport can drop the whole top of the tree and leave its
leaves in view. **No hierarchy can do that**: a parent contains its children, so a parent is in view
whenever any child is and the root is in view always. Building membership *from* the tree — the root
owning the row space, each node splitting its range among its children — gives a different answer.

**A hierarchy is cheap to store**, which is worth settling first: every level covers the corpus, so
the layer holds `depth × rows` of membership — but each node is a **contiguous range**, hence one run
whatever its size, so the whole layer is `depth × (rows / 65 536)` containers. Measured at **1.2 row
blocks per artifact** over 10⁶ nodes on a 10⁹-row corpus. Nesting does not cost what its member count
suggests.

At 10⁶ artifacts over 10⁹ points, milliseconds:

| principal \ viewport | 100% | 75% | 50% | 25% | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|---:|---:|---:|
| **100%** | 14.3 | 87.8 | 71.5 | 33.8 | 13.6 | 4.0 | 1.9 |
| **75%** | 72.7 | **102.2** | 89.4 | 58.4 | 37.5 | 25.9 | 14.3 |
| **50%** | 61.5 | 87.8 | 76.9 | 53.0 | 33.3 | 25.2 | 14.7 |
| **25%** | 46.6 | 70.0 | 58.2 | 40.4 | 28.3 | 23.0 | 12.6 |
| **9.4%** | 30.0 | 46.8 | 39.0 | 28.3 | 20.8 | 16.8 | 9.8 |
| **3.1%** | 28.9 | 38.5 | 33.2 | 26.4 | 20.6 | 17.4 | 17.1 |

**The cut stops being the problem** — 0.3 to 11.4 ms across the whole grid, against 176 ms on the
ordinal-tree fixture. The ridge §7.2 reports was largely that fixture's own, and the downward walk's
guard was being blamed for it.

**What dominates instead is the masked count of a coarse node.** It runs 3.6 ms at a full mask and
**20–52 ms at a mask below one, falling about fourfold as the viewport narrows** — 52.3 ms at a 75%
mask and the whole map, 12.6 ms at the same mask and a 0.024% viewport. **Measured at 10⁶ nodes
only.** An earlier revision read the same grid as *"52 ms at any mask below one, flat across the
viewport"*, which is neither of the two things it shows: the mask step is a cliff, and the viewport
axis is a slope. What is stable across the grid is the *cliff*, and it follows from what the count is.
The count is taken over the *served* set, which the budget bounds; but a shallow cut serves **coarse**
nodes, and a coarse node's membership is most of the corpus. Against a full mask that is one run;
against a fragmented one it is fifteen thousand containers, three times over for base, minus and
plus.

⊘ **Per-signature counts would remove it, and cheaply.** `|membership ∩ M_auth|` is
`Σ over satisfied signatures of |membership ∩ sig|` — build-time, mask-independent, and exact once
the overlay's small `minus` and `plus` are applied over it. It is only worth storing where the count
is dear, which is the coarse nodes: the top nine levels of a 10⁶-node tree are ~10⁴ nodes, so ~1.3 MB
at thirty-two signatures. **Its cost scales with the signature count, not the artifact count**, which
is the thing to check before building it — and thirty-two is the fixture's axiom, where the measured
corpus holds 54,791 distinct signatures over 2.42M items (§4.2). At a thousand distinct signatures the
same structure is 40 GB at 10⁷ artifacts if stored for every node rather than the coarse ones.

### 7.4 What bounds the system now

**On a real hierarchy, the masked count of a coarse node** — 3.6 ms at a full mask, and 20–52 ms below
one, falling ~4× as the viewport narrows, **measured at 10⁶ nodes only** (§7.3). The cut, which two
earlier revisions of this section named as the bound, is 0.3–11.4 ms there; what made it look
expensive was a fixture whose tree was unrelated to its geometry.

**Where the walk declines, the cut's sweep floor** — ~8 ms at a 10⁷ level however few artifacts pass,
because the sweep's side tables are ~100 MB of first touch per call. **That figure has one lineage and
it is worth naming**: it is the cut column of §7.2's grid at a 0.024% viewport, 7.6–7.7 ms over 2 112
candidates on the **ordinal-tree** fixture — the one §7.3 shows is not a hierarchy. `artifact_cut_cost`
measures the same floor at ~20 ms on its own treed arm and 28 ms on its flat arm at full passing, so
the constant depends on the fixture and the three should not be quoted interchangeably. The downward
walk applies only where most of the level passes, and a flat level has no tree to walk at all.

**On a scattered layer, the row-major scan at mid-zoom** — `O(k × visible rows)` puts a 6.25%
viewport over 10⁹ points at ~600 ms *(derived)*, which is the one cell of the whole campaign still
over budget.

⊘ **Three reductions are scoped and unbuilt**, in the order they are worth taking:

- **Per-signature counts for coarse nodes** (§7.3) — removes the hierarchy's dominant term for
  ~1.3 MB, and the thing to check first is how the storage scales with a real corpus's signature
  count rather than the fixture's thirty-two.
- **Scratch buffers for the sweep** — its floor is page faults, not work, confirmed at 769 475 minor
  faults across the probe. This trades a pure function for reused state, so it wants a ruling.
- **Handing the cut a bitmap** rather than a materialised slice, which also removes the ~240 MB of
  `passing` tuples a wide request allocates and which appears in no figure here.

## 8. What this depends on, and is not yet true

Two live defects. Neither is about scale, and the design is worth little without them.

**8.1 Any artifact write invalidates every cached row form in every view.** `ArtifactStore`'s
`version` is global and `ProjectionKey` carries it, so one suppression, one grown membership or one
publication anywhere rebuilds every layer's projection everywhere — ⊘ *the "138 s at 10⁷ artifacts"
figure is from an unrecorded earlier revision; the committed logs say **147.8 s and 151.3 s** at 10⁸
points, and 282–298 s at 10⁹. Re-measurement queued; the order of magnitude is not in doubt and the
direction is worse, not better.* Under read-write load the cache never survives to be used, and the
containment partition inherits the same fate, since it is keyed on the same store version (§4.2). It
needs to be per `(layer, level)`, and ideally patched rather than rebuilt: a write touches a handful
of ordinals.

**8.2 `Lineage` is rebuilt on every request**, per level, under the artifacts lock, from something
that depends on neither the mask nor the viewport. It is now 87 ms at 10⁷ because it carries depth —
which is the right home for depth, and the wrong cadence for the object.

**8.3 §8.5's cache key is incomplete, and the gap is fail-open.** As specified it is *(auth-data
hash, auth-plugin version, overlay version)*, and none of those moves when an **artifact** does. A
generating set that grew has more to contain, so a cache keyed only on the viewer keeps serving a
label the viewer no longer contains — and growth makes containment *harder*, so the stale answer is
the permissive one. The key needs the artifact store's version beside the overlay's;
`ProjectionKey` already carries exactly that one layer down. Worth writing into §8.5 whether or not
any option here is taken.

**8.4 What the request still allocates after the verdict.** The serving loop collects
`passing: Vec<(u32, EntityId, u64, Option<u32>)>` — every artifact that cleared the predicate — and
hands its ordinals to the cut. At 10⁷ passing that is **~240 MB allocated and freed per request**,
and it is in no figure here. The routes in §4 leave the passing set as a bitmap, so materialising is
a *choice*; what stands in the way is that `cut` takes a slice. ⊘ Unmeasured, and it is the next
reduction: the cut's remaining 188 ms is **five** memory-bound passes over the level with no dominant
term — the frontier walk, the depth buckets and three sweeps, at ~5–7 ns a node — so what is left is
structural rather than local. (An earlier revision said nine; the probe counts five.)

## 9. The ruling this asks for

**Whether a layer with no column and no row-space locality carries a declared bound** — refused,
warned, or merely reported. §5.1 removes the wall for anything that partitions, so what is left
un-helped is a layer that is scattered **and** overlapping **and** numerous: an enumerated set with
no column. Every real instance of that shape is human-made or vocabulary-made, and the realistic
counts are thousands, where the cost is 1.5–15 ms.

Three routes, and the house rule reads clearly here — nothing leaks, nothing is irreversible, and a
wrong guess costs a rebuild:

- **(a) Row-major wherever the layer partitions.** §5.1. Removes the wall rather than bounding it.
- **(b) A declared bound on the rest.** A warning that prints the numbers, not a refusal.
- **(c) Report the shape in the build's frame report.** Blocks per artifact is one number and the
  build already computes the row form it comes from, so a layer that will be slow says so when it is
  built rather than when it is panned.

**Recommended: (c) always, (a) wherever the layer partitions, (b) as a warning.**

**Ruled 2026-08-21: (c) always, (a) wherever the layer partitions, and (b) not at all** —
[decision 0092](../decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md).
The recommendation is not taken in full: there is no declared-bound machinery, as a refusal or as a
warning key, because what costs is locality rather than the count and the report already prints the
measured number a declared one would have been compared against. The ruling also withdraws the
per-request bound the delivery record had owed since the model was written.

## 10. What is not in scope, and what is not measured

- **Nothing changes the client contract.** No option above requires a new request field; the layout
  is not a wire fact (§4.1 of [the selection surface](../evidence/memos/2026-08-21-artifact-layout-selection.md)).
- **The write path *does* change, and an earlier revision of this list said it did not.** The index,
  the extents and the containment partition are derived and built where row forms are built, so those
  are cheap. **The row-major layouts are not derived — they are a durable form**, and they touch four
  seams: the online extent writer, which must emit a column instead of bitmaps; the fold's repack,
  same; the open-time seed, which must recognise which form a level is in; and the fold's **row
  renumbering**, which today permutes an entity-addressed membership and must instead permute a
  row-addressed column — a whole-level rewrite whose cost is unpriced. §8.1's fix makes writes
  cheaper; this makes them different.

  **Two quantities have no row-major form and keep an artifact-major structure beside the column.**
  The **proportional criterion's denominator** — an artifact's declared size — is corpus-wide and
  constant per artifact, so it is stored per artifact and costs a small array rather than a
  membership; and **`generated_from`**, the generating set, is entity-space by construction (write
  cycle §2) and is not a row-space object at all. Neither is a reason against the layout; both are
  reasons the column is never the whole of a level.
- **The routes are probe-side.** Every quantity goes through the shipped structures —
  `ArtifactRows::build`, `intersects`, `masked_count`, `satisfied_rank`, the three-set composed
  arithmetic `EffectiveMask` does, viewports from `tiles_for_bbox`. The control flow is written in
  the probe, not in `viewport.rs`. The cut in §6 is the exception: that is in the engine.
- **One level, one layer.** A treed layer's loop runs per level and a request may name several
  layers; a hierarchy does not multiply the cost, because each level's cost is proportional to its
  own population and the figures are for a total — but several *layers* do.
- **Not a real clustering.** The arms bracket it; real membership is 14–170× cheaper per member than
  the synthetic arm.
- **Selection functions** — rank by masked size, filter by label text — need a masked count for every
  candidate rather than for the served few, which is §4.3's wide case at every zoom. Nothing here
  makes them cheap: the structure that would have (a per-token count) is what 0093 declines. The
  ranking contract itself is unruled and is not proposed here.

## Appendix R — review trail

- **r2** — **the adversarial review, dispositioned** (2026-08-21;
  [the record](../evidence/memos/2026-08-21-artifact-serving-scale-review.md)). Three lenses —
  disclosure, correctness and lifecycle, claims audit — over this document,
  [the selection surface](../evidence/memos/2026-08-21-artifact-layout-selection.md) and decisions
  0092–0094. **Two fail-opens in the unbuilt design.** §4's settled half claimed containment answered
  *"has a visible member"*, which the structure 0093 kept does not answer, and the probe's routes G
  and H omitted the test; §4 step 2 now specifies one early-exiting `Bitmap::intersect` against
  `viewport ∩ M_auth`, exact for a settled artifact because membership ⊆ viewport, and §4.1's second
  bullet is the collapse rather than an answer. And §4.2's deny correction is `deleted ∪ suppressed`
  evaluated live at the ack, not a refresh with a window — with the entity→artifacts index it needs
  recorded against `annotation-write-cycle.md` §4.5, which exists to avoid exactly that lookup.
  **Every §7 grid is superseded pending re-measurement**, marked at each table. Containment is
  restated at `(artifact, rank)`; the expression count is marked unmeasured and the fixture's
  thirty-two named as an axiom; the partition's key, cadence and unmeasured build cost are stated;
  §10 is rewritten because the row-major layouts are a durable form touching four write-path seams;
  §6 is rewritten around the downward walk with one table per arm. Figure corrections through §7:
  the stage table is an assembly with its totals withdrawn, the flat-in-corpus-size table is the
  deleted per-token route, the ridge is 232–279 ms over three runs, the coarse-node count is 20–52 ms
  and slopes with the viewport, the 10⁹/10⁷ layer covers ~10% of the corpus, and four figures that
  appear in no committed log are marked *unrecorded, re-measurement queued*. Two leak-register
  annotations were approved with it (architecture Appendix C, C4 and C15 shapes), and the three
  sentences claiming the layout choice has no disclosure content are narrowed to *nothing on the wire
  names a layout*.
- **r1** — first draft, the campaign's options memo, ruled by decisions 0092–0094 (2026-08-21).
