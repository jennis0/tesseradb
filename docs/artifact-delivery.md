# Artifacts — delivery and status

**Status:** Living. **This file is the status record for the artifact work** — the stages, what each
must be true to be finished, and where it has got to. Tracked here rather than in GitHub issues by
owner direction (2026-08-15).

**The rule that keeps it true: it moves in the change that moves the work.** A stage that landed
without this file changing is a stage whose status is now wrong, and a file that disagrees with the
code is the file that is wrong. That is the whole discipline — this repository has previously
accumulated plans whose boxes were unticked while the work was complete, because completion lived
somewhere else.

**How the work is done: each stage is implemented in its own git worktree** (owner direction,
2026-08-15), on the convention [`agents/parallel-work.md`](agents/parallel-work.md) already
carries — `.claude/worktrees/<name>` on its own branch, with the track marker and the allowlist —
whether or not anything is running beside it. Stages 5 and 6 are the pair that will actually
overlap; the rest take it so that a stage's diff stays separable and the main tree stays clean.

**Reads with:** [`design/annotations.md`](design/annotations.md) (the model),
[`design/annotation-representation.md`](design/annotation-representation.md) (the representation),
[`design/annotation-write-cycle.md`](design/annotation-write-cycle.md) (the write cycle) — all three
**drafts**, two of them unreviewed; [`design/derived-artifact-gating.md`](design/derived-artifact-gating.md),
superseded in scope; [`design/correctness-suite.md`](design/correctness-suite.md) §12 (the fixture
machinery this extends); [`probes/dataset.md`](../probes/dataset.md) (the corpus);
[`probes/2026-08-15-artifact-representation/`](../probes/2026-08-15-artifact-representation/) (the
campaign every sizing figure below comes from). Ordering precedent:
[`design/records-and-search.md`](design/records-and-search.md) §13.

---

## Where it stands

Nothing is built. There are no artifacts, no layers, no membership structure and no frontier.

| Stage | State | Finished when | Evidence |
|---|---|---|---|
| **0** Rulings and promotion | **review done, findings open** | the three designs are normative and the register carries their rows | [the review](evidence/memos/2026-08-15-artifact-design-review.md): four fail-opens, one structural blocker, five rulings owed. **Stage 1's registry half is blocked on them** |
| **1** The spine — allocation and the layer registry | **in progress** (`artifacts/stage-1`) | an empty layer is reachable by gate, suppressible at the ack, droppable for ever, and survives restart | **the tiebreak is in, both build paths**, with a test that fails without it, and the geometry read moved so it costs no extra pass; verified on the real 2.4M corpus. The registry is not started |
| **2** One flat level, masked counts | not started | two principals get different counts for one real cluster, neither equal to its size; below-threshold artifacts are indistinguishable from absent ones | — |
| **3** Content — derived, supplied, containment | not started | both principals fail the same real label and both satisfy its per-term variant | — |
| **4** The write cycle | not started | a deleted source document's label vanishes at the ack and **stays gone** across a fold; the stage battery covers the artifact surface | — |
| **5** Hierarchy, levels, selection | not started | the non-covering case behaves as the design says under both pruning settings, and the build named the lossy edge in advance | — |
| **6** Predicate membership | not started | the taxonomy layer returns identical masked counts as an enumerated layer and as a predicate one | — |
| **7** Runtime artifacts | not started | a set of ten shared across a clearance boundary shows seven, and the day-one bookmark survives a hundred edits | — |
| **8** Filters, search, scale | not started | an invisible artifact and a nonexistent one cost the same; 10⁹ points with ~10⁷ artifacts serves and folds inside budget | — |

Stages 5 and 6 touch largely disjoint machinery — edges and descent against geometry and the
existing filter path — so they can run beside each other. Nothing else here can.

## 1. What decides the shape of this plan

Two facts do most of the sequencing work, and neither is about clusters.

