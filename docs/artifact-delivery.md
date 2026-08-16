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
**normative** as of 2026-08-16 (decisions 0074–0083; the architecture amendments they owed are
performed at r43);
[`design/correctness-suite.md`](design/correctness-suite.md) §12 (the fixture
machinery this extends); [`probes/dataset.md`](../probes/dataset.md) (the corpus);
[`probes/2026-08-15-artifact-representation/`](../probes/2026-08-15-artifact-representation/) (the
campaign every sizing figure below comes from). Ordering precedent:
[`design/records-and-search.md`](design/records-and-search.md) §13.

---

## Where it stands

A flat clustering is live end to end: layers register, artifacts publish, and a viewer sees each
cluster with the count its own visible set generates — on the engine, on all three frame decoders,
and on the map. What does not exist yet is everything above one flat level: no content on an
artifact, no edges, no levels and no frontier, and no write cycle keeping any of it true across a
delete.

| Stage | State | Finished when | Evidence |
|---|---|---|---|
| **0** Rulings and promotion | **done** 2026-08-16 — decisions [0074](decisions/0074-row-less-entities-are-allocated-downward.md)–[0083](decisions/0083-the-frontier-is-a-request-time-budget.md) | the three designs are normative and the register carries their rows | [the review](evidence/memos/2026-08-15-artifact-design-review.md), ten rulings, and architecture **r43** — §7.5's descent and §7.7's ladder amended, §8.4's second threshold withdrawn, C1 and C17 annotated, C27 and C28 added. Five ⊘ items stay open **inside** the normative documents, each allocated to the stage that needs it |
| **1** The spine — allocation and the layer registry | **done** 2026-08-16 (`artifacts/stage-1`) | an empty layer is reachable by gate, suppressible at the ack, droppable for ever, and survives restart | **all five bullets built and gate-green.** The tiebreak in both build paths, verified on the real 2.4M corpus; the two-region allocator with both marks durable; the registry seeded from the manifest and replayed over; `PUT`/`DELETE /control/layers`; `/v1/meta`'s gate-filtered list. Eleven tests, of which the disclosure one is that a gate-failed name and a never-registered one are **one identical set probe** |
| **2** One flat level, masked counts | **done** 2026-08-16 (`artifacts/stage-2`) | two principals get different counts for one real cluster, neither equal to its size; below-criterion artifacts are indistinguishable from absent ones | **met on the map.** One 24-cluster k-means over the 2.4M bundle: the same cluster is 4 / 485 / 1,962 / 4,138 / 8,380 members to five principals against 11,008 declared, and under a `min_visible` of 1,000 the same membership serves them 0 / 0 / 8 / 20 / 24 clusters. Engine, server and all three frame decoders; `@tessera/client` and the viewer; one ⊘ open below |
| **3** Content — derived, supplied, containment | not started | both principals fail the same real label and both satisfy its per-term variant | — |
| **4** The write cycle | not started | a deleted source document's label vanishes at the ack and **stays gone** across a fold; the stage battery covers the artifact surface | — |
| **5** Trees, levels and the cut | not started | a passing child sits beneath a failing parent under the proportional criterion and never under the absolute one, and two budgets agree on every artifact both return | — |
| **6** Predicate membership | not started | one layer built by rule and by list returns identical masked counts for every principal and every viewport | — |
| **7** Runtime artifacts | not started | a set of ten shared across a clearance boundary shows seven, and the day-one bookmark survives a hundred edits | — |
| **8** Filters, search, scale | not started | an invisible artifact and a nonexistent one cost the same; 10⁹ points with ~10⁷ artifacts serves and folds inside budget | — |

Stages 5 and 6 touch largely disjoint machinery — edges and display pruning against geometry and the
existing filter path — so they can run beside each other. Nothing else here can.

## 1. What decides the shape of this plan

Two facts do most of the sequencing work, and neither is about clusters.

