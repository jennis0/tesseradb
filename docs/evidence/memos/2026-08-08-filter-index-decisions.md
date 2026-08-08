# The filter index: options, measurements, and what needs ruling

**Date:** 2026-08-08 · **Status:** Decision memo — not normative. **D1, D3 and D4 ruled 2026-08-08; D2 and D5 resolved by
the filter-latency budget below.**
**Reads with:** [`filter-index.md`](../../design/filter-index.md),
[`filter-surface.md`](../../design/filter-surface.md) (both Provisional),
[`2026-08-08-filter-index-structures.md`](2026-08-08-filter-index-structures.md) (the structural
analysis), [`probes/2026-08-08-filter-layout/`](../../../probes/2026-08-08-filter-layout/) (the
measurements this memo turns on)

Five rulings, all now made. The memo is kept as the record of what each was decided on — and of two
places where my framing, not the measurement, was what needed correcting.

---

## 1. What is being decided

`used_for = "filter"` is declared in `schema.toml` and, until this week, refused at parse. The two
provisional design documents specified an **inverted-postings** index for all four families —
categories, strings, numerics, lists — modelled on the authorisation term index. That premise did
not survive review, and the documents are now known to be wrong in their organising principle
rather than in their details.

Three findings moved the ground, in order:

1. **The postings shape was imported, not argued.** `M_auth` needs inverted postings because it is
   a union over ~10⁴ term postings materialised once per session and reused by every request in it.
   A filter operand is per-request, over a narrow predicate, and can take `M_auth` as a candidate
   set. Nothing established that the shape transfers.
2. **Postings are a *categorical* instance** (owner ruling). A category's value already has an
   integer identity — its vocabulary code — and its overlap is high, so one bitmap replaces millions
   of repeated codes. No other family has either property. Other types belong in a flat table or in
   an index suited to that type.
3. **The scan is affordable far further than anyone modelled, and the layout question has a
   measured answer** (§2). This is the first hard evidence in the whole discussion; everything
   before it was modelled or assumed.

---

## 2. Measurements

**New, this campaign** ([`probes/2026-08-08-filter-layout/`](../../../probes/2026-08-08-filter-layout/)).
In-memory, single-threaded, `u32` column, 12-core WSL2 host.

| Quantity | Value | Standing |
|---|---|---|
| Masked scan, contiguous candidate | **~2.9 ns per candidate entity** | measured, stable 10⁶→10⁹ |
| Masked scan, scattered candidate | **~22 ns per candidate entity** | measured; 7.6× penalty is cache misses, not bandwidth |
| 25% principal, 10⁹, bare column | **730 ms** | measured |
| Scale behaviour | linear, 9.5–11.2× per decade, no cliffs | measured across three decades |
| Affordable coverage at the ruled 1 s filter budget | ~3.4×10⁸ contiguous (34% of 10⁹), ~4.5×10⁷ scattered | derived from the two constants |
| Full-corpus scan at 10⁹ | ~3.0 s — the only case outside the band | measured |

Addressing structure, bytes per present entity at 10⁹ (totals in parentheses below 1 B):

| Presence shape | bare | runs | roaring | pairs |
|---|---|---|---|---|
| Universal | **0** | (12 B) | (216 KB) | 4.00 (4.0 GB) |
| Slice-blocked, 10 slices | n/a | (12 KB) | (36 KB) | 4.00 (400 MB) |
| Scattered 10% | n/a | 10.80 | **1.25** | 4.00 |

Masked-scan latency at 10⁹, ms:

| Presence / candidate | bare | runs | roaring | pairs |
|---|---|---|---|---|
| universal / 1% contiguous | **28.7** | 35.0 | 1078 | 279 |
| universal / 25% broad | **730** | 939 | 1518 | 870 |
| universal / 1% scattered | **221** | 259 | 3684 | 1413 |
| slice-blocked / 25% broad | n/a | 2873 | **161** | 800 |
| scattered / 25% broad | n/a | 10204 | **232** | 881 |