**One decision cannot be taken later.** Entity IDs are allocated `(signature, source_id)`, and the
campaign measured `(signature, morton)` as **4.08× smaller** on the artifact disk form with term
postings **byte-identical** (M3, *measured*). It cannot be retrofitted under **I9**, so it is taken
before the first build that writes an artifact or not at all. It also renumbers every entity in
every fixture, so it wants to be taken *once*, in the same rebuild as the full-schema 2.4M corpus
[#88] is already committed to.

**Two of the three designs have not been reviewed.** The write cycle is reviewed and dispositioned;
the model and the representation are drafts, the representation carries round-two findings that are
*not yet dispositioned*, and two of the model's claims **contradict the normative architecture**
(§7.5's descent, §7.7's label ladder). The roadmap already gates this work on it. Writing code
against an unpromoted draft here means writing it twice.

Everything else is ordinary sequencing: a layer must exist before an artifact, an artifact before
its content, content before the events that can invalidate it.

**What already exists and is reused unchanged** — entity IDs, the deny lane, the WAL, the overlay
and both removal rules; the record blob (where supplied content lives); row space, the permutation
and the fold; the composed mask and `and_cardinality`; the filter tree and the token index. **What
does not exist** — anything artifact-shaped at all, and the WAL'd registry pattern the layer
lifecycle is specified against: `slices-and-multi-table.md` §3 is a *design*, not code, so the layer
registry is the first implementation of that shape rather than a reuse of it, and slices will
inherit it.

## 2. Stage 0 — what must be settled, and what it blocks

No code. One adversarial review and a ruling pass, sized at days rather than weeks because the
material is written; what is missing is the disposition.

| What | State | Blocks | Note |
|---|---|---|---|
| **Independent review** of the model and the representation | ✔ **run 2026-08-15** — three lenses, [the record](evidence/memos/2026-08-15-artifact-design-review.md) | — | the model's core survived all three; its *derived* rules did not |
| **Review ruling 1** — where artifact entity IDs come from | ✔ **ruled** — [decision 0074](decisions/0074-row-less-entities-are-allocated-downward.md) | Stages 1–4 | row-less entities allocate downward from the top; the repairs from the review's other findings are made |
| **Review ruling 2** — the masked count as an existence criterion | ✔ **ruled** — [decision 0075](decisions/0075-the-masked-count-is-an-existence-criterion.md) | Stages 1–2 | it never suppressed a count: it decides whether the artifact is served. Declared per layer, absolute or proportional, no default, **independent of the gate mode** |
| **Review ruling 3** — does a label's existence follow its content? | ✔ **ruled** — [decision 0076](decisions/0076-an-artifact-is-served-whole-or-not-at-all.md) | Stage 3 | wider than asked: **no levels of restriction within one artifact**, beyond versions. C3's question evaporates; §8.6's degrade-to-derived is withdrawn |
| **Review ruling 4** — the artifact **edit** mechanism | **deferred to its own design pass** *(owner, 2026-08-15)* | Stage 7 only | not load-bearing: publishing and republishing artifacts needs no edit route. Two consequences, both stated rather than discovered — the emergency path becomes *suppress, then republish* (slower, not weaker), and runtime selections, whose whole lifecycle is editing, wait for it |
| Where supplied content **lives** | ✔ **ruled** — [decision 0077](decisions/0077-supplied-content-lives-in-the-record-blob.md) | Stage 3 | the record blob, at the artifact's entity. Its addressing is rank in the blob's **own** has-row bitmap, independent of row space, so an artifact having no row does not bear on it |
| **Review ruling 5** — how search gates on containment | open | Stage 8 | the term-signature conjunction is the candidate shape; the route stays withdrawn until ruled, which costs nothing before Stage 8 |
| Implement [0072](decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) — slot reuse | **deferred, deliberately** *(owner, 2026-08-15)* | nothing here | not a dependency: artifacts need identity, the deny lane and the opaque identifier, none of which need reuse. Deferring **removes** the recycled-slot fail-open rather than carrying it; the membership-reconciliation clause ships with it whenever it lands |
| **The signature-sort tiebreak** (rep §12) | ✔ **ruled 2026-08-15** — [decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md) | Stage 1 | taken: allocation becomes `(signature, morton_code, source_id)`. Free in the format and *measured* so; what it costs in the build's batch plan is named there and sized in Stage 1 |
| The descent change (model §6) — per-artifact test replacing §7.5's tree walk | open | Stage 5 | contradicts a normative document |
| The label ladder reduced to guidance | ✔ **ruled** — [decision 0078](decisions/0078-the-service-takes-no-opinion-on-which-variation.md) | Stage 3 | the service resolves a caller-supplied ordering and chooses nothing. **A variation is a general artifact property**, not a label one |
| The three gate modes (model §5) | open | Stage 2 | substitutive disables `min_visible_members` by one schema word — the register row is part of the ruling |
| The suppression-across-regeneration refusal (rep §5.0.2) | open | Stage 7 | makes the stable key mandatory once a layer has taken a suppression |
| Membership as a filter (rep §7) | open | Stage 8 | a disclosure question, not a cost one |
| Appendix C edits: C1 gains layers as a differencing surface; a C23-shaped row for substitutive; a C12-shaped row for the corpus-independence declaration; C17 annotated; C7's disposition (already written, r42) | part done | Stages 2–4 | the register is exhaustive by construction or it is not a register |
| `derived-artifact-gating.md` — folded into the three or retired | open | promotion | its taxonomy is superseded; leaving both standing is the "two answers" problem it was written to fix |

**Two design items are owed and are not rulings.** The entity budget under repeated regeneration —
a 10⁷-artifact layer mints 10⁷ IDs per regeneration against a `u32` space (write-cycle §9), with
[decision 0072](decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) as the likely
answer since a dropped layer's slots return at the fold; and the **per-request bound on predicate
evaluation** (rep §2.0), without which a nationwide boundary level is seconds per query. The first
is due before Stage 7, the second before Stage 6.

## 3. The stages

Each stage ends in something that can be demonstrated against a real corpus and checked by a test
that fails if the stage is removed. Data each stage needs is named here and specified in §5.

### Stage 1 — The spine: entity allocation and the layer registry

**Capability:** a layer exists, is reachable or not by gate, can be suppressed immediately and
dropped permanently — with no artifacts in it at all.

- [Decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md)'s tiebreak in the
  build's comparator, in **both** build paths; fixtures rebuilt; postings verified byte-identical
  and the permutation's monotone runs verified at ~44 against ~1 (*measured* prediction from
  M3/M7). The decision names two record layouts for the signature sort and does not choose between
  them — that choice, and its effect on the feasible batch, is this stage's first measurement.
  Projection figures are re-baselined here, since gathered rows now arrive in sorted runs.
