# The filter index — design

**Date:** 2026-08-08
**Status:** **Provisional — r4, rewritten on measurement and owner rulings.** The category case is
built to this design; see the ⊘ notes for exactly what. The organising rule
changed: the flat value column is the artefact of record and every accelerator is derived from it
(Appendix R). To become normative: confirmation of §2's constants at a value
width other than `u32` and on a string column, and a ruling on surface §4's measured
project-vs-per-tile rule. Measured input:
[`../../probes/2026-08-08-filter-layout/`](../../probes/2026-08-08-filter-layout/).
**Built so far:** the crate and its keyed reader, the `filter` placement in the schema and manifest,
and the batch build's derived-posting emit for **category** columns. Everything else here — the value
column itself, strings, numerics, lists, ingest, deletion, the fold — is specified and unbuilt, marked
at each claim.
**Reads against:** architecture §4 (I2, I7, I9, I12), §9, §10.2–§10.4, Appendix A;
[`contracts.md`](contracts.md) §2.1–§2.4; [`write-path.md`](write-path.md) §2.1–§2.5, §4.3–§4.5,
§5.3–§5.4, §7; [`compaction.md`](compaction.md) §2–§4;
[`per-point-attributes.md`](per-point-attributes.md) §2–§3; design memo 2026-07-29 (secondary
attribute indexing); decisions [0013](../decisions/0013-mark-specified-vs-implemented.md),
[0039](../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0042](../decisions/0042-a-dictionary-extent-never-repeats-a-descriptor.md),
[0048](../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md),
[0050](../decisions/0050-a-fold-invalidates-the-term-index-and-every-fragment.md);
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
could forget, and substring matching needs no trigram index. And the work a scan does is a function of
the candidate mask and the column, never of the value — which is what makes a hidden value and a
nonexistent one indistinguishable **in work**, as per-point-attributes §3.8 requires (§2.2).

Two further consequences. The artefact is the same one per-point-attributes §3.3 needs for vocabulary
visibility, so building it closes the `listing = "per_viewer"` refusal rather than deferring it. And it
is **entity-space, so it is slice-invariant**: a slice attaches, populates or drops without touching any
of it (§7).

> **⊘ Built: categories only.** For a category column the value column, its presence bitmap, the
> masked scan (equality and set membership), `entity → value`, and the derived per-value postings all
> exist, in both build implementations and under the manifest digest. **Everything else here is
> specified and unbuilt** — every other family, ingest, deletion and the fold — and is marked at each
> claim. Present behaviour is fail-closed
> throughout: a filter that cannot be expressed narrows nothing, and a value set that cannot be gated
> is withheld entirely.

### 1.1 What this deliberately does not do

