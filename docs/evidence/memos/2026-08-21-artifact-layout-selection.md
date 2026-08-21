# Choosing a layer's serving layout

**Date:** 2026-08-21 · **Status:** Design — **reviewed and amended** (2026-08-21;
[the record](2026-08-21-artifact-serving-scale-review.md)), together with
[`artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md). Nothing in it is built,
and §9 is the list of constraints the build wave inherits.

Ruled by
[decision 0094](../../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md):
the layout is chosen automatically, recorded per (layer, level), overridable per layer, and
re-evaluated at every compaction fold. That decision settles *the shape of the mechanism*. This
memo is the surface — what the enum holds, what the automatic pick reads, how the key is spelled and
what it refuses, and where in the fold the re-evaluation sits.

The measurements every claim below rests on are the scale memo's §4.2, §4.4 and §5, and the probe
they come from is
[`probes/2026-08-20-artifact-serving-scale/`](../../../probes/2026-08-20-artifact-serving-scale/README.md).

## 1. What is being selected, and what is not

A **layout** is how a level's membership is stored and scanned. Three of the structures the scale
campaign proposes are *not* alternatives and are not in the enum:

- **The containment partition** (§4.2) is built for every layer whatever its layout. It answers
  `G ⊆ M_auth` from terms alone, per `(artifact, rank)`, and names no principal
  ([decision 0093](../../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md))
  — but it is not the whole answer: `denied = deleted ∪ suppressed` is applied live against the
  overlay beside it, and the entity→artifacts index that needs is sized by `Σ|G|` whatever layout the
  level is in.
- **The hierarchical row-range index and the per-artifact extents** (§4.4) belong to the
  artifact-major layout alone. A row-major layer has nothing to index: its candidacy is a scan of
  `viewport ∩ M_auth`, which is already bounded by the viewport.
- **The row form itself** stays what it is. The row-major layouts do not replace the artifact-major
  form as a *store*; they replace it as the thing the request path walks.

⊘ **None of the three is built.** Today every layer is served from cached artifact-major row forms,
containment is computed live per artifact per request, and there is no index.

**One reconciliation for a reader arriving from Stage 6.** The delivery record's Stage 6 discussion
names a *column-histogram route* — one masked histogram over the `attrs/` value column a predicate
names, answering every artifact at once, measured at 175 ms against 462. That route and this memo's
**row-major label** layout are the same structure described at two grains: a label per row, scanned
for candidacy and histogrammed for counts. The scale memo generalises it in one direction — a
**list** per row, which extends it from partitioning layers to overlapping ones — and moves it from
entity space to row space, which is where ~120 ms of that 175 went.

## 2. The enum, and its grain

⊘ **Specified here, not implemented.** `ServingLayout` lives in `tessera-types` beside
`MembershipSource`:

| variant | the membership is | candidacy | count | what it does **not** replace |
|---|---|---|---|---|
| `ArtifactMajor` | one row-space bitmap per artifact | the hierarchical index, then the extent test, then the composed probe at the viewport's edge | `masked_count` per served artifact | — |
| `RowMajorLabel` | one artifact label per row | one scan of `viewport ∩ M_auth`, marking labels | one histogram over `M_auth`, held per (session, layer) | the generating set (entity space); the proportional denominator (per artifact); the containment partition |
| `RowMajorList` | a list of labels per row | the same scan at a larger constant | the same histogram at a larger constant | the same three |
| `SpatialRanges` | the declared shape decomposed to Morton ranges | range-against-tile arithmetic | `count_range` per artifact | the generating set; the declared denominator, which a predicate does not have at all |

**Recorded per (layer, level)**, as `layouts: Vec<ServingLayout>` on the registered layer, which the
manifest already serialises. A treed layer's coarse level and its leaf level have different
populations and different locality, and one record for the layer would average two different
problems.

**The label/list split is not a choice.** It follows from whether the membership is single-valued: a
partitioning layer has exactly one label per row and an overlapping one does not. So the enum has
four variants and the override, below, spells three families.

## 3. The automatic pick

Four inputs, and the last two are observations rather than declarations.

| input | where it comes from | when it is known |
|---|---|---|
| membership source — `enumerated`, `spatial`, `{ attribute = f }` | the layer's declaration (`configuration.md` §1) | parse |
| single- or multi-valued | the attribute's own declaration; an enumerated layer is multi-valued unless the member source is a scalar key column | parse |
| **blocks per artifact** at this level | counted over the built row form — the same number decision 0092 makes every build report | build, and again at each fold |
| **artifact count** at this level | the level's cursor, after retirements | build, and again at each fold |

⊘ **The proposed rule**, stated so a reviewer can attack it rather than infer it:

- `spatial` → `SpatialRanges`. The ranges *are* the membership, so this is a definition rather than
  a choice.
- Otherwise, **blocks per artifact decides**, and the artifact count is the tiebreak. Above roughly
  a few row blocks per artifact the layer has no row-space locality, the index and the extent test
  buy nothing, and the row-major form is both faster and — at 10⁹ points — the only form that fits:
  4 GB against 78.5 (⊘ *derived* from the residency campaign's measured 78.5 B per container, not
  measured at 10⁹). Below that, artifact-major, where whole-map zoom is answered without a scan at
  all.
- Row-major requires a per-row source to be cheap. Where the layer has one — an attribute column, or
  the integer-key or list column `artifacts-from-points` already reads — the build was handed the
  layout and keeps it. Where it does not, producing one is an inversion of the whole level.

⊘ **The threshold is not a number yet, and "a few row blocks per artifact" is a placeholder.** Every
recorded run sits at 1.0–1.6 blocks per artifact or at 10.0–96.8, both ends constructed by the
generator rather than observed, and **no measurement exists between 1.6 and 10** (scale memo §5).
So the *axis* is measured — it separates the two costs by two decades — and the point on it where the
pick should flip is unbracketed. Row-major costs ~4–5 ns per visible row and is flat in the artifact
count; artifact-major costs blocks per artifact and is flat in the corpus size; the crossover is
measured at 10⁸ points and modelled above it. Until the campaign's sweep brackets it, the pick should
be conservative in the direction of what is built today.

## 4. The override

⊘ One optional key on `[[layer]]`, alongside `membership` and `hierarchy`:

```toml
layout = "artifact-major"   # or "row-major", or "spatial"
```

Absent, the pick is automatic. Present, it pins every level of the layer, at the build and at every
fold after it. Three words rather than four, because the label/list split is a consequence of the
membership and not a choice an author can make.

### 4.1 What is refused, and what is only reported

The line follows the house rule rather than the shape of the table: **refuse what the layout cannot
represent; report what it merely makes expensive.**

| pinned | layer declares | outcome |
|---|---|---|
| `spatial` | a membership that is not `spatial` | **refused at parse** — there is no shape to decompose |
| `row-major` | `membership = "spatial"` | **refused at parse** — a spatial predicate has no per-row source, and inverting the ranges to a column would materialise the membership the ranges exist to avoid |
| `artifact-major` | anything | accepted; always representable |
| `row-major` | `{ attribute = f }` | accepted — `RowMajorLabel` where `f` is single-valued, `RowMajorList` where it is not |
| `row-major` | `enumerated`, with a per-row source | accepted |
| `row-major` | `enumerated`, with no per-row source | ⊘ **open** — representable, by inverting the level at build. Proposed: accepted and **reported with the inversion's cost**, not refused |

**A pin is refused rather than ignored** where it is refused at all. An ignored pin is the silent
case: the operator declared a layout, got another, and has nothing to look at. That is the same
argument `deny_unknown_fields` already carries across the configuration surface, and the cost of
being wrong is a config edit.

⊘ **One thing parse cannot see**, and it decides a cell above: whether an attribute is single- or
multi-valued may not be knowable from the declaration alone in every spelling. Where it is not, the
refusal becomes a build-time report rather than a parse refusal, which is worse ergonomically and no
less correct. This wants checking against `configuration.md` §1's actual attribute block before it
is built.

## 5. Re-evaluation at the fold

**Where: inside the artifact pass, before the registry snapshot.** The fold's real order is
`rewrite_membership_extents` — *which writes the files* — then the degradation report, then the
segments assembly, then `registry_for_publication`, then the manifest, then the `CURRENT` flip, then
retirement, then the warm. An earlier revision of this section placed the re-evaluation "between the
artifact pass and the row-form rebuild", which is **after** the files and **after** the manifest: the
choice would have reached neither, and the fold would have published a level in the old layout with a
record claiming the new one.

So the decision is taken **inside** the artifact pass, before a byte is written. The observations it
needs are available there: `repack_all` takes `&self`, so the retired-out blobs — the memberships as
they will be, minus what the fold executes — exist as values before anything is serialised, and both
inputs can be counted off them.

**What it reads.** The artifact count and the blocks-per-artifact figure, both observed
**post-retirement**. A fold that retired most of a level has changed the answer, which is exactly
the case the re-evaluation exists for.

**What makes a disagreement loud rather than silent.** `MembershipExtent` carries a **layout tag**, so
the manifest states which form each level's file is in; and each layout's file format carries a
**distinct magic**, so a reader that opens a file the manifest mis-describes refuses at the first
bytes rather than decoding a `u32` column as a bitmap. Neither is compatibility machinery — there are
no old bundles ([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md))
— both are the fail-closed guard a *running* process needs, of exactly the kind that rule keeps.

**What it does.**

- **An override is read, never re-derived.** A pinned level is rebuilt in its declared layout and
  the observations are recorded beside it, so an operator can see what the automatic pick *would*
  have said.
- **A flip drops the level's cached projections explicitly.** The projection cache is
  replace-on-mismatch: an entry is evicted when its key is next asked for. A level that flipped to
  row-major is never asked for in the artifact-major form again, so nothing ever evicts it and the
  last generation's `Arc` pins it for the process's life.
- **A flip does not bump the layer version.** That version gates reachability and is a fail-closed
  guard against a reader holding a stale idea of a layer; a layout is not a client-visible fact — no
  request field names it, and both layouts answer identically, asserted ordinal for ordinal and
  count for count. Bumping it would make every client re-resolve a layer for a change none of them
  can observe.
- **The record persists in the fold's manifest**, which is the manifest that also names the files
  the new layout wrote. The two cannot disagree, because they are written together.

⊘ **What a flip costs is not measured.** The fold rewrites the level whole either way, so the flip's
marginal cost is the difference between writing a row form and writing a column — plausibly small,
and stated as plausible rather than as measured. The campaign's serving-during-fold run is where it
becomes a number.

## 6. The deferred control verb

⊘ **Specified, not implemented, and deliberately not in this campaign.** An operator who wants a
layout changed on a running bundle sets the override, and the change takes effect **at the next
fold** rather than immediately. The verb is therefore a write to the layer record and not a rewrite
of a level, which is what makes it a small piece of work in the runtime-artifacts stage (delivery
Stage 8) rather than a large one here.

The alternative — forcing a rewrite of a live level on demand — is the operation the fold exists to
batch, taken while the level is being served, for a latency gain the next fold would have delivered
anyway. There is no case for it that a nightly fold does not already answer.

## 7. What this deliberately does not do

- **It does not put the layout on the wire.** No request field names one, no response says which
  served it, and `/v1/meta` does not carry it. A client that could tell the layouts apart would be
  reading a fact about storage.
- **It does not make the layout a disclosure control.** Both forms compute the same quantities from
  inside `M_auth` — the principal's visible set — so **I2** is untouched. What is true is that
  **nothing on the wire names a layout**; what is *not* true is that the choice is outside the leak
  register. Two annotations cover it (`architecture.md` Appendix C, owner-approved 2026-08-21): a
  **C4-shaped** one, because the candidate-generator walk makes artifact-path service time vary with
  where in row space artifacts the viewer cannot see happen to sit; and a **C15-shaped** one, because
  a layout flip at a fold is detectable in the timing channel — about one bit per (layer, level) per
  fold, about corpus shape rather than content, bounded by the layer gate.
- **It does not bound anything.** A layer whose shape suits no layout is reported and served
  ([decision 0092](../../decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)).
- **It does not re-evaluate on publication or on ingest.** Both move the observations; neither is a
  cheap moment to rewrite a level, and a layout that flips on a write would flip back on the next
  one.

## 8. The questions this document wants attacked

Five, in the order they would hurt.

1. **Is the fold the only flip point?** A level that grows past the crossover between two folds is
   served in the wrong layout until the next one — which on a nightly fold is at most a day. Is that
   acceptable, or does a large enough divergence want its own trigger?
2. **Is the override the right grain?** Per layer is proposed because that is where a declaration
   lives, but the record is per (layer, level), so an operator cannot pin one level of a treed layer
   without pinning all of them.
3. **Is `enumerated` with no per-row source a refusal or a report?** §4.1 proposes a report. The
   inversion is a whole-level pass, which is the largest thing in this document that is not a
   refusal.
4. **Does the automatic pick need the corpus size as a fifth input?** Blocks per artifact and
   artifact count are both level-local; the row-major scan's cost is `O(visible rows)`, which is
   not.
5. **Does anything else belong in the enum?** The scale memo's structures are three; a fourth
   layout proposed later would have to be added to a manifest field, which is free pre-release
   ([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)) and
   not free afterwards.

## 9. What the build wave inherits

Twelve constraints the review recorded rather than settled. Each is small enough to lose and large
enough to cost a rewrite if it is lost, and none of them is a design question still open — they are
things the implementation must not be free to decide differently.

1. **One snapshot and one lock for the whole per-generation family.** The row form, the index and the
   extents describe the same population, and a **growth** between two reads leaves a stale-narrow
   extent — an artifact whose membership reaches beyond the `(min_row, max_row)` a request is testing
   against, which settles it wrongly. Growth is the only producer of that state (`artifacts-from-points`
   §6.1), and it is why the three are acquired together rather than each on its own.
2. **`ArtifactProjections` and `Lineages` need a `forget(layer)`.** A dropped layer's entries are
   otherwise pinned by the last generation's `Arc` for the process's life, and the flip in §5 has the
   same problem in the other direction. The hazard to name at the call site is **drop and
   re-register**: a layer name reused after a drop must not alias the old layer's cached objects. The
   cadence track carries this and is briefed on it; this section exists so the two do not diverge.
3. **`None` is a hole, not an empty membership.** The extent vector is dense over ordinals, and an
   ordinal may be a hole — a deleted artifact whose slot is held open because an ordinal is identity
   (write cycle §3.4, Rule F's artifact arm). A hole must not read as an artifact with an empty
   membership: the first is absent, the second is served with a zero count where the layer declares no
   criterion.
4. **Expression identifiers are interned and canonicalised at build**, with a width of **at least
   `u16`** and a stated rule for what makes two expressions equal. The scale memo's §4.2 records why a
   byte is not enough; the canonical form is what makes sharing an answer sound.
5. **The wide/narrow switch is a constant with a measurement behind it and no home in the design.**
   The probe uses `settled + open > max(rows / 64, 4096)` to choose between unioning the satisfied
   expressions and testing each candidate. It appears nowhere in the scale memo, and a build that
   picks a different constant measures a different system.
6. **The index's fan-out and depth are the probe's**, `FINEST_SHIFT = 10` and `LEVEL_STEP = 4`. The
   scale memo's §4.4 argues that a fixed granularity is wrong at most scales; that argument applies to
   these two constants, and neither is measured at any other value.
7. **`everywhere` is part of the index, not an implementation detail of the walk.** Artifacts too wide
   for any node are returned on every request whatever the viewport, and they are what makes a
   scattered layer expensive at whole-map zoom — the one direct run at the target spends 137 s there
   (scale memo §7.2). A rebuild that folds them into the root's subtree loses the distinction the cost
   model rests on.
8. **The row-major file formats are a durable surface**: the label width per `configuration.md` §1's
   `u8`/`u16`/`u32` declarations, an explicit **HOLE** sentinel for a row belonging to no artifact,
   the list layout's own encoding, and one manifest entry per level carrying the layout tag and the
   format's magic (§5).
9. **What the index, the extents and the partition add to the fold's artifact pass is unpriced.** The
   pass is measured at 32.8 s on eight threads for the memberships alone; three derived structures
   ride on top of it, and no figure covers them.
10. **Where the build reads `vis(e)` at composition time is an open question with a soundness
    consequence.** The partition composes `⋀ vis(e)` over a generating set, and the candidate shape is
    the entity's **term signature**, with per-signature satisfaction asked once per session. **Whether
    a plugin may make satisfaction non-signature-shaped decides whether the partition is sound at
    all** — a plugin whose answer depends on something other than the satisfied term set breaks the
    equivalence the whole structure rests on, and that reaches **I5** and **I6**. Nothing in the
    corpus currently forbids it, and no plugin exists to test it (`conformance.md` r15: I6 is the one
    uncovered row that wants an implementation rather than a test). **This is the first thing to
    settle in the build wave**, because everything else in the partition is downstream of it.
11. **The masked-count histogram is byte-budgeted and per (session, layer).** ~4 B per artifact, 4 MB
    at 10⁶ and 40 MB at 10⁷, on the session-geometry refresh cadence and inside the same byte budget
    the row-projection cache answers to — the single exception
    [decision 0093](../../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
    names, and it applies to row-major layers alone.
12. **The deny correction is on the accept path or it is fail-open.** `deleted ∪ suppressed`, live
    against the overlay per request or applied synchronously with the acknowledgement; the inverted
    entity→artifacts index it needs is sized by `Σ|G|` and contradicts `annotation-write-cycle.md`
    §4.5's *"the deny lane does no artifact work"*. Reconciling those two is work, not a detail.