- Layer registry: manifest section plus WAL'd create/drop/tombstone, served as manifest + WAL
  overlay. One entity ID per layer, so layer suppression rides `/control/changes`.
- Reserved entity runs per level as a **list** of 2¹⁶-aligned blocks, never one block.
- `/v1/meta` carries the gate-filtered registry — identity, structure, zoom-to-level map, declared
  derived vocabulary, slices, supplied-content kinds. **Never the artifact cardinality.**
- Layer reachability keyed on the layer version, with a live `verdict` check ahead of it.

**The check:** a layer is visible to one principal and, to another, indistinguishable — in outcome
*and in work* — from a name that was never registered; suppression takes effect at the ack for an
already-open session; a dropped name is refused on recreation; all of it survives WAL replay and
restart. **Data:** none new.

**Where the tiebreak got to.** Landed in both build paths as a **16-byte sort record**, with the
batch plan's residency model widened to match — a model left at 12 would plan a batch the loop
cannot hold. The codes reach the sort through a mapped `morton-of-ordinal.u32`, filled by its own
pass over the points file, because an ordinal exists only once the source ids are sorted and the
first pass sees the file's order rather than the corpus's. **No existing test moved** — the suite
pinned signature *grouping* and never the order inside a group — so the new one asserts the
direction on both the pre-sort and the tie-refinement path.

**What the real 2.4M corpus says** (*measured* 2026-08-15, builds of the same corpus through the
shipped writer, warm cache, `categories-subclass`, **minimum of three runs**):

| | baseline | shipped |
|---|---:|---:|
| build wall time, 2.4M | 7.34 s | 8.35 s (+14%) |
| build wall time, 25M | 22.11 s | 25.11 s (+14%) |
| points-file passes | 3 | **3** |
| `postings.arrow`, `pairs.parquet`, `morton.u32` | | **byte-identical** |
| mean monotone run in `permutation.bin` | 2.00 | **49.19** |

