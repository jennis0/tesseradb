# The filter index — design

**Date:** 2026-08-08
**Status:** **Provisional — r7, reviewed under two lenses and dispositioned in one pass** (Appendix
R). The category case is
built to this design; see the ⊘ notes for exactly what. The organising rule
changed at r4: the flat value column is the artefact of record and every accelerator is derived from it
(Appendix R). To become normative: confirmation of §2's constants at a value
width other than `u32` and on a string column, a ruling on surface §4's measured
project-vs-per-tile rule, and decision 0061's leak-register row landing (in flight on the postings
track; §6.2 names the dependency). **§6.3's rulings are all made** (owner, 2026-08-10). Measured input:
[`../../probes/2026-08-08-filter-layout/`](../../probes/2026-08-08-filter-layout/).
**Built so far:** the **read path, for every family** — the value column and its presence bitmap for
categories, strings and numerics; the masked scan behind all nine operators; `entity → value`; the
wire surface (`/v1/meta`'s operand list, the viewport's filter expression, the boolean tree) — and
the **whole write side**: a flush appends one extent per filterable column and the
reader composes base with extents (§5); the **extent coalesce** bounds their number between folds,
as the fourth axis of the engine's entity-space pass (§5.2); and the **fold's attribute pass** folds
what survives back into one base, blanks the deleted entities and rebuilds the derived postings
(§6.2). What remains unbuilt is two operands and one family: lists, `none_of` and `match` are
specified and refuse by name. Marked at each claim.
**Reads against:** architecture §4 (I2, I7, I9, I12), §9, §10.2–§10.4, Appendix A;
[`contracts.md`](contracts.md) §2.1–§2.4; [`write-path.md`](write-path.md) §2.1–§2.5, §4.3–§4.5,
§5.3–§5.4, §7; [`compaction.md`](compaction.md) §2–§4, §6, §9;
[`per-point-attributes.md`](per-point-attributes.md) §2–§3; design memo 2026-07-29 (secondary
attribute indexing); decisions [0013](../decisions/0013-mark-specified-vs-implemented.md),
[0039](../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0042](../decisions/0042-a-dictionary-extent-never-repeats-a-descriptor.md),
[0048](../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md),
[0050](../decisions/0050-a-fold-invalidates-the-term-index-and-every-fragment.md),
[0052](../decisions/0052-the-folds-page-cache-mitigation-is-a-hint-not-a-throttle.md),
[0056](../decisions/0056-a-folds-schedule-is-a-gated-window-not-a-pure-timer.md),
[0061](../decisions/0061-category-postings-serve-public-listings-and-never-per-viewer-ones.md);
[`probes/2026-08-08-filter-layout/`](../../probes/2026-08-08-filter-layout/).
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **index §n**. The companion read-side design is
[`filter-surface.md`](filter-surface.md), cited as **surface §n**.

---

## 1. Summary

A per-item attribute declared `used_for = "filter"` is stored as a **flat entity-space value column**:
values addressed by entity id, scanned under the viewer's authorised set to produce a bitmap, composed
by intersection under the filter contract (§8.2). Where a family's values have an integer identity of
their own and repeat heavily — which is **categories, and only categories** — a Roaring posting per
value is derived on top as an accelerator. This document owns the artefact and its lifecycle. What a
query does with it is [`filter-surface.md`](filter-surface.md).

**The flat column is the record; every accelerator is derived from it.** That is the whole organising
rule, and three things follow that an inverted-postings design could not give. A derived structure is
**self-retiring** — rebuilt whole at the fold, never a second durable identity, so no ordinal has to
stay stable across ingest and the C11 hazard that shape carries does not arise. `entity → value` is one
array index, so the conformance oracle's relation *is* the artefact rather than a side output the fold
could forget, and substring matching needs no trigram index — it is a region search over the values a
contiguous candidate covers, measured at 1.7 ns per candidate entity (§2.2). And the work a scan does is a function of
the candidate mask and the column, never of the value — which is what makes a hidden value and a
nonexistent one indistinguishable **in work**, as per-point-attributes §3.8 requires (§2.2).

Two further consequences. The artefact is the same one per-point-attributes §3.3 needs for vocabulary
visibility, so building it closed the `listing = "per_viewer"` refusal rather than deferring it. And it
is **entity-space, so it is slice-invariant**: a slice attaches, populates or drops without touching any
of it (§7).

> **⊘ Built: the read path for every family, and ingest.** The value column, its presence bitmap,
> the masked scan behind every operator each family declares, and `entity → value` all exist for
> categories, strings and numerics, in both build implementations and under the manifest digest; a
> category's derived per-value postings answer `eq` and `in` where its vocabulary is
> `listing = "public"`, and serve `/v1/categories`' membership question on every category that has
> them (§2.3). A flush appends an extent per column and a generation composes them (§5), and the
> **fold folds them back in**, blanks the deleted entities' slots and rebuilds the postings from the
> folded column (§6.2). Lists, `none_of` and `match` are unbuilt and marked at their claims.
> Present behaviour is fail-closed throughout: a filter that cannot be expressed narrows
> nothing, and a postings file or an extent that will not open refuses rather than reading as "those
> entities carry no value".

### 1.1 What this deliberately does not do