**One decision cannot be taken later.** Entity IDs are allocated `(signature, source_id)`, and the
campaign measured `(signature, morton)` as **4.08× smaller** on the artifact disk form with term
postings **byte-identical** (M3, *measured*). It cannot be retrofitted under **I9**, so it is taken
before the first build that writes an artifact or not at all. It also renumbers every entity in
every fixture, so it wants to be taken *once*, in the same rebuild as the full-schema 2.4M corpus
[#88] is already committed to.

**All three designs are normative**, as of 2026-08-16. The Stage 0 review's findings are
dispositioned by decisions 0074–0083, the register carries their rows, and the two amendments the
rulings owed the normative architecture are performed — §7.5's descent and §7.7's ladder, with
§8.4's second threshold withdrawn alongside them (architecture r43). The roadmap's gate on this work
is discharged. **Five items stay open inside those normative documents**, marked ⊘ at their sites and
allocated to the stages that need them: search's containment gate and the filter axis (Stage 8),
membership packaging (Stage 2), the proportional criterion's denominator for predicate membership
(Stage 6), and the edit pass (Stage 7). None of them touches the spine, which is why promoting
before they close was the cheaper order — the alternative was holding an implementer on a question
about search.

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

No code, and it is finished. The adversarial review is run, the ruling pass is made (decisions
0074–0083), the register rows are written and the two architecture amendments are performed.

| What | State | Blocks | Note |
|---|---|---|---|
| **Independent review** of the model and the representation | ✔ **run 2026-08-15** — three lenses, [the record](evidence/memos/2026-08-15-artifact-design-review.md) | — | the model's core survived all three; its *derived* rules did not |
| **Review ruling 1** — where artifact entity IDs come from | ✔ **ruled** — [decision 0074](decisions/0074-row-less-entities-are-allocated-downward.md) | Stages 1–4 | row-less entities allocate downward from the top; the repairs from the review's other findings are made |
| **Review ruling 2** — the masked count as an existence criterion | ✔ **ruled** — [decision 0075](decisions/0075-the-masked-count-is-an-existence-criterion.md) | Stages 1–2 | it never suppressed a count: it decides whether the artifact is served. Declared per layer, absolute or proportional, no default, **independent of the own-terms flag** |
| **Review ruling 3** — does a label's existence follow its content? | ✔ **ruled** — [decision 0076](decisions/0076-an-artifact-is-served-whole-or-not-at-all.md) | Stage 3 | wider than asked: **no levels of restriction within one artifact**, beyond ranked variations. C3's question evaporates; the model's degrade-to-derived is deleted |
| **Review ruling 4** — the artifact **edit** mechanism | **deferred to its own design pass** *(owner, 2026-08-15)* | Stage 7 only | not load-bearing: publishing and republishing artifacts needs no edit route. Two consequences, both stated rather than discovered — the emergency path becomes *suppress, then republish* (slower, not weaker), and runtime selections, whose whole lifecycle is editing, wait for it |
| Where supplied content **lives** | ✔ **ruled** — [decision 0077](decisions/0077-supplied-content-lives-in-the-record-blob.md) | Stage 3 | the record blob, at the artifact's entity. Its addressing is rank in the blob's **own** has-row bitmap, independent of row space, so an artifact having no row does not bear on it |
| **Review ruling 5** — how search gates on containment | open | Stage 8 | the term-signature conjunction is the candidate shape; the route stays withdrawn until ruled, which costs nothing before Stage 8 |
| **A hierarchy lives in its edges; levels are resolutions** | ✔ **ruled** — [decision 0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md) | Stage 5 | a condensed tree is unbalanced, so a level number says nothing about lineage. A treed layer declares **no levels**; levels stay for balanced semantic resolutions and for stacked independent analyses, and the two are independent declarations. Rollup then falls out of per-artifact testing — ⊘ under an **absolute** criterion only, since a ratio does not shrink downward |
| **The frontier is a request-time budget** | ✔ **ruled** — [decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md) | Stages 2, 5 | levels had been bounding the response quietly; with the tree in edges a viewport intersects a root and every passing descendant. The cut's depth becomes a request parameter beside the mark budget, met by serving ancestors and never by sampling. **A budget is not a disclosure control** — every artifact it returns passed its own test — which §8.4's maximum depth was, and the two occupy the same place in a request |
| Implement [0072](decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) — slot reuse | **deferred, deliberately** *(owner, 2026-08-15)* | nothing here | not a dependency: artifacts need identity, the deny lane and the opaque identifier, none of which need reuse. Deferring **removes** the recycled-slot fail-open rather than carrying it; the membership-reconciliation clause ships with it whenever it lands |
| **The signature-sort tiebreak** (rep §12) | ✔ **ruled 2026-08-15** — [decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md) | Stage 1 | taken: allocation becomes `(signature, morton_code, source_id)`. Free in the format and *measured* so; what it costs in the build's batch plan is named there and sized in Stage 1 |
| The descent change — per-artifact test replacing §7.5's tree walk | ✔ **ruled** — [decision 0080](decisions/0080-the-frontier-is-a-per-artifact-test.md) | Stage 5 | dropped. Non-covering hierarchies make the walk ill-defined; the rollup promise weakens and the caller supplies a covering top level if they want it. ⊘ §7.5 amendment owed |
| The label ladder reduced to guidance | ✔ **ruled** — [decision 0078](decisions/0078-the-service-takes-no-opinion-on-which-variation.md) | Stage 3 | the service resolves a caller-supplied ordering and chooses nothing. **A variation is a general artifact property**, not a label one |
| The three gate modes | ✔ **ruled** — [decision 0079](decisions/0079-the-gate-is-one-flag-not-three-modes.md) | Stages 1–2 | they were a two-by-two in three names. One flag — does the artifact carry its own terms — beside the independent criterion, which also stops a schema word disabling a disclosure control |
| The suppression-carry refusal | ✔ **withdrawn** — [decision 0081](decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md) | — | the premise was wrong: a refresh need not mint identities. An **edit** keeps them and suppressions survive natively; a **replacement** ends them and nothing carries, correctly. A report replaces the refusal; the stable key is optional again |
| Membership as a filter (rep §7) | open | Stage 8 | a disclosure question, not a cost one |
| Appendix C edits | ✔ **done** — architecture r43 | Stages 2–4 | **C1** annotated twice: a criterion bounds a grouping's existence and shape and **never its count**, which §7.1 and §7.3 already serve exactly, so a compact artifact's masked count is recoverable by summing the underlay whatever it declares; and where several layers cover the same points, the most permissive declaration governs what is recoverable about all of them. **C27** — the own-terms flag, C23-shaped. **C28** — the corpus-independence declaration on supplied content, C12-shaped, `High if mis-declared`. **C17** — an artifact identifier probes the same channel and stays inside the same bound. C7's disposition was already written (r42) |
| `derived-artifact-gating.md` — retired | ✔ **deleted** 2026-08-15 | — | its taxonomy is superseded; what existed nowhere else — the point-scale edge argument, the edge gate's form, the induced-subgraph sampling problem — is carried in the model (§5, §11), and the roadmap names the three successors |

**Two design items are owed and are not rulings.** The entity budget under repeated replacement —
a 10⁷-artifact layer mints 10⁷ IDs per wholesale replacement against a `u32` space (write-cycle §9; the burn is replacement's, not the model's — decision 0081), with
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

**Where the registry got to** (2026-08-16, `artifacts/stage-1`). All five bullets are built and the
gate is green: `cargo test --workspace` at 134 binaries, clippy at `-D warnings`, `check-layers` and
`check-doc-links` clean.

⊘ **One test does not survive the branch.**
`a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero` (server `http` suite)
fails on **both** artifact branches (three runs on `stage-2`, one on `stage-1`) and passes on
`text/owed-work` — so it arrived with this stage rather than with the client work above it. What breaks is the
assertion that a stalled reader's held response must not read as complete; the gauge half of the
test passes, so the shed itself still happens. **Why is not established**: the assertion needs the
body to be large enough to fill the socket and channel buffers before the stall deadline, and the
tiebreak reorders what that fixture's request gathers, but nothing here measures the two response
sizes. It wants whoever owns the streaming test.

**The entity space grows from both ends now, and the reason is not identifier supply.** A flush's row
table, a merge's window and the fold's pre-flight all size themselves over entity *ranges*, so a
row-less run sitting between two point segments is dense waste in a resident array and budget in a
fold that can refuse for it — and under one monotone allocator that interleaving is the *normal*
case. Two regions make it unrepresentable. Exhaustion becomes the two marks meeting, which is the
true condition where a fixed ceiling per region would be a guess about the split.

**Row-less allocation is whole 2¹⁶-aligned blocks and nothing finer.** A level's membership then
sits inside whole Roaring containers; a level straddling a boundary pays for a partial container at
each end on every operation for ever. The top 65 535 ids are unusable by design, with a test saying
so, because "recovering" them would put the first block across a boundary. Layer entities come from
a block the registry holds — mixing widths in the allocator leaves the mark unaligned after every
single-entity allocation.

**Both marks are durable in both homes**, which is the recovery hazard
[decision 0074](decisions/0074-row-less-entities-are-allocated-downward.md) names as the part to get
right: rotation reclaims the `LayerCreate` records the mark is otherwise recovered from, so
`SEGMENTS-<n>.json` gains `entity_id_low_water`, `layers` and `layer_tombstones` — **required, not
defaulted**, because a default makes an absent mark and a *lost* mark the same value, and the
safe-looking default is the ceiling, which is exactly the value that reissues every live layer's
ids. Both names join `HONOURED_STATE` beside the code that acts on them. The fold carries all three
forward untouched: it rewrites the point region, and a layer has no rows to renumber.

**A registration is validate, allocate, append, sync, apply — and nothing is applied before the
record is durable**, which is the opposite of a deny's posture. A suppression is applied even when
its append fails, because leaving an accepted deny unapplied is a fail-open; a layer has no such
asymmetry, since one existing in memory and not in the log comes back from a restart as a free name
having already handed a caller an identifier for its entity.

**Two traps found, each of which would have shipped silently.**

*The admin plane's misdirection guard was written for one region.* It refused any entity at or above
the point mark, so every layer identifier this deployment could issue read as naming nothing — and
the symptom is not a range check, it is that suppressing a layer answers *no such thing*. The issued
space is two ranges; what names nothing is the gap between the marks. Verified load-bearing by
reverting it, which fails two of the six engine tests.

*`skip_serializing_if` is fatal under postcard,* which decodes positionally: omitting an absent
`Option` shortens the record and every field after it reads the wrong bytes, so a layer can replay
carrying a different gate. The rule is now absolute at the site, with a test that turns re-adding one
into a red build.

⊘ **The layer-entity cursor is deliberately not durable.** A `LayerCreate` records which entity a
layer took, not which block it came from nor how much was left, and resuming from `max + 1` is wrong
the moment a drop retires the highest-numbered layer. Reseeding costs at most one block per restart
out of 65 536; a durable cursor would buy back an id space nothing is short of.

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

- ✔ Enumerated membership, entity space only, and the publication that gets it there: a batch is
  one WAL record and one fsync, ordinals are claimed densely from the level's cursor on the
  executor, and a level that outgrows its 65 536-entity reservation appends another block — carried
  in the record, because ordinals walk the runs in *allocation* order and a re-derived extension
  would renumber everything above it. `PUT /control/layers/{name}/artifacts` accepts members by
  external id or `tessera_id`, resolves them once at the boundary, and refuses the whole batch on an
  unresolvable one: a dropped member moves both the count a viewer is shown and the size the
  proportional criterion divides by.
- ✔ **Membership has a home outside the WAL: the packed extent** (owner-ruled 2026-08-16 — the
  packed file, read normally, over both the record store and a mapped form). One file per level per
  publication, addressed by dense ordinal, behind **one** manifest entry — which was the whole of
  §2.4's open question, the bytes having always been affordable and the packaging not. Written and
  fsynced before the manifest that names it; the log is released only once that manifest is durable.
  A truncated extent refuses rather than decoding to a shorter one, because a membership that came
  back short is an artifact with a low masked count for every viewer, which the criterion renders as
  *absent* with no error to notice.
- ⊘ **A node that has published artifacts no longer folds.** The fold publishes a new prefix and
  extent paths are prefix-relative, so carrying them forward names nothing and dropping them loses
  every membership silently. Rebuilding them is `annotation-representation.md` §5.0.3's **fold
  artifact pass** — Stage 4's first measurement, and not a copy, since the fold retires deleted
  entities that a carried membership would go on counting. Until then the fold refuses loudly and
  counts a `fold_failures`. The cost is stated rather than hidden: segment count and tombstone load
  grow on a node serving artifacts.
- ✔ Found in the doing: **the registry reached a manifest only via a flush.** A deny-lane
  publication carried the deny state and not the layers, which was harmless while the WAL held them
  and fatal beside an extent — at open a layer's reserved runs are what turn an ordinal into an
  entity, so the extents would have been skipped whole and every artifact served as absent. Both
  publication paths now carry it.
- ✔ Found in the doing: **both publication paths clone a stale manifest.** A side-manifest write does
  not swap the generation, so a second publication that extended its clone would drop the first's
  entries. The extent list is held as complete current state and assigned, which is the posture the
  deny list already takes for the same reason.
- ✔ The derived **row-space** operator, built member-wise and never range-wise, cached per
  `(slice, layer, level)` and rebuilt when the prefix, the segments version or the store's own
  version moves. Replace-on-mismatch rather than an LRU: the key names the only generation a
  projection is valid for, so a stale entry has no value to keep warm.
- ✔ The visibility predicate in one place, overlay first: `verdict` → layer gate → own terms ∧
  existence criterion (decision 0079), enforced on the **live** count.
- ✔ The count is taken against the **composed** mask, and the type enforces it: the trait the
  predicate reads has exactly one implementor outside a test build, so a count cannot be taken
  against the pre-overlay projection — which strictly contains `M_auth` after any accepted delete.
- ✔ Viewport carries the artifacts whose rows intersect the tile ranges, with masked counts, as a
  frame of its own (kind 5) after the counts and before any point. Drill-down by `tessera_id`
  calls the same predicate — an artifact reachable by identifier but not by viewport would be two
  transcriptions of one rule. Ordinals never cross the wire, and neither does an unmasked size.
- ✔ Contracts work: the artifacts frame, the **artifact budget** request field (accepted, and inert
  on a flat layer — the only reduction the representation allows is structural, and a flat layer
  has no ancestors to cut to), and a **layer selector** beside it, which narrows and never widens.
  The budget is defined here rather than at Stage 5 because it is a wire shape, and adding a
  request field to a shipped frame later is the change this ordering exists to avoid
  ([decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md)).
- ⊘ **No artifact carries its own terms yet**, so a layer declaring `artifacts_carry_own` serves
  nothing on either route. Fail-closed and deliberate: the per-artifact label arrives with content
  at Stage 3, and admitting an unlabelled artifact would make a missing declaration a grant to
  everyone.
- ✔ All three frame decoders — Rust, TypeScript, the Python oracle — know the artifacts frame, and
  `artifacts` is a compared surface in the conformance canonical form, so a determinism break in
  the artifact channel cannot pass every comparison in the suite. ✔ Drill-down is routed at
  `POST /v1/artifacts/{tessera_id}`, one `404` for every withheld case.
- ✔ **The client integration**, briefed in [the client handover](artifact-client-handover.md) and
  landed: `/v1/meta`'s layer list, the `layers` and `artifact_budget` request fields and the
  drill-down verb in `@tessera/client`; a layer control, cluster marks carrying their masked counts
  and a click-through in the viewer. The annotation channel issues its **own** request (`k = 0`,
  one named layer) rather than reading artifacts off the point path's responses, because the
  replica elides tiles it already holds and an elided tile carries no artifacts — clusters would
  have thinned out as the cache warmed. `scripts/publish-clusters.mjs` registers a layer and
  publishes a k-means clustering of the corpus's own points; there is no artifact geometry on the
  wire, so it writes centroids to a sidecar the viewer joins **by served artifact only**.

**The check:** on the real 2.4M clustering, a broad principal and a one-term principal receive
different counts for the same cluster, neither equal to its declared size; clusters below the criterion
are **absent**, not refused, and the response cannot distinguish them from clusters that never
existed. The conformance oracle recomputes every count from the same membership and the same mask
independently. **Data:** `clusters/hdbscan-2026-08` at 2.4M (§5.2), the seeded generator's artifact
arm at 10⁴ (§5.1).

**Met on the map, 2026-08-16**, on the 2.4M demo bundle with a 24-cluster k-means published over
156,828 of its own points. One cluster (`c-0013`, 11,008 members declared) across the five measured
principals:

| principal | visible items | clusters served | `c-0013` |
|---|---|---|---|
| narrow — term 14 | 243 | 5 of 24 | 4 |
| sparse — 1.9% | 35,138 | 17 of 24 | 485 |
| medium — 19% | 360,239 | 24 of 24 | 1,962 |
| heavy — 50% | 929,811 | 24 of 24 | 4,138 |
| full — top 4096 terms | 1,856,276 | 24 of 24 | 8,380 |

No principal's count equals the declared size, because the clustering was published from a
principal broader than any the viewer offers — the top 16,384 ranked terms. Under
`visible_when = {min_visible: 1000}` over the *same* membership the five are served 0, 0, 8, 20 and
24 clusters: presence itself moving with the mask, and the response saying nothing about why.
Reproduced by `clients/ts/viewer/smoke-artifacts.mjs`, which fails if the counts stop moving with
the principal.

✔ **The deployment-wide `min_visible_members` key is deleted, not wired**
([decision 0085](decisions/0085-the-existence-criterion-has-no-deployment-wide-form.md), 2026-08-16)
— the reconciliation architecture r43 deferred to this stage. A deployment default would make an
undeclared criterion mean *inherit this floor* where [decision 0084](decisions/0084-an-undeclared-criterion-declares-no-test.md)
rules it means *no test*, and no layer could then decline it; the proportional form has no
deployment-wide parameter to default in any case. `[disclosure]` stays required and holds
`token_max_lifetime` alone, and the startup obligation the key carried — state your disclosure
parameters, do not inherit them — is discharged at layer registration, which has no default either.
§7.5's threshold stops being ⊘ specified-and-unimplemented and becomes a control that runs
(architecture **r45**).

✔ **A zero masked count is served by the drill-down where the viewport withholds it**, on a layer
that declares no criterion — and that is **ruled correct**
([decision 0084](decisions/0084-an-undeclared-criterion-declares-no-test.md), owner, 2026-08-16).
The viewport rule is *any member visible to this principal falls inside the requested tiles*, so a
cluster this principal can see none of never appears on the map; the identifier route applies the
existence predicate alone, which an artifact with a zero count passes when `visible_when` is
`null`. A declaration of *no threshold* is a declaration, and the service adds no floor of its own:
one that the schema cannot express, applied on the service's initiative, would be a rule nobody
wrote and nobody could turn off. `min_visible = 1` expresses the floor exactly, for a deployment
that wants it. The cost is recorded rather than sheltered — C17's bound moves from *items the
principal already sees* to *the layer's gate*, and the decision carries the three properties that
bound it. No code changed.

### Stage 3 — Content: derived, supplied, and the containment test

**Capability:** a label is served only to a viewer who can see everything it was generated from.

- The declared derived vocabulary — count intrinsic, centroid/hull/box opt-in — under the closure
  rule that a derived property is a function of `membership ∩ M_auth` and nothing else.
- Supplied content in the record blob; **`G` as an immutable sorted entity-space array**, mmapped on
  touch; containment `and_cardinality(G, M_auth) == |G|` against the **composed** mask, cached
  nowhere, and costing the same on the pass and fail paths.
- Ranked variations: one artifact, one identity, the first satisfied served entire — or no artifact
  at all (decisions 0076, 0078).
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

### Stage 5 — Trees, levels and the cut

**Capability:** a layer's lineage lives in its edges, its levels are declared resolutions, and a
viewport returns a cut through the tree that fits what the client can draw.

- Parent/child edges as the hierarchy, with per-artifact criterion testing and no walk
  (decision 0080). A treed layer declares **no levels** and sits at level 0 on one reserved run; a
  levelled layer declares them, and may carry edges as well, which is the administrative case
  (decision 0082).
- The **request-time artifact budget** (decision 0083), in the shape of the mark budget a viewport
  already carries, met by serving ancestors instead of their descendants and never by sampling.
  `prune_children` becomes the layer's default rather than its only setting. ⊘ One depth for the
  whole tree is what this stage builds; a budget resolving to different depths in different branches
  is the honest general case and is unspecified.
- Display pruning as a declared policy per layer, with a per-level override on levelled layers that
  may only **raise** the criterion.
- Build-time containment verification that **reports** violating edges rather than deciding
  anything.
- **A level is served partially and never withheld because part of it is suppressed.**

**The check** has three parts, and the middle one is the reason this stage is not just plumbing.
*The non-covering case on real data:* HDBSCAN's children are subsets of their parents but do not
exhaust them, so a principal holding only the term covering a parent's stray members sees that parent
and no child, and the build's report named the edge in advance. *The criterion's two forms, which
diverge here and nowhere else:* under `min_visible` a passing child never sits beneath a failing
parent, and under `min_fraction` one does — a run that fails to reproduce that gap has not exercised
the proportional form at all. *The budget:* a cut at one depth and a cut at a deeper one agree on
every artifact both return, and neither reveals an artifact that failed its own test. **Data:** the
real clustering's condensed tree, whose non-exhausting splits are a property of HDBSCAN rather than
something planted (§5.2).

### Stage 6 — Predicate membership

**Capability:** a boundary or a tagged set behaves as a layer, with membership derived per request
and never stale.

- Spatial predicate: the shape only, decomposed to Morton ranges, counted by `range_cardinality`.
- Attribute predicate: no new storage — the existing value column and postings.
- The per-request bound the design does not yet specify (§2 above).
- ⊘ The proportional criterion's denominator, which this stage is where it bites: *"the points inside
  this shape"* declares no member set and its size changes at every write. Until it is ruled a
  predicate layer may declare an absolute criterion or none.

**The check:** the tagged-programme layer is built twice — once enumerated, once as an attribute
predicate here — and the two return **identical** masked counts for every principal and every
viewport; a point ingested inside a boundary is a member on the next request with nothing rebuilt.
The layer earns the attribute source by declaring **derived** geometry: a tagged set with nothing to
draw is a category, and the model sends that case to a column rather than to an artifact. **Data:**
`programmes/portfolio` and `regions/synthetic-geo` (§5.2).

### Stage 7 — Runtime artifacts

**Capability:** an analyst assembles a set mid-session, shares it, and edits it without breaking the
share.

- The create/edit control verb (contracts work): `(layer, membership, gate, content, stable key?)`,
  members named by `external_id` or `tessera_id`, resolved at admission.
- The edit verb, from the edit design pass this stage waits on (decision 0077 defers it): content
  in place with `G`; membership in place with a version bump; gate widening in place; **gate
  narrowing = suppress, re-grant, unsuppress**. Identity, edges and suppressions survive every edit
  (decision 0081).
- Optional stable keys; the replacement report (a replacement that strands live suppressions is
  reported, not refused); the dangling-dependent refusal, which binds replacement only.

**The check:** a set of ten shared with a colleague who cannot see three of its members shows
**seven**; a hundred edits later the day-one bookmark still resolves; a replacement that would
dangle an edge is refused with the offending dependents named, and one that strands a suppression
is reported. **Data:**
`selections/analyst-*` and `programmes/portfolio` (§5.2).

### Stage 8 — Filters, search and scale

**Capability:** artifacts are searchable and filterable within their disclosure rules, at the scale
the design claims.

- Membership as a filter — unrestricted for layers declaring no criterion, criterion-inherited for
  those declaring one, and refused work-indistinguishably for a non-visible artifact.
- Search over artifact text via the token index on the artifact population, with C25 re-read against
  a population two orders smaller.
- The 25M / 250M / 10⁹ artifact tiers, built once with the corpus tiers.

**The check:** per-tile counts under a membership filter never resolve below the layer's existence criterion;
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
pathological membership both occur by construction; the own-terms flag and criterion cycled across
artifacts so all four cells of the two-by-two are exercised at every size; a generating set as a keyed ~10²-member subset; deliberate overlaps
within a level, one single-member artifact, one artifact with zero visible members for a chosen
grant, one level with no artifacts at all.

**This is the only family that can carry Stage 4**, because the stage battery compares recorded
answers across eight stages at sizes where no expectation can be stored. It is also the only one
that reaches 10⁹ without a bundle on disk, following the decomposition probe's approach.

### 5.2 Real artifacts over the real 2.4M corpus

The corpus is 2,422,486 real arXiv papers with real BGE→PCA→UMAP coordinates, real categories and
real author surnames — so the *access terms* are real, which is what makes a containment test mean
something. Six layers, each earning its place by being the case some stage cannot test without it:

| Layer | Artifacts | Structure | Membership | Access | Content | The case it carries |
|---|---:|---|---|---|---|---|
| `clusters/hdbscan-2026-08` | ~10⁴ nodes | **a tree in its edges**, no levels | enumerated | no own terms; criterion | count, centroid, hull | the baseline. 20–25% noise means children are subsets of their parents but never exhaust them, which is exactly the shape per-node testing exists for, and the tree the budget cuts |
| `topics/ctfidf-2026-08` | ~3 per cluster, plus per-term variants | attached by edge | enumerated (the sample) | no own terms; no criterion — containment decides | label text from real titles | the containment result that surprises: broad and narrow viewers fail the *same* label |
| `centroids/kmeans-2026-08` | 4,000 | flat | enumerated | no own terms; criterion | **supplied** centre+radius fitted over full membership | model §8.6's trap — supplied geometry that looks derived; a viewer failing its containment sees no artifact (decision 0076), and the everyone-visible remedy is a last-ranked variation |
| `regions/synthetic-geo` | ~2,000 over three scales | **levels *and* lineage** — they agree | spatial predicate | own terms `public`; no criterion | authored polygons and names | the administrative case decision 0082 names, where a level number and a tree depth mean the same thing — the only shape in which reading one as the other is safe. Also the perimeter cost and §5.1's "draw all boundaries or gate them" trap |
| `programmes/portfolio` | ~30 | flat | **attribute predicate**, and an enumerated twin built from the same rule | own terms: any principal; no criterion | authored name, **derived** extent | Stage 6's equality check — the same layer by rule and by list must return identical masked counts — and model §8.5's programme with a **zero** count and no hull |
| `selections/analyst-*` | ~50 | flat | enumerated, scattered | own terms: per-analyst; no criterion | none | model §8.3 — the set of ten that shows seven |

**A seventh layer was dropped rather than descoped.** arXiv subject classes were carrying the
attribute-predicate arm, and the configuration exercise found the model sends exactly that case to a
**category**: the value belongs to the corpus's own vocabulary, thousands of items carry it, and
nothing is drawn. A fixture built on it would have tested artifacts against a thing the design says
should not be an artifact. The tagged programme takes the arm instead, and earns it only because it
declares derived geometry — without something to draw it is a category with extra steps.

**One register claim is checked without a fixture of its own — ✔ done 2026-08-16.** C1 records that
a criterion bounds a grouping's existence and shape and never its count, because the density
underlay already serves exact masked counts at any depth. The check was therefore against machinery
that already exists, and needed no new layer:
`a_withheld_compact_cluster_has_its_count_recovered_from_the_underlay` publishes a cluster whose
membership is every point in one depth-1 cell, sets a bar the narrow principal cannot clear, and
finds the artifact **absent** while that principal's own tile count over the cell is **exactly** the
number withheld — 816 of them. The point is that the recovery succeeds and that the register says
so; the assertion that would fail is the one where the two disagree, which would mean C1 overstates
the exposure. Nothing here is a leak — every number summed is a masked count the principal already
holds — and the value of the test is that it fails the day anyone describes the criterion as
protecting counts.

**Four things this fixture must get right, each of which has already gone wrong once:**

*The clustering ships as a condensed tree, not as cut levels* — and this is new, from
[decision 0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md). The campaign
produced three independent HDBSCAN runs at three `min_cluster_size` settings, which is the **stacked**
case and has no lineage at all; the fixture this stage needs is one run whose condensed tree is
exported with its parent/child edges intact. That is the shape the library already computes and the
one an earlier plan discarded by cutting it into levels. Both fixtures are worth having — the stacked
one is a real configuration and tests the other half of §6.2 — but they are **two layers, not one**,
and the treed one is the baseline.

*The clustering is HDBSCAN on a 250k sample with every row assigned by nearest centroid and the
noise fraction restored by a distance cut* — that is what the campaign ran, and it bounds what the
fixture licenses: the cluster count and noise fraction are HDBSCAN's, the fine boundary detail is
not. ⊘ **Nearest-centroid assignment does not extend the condensed tree**, which is a consequence of
the change above rather than a known problem: a row assigned to a leaf by distance has no place in
that leaf's ancestry unless the assignment is propagated up the edges, and whether that reproduces
HDBSCAN's own membership is unverified. Worth one attempt at full 2.4M HDBSCAN on the GPU over the
2-D projection before accepting the sample, which would dissolve it; if that does not run, the
propagation rule and this caveat are carried in the fixture's manifest rather than in someone's
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

Arithmetic, *modelled*, and **restated on tree nodes rather than cut levels**
([decision 0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md)): the count
that matters is a condensed tree's total node count per replica, not a chosen level's width. At
`min_cluster_size ≈ 6` the 2.4M tree is expected to carry ~10⁴ nodes, so 413 replicas give
≈ **4×10⁶** artifacts at 10⁹. Reaching the design's 10⁷ ceiling needs a smaller
`min_cluster_size` or a top-up from the seeded generator — **decided by measuring the tree, not by
predicting it**, and the measurement is now one number per run rather than one per level, which is
the small practical gain the ruling brings here.

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
| ✔ **Residency** — **measured 2026-08-16**: ~80–94 B per Roaring container, flat over 10⁴–10⁷ artifacts, so **6.16×** serialised on contiguous membership and **7.79×** on the synthetic arm. 794 MB is ~4.9–6.2 GB resident; the pessimistic arm at 10⁷ artifacts does not fit in 47 GB. [The probe](../probes/2026-08-16-membership-residency/README.md) | 2 ✔, re-measured at 8 | the slice budget still has no line for the multipliers, which stand on top of this |
| **The fold's artifact pass**, as a comparison: riding pass 1 with the inverted multimap resident against per-artifact translation through a scatter-built mapped table (rep §5.0.3's corrected posing) | 4 | the largest unpriced item left; decides whether `plan_fold`'s ~9–10 GB anonymous peak moves |
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