**Read single runs as ±1 s at 2.4M and ±5 s at 25M**, and quote minima of three. An earlier revision
of this section carried **+49%** from one run — noise, withdrawn, along with the precise
`+0.33 s record / +3.04 s pass` split derived the same way.

**Two results correct the design.** M3's *"postings byte-identical"* now holds end to end rather
than in a re-derivation — and it is structural, not luck: a signature group occupies the same
contiguous entity range however its interior is ordered, so a term's postings cannot move. M7's
permutation claim was *"~1 → ~44"*; the measured pair is **2.00 → 49.19**, because the uncorrelated
baseline for a random permutation is 2, not 1. The direction and the destination hold; **the ratio
is 24.6×, not 44×**, and anything quoting the old baseline is quoting a mistake.

**Where the geometry read goes, settled at 25M** (*measured*, minimum of three warm runs; every
variant produces a **byte-identical** bundle, `created_at` aside):

| | 2.4M | 25M | |
|---|---:|---:|---|
| baseline, no tiebreak | 7.34 s | 22.11 s | |
| **a)** separate Morton read — four points passes | 8.84 s | 28.56 s | +29% |
| **b)** geometry read once, standalone permute | 8.20 s | **30.66 s** | +39% — *worse than (a)* |
| **c)** geometry read once, **fused into the assignment walk** | 8.35 s | **25.11 s** | **+14%** — shipped |

**2.4M cannot answer this question and nearly gave the wrong answer.** At that size the corpus fits
one batch, all three variants sit within a second of each other, and (b) looked like the winner. At
25M (b) is the worst of them. Anything decided from the small tier here would have gone into the
10⁹ rebuild backwards.

**The diagnosis that mattered was not the one this file first recorded.** (b) was written believing
the standalone permute added random access. It does not: the scatter into entity-ordered geometry
exists in *every* variant, including the baseline, where it sits inside the geometry scan's own join
callback — and at 25M that callback iterates ordinals ascending too, because one join chunk covers
the corpus. What (b) actually did was trade **parallel** work for **serial**: `scan_points` decodes
row groups on a worker pool, so the pass it removed was multi-threaded streaming, and the permute
that replaced it is a single-threaded traversal. That correction came from an independent
algorithmic pass over the stage, and it is what produced (c).

**(c) pays for the geometry move where the build is already paying.** The assignment walk visits
every item with both indices in hand, randomly writing `entity_of_ordinal` and randomly incrementing
a per-term counter; adding two gathers to a loop already stalling on memory costs far less than a
traversal of its own. Entity ids ascend with position there, so the geometry *writes* stream. The
ordinal-space arrays are released as soon as that walk ends — at 10⁹ that returns 8 GB of dirty
mapped pages before the band sweep and postings write start competing for cache.

⊘ **What survives is +14%, consistent across both tiers**, and it is the tiebreak itself: the wider
sort record, the extra comparator field, the ordinal-space arrays and the two gathers. ⊘ **And the
10⁹ ordering is modelled, not measured** — the argument that (a) cannot win there is that ~25 GB of
points will not stay in page cache beside the build's own scratch, so its fourth pass becomes real
I/O rather than a cached decode. That is the claim to check when a 10⁹ build is first run, and it is
the reason (a)'s 25M win over (b) was not taken as the answer.

### Stage 2 — One flat level, served with masked counts

**Capability:** a viewer sees the clusters their own visible set generates, with a count that is
never the cluster's size.

- Enumerated membership: entity-space Roaring on disk (`members/<ordinal>.roaring`), the derived
  **row-space** operator built at open, member-wise and never range-wise.
- The visibility predicate in one place, overlay first: `verdict` → layer gate → gate mode.
- The derived gate and `min_visible_members`, enforced on the **live** count; the session's resolved
  visibility set as candidacy only, keyed on `(layer, version, generation)`.
- Viewport carries the artifacts whose rows intersect the tile ranges, with masked counts.
  Drill-down by `tessera_id` resolves through the resolved set. Ordinals never cross the wire.
