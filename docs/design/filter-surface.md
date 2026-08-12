# The filter surface — design

**Date:** 2026-08-08
**Status:** **Provisional — r4, revised where the artefact changed under it.** `filter-index.md` r4
made the flat value column the artefact of record and the mask its scan candidate, which withdraws two
controls this document had registered (§2.1, §3.2) and withdrew §4.1–§4.5's shared projection cache.
§4's measured project-vs-per-tile rule replaces it and is built. To become normative: an owner ruling
accepting that rule, and the served-surface sections (§6–§7) re-read against it. Reviewed once under
three lenses (Appendix R).
**Reads against:** architecture §4 (I2, I3, I7, I12), §8.1–§8.5, §10.2–§10.4, Appendix C (C8, C11, C22);
[`contracts.md`](contracts.md) §3.2; [`per-point-attributes.md`](per-point-attributes.md) §3.3, §3.8;
[`records-and-search.md`](records-and-search.md) §2–§3, §6.2 (cited as **records §n**);
[`conformance.md`](conformance.md) §3–§4; decisions
[0008](../decisions/0008-candidate-list-route-declined.md),
[0041](../decisions/0041-pins-become-a-staleness-stamp.md),
[0044](../decisions/0044-invisible-means-stale-serve-plus-background-refresh.md),
[0064](../decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md),
[0068](../decisions/0068-a-row-space-operand-bounded-by-the-requests-domain-is-admitted.md).
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **surface §n**. The companion write-side design is
[`filter-index.md`](filter-index.md), cited as **index §n**.

---

## 1. Summary

A filter operand resolves to an **entity-space Roaring bitmap** — or, where the column renders, to a
row-space set bounded by the request's own domain (decision 0068) — and composition is intersection. That is
§8.2's contract in full, and this document is what it takes to honour it: how an operand is evaluated,
what the authorised set does when pushed into that evaluation, how a bitmap over entities becomes ranges
over rows without paying a cost the system cannot afford, and which counts a filter may produce.

Three things decide the design, and none of them is the index.

- **The mask goes in first.** `M_auth` — the viewer's authorised set — is usually the most selective
  operand, and every un-intersected intermediate contains unauthorised IDs. It is also the *candidate
  bitmap* §8.2 lets an operand exploit.
- **The projection is the expensive step, not the lookup.** Turning an entity-space bitmap into row ranges
  costs O(cardinality) — *measured* at 8.8 s for a 69.3×10⁶-entity mask, 10.7 s end to end at 10⁹ — and
  §8.5 says the filtered selection turns over on every keystroke. Section §4 is about not paying that per
  keystroke, and about not paying it per publication either.
- **Cost is a disclosure surface.** A filter answering correctly in a time proportional to something the
  principal cannot see has leaked it. §3.2 states the channel this design accepts and what actually bounds
  it.

> **⊘ The operand surface and §4's crossing are built. §4.1–§4.5's shared projection cache is
> withdrawn rather than pending** — see the note above it, and do not implement it. `/v1/meta`
> publishes `filter_operands` from the schema's filterable columns, a viewport request carries an
> operand, and a filtered request crosses into row space by whichever of §4's two routes the rule
> chooses. `listing = "per_viewer"` is filtered at `/v1/categories` by §3.3's membership predicate.
> **Both operand kinds are built**: the row-space leaf of §2 runs inside the per-tile sweep for a
> rendered category, and a differential asserts the two routes agree over the domain either can
> answer for.

---

## 2. The operand

**Every filter returns a set the composition can intersect, and there are exactly two kinds**
(decision 0068). New filter forms add operands rather than changing the shape of the call, which is
what keeps the retrieval surface enumerable — and Appendix C is exhaustive *because* the surface is.

```rust
fn resolve(op: &FilterOperand, idx: &FilterIndex, candidates: Option<&Bitmap>)
    -> io::Result<Bitmap>          // entity space
```

The second kind is a **row-space set bounded by the request's own domain**: a rendered column's
values are already in the hot column, so its leaf is evaluated over the rows the request asks about,
exactly over that domain and silent outside it. It is an amendment to §8.2's contract shape and no
wider — not a general row-space operand, and never a route a statistic chooses. A tree naming both
kinds evaluates its entity-space sub-tree, crosses once by §4's measured rule, evaluates the
row-space leaves over the crossing's domain and combines there (records §6.2). Both kinds carry §5.1's
rule unchanged: the set they are evaluated against is the **composed verdict**, never a raw fragment.

The four rules §8.2 places on the signature above:

