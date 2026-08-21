# Serving artifacts at scale — the review

**Date:** 2026-08-21 · **Status:** Review record — evidence, not normative. **Findings are
dispositioned**; the amendments they produced are in the documents themselves and this file is the
account of what was attacked and what survived.
**Reviewed:** [`artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md) (the
design), [`2026-08-21-artifact-layout-selection.md`](2026-08-21-artifact-layout-selection.md) (the
selection surface), and decisions
[0092](../../decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md),
[0093](../../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md) and
[0094](../../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md),
all one day old and none of it built.
**Method:** three independent reviewers, distinct lenses — disclosure, correctness and lifecycle,
claims audit — none seeing another's report, each briefed to refute rather than approve. Every
headline finding was checked against the probe source or the run logs by the controller before being
written down; claims that did not survive that check are not here.
**Owner rulings taken on the day:** two, recorded in §3.

---

## The headline

**The construction survives and two of its claims did not.** The separation by cadence — request,
generation, and nothing per token — is what all three lenses attacked, and none of them broke it. The
hierarchical index, the downward walk, the row-major inversion and the build-time term partition are
all still the answer.

**Two fail-opens, both in the unbuilt design rather than in shipped code.** The settled half of the
candidate walk answered a question the structures actually kept do not answer, and the containment
partition's deny correction was written as a refinement when it is the partition's acceptance test.
Neither can leak today, because none of it is built; both would have leaked as specified, and the
probe that measured the design shared the first of them.

**And the figures are ahead of the measurements.** The claims audit found no fabricated result, and
a dozen figures that say more than their run does: an assembled table read as a measured one, two
tables produced by the very route decision 0093 deletes, a ridge quoted as one number where three
runs give a range, a fixture axiom quoted as a corpus property. **Every grid in §7 is superseded
pending re-measurement on the corrected probe**, and each is marked where it stands.

## 1. The two fail-opens

**The settled half tested containment where it needed the mask** (all three lenses; controller-
confirmed against the probe source). §4 step 2 said a settled artifact — one whose membership lies
wholly inside the viewport — was *"done"* on `settled ∩ passes`, because *"the token structure
answered"* whether it had a visible member. That structure was `architecture.md` §8.5's per-token
servable-label set, which 0093 deletes. What replaced it answers `G ⊆ M_auth`, a question about the
artifact's **generating set**; the question the request needs is `|membership ∩ M_auth| > 0`, about
its **members**. An artifact whose generating set the viewer contains and whose members are all
outside `M_auth` therefore passes and is served — a grouping announced to a viewer who can see
nothing inside it.

The probe's routes `grouped` and `hoisted` (G and H) put settled artifacts into `passing` on
containment alone, so **§7's grids measure the cheaper thing**. The equality assertion that would
have caught it covers route E, which does perform the masked test; and the fixture plants 32
signature groups, draws every generating set from inside one, and derives masks from the same 32, so
containment and non-empty masked membership coincide and nothing disagrees.

**Disposition:** §4 step 2 is rewritten. Candidacy is one early-exiting `Bitmap::intersect` against
`here = viewport ∩ M_auth`, composed once per request — the hoisted form — and for a settled artifact
that probe *is* `masked_count > 0` exactly, because intersecting with the viewport removes nothing
from a membership already inside it. §4.1's second bullet keeps containment as **the collapse that
makes one probe exact for two questions** and no longer as an answer; the index keeps its
superset-filter role unchanged. The probe correction is a separate track.

**The deny correction is the partition's acceptance test.** Both the design's §4.2 and 0093 carried
it as a ⊘ *"two things the arm does not model"*, naming **suppression** alone and proposing a refresh
*"when the overlay changes"*. Three corrections, all dispositioned into both documents: it is
`denied = deleted ∪ suppressed`; it is evaluated live against the overlay per request, or applied
synchronously with the acknowledgement, because `annotation-write-cycle.md` §3.4 puts containment's
response to a deny at **accept** and any refresh window is fail-open for its length; and an
**unsuppress re-derives** rather than subtracts, since delete → suppress → unsuppress must leave the
entity deleted. The index it needs — entity → the artifacts whose generating set holds that entity,
sized by `Σ|G|` — is recorded as being in tension with `annotation-write-cycle.md` §4.5, whose whole
content is that the deny lane does no artifact work. **Recorded, not resolved**: the build wave
reconciles them, and it is the largest open question the design now carries.

## 2. What else the design owed

Six findings that changed the design without being fail-open.

| finding | disposition |
|---|---|
| Containment is per `(artifact, rank)` and returns the first satisfied rank, not a boolean — ranked contents are the contract (0076, 0078) | §4.2 restated at that grain; storage multiplies by the mean content count; the 10 MB / 40 MB figures superseded |
| The distinct-expression count is **unmeasured**, and the fixture's thirty-two was an axiom, not a result | §4.2 and 0093 mark it ⊘; the identifier is at least a `u16`; the dangling *"§7.8's per-term generating set"* citation is corrected to `annotations.md` §8.1, which offers the per-term variant as a **mitigation an author may adopt** rather than a rule, so the claim is weakened accordingly. The demo corpus holds 54,791 distinct signatures over 2.42M items |
| The partition had no stated cadence, key or build cost | Keyed `(prefix, view, store_version)`, rebuilt in the fold's artifact pass and at store-version bumps; `G` shrinks at every fold on a permissive layer; the projection-loss flag folds in per view; the build cost is marked unmeasured, and the parity table's *"(no setup)"* header corrected |
| The fold flip was placed after the files and after the manifest | Moved **inside** the artifact pass, before `registry_for_publication`; the fold's real order is recorded; `repack_all` takes `&self`, so the observations exist before a byte is written. `MembershipExtent` gains a layout tag and each format a distinct magic, so a manifest/file disagreement is a refusal rather than a misread |
| §10 said the write path does not change | Rewritten: the row-major layouts are a **durable** form touching four seams — the online extent writer, the fold's repack, the open-time seed, and the fold's row renumbering, which must permute a row-addressed column. Two quantities have no row-major form and keep artifact-major structures beside the column: the proportional criterion's denominator (corpus-wide and constant per artifact, so cheap to store that way) and `generated_from` |
| The blocks-per-artifact axis has **no measurement between 1.6 and 10**, and both endpoints are generator-imposed | The automatic pick's threshold is ⊘ in the selection memo until a sweep brackets it; 0092 and 0094 carry the caveat where they quote the statistic |

## 3. The two owner rulings

**One exception to 0093, named rather than argued from the general rule.** A **row-major** layer may
hold a masked-count histogram per `(session, layer)` — ~4 B per artifact, 4 MB at 10⁶ and 40 MB at
10⁷ — byte-budgeted like the row-projection cache and refreshed on the session-geometry cadence. The
general rule stands: nothing else per token is sized by the artifact population. What earns the
exception is that a row-major layer has no other route to the whole-membership count the disclosure
rule requires — an artifact-major layer prices the count per served artifact, where a row-major one
can only histogram the whole layer — so declining it would pay the same work per request instead of
once a session. Amended into 0093 in place, that decision being a day old and never merged.

**Two leak-register annotations approved.** Appendix C is owner-only; these two edits were approved on
the day and `architecture.md` r49 says so. A **C4-shaped** annotation records that the artifact path's
candidate generator makes service time vary with the row-space placement of artifacts the viewer
cannot see — the walk uses geometry only to choose which question to ask, and the answer stays masked,
but its cost is a function of unmasked artifact density at the viewport's boundary. A **C15-shaped**
annotation records that a layout flip at a fold is detectable in timing: about one bit per (layer,
level) per fold, about corpus shape rather than content, bounded by the layer gate and accepted on
C15's own basis.

With them, three sentences are narrowed to what is true — 0092's *"the choice carries no disclosure
content"*, 0094's *"a flip is not a client-visible event… no client can tell which one served it"*,
and the selection memo's *"there is no leak-register row here"*. All three become **nothing on the
wire names a layout**, with the register citations beside them.

## 4. The claims audit

No result was fabricated and no ratio was inverted. What the audit found is a document quoting its
measurements more confidently than the runs support. The corrections, applied in place:

- **§7.1's stage table is an assembly** — a 10⁸-point verdict row and an `artifact_cut_cost` cut row
  added together — so its totals are withdrawn and §7.2/§7.3 are named as the measured grids.
- **§2's "0.49 ms and 13.5 ms" and §7's "flat in the corpus size" table are the per-token route**
  0093 deletes, with that route's session setup excluded; both are withdrawn as evidence for this
  design, and the grouped route was never run at 10⁶ artifacts.
- **The ridge is 232–279 ms across three recorded runs**, 232.4 being the minimum, and several cells
  of §7.2 are per-cell minima over two routes. Run-to-run spread reaches 2.3× at the wide cells.
- **The one direct scattered run at the target contradicts a claim it sat beside.** 137 s of
  candidacy, grouped, whole map, full mask, 10⁷ scattered artifacts over 10⁹ points — and the run did
  not complete. *"Whole-map is answered artifact-major"* is true of the 10⁸ arms and false here, so
  the argument is re-posed for a layer whose extents reach across the row space. The probe's
  **`everywhere`** set, which no document mentioned, is where that case lives and is now named.
- **The 10⁹/10⁷ grid's layer covers ~10% of the corpus** (`--members 10`), so the flatness comparison
  confounds coverage with corpus size; and grouped candidacy grew 34 → 103 ms across it, unexplained.
  A corrected run is queued.
- **The masked count is priced at a 1 000 budget** while the ordinal-tree fixture serves up to 2.4M
  artifacts at a shallow cut; §7.3's nested arm is the honest pricing.
- **The coarse-node count is 20–52 ms depending on mask, falling ~4× as the viewport narrows,
  measured at 10⁶ nodes only** — not "52 ms at any mask below one, flat". Corrected in §7.3, §7.4 and
  the handover's §0.
- **The cut's figures are separated by arm**, §6 is rewritten around the downward walk that produced
  the headline, §8.4's "nine passes" is corrected to the probe's five, §7.4's 8 ms sweep floor is
  attributed to its own lineage, and the delivery record's RSS pair is re-attached to the flattening
  it belongs to.
- **Storage arithmetic**: the partition is 30 MB of bitmaps plus 10 MB of identifiers at the
  32-expression fixture, so §3's "10 MB" and §4.2's "40 MB" were the same number twice; index and
  extents are 42 + 80 = **122 MB**, not ~84; 19.8 GB and 25.3 GB are two different runs; the list
  column's 4.4 GB is at `k ≈ 0.1`.
- **Four figures appear in no committed log** and are marked *unrecorded earlier revision,
  re-measurement queued*: §4.2's parity table (and its copy inside 0093), the scattered/list table
  (labelled 10⁷ points in one place and 10⁸ in another), "3.2 s to build" (logs say ≤1.7 s), and
  "138 s" (logs say 147.8 and 151.3 s).
- **§4's "1.0 row blocks per artifact"** counts 65 536-row containers while the index's finest node is
  1 024 rows, is imposed by the arm's stride, and does not bound what a viewport settles. Restated.
- **§1's one-core budget** now states its conditions: one layer, lineage held per generation, verdict
  and cut only — gather, blob reads, wire encoding and the ~240 MB passing allocation all excluded.

## 5. What survived

Attacked and not broken, recorded so it is not re-attacked:

- **Geometry as a candidate generator.** Using the index as a superset filter withholds nothing, and
  the containment collapse is exact rather than conservative — once §4 step 2 performs the masked
  probe, which is the fail-open above and not an objection to the construction.
- **The cut rewrite** (§6). Built, gate-green, checked against the reference implementation over
  random trees, with a test asserting the downward walk is taken rather than silently declining.
- **The lineage cadence.** Mask-independent and viewport-independent, so per generation is the right
  home, and depth belongs on `Lineage`.
- **The base-row rule.** The row form covers members holding base rows; a member still in a flush
  extent contributes nothing until the fold. Fail-closed, and unchanged by anything here.
- **§5.1's recorded tables.** The label-column arm's figures are in `data/r1e8-p1e3.csv` and
  `r1e8-p1e4.csv` and match the document.
- **§7.3's grid**, and the nested arm being a **real** hierarchy — membership built from the tree, so a
  parent contains its children and the root is in view always. This is the correction that moved the
  campaign's conclusion about what bounds the system, and it holds.
- **The residency arithmetic** — 78.5 B per container, 4 GB against 78.5 GB at 10⁹ — derived from a
  measured constant and marked as derived.

## 6. What re-measurement is queued

1. **The corrected probe**: routes G and H perform §4 step 2's masked probe, and the equality
   assertion covers them. Everything in §7 re-runs against it.
2. **The distinct-expression count** over a real generating-set population, which decides the
   identifier width and whether the per-request union stays cheap.
3. **The 10⁹ grid at full coverage**, separating coverage from corpus size, and explaining the
   34 → 103 ms candidacy growth.
4. **The blocks-per-artifact sweep** between 1.6 and 10, which is what turns the layout threshold from
   a shape into a number.
5. **The scattered arm at the target**, completed rather than abandoned at 137 s, with the correctness
   assertions sampled rather than exhaustive.
6. **The build costs nothing has priced**: the containment partition, and what the index, the extents
   and the partition add to the fold's artifact pass.