- Contracts work: the viewport frame and `/v1/items` shapes for an artifact.

**The check:** on the real 2.4M clustering, a broad principal and a one-term principal receive
different counts for the same cluster, neither equal to its declared size; clusters below threshold
are **absent**, not refused, and the response cannot distinguish them from clusters that never
existed. The conformance oracle recomputes every count from the same membership and the same mask
independently. **Data:** `clusters/hdbscan-2026-08` at 2.4M (§5.2), the seeded generator's artifact
arm at 10⁴ (§5.1).

### Stage 3 — Content: derived, supplied, and the containment test

**Capability:** a label is served only to a viewer who can see everything it was generated from.

- The declared derived vocabulary — count intrinsic, centroid/hull/box opt-in — under the closure
  rule that a derived property is a function of `membership ∩ M_auth` and nothing else.
- Supplied content in the record blob; **`G` as an immutable sorted entity-space array**, mmapped on
  touch; containment `and_cardinality(G, M_auth) == |G|` against the **composed** mask, cached
  nowhere, and costing the same on the pass and fail paths.
- Ranked versions: one artifact, one identity, first satisfied served.
- The attachment edge, and the term that is fail-open if omitted: an attached artifact is tested on
  its target's `verdict` **and** its target's gate, on every route, including the ones that never
  traverse the edge.

**The check:** the design's own worked example, reproduced on real data — a broad viewer and a
narrow viewer fail the *same* full-sample label for the same reason, and both satisfy its per-term
variant. Suppressing a cluster stops its labels serving on search and by held identifier, not only
on traversal. **Data:** `topics/ctfidf-2026-08` and `centroids/kmeans-2026-08` (§5.2).

### Stage 4 — The write cycle

**Capability:** ingest, delete, suppress and the fold leave every artifact correct, and the fold
does not resurrect a withheld label.

- The row operator's three arms: **union the new extents at flush, rebase over the merged span at a
  merge, rebuild at the fold.** The merge arm is the one a reader leaves out, and leaving it out is
  fail-open.
- The fold's artifact pass, in the cheaper of the two constructions (§6 measures which), plus
  **Rule F's artifact arm** — membership file, `artifacts.arrow` slot and every edge naming a
  deleted artifact dropped *before* the overlay entry retires.
- The fold's report sweep: `and_cardinality(G, D₀)` per `G`-bearing artifact, published with the
  fold, discharging the notification obligation.
- The strict/permissive declaration, strict by default; publish-time member validation (a declared
  member that is deleted is a 422, one that is suppressed is accepted).

**The check:** the correctness suite's stage battery extended to the artifact surface — recorded
after every one of the eight stages, judged by the same three mechanisms. The regression test this
stage exists for: delete a member of a label's generating set, watch the label vanish at the ack,
run a fold, and **it stays gone**. **Data:** the seeded generator at 10⁶–10⁸ (§5.1) — real data
cannot carry this, because the check is "nothing is missing or extra" over all *n*.

### Stage 5 — Hierarchy, levels and selection

**Capability:** several levels of structure, chosen by zoom or by the client, with rollup that is
honest about what it cannot cover.

- Parent/child edges; per-artifact threshold testing rather than a tree walk; pruning as a declared
  policy per layer with a per-level override that may only **raise** the threshold.
- Build-time containment verification that **reports** violating edges rather than deciding
  anything.
- Nested against stacked declared, never inferred; the zoom-to-level map as advisory metadata.
- **A level is served partially and never withheld because part of it is suppressed.**

**The check:** the non-covering case on real data — a principal holding only the term covering a
child's stray members sees nothing with pruning on and sees the child with pruning off, and the
build's report named that edge in advance. **Data:** the real clustering's four levels, whose
non-covering edges are a property of HDBSCAN and not planted (§5.2).

### Stage 6 — Predicate membership

**Capability:** a boundary or a category behaves as a layer, with membership derived per request and
never stale.

- Spatial predicate: the shape only, decomposed to Morton ranges, counted by `range_cardinality`.
- Attribute predicate: no new storage — the existing value column and postings.
- The per-request bound the design does not yet specify (§2 above).