**Pre-existing, load-bearing.** Selection-path p99 **158–191 ms** at 10⁹ (§10.4) — the *viewport*
budget, and **not** the one a filter is judged against (see D2). Posting-shape latency spread **21.7 ms vs 2,885 ms** at equal
coverage, from a **13×** per-container constant (114 ns array, 1.54 µs bitmap). Signature-sort
compression **8.9–36.7×**, and **1.0×** for the `surnames` policy — contiguity is policy-dependent,
never assumed. Category membership postings cost **0.31–1.01×** the render column they index. Row-space
projection is **cardinality-dependent**: 127 ns/set-bit at 69M (`probes/results.md` §6), 10.7 ns/item
at 10⁹ (§10.4) — quote the range, never a flat constant.

**Refuted by measurement.** The structural memo's first revision assumed 5–10 GB/s and derived
40–80 ms for a 25% principal at 10⁹. Measured is 730 ms, an effective ~1.4 GB/s. **The masked scan
is bound by per-candidate work and cache misses, not memory bandwidth.** Any sizing that treats it
as a bandwidth problem runs ~5–7× optimistic.

**Not measured, and material.** Value widths other than `u32`; strings and variable-width values;
parallel scan; cold/on-disk scan (all of the above is RAM-resident); and the row-space projection
curve between the corpus's two disagreeing measured points, which the filter budget promotes to the
binding term (§6).

---

## 3. The options

### 3.1 Artefact of record

| | What it is | Verdict |
|---|---|---|
| **A. Inverted postings, all families** | One bitmap per distinct value; strings and numerics get dictionaries and ordinals | **Dead.** Needs a second durable identity per value, seeded from every home the code is (manifest, extensions, WAL); a duplicated ordinal is two values sharing a posting slot — leak-register **C11**, reachable by ordinary operation. Two document revisions were spent repairing this and the third found the mechanism reintroduced the hazard it was added to prevent |
| **B. Flat entity-indexed column, type-appropriate accelerators derived on top** | Values addressed by entity; accelerators self-retiring, rebuilt at the fold, never a second durable identity | **Recommended.** Deletes dictionaries, promotion, delta tiers and ordinal stability before any of it is built. Gives `entity → value`, which the conformance oracle relation needs and which #44 needs |
| **C. Explicit `(entity_id, value)` pairs** | The obvious table | **Refuted on both axes**, at every scale and presence shape measured. 4 B/entity *per column* — 64 GB at 10⁹ × 16 columns, against a ~20 GB term-index budget — and never fastest. Its case is simplicity, not cost |

### 3.2 Addressing, given B

Measured, so this is a rule rather than a judgement: **bare array where presence is universal;
Roaring presence bitmap with compact values where presence is partial.**

Two traps the measurements exposed, both of which the modelling had backwards:

- **Run tables** `(start, len, base_rank)` are unbeatable on storage — 12 bytes for an entire 10⁹
  column — and collapse on broad candidates, because rank is a binary search per candidate entity:
  2.9 s slice-blocked, 10.2 s scattered. **Do not choose an addressing structure from its storage
  column.**