**Threshold, never top-*k*.** An operand whose result depends on what else is in the query cannot be
composed independently. No family here is ranked, so this binds nothing today — it binds the vector
operand when [#44] lands, and it is restated because that is the operand that will want to break it.

**Optional candidate push-down.** Operands accept a candidate bitmap; those that can exploit it do.
Surface §3 is what this costs and buys.

**The mask goes in first, not last.** Ordering is fixed at design time by cost class, not chosen per
request. §8.2's reason is the one that matters here: statistics-driven reordering *would make execution
time a function of how much the principal can see*.

**Pre-intersection cardinality is structurally unreachable.** A raw match count is a corpus-wide count
over unauthorised records (C8). No route returns a cardinality on an un-intersected intermediate.

### 2.1 An unresolvable value is an empty operand, never a refusal

A filter naming a value the principal cannot see contributes an empty operand. So does one naming a value
that does not exist. **The two are indistinguishable in outcome** — status, body and every count —
deliberately and without exception.

Refusing the first would make the filter surface an existence oracle over exactly the vocabulary
`listing = "per_viewer"` hides: a caller enumerates hidden values by observing which keys are refused.
This follows contracts §3.2's unmatched-token precedent, and it is the same construction that closes the
drill-down timing channel — that path answers an entity-space question with *identical work* for an entity
that does not exist, one that exists and is invisible, and one that exists and is visible.

**Indistinguishability in *work* holds too, and the earlier supersession of it is withdrawn.** An
intermediate revision ruled work-indistinguishability a preference rather than a bar, on the reasoning
that a filter operand could not achieve it without giving up shared projection. That reasoning belonged to
an inverted-postings artefact evaluated unmasked. Under index §2.1's flat value column the operand is
scanned **under the candidate mask**, so its work is a function of `(candidate, column)` and never of the
value — a hidden value and a nonexistent one cost the same because the same bytes are read either way.

For a category's derived posting the property is measured rather than structural, and it holds: at 10⁹ and
25% coverage, a value with no members and a hidden value with 250M members both intersect in **0.000 ms**,
because Roaring short-circuits on container keys (index §2.3). **per-point-attributes §3.8 therefore stands
as written** — indistinguishable *"in outcome and in work"* — and no amendment is owed against it.

> **⊘ One case is unmeasured**, and it is named rather than covered by the claim: a scattered posting whose
> containers the candidate meets while no bits match does container-proportional work for an empty result.
> A uniformly scattered value cannot be fully hidden from a broad principal, so the case is narrow.

---

## 3. The authorised set as the candidate bitmap

`M_auth` is the candidate bitmap. Pushing it into an operand's evaluation bounds every intermediate: each
posting as a wide union accumulates, each canonical node of a level-tree range, and the working set across
a bit-sliced column's *k* slice operations.

**The cost asymmetry runs in the right direction.** Bitmap operations cost **O(containers touched), not
O(cardinality)**. A sparse principal's `M_auth` touches few containers, so push-down is cheapest exactly
where the principal is least privileged — the opposite of the asymmetry **I7** warns about elsewhere.

That is a prediction with partial support, and the qualifier travels with it. The category-membership probe
found it held on one corpus and was untested on the other — ***supported, not confirmed*** — and found
something sharper alongside: **the expensive principal is term count, not coverage.** Many narrow grants
cost more than a few wide ones at a fifteenth of the mask cardinality. A benchmark that varies coverage and
not grant shape measures the axis on which the prediction is trivially true and never probes the one on
which it fails; surface §9 fixes the arm accordingly.

### 3.1 Push-down forks the caching

An operand evaluated with `M_auth` pushed in is **principal-specific**, so its result cannot be the shared,
principal-independent object §4 caches. An operand evaluated without it is shareable but does more work.
There are therefore two modes, and **which mode an operand uses is fixed at design time** — by its family
and the shape of the query text, never by a cardinality observed at request time, which is precisely the
statistics-driven choice §8.2 forbids.

The dynamic input is the cache's **admission** decision (§4.2), and it is not a statistic about the
principal: it is a property of the operand, known before any mask touches it. An operand the cache declines
is evaluated with `M_auth` pushed in, which is both the cheaper route for a one-off and the narrower one
for disclosure.

### 3.2 The timing channel that shared mode carried, and why it no longer arises

> **This section records a channel the design no longer has.** It is kept because the channel was
> registered and accepted by owner ruling, and a control that is withdrawn should say so at the site rather
> than vanish.

The channel was a consequence of evaluating an operand **unmasked** and projecting it before it met the
principal's mask: projection is O(cardinality), so an operand's cold cost encoded `|postings(v)|` — the
corpus-wide, pre-mask membership count — and a principal seeing no member of a `per_viewer` value could
separate *"no such value"* from *"a value I cannot see"* by timing.

**There is no unmasked evaluation any more.** The mask is the candidate for the scan itself (index §2.2),
so nothing is computed before it applies and no cold cost encodes a pre-mask count. The accepted channel is
withdrawn rather than bounded, and §2.1 states the property that replaces it.

The paragraphs below are retained for the record of what was accepted and on what reasoning.

**What the channel yields.** Roughly how many items carry a value the principal cannot see — not which
items, not whether any given item does, and not the value's name, which they must already possess to probe
for. It is a coarse cardinality channel, the same class as **C22**, which the register already carries as
*accepted, not mitigated*: a category code discloses vocabulary size.

**What bounds it, stated honestly, because the first draft of this section got it wrong twice.** "Cold
occurs once per operand per `segments_version` across all principals, so a prober competing with real
traffic measures a warm hit" is false in both directions:

- **A prober can manufacture the cold path.** Admission is a reuse count on the operand identity, and §2.1
  guarantees an operand naming an invisible value is still evaluated. So a single principal issues the
  operand until it crosses the threshold and times the crossing request — no competition with real traffic
  required. The threshold therefore counts **distinct authorisation fingerprints**, not requests, so
  crossing it requires a population rather than a loop.
- **Eviction re-colds on demand.** §4 gives the cache a byte budget, and a broad entry reaches ~125 MB
  *measured*; a prober requesting a few broad operands can evict a target and re-measure it. This is not
  closed, and it is the honest residual.
- **And a publication does not re-cold at all**, because §4.1's rule extends and rebases rather than
  rebuilding — so the per-version framing was wrong in the other direction too.

So the registered residual is: **the approximate pre-mask cardinality of any gated value whose code a
principal can name, measurable at first use and at any eviction the population's byte pressure allows.**
That is what the owner ruling accepts.

**One mitigation is incidental and therefore free.** §4.2's admission rule declines to project an operand
below its threshold, intersecting in entity space and projecting only the result — push-down under another
name. A rarely-touched gated value is exactly that operand.

Prefix operands carry the same channel with the same bound. They do **not** carry a value-enumeration
channel: a prefix returns matching entities, every count over them is masked, and no family but a category
enumerates its values at all (index §2.3).

### 3.3 The assignment

| Operand class | Mode | Why |
|---|---|---|
| Category or string equality; set membership | shared | Narrow, highly reused, and the unmasked posting is the artefact's natural unit |
| Prefix | shared, per matched-posting-set (§4.3) | Reuse is high across principals; the identity is the matched set, not the query string |
| Level-tree range | shared, **per canonical node** | The nodes are the reused objects; the range is not (§4.3) |
| Bit-sliced range | shared | Its unmasked intermediate is acceptable — §8.2 accepts un-intersected intermediates internally, and C8 forbids only exposing a cardinality on one. Pushing down instead costs a privileged principal *k* dense-container intersections against a wide mask, order 4 GB of traffic ≈ **0.8 s modelled** at the *measured* ~5 GB/s dense-merge bandwidth, per query and uncacheable |
| Anything the cache declines | **push-down** | Not a mode decision — §4.2's admission rule, and entity-space intersection is what remains |

---

## 4. From entity space to row space

**The rule is measured, and it is one line:** *project the result while it is no more than three
times the viewport's row count; past that, test the viewport's own rows and project nothing.*

Two costs set it. A projection costs **~20–30 ns per set bit** and scales with the result
([`probes/2026-08-08-filter-layout/`](../../probes/2026-08-08-filter-layout/) arm 3 at 10⁹ and
[`probes/2026-08-11-viewport-crossing/`](../../probes/2026-08-11-viewport-crossing/) at 10⁸ agree on
this). A per-tile test costs **~20–29 ns per viewport row** on a contiguous result and **~57–106 ns**
on a scattered one, and scales with the viewport.

The asymmetry is the whole point. A projection scales with **the result**; a per-tile test scales with
**the viewport**, which is already bounded by the drawn-mark budget (§7.2), so it scales with neither
the corpus nor how much the filter matched. Projecting a 10⁸-entity result costs **2,779 ms — more
than the 730 ms scan that produced it** — and at 10⁹ it is outside §2.2's 0.5–1 s filter budget
outright, while the per-tile route stays in tens of milliseconds however much matched. **That is what
makes a mid-to-high coverage principal affordable at all**: a viewer seeing half the corpus and
filtering to a tenth of what they see is well past the crossover, not near it. The route that remains
for narrow results is cheap precisely because they are narrow: 3.8 ms at 10⁵.

**The crossing back is a file, not a computation.** `row-entity.u32` — a `u32` per row beside
`permutation.bin`, written by the batch build and the compaction fold and by nothing else — answers
"which entity holds this row" in one mapped read. Deriving it instead from the row's `tessera_id`
through `IdentityKey::invert` costs **~17.5 ns per row**, which dominates every other term in the
loop and cannot be batched away: inverting a whole tile into a scratch buffer before testing
membership measures *slightly worse* than interleaving (6.22 ms against 6.00 ms), because the cost is
the four Feistel rounds and not a stalled pipeline. The table removes it for **4 bytes per row per
slice**, shared across every filter column — it is a property of the slice's geometry, not of any
attribute, so sixteen filterable columns need no more of it than one does.

**Both constants are shape-dependent and neither may be quoted flat.** Arm 3's results are contiguous,
the cheap end for a gather *and* for a membership test. The corpus's 127 ns/set-bit point
(`probes/results.md` §6) is ~4.7× its projection constant and is the likely shape of a scattered
result; the per-tile constant moves in the same direction. **They move together**, which is why the
crossover is more stable than either constant — but the two do not move by the *same* factor, and the
threshold sits at 3× rather than the 1× a contiguous pair would give because scattered is the
realistic shape for an ingest-ordered column. Erring towards projecting costs milliseconds either
side of the crossover; the win the per-tile route exists for is two orders of magnitude out.

> **A Morton-cell → entity pre-filter is refuted by measurement.** The idea was a bitmap per coarse
> cell holding the entities whose rows fall in it, so a filtered viewport would be an intersection in
> entity space with no per-row crossing at all. It is the slowest of the three routes at every point
> measured, by 4–400×, and its structure is *larger* than the inverse permutation it emulates
> (3.2–7.9 B/entity against 4 B). A cell's entity set is scattered in entity space — entity ids are
> assigned in permission-signature order, uncorrelated with position — so the union over a viewport's
> cells touches every container and costs O(corpus), which is exactly what it was meant to avoid. The
> property that makes authorisation postings compress works against it.

> **⊘ Not measured: how the crossover moves with thread count.** The probe was single-threaded. Both
> routes parallelise, each over its own axis — the projection over the result, the per-tile crossing
> over the viewport — so the ratio is *modelled* to survive, not shown to. The engine counts requests
> by route (`Engine::filter_crossing_routes`, unconditional rather than behind a bench feature) so a
> deployment where the split is nothing like the model's prediction is visible without a bench.

> **⊘ §4.1–§4.5 below are superseded and retained only as a record.** They specify a **shared
> projection cache**: an operand projected once, principal-independently, reused across principals.
> The premise was that an operand is evaluated *unmasked*, which is what made a result
> principal-independent and shareable. Under `filter-index.md` §2.2 the mask is the scan's candidate,
> so a result is `M_sel` and is principal-*specific* — there is nothing left to share. And the measured
> rule above removes the need for the cache on its own terms: the expensive projections are the broad
> ones, and those are exactly the ones the per-tile route never performs.
>
> **Do not implement §4.1–§4.5.** §4.3's canonical level-tree node identities also address a structure
> `filter-index.md` §3 cut. What is owed in their place is small — the crossover test above, and an
> ordinary per-`(operand, segments_version)` memo for repeated *narrow* operands if measurement ever
> shows one is wanted.

Filters produce entity-space bitmaps. Tile counts, density and selection are row-space questions. The step
between them is `Permutation::project`, which touches every set bit and then sorts: **8.8 s for a
69.3×10⁶-entity mask, measured**, and §10.4's warning is unambiguous — *if it ever drifts onto the
per-viewport path, the system dies*.

The mask itself avoids this because the cached projection is of the **fragment**, which does not change for
a session's lifetime. A filter cannot use that trick, because a filter *is* the thing that changes.

**What makes it affordable is that the broad results are never projected at all** — the rule at the head
of this section, not a cache. A filter operand's projection is *not* principal-independent and cannot be
shared: the operand is evaluated under the candidate (`filter-index.md` §2.2), so its result is `M_sel`.
The architecture's cache table says so.

### 4.1 Publication: extend, rebase, rebuild

A cached projection must survive publications, and the corpus already has a measured three-rung ladder for
exactly this problem on the mask projection. This design uses the same one; the first draft used only its
top and bottom rungs and would have died under steady ingest.

| Publication | Response | Cost per entry |
|---|---|---|
| **Flush** — a segment is appended, existing rows do not move | **Extend**: re-resolve the operand over the new delta tier, then project the new extent and union it in | *measured* 0.24 ms for the extent union, dominated by the entry clone at **40.9 ms** (max 128 ms), since cache entries are immutable |
| **Merge** — row space is permuted inside the merged span, within the same prefix | **Rebase**: keep the base's contribution, re-project the extents | *measured* **44.6 ms**, against the 4,550 ms rebuild it replaces — 102× |
| **Fold** — row space is permuted globally and the prefix flips | **Rebuild** | *measured* 4,550 ms synthetic, 10.7 s end to end at 10⁹ |

**The merge rung is not optional.** Merge policy bounds live segment count against a flush cadence of
order 90 s, so merges run at minutes cadence in steady state. Treating a merge as a rebuild — the first
draft's rule — would rebuild every cached operand every few minutes, which is the continuous churn the
extension rule exists to avoid.

**The discriminator is not the cache key.** A flush and a merge are indistinguishable in
`(segments_version, prefix)`: both bump the version, and both publish *within* a prefix — it is a fold, not
a merge, that flips it. Extending across a merge would union a pre-merge projection with extents over a
**permuted** row space, so the cached bitmap names different entities' rows, and a filtered count or
selection then draws rows belonging to entities the principal did not match. The engine already solves this
and not via the key: the candidate entry's covered-extent boundary segment ID and base row count are
compared against the live row space, and segment IDs are never reused. **That comparison is the safety
argument, and the key only selects the candidate.**

**The extension's own cost is the clone, and it couples the cache's population to the publication cadence.**
At *P* resident broad entries per slice, each flush costs ~*P* × 41 ms of pool time; the byte budget is
therefore a throughput constraint as well as a memory one, and §4.4 sizes it as both. Extension is **lazy**
— performed on next touch, folding ~41 ms into one request per operand per publication — rather than eager
over the whole resident set, so an idle operand costs nothing.

### 4.2 Admission is two-axis

"Project only above a reuse threshold; below it, intersect in entity space and project the result —
O(|`M_sel`|), cheap exactly when the filter is selective" is true only in the corner it names. Reuse and
result size are independent, and the bad corner is reachable: a **broad** operand below the threshold, for
a **privileged** principal, gives |`M_sel`| ≈ 10⁸ and an inline projection of **~12.7 s** (*modelled* at
the *measured* ~127 ns per set bit) — §10.4's death sentence, reached by following the rule.

So admission has two axes:

- **Above an operand-cardinality bound: admit on first use.** The operand's own cardinality is known before
  any mask touches it — a shared, principal-independent quantity, so bounding by it is not the
  statistics-driven choice §8.2 forbids.
- **Below that bound: a reuse threshold**, counted in distinct authorisation fingerprints (§3.2).

**Reuse accounting lives on the operand identity across `segments_version`, and survives extension and
rebase.** Counting per cache key would reset every publication, so a hot broad operand would re-earn
admission every ~90 s and never amortise — deleting the benefit the section exists to buy.

**A miss is served by push-down, never by a refusal.** The token projection cache sheds a concurrent
cold-build racer with a 429, which is right there because the work is per-session and the caller is the
only one waiting. Here the entry is population-wide, so shedding would stall every filtered request from
every principal for the duration of one build. The build runs off the request path; a request arriving
while an entry is building — or below admission — takes §3.3's push-down route and answers correctly,
slightly more expensively. **No new 429 surface.**

This is a **second cache instance** over the existing single-flight machinery, with its own key and value
types — not the token projection cache widened. That cache's per-session sizing, its token-scoped pruning
and its freshest-entry selection are all wrong for a principal-independent entry, and the reuse counter is
new machinery neither has.

### 4.3 Operand identity is canonicalised

Keyed textually, a range slider dragged across its domain mints an identity per tick, and a coarse range
matching 10⁸ entities costs ~12.7 s to project (*modelled*). So:

- **A range operand is cached per canonical level-tree node**, and a drag reuses O(log_b) already-projected
  nodes. Nodes change only at a fold, which flips the prefix and bumps the version, so no stale node
  projection can answer.
- **A prefix operand is keyed by a digest of its matched posting set**, so `smit` and `smith` selecting the
  same values share an entry — **but the digest is derived through a cached prefix→digest map, and above a
  matched-count bound the operand falls back to textual identity.** Enumerating the matched set to derive
  the key costs O(matched values), which on a short prefix over a large dictionary is ~0.05–0.5 s
  (*modelled* at the *measured* ~1.3 µs FST lookup) — paid on hits as well as misses, and worst exactly
  where reuse is highest.

### 4.4 Budget, eviction and what the sizing has to survive

The entries worth caching per canonical node are the *upper-level* nodes, and those are the broad ones: a
node near the root covers ~*N*/*b* entities, and attribute membership is scattered in row space, so its row
projection is bitmap-container dominated at **~31–125 MB each at 10⁹** (*modelled* from the *measured*
125.12 MB dense bound). One popular column's top levels are 4–20 such nodes — **0.5–2.5 GB per slice per
generation, modelled** — doubled by a retained superseded generation and again by a second slice.

That is the arithmetic the budget must be set against, and it is not obviously satisfiable: a few GB holds
one hot column and evicts under a second, while each eviction of a broad entry re-arms a ~12.7 s
projection. Two consequences the design takes rather than defers:

- **The top one or two levels may be declined rather than cached.** Their answers compose from children
  per-tile as additive disjoint-node counts, O(nodes × containers) — trading the largest entries for a
  bounded per-request term.
- **The operand-population economics are a promotion gate, not a footnote.** §9 names the arm.

**Eviction is also a channel**, registered in §8: it is driven by aggregate cross-principal filter volume,
so a principal watching its own entries go cold learns roughly how much filtering the rest of the
population is doing. Activity, not content — the class the corpus already accepts — but the shared cache
introduces it and the register must carry it.

### 4.5 The cache key

`(operand identity, slice, segments_version, bundle identity, prefix)`.

**The bundle identity is required, not defensive.** Attribute ordinals are bundle-relative positions
(index §2.4), so an entry surviving a rebuild would serve a bitmap naming a different set of entities. The
mask fragment cache hashes the bundle identity for exactly this reason.

`prefix` is carried alongside `segments_version`, not instead of it. §10.2 forbids a row-space artefact
keying **on** the prefix as its discriminator; carrying it in addition is what distinguishes a fold's flip
from a version bump, and the engine's projection cache already carries both. The key selects a candidate;
§4.1's boundary comparison decides what may be done with it.

---

## 5. Composition, and what a filter may count

`M_sel = M_auth ∧ filter₁ ∧ … ∧ filterₙ`. Intersection is the only top-level operator.

### 5.1 Where the filter meets the composed mask

**Above composition, per range — never inside it, and never folded into its base.** The composed mask is a
base projection plus two row-space diffs, with the deny state folded into them; its counts are range
cardinalities over that structure. A filter is a fourth operand, applied to the *result*, by whichever of
§4's two routes the result's size selects:

```
matched_in_range(r) = rows_in_range(r) ∧ filter_proj          # narrow result: project once, intersect
matched_in_range(r) = |{ row ∈ r : entity_of(row) ∈ M_sel }|  # broad result: test per tile, project nothing
```

The construction the alternative invites — folding `filter ∧ base` in as a new base — breaks the
structural invariants the diffs are asserted against, and breaks the unfiltered total that θ must anchor on
(§5.2).

**The candidate a filter scans under must be the composed verdict, not a raw fragment**, and this is the
sentence the section exists for. A fragment contains suppressed and deleted-but-unfolded entities — a
suppression never touches the artefact at all (write-path §5.4, Rule S; index §6.1). The scan returns
whatever its candidate contained, so a scan under a fragment silently resurrects a suppressed entity, and
one under the composed verdict cannot.

An earlier revision reached the same conclusion by a different route: the operand was evaluated *unmasked*,
so the composition had to come afterwards. Masked evaluation moves the obligation **earlier** rather than
removing it — from "compose the result" to "compose the candidate" — which is a simpler rule and a stricter
one, since a result that was never scanned cannot be forgotten to be composed.

The residual per-request cost is one intersection per viewport range on the narrow route, O(containers in
range); on the broad route it is the per-tile membership test, *measured* at ~6–22 ns per viewport row and
independent of how much the filter matched. Either way it is bounded by the viewport rather than by the
result, which is the property §4's rule exists to preserve and the one §9's viewport delta measures.

It lands on the per-tile half of the request, which is where a warm request's time already is. *Measured*
on the 2.4×10⁶ demo bundle at depth 7, k=500, a lean five-column schema and a **warm row-projection
cache**: a 1-to-164-tile request costs ~170 µs and a 16,384-tile request 7.2 ms, so the fixed per-request
prefix is ~170 µs and the remainder is per-tile. A filter adds against that shape rather than to the
prefix, which is why a per-tile intersection is the right place to pay it and a whole-mask materialisation
is not. *(A probe against the demo bundle, not a campaign in `probes/`; the two endpoints are what the
ratio rests on, and the points between them track where the data is rather than tile count.)*

**Every one of those qualifiers is load-bearing, and the cache one is load-bearing for this section
specifically.** The ~170 µs floor is a steady-state figure: the session's projection was already resident.
A cold session pays the projection build instead — 4,550 ms at 10⁹ (*measured*), four orders of magnitude
above the floor and wholly dominant when it happens. So the prefix is ~170 µs *between* geometry
publications and nothing like it across one. That is the right split for this design rather than an
inconvenience to it: the per-range filter intersection priced here is a **warm-path** term, and the cold
path is §4's subject entirely — which is why §4 spends its length on extend-versus-rebase-versus-rebuild
and this paragraph does not. The schema qualifier matters in the same way and in the other direction: the
gather share grows with schema width (a 19-column fixture measured 944 ms against ~51 ms), so ~98%
per-tile is a property of a lean schema, not a constant.

### 5.2 Thresholds and the frontier

**Two thresholds, with different jobs, and filtering may relax neither.** §8.4 governs the frontier — the
set of cluster labels shown at a given zoom. The naive implementation gets the direction wrong:
`M_sel ⊆ M_auth`, so filtered counts are never higher, and descending on `M_sel` against a fixed threshold
would make the frontier uniformly *coarser* as someone types, dissolving the map exactly when it is most
needed. So `min_visible_members` is evaluated against **`M_auth`** and sets the maximum depth — a
disclosure control and the operational form of **I12**, *a filter may move the frontier up, never down* —
while a much smaller display threshold against `M_sel` decides how far within that bound to descend.

The selection threshold takes its total from **`M_auth`**, never `M_sel`. Anchoring on filtered counts would
make θ a function of the filter, and both **I12** and §8.4 break at once.

**Containment binds to `M_auth`, never to `M_sel`** (**I3**). A generating set contains items that do not
match a filter, so testing containment against the filtered mask would make essentially every label vanish
the instant anyone typed. The consequence is also the optimisation: the servable-label set is computed once
per token and reused across every keystroke.

### 5.3 The counts a filter may produce

**Every count is an `and_cardinality` against the composed verdict** — never against a mask fragment, and
never against a bare base projection. The reason is §5.1's: a fragment contains suppressed members, so a
facet or legend count over one over-reports by the number of suppressed-or-unfolded entities carrying that
value, and differencing it against a count the viewer can compute yields a running tally of suppressed
items per value. That is a corpus-side aggregate over removed records — **C8** and Rule S — and it is the
one place the composition rule is easy to elide, because counting feels like a different affordance from
selecting.

The constructive form is §8.1's free affordance, and it should be the recommended pattern for every family:
`range_cardinality` over both masks on the same range gives *matched-and-visible* against *total-visible*,
exactly, for two bitmap operations. Highlight in context rather than removing everything else and leaving
the viewer twelve points on a blank canvas — I2-clean by construction, because both operands are inside
`M_auth`.

**Range summaries are where this design adds a leak-register entry**, because they look like layout. A
slider's minimum and maximum are corpus-wide extrema: textbook **I2**, invisible to review because nobody
reads a slider as an aggregate. The histogram behind it is worse — per-bucket counts fall straight out of a
level tree's precomputed range nodes, and reading them off is exactly the precomputed unmasked aggregate the
invariant forbids. **Bounds and bucket counts are masked, or they are fixed and data-independent. There is
no third option.** Bit slicing gives masked extrema free; a level tree does not, and computing them means
descending its nodes *against the mask*.

> **⊘ No route serves a range summary.** No row of §6's table returns bounds or buckets, and no contract
> carries them. The register entry and the rule above exist to pre-empt the one-line wrong implementation
> whoever builds a slider will otherwise write. When a route is added it must also be told **where** the
> computation runs: a masked descent probes dense upper-level nodes at order **24 ms each** (*modelled* at
> the *measured* dense-container constant), so a masked extremum is order 100–200 ms and a 50-bucket
> histogram is order a second — cacheable on §7's `(auth fingerprint, generation, overlay version)` key and
> never computed inside a viewport request. Fixed, data-independent buckets are the recommended default for
> that reason as well as the I2 one.

---

## 6. The served surface

| Surface | What changes |
|---|---|
| `/v1/meta` | `filter_operands` stops being an empty list and enumerates each filterable column with its family's operator names: every column declared `index = true`, **plus every rendered category**, whose leaf is answered over the request's own rows (decision 0068). A rendered number is absent, being refused at the schema parse until decision 0064's render half lands; a blob-resident column is absent because it is no operand at all. One predicate serves this list and the viewport's parse gate, so a client is never published a surface its requests are not held to |
| Viewport request | Carries operands, composing with either viewport form — a bounding box or an explicit tile list, which the request already validates as an exactly-one-of pair. Operands extend that validation rather than sitting beside a bbox check. A filter naming an invisible or nonexistent value contributes an empty operand (§2.1) |
| Viewport response | The two-layer form of §8.5 — see below |
| `/v1/categories` | Gains the per-viewer gate it is currently refused for (§7) |
| `/control/status` | An attribute-side counterpart to the existing posting-fragmentation gauge, so contiguity erosion in the attribute index is visible rather than inferred |

**The two layers need a definition, not just a name**, because the conformance differential compares served
sets and cannot do so against a description. A *context* layer is sampled from `M_auth` under §7.2's
definition, unchanged by the filter. A *match* layer is drawn from `M_sel`, and:

- It is subject to the **same cap** as any selection — the served count is `min(k, max_k, k_max_marks)`, so
  "sent in full" means *unsampled below the cap*, not *uncapped*. §10.4's rule that the served set is a pure
  function of `(mask, corpus state, k, viewport)` is preserved with `M_sel` as the mask.
- When the filter is broad enough that `M_sel` exceeds the cap, the match layer is sampled by the same
  direct evaluation the context layer uses, **anchored on `M_auth`** per §5.2 — so the anchor does not move
  as someone types.
- There is therefore no selectivity threshold and no mode switch. "Small enough to send in full" describes
  the common outcome, not a branch.

**Only the context layer is delta-servable, and that is a consequence of the split rather than a rule added
to it.** `delta-serving.md` §2 lets a client declare what it already holds so the server can omit or elide
points, and rules that declarations are **ignored on any request carrying a filter** — because eliding is
exact only while a tile's visible set is unchanged, and asking the client to
work out which of its held points still match would be client-derived membership, which
`client-interaction.md` §5 declines. The context layer is exempt from that reasoning: it is drawn from
`M_auth` and is a pure function of `(mask, corpus state, k, viewport)`, so a filter does not move it. The
match layer never is: it is `M_sel` by definition.

So the narrowing available — delta-serve the context layer, never the match layer — is worth having,
because the context layer is the expensive half while a selective filter's match layer is small enough to
send in full anyway. **It is not taken here**, and when it is taken the shape is already constrained.

The content coordinate is a **16-byte opaque hash, compared for equality and nothing else**, and that
opacity is load-bearing: it is what lets its components change without a client contract change. So
"context half unchanged, match half new" cannot be said by structuring it, and a design that decomposed it
would spend the property it exists to have. Two coordinates side by side is the shape — the existing
content key governing the context layer, a second filter-scoped one governing the match layer — so each
layer stays independently checkable and both stay opaque.

The invariant either must satisfy: **a coordinate moves whenever the set it governs could have gained
members.** Removals need no move, because a client holding every identity below its bound still holds a
superset once the set shrinks — which is why the existing key tracks the watermark and ignores
`segments_version`. A filter that narrows `M_sel` therefore cannot by itself invalidate a context-layer
coordinate, which is the same fact this section opened with, arriving from the other direction.

Until filter operands reach `ViewportRequest`, §2's conservative rule governs and costs only the context
layer's cache benefit.

**The selection route does not change.** There is one selection route — direct evaluation of the definition
from the mask, at every coverage — and decision 0008 declined the precomputed candidate-list alternative
because filtering a precomputed unmasked list at query time lets a sparse principal see an empty tile where
items exist, which is **I7** inverted and fails silently. A filter narrows the mask that direct evaluation
runs over; it does not introduce a second route. `scripts/check-layers.sh` fails if the marker recording
that refusal is removed.

---

## 7. Vocabulary visibility

per-point-attributes §3.3 specifies the predicate and it has been unbuildable for want of the member sets.
Those sets are index §2.3's per-value postings, which a `per_viewer` category is **owed** rather than
merely permitted — the one place those postings stop being an optional accelerator, because this
disclosure control depends on their existing. The predicate becomes computable as a consequence:

```
visible(v) = !((members(v) ∩ fragment).andnot(overlay_fail)).is_empty()
          || !(members(v) ∩ overlay_pass).is_empty()
```

**Where the two overlay bitmaps come from is the part the formula hides, and getting it wrong is how a
suppression stops suppressing.** `overlay_fail` is the overlay's denied set — the single union composition
already folds. `overlay_pass` is the buffered-and-passing set, and it is **principal-dependent**, because
the buffer rule needs the session's satisfied term set: it is derived per `(session, overlay version)` by
walking the buffer through the *same* deny-precedence function composition calls, not a second
transcription of it. per-point-attributes §3.3's "once per generation" phrasing is imprecise on this point
and this section supersedes it.

Three properties carry over unchanged, each a place the predicate could be implemented wrongly and still
look right. It is evaluated **in entity space against the composed verdict** — not against the cached
fragment, which still contains suppressed items, and not by projecting into row space, which vocabulary
visibility has no need of and which costs seconds. It is **derived per request, never maintained**, because
a maintained union is non-monotone under deletion and would need a third retirement rule beside the two that
exist. And any cache of the result is keyed by `(auth fingerprint, generation, overlay version)`; the
generation carries the overlay version, which moves on every accepted change, and an overlay-blind cache row
is fail-open by another route.

**A category carrying `per_viewer` gets its member sets, whatever its flags say** (owner ruling,
2026-08-08). A membership set is not an optional *filter placement* that a disclosure control smuggles in
past §10.3's routing rule — it is **what a category is in entity space**, as its code is what it is in row
space. `index = true` decides whether the operand is answered from entity space; it does not decide
whether the category has members. So a `render`-only category declared `per_viewer` is served, not refused,
and the cost is **reported, never refused**, per per-point-attributes §2.3: the plan step names the member
sets, their measured size against the column they index (0.31–1.01×), and the total across attributes.

**This route stops being admission-free, and contracts §3.2 says otherwise today.** `/v1/categories` is
currently classified as outside the compute-admission gate because its work is a bounded walk of an
in-memory map with no mask composition, no projection and no file IO. The gate above needs the session's
fragment and a bitmap intersection per value, so the classification flips — an amendment owed on promotion.

> **Built.** The batch build emits per-value postings for every `per_viewer` category and
> `/v1/categories` evaluates the predicate above against them, unioned with the value-column extents a
> flush writes — so a value carried only by entities ingested since the build is still offered. A column
> whose member sets cannot be read is refused rather than served empty, which would be indistinguishable
> from a correctly-computed empty answer. `listing = "public"` publishes as authored. The compute-admission
> amendment this section names is **landed in contracts §3.2**, which now states the `per_viewer` cost
> rather than the `public` one for both.

---

## 8. The leak register

Every row is an instance of a registered leak arriving attached to a *data type* rather than to a query,
which is the class a mechanism-focused review misses.

| Affordance | Leak | Form |
|---|---|---|
| Facet or legend with counts | Counts computed before masking, or against a fragment, are counts over unauthorised or suppressed records | **C8** and Rule S. One `and_cardinality` per value against the **composed verdict** (§5.3) |
| A category's value list | Offering a value reveals that something carries it — possibly only items the principal cannot see | **C11**, closed by §7's membership-derived gate. Categories alone: no other family enumerates its values (index §2.3) |
| Range bounds and histogram | A slider's extrema and its bucket counts are corpus-wide aggregates that read as layout | **New entry.** Masked, or fixed and data-independent (§5.3). Cheap to honour now that a range is a masked scan: the extrema come from inside the candidate for free, and no precomputed unmasked structure exists to be tempted by — which is why index §3 declines zone maps rather than deferring them |
| "N results" before intersection | Pre-intersection cardinality | **C8**, structurally unreachable per §8.2 |
| Operand latency over a gated value | The cold cost of an *unmasked* operand would encode its corpus-wide membership count | **Withdrawn, not accepted** — the channel it named no longer exists. A masked scan's work is a function of `(candidate, column)` and never of the value (index §2.2); *measured*, a value with no members and a hidden value with 250M members both intersect a category's derived posting in 0.000 ms. The row is kept as the record of a control that was accepted by ruling and then removed by construction (§3.2) |
| Shared-cache eviction cadence | A principal's entries going cold would disclose aggregate cross-principal filter volume | **Withdrawn** with the shared cache that introduced it (§4). Nothing principal-independent is cached, so there is no cross-principal cadence to observe |
| Empty-result disclosure | "No matches" over a masked set is safe; over an unmasked set then gated, it is not | The decision must be a function of data on the principal's own side (§2.1) |

---

## 9. Conformance and measurement

**This design makes I12's mask half coverable.** The suite records I12 as uncovered and explicitly not as a
testing gap — *"no label service, no plugin host, no generating sets, no filter surface."* Landing the
surface removes the last of those, but **not the others**: there is no label service, no frontier and no
`min_visible_members` in the tree, so the frontier half of I12 and the whole of I3 stay blocked on machinery
this design does not build. Claiming otherwise would be the present-tense-about-absent-machinery error
decision 0013 forbids, inside a conformance section.

| Row | What it asserts | Status |
|---|---|---|
| **I12, mask half** | `M_sel ⊆ M_auth` across the adversarial mask catalogue; per tile, matched ≤ visible | Coverable |
| **Rule S** | A suppressed entity is absent from every filter result *and every filter count* although its postings stand — the case §5.1 and §5.3 turn on, and the one a fragment-intersecting implementation passes every other test while failing | Coverable |
| **C11** | A filter naming an invisible value and one naming a nonexistent value agree **in outcome** — status, body, every count. Work is deliberately not asserted (§3.2), and a test asserting it would fail the design as ruled | Coverable |
| **I2 canary** | Canary items carrying attribute values move no filtered aggregate. The canary allocation rules gain *"canary attribute descriptors interned last"*, mirroring the existing rule for canary terms — without it, a canary displacing an attribute ordinal makes the comparator flag fixture perturbation as disclosure | Coverable |
| **I12, frontier half; I3** | ⊘ Blocked on the label service, as they are today | Not coverable |
| **C8** | No response shape carries a cardinality over an un-intersected intermediate | A **review obligation**, not a test row: a universally-quantified negative over all shapes is what the suite's own standard rejects. What holds it in test is the differential's masked-count equality and the canary comparator |

**The differential relation carries values.** Index §9 specifies `(entity_id, column, value)` rather than
ordinals, because a filter's definition is a predicate over values and an oracle reconstructing values from
ordinals would have to reproduce the engine's key encodings — making the differential a transcription at
exactly the encodings most likely to be wrong.

The adversarial **mask** catalogue gains an adversarial **value** catalogue: an empty value; a
single-member value; a value whose members straddle a container boundary; a value all of whose members are
invisible to the test principal; a value spanning base and delta tiers; a value whose only member is
deleted-but-not-folded; and one whose only member is **suppressed**.

**Bench arms**, extending measurement §7's declared-but-unbuilt `filter` arm. Four falsifiers this design
needs and the arm as declared does not carry:

- **Grant shape** — term count at fixed coverage, not sparsity alone. The probe's sharper finding is that
  the expensive principal is term count (§3), and an arm sweeping coverage confirms the easy half.
- **The admission grid** — operand breadth × principal breadth, which is where §4.2's two axes are checked
  rather than assumed.
- **The filter-on viewport delta** at 10⁹, against the *measured* 123–164 ms operating point — §5.1's
  per-range intersection is the one per-request cost the amortisation argument does not cover.
- **Operand-population economics** — resident bytes, hit rate and rebuilds per publication under the
  operand-churn trajectory, which is what §4.4's sizing turns on and what the cardinality sweep does not
  measure.

---

## Appendix R — review trail

**2026-08-12 — the second operand kind lands** (decision 0068). §2 gains the row-space set bounded by
the request's own domain, which a rendered column's leaf resolves to, and §6 records what `/v1/meta`
therefore publishes: the columns declared `index = true`, plus every **rendered category**. §5.1's
composed-verdict rule binds the new leaf exactly as it binds a scan, and is stated at §2 so the
route that would be convenient to exempt is not. Nothing else moved — a rendered *number* is not an
operand at all, refused at the schema parse until decision 0064's render half lands.

**2026-08-08 (r3, third pass) — §5–§8 re-read against the artefact, and two registered channels
withdrawn.** §5.1's *rule* survives — a filter is applied above composition, per range, never folded into
the base — but both its mechanism and its central argument moved. The mechanism gains §4's two routes, so
a broad result is tested per tile rather than projected. The argument is the more important change: it had
run "the operand is evaluated unmasked, so compose the result afterwards"; masked evaluation moves the
obligation **earlier**, to "the candidate a filter scans under must be the composed verdict, not a raw
fragment". That is simpler and stricter, because a result that was never scanned cannot be forgotten to be
composed. §5.2 and §5.3 are untouched: `M_auth` anchoring, the frontier direction and containment binding
are properties of the mask, not of how a value is stored. §6 likewise — it describes served sets.

Two leak-register rows are **withdrawn rather than accepted**, and kept in the table as the record of
controls that were ruled on and then removed by construction: operand latency over a gated value, which a
masked scan makes value-independent (*measured*: 0.000 ms for both a valueless and a 250M-member hidden
value); and shared-cache eviction cadence, which goes with the shared cache. A third row gains a note
rather than changing: masked range bounds and histograms are cheap to honour now that a range is a scan,
which is also why index §3 declines zone maps outright rather than deferring them — a precomputed unmasked
structure is exactly the thing that would put the channel back.

**2026-08-08 (r3, second pass) — §4's cache is superseded by a measured rule, not merely suspended.**
With the filter-latency budget ruled at 0.5–1 s (`filter-index.md` §2.2), the binding term moved from
finding the matching entities to getting them into row space — so that step was measured rather than
reasoned about (arm 3). A projection costs ~27 ns per set bit and scales with the *result*; a per-tile
membership test costs ~6–22 ns per viewport row and scales with the *viewport*, which the drawn-mark
budget already bounds. At 10⁹ a 10⁸-entity result costs 2,779 ms projected against 6.49 ms tested —
and 2,779 ms is more than the 730 ms scan that produced the result, so for any broad filter the
projection had been the dominant cost all along.

That replaces §4's machinery with one line — project below ~a quarter of the viewport's rows, test per
tile above it — and removes the cache's justification independently of the premise failure recorded
below: the expensive projections are the broad ones, and the per-tile route never performs them. One
negative result recorded so it is not mistaken for a settled constant: this arm's results are
*contiguous*, the cheap end for a gather, so ~27 ns/set-bit is a floor. The corpus's 127 ns point is
~4.7× it. **The per-bit constant is shape-dependent; no design may quote a flat one.**

**2026-08-08 (r3) — revised where `filter-index.md` r4 changed the artefact underneath it.** That
document's organising rule became the flat value column as the artefact of record, with the mask pushed
in as the scan's candidate. Three consequences here, two of which *remove* mechanism rather than adding
it.

**§2.1's supersession of per-point-attributes §3.8 is withdrawn.** r2 ruled work-indistinguishability a
preference rather than a bar, because an inverted-postings operand evaluated unmasked could not achieve
it without giving up shared projection. A masked scan's work is a function of `(candidate, column)` and
never of the value, so the property holds structurally; for a category's derived posting it is measured
— a value with no members and a hidden value with 250M members both intersect in 0.000 ms at 10⁹ and 25%
coverage. §3.8 stands as written and no amendment is owed against it. One narrow case is named unmeasured
rather than covered.

**§3.2's registered timing channel is withdrawn rather than bounded.** It existed only in unmasked
evaluation, which no longer occurs. The section is kept, marked, because a control accepted by owner
ruling should be recorded as retired rather than vanish.

**§4's shared projection cache is suspended.** Its premise — that an operand result is
principal-independent and therefore shareable — was a consequence of unmasked evaluation. A masked scan
returns `M_sel`, which is principal-specific. The problem it addressed survives; the solution does not,
and §4.3's canonical level-tree node identities address a structure that no longer exists. Retained
unedited as the record of a reviewed design whose premise moved, with an explicit instruction not to
implement it without re-deriving that premise.

**2026-08-08 (r2) — reviewed under three lenses.** The document survived its security argument and failed
on cost and on seams.

The finding that changed the most: **§4 used only the top and bottom rungs of a ladder the corpus has
measured three rungs of.** r1 said a flush extends and "only a merge or a fold forces a rebuild" — but
merges run at minutes cadence under steady ingest, so that rule rebuilds every cached operand every few
minutes, the churn the extension rule exists to avoid. `rebase_extents` is *measured* at 44.6 ms against
the 4,550 ms rebuild; §4.1 now carries all three rungs, the clone as the extension's real cost, and the
boundary-segment-ID comparison as the safety argument — because a flush and a merge are indistinguishable
in the cache key, and extending across a merge names different entities' rows.

Two more that changed a mechanism. **§4.2's admission rule had an unbounded corner**: reuse and result size
are independent, so a broad operand below the threshold puts a ~12.7 s inline projection on a request — and
the reuse counter, if kept per key, resets every publication and deletes the amortisation entirely. It is
now two-axis, counted on the operand identity across versions, with the miss path answering by push-down
rather than by a 429, since shedding a population-wide build would stall every principal. And **§5.1 now
says where a filter meets the composed mask** — above it, per range, never folded into its base — which r1
left to inference; the construction inference invites re-opens the suppressed-row question §5 exists to
close.

**§5.3 acquired the composition rule that §5 already had.** r1 required the *selection* result to compose
with the deny state and then said counts are "an `and_cardinality` inside `M_auth`", which an implementer
reads as the fragment — over-reporting every facet count by the suppressed members carrying that value, and
differencing that against a computable count yields a per-value tally of suppressed items. The rule is
restated at the count site because that is where it is elided.

**The accepted timing channel's bound was wrong in both directions** (§3.2): a prober manufactures the cold
path by supplying its own reuse, and eviction re-colds on demand, while a publication does *not* re-cold at
all under §4.1's rule. The channel is still accepted; the registered residual is now the true one, and the
admission threshold counts distinct authorisation fingerprints so a single principal cannot cross it alone.
Eviction cadence joins the register as an accepted activity channel.

Three claims were scoped down. **I12 becomes coverable only in its mask half** — the frontier half and I3
stay blocked on the label service, and a test row for absent machinery is what decision 0013 forbids. The
**conformance relation carries values, not ordinals**, or the oracle reproduces the engine's key encodings
and stops being a second implementation. And **§5.3's range-summary machinery is marked ⊘** with its
placement stated, since no route serves a summary — the register entry is earned, the mechanism was
designed for an affordance nobody asked this document to serve.

Also added: **§6 defines the two layers** rather than naming them, since the differential compares served
sets and the cap interaction and broad-filter anchor were undefined; **§7 states where `overlay_pass` comes
from** and that it is principal-dependent, which per-point-attributes §3.3's "once per generation" phrasing
obscures; and three normative contradictions are now named — per-point-attributes §3.8's "and in work"
(§2.1), architecture §8.5's cache row keyed on partition and invalidated by ingest (§4), and contracts
§3.2's classification of `/v1/categories` as admission-free (§7).

**2026-08-08 (r1) — drafted**, after a three-lens review of the plan it was written from. What shaped it:
mode assignment is driven by disclosure rather than cost, and the owner then ruled the resulting timing
channel *accepted* rather than closed; the shared projection amortises across principals, not keystrokes;
bit slicing belongs in shared mode; the draft's proposed sublinearity benchmark gate was withdrawn as the
negation of the cost model rather than its operational form. Owner corrections at the same time: **strings
are not categories**, so `listing` has nothing to govern on one and prefix keeps its operand while losing
an autocomplete surface it should never have had.

[#44]: https://github.com/jennis0/tessera-index/issues/44