**The check:** the arXiv taxonomy layer is built twice — once enumerated at Stage 5, once as an
attribute predicate here — and the two return **identical** masked counts for every principal and
every viewport; a point ingested inside a boundary is a member on the next request with nothing
rebuilt. **Data:** `taxonomy/arxiv-2026-08` and `regions/synthetic-geo` (§5.2).

### Stage 7 — Runtime artifacts

**Capability:** an analyst assembles a set mid-session, shares it, and edits it without breaking the
share.

- The create/edit control verb (contracts work): `(layer, membership, gate, content, stable key?)`,
  members named by `external_id` or `tessera_id`, resolved at admission.
- The edit table: content in place with `G`; membership in place with a version bump; gate widening
  in place; **gate narrowing = suppress, re-grant, unsuppress**.
- Stable keys, the suppression-carry refusal and the dangling-dependent refusal on regeneration.

**The check:** a set of ten shared with a colleague who cannot see three of its members shows
**seven**; a hundred edits later the day-one bookmark still resolves; a regeneration that would
drop a live suppression or dangle an edge is refused with the offending keys named. **Data:**
`selections/analyst-*` and `programmes/portfolio` (§5.2).

### Stage 8 — Filters, search and scale

**Capability:** artifacts are searchable and filterable within their disclosure rules, at the scale
the design claims.

- Membership as a filter — unrestricted for substitutive layers, threshold-inherited for derived
  ones, and refused work-indistinguishably for a non-visible artifact.
- Search over artifact text via the token index on the artifact population, with C25 re-read against
  a population two orders smaller.
- The 25M / 250M / 10⁹ artifact tiers, built once with the corpus tiers.

**The check:** per-tile counts under a membership filter never resolve below `min_visible_members`;
a filter naming an invisible artifact and one naming a nonexistent artifact do the same work. At
10⁹ with ~10⁷ artifacts, a viewport serves inside its budget and the fold completes inside
`plan_fold`'s memory ceiling. **Data:** the scale tiers (§5.3).

## 4. What holds the line between stages

Stages 2 and 3 ship a bundle that carries layers before the write cycle exists. Two different
things are true of that state, and conflating them is how a fail-open ships:

- **Ingest is safe and stale.** A member ingested since the last fold has no bit in the row form, so
  every count **understates** — fail-closed, and the same posture as a buffered point being
  invisible until its flush.
- **A fold is unsound.** Row space renumbers globally, so every resident membership form is
  meaningless at the flip. Until Stage 4's pass exists, `plan_fold` **refuses** when any layer is
  present, and says why. A loud refusal is the only acceptable placeholder; stale-serve is not one
  here, and `compaction.md` §6.2 already says so for this class of artefact.

## 5. The test data

Four fixture families, because they answer four different questions. The word *artefact* below is a
stored file; *artifact* is the annotation object.

### 5.1 The seeded generator — the only fixture that scales

`tessera-corpus` gains an artifact arm, in Rust, under the same rules the rest of it obeys: every
property of artifact *a* is a keyed function of `(seed, layer, level, a)`, constant-time, no I/O, no
table, prefix-stable in *n*. Both directions must be closed-form — the members of an artifact, and
the artifacts holding an entity — or the census cannot answer "nothing is missing or extra".

Shape: a keyed contiguous Morton interval per artifact plus a keyed scatter, so compact *and*
pathological membership both occur by construction; gate modes cycled across artifacts so all three
are exercised at every size; a generating set as a keyed ~10²-member subset; deliberate overlaps
within a level, one single-member artifact, one artifact with zero visible members for a chosen
grant, one level with no artifacts at all.

**This is the only family that can carry Stage 4**, because the stage battery compares recorded
answers across eight stages at sizes where no expectation can be stored. It is also the only one
that reaches 10⁹ without a bundle on disk, following the decomposition probe's approach.

### 5.2 Real artifacts over the real 2.4M corpus

The corpus is 2,422,486 real arXiv papers with real BGE→PCA→UMAP coordinates, real categories and
real author surnames — so the *access terms* are real, which is what makes a containment test mean
something. Seven layers, each earning its place by being the case some stage cannot test without it:

