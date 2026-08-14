# The record and the searchable thing — collapsing `filter`, and what strings actually need

**Date:** 2026-08-12 · **Status:** Design memo — evidence, not normative. Proposes rules; rules
nothing. Six rulings are named in §7.
**Measured input:** [`probes/2026-08-12-string-storage/`](../../../probes/2026-08-12-string-storage/)
(this memo's own — three arms on real arXiv titles and surnames, to 2.4M);
[`probes/2026-08-12-filter-placement/`](../../../probes/2026-08-12-filter-placement/) and
[`probes/2026-08-08-filter-layout/`](../../../probes/2026-08-08-filter-layout/), whose constants
this reads against rather than re-measures.
**Reads against:** [`filter-index.md`](../../design/filter-index.md) §1.1, §2.1–§2.6, §5, §6.2;
[`filter-surface.md`](../../design/filter-surface.md) §2.1, §3–§4;
[`per-point-attributes.md`](../../design/per-point-attributes.md) §1–§2.1, §3.7, §4.3; architecture
§4 (I2, I7, I12), §8.2, §10.3; the companion memo
[`2026-08-12-filter-placement.md`](2026-08-12-filter-placement.md), cited as **placement §n**;
decisions [0013](../../decisions/0013-mark-specified-vs-implemented.md),
[0039](../../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0062](../../decisions/0062-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0063](../../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
[0064](../../decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md),
[0065](../../decisions/0065-the-inverse-permutation-is-stored-for-the-filtered-viewport.md),
[0066](../../decisions/0066-none-of-requires-a-value-and-names-one-column.md).

---

## 1. What this proposes

**A token index over a prose column is smaller than the flat column it replaces, not larger.**
Measured, on real titles, at three scales: **23.6 B/entity against 83.6**, a 3.5× *saving*, holding
across a 9.6× scale range. Every scheme for shaving the flat column — block compression, FSST,
narrower offsets — argues over 1.1–2.4× on a structure that should not be the structure.

That reverses the shape of the question. Five proposals follow.

1. **`filter` stops being something a caller declares.** A field says what it *is*, whether it
   renders, and whether it is **searchable**. Placement derives, as §2.1 already requires of
   everything else. Placement §5's first ruling becomes a consequence rather than a question.
2. **Where the record lives derives from *how* the column is searched, not merely whether.** A
   scanned column's record must be flat and uncompressed, because the scan needs random access at
   scan rates. **An exactly-answering index means no query touches the record at all**, so it may be
   block-compressed at a measured 2.44×. The index and the compressed record are one decision.
3. **A searchable column's mechanism follows the shape of its vocabulary, not its type.** Prose and
   names are different vocabularies and the measurement separates them: 66–70% of title postings
   entries fall in a thousand tokens, against 24–33% for surnames.
4. **A token index over prose is earned, on both axes.** It is cheaper on disk (§3) *and* it is the
   only mechanism that serves the aggregate surface (§5), which a scan cannot at coarse zoom and a
   viewport-bounded route cannot at all.
5. **The row-bounded string route (§6) is an optimisation, not a substitute.** It makes the
   interactive high-zoom cell cheap and it changes no bytes on disk.

**An earlier draft of this memo argued the opposite of 4, and the reasoning failed in a way worth
recording** so it is not repeated: it took "searchable ⇒ record must be uncompressed" as a
constraint, when that holds *only* under the scan route. Choosing the scan and then pricing storage
under the scan's constraints assumes the conclusion. It also treated coarse-zoom aggregates as one
rare cell when the map at zoom 0 *is* the corpus, which placement §1 states plainly and the draft
quoted without following through.

---

## 2. The declaration, collapsed

Today a caller writes `used_for = ["render", "filter", "inspect"]` — three placements, orthogonal
and additive, each a *storage decision wearing a use's name*. Spec §2 derives them from §10.3's
access ratios: per rendered mark, per query, per interaction. The cadences are right. What is wrong
is that `filter` and `inspect` are two faithful copies of one value and the caller chooses to pay
for both.

The proposed surface says what a caller actually knows:

| declared | meaning |
|---|---|
| `type` | what the value is |
| `render` | draws on the map — a fixed-width hot column, per slice, per mark |
| `searchable` | can be filtered or searched on |

and everything else derives. `inspect` is not a placement a caller opts into: **the record always
exists**, because a field declared at all is a field the system stores. What varies is where, and §3
is the rule.

**Placement §5's ruling 1 falls out.** That memo asks whether `used_for = ["render"]` should imply
filterable and recommends yes. Under this surface the question does not arise in that form — the hot
column already affords a row-space search over the request's own rows, so a rendered column is
searchable at no additional storage, and the ruling reduces to the route rule (placement §3), which
is a cost question rather than a surface one.

**`used_for = ["filter"]` today already means "searchable, and do not render it"**, so the migration
is mechanical for every existing schema, and decision 0048 makes it a rename with no compatibility
surface. It does not touch what a caller may not do: decision 0039's fence is a rule about hot
columns and is unmoved; per-point-attributes §3.7's refusal restates as "a list is never rendered",
which is the same sentence.

---

## 3. Where the record lives, and why the index is a storage argument

Two facts decide it, and the second is the one an earlier draft missed.

**A scanned column's record cannot be compressed.** The campaign measures a block-compressed sidecar
at 7–594 µs per single-value random read; a scan touching 300,000 scattered entities through
64 KB blocks is 13.5 s. Against filter-index §2's uncompressed constants the best ratio (2.44×)
costs 48.6 ns per value with the whole block read — 29× the contiguous scan — and 4 KB blocks reach
only 1.87× at 127 ns/value, worse than the uncompressed *scattered* figure. Compression is not a
trade to price for a scanned column; it is a route that does not exist.

**An exactly-answering index removes the scan, and with it the constraint.** If the query is served
from postings, no request path touches the record, so the record is read only at drill-down cadence
— where 169 µs is free and 2.44× is available.

At 10⁹, for `title` (per-entity figures from arm 3 at 2.4M; the extrapolation is linear and is
marked below):

| | on disk | serves coarse-zoom aggregates | serves drill-down |
|---|---|---|---|
| flat `utf8` column, scanned | **83.6 GB** | no (§5) | yes |
| token index + block-compressed record | **23.6 + 31.0 = 54.6 GB** | yes | yes |
| token index alone, no record | 23.6 GB | yes | no |

**Both axes point the same way**, which is the finding. The index is not speed bought with storage;
it is less storage *and* the capability the flat column lacks.

⊘ **The 10⁹ figures are a linear extrapolation of a per-entity cost measured to 2.4M.** The trend is
flat across a 9.6× range — 23.33 → 22.70 → 23.55 B/entity while the singleton fraction climbs 45.8%
→ 59.5% — because a head token's posting densifies as a tail token's spreads, and the two cancel.
2.4M → 10⁹ is a further 400× and nothing measured says the cancellation survives it. This is the
single number in the memo most worth extending, and §7's ruling 4 does not depend on its exact
value, only on its sign.

The resulting rule:

| | fixed-width | variable-width |
|---|---|---|
| **searchable by scan** | flat value column (as today) | flat **uncompressed** value column (as today) |
| **searchable by index** | — (a code column *is* the index's input) | index + **compressed** record |
| **not searchable** | flat value column — cheaper than a sidecar row at 1–8 B | **compressed** record, 2.44× measured |

### 3.1 Two negative results, kept because the obvious answers look affordable

**FSST does not rescue a scanned column.** It reaches 1.56× (a floor; published ~2× on text) and
keeps random access, but it is asymmetric across the four string operators and the asymmetry lands
badly. `eq` and `in` compare in the compressed domain and get faster; `prefix` is near-neutral;
**`contains` cannot** — a substring may begin mid-symbol and encodes differently by where the greedy
match started, so the value must be decoded first. The casualty is filter-index §2's
contiguous-region optimisation, 9.7 → 1.7 ns/entity, which searches concatenated bytes that would
now be encoded. ⊘ Argued from FSST's published mechanics, not measured.

**Dictionary encoding contributes nothing to a near-unique text column**, measured: 299,846 distinct
values in 300,000, and parquet's dictionary saves nothing over plain zstd. That transfers to any
scheme hoping to intern this column and does *not* transfer to a category, where interning is the
design.

---

## 4. Mechanism follows the vocabulary, not the type

| family | example | mechanism | new machinery |
|---|---|---|---|
| **enum-like** | `archive`, `license` | a `category` — flat code column, derived per-value postings | none; built |
| **prose** | `title`, `abstract` | **token index**, record compressed beside it | ⊘ unbuilt; §6 |
| **name lists** | `authors`, `surnames` | **open** — see below | placement §4's list, or §6's index |

Prose earns the index on the measurement in §1 and §3. The **top-1000 share** is where its
vocabulary differs from names: 66–70% of title postings entries fall in a thousand tokens, against
24–33% for surnames. That fat head is what amortises postings, and it is exactly what placement §1
arm 3's surname-shaped result could not speak to.

**Authors is open, and an earlier draft of this memo settled it too early.** That draft read
placement §1 arm 3 — CSR 24.0 B/entity against postings 32.3 on a surname-shaped column — as ruling
postings out. This campaign measures *real* surnames at a nearly identical fan-out and vocabulary
(4.7 values/entity, 424,168 distinct) and gets postings at **20.9 B/entity against a flat joined
column's 43.0**. The parameters line up and the numbers do not, because arm 3's synthetic values are
not real surnames' width. **Neither result is wrong and neither settles it; arm 3's harness needs
re-running on real values.** Until then authors' mechanism is a measurement away, not a ruling away.

---

## 5. Why the scan cannot be the whole answer: the aggregate surface

The filter contract (§8.2) is entity-space — *every filter returns a set of entity IDs as a bitmap,
and composition is intersection* — because what the system produces is **counts, densities, clusters
and summaries**, and I2 requires each to be computable from inside `M_auth` alone. That is the
product, not a secondary surface.

**A scan cannot serve it at coarse zoom.** At 10⁹ with a scattered 25%-coverage principal, a
`contains` over the authorised set is 250×10⁶ × 96 ns ≈ **24 s**; even the contiguous constant over a
whole-corpus candidate is 1.7 s. Both are outside filter-index §2.2's 0.5–1 s budget, and coarse
zoom is where placement §1 arm 2 already found the entity-space column earning its bytes for
categories.

**A viewport-bounded route cannot serve it at all**, and placement §2.1 says so directly: a
row-space filter *"cannot serve a caller outside a viewport, which is every filter surface the
system might grow that is not a map request."* Nor does it help at zoom 0, where — placement §1
again — *"the view is the corpus"*, so rows-on-screen approaches corpus size and the route
degenerates into the scan it was avoiding.

**The index restores the contract's shape rather than bending it.** A token lookup yields an
entity-space bitmap; intersecting it with `M_auth` is the category-postings construction, already
built and already measured affordable at 10⁹. No new operand kind, no domain-bounded answer, no
ruling on §8.2 required — which makes the index the *conservative* option against the contract and
the row-bounded route the novel one.

---

## 6. The row-bounded string route — an optimisation, and what it is not

Placement §2 found a filter bounded by what is on screen costs 0.48–0.73 ns per viewport row,
invariant in corpus size, mask shape and coverage. That route is closed to strings mechanically: a
`utf8` column is refused from the hot column (per-point-attributes §4.3), so there is nothing in
`columns.arrow` to test.

The same escape is available from the other direction and **is not built**. Decision 0065 stores the
inverse permutation and `row-entity.u32` publishes it, so a viewport's rows reach their entities by
lookup; the string operand can then be evaluated over *those entities only*. Using filter-index §2's
scattered constants plus ~15 ns per row for the lookup: `eq` 9.0 ms, `prefix` 14–17 ms, **`contains`
~33 ms**, for a 300,000-row viewport at 10⁹, any principal.

**This is not what the per-tile crossing does.** `cross_filter_into_row_space` takes an
already-evaluated entity bitmap and asks each viewport row whether its entity matched — it avoids
the *projection*, not the *evaluation*. This narrows the evaluation itself.

Its properties are good: exact over the domain (`FilterRows::Viewport { rows, domain }`, which
`EffectiveMask::with_filter` already consumes and `filter_routes_agree_over_the_domain` already
asserts); composes under 0062 by placement §2.2's rule unchanged; I2 holds, since every count is
computed inside `M_auth ∩ entities(viewport)`; and work-indistinguishability is preserved, work
being a function of the viewport's rows and the column and never of the value.

**But it changes no bytes on disk and it does not serve §5.** It is worth building for the
interactive cell — and it is worth building *whether or not* the index lands, since a per-keystroke
high-zoom filter should not pay a postings lookup either. It is not an argument against the index
and an earlier draft used it as one.

### 6.1 What the index must be true of

**Term identity is per-extent and rebuilt whole at the fold**, self-retiring exactly as the category
postings are (filter-index §1). That is what keeps a durable per-value identity — and the C11
ordinal hazard that lives in one — from arising. This system's extent/flush/coalesce/fold lifecycle
is structurally a segments-and-merge model, which is what makes a derived index affordable here.

**The index is never served.** filter-index §1.1 refuses a *value list* and specifically prefix
autocomplete, because offering suggestions "would manufacture a value set for a type that has none".
That is a refusal of a surface crossing the trust boundary. An index the server never exposes, used
to narrow a candidate then intersected with `M_auth`, publishes nothing and is not a listing. **The
autocomplete refusal stands untouched and is not evidence against the index.**

**Work-indistinguishability must be shown, not inherited.** filter-surface §2.1's property — a
hidden value and a nonexistent one cost the same — is *measured* for category postings (both
0.000 ms at 10⁹ and 25% coverage, Roaring short-circuiting on container keys). A token vocabulary is
the same structure at 270k+ terms and the property is expected to carry, but it is the one thing
here that is a disclosure claim rather than a cost claim, and it should be measured before the index
is normative.

**Tokenisation becomes a conformance surface.** The oracle must agree with the tokeniser, and
`contains` changes meaning from arbitrary byte-substring to token match. For prose that is what
callers want; for identifier-shaped columns substring genuinely means substring, so `utf8`-with-scan
should survive alongside the new type rather than be replaced — which is the split filter-index §2
already anticipates in naming `text` as where this is revisited.

### 6.2 An argument to withdraw

Placement §4 rules that a string list gets no derived postings, resting it on "nothing may
manufacture an identity for a string value (§2.3's C11 hazard)". **Recommend the rule be regrounded
on measurement rather than on the hazard.** The C11 form forbids the prose index above, which the
evidence does not — and §4 shows the measurement for names is itself unsettled. Whatever arm 3's
re-run says, it should be what carries the rule.

---

## 7. What needs ruling

Each is stated so it can be ruled without reading the code. The recommendation is mine.

1. **Does `filter` stop being a declared placement, in favour of `searchable`?** *Recommend yes*
   (§2). It removes the double-store, makes placement §5's ruling 1 a consequence, and changes no
   invariant's statement. The cost is a schema surface change across every fixture and document that
   spells `used_for`.
2. **Does the record's home derive from the search mechanism** (§3) — flat and uncompressed under a
   scan, compressed beside an index, compressed when not searched? *Recommend yes.* What is given up
   is a caller's ability to ask for a cheap copy of something they also scan, which §3 says was never
   affordable.
3. **Is a token index over prose in scope?** *Recommend yes* (§1, §3, §5) — it is cheaper on disk
   than the column it replaces and it is the only mechanism serving coarse-zoom aggregates. This
   reverses an earlier draft of this memo. The cost is the largest item here: a token vocabulary,
   per-extent term identity, the extent/coalesce/fold triple over it, and an oracle agreeing with a
   tokeniser.
4. **Is the row-bounded *string* route admissible under §8.2?** (§6.) Same question as placement §5's
   ruling 2 — a row-space operand bounded by the request's domain — and *recommend the same answer*,
   since they would be one mechanism reached by two paths. Ruling them apart is the outcome to avoid.
5. **Reground placement §4's string-postings rule on measurement rather than C11** (§6.2), and
   **re-run arm 3 on real values** before authors' mechanism is fixed either way (§4). *Recommend as
   stated*; the rule itself does not move today.
6. **The standard dataset** (the question this began as): *recommend building the 2,422,486 /
   25,000,000 / 10⁹ bundles on the fixed-width schema now* — categories, dates, `license`, the bools,
   the counts — and holding `title`, `authors` and `abstract` for the mechanism rulings above.
   Committing them to a `utf8` filter column would build the structure §1 measures at 3.5× worse than
   its replacement.

---

## 8. Order, against placement §6

Placement §6 puts the render-column feature first and the list family second. This memo adds three
items:

- **§2's surface change** is cheapest immediately before or with the render-column feature, which is
  already touching schema parse and `/v1/meta`'s operand list. Deferring it means shipping
  `used_for = ["render"]`-implies-filterable as a rule and then deleting the rule.
- **§6's row-bounded string route** sits alongside the list family — same crossing, same
  domain-bounded answer type — and is independent of the index.
- **§6's index is the epic**, and it should not start before §6.1's work-indistinguishability
  measurement and §4's arm-3 re-run, which together decide its shape for two of the three families.

Nothing above changes an invariant's statement. I2, I7 and I12 are argued at §5 and §6; decision
0039's fence stands unmoved; and the one rule this memo would retire (§6.2) is retired in its
grounding, not in its effect.