**No trigram index.** Substring matching over a `filter` string column is a **masked scan predicate**,
needing no index and no per-value loop: a contiguous candidate's values are adjacent bytes, so the
search runs over the region and maps its hits back through the offsets (§2.2). An earlier revision cut substring to [#44] because a trigram
conjunction returns a superset needing verification against the stored value, and an inverted-postings
design gave a filter-only attribute no `entity → value` route to verify against. That premise was a
consequence of the design, not a requirement; under a flat column the route is an array index. What
remains at [#44] is the *trigram acceleration* of substring, which is a different question from whether
substring is expressible.

**No column's values are enumerated except a category's.** `/v1/categories` serves a category's value
set because a category *has* one. No other family does, so none acquires a listing surface: no value
list, and in particular **no prefix autocomplete** — offering suggestions over a string column would
manufacture a value set for a type that has none. §2.5 argues the distinction.

**Negation is not an operand.** §8.2 composes by intersection, and `NOT` is a different shape: its
result is principal-dependent by construction, so it can never share a cached projection, and it
inverts a superset into a subset — the unsafe direction for any family producing one. It also
inverts the failure arithmetic every safety argument here leans on: §5's positivity property, which
any design lifting the fence must revisit first.

---

## 2. The artefact

### 2.1 The value column, and how it is addressed

A column's values live in entity order under `attrs/<column>/`. How they are addressed depends on
whether every entity carries one, and the choice is **measured, not judged**
([`probes/2026-08-08-filter-layout/`](../../probes/2026-08-08-filter-layout/), 10⁶–10⁹):

| Presence | Addressing | Cost of the addressing structure at 10⁹ |
|---|---|---|
| Every entity carries a value | **None** — the entity id is the array index | 0 |
| Partial | **A Roaring presence bitmap**; the *k*-th set bit's value is at slot *k* | 36 KB slice-blocked, 1.25 B/present scattered |

Two results from that campaign are worth carrying at the site, because both invert what the arithmetic
suggests. An explicit `(entity_id, value)` pair column — the obvious shape — is **never optimal on
either axis** at any scale or presence shape measured: it costs 4 B/entity *per column*, 4 GB per column
at 10⁹, and is never the fastest. And a run table `(start, len, base_rank)` is the smallest structure of
all — 12 bytes for an entire 10⁹ column — yet collapses on a broad candidate, 2.9 s to 10.2 s, because
rank becomes a binary search per candidate entity. **An addressing structure cannot be chosen from its
storage column.**

**Partial presence is the ordinary case, not the exception.** `x-tessera-slice` is per request and a
flush plans per slice, but write-path §4.2 makes a segment's entity range *ascending-with-holes* "where
a commit window interleaved slices" — merge's adjacency test is `hi < lo`, not `hi + 1 == lo`, for
exactly that reason. Under concurrent multi-slice ingest a dense positional extent would waste up to
(*S*−1)/*S* of its slots, so "the entity id is the array index" is single-slice reasoning and holds only
where a column genuinely covers everything.

### 2.2 The scan, and why its work carries no channel

A filter operand is evaluated by scanning the value column **under the candidate mask** — `M_auth`
pushed in first, as §8.2 requires — and the measured cost is per candidate entity rather than per
corpus byte: for a **category or numeric** column, **~0.28 ns** with a contiguous candidate and
**~9.6 ns** with a scattered one, stable across three orders of magnitude and linear in *n*. A
whole-corpus scan at 10⁹ is ~280 ms.

**Those are the cost of *finding* the matches, and they price a filter only while it is selective.**
An unselective predicate — a range over most of a domain, a set with every value ticked — is priced
by the size of its **result** instead, because the answer has to be built: measured at 10⁹, a
predicate matching a quarter of a whole-corpus candidate costs 3.4 s and one matching half of it
6.0 s, against ~280 ms to scan the same column selectively. It decomposes, measured, into roughly **40% mispredicted
branches** — the predicate is a coin flip at middling selectivity and perfectly predictable at both
extremes, so counting matches alone costs 4.6× more at 25% than at 100% over identical work — **30%
Roaring assembling a 250–500 million-entity bitmap**, and **30% buffer traffic**. So **no accelerator
over the column would touch it**: it is not the scan finding the matches, and no single one of the
three terms is a majority.

Removing the branch is the obvious fix and **was measured and refused**: in isolation it makes the
cost flat at ~445 ms across the whole selectivity range, a 10× improvement at its worst, but built
into the scan it cost the *selective* arms 1.5–2.1× to buy 1.3–1.6× here, leaving the case outside
the budget anyway. A viewer filtering to one category value is the common case and it is selective.
It becomes attractive only alongside a cheaper result representation, since it is the combination
that reaches the budget (probe arm 7). This is the largest known gap in the filter path and it is stated here rather than in the probe
alone, because a reader sizing against the constants above would not otherwise meet it.

Two things bound it rather than close it. Consecutive matches are added as **ranges**, which takes
the fully-matching case from 7.3 s to 878 ms and its result from 4.1 GB to nothing — an unselective
predicate matches in long contiguous stretches by nature. And the matches are folded into the result
every 64 Ki entities, which caps the transient buffer at 512 KB: it was previously proportional to
the result, **1.1 GB for a filter matching a quarter of the corpus, per concurrent request**, for a
quantity the compute-admission gate rations CPU for and knows nothing about (probe arm 7).

**A text column costs about fourteen times that, and the design must size against its own number**
rather than borrowing the fixed-width one (probe arm 6, at 10⁸):

| | contiguous | scattered |
|---|---|---|
| `eq` | 3.5 ns | 30 ns |
| `prefix` | 4.3 ns | 47–56 ns |
| `in` (5 values) | 8.2 ns | 61 ns |
| `contains` | **1.7 ns** | 96 ns |

The ratio is what the storage is: per value the scan streams two 8-byte offsets and the value's
bytes, about 22 against a `u32` column's 4.

**`contains` is outside the budget on a broad candidate of either shape, and by more than the
scattered column alone suggests.** At 9.7 ns a *contiguous* 25% candidate at 10⁹ is 2.4 s by this
table's own arithmetic — measured directly at 2,868 ms, and 3,890 ms for a needle many values share
(probe arm 12). The scattered column is worse still: 96 ns is ~1 s per 10⁷ candidate entities. An
earlier revision of this section read the scattered figure as the only cell over budget and did not
do the multiplication for the contiguous one; the correction matters, because the two have different
fixes.

Where the time goes was measured rather than assumed (arm 11), and it inverts between the two
shapes. Scattered, the 96 ns is ~31 ns of cache-line fetches for the offsets and the value's bytes
and ~52 ns of scanning, and the scanning increment is itself mostly latency on a value's second
line — so a faster inner loop recovers only 10–20%. Contiguous, memory is nearly free (1.1 ns of
10.9) and the loop is ~80% of the cost.

Two consequences follow, and the second is a floor rather than a gap:

- **A contiguous candidate does not need a per-value loop at all, and no longer uses one.** The
  values it covers are *adjacent bytes*, so the substring search runs over the concatenated region
  and maps its hits back through the offsets, discarding matches that straddle a boundary — the
  concatenation joins unrelated values, so `"ab" ++ "cd"` contains the bytes `bc` and neither value
  does. Measured 9.7 → 1.7 ns per candidate entity, which takes the 25% cell above from 2,868 ms to
  ~430 ms, at no storage cost. **Built.**
- **No candidate-driven scan brings a broad *scattered* principal inside the budget**, whatever the
  predicate does, because one random cache line per candidate entity is ~13–15 ns and 10⁸ candidates
  is therefore ≥1.5 s before any comparison. Closing that needs the per-value memory traffic to
  shrink — a packed descriptor of offset, length and a trigram summary in one word measured
  92.5 → 39 ns, at **+8 GB per 10⁹ column** — or an index, which is a disclosure question as well as
  a storage one. ⊘ **Neither is built**, and a scattered candidate therefore still scans per value.

**A `text` type is where this is expected to be revisited, and the split is deliberate.** `utf8` is
row data: its values are compared, never enumerated, and it is fastest at the predicates that
compare — exact match and prefix, at 3.5 and 4.5 ns. A separate **`text` type optimised for
in-query filtering** is the natural home for the structures priced above, because they buy substring
and phrase matching at a storage cost only a column declared for that purpose should pay. ⊘ **`text`
is not specified and not built**; today `contains` on a `utf8` column is the scan described here,
which is inside the budget for every candidate shape but a broad scattered one.

**Set membership costs what equality costs where the domain allows a table.** A `u8` or `u16`
category's whole code domain fits in 32 bytes or 8 KB, so `in` is a constant-time lookup built once
per scan — measured flat at 0.43–0.44 ns for 2, 8 and 32 values. That is also the stronger security
property: the work does not depend on *which* codes are named, so an unheld code and a hidden one
cost the same. A `u32` domain is not a table and keeps a sorted list at O(log k); a text set keeps a
first-byte bucket index.

Those are the **shipped scan's** figures, and the distinction earned its keep: the campaign's layout
arms reimplement the loop, which is right for comparing storage shapes and wrong for sizing the code —
the reimplementation was 23% optimistic against the scan as it then stood. Pointing the harness at
`ValueColumn` and then optimising it took the contiguous case from 3.10 ns to 0.25 ns, with identical
results throughout (probe arms 4 and 6). Three changes, and each is a property of the *shape* of the
work rather than of the values:

- **Iterate runs, not values.** A candidate run is a contiguous slice of the value column, because
  the entity id *is* the index. This is worth more still on the **partial-presence** path, where
  rank is what costs: slot *k* is the *k*-th set bit of the presence bitmap, so a per-bit walk pays
  O(present) however small the candidate is. Rank is affine *inside* a run — entity `e` in a run
  from `ps` with `base` bits before it is at slot `base + (e − ps)` — so merging the two bitmaps'
  runs gives every slot by arithmetic. A 1% candidate over a slice-blocked column went from
  124.89 ms to **0.30 ms**. Arm 1 read that cell as a cost of the addressing structure; it was the
  rank algorithm, and the presence bitmap is now a filter on work rather than a tax on it.
- **Traverse at the column's own type**, so the `Codes` dispatch and the widening to a common
  numeric leave the inner loop, and a numeric bound is narrowed to the column's native type once per
  scan. Narrowing also settles the degenerate cases once rather than per element: a bound below the
  type's floor constrains nothing, one above its ceiling excludes everything, a fractional bound on
  an integer column rounds outward.
- **Compare bytes, not `&str`.** Resolving a text slot to a `&str` runs a UTF-8 validation per value
  per request, over bytes the file format has already validated — which was the dominant term in
  every text predicate. Byte comparison answers the same question because UTF-8 is
  self-synchronising: a valid needle cannot match starting part-way through a character.

**One traversal serves every family, and that is a security property before it is a tidiness one.**
It hands out contiguous *slot ranges* rather than single slots, which is what lets a fixed-width
column walk a slice of values and a text column walk a slice of offset pairs, each without a bounds
check per element. Because the ranges are a function of `(candidate, presence)` alone, a predicate
cannot skip work whatever it is testing for — so adding a family adds a comparison and cannot add a
channel.

**These constants are a property of the hot loops' *addresses*, and the fast value above is one of
two.** An unrelated addition to `tessera-filter` moves them 30–70% — three times from code that
never runs during a scan, once from a function never called at all — and the mechanism is
instruction-address alignment, not code generation
([`scan-constant-sensitivity`](../evidence/memos/2026-08-11-scan-constant-sensitivity.md)):
the perturbed builds emit the hot functions instruction-for-instruction identical and merely place
them elsewhere, and padding the crate's text with inert bytes reproduces the whole effect as a
function of shift mod 64. Over the layouts measured the packing path's constant takes **~0.25–0.28
or ~0.42–0.44 ns and nothing between**, two of the four 16-byte residues landing on each. **Read
every figure in this section as the favourable draw**; an unlucky link of byte-identical scan code
costs 65% more. The memory-bound cells — every scattered candidate — are immune, which is why the
9.6 ns and the scattered text figures carry no such caveat.

⊘ **The remedy is identified and not applied.** Pinning function starts to 64 bytes
(`-C llvm-args=-align-all-functions=6`) closes the channel by which unrelated code reaches the
constant, at no measurable baseline cost against `codegen-units = 1`'s ~15%; it is a workspace
profile change and is not made here. Until it is, **any change to this crate must be A/B'd
interleaved before its constants are believed** — and even after it, a change to the hot files
themselves still relocates their own blocks and still needs the discipline.

The typed change carries a caveat worth stating at the site, because the obvious form of it is a
regression: **a scattered candidate is one-element runs**, and building a slice iterator per run
costs more than the direct index it replaces — measured at +20% on the scattered arm before a
length-1 fast path was added. Anything that revisits this traversal must measure both candidate
shapes.

**The budget is a filter budget, not a viewport budget** (owner ruling, 2026-08-08): **0.5–1 s is
acceptable for a filter change at 10⁹**, because a filter changes far less often than the viewport does.
That ruling is what makes the scan the general answer rather than a fallback, and the arithmetic is not
close:

| Candidate | Affordable coverage at 10⁹, 1 s |
|---|---|
| Contiguous | ~4×10⁹ entities — **four times the whole corpus at 10⁹** |
| Scattered | ~1×10⁸ entities — 10% |

Measured, a 25%-coverage contiguous principal costs **61 ms** over a universal column and **12 ms**
over a slice-blocked one: inside the band by more than an order of magnitude, with no accelerator of
any kind. **No coverage exceeds the budget on a contiguous candidate** — the whole corpus scans in
~240 ms — so the corner an accelerator addresses is a scattered candidate at high coverage, and not
"broad coverage" as an earlier revision framed it against a borrowed 50 ms viewport budget.

The security property this buys is the reason it leads. **The work is a function of `(candidate,
column)` and never of the value**, so a value the principal cannot see costs exactly what a value that
does not exist costs. per-point-attributes §3.8 requires those to be indistinguishable in work, and no
inverted-postings design achieved it — filter-surface §2.1 superseded the requirement because option A
could not meet it. That supersession is **withdrawn**: the requirement holds as originally written, for
every scanned family.

> **Sizing the scan as a bandwidth problem misleads in both directions, and which direction depends
> on the family.** A model assuming 5–10 GB/s predicted 40–80 ms for a 25% principal at 10⁹ against a
> then-measured 730 ms — ~5–7× optimistic. The same cell now measures **66 ms**, which that model
> would have called *pessimistic*: a fixed-width column at 0.25 ns per 4-byte value is ~16 GB/s,
> above what the model allowed, because a contiguous candidate walks the array sequentially and
> prefetches. A **text** column at 3.5 ns per ~22 bytes is ~6 GB/s and *is* at bandwidth. So the
> fixed-width bound is per-candidate work and moves with the loop; the text bound is the bytes and
> moves only if the storage does.

### 2.3 The category accelerator

**This is an optimisation for the privileged tail, not a requirement — with one exception.** Under
§2.2's budget the scan alone serves a category at **any** coverage, contiguous or not; a category column *may* also
carry **one Roaring posting per value**, derived from the column and rebuilt whole at the fold, to keep
near-total coverage inside the band as well. Because it is derived, a deployment that builds it and one
that does not answer identically and differ only in latency — so this is ordinarily a per-column build
choice, not part of the contract.

**The exception is `listing = "per_viewer"`, where the postings are owed** (owner ruling, 2026-08-08).
That control gates the existence of a value name, and the gate is membership-derived: a value is offered
only if the principal can see an item carrying it (per-point-attributes §3.3). The member sets it needs
*are* these postings. Deriving them instead by scanning the value column per request is inside a
*filter's* latency budget but not inside `/v1/categories`', and it would falsify contracts §3.2's
compute-admission justification for that endpoint — "no mask composition, no projection, no file IO".
So a `per_viewer` category gets postings whatever its `used_for` says, where a `public` one gets them
only from `used_for = "filter"` — a published value set is served as authored and derives no membership
at all.

**The postings answer a filter only under `listing = "public"`** (decision 0061). Under `per_viewer`
they exist for the membership question above and the *filter* is answered by the masked scan, for the
timing reason the note below this section records. The route is a function of the **declaration** —
never of the request, the principal, or any statistic, which §8.2 forbids because a statistics-driven
route makes execution time a function of how much the principal can see — so it is fixed at schema time
and identical for every viewer. It is registered as leak-register row **C24**, which decision 0061 makes
a condition of itself.

**A routed answer is `postings ∩ candidate` unioned with a scan of every extent layer.** The postings
cover the base build's entity range and no flush writes any (§5), so an answer taken from them alone
would omit every entity ingested since — narrower, safe under **I12**, and *indistinguishable from a
correct one*, which is the failure this artefact's whole composition rule exists to avoid. The same
union is what `/v1/categories`' membership question takes: a value carried only by post-build entities
must still be offered to a principal who can see one of them. **A postings file the manifest names but
that will not open is a refusal**, never a fall back to the scan and never an empty answer — the first
would answer correctly while hiding a broken artefact, and the second would say no entity carries the
value.

Measured at 10⁹, the same predicate answered by intersection rather than scan costs **3.56 ms at 25%
coverage** and **49.5 ms in the worst cell measured** (a scattered posting whose value covers a quarter
of the corpus, at full coverage) against 5,271 ms scanned — a 107× reduction landing inside the
operating point. Storage is small beside the column it accelerates: 8 B–54 KB for a correlated posting
at 10⁹, 2–125 MB scattered, against a 1 GB `u8` column.

**Categories earn this and other families do not**, for two reasons that do not generalise. A category
value already has an integer identity — its vocabulary code, minted by an authority that exists anyway
— so nothing is derived alongside it and no second durable quantity has to stay stable. And a category's
values repeat heavily, so one bitmap replaces millions of repeated codes. A string or a numeric has
neither property: identity would have to be manufactured, and that manufactured identity is precisely
the C11 hazard §2.5 records.

**The intersection is evaluated directly, and carries no measurable timing channel.** A candidate-driven
form — walking the candidate's containers and probing the posting per container — was proposed to
equalise work across values. Measurement shows it unnecessary: at 10⁹ and 25% coverage, a value with no
members and a hidden value with 250M members both cost **0.000 ms** intersected, because Roaring
short-circuits on container keys and a posting whose containers do not meet the candidate's costs a
key-list merge and nothing more. The candidate-driven form costs a ~1.1 ms floor on every operand and
buys nothing, so it is not adopted.

> **The case an earlier revision marked unmeasured is now measured, and the residual is real**
> (probe arm 9): a scattered posting whose containers the candidate meets while no bits match costs
> ~2 ms per operand at 10⁹ against ~0 for an absent value — container-proportional work for an
> empty result. That measurement is what decision 0061 is built on: a `per_viewer` column never
> takes the postings route, because under that control the ~2 ms *is* the disclosure, while under
> `listing = "public"` what the timing distinguishes is a fact `/v1/categories` already serves.

### 2.4 Separate files, separate types, and a shared format core

Attribute artefacts live under `attrs/`, beside but never inside `terms/`.

per-point-attributes §3.5 gives the binding reason and it is an authorisation argument. A membership
posting is an **attribute** term, which may only narrow `M_sel` (**I12**). An authorisation term gates
label containment and frontier depth (**I3**). The dictionary writer interns raw descriptor bytes
supplied by the caller's plugin, so a namespace tag placed *inside* a descriptor lives in a space the
caller also writes into: an attribute descriptor byte-equal to a satisfied auth descriptor would union
every item carrying that value into `M_auth`. Separate files make the collision impossible rather than
prevented.

**The separation is also a type property, and that requires the format to be untyped.** The two indexes
share a *format*, so a call site could pass an attribute ordinal where an authorisation term ordinal
belongs. The resolution is a **raw-`u32` format core** — the CSR reader and writer, the tiered records,
the delta tier and the term sweep, all taking a bare ordinal — with each consumer wrapping it in its own
newtype at its own boundary: `TermId` in the authorisation crate, `AttrLocalId` in the filter crate, no
conversion between them, held by `trybuild` compile-fail rows exactly as `EntityId` and `RowId` are under
**I4**. A shared reader typed in one consumer's newtype would force the other to convert at every call —
which is the cross-wire the newtypes exist to forbid, reintroduced as boilerplate.

### 2.5 One file per column

```
attrs/<column>/values.arrow            the value column, entity-ordered (§2.1)
attrs/<column>/presence.roaring        present entities — omitted where presence is universal
attrs/<column>/postings.arrow          derived per-value postings — categories only (§2.3)
attrs/<column>/extents/<flush_id>.arrow    one extent per flush, its values in entity order
attrs/<column>/extents/<flush_id>.roaring  that extent's present entities — never omitted
```

**An extent's presence bitmap is not optional, where a base column's is.** The base column omits it
to mean *the entity id is the array index*; a flush publishes an entity set that starts above the
build's high-water and need not be contiguous (§2.1), so an extent read positionally would pair
every value with the wrong entity. Both files are named in the partition's side-manifest and
digested, so a missing one refuses at open rather than reading as "those entities carry no value" —
which is the wrong answer that looks exactly like a right one.

**One record batch per value column**, which is what lets the reader map the file and borrow the
values out of it rather than copying them (§8). A second batch is refused rather than concatenated,
because concatenating is exactly the copy the mapping exists to avoid. A `utf8` column is written
`LargeUtf8`: 32-bit offsets cap the concatenated bytes at 2 GiB, which a 10⁹-entity column passes at
two bytes a value.

**Per column, not per column group**, and the reason is a format constraint rather than a preference:
per-column presence makes the compact value arrays *different lengths*, and one Arrow record batch
cannot hold columns of differing length. Grouping survives only for a set of columns that are all
universal-presence and fixed-width — where it saves nothing, because under §2.1 there is no `entity_id`
column to share. Contracts §2.4's rule against per-*term* files — *"a hundred million inodes is not a
format"* — does not reach here: the file count is the number of declared filterable columns, which is
tens.

**Each column owns its identifier space where it has one at all.** For a category, `local` is the
vocabulary code, meaningless outside its column. An earlier revision put every column's postings in one
positional file addressed as `base + local`, and it does not survive ingest: a value minted at a
commit-window close after the build takes the next local ordinal, which is the *next column's* base, and
`base ∪ tiers` then unions one value's members into another's. Because vocabulary visibility is
membership-derived (per-point-attributes §3.3), that shows a value to a principal on the strength of a
different value's members — **C11**, reachable by ordinary operation. §6's carry-forward rule then
forbids the repair. Per-column files remove the arithmetic rather than defending it.

### 2.6 The families

| Family | Stored as | Predicates | Accelerator |
|---|---|---|---|
| **Category** | flat code column, `u8`/`u16`/`u32` | equality, set membership | **one Roaring posting per value** (§2.3) |
| **String** | flat UTF-8 column | equality, set membership, prefix, substring — all scan predicates | none |
| **Numeric / timestamp** | flat column in the native encoding | equality, range | none built; §3 states the open corner |
| **List** | flat list column | as the element family | as the element family |

`bool` is the degenerate numeric. **Lists cost no new value storage but do cost addressing work,
and an earlier revision claimed otherwise**: "more than one value per entity" breaks §2.1's
one-presence-bit-one-slot rule, on which the affine-rank traversal, §6.2's blanking and §5.2's
merge are all built — so the list family needs its own addressing (a per-entity value count or
offset beside presence) before any of those specifications extend to it, and none of them claims
to cover it meanwhile. Lifting the parse refusal is therefore an addressing design plus a schema
and build change, and it lifts **for `filter` only** — `inspect` has no sidecar to place data in,
and decision 0013 requires an unimplemented placement to stay refused naming itself.

Decision 0039's fence stands, restated because this is where someone will look for permission to cross
it: a multi-valued attribute is **never** `render`, and **no projection, derived value or summary of one
earns a hot column on its behalf**. The encoding that looks free is the disclosure — a "has more values"
bit is computed over the full value set and baked into the row, so a principal who knows their only
visible value on a point and reads that bit has learned the point carries a value they cannot see.

**A string is not a category, and the difference is in the data model rather than in the index.** A
category's value is a **vocabulary entry**: a named object with an identity, a pinned code, properties
and a lifecycle, existing independently of any row, referenced from a row by its code, and served as a
set by `/v1/categories`. A string is **row data**, exactly as a number or a timestamp is — its visibility
is the visibility of the rows that carry it, and no object stands behind it. So only a category has a
value set, and therefore only a category has a `listing`: the `per_viewer` control gates the *existence
of a value name* (C11), and a string column has no name to gate, publishes no value list, and
contributes no C11 surface.

Under a flat column that distinction also stops costing anything. A string needs **no dictionary, no
FST and no index** — nothing has to manufacture an integer identity for it, so the "interning is not a
vocabulary" argument an earlier revision needed is moot. Prefix and substring return **matching
entities**, which intersect `M_auth` like any operand and whose every count is masked, so a caller
walking `sm` → `smi` → `smit` learns only about rows they could already see. They do not return
suggestions.

---

## 3. Numerics, and the one corner still open

A numeric or timestamp column is stored in its **native encoding and compared natively**. There is no
order-preserving key: the sign-flip/XOR mapping exists to make IEEE bits sort correctly when compared as
*unsigned bytes* in a byte-ordered key store, and nothing here compares them that way. Native `f64`
ordering is already correct, and gives NaN-matches-nothing for free, since every comparison against NaN
is false. A NaN is therefore absent from every range result without a rule being written for it.

**The level tree and range-encoded bit slicing are both cut from the base design.** Meilisearch's
`facet_id_f64_docids` level tree decomposes a range over *per-value postings*; range-encoded BSI is
FeatureBase's answer *because* FeatureBase is a bitmap engine. Neither premise survives a flat column,
and an earlier revision specified both — along with a probe to calibrate the crossover between them,
which is no longer a question that exists.

**Broad numeric ranges need no accelerator, and both candidates are declined** (owner ruling,
2026-08-08). A range is a scan, and §2.2's budget puts a 25%-coverage range at 202 ms measured — inside
the band several times over. The two structures an earlier revision reached for are therefore not built, and the reasons
they are *declined* rather than deferred are worth stating, because one of them is a security result:

- **Zone maps** — per-block min/max, skipping blocks that cannot match — are declined **because they
  would cost a disclosure for a speed-up the budget does not need**. The skip consults **unmasked**
  extrema, so a block whose *visible* rows do not match scans slower than one where nothing matches:
  timing reveals whether invisible rows fall in the queried range. That is C4's shape, and Appendix C has
  no row covering it. Adding a leak-register row to buy latency that is already inside budget is the
  wrong trade, and declining them keeps §2.2's work-indistinguishability property whole.
- **Bit-sliced indexing** is declined as unnecessary rather than harmful. It is I2-clean and would give
  masked min/max and sum for free — the argument for revisiting it is an *aggregate* argument, not a
  range-latency one, and it should be made on that ground if it is made at all. No mature Rust library
  exists, so it is a few hundred lines against a published design, and its ~half-dense slices are what
  Roaring barely compresses.

**So a numeric column is a value column and nothing else.** The scan is exact, carries no timing channel,
and needs no per-column structure choice, no build-time cardinality measurement and no fold rebuild.

## 4. Build

The batch build produces this artefact in a stage of its own, after entity assignment and before the
tiler sort ([`crates/tessera-build/src/pipeline.rs`](../../crates/tessera-build/src/pipeline.rs)).

**The placement is forced, not stylistic.** Entity IDs are assigned by the **authorisation** signature
sort and are permanent under **I9**, so attribute values cannot influence the order. And they are not in
hand where the authorisation postings are written — they arrive in a later pass over the points file —
so the emit cannot ride on that stage. It is its own stage, reading values in entity order, which is
also what makes each derived posting's entity list ascending, so the writer's unconditional sortedness
check re-verifies from disk exactly the property the loop established.

**What the build writes per filterable column:** the value column in entity order; a presence bitmap
where presence is partial (§2.1); and, for a category, the derived postings (§2.3). Every file is
registered in `MANIFEST.files` and digested, so the artefact is under the same digest-or-refuse rule as
everything else in a bundle.

> **⊘ Built: the category case.** Both build implementations emit the value column, its presence
> bitmap where presence is partial, and the derived postings — byte-identically, the streaming build
> reading its attribute values column-major and the reference transposing them out of the staged
> items, with the equivalence test comparing the results. A test asserts the postings agree with the
> column value for value, which is what makes one a derivative of the other rather than two writers
> that happen to agree today.

**The emit is banded, and streams into both writers.** The value column is pushed to the streaming
writer in bounded chunks; the postings take a counting pass for band prefix sums, then one column
scan per band cursor-scattering into a flat buffer sized by them, appended through the keyed
postings writer — the construction §6.2 specifies, written once and called by both producers. The
band budget is a constant the emit chooses, and the counting pass's residue is one count per
distinct code, vocabulary-sized by definition. *Measured* at 2×10⁸ over a fully covered `u32`
category (`probes/2026-08-08-filter-layout/run-writers-2e8.csv`): the postings emit's peak
anonymous residency fell from 832 MB to 269 MB — the band buffer plus the counts — and the value
column's from 800 MB to 1 MB. Both writers still map their spool at assembly, so the process's
high-water resident set includes the finished artefact once as clean page cache; what banding
bounds is the heap, which is the term the plan is written against.

What this does **not** bound is the attribute tail it reads from: the reader materialises
`Vec<ScalarValue>` per column at ~24 B per value, which at 10⁹ is tens of GB and outside the memory
plan regardless. That ceiling is the reader's, stated where it is paid; the emit no longer adds a
second copy of the column to it.

The two fail-closed scatter checks carry over unchanged and for unchanged reasons: an overflow check,
because one term's entities silently becoming another's is a disclosure; and a short-fill check **by
count rather than by value**, because zero is a valid entity ID.

### 4.1 What contiguity is worth here, and what it is not

Postings compress because entity IDs are signature-sorted — *measured* at **8.9–36.7× on posting storage
and up to 130× on union at equal coverage**. A derived attribute posting **does not inherit that
automatically**, because the sort is on *authorisation* signatures: an attribute's contiguity is a
property of how well it correlates with the label set, which is a property of the deployment rather than
of this design. The category-membership probe measured the spread across two label sets chosen to
bracket it — membership sets cost **0.31×** the render column they index when the attribute tracks the
labels, **1.01×** for an orthogonal control.

**The latency consequence is smaller than an earlier revision claimed, and the correction matters.** That
revision carried the 21.7 ms / 2,885 ms spread — a 13× per-container constant — as evidence that a
posting's shape could make it useless. That measurement is a **union over ~10⁴ authorisation term
postings** during `M_auth` construction. A single value intersected against a candidate is one bitmap
operation and never enters that regime: measured at 10⁹, the worst posting shape at the worst coverage
costs 49.5 ms (§2.3). **Do not cite the union spread against a filter operand.**

What contiguity does still govern is the *scan*, where it is worth **~40×**: ~0.24 ns per candidate
entity contiguous against ~10 ns scattered. A contiguous candidate collapses to a handful of runs, so
the scan walks slices of the value column; a scattered one degenerates to a run per entity and pays
cache misses on every read (§2.2).

---

## 5. Ingest

**A value column extends by appending an extent.** The commit window is signature-sorted and allocated
from the high-water in one call (write-path §2.3), monotone and never reused under **I9**, so a flush's
entities are ids no earlier layer holds and nothing already written moves. A scan decomposes across
the layers by clipping the candidate to each — which the extent's own presence bitmap does, at
O(containers touched) — and the results are unioned. The union is disjoint by **I9**, and that is
*checked* at composition rather than assumed: two layers claiming one entity would make it match two
values at once, and a filter naming either would return it with nothing to notice.

**Composition is per generation, not per request.** A published flush builds the next generation's
columns from the live ones by pushing a pointer, so a request pays one scan per layer over a
candidate the layer has already narrowed, and the layer count is a property of the bundle rather than
of what is asked for — the work stays a function of `(candidate, column)`, which is what §2.2's
timing property requires.

That is the whole of it, and the absences are the point:

- **No dictionary, so no promotion and no resolver.** Nothing has to mint an identity for a value at
  admission, so there is no extension-id space, no resolve-then-intern at flush, and no discard-and-replan
  when a dictionary moves under a promoting flush. An earlier revision specified all of it.
- **No `max_distinct_values` bound.** It existed to stop an unbounded dictionary. A flat column's cost is
  its values, whatever their cardinality.
- **No quadratic near-unique-string hazard.** The shape that broke the previous design — a near-unique
  string column, where a map-backed dictionary clones its lookup map per promoting flush at a *measured*
  7.1 GB per copy at 1.17×10⁸ terms, against a dictionary growing at ingest rate — does not arise.
  Appending strings to a flat column is O(bytes) whatever the cardinality.
- **No per-flush delta tiers for the record.** A tier existed so a posting could be extended without
  rewriting it. A column is extended by appending.

**A category's derived postings do not extend.** They cover `[0, fold_watermark)`; entities above the
watermark are answered by scanning the appended extents and unioning the result in. At the owner's stated
ingest rates a day is ≤ ~2.4×10⁷ values, and the scan constant (§2.2) puts that at ~10–20 ms per operand
— *modelled*, from a measured constant. So the accelerator is rebuilt at the fold and never maintained
incrementally, which is what makes it self-retiring and keeps ordinal stability out of the design.

**Resolution stays fail-closed in the same direction and for a simpler reason.** A buffered entity has no
row, so no row-space verb sees it; the entity-space verbs under-report until flush. Under-reporting
narrows `M_sel`, which is safe under **I12** — and it is a lag of one flush interval rather than a
coverage cliff, which is what makes it a design rather than a gap.

**Every "degrades safely" argument in this document rests on one property, named here so it cannot
be lost silently: every shipped operand is positive.** An entity whose value is unreachable — not
yet flushed, in a layer that failed to compose, blanked at the fold — matches *no* positive
predicate, so any failure that loses values under-reports, under-reporting narrows `M_sel`, and
**I12** absorbs it. A negative operand inverts that arithmetic: under `none_of`, an entity with no
reachable value *matches*, so the same failures — a lost layer, a lagging flush, a blanked slot —
**widen** the result instead of narrowing it. `none_of` is fenced today for a different reason
(decision 0060's existence oracle over gated vocabularies), so nothing enforces this one; the
review that ever lifts that fence must therefore revisit layer composition and every start-up
failure mode in §6.2 under the inverted sign, and this paragraph is the tripwire that forces it.

> **⊘ Built, and the accelerator's tail is answered by scanning it.** A flush writes one extent per
> column that **owes a value column** — `used_for = "filter"`, *or* a category whose vocabulary is
> `listing = "per_viewer"`, which is the build's own `postings_are_owed` rule mirrored on the write
> side (§2.3). It writes one for a column no flushed entity carries a value in too, so the file set
> is a function of the schema rather than of the data; it names both files in the partition's
> side-manifest, digests them, and composes them onto the live generation's columns before the
> manifest commits.
>
> **The two halves of that rule have to agree, and for a while they did not.** The build owed a
> `per_viewer` render-only category its postings — that column's postings are not an accelerator but
> the evidence §3.3's visibility predicate is derived from — while the flush selected extents on
> `used_for` alone. A value first carried by an entity ingested after the build then existed in no
> artefact any reader consults, so `/v1/categories` could never offer it, permanently and with no
> symptom. One predicate now serves the flush's selection and the reader's open.
>
> A category's derived postings still cover `[0, fold_watermark)` only, because nothing writes
> postings for an extent. **The un-folded tail is scanned instead**: a routed filter unions the
> postings' answer with a scan of the extent layers, and `/v1/categories` sweeps those layers for the
> codes their entities carry (§2.3). Both are exact; what the tail costs is a scan proportional to
> the extents, which §5.2's coalesce and §6's fold are what bound.

### 5.1 What layer accumulation costs, and what bounds it

Every flush adds a layer and only the fold removes one, so between folds a query scans a growing
list and a generation open maps one. Both costs are now **measured** rather than argued
(probe arm 13, at 10⁹ over the shipped scan, medians of three): a layer the candidate's entities
never reach costs nothing measurable — the per-layer `candidate ∧ presence` short-circuits on
container keys — and a layer the candidate does meet costs **~9 µs of layering overhead** on top of
its entities' own scan cost, which a folded base would pay anyway. At 960 layers — a day of
continuous ingest at the 90 s flush tick — the worst operand shape measured (full-corpus candidate)
pays +15.3 ms over a zero-layer column, ~5% over the folded equivalent and far inside §2.2's
0.5–1 s budget. Modelled linearly from that constant, a week without a fold (~6,700 layers) adds
~110 ms per operand.

**What accumulates fastest is files, not scan time.** Two files per column per flush is ~31,000
files a day at sixteen columns: manifest entries, digest-sweep members (§8), and 28 ms per column
of open-and-compose at 960 extents (measured, arm 13). That is the axis that needs bounding, it is
a *file-count* axis rather than a query axis, and the system already has the pass whose job that is
— §5.2 makes attribute extents its fourth axis. What the fold is then left holding is §6.2.

### 5.2 The extent coalesce: a fourth axis on the entity-space pass

⊘ **Built (2026-08-10).** The pass is the engine's entity-space coalesce with this axis added; the
merge is `tessera_filter_write::coalesce_attr_extents`, the replace is `FilterColumns::with_coalesced`,
and everything below is what runs.

The engine's entity-space coalesce (write-path §7; `tessera-engine`'s `coalesce` module) already
bounds three per-flush, entity-space, accumulating axes — delta postings tiers, dictionary extents,
external-id runs — as a **content-preserving re-encode**: no `segments_version` bump, no cache key
rotated, no projection or fragment invalidated, everything named by path and therefore ABA-safe
against concurrent flushes. Attribute extents are the same shape as delta tiers, and take the
same safety argument: **layers are unioned at composition, so their division into files is
immaterial; what must not change is the set of `(entity, column, value)` triples**, which a merge
of disjoint extents preserves exactly. Like every pass in that module, **a coalesce retires
nothing**: a deleted entity's value rides through untouched, because removal is the fold's (§6).

The mechanics, against the pass's existing shape:

- **Selection is per column, over that column's own extents in list order.** `AttrExtent` carries
  the column's declared name, so each column's extents are a subsequence of `attr_extents` with no
  path parsed and no flush identity needed — the selection unit is the column, which exists in the
  format today, where "a window of flushes" did not (an `AttrExtent` records no flush, and §2.5
  forbids recovering one from the path). Any contiguous window of the column's subsequence
  qualifies, for the tier axis's reason: a union has no order. The recursion is free — a coalesced
  extent is an entry in the same per-column subsequence and is selected at the next rung
  identically. One pass may take windows in several columns and publish them together; a coalesce's
  file set is therefore data-driven, and deliberately so — §2.5's file-set-is-a-function-of-schema
  property belongs to the *flush*, where an operator predicts what ingest produces, not to a
  maintenance pass that fires where the policy says there is work.
- **The policy's knobs carry over per column, which is what keeps one heavy column from starving
  the rest.** `width` (8) is how many of a column's extents collapse into one; the size `floor`
  (1 MiB) does for extents exactly what it does for tiers — a flush's extent is ~100 KB at the
  owner's stated rates, so without the floor every tick would mint its own size class and the pass
  would silently never fire; the size tier is computed over the *column's own window*, so the
  ladder is per column and well defined. The input cap (256 MiB) bounds the pass transient —
  the input extents' values and presence held during the merge — and applies per column: a text
  column whose values outgrow it stalls **itself**, never its neighbours, and for that column the
  window narrows to the widest `width ≥ 2` that fits the cap rather than silently reverting to
  unbounded file growth. A column one extent of which alone exceeds the cap is genuinely
  uncoalesceable and waits for the fold; that is a statement, not an oversight.
- **The merge is `coalesce_attr_extents`, per family, and it refuses overlap itself** — the
  composition check cannot: `compose` tests disjointness *between* layers, so once eight extents
  become one file an overlap among the inputs is internal to a single layer and invisible to it
  forever. The merge therefore carries the axis's own guard, as the dictionary axis carries its
  never-repeat guard: the output presence is the union of the inputs', refused unless the union's
  cardinality equals the sum of the inputs' — O(containers), checked before any value is written.
  Values merge by family: a fixed-width column concatenates its inputs' slices in entity order; a
  text column concatenates value bytes and rebases offsets. **Lists are excluded** until §2.6's
  addressing for them exists — they refuse at parse today, so nothing is fail-open — and the
  output is the one record batch §2.5 requires, via the streaming writer §6.2 makes a deliverable.
- **Output lands under `coalesced/<id>/attrs/<column>/`**, on contracts §2.1's existing precedent
  for entity-space output that belongs to no segment — the coalesced tier and run already live
  there, and the never-reused `<id>` rule is what stops two passes truncating each other's mapped
  files. No format change follows: `attr_extents` names paths, never a path convention (§2.5's
  own rule), so the manifest edit is remove-consumed, insert-coalesced, exactly the tier axis's
  edit. A failed pass leaves the directory an orphan nothing references, the tier axis's posture —
  ⊘ and, like the tier axis's orphans, it waits on an in-prefix orphan sweep that is **not
  built**; this axis makes that debt heavier by up to a column-count multiple per failure, stated
  here rather than discovered.
- **Composition is by *replace*, which is a second operation beside `compose`'s append.** The
  successor generation's column replaces the consumed layers with the coalesced one, and its
  correctness condition is *different* from append's: the coalesced layer's presence must **equal**
  the union of the consumed layers' — tested as a bitmap equality, refused on mismatch — or
  `covered` drifts silently and every later disjointness check tests against the wrong coverage.
  Checked at the same register as the rest of this section: the merge's own guard (above) makes
  the equality unreachable, which is exactly why it is cheap to verify and wrong to assume.
- **The publication order is the flush's, for the flush's stated reason: compose first, manifest
  second, swap third.** The completed pass carries the **opened** coalesced column per window —
  `FlushedExtent`'s precedent, so publication cannot fail on IO after the manifest edit — and the
  executor builds the successor `FilterColumns` by the replace operation *before* writing the
  manifest: a composition that refuses must not leave a published manifest naming layers this
  process cannot serve, and the reverse order commits a manifest whose own writer then refuses it.
  A refusal discards the pass — files become orphans, the consumed entries stand, the next tick
  re-plans. Nothing row-space moves at the swap, so decision 0043 is satisfied by construction.

**What this bounds — and the mechanism is not the one this section first claimed.** The conclusion
holds: repeated coalescing takes the layer count from a day's ~960 to a bounded handful, which is
what returns the open path and makes §5.1's "a week without a fold" arithmetic moot. The file-count
axis no longer waits for the fold. The query-axis saving is real and unimportant — layers were
already microseconds each (measured, §5.1).

**But the decay is linear, not tiered.** An earlier revision said repeated width-8 coalescing walks
the same size-tier ladder the segment merge does. Measured on real data
(`probes/2026-08-10-filter-lifecycle/`, 64 extents per column driven to one in nine passes): a pass
removes **seven extents per column**, every time — because every extent sits below the policy's
1 MiB size floor and therefore in one size class, so a window is always eight of them and always
yields one. The ladder needs inputs that *differ* in size class, and a flush's extents do not. That
is why the floor exists at all — without it each tick would mint its own class and the pass would
never fire — but it also means the ladder's geometric collapse is not available here, and a
deployment far behind on coalescing pays passes linear in its backlog rather than logarithmic.
**"Walks the size-tier ladder": NOT confirmed by measurement — do not claim it is.**


## 6. Deletion, suppression and retirement

Write-path §5.4's two removal rules govern, and **conflating them is fail-open**. The distinction has been
lost twice in this project's review history, which is why it is restated at every site that touches it.

| Event | The filter artefact | Where the invisibility lives |
|---|---|---|
| Suppression | **nothing** | the overlay's `suppressed` set; retires **only** on unsuppress (Rule S) |
| Unsuppress | nothing | the entity leaves `suppressed`, and the derived mask is re-derived from scratch rather than subtracted incrementally |
| Deletion | **nothing until the fold** | the overlay's `deleted` set; the flush never writes the row, and the entity ID stays burned (**I9**) |
| Compaction fold | deleted entities' value slots are blanked; the derived postings are rebuilt whole | the tombstone leaves `deleted` in the fold's own publication (Rule F) |

> **⊘ Built, every row of it.** Rules S and F are enforced for the authorisation index and the fold
> executes them; the attribute pass (§6.2) is what makes this table true of attribute data too. A
> suppression changes no attribute artefact — asserted against the folded files, not only against an
> answer — and a fold blanks exactly `D₀`, the same set its other passes take, rebuilding each
> category's postings from the folded column.

**Why the fold blanks a deleted entity's slot, and it is not Rule F's reason.** Decision 0050 requires a
deleted entity to be gone from the *authorisation* term index, and its argument is a fail-open: leave the
entity in the term index and, when the overlay entry retires, the item becomes visible to every
authorised principal permanently. **That argument does not transfer here.** A filter column is not
authorisation — the fold has already removed the entity from `M_auth`, so a masked scan never visits its
slot whatever it holds, and no retirement can resurrect it.

The reason is a **retention asymmetry** instead. After a fold, a deleted item's *render* value is gone
because its row is gone; its *filter* value persists, because the slot is positional and **I9** forbids
renumbering it away. Blanking removes that asymmetry. It is a weaker obligation than Rule F and must not
be described as one — this project's deny model depends on the two retirement routes never being
conflated, and borrowing Rule F's authority for a retention decision is how that starts.

What the fold does to each artefact, and what it costs, is §6.2. **Nothing there is a third
retirement rule.** Every derived structure is rebuilt from the column at the fold, so derivation
self-retires, and Rules S and F remain the whole of the removal model.

### 6.1 Why a filter result is intersected against the composed verdict

An obvious argument runs: the filter operand is intersected with `M_auth` before anything is counted
(§8.2's *the mask goes in first, not last*), so a stale value for a removed entity is harmless. **That
argument is wrong, and the design depends on its being wrong.**

It fails twice. A **suppression is never folded at all** — Rule S is the whole of it — so the mask
fragment always contains a suppressed entity; only the overlay hides it. And the entity-space verbs are
not protected by `M_auth` in the first place: vocabulary visibility evaluates against the **composed
verdict**, which is what applies deny precedence (per-point-attributes §3.3).

So the rule is that a filter result — and every count over one — is composed with the deny state on the
same path every other answer is, never intersected with a raw fragment. Surface §5 states where that
happens. What the fold buys is therefore not correctness against suppression, since suppressions do not
fold, but space and the retention property above.

Vocabulary visibility is **derived per request and never maintained**, which keeps this to two retirement
rules. A maintained union of members' term signatures would be monotone under ingest and non-monotone
under deletion, needing a third rule beside S and F — one of which is itself unbuilt. Derivation
self-retires.

### 6.2 The fold's attribute pass

**The objective is the owner's, verbatim: after a fold, a bundle should cost what a freshly built
one costs — to open, to hold resident, and to query — without disrupting serving to get there.**
For this artefact both halves are now quantified, and with §5.2 bounding the file-count axis
continuously the fold's share is smaller than a day's pile-up: the layered column's *query* cost is
within ~5% of a single build's at a day of layers and the coalesce holds the layer count at tens
(measured and modelled, §5.1–§5.2), so what only the fold can do is **retention** — the blanking
below — the **postings rebuild** to the new watermark, and the final collapse of the surviving
handful of layers into one base, which is what makes the folded bundle *equal* to a built one
rather than close to it. The fold is no longer the only thing standing between the open path and a
day's ~31,000 files; missing a window costs a bounded steady state, not unbounded growth. The
non-disruption half is the pass's own cost, sized at the end of this section.

⊘ **Built (2026-08-10), bar one hint.** The pass runs where this section places it — on the fold's
own thread, after the external-id pass and before the digests — and everything below is what it
does, with one exception marked at its own paragraph: the `MADV_SEQUENTIAL` asked for there is
**not** taken, because `ValueColumn::open` has no route to it. What the pass inherits from
[decision 0052](../decisions/0052-the-folds-page-cache-mitigation-is-a-hint-not-a-throttle.md) is
the ownership rule — it maps its own inputs and never advises the request path's — rather than the
hint itself.

**The pass is a crate rather than a module, and the reason is audit separation — not the
measurement that originally prompted it.** Written inside `tessera-filter` it cost the scan's
universal-contiguous arm **0.27 → 0.44 ns** per candidate entity at 10⁹, with the hot file
byte-identical. That figure is real and its explanation was wrong: the cause is §2.2's
instruction-address alignment, so the crate boundary did not remove the sensitivity but re-rolled
the layout, and the "within drift" reading the split earned was luck rather than structure. A crate
holding only the scan would be the same gamble. The split stands on its own merits — the write side
has no business in the read path's crate, and §9 records the edges that placement implies — and
alignment pinning is what actually addresses the constant.

One pass on the fold's dedicated thread, before the manifests, mirroring compaction §2's
snapshot/publication split:

| | At the snapshot | At publication |
|---|---|---|
| **base value column + presence** | folded: one new base per column, snapshot extents in, `D₀`'s entities out | — |
| **extents, per-flush and coalesced (§5.2)** | consumed by the fold | post-snapshot extents carried forward, listed in the new `attr_extents` |
| **category postings** | rebuilt whole from the folded column | — |
| `suppressed` | **untouched — no attribute artefact ever changes for a suppression** (Rule S) | the live set, exactly as compaction §2 already publishes it |

**Per column, the fold streams base + snapshot extents in entity order and emits one new base.**
The layers partition entity space and each is entity-ascending, and a flush's entities sit above
every earlier layer's (**I9**), so the concatenation is a linear merge with no sort. An entity in
`D₀` — the plan's tombstone clone, never `executed`, exactly as compaction's passes 1 and 3 take it
— is skipped. The new presence bitmap is `(∪ layers' present) ∖ D₀`, omitted only when every
entity from 0 to the snapshot bound is present — the reader's dense-from-zero convention, "the
entity id is the array index" (§2.1, §2.5), and nothing looser; **blanking a deleted entity's slot means removing it
from presence and emitting no value bytes**, not overwriting them — a sentinel would keep the bytes
the retention argument exists to remove, and every family would need a reserved value it does not
have. A previously universal column therefore becomes partial at its first folded deletion, which
moves it from the bare-array constants to the partial ones (§2.2) — both measured, both inside
budget, and the partial path was faster in every broad cell measured (probe arm 4). The merge is
specified for the shipped families — fixed-width and text, one value per present entity — and
**not for lists**, whose storage §2.6 leaves unaddressed and whose parse refusal stands meanwhile.

**Every snapshot extent folds in; carrying an untouched one forward is declined.** An earlier
revision offered it as an implementation option. It trades the objective — zero extents after a
fold — for IO the fold can afford under decision 0056's window, and it makes the folded state a
function of deletion history rather than of the schema. The extents that *are* carried forward are
the post-snapshot ones, published during the fold's flight: hard-linked, digests carried, listed in
the new `SEGMENTS-<n>.json`'s `attr_extents`, and composed onto the new base at the flip exactly as
a flush composes them — their entities sit above the snapshot bound, so the disjointness check
holds unchanged.

**A category's postings are rebuilt whole from the folded column** — the self-retiring derivation
§2.3 requires, and after the rebuild they cover the new `fold_watermark`, closing the un-folded
tail the flush marker in §5 records. Where the rebuilt postings serve a *filter* — a `public`
listing's route under decision 0061 — this rests on the leak-register row 0060 names as a
condition of itself, **which does not exist yet in Appendix C and is being registered on the
postings track**; until that row lands, 0061's condition is unmet and this paragraph inherits the
dependency. The rebuild reuses the build's emit — one banded emit over a
`(entity, code)` source each producer supplies, the build's from its staged values and the fold's
from the column it has just written, so the postings are a derivative of the artefact of record
rather than a second opinion about it. That sharing is the equivalence argument in general: the
pass emits through the same value-column writer the build uses, so a folded column and a freshly
built one over the same live entities are the same bytes, and "as close as possible to the single
build" is byte-identity rather than a tolerance — checked as one, at every layering and every band
size. **The presence bitmap is normalised at the writer** for that reason and not for storage: the
fold's presence arrives as a union of its layers' and the build's from repeated insertion, and
croaring serialises the two encodings differently, so the same entity set would otherwise produce
two different files.

**The timing property survives untouched.** The folded artefact is the same shape the scan already
reads — a value column with optional presence — so per-request work remains a function of
`(candidate, column)` and never of the value sought (§2.2). The pass adds no per-request structure,
and presence is membership, not values, so its post-fold shape discloses nothing a candidate does
not already encode.

**At the flip, the filter columns are opened from the new prefix — never cloned from the live
generation.** A clone would serve pre-fold values, missing the blanking and the folded extents,
from files the reclamation is about to unlink (safe to hold on POSIX, wrong to serve). So the
filter columns join the rotation the way the postings reader and the external-id sidecar already
did, for decision 0050's reason: the swap carries a `FilterColumns::open` over the new prefix — the
folded bases plus the carried-forward flight extents — and the old mappings die with their last
holder. **The symptom of getting this wrong is not a wrong answer**, which is why the test for it
reads the process's own mappings: a folded entity is outside every candidate, so the stale values a
cloned column holds are unreachable, and what a clone actually costs is the fold's reason for
existing — the superseded prefix stays mapped, so the reclamation unlinks names and frees nothing.

**Slice invariance holds through the fold**: the pass is per partition in entity space, reads
nothing per-slice, and emits nothing per-slice. §7's statement is unchanged by it.

**Deliberately not designed here.** A resumable attribute pass — the fold has no resume anywhere,
deliberately (compaction §3), and this pass inherits that. And any change to *when* folds run:
decision 0056's schedule is taken as given, and nothing here adds a trigger. (An earlier revision
also declined the extent coalesce here, on the query axis — the wrong axis, since the binding cost
is files and open time; it is now designed at §5.2.)

#### What the pass costs, against non-disruption

Per `u32` column at 10⁹: ~4 GB read, ~4 GB written, and ~4 GB re-read for the digest — the fold
digests by reading back (compaction §3, pass 5) — so **~12 GB of streaming IO per column:
*modelled*, seconds per column at raw NVMe bandwidth, minutes only under the fold's own gentled,
interleaved regime, which is the regime it runs in; ~16 columns adds minutes to tens of minutes to
an operation already minutes-to-hours**. That lands inside the constraint that matters: P3
measured a corpus-scale streaming read costing a concurrent viewport up to 2.03×, and P4 measured
a *real* fold at 1.05–1.18× because it interleaves passes and computation. That the attribute pass
behaves like P4's fold rather than P3's reader is **reasoning from its shape, not a
measurement** — it interleaves reads, writes and encoding the same way — and is marked as such.
It extends the fold's duration — free under the owner's slower-is-gentler ruling
(compaction §6.1) — rather than its intensity. The pass opens its **own** mappings and takes
`MADV_SEQUENTIAL` on them, per decision 0052's rule that the hint belongs to mappings the fold
owns; it must not advise the live generation's `FilterColumns` maps, which are the request path's,
for exactly pass 2's reason. The pass is single-threaded like the rest of the fold; parallelism is
excluded by owner ruling and not further discussed.

**Memory: banding bounds the transient only because the writers stream — ✔ and they do.** The
writers this design called for are built (2026-08-10): `ValueColumnWriter` spools value bytes, and
a text column's offsets, and assembles the single record batch at `finish` with the spool mapped as
the array's own buffer; `KeyedPostingsSpool` appends encoded records band by band and assembles the
same way, its strictly-ascending-key check comparing across band boundaries and not only within
one. Both are on the repo's own spool-then-assemble discipline, both are byte-identical to the
whole-column writers they replace at every chunking tested, and the batch build is ported onto them
— so there is one writer per artefact rather than two producers that happen to agree.

That mattered more than an optimisation: it is what the earlier revision of this paragraph had
wrong. `write_value_column` took a fully materialised `Codes` and the keyed writer its complete
entries slice, and the format's reader refuses a second record batch, so incremental append was no
escape — banding the *ids* against unbanded writers would have moved the resident gigabytes from
the ids to the values array or the serialised postings rather than removing them. Measured at
2×10⁸ over a fully covered `u32` category, heap peak: the value column 800 MB → **1 MB**, the
postings 832 MB → **269 MB** at three bands. With those in place the emit **bands the code space**: a code never split
across a band, a counting pass over the column sizing each band from a memory budget, then per
band one column scan cursor-scattering into a flat buffer laid out by prefix sums and appended
through the spool — bands partition ascending code space, so the writer's ascending-order check
holds across them unchanged. The in-flight cost the banding bounds is the **raw entity ids, 4 B
per present entity, ~4 GB per fully covered category column at 10⁹** (an earlier revision quoted
~2 GB from probe arm 9 — the *serialised* size; the transient is the ids). The cost is one column
scan per band at §2.2's measured ~280 ms per 10⁹ — seconds per column even at sixteen bands — and
the memory is the band budget, **a constant the planner chooses**, which is what dissolves the
pre-flight ruling an earlier revision put to the owner (§6.3). The counting pass's residue is one
count per distinct code, vocabulary-sized by definition (§2.3) — kilobytes, not a term. **The
build owes itself the same fix**: its shipped emit is the unbanded shape (§4's finding), so the
banded emit and both writers are written once and both producers call them, which is also what
keeps them one writer rather than two that agree.

**No new gauge.** The fold's free-space precondition already covers `attrs/` — its estimate is the
bytes the manifests name, which these files are — and the extent axis needs no trigger of its own
twice over: §5.2's coalesce bounds it continuously on its own policy, and what escapes the policy
moves one-for-one with the segment axis compaction §9 already gauges, at microseconds per layer
where a segment costs a binary search per tile (measured, §5.1). What the pass owes instead is
*visibility*: attribute bytes read and written in the fold's dispatch log line and
`/control/status`'s fold block, beside the figures already there. (An earlier revision of this
section promised the *gauges* attribute-bytes terms; that was a trigger where only reporting is
warranted, and it is withdrawn.) ⊘ **The bytes are not reported.** What an operator sees today is
the pass itself in the fold's cost staircase — its wall clock and the resident set it ended at,
under the name `4a attributes`, in the same dispatch log line as every other pass — which
attributes time and memory to it but not IO.

#### Start-up

Opening a **folded** bundle costs what opening a built one costs, which is the objective: one
`values.arrow` map per declared column, presence and postings where they exist, plus any
carried-forward flight extents — and the digest sweep at O(bytes), dominated by the columns
themselves (4 GB per `u32` column at 10⁹), whose first-touch deferral is §8's owed contracts
amendment and not re-argued here. The fail-closed rules are already built and stay: a declared
column whose files are missing refuses to open, an extent named but absent or short refuses, a
digest mismatch refuses — never "those entities carry no value", the wrong answer in a right
answer's clothes (§2.5). A fold changes which files those rules bind, never the rules.

**Publishing an attribute artefact is two obligations, and doing one is worse than doing neither.**
A column's layers are composed at open from the manifest's `attr_extents` list, so the files and the
list are independent halves: hard-linking or writing the bytes while leaving the list empty produces
a bundle that opens cleanly and **silently answers filters without every post-build entity's value**
— a wrong answer with no symptom, strictly worse than a refusal to open. `execute_compaction` writes
an empty `attr_extents` today, which is why the gap above is currently loud rather than silent. The
pass must publish both halves in the same manifest write, and a test that a folded bundle still
answers over post-build entities is what keeps that true.

### 6.3 The fold's rulings, and what they settled

**All three are made; nothing here is open.** Two never reached a ruling and one was answered: the **interim carry-forward is withdrawn** (owner, 2026-08-09 — nothing is deployed, so
there is no folded bundle to rescue and a state whose only justification is the pass's absence is
decision 0048's forbidden shape; the gap stays loud until the pass closes it), and the
**postings-memory ruling dissolved** rather than being ruled — banding the emit (§6.2) turns the
corpus-dependent gigabyte term into a planner-chosen band budget, so there is no longer a choice
between an explicit formula and consumed headroom. No corpus-proportional residual survives the
banding: the counting pass holds one count per distinct code, which is vocabulary-sized. What
remains for compaction §3 is recording the band budget as a stated constant in the pre-flight —
an owed amendment, not a ruling.

**The attribute axis is reported, never triggered on** (owner, 2026-08-10). Compaction §9's
automatic trigger stays an OR over its four gauges — segment count, retirable overlay depth, the
tombstoned-row fraction and the dead-byte ratio — and **gains no fifth for attribute work**. An
earlier revision of this document promised those gauges attribute-bytes terms; that promise is
withdrawn rather than deferred.

The argument the ruling accepts: §5.2's coalesce bounds the extent axis continuously, so extents no
longer wait on a fold at all; what escapes the coalesce moves one-for-one with the segment axis
already gauged, so a fifth gauge would fire when the first does and cover nothing the first misses —
and §9's gauges are an OR *because the obligations are independent*, which a correlated axis is not;
and a layer costs microseconds to query (measured, §5.1), so there is no latency pressure to detect.

What the pass owes instead is **visibility**: attribute bytes read and written in the fold's dispatch
log line and `/control/status`'s fold block, beside the figures already there. An operator can see
the work; nothing dispatches a fold on it. ⊘ Only the pass's *time and memory* are reported so far,
through the cost staircase it joins as `4a attributes`; the byte terms are owed (§6.2). Cost if this is wrong: an axis nobody triggers on, bounded
regardless by the coalesce policy and the segment ceiling at 64.

Amendments this design owes elsewhere, none of which it makes itself: compaction §2's table and §3's
pass list gain the attribute pass and the band budget (normative — its own review); write-path §7
and contracts §2.1's tree gain the coalesce's fourth axis and `coalesced/<id>/attrs/<column>/`; and
§8's first-touch digest deferral remains contracts §2.4's owed amendment.


## 7. Slices

**The filter index is slice-invariant.** Nothing about it is per-slice, and a slice attaching, being
populated or being dropped touches none of it.

The governing statement is architecture §9: the term index, node memberships and generating sets are
shared across slices in entity space (**I4**), while each slice stores its own permutation and derives its
own tile ranges. per-point-attributes §3.9 draws the same line for attributes — `render` is row-space and
therefore per-slice; `filter` and `inspect` are entity-space, declared once, and apply everywhere.
`slices-and-multi-table.md` reaches the same conclusion in more detail, but it is provisional and
explicitly not approved, so it corroborates this section rather than grounding it.

- **One entity ID globally**, never one per `(slice, entity)`. An entity appearing in several slices has one
  set of attribute postings and one membership in every value it carries.
- **Attach and drop are row-space operations.** Populating a new slice builds that slice's Morton order and
  permutation; dropping one tombstones a name and leaves its row-space artefacts to the next compaction.
  Neither deletes an entity, and neither reads or writes anything under `attrs/`.
- **The per-slice cost is the projection, not the index** — surface §4's subject.

With tier paths slice-independent (index §5.3), there is no per-slice case anywhere in this document — and
the two removal rules gain none either, because deletions and suppressions are entity-space mechanisms
that do not know slices exist.

---

## 8. Residency, and the startup path

Attribute filters are read once per **query** — not once per rendered mark, and not once per session.
§10.3 (r21) names that cadence as one of the three the routing rule is built on, and §10.5 orders
structures by cadence rather than by size.

**The value columns are mapped, not read, and that is what keeps the cadence affordable.** A
generation opens every *declared* filter column at once and holds them for the process lifetime, so
reading them would make residency a function of what the schema declares rather than of what anyone
filters on: at 10⁹ with sixteen `u32` columns, 64 GB resident before a single filter arrives.
Measured over eight 10⁸-entity `u32` columns — 3.2 GB of values (probe arm 5):

| | Open | Resident after open | After one 1% scan |
|---|---|---|---|
| read into memory | 2,196 ms | 3,301 MB | 3,301 MB |
| **mapped** | **0.2 ms** | **2 MB** | 6 MB |

The scan is unaffected — 0.24–0.27 ms either way — so the mapping costs nothing once the pages are
resident and resides only what a request touches. Those scan figures are warm-page-cache and are not
a cold-start claim; what the comparison establishes is that the read path pays its I/O for every
declared column while the mapped path pays it for the columns actually scanned.

> **⊘ A text column is resident at open where a fixed-width one is not — but it is *clean page
> cache*, and the distinction is the whole finding.** Measured per column at 2.5×10⁷ on real data
> (`probes/2026-08-10-filter-lifecycle/`), resident bytes at open: a 203 MB `i64` column **98 KB**,
> three category columns 111–143 KB, and a 365 MB `utf8` column **362 MB** — the whole file. That
> looks like the mapping failing for text, and it is not. Splitting the figure (arm 15,
> `textresident`, a 475 MB column at 2×10⁷): **`RssAnon` delta 0, `RssFile` delta 472 MB.** Nothing
> is copied to the heap; the mapping does exactly what §8 claims. What happens is that every page is
> *touched* — the reader decodes a `LargeStringArray` and Arrow validates UTF-8 across the values
> buffer — so the pages become resident, and being clean and file-backed they are evictable under
> pressure, which is the property mapping exists to give.
>
> So the residual cost is **open time and page-cache pressure, not memory that cannot be
> reclaimed**: ~40 ms per 475 MB column, which extrapolates to ~1.6 s for a 19 GB text column at
> 10⁹ (*modelled*, and the extrapolations in this campaign have a poor record). Fixed-width columns
> are untouched by any of this and behave exactly as the table says.
>
> **Do not "fix" this with `skip_validation` alone.** Nothing here depends on Arrow's UTF-8
> guarantee — `text_at` converts with a checked `from_utf8` and the byte predicates compare bytes —
> so that half is redundant, and no unvalidated string can reach a bundle in the first place (every
> route in is a Rust `String` or a validated Arrow array). But the same switch drops the **offset**
> validation, and a corrupt offset pair would then reach `&bytes[lo..hi]` and panic on a request
> path where today it refuses at open. It would also buy less than it appears to: `verify_files`
> hashes every named file in full at open regardless, so Arrow's pass is the *second* read of bytes
> already in cache. The change with the real win is §8's owed first-touch digest deferral, which
> removes the first read — and that is an owner ruling, because it narrows a deliberate fail-closed
> rule. **Note the asymmetry it turns on**: a corrupt *value column* can only narrow `M_sel`, since
> the scan runs inside the candidate and **I12** holds structurally — but a corrupt *posting* now
> feeds `/v1/categories`' `per_viewer` visibility predicate (decision 0061), which is a disclosure
> control. Deferral is arguable for value columns and is not obviously safe for postings.
>
> **A category's postings are read, not mapped, and are therefore fully resident** — 6–12 MB per
> column here, and by decision 0061 they are now on the serving path for a `public` listing. That is
> correct as designed and is not covered by the table either.

- **Category membership at 0.31–1.01× the render column it indexes** (*measured* at 2.4×10⁶ items; the 10⁹
  figure is *modelled*, and index §4.1 says why the ratio may not hold).
- **The value column is the resident term, and it is the working set rather than the column** — the
  column is 1 GB per `u8` at 10⁹ and 4 GB per `u32`, and mapping means what resides is the part a scan
  touches. There is no dictionary beside it to hold resident. An earlier revision priced an
  FST dictionary here (*measured* 0.78 GB against 7.09 GB for the equivalent hash map at 1.17×10⁸ keys);
  that structure is cut, and the figure is retained only in [`dict-fst`](../../probes/2026-08-03-dict-fst/)
  where it still governs the authorisation dictionary.
- **A category's derived postings are small beside the column they accelerate** — *measured* at 10⁹:
  8 B–54 KB correlated, 2–125 MB scattered, against a 1 GB `u8` column.
- **The scan is sequential over the candidate's range**, so a column that is not resident is paged in at
  the access pattern page-cache handles best. That is *reasoning, not measurement*: every constant in
  §2.2 was measured in RAM, and no cold-scan arm — one that drops the page cache first — has run. Arm
  5 shows only that a *warm* mapped scan costs what a resident one costs.

**Open-time cost is dominated by the digest sweep, not by record validation, and the design must not claim
otherwise.** Bundle open reads every file the manifest names **in full** and hashes it — deliberately, and
chunked so it costs no memory — because a bundle whose bytes were not checked is a bundle whose
authorisation data was not checked. So digest-gating the postings reader's per-record round-trip halves a
term rather than removing one, and every artefact this design adds joins the sweep at O(bytes): 4–8 GB per
filterable column, which is the column's own size. Nothing here is small enough to ignore once `attrs/`
exists.

The corpus already holds the pattern: contracts deviation 9 defers the external-ID sidecar's digests to
first touch, precisely because digesting it at open would reimpose the sequential read the deviation exists
to remove. **`attrs/` takes the same deferral**, per file, with the digest checked on first touch before
any borrowed view is constructed. Record-level validation stays on that first-touch path, which is what
actually guards the unsafe zero-copy view; the build and each fold validate in full, where the cost is
already being paid.

**This changes the shared reader's contract, and that amendment is owed rather than assumed here.** The
open-time validation the authorisation reader performs is named in its own safety argument, so relocating
it is a change to contracts §2.4 and to that module's discharge of an `unsafe` block — not something a
provisional filter design settles on its own.

---

## 9. Placement and outputs

**The conformance relation is the value column itself.** The mask differential works because an
authorisation term ID is opaque: `M_auth` is a union over term IDs, so a flat `(entity_id, term_id)`
relation lets the oracle derive the same set by direct scan. A filter's definition is a predicate over
**values** — `age ∈ [30, 40]` cannot be evaluated from an ordinal — so the oracle needs
`(entity_id, column, value)` in the column's declared type.

An inverted-postings design could not supply that from its own artefact, so an earlier revision specified
a *separate* relation, optional for serving and required for a conformance run, re-emitted by the fold as
a side output. **The flat column removes that entirely**: it already is the relation, so the oracle reads
the artefact under test rather than a parallel emission, and the differential stops depending on a
re-emission the fold could forget. This is one of the strongest arguments for the flat shape and belongs
in any reconsideration of it.

**The filter machinery is its own crate, `tessera-filter`** (owner ruling, 2026-08-08). Both indexes are
entity-space and neither mentions row IDs, so a module inside `tessera-authz` would have been cheaper —
but that crate owns `M_auth` and **I3**, while filter machinery may only ever narrow `M_sel` under **I12**,
and it is the crate an assurer opens first. Keeping its public surface authorisation-only is worth one
crate and one dependency edge.

The layer script gains what that placement implies, since none of it is automatic: `deny tessera-server
tessera-filter`, so the server keeps seeing engine API types only; and `deny tessera-filter tessera-store`
plus `deny tessera-filter tessera-spatial`, so the filter crate stays entity-space and never sees `RowId` —
the same two edges the authorisation crate is denied, for the same reason.

**The artefact's write side is a second crate, `tessera-filter-write`** — the fold's merge and the
banded postings emit both producers call — and its reason is the measurement §6.2 records rather
than symmetry: code that never runs during a scan still moved the scan's constant by 65% from
inside `tessera-filter`, because codegen units are partitioned per crate. It takes the same three
denies for the same reasons, plus `deny tessera-filter tessera-filter-write`, which is what keeps
the dependency one-way and the read crate's codegen a function of its own source.

---

## Appendix R — review trail

**2026-08-10 (r7) — adversarial review, two lenses, dispositioned in one pass.** Both lenses held
the shape and attacked load-bearing claims; every finding was accepted except one whose *remedy*
was declined for a cheaper one. What changed: **selection is per column** over that column's own
`attr_extents` subsequence — which dissolves the missing selection unit (the reviewed draft
selected "a window of flushes", an identity the format does not record; the suggested group-key
format change is declined because the column, which the format does record, is the better unit and
also survives the second rung), confines cap starvation to the offending column with a
narrow-to-`width ≥ 2` fallback, and gives the size tier a well-defined base. **The merge carries
its own overlap refusal** — after coalescing, an input overlap is internal to one layer and
invisible to `compose` forever, so the union-equals-sum-of-cardinalities guard lives in
`coalesce_attr_extents`, as the dictionary axis's guard lives in its merge; composition gains the
**replace** operation `compose` cannot express, refusing unless the coalesced presence *equals*
the consumed union. **Publication order is fixed to the flush's** — compose, manifest, swap, with
the completed unit carrying opened columns. **"The writers spool" was false** and the two
spool-then-assemble writers (value column, keyed postings) are now explicit deliverables — banding
bounds nothing without them. **The flip must open filter columns from the new prefix**, joining
the rotation as postings and the sidecar did. And three properties are now recorded at their
sites: **positivity** — every "degrades safely under I12" argument holds only while every operand
is positive, so lifting `none_of`'s fence must revisit §5.2/§6.2 under the inverted sign (§5, the
review's most valuable finding); **lists break one-bit-one-slot addressing** and are excluded from
the merge and blanking specifications until §2.6's addressing exists; and §2.3's "unmeasured"
marker was stale — arm 9 measured the residual and 0060 is built on it. §6.2 also now names its
dependency on 0061's leak-register row, which is registered on the postings track, not here.

**2026-08-10 — the fold's attribute pass is built**, to §6.2 as written. Two things the design did
not anticipate, both recorded at their sites. The pass had to become its own **crate**: written
inside `tessera-filter` it cost the scan 65% with the hot file byte-identical, which is the same
code-shape hazard that split `extent.rs` out of `values.rs`, one level up — codegen units are
partitioned per crate, so a file boundary cannot hold it (§6.2, §9). And **the presence bitmap
needed normalising at the writer** for the byte-identity claim to be true at all: the fold's
presence arrives as a union of its layers' and the build's from repeated insertion, and croaring
serialises the two encodings differently, so the same entity set produced two different files
(§6.2). Two things §6.2 asks for are **not** built and are marked: `MADV_SEQUENTIAL` on the pass's
own mappings, and the attribute-byte terms in the fold's log line and `/control/status`.

**2026-08-10 — the fold's rulings are closed** (owner). The attribute axis is **reported, never
triggered on**: compaction §9's OR over four gauges gains no fifth, and this document's earlier
promise of attribute-bytes terms on them is withdrawn rather than deferred — a correlated axis
cannot earn a place among gauges whose independence is the reason they are an OR. With the
carry-forward withdrawn under 0048 and the postings-memory question dissolved by banding, §6.3 has
no open question; what stands between this design and normative is §2's constants at a second value
width and on a string column, surface §4's project-vs-per-tile rule, and one adversarial round.

**2026-08-10 (r6) — two owner corrections, both of which change what gets built.** First, the
extent coalesce was declined at r5 on the query axis — the wrong axis, since arm 13's own numbers
put the binding cost in files and open time — and is now designed as the **fourth axis of the
existing entity-space coalesce** (§5.2): same policy, same `coalesced/<id>/` precedent, same
content-preserving and retire-nothing rules, with the fold re-derived to hold retention, the
postings rebuild and a final collapse of tens of layers rather than a day's ~960. Second, the
postings-memory figure was wrong twice — ~2 GB is the *serialised* size where the in-flight
transient is the raw ids at ~4 GB, and it is not the fold's problem alone: the **build's shipped
emit is unbanded today** (§4's finding). The emit is now specified banded by code space on the
authorisation build's own construction (§6.2), shared by both producers, which dissolves r5's
pre-flight ruling: the term becomes a planner-chosen band budget with no corpus-proportional
residual. §6.3 is down to one ruling — no attribute gauge.

**2026-08-09 (r5) — the fold and start-up designed, on a new measurement.** §5.1 and §6.2 are new
and §6.3 lists what the owner must rule; probe arm 13 measured layer accumulation — ~9 µs per layer
net on the worst operand shape, +15 ms at a day of 90 s flushes, so the fold's pressure is the
open-path file count rather than the scan. Three earlier statements are corrected at their sites:
the option of carrying an untouched extent through a fold is withdrawn (it defeats the
single-build-equivalence objective for IO the fold can afford); "the fold gauges gain
attribute-bytes terms" is narrowed to reporting, since the extent axis moves one-for-one with the
gauged segment axis; and "deletion and the fold touch no attribute artefact" is replaced by the
truth §6's marker already carried — the fold destroys the artefact, and §6.2 specifies the pass that
closes it. An interim carry-forward was drafted into §6.2 and §6.3 and **withdrawn by the owner the
same day**: with nothing deployed there is no folded bundle to rescue, so a state whose only
justification is the pass's absence is the shape decision 0048 forbids.

**2026-08-09 — the flush's half of the write side landed, and one measurement is worth carrying.**
§5's extent is built and §1's and §5's markers move with it; §2.5 gains the extent's presence file,
which is mandatory where a base column's is optional, and §6 records that a fold destroys the
artefact today rather than merely failing to rebuild it. The measurement: adding the three extent
functions to the *same file* as the scan cost the universal-contiguous arm 0.27 → 0.46 ns per
candidate entity at 10⁹ — code that never runs during a scan, in a file whose module doc already
records three such regressions — and moving them to their own module restored 0.26 ns exactly.
Anything that adds to `values.rs` must re-run `probes/2026-08-08-filter-layout/`'s `realscan`.

**2026-08-08 (r4) — the organising rule changed, on an owner ruling and the first measurements.**
r1–r3 specified an **inverted posting per distinct value for every family**, a shape imported from the
authorisation term index without an argument that it transferred. `M_auth` needs it — a union over ~10⁴
term postings, materialised once per session and reused by every request in it. A filter operand is
per-request over a narrow predicate and can take `M_auth` as a candidate set, and nothing established the
two were alike.

The owner ruled that **postings are specifically a categorical instance**: a category's value already has
an integer identity and repeats heavily, and no other family has either property. Other types belong in a
flat table or in an index suited to that type.

Two probes then settled what reasoning had been guessing at
([`2026-08-08-filter-layout`](../../probes/2026-08-08-filter-layout/)). The masked scan is **~0.24 ns per
candidate entity** contiguous and **~10 ns** scattered, stable across 10⁶–10⁹ — which refuted a 5–10 GB/s
bandwidth model that had predicted 40–80 ms for a 25% principal at 10⁹ against a measured 730 ms, itself
since improved 13–272× by run-based iteration and typed traversal (arm 4). The
addressing choice was measured rather than argued: bare array where presence is universal, Roaring
presence bitmap where partial, explicit `(entity_id, value)` pairs never optimal on either axis, and run
tables a trap that is smallest on disk and collapses at 2.9–10.2 s on a broad candidate. And the derived
category posting closes the broad-coverage corner at **107×**, with **no measurable timing channel**
between a hidden value and a nonexistent one — which is what allows §2.2 to withdraw filter-surface §2.1's
supersession of per-point-attributes §3.8 and restore the original "indistinguishable in work" requirement.

What that deleted from the design: the value dictionaries and their promotion path, the extension-id
resolver, per-flush delta tiers, `max_distinct_values`, the near-unique-string quadratic hazard, the level
tree, range-encoded bit slicing, the order-preserving float key, and the separate conformance relation.
What it added: one presence bitmap, and one open corner (§3's broad numeric ranges) that needs an owner
ruling rather than a measurement.

Three corrections carried in from review of the analysis that produced this revision, recorded because
each was wrong in a way that would have reached the design. Decision **0050 does not justify the fold
blanking a deleted entity's filter slot** — its argument is a fail-open in `M_auth` and does not transfer;
the retention asymmetry does, and §6 now says so. The **candidate-driven container probe** was proposed as
"value-independent by construction" and is not — it equalises the probe count only — and measurement then
showed it unnecessary, since the plain intersection is already flat. And the **21.7 ms / 2,885 ms union
spread must not be cited against a filter operand**: it measures a union over ~10⁴ authorisation postings,
not a single-value intersection, and §4.1 now says so at the site where it was previously quoted as a risk.

**2026-08-08 (r3) — a simplification found while implementing r2.** §2.4 had a category's postings
addressed by a `posting_ordinal` minted beside its code, to fit the positional CSR record format. Writing
it showed the cost: a second durable quantity seeded from every home the code is seeded from — manifest,
segment extensions, WAL — where `vocabulary.rs` records that missing one of those homes is the module's
characteristic failure, and a duplicated ordinal is two values sharing a posting slot, which is the C11
disclosure §2.4 invokes decision 0042 to prevent. The mechanism added to avoid a hazard reintroduced it.

§2.5 removes it by choosing the record format to fit the domain instead: **scattered identifiers use the
keyed record format**, which contracts §2.4 already defines and every delta tier already uses, so a
category is addressed by its code and nothing is derived alongside it. Positional CSR stays where
identifiers genuinely are dense. This is more faithful to the two-route ruling than r2 was, not a
departure from it — a category's identity comes from the vocabulary table, now with nothing beside it —
and it lands where per-point-attributes §6 already ruled: *the code is the identifier*.

A limited review of that change found it sound and found the edit **incompletely applied**: §2.4 still
carried r2's prescriptive paragraph mandating the ordinal, §5.2 still said a category had one, and §6
stated the positional sweep rule blanket across both formats — which for a keyed column would have meant
sweeping a 4×10⁹-code domain, the size §2.5 exists to refuse. All three are corrected, and §6 now states
the keyed fold's shape and its drop-on-empty rule rather than leaving them to be discovered. Two negative
results from the same review, recorded so they are not re-derived: nothing outside this document
referenced `posting_ordinal`, so there was no consumer to unwind; and reusing `coalesce_delta_tiers` for a
keyed base would fit the signature while voiding the whole-file-read justification its own doc gives.

**2026-08-08 (r2) — reviewed under three lenses, and the artefact's address space failed.**

All three lenses independently found the same defect, which is the strongest signal this process
produces. r1 addressed the postings file as `base + local` over per-column extents in one positional file;
under ingest a value minted after the build takes an ordinal belonging to the next column, `base ∪ tiers`
unions one value's members into another's, and §6's never-renumber rule forbids the repair — a C11
disclosure reachable by ordinary operation. §2.2 replaces it with **one postings file per column**, so an
`AttrTermId` is `(column, local)` and no arithmetic relates the two routes. Per-column extents are gone;
the families that appeared to justify them do not need them.

Three more that changed a mechanism. The r1 claim that the two indexes have distinct newtypes *and* share
a reader was self-contradictory, since the shared reader is typed in one of them — §2.1 makes the format
core raw-`u32` with typed wrappers per crate. The extension-ordinal guarantee was inherited from the
authorisation side without its enforcement: that side rests on a *checked* plugin bound, and attributes have
none, so §5.1 declares `max_distinct_values` and refuses at promotion. And §8's "digest-gate at open" did
not do what it claimed — bundle open reads and hashes every named file regardless — so §8 takes contracts
deviation 9's first-touch deferral instead, and records that relocating the reader's validation is an owed
amendment rather than this document's to make.

Four that changed a number or a rule. **§3.4's level-tree sizing was the lucky case**: the upper levels are
~1.25 GB modelled when the attribute correlates with the label set and ~20–30 GB when it does not, so the
structure choice reads a contiguity statistic and not cardinality alone — the same spread §4.1 already
quoted for membership sets and failed to apply here. **Every fold rebuilds the accelerator**, not only a
fold that changes a structure, since deltas carry level 0 only (§6.2). **The un-folded delta range path**
was a sentence and is now a mechanism, with the tier carrying keys so a range scans records rather than
probing ordinals, and the FST usable for rank only because §3.3 now fixes the key big-endian. And **§4's
build pass** is its own stage pair rather than a rider on the authorisation write, with the bit-sliced pair
volume, the 4 GB scatter floor and the streaming encoder stated, because the r1 claim that it was "bounded
by the same arithmetic" was unfalsifiable without them.

Two findings were rejected on verification and are recorded so they are not re-raised. A row-space cache key
**may** carry the prefix alongside `segments_version`; §10.2 forbids keying on the prefix *instead of* the
version, which is a different thing. And the shared-format-crate extraction is **not** forced by §3.5.

**2026-08-08 (r1) — drafted**, after a three-lens review of the plan it was written from. The findings that
shaped it: level-tree nodes are rank-keyed in the reference implementation, so interning them breaks
decision 0042 (§3.1); the argument that `∧ M_auth` makes stale postings harmless is false, because
suppressions never fold and the entity-space verbs use the composed verdict (§6.1); floats have no total
order under raw IEEE bits, so range answers over a signed float column would be wrong rather than slow
(§3.3); and a category's posting ordinal comes from the vocabulary table rather than a second interner
(§2.4). Owner corrections at the same time: **strings are not categories** — no value set, no listing, no
autocomplete, and the dictionary's interning is a posting key rather than a vocabulary (§2.3).

[#44]: https://github.com/jennis0/tessera-index/issues/44