**No trigram index.** Substring matching over a `filter` string column is a **masked scan predicate**,
needing no index and no new library. An earlier revision cut substring to [#44] because a trigram
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
inverts a superset into a subset — the unsafe direction for any family producing one.

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
| `contains` | 9.7 ns | 96 ns |

The ratio is what the storage is: per value the scan streams two 8-byte offsets and the value's
bytes, about 22 against a `u32` column's 4. Equality is therefore near memory bandwidth for the
shape, and the remaining headroom is a narrower offset — not an index. **The one cell outside the
budget is a scattered candidate over a text column**: `contains` at 96 ns is ~1 s per 10⁷ candidate
entities, so a poorly-correlated principal issuing a substring filter at 10⁹ is at the edge of the
ruled budget. That is the corner an accelerator would address if one is ever wanted, and it is a
narrower corner than "broad coverage" or "text" alone.

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
So a `per_viewer` category gets postings whatever its `used_for` says, and a `public` one does not,
because a published value set is served as authored and derives no membership at all.

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

> **⊘ One case is unmeasured.** A *scattered* posting whose containers the candidate meets while no bits
> match does container-proportional work for an empty result. A uniformly scattered value cannot be
> fully hidden from a broad principal in the first place, so the case is narrow — but it is where a
> residual channel would live, and it has not been measured.

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
attrs/<column>/extents/<flush_id>.arrow  one appended extent per flush that touched the column
```

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

`bool` is the degenerate numeric. **Lists cost no format work**: a multi-valued attribute is the same
column with more than one value per entity. Lifting the parse refusal is a schema change and a build
change, and it lifts **for `filter` only** — `inspect` has no sidecar to place data in, and decision
0013 requires an unimplemented placement to stay refused naming itself.

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

The current emit groups the entity-major values the attribute pass already materialises, which holds a
`u32` per non-absent entity per column on top of the attribute tail. That is a scale ceiling rather than
a correctness gap, and it is the same ceiling the existing attribute reader already has: it materialises
`Vec<ScalarValue>` per column at ~24 B per value, which at 10⁹ is tens of GB and outside the memory plan
regardless. A streaming emit — a counting pass for band prefix sums, then an emit pass — is what the
plan's arithmetic needs and is not written.

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
entities occupy an ascending range and nothing already written moves. The extent covers that range;
addressing across extents is a bounds check rather than a search, and a scan decomposes across them by
clipping the candidate mask to each.

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
narrows `M_sel`, which is safe under **I12**.

> **⊘ None of this is built.** Ingest touches no attribute artefact today.


## 6. Deletion, suppression and retirement

Write-path §5.4's two removal rules govern, and **conflating them is fail-open**. The distinction has been
lost twice in this project's review history, which is why it is restated at every site that touches it.

| Event | The filter artefact | Where the invisibility lives |
|---|---|---|
| Suppression | **nothing** | the overlay's `suppressed` set; retires **only** on unsuppress (Rule S) |
| Unsuppress | nothing | the entity leaves `suppressed`, and the derived mask is re-derived from scratch rather than subtracted incrementally |
| Deletion | **nothing until the fold** | the overlay's `deleted` set; the flush never writes the row, and the entity ID stays burned (**I9**) |
| Compaction fold | deleted entities' value slots are blanked; the derived postings are rebuilt whole | the tombstone leaves `deleted` in the fold's own publication (Rule F) |

> **⊘ Specified, not implemented.** The middle column describes an artefact whose value column does not
> exist. Rules S and F themselves are built and enforced for the authorisation index, and the fold that
> executes them runs — so a reader may take the *rules* as delivered and must not read this table as
> saying anything about attribute data.

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

**What the fold does per column:** the value column is rewritten sequentially with deleted slots blanked
and appended extents concatenated; the presence bitmap is rebuilt; a category's postings are rebuilt from
the folded column. A 4 GB `u32` column re-copies sequentially in seconds (*modelled* at disk bandwidth).
Entity ranges never renumber, so an extent no deletion touched can be carried forward unchanged rather
than re-copied — an option the implementation may take, not a requirement.

**Nothing here is a third retirement rule.** Every derived structure is rebuilt from the column at the
fold, so derivation self-retires, and Rules S and F remain the whole of the removal model.

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

### 6.2 What a fold costs

The fold reads and rewrites every filterable column plus its derived structures. Sizing it against P3's
measurement — a corpus-scale streaming read costing a concurrent viewport up to **2.03×** — is the
constraint that matters, not the wall-clock: the fold runs in decision 0056's gated window precisely so
that cost lands where a viewport is not competing for it.

Two things the flat design removes from this budget. There is no level-tree accelerator to rebuild, which
an earlier revision priced at ~30–50 min *modelled* for a high-cardinality attribute at 10⁹ **on every
fold**, because delta tiers carried level-0 postings the tree could not see. And there is no dictionary
to carry forward, so no ordinal-preservation obligation and no never-shrink rule to honour across it.

What remains is proportional to the columns themselves: one sequential read and one sequential write per
column, plus a rebuild of each category's postings from the folded column. Decision 0056's fold gauges
gain attribute-bytes terms so the cost is visible rather than inferred.


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

---

## Appendix R — review trail

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