- **Roaring is the *slowest* layout when presence is universal** (1,078 ms vs bare's 28.7 ms), because
  the rank walk is O(present) and swamps the cheap intersection. It wins only where it is needed.

**Presence is partial more often than it looks.** `x-tessera-slice` is per request and flush plans
per slice, but write-path §4.2 makes an entity range *ascending-with-holes* "where a commit window
interleaved slices" — merge's adjacency test is `hi < lo`, not `hi + 1 == lo`, for exactly that
reason. So under concurrent multi-slice ingest a dense positional extent wastes up to (S−1)/S of its
slots, and "the array index is the entity id" is single-slice reasoning.

### 3.3 Table layout

**Per-column files by default.** Not a preference: per-column presence makes the compact value arrays
*different lengths*, and one Arrow record batch cannot hold columns of differing length. Grouping
survives only for a set of columns that are all universal-presence and fixed-width, where sharing is
free because there is no `entity_id` column to share. The storage win originally sought from grouping
does not exist under B — the IDs are not stored at all.

### 3.4 Numerics and strings

The level tree is Meilisearch's answer *because* Meilisearch stores docid bitmaps; range-encoded BSI
is FeatureBase's *because* FeatureBase is a bitmap engine. Neither premise is ours under B. Both are
**cut from the base design**; BSI survives only as a named escalation for the broad corner. The
order-preserving float key goes with them — it exists to make IEEE bits sort correctly as unsigned
bytes in a byte-ordered key store, and native `f64` comparison is already correctly ordered, with
NaN-matches-nothing for free.

Strings need **no dictionary, no FST, no index** under B: equality, prefix and substring are all
masked scans over a flat UTF-8 column. No new library is required beyond `memchr`; `croaring` and
`arrow` are already in tree.

---

## 4. The decisions

### D1 — Adopt the flat entity-indexed column as the artefact of record, categories included?

Category postings are **built and green** (§5). Adopting B reclassifies them from *the record* to a
*derived, base-only, fold-rebuilt accelerator*.

- **Yes:** one lifecycle for every family. Attribute delta tiers, value dictionaries, promotion and
  ordinal-stability machinery are deleted before they are written. The conformance relation stops
  being a side output the fold could forget and becomes the artefact itself. Cost: ~1× the render
  column of extra entity-space storage per category (2 GB for a `u16` category at 10⁹).
- **No:** the built code stands unchanged, but categories keep a separate lifecycle needing per-flush
  tiers, and `entity → value` for filter-only categories needs the side relation after all.

**Recommendation: yes.** Its grounds were never primarily storage — they are lifecycle deletion, the
work-channel closure in D4, and the conformance dividend — and the measurements strengthened the
layout economics rather than weakening them.

### D2 — Is the accelerator required? **Resolved: no.**

**The budget was wrong, and it was wrong in my framing rather than in the measurement** (owner
ruling, 2026-08-08). I judged the 730 ms scan against the selection path's 158–191 ms p99 and
concluded an accelerator was *required wherever coverage can be broad*. That is a viewport budget. A
filter is not a viewport: **it changes far less often, and 0.5–1 s at 10⁹ is acceptable.**

At the measured constants that budget buys ~3.4×10⁸ contiguous candidate entities — **34% of the
corpus at 10⁹** — for 1 s, and a 25%-coverage principal measures 730 ms with no accelerator at all.
Only near-total coverage exceeds the band, at ~3.0 s measured.

The arm ran anyway and its result stands: postings reduce the worst measured cell **107×**, from
5,271 ms to 49.5 ms, and cost 8 B–125 MB. (The 21.7/2,885 ms union spread I raised as a risk does
not apply — it measures a union over ~10⁴ *authorisation* postings, not a single-value intersection.)

**So the accelerator is an optimisation for the privileged tail, not a requirement.** Because it is
derived, a deployment that builds it and one that does not answer identically and differ only in
latency — a per-column build choice rather than part of the contract. The built code keeps its place
on those terms.

### D3 — Does substring re-enter scope?

Trigram matching was cut to [#44] because a trigram conjunction returns a superset needing
verification against the stored value, and a filter-only attribute had no `entity → value` route. That
premise was a consequence of option A, not a requirement. Under B, `entity → value` is one array
index, and substring is a masked scan needing no trigram index at all.

- **Reinstate**, as a scan predicate on string columns — no new structure, no new library.
- **Leave at #44**, and let the text operand own it when it lands.

**Recommendation: reinstate the *scan* form; leave the trigram index at #44.** They are no longer the
same piece of work.

### D4 — The residual timing channel on categories

Under B the masked scan's work is a function of `(candidate, column)` and never of the value, so a
hidden value and a nonexistent one are indistinguishable **in work by construction** — which is what
per-point-attributes §3.8 originally required and what filter-surface §2.1 superseded because option A
could not deliver it. That supersession can be **reversed for every scanned family**.

Categories are the exception. `posting(v) ∧ M_auth` costs O(min-containers), so an absent code returns
faster than a broad hidden one. The proposed closure inverts the loop — iterate the *candidate's*
containers and probe the posting per container, Θ(candidate containers) whatever the value. This
equalises the **probe count** but not the work inside each probe: a failed container lookup is cheap, a
hit does a real intersection.

- **Accept the residual** and register it, narrower than the channel it replaces.
- **Require the candidate-driven probe** for `per_viewer` categories only, straightforward intersection
  for `public` ones.

**Recommendation: both** — the probe form for `per_viewer`, the residual registered rather than claimed
absent. Do not let "value-independent by construction" reach a design document; it is not true as
stated.

### D5 — Zone maps, and the normative amendments

**Zone maps: declined, and the filter-latency budget is what makes that free.** They trade a C4-shape
timing channel — block-granular skipping consults *unmasked* extrema, so timing reveals whether
invisible rows in a block fall in a queried range — for a speed-up on a range that already measures
730 ms at 25% coverage, inside the band. **Adding a leak-register row to buy latency that is already
affordable is the wrong trade**, and declining them keeps the work-indistinguishability property whole
rather than carving an exception into it. Bit slicing is declined as unnecessary on the same arithmetic;
if it returns it should return on an *aggregate* argument (masked min/max/sum, I2-clean), not a
range-latency one. A numeric column is therefore a value column and nothing else — no structure choice,
no build-time cardinality measurement, no fold rebuild.

**Architecture §10.5 r21** routes "data read once per query" to "an entity-space bitmap behind the
filter contract". A scanned flat column is per-query but is neither a bitmap nor a hot column — a third
placement the rule does not have. Adopting D1 requires amending it, plus §8.3, Appendix A,
per-point-attributes §2.1 and contracts §2.4. Pre-release, format change is free
([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)); the
amendments are the cost, and they are owner-level because §10.5 is normative.

---

## 5. What is already built

Green under the full gate as of this memo: `tessera-filter` (the crate and its keyed reader), the
`filter` placement in schema and manifest, and the batch build's per-column keyed postings emit for
category columns — in both build implementations, byte-identical under
`attributed_build_is_byte_identical_to_the_reference_build`.

**D1 does not invalidate it.** Under B it is exactly the accelerator the principle says categories
should have — the one family whose values already carry an integer identity and whose overlap is high
enough to earn a bitmap. What changes is its *standing*: derived rather than the record, rebuilt at the
fold rather than extended by delta tiers.

What is **not** built, and is marked at each claim in `filter-index.md`: every family but categories,
the streaming emit and its band arithmetic, ingest, deletion, and the fold.

---

## 6. Recommended order

Steps 1–5 are done: D1, D3 and D4 ruled; D2's arm run and D2 and D5 resolved by the budget; both
designs rewritten; the §10.5 r37 amendment and its four companions taken as one change; the dead
cardinality probe deleted.

**What the budget promoted to the top of the remaining work.** With the scan affordable, the binding
term is no longer finding the matching entities — it is **projecting them into row space**. That cost is
O(set bits) and the corpus's two measured points disagree per-bit (127 ns at 69.3×10⁶; 10.7 ns/item at
10⁹), so the curve between them is unmeasured, and a broad filter result is exactly the large-cardinality
end. `filter-surface.md` §4 is suspended because its shared-projection cache assumed unmasked evaluation;
the alternative it never costed — a **per-tile membership test** against the entity-space result, which
projects nothing — is now the more likely answer. That measurement, not the scan, is what the design
should look at next.

One unrelated item, open since before this discussion and unaffected by it: `/v1/categories` crossing
the compute-admission boundary is a **contracts §3.2 edit in its own right** — that section gives an
explicit reason ("no mask composition, no projection, no file IO") that stops being true.

[#44]: https://github.com/jennis0/tessera-index/issues/44