**Residency, not storage — answered, and it does not refute the shape.** Every sizing number in the
design is serialised bytes; the question was whether 10⁷ resident bitmaps cost materially more.
They cost **6.16× on contiguous membership and 7.79× on the pessimistic arm**, flat across three
decades, because the cost is ~80–94 B per Roaring *container* rather than per artifact or per member
([the probe](../probes/2026-08-16-membership-residency/README.md)). The design's own point is 3.6 GB
resident against 582 MB serialised — large, bounded, and servable. **The assignment column stays
deleted.**

What the number does change is the packaging question below it. Serialised is 15.2 B/container
against 94.0 resident, so a form usable *in place* — mapped rather than deserialised — costs roughly
its disk size and moves the cost from anonymous memory to reclaimable page cache. That is a ~6×
argument for the frozen-format option, and it is now measured rather than aesthetic.

**What is not answered** is the multipliers: by slice, by level, and by two during a replace. The
engine also holds two resident copies today — the entity-space store and the row-space projection —
so the design's point is 3.6 GB *per copy* before any multiplier. Re-measured at Stage 8.

**The fold.** If the artifact pass cannot be fitted inside `plan_fold`'s budget, the choice becomes
a first-toucher stall of tens of seconds per level or blank annotation levels after every nightly
fold, and neither is acceptable. This is the item to measure first inside Stage 4, not last.

**The drill-down conflict.** The resolved visibility set restores the structural closure a
per-identifier count would break, and the live-count rule says a cache may bake in counts but
never verdicts. Those two are in tension on exactly one route, and the design says so. It needs a
small design pass during Stage 2, not a decision taken by whoever implements the endpoint.

## 8. Relation to the epics

[#13] and [#41] remain open and describe the same capability from the outside — cluster structure
from the viewer's own visible set, and labels gated on containment. **They are not the status
record for this work**; this file is, by owner direction. If the artifact work is ever moved back
onto issues, this file is deleted rather than left to disagree with them.

[#13]: https://github.com/jennis0/tessera-index/issues/13
[#41]: https://github.com/jennis0/tessera-index/issues/41
