# Filtering a render column, and filtering a list — the design, and what needs ruling

**Date:** 2026-08-12 · **Status:** Design memo — evidence, not normative. Proposes rules; rules
nothing. Six rulings are named in §5.
**Measured input:** [`probes/2026-08-12-filter-placement/`](../../../probes/2026-08-12-filter-placement/)
(three arms, 10⁷ and 10⁸, single-threaded).
**Reads against:** [`filter-index.md`](../../design/filter-index.md) §2.1–§2.6, §5, §6.2;
[`filter-surface.md`](../../design/filter-surface.md) §3–§4; architecture §4 (I2, I7, I12), §8.2,
§10.3; [`per-point-attributes.md`](../../design/per-point-attributes.md) §3.7, §4.3; decisions
0013,
0039,
[0062](../../decisions/0062-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0063](../../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
0064,
0065,
[0066](../../decisions/0066-none-of-requires-a-value-and-names-one-column.md).

---

## 1. What the measurements say

**A filtered viewport answered from the render column is 7×–1,269× cheaper at 10⁸** than the built
route, and needs no entity-space artefact and no crossing. The entity route's cost is set by the
principal (it scans the whole authorised set before the request narrows anything); the row-space
route's is set by what is on screen — 0.48–0.73 ns per viewport row, invariant in corpus size, mask
shape and coverage. The gap widens with both scale and privilege, and is 10,038× in the worst
measured cell (a scattered 25%-coverage principal, 30,000-row viewport).

**But the entity-space column is not redundant.** At the coarsest zoom the view *is* the corpus, and
the two routes swap places: a whole-slice filtered count over a contiguous low-coverage principal is
**30× cheaper** from the entity-space column. Neither dominates, so this is a route rule, not a
replacement.

**A list column wants the shape the single-valued design already has** — a flat CSR value column as
the record, per-value postings derived on top for a category. Postings win seven of the eight timing
cells, by **1.7–57×**, and lose the eighth — a contiguous 1% candidate at a small dense vocabulary,
where CSR costs 8.7 ns against their 10.08. The *storage* ranking inverts with the vocabulary: 3.8 B/entity against CSR's 11.9 for a
`categories`-shaped column, but 32.3 against 24.0 for a `surnames`-shaped one, where 400,000 bitmaps
with a singleton tail have no repetition to amortise. Explicit `(entity, value)` pairs are never
optimal on either axis, at any shape measured.

Full tables, constants and the negative results are in the campaign's
[`results.md`](../../../probes/2026-08-12-filter-placement/results.md).

---

## 2. Proposed: a render column is filterable, in row space

**`used_for = ["render"]` makes a column filterable.** The filter is evaluated over the request's own
rows against the hot column in `columns.arrow`, producing `FilterRows::Viewport` directly. Declaring
`filter` as well buys the entity-space column, which is what §3's cases still need.

Five things make this smaller than it sounds.

**The type it produces already exists.** `FilterRows::Viewport { rows, domain }` — *only the rows
inside `domain` were tested; outside it the emptiness means nothing* — is exactly what a row-space
scan yields, and `EffectiveMask::with_filter` already consumes it. The per-tile crossing route
introduced both. Nothing new enters the composition.

**It needs nothing on the write side.** A flush, a merge and the fold all carry the scalar tail from
the manifest's schema, so a render column is current in row space by construction. No extents, no
coalesce axis, no fold pass, no postings rebuild — against `filter-index.md` §5, §5.2 and §6.2, which
is where most of the filter index's machinery lives.

**The mask reaches it unchanged.** The composed row-space mask already carries the overlay, so a
suppression or a deletion narrows a render-column filter by the same construction that narrows an
unfiltered viewport. There is no attribute artefact, so write-path §5.4's Rule S and Rule F have
nothing new to retire — the shape most likely to be got wrong is absent rather than handled.

**The work carries no channel, and one variant's is stronger than the entity route's.** The dense
variant reads the same bytes whatever the principal may see: its work is a function of the request's
ranges and the column alone. The mask-first variant's work is a function of the mask, which is what
`filter-index.md` §2.2 already accepts for the entity-space scan. Neither depends on the values, which
is the property §2.2 requires.

**It is exact, not approximate.** Over every range the request can ask about, the two routes return
the same set — the claim `filter_routes_agree_over_the_domain` already asserts for the crossing, and
which the campaign checked on all 48 cells.

### 2.1 Where it stops, and this is the part that constrains the scope

**Only a category can express absence in row space.** `columns.arrow` is non-nullable (contracts R4);
a category's code `0` is an explicit *absent* sentinel, but an absent **number** is stored as the
type's zero by `ScalarValue::or_render_placeholder`, decision 0064's render half being open. So a
range containing zero would match every item that carries no number — precisely the defect fixed on
the entity path on 2026-08-11. **Filtering on a render column is therefore a category capability
until 0064's render half lands**, and a numeric declared `render` alone must stay refused, naming
that reason (decision 0013).

The other three limits are inherent rather than temporary, and each is a reason the entity-space
column continues to exist: a row-space filter is **per slice** (entity space is slice-invariant,
index §7); it cannot answer `/v1/categories`' membership question, which is entity-space and is what
`listing = "per_viewer"` is owed (§2.3); and it cannot serve a caller outside a viewport, which is
every filter surface the system might grow that is not a map request.

### 2.2 Composition, when a tree names both kinds

Decision 0062's boolean tree may name a render-only column and a `filter` column in one expression.
The proposed rule keeps the existing crossing as the only bridge: **evaluate the entity-space
sub-tree first, cross it by §4's measured rule, then evaluate the row-space leaves over the crossing
domain and combine in row space.** One crossing per request, whatever the tree's shape, and the
row-space half is bounded by the viewport either way.

---

## 3. Proposed: which route serves a request

Both routes exist for a column declaring both placements, so a rule must choose, and §8.2 forbids
choosing on a statistic about how much the principal can see.

**The proposal is the shape `filter-surface.md` §4 already uses:** compare the request's rows against
the principal's own cardinality — both quantities the caller could compute for themselves — and take
the row-space route while `rows_in_ranges ≤ |M_auth|`, the entity route past it. At a 300,000-row
viewport that is every request from a principal seeing more than 300,000 entities, which is the
interactive case the measurements say is 7×–1,269× cheaper; at zoom 0 it is the coarse case, where
arm 2 says the entity column earns its bytes.

A column declaring only `render` has one route and takes it, at whatever the coarse cell costs
(42–260 ms at 10⁸ single-threaded; 0.4–2.6 s at 10⁹ **modelled**, before the parallelism the sweep
already has). That is the price of not storing the second copy, and it is bounded by a full column
scan rather than unbounded.

---

## 4. Proposed: a list column

**`multi = true` becomes admissible under `used_for = ["filter"]`, and nowhere else.** Decision 0039's
fence is unchanged and this proposal does not approach it: no render placement, and no projection,
derived value or summary of a list earns a hot column.

**A list is a CSR flat column**: values in entity order, `offsets[e]..offsets[e+1]` delimiting an
entity's run, presence as today. This is `filter-index.md` §2.1's addressing generalised — the entity
id still reaches the values by arithmetic, and the affine-rank traversal still merges candidate runs
with presence runs; what changes is that a run of *entities* becomes a run of *values* whose length is
read from the offsets rather than assumed to be one.

**A category list derives per-value postings, exactly as a single-valued category does** (§2.3), and
under the same rule: they answer a filter only where `listing = "public"` (decision 0063), and a
`per_viewer` list is answered by the scan. A **string** list gets none — nothing may manufacture an
identity for a string value (§2.3's C11 hazard), which is also the case the storage inversion in §1
points at: a `surnames`-shaped column is where postings cost the most and are least permitted.

Three consequences worth stating before they are discovered:

- **`all_of` needs no new syntax and changes meaning.** Decision 0062's tree already conjoins leaves;
  over a single-valued column `all_of: [{c: {eq: a}}, {c: {eq: b}}]` is empty by construction, and
  over a list it is satisfiable. The request language is unchanged; what changes is that a previously
  vacuous expression starts returning members.
- **`none_of` reads unchanged** (decision 0066): *carries a value in this column, and none of these
  matches it.* Presence is what the list's presence bitmap already says, so the positive-predicate
  form survives verbatim and §5's failure arithmetic still never inverts.
- **The write side is the work.** §5's extents, §5.2's coalesce merge and §6.2's fold blanking are all
  written against one slot per present entity. Each needs the offsets carried, merged and rebuilt —
  which is why this is the larger of the two features by a wide margin, and why the render-column
  feature does not wait for it.

---

## 5. What needs ruling

Each is stated so it can be ruled without reading the code. The recommendation is mine; the
measurements behind each are in §1.

1. **Does `used_for = ["render"]` imply filterable?** *Recommend yes*, for categories now and for
   numerics when 0064's render half lands. The alternative — require `["render", "filter"]` — keeps
   today's surface and costs a second copy of every rendered filterable column (1–8 GB per column at
   10⁹) to buy the coarse-zoom cell §3 already has a rule for.
2. **Is a row-space operand admissible under §8.2 at all?** The contract says every filter returns an
   entity-space bitmap. A render-column filter returns a row-space one, bounded by the request's
   domain. *Recommend admitting it as a second operand kind*, on the ground that the composition rule
   (§2.2) keeps one crossing and the answer is exact over the domain — but this is the contract's
   shape, so it is the owner's.
3. **The route rule** (§3): rows-in-ranges against `|M_auth|`. *Recommend as stated.* It is the same
   class of rule as §4's 3× crossover and uses no quantity the caller could not compute.
4. **`multi = true` for `filter` only** (§4). *Recommend as stated* — 0039 already fixes what this
   may not do; the ruling wanted is that lifting the parse refusal is in scope at all.
5. **A list's addressing**: CSR flat column plus derived category postings. *Recommend as stated.* The
   alternative worth naming is postings-*only* for categories, which is 3.1× smaller and 5–23×
   faster on a `categories`-shaped column in three of its four candidate shapes — 0.86× in the fourth,
   the contiguous 1% cell — and abandons the rule that the flat column is the record,
   which is what makes the artefact self-retiring, keeps the oracle's relation, and leaves `per_viewer`
   a scan route to take.
6. **An empty list at ingest.** *Recommend: an empty list is absent* — the entity carries no value and
   occupies no slot. It is not the empty string, which is refused because an unset field and a client
   bug produce the same bytes; an empty list has no competing spelling and refusing it would make
   every entity with no tags unloadable.

---

## 6. Suggested order, and what each costs

**The render-column feature first, and separately.** It is a read-path change with no format change,
no write-path work and no new artefact: the schema parse, a row-space evaluation of the existing
`FilterExpr` over the crossing domain, the route rule, `/v1/meta`'s operand list, and the conformance
oracle learning the second route. Its risk concentrates in one place — that the row-space and
entity-space answers agree over the domain — and there is already a test asserting exactly that
property for the crossing.

**The list family second**, as the larger piece: a schema and ingest wire change, the CSR addressing
through the value column, the extent/coalesce/fold triple, the postings derivation for a category
list, and the oracle. It is the epic tail `filter-index.md` §2.6 names.

Neither is blocked by the other, and nothing above changes an invariant's statement — I2, I7 and I12
are argued in §2, and 0039's fence stands unmoved.