| Layer | Artifacts | Membership | Gate | Content | The case it carries |
|---|---:|---|---|---|---|
| `clusters/hdbscan-2026-08` | 8 / 111 / 884 / ~10⁴ over four levels | enumerated | derived, threshold | count, centroid, hull | the baseline; 20–25% noise means **non-covering in both directions**, which is the shape model §6 exists for |
| `topics/ctfidf-2026-08` | ~3 per cluster, plus per-term variants | enumerated (the sample) | containment | label text from real titles | the containment result that surprises: broad and narrow viewers fail the *same* label |
| `centroids/kmeans-2026-08` | 4,000 flat | enumerated | derived | **supplied** centre+radius fitted over full membership | model §8.6's trap — supplied geometry that looks derived, and the degrade-to-derived behaviour |
| `taxonomy/arxiv-2026-08` | ~60 archives → ~176 subject classes | attribute predicate (the real `categories` column) | substitutive, `public` | authored names | a **covering** hierarchy, zero-count artifacts, and Stage 6's two-membership-sources equality check |
| `regions/synthetic-geo` | ~2,000 | spatial predicate | substitutive | authored polygons | the perimeter cost, and §5.1's "draw all boundaries or gate them" trap |
| `selections/analyst-*` | ~50 | enumerated, scattered | substitutive, per-analyst term | none | model §8.3 — the set of ten that shows seven |
| `programmes/portfolio` | ~30 | enumerated | substitutive, any principal | authored name and extent | model §8.5 — a named programme with a **zero** count and no hull |

**Three things this fixture must get right, each of which has already gone wrong once:**

*The clustering is HDBSCAN on a 250k sample with every row assigned by nearest centroid and the
noise fraction restored by a distance cut* — that is what the campaign ran, and it bounds what the
fixture licenses: the cluster count and noise fraction are HDBSCAN's, the fine boundary detail is
not. Worth one attempt at full 2.4M HDBSCAN on the GPU over the 2-D projection before accepting the
sample; if it does not run, the caveat is carried in the fixture's manifest rather than in someone's
memory.

*Geographic shapes are drawn at native density.* The campaign's 56× corridor figure was its own
subsampler holding member count constant with a stride, which forces one run per member for any
shape. Corrected, real coastlines cost **1.4×** and archipelagos **4.6×** a compact blob. A fixture
that decimates reproduces the bug rather than the shape.

*Labels are real text with a real generating set.* c-TF-IDF over the titles of a declared sample per
cluster gives English topic labels and a `G` that is an actual set of documents — which is the whole
point, since a synthesised `G` cannot fail containment for a real reason. Titles join from the
Kaggle snapshot, so this rides the full-schema rebuild rather than adding a second one.

### 5.3 The larger tiers — synthetic in exactly the way the corpus already is

The 250M and 10⁹ corpora are the real 2.4M repeated as ~103 and ~413 replicas, each an affine
transform plus jitter, with five replicas pinning deliberate edge cases. **Artifacts ride the same
machinery:** each replica carries the Tier A layers through *its own* transform, so a replica's
cluster is a real cluster's shape under an affine map — real size skew, real noise placement, real
Morton contiguity — and the degenerate replicas (extreme compression, corner-pinned, collapsed
line) produce degenerate artifact geometry for free rather than by invention.

Label text per replica is recombined from the real title vocabulary deterministically from the
replica index: it reads like an arXiv topic and is a sentence nobody wrote. Generating sets are
sampled per replica exactly as at 2.4M.

Arithmetic, *modelled*: 413 replicas × 884 at L2 ≈ **3.7×10⁵** artifacts at 10⁹; a fourth level at
`min_cluster_size ≈ 6` is expected to give ~10⁴ per replica, so ≈ **4×10⁶**. Reaching the design's
10⁷ ceiling needs a fifth level or a top-up from the seeded generator — decided by measuring the
fourth level's count, not by predicting it.

**What does not transfer, stated so nobody quotes it later:** cluster *semantics* (a replica's
cluster means nothing), and the fact that the same paper appears 413 times, which makes any
statement about label uniqueness or vocabulary growth at scale a statement about the generator.

### 5.4 Two rules the fixture producer must follow

**A scale is a prefix filter on entity ID**, so membership and generating sets must be materialised
**per scale**, never filtered blindly. A prefix cuts at replica boundaries above 2.4M but cuts
*inside* replica 0 at 250k — where a cluster loses members and a generating set loses members it
declared. Filtering membership is correct; filtering `G` and keeping the old `|G|` makes every label
unsatisfiable, and filtering `G` and recomputing `|G|` is the shrink the design refuses. `G` is
**re-declared** per scale from the members that scale has.

**Membership goes to disk in entity space**, whatever the campaign says about row space. Row space
is per slice and derived; a fixture that ships the row form has frozen a projection and will be
wrong after the first fold.

### 5.5 Who writes what

The seeded generator is **Rust**, in `tessera-corpus`, for the reason that crate already gives: it
defines the corpus rather than the system's behaviour, and two generators would put the fixture
under test. The arXiv layer producer is **Python**, in `probes/`, beside `build_corpus.py` and
`build_scaled_corpus.py` — offline data preparation, which is where Python belongs here. Neither
line moves: nothing Python reaches a request path, and the bundle artefacts are written by
`tessera build`.

## 6. What each stage owes a measurement

The campaign settled storage and dissolved one leak-register escalation. What it did not settle is
allocated here rather than left as a list:

| Owed | Stage | Why it could refute something |
|---|---|---|
| **Residency** — 10⁷ separately allocated bitmaps carry per-object overhead the campaign never measured; 794 MB is *serialised* bytes, and it multiplies by slice, by level, and by two during a replace | 2, re-measured at 8 | the slice budget has no line for it |
| **The fold's artifact pass**, as a comparison: riding pass 1 with the inverted multimap resident against per-artifact translation through a sequentially written mapped table | 4 | the largest unpriced item left; decides whether `plan_fold`'s ~9–10 GB anonymous peak moves |
| **The three arms** — flush-union, merge-rebase, fold-rebuild | 4 | the merge arm's bound is proportional to the merged span, not to the artifact population |
| **Σ\|G\| and containment per request** | 3 | ~4 B/member *assumed*; refuted if a real deployment's Σ\|G\| approaches membership's order |
| **Reach**, if it is materialised at all | 5 | the deleted assignment-column framing made it look free; it is a second membership structure |
| **Predicate evaluation per request** — ~10³ ranges for a 40,000-member corridor, so ~0.3–1.2 ms per artifact per request *(modelled)* | 6 | a nationwide boundary level is seconds without a bound |
| **A real clustering at 10⁹** | never here | Tier A and Tier B agree on direction and not on constant: real membership is 14–170× cheaper per member than the synthetic arm |

Two arms of the existing campaign can be finished now that they could not be then: `data/corpus.parquet`
and `data/geometry.parquet` are both present on this machine, so `m7_core_rows.py` runs, and the
permutation-compression and `Permutation::project` questions the campaign left open are cheap to
close before Stage 1 commits to the tiebreak.

## 7. What could still refute the shape

**Residency, not storage.** Every sizing number in the design is serialised bytes. If 10⁷ resident
bitmaps cost materially more than their serialised form, the fine-level case stops being servable
and the assignment column — documented, deleted, and recoverable in rep §2.7 — comes back for that
regime.

**The fold.** If the artifact pass cannot be fitted inside `plan_fold`'s budget, the choice becomes
a first-toucher stall of tens of seconds per level or blank annotation levels after every nightly
fold, and neither is acceptable. This is the item to measure first inside Stage 4, not last.

**The drill-down conflict.** The resolved visibility set restores the structural closure a
per-identifier count would break, and the live-threshold rule says a cache may bake in counts but
never verdicts. Those two are in tension on exactly one route, and the design says so. It needs a
small design pass during Stage 2, not a decision taken by whoever implements the endpoint.

## 8. Relation to the epics

[#13] and [#41] remain open and describe the same capability from the outside — cluster structure
from the viewer's own visible set, and labels gated on containment. **They are not the status
record for this work**; this file is, by owner direction. If the artifact work is ever moved back
onto issues, this file is deleted rather than left to disagree with them.

[#13]: https://github.com/jennis0/tessera-index/issues/13
[#41]: https://github.com/jennis0/tessera-index/issues/41
