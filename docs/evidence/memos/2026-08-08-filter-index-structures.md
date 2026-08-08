# Design memo — filter structures: the flat column is the artefact, postings are the categorical accelerator

**Date:** 2026-08-08
**Status:** analysis memo — evidence and recommendation, never normative. Written against
`filter-index.md` r3 and `filter-surface.md` r2, both provisional; this memo treats them as under
attack, not as authority. Every figure is tagged **measured**, **modelled** (arithmetic from a
measured constant) or **assumed** (no repo measurement exists).
**Revised 2026-08-08 (r2)** against `probes/2026-08-08-filter-layout/`, which refuted r1's
bandwidth model of the masked scan (5–7× optimistic) and settled the addressing-structure
question §5 had reasoned about. §2, §5, §6, §8 and §9 are rewritten on the measured constants.
**Reads against:** architecture §4 (I2, I7, I9, I12), §8.2, §10.3–§10.5, Appendices A, C, D;
contracts §2.1–§2.4; per-point-attributes §2–§3; write-path §2, §4.2, §5; compaction §2–§4;
`probes/results.md`; `probes/2026-08-08-filter-layout/results.md`; design memo 2026-07-29
(secondary attribute indexing).

---

## 1. Recommendation

**The artefact of record for every filterable column is a flat, entity-indexed value column,
served by a scan driven by the pushed-down candidate mask.** Addressing is by position, never by
stored `(entity_id, value)` pairs — a bare array where every entity in the extent carries a
value, a Roaring **presence bitmap** with compactly-stored values where presence has holes
(**measured**, `probes/2026-08-08-filter-layout/`: pairs are never optimal on either axis, and a
presence bitmap costs 36 KB–125 MB per 10⁹ column against pairs' 4 GB). Categories additionally
get what is already
built: one keyed Roaring posting per `(column, code)`, now reclassified as a **derived
accelerator** over the flat column — base-only, rebuilt at the fold, never a second durable
identity. No other family gets an index by default, and no family gets a value dictionary,
because no family but a category has a value→identifier map to keep (the owner's position, and
this memo finds nothing that contradicts it).

| Family | Artefact of record | Accelerator | Library |
|---|---|---|---|
| Category | flat code column (`u8`/`u16`/`u32`) | keyed Roaring posting per code (built) — **required at broad coverage**, §6 | `croaring` 2, `arrow` 59 — both in tree |
| Boolean | flat `u8` column | the two postings, if ever measured hot | same |
| Numeric / timestamp | flat column at the declared type, native encoding | per-container zone maps or range-encoded BSI — one of the two is **required for broad ranges** (§6; §8 arms 5–6 decide which) | same; BSI is a few hundred lines over `croaring` (no mature Rust library exists — memo 2026-07-29 §11.4) |
| String | flat entity-indexed `utf8` column (offsets + bytes) | **none** | `arrow` 59; `memchr` (already in the lock) for substring |
| Multi-valued list | entity-indexed list column (offsets + values) | per-`(item, value)` postings for list-of-category (decision 0039's slow path) | same |

What this deletes from the two provisional documents: the per-column string/numeric dictionaries
and their promotion machinery, `max_distinct_values`, the FST-absorption pool job, the
order-preserving big-endian key, the level tree entire, BSI-as-default, per-flush attribute delta
tiers, and most of the filter-surface shared-projection cache with its two accepted timing
channels. §7 names the sections. What it keeps: the filter contract (§8.2) verbatim — operands
still return entity bitmaps and compose by intersection; only the storage behind an operand
changes — the `attrs/`-beside-`terms/` separation, the composition rules of filter-surface §5,
and the category machinery that is already built.

The scan's affordability is now **measured, and narrower than r1 modelled**: ~2.9 ns per
candidate entity contiguous, ~22 ns scattered, stable from 10⁶ to 10⁹ — so a 25%-coverage
principal at 10⁹ costs **730 ms**, not r1's modelled 40–80 ms, and the affordable region at a
50 ms operand budget is ~1.7×10⁷ visible entities contiguous, ~2.3×10⁶ scattered (§6). Two
consequences: the category-posting accelerator is **load-bearing for broad principals from the
start**, not a deferred escalation; and broad numeric ranges are blocked on §8's accelerator
arms before numerics promote. Every design has this corner — the superseded one had a 12.7 s
modelled projection there — but it is no longer rare.

---

## 2. The artefact of record, and why it survives attack

### 2.1 The shape

Entity IDs are dense, append-only, never reused (**I9**), and assigned once — but an extent's
entity range is **not gap-free**. write-path §4.2: a flush segment covers a contiguous ascending
range that is *ascending-with-holes* where deletes struck **or where a commit window interleaved
slices** (merge's adjacency test is `hi < lo`, not `hi + 1 == lo`, for exactly this reason). So
"the array index is the ID" holds only where presence is universal; under concurrent multi-slice
ingest a dense positional extent wastes up to (S−1)/S of its slots. The addressing structure
beside the value array is therefore a choice, and
`probes/2026-08-08-filter-layout/results.md` measured the four candidates at 10⁶–10⁸–10⁹
(**measured**, warm, in-memory, single-threaded; linear in *n* throughout, 9.5–11.2× per decade,
ranking stable across scale):

- **Presence universal → a bare value array**, entity ID as the index. Zero addressing bytes and
  fastest in every candidate shape (28.7 ms at 1% contiguous, 10⁹).
- **Presence partial → a Roaring presence bitmap, values stored compactly** (value *i* belongs
  to the *i*-th present entity). 36 KB per 10⁹ column for slice-blocked presence, 1.25 B per
  present entity when genuinely scattered — and the only layout that does not degrade on a broad
  candidate (161 ms against a run table's 2,873 ms, slice-blocked broad).
- **Explicit `(entity_id, value)` pairs — never optimal on either axis**, at any scale or
  presence shape measured. 4 B/entity *per column*: 16 filter columns at 10⁹ is 64 GB of
  redundancy against Appendix A's ~20 GB term-index budget.
- **Run tables `(start, len, base_rank)` are a trap**: 12 bytes for a whole 10⁹ column, and a
  binary-search rank per candidate entity collapses them on broad candidates (2.9–10.2 s at
  10⁹). Recorded so the storage column does not re-derive them.

The presence bitmap doubles as the **not-null** structure for numerics and strings (code 0 stays
the category absent sentinel, as ruled); it compresses with entity-space contiguity like every
posting does, and it is what `IS NULL` and the BSI escalation need anyway. Files are Arrow IPC,
uncompressed, mmapped — §10.3's rules; a flush appends one extent per its own entity range, the
discipline flush segments already follow; nothing is promoted, resolved, interned or renumbered.

### 2.2 What the query does

`resolve(op, candidates)` walks the candidate bitmap and reads the column only where the
candidate is set. **The cost is per candidate entity, and the constant is the candidate's shape,
never the data's or the value's** (**measured**, layout probe, stable 10⁶→10⁹):

- **~2.9 ns per candidate entity** where the candidate is contiguous — sequential value reads;
- **~22 ns per candidate entity** where it is scattered — a 7.6× cache-miss penalty on random
  access into the value array.

The scan is bound by per-candidate work and cache misses, **not by memory bandwidth**: the
effective rate at 25% contiguous coverage is ~1.4 GB/s, not the 5–10 GB/s r1 assumed, and any
sizing that treats the scan as a bandwidth problem is ~5–7× optimistic. r1's separate
"container-sequential versus per-bit gather" mode machinery dissolves into these two constants —
one loop over the candidate's set bits exhibits both, because contiguity is a property of the
mask. The result is built in ascending entity order (`add_many` on a sorted buffer), the bulk
construction §10.4 already prescribes.

Two properties fall out that the current design buys with machinery, or fails to buy at all:

- **Work is a function of `(mask, column)` and never of the value asked.** A hidden value, a
  nonexistent value, an empty range and a full range cost the same scan. per-point-attributes
  §3.8's "indistinguishable in outcome *and in work*" — which filter-surface §2.1 downgraded to a
  preference and §3.2 registered a channel against — holds **by construction** for every scanned
  family. The registered residual ("cold cost encodes the corpus-wide membership of a gated
  value") does not arise, because nothing corpus-wide is ever touched.
- **Aggregates are I2-clean for free.** Masked extrema, histograms, sums are computed inside the
  candidate during the same scan. filter-surface §5.3's range-summary machinery (masked
  level-tree descent at a modelled 24 ms per node) is replaced by "the scan computes it".

### 2.3 Lifecycle

- **Ingest**: append an extent at flush. No dictionary, no promotion, no discard-and-replan, no
  quadratic near-unique-string hazard (filter-index §5.2's whole subsection dissolves — appending
  strings to a flat column is O(bytes) whatever the cardinality).
- **Suppression**: nothing, as everywhere (Rule S) — the scan is driven by a mask the caller
  composed against the deny state, so a suppressed entity is never read.
- **Deletion**: nothing until the fold. The fold **blanks** deleted entities' slots (writes
  absent / clears the presence bit) in a sequential rewrite — and the reason is **retention, not
  decision 0050**. 0050's argument is a fail-open specific to `M_auth`: postings left standing
  become visible to everyone when the overlay entry retires. A filter column is not
  authorisation — the fold removes the entity from `M_auth`, so a masked scan never visits the
  slot again. What forces the blank is the asymmetry it would otherwise leave: after a fold a
  deleted item's *render* values are gone with its row, while its *filter* values would persist
  indefinitely, because the slot is positional and **I9** forbids renumbering it away. Data
  retained after deletion, on no rule — so the fold blanks, on its own grounds, without touching
  the two retirement routes (which must never be conflated — write-path §5.4). A 4 GB `u32`
  column re-copies sequentially in seconds (**modelled** at disk bandwidth); the fold also
  concatenates small extents then.
- **Accelerators are base-only and self-retiring.** A category's posting file (and a BSI, if ever
  built) covers `[0, fold_watermark)`; the region above the watermark — at most a fold interval's
  ingest — is answered by scanning the flat extents and unioning in. At the owner's stated rates
  (10²–10⁶ items/hour) a day's ingest is ≤ ~2.4×10⁷ entities, and the scan touches only the
  candidate's members among them: ≤ coverage × 2.4×10⁷ × 2.9–22 ns per operand — ~17 ms for a
  25% contiguous principal, ~130 ms for a 25% *scattered* one (**modelled** from the layout
  probe's measured constants; the scattered-broad shape is accelerator territory in the base
  too, §6). This **deletes per-flush attribute delta tiers** (filter-index §5.3),
  their write path, their per-tier range scan and the coalesce dependency — none of which is
  built, so the deletion costs nothing.

### 2.4 The attack, honestly

1. **Broad principals.** A 25%-coverage principal at 10⁹ costs **730 ms measured** on a bare
   column (2.5×10⁸ candidates × 2.9 ns) — 4× the entire measured selection budget (p99
   158–191 ms), and 5–7× worse than r1's refuted bandwidth model predicted. This is the attack
   that lands: the scan alone does not serve head principals, so the accelerator is not a
   deferred escalation but a required component wherever coverage can be broad — category
   postings (built) for equality and IN; §8's arms decide the numeric-range answer before
   numerics promote. Below ~1.7×10⁷ visible entities contiguous the scan is inside a 50 ms
   operand budget with room to spare (**measured**, §6).
2. **Scattered masks.** Signature-sorted contiguity is policy-dependent (**measured**: 8.9–36.7×
   posting compression for category-like policies, ~1.0× for surname-like). A scattered
   candidate pays the measured **~22 ns per candidate entity** — 7.6× the contiguous constant —
   so the affordable region shrinks to ~2.3×10⁶ visible entities at 50 ms. r1's assumed
   50–100 ns per random read was the right region; the measured figure supersedes it. Work still
   tracks the principal's own mask, never the value.
3. **Storage duplication.** A `render`+`filter` category stores its code twice — row space per
   slice, entity space once. That is 1× the render column, against the **measured** 0.31–1.01×
   of the membership postings alone; at 10⁹ a `u16` category costs 2 GB entity-space. Reported by
   the plan step (per-point-attributes §2.3), not refused.
4. **Long strings.** A scan reads value bytes; a column of paragraphs would be slow. That is
   #44's text operand, not a filter column; the plan step reports mean value length and warns.
5. **Provenance.** This is not a novel bet: it is decision 0008's own result — direct evaluation
   from the mask beat the precomputed candidate list, **by measurement**, on the selection path —
   applied in attribute space. The repo's cost model (O(containers touched)) and its
   highest-leverage property (entity-space contiguity) are exactly what a masked scan exploits.

**Verdict: adopt, on §9 escalation 1's conditions** (attack 1 makes the accelerator a
requirement at breadth, not a footnote). The conformance dividend seals it: filter-index §9 invented an optional
`(entity_id, column, value)` relation for the oracle because postings cannot answer
entity→value. The flat column **is** that relation, as the artefact of record rather than a side
output — the oracle reads it directly, and the differential stops depending on a re-emission the
fold could forget.

---

## 3. Per family (question A)

Query shapes: **selective** = an operand whose result is small (equality on a rare value, a tight
range); **broad** = result a large fraction of the candidate.

**Category.** Record: flat code column. Accelerator: the built keyed posting per code
(`write_filter_postings`, `ColumnPostings::open_keyed` — `croaring` 2 keyed records, binary
search). Selective: one search + one intersection, O(min containers) — microseconds to
milliseconds (**modelled** from the measured 114 ns–1.54 µs per container). Broad IN-list over k
values: k lookups, then either union-then-intersect or the candidate-driven probe (§4). Build:
the grouping pass built today; **measured** membership sets 0.31–1.01× the render column at
2.4×10⁶. Maintenance: base-only, rebuilt at fold (sequential sweep of the flat column); no tiers.
The keyed format stays — codes are scattered by mint (per-point-attributes §3.4), and the r3
argument that a positional file over a scattered code space is not a format holds.

**Boolean.** Flat `u8`. A posting pair is a degenerate category and may be derived if a
deployment measures a hot boolean; by default the scan is the route, at the §6 per-candidate
constants — **whether a `u8` column improves on the `u32`-measured constants is unmeasured**
(the layout probe's own caveat: narrower values cut bytes but not the per-candidate work, so
the scattered case should gain more than the contiguous one; §8 arm 2).

**Numeric / timestamp.** Flat column at the declared width, **native little-endian, compared
natively**. The order-preserving key (sign-flip / bit-invert, big-endian) is confirmed as an
artefact of byte-lexicographic key stores: a scan compares `f64` with `<`, and NaN falls out of
every range *for free* because IEEE comparisons with NaN are false — filter-index §3.3's
store-as-absent rule and its property test reduce to the language semantics. `timestamp_us`
stores as the `i64` contracts §2.2 already defines; the resolution declaration,
delta-from-minimum and span refusal (filter-index §3.2) are deleted — they existed only to fit a
64-bit key into an affordable slice count. Selective predicates: masked scan, one comparison per
candidate value, inside budget below §6's measured thresholds. Broad coverage is the measured
gap — 730 ms at 25% (§6) — and is what the accelerators exist for:
- **Zone maps** — min/max per Roaring-container-aligned block of 2¹⁶ entities, ~16 B × 15,259
  containers ≈ **244 KB per column at 10⁹** (arithmetic), rebuilt trivially at fold, ~100 lines.
  Strongest exactly where it matters: ingest-time timestamps are near-monotone in entity ID
  (batch-append allocation), so a time-range predicate skips almost everything. **Carries a
  timing channel** — §4 — that needs an owner disposition before adoption.
- **Range-encoded BSI** — the named escalation for broad ranges and masked min/max/sum when the
  scan measures too slow. ~33 slices ≈ **4 GB per `u32` column at 10⁹, modelled, never
  measured** (memo 2026-07-29 §6.6); derived, base-only, rebuilt at fold. Adopt only on §8's
  measurement. The **level tree is cut**: its rank-keyed node instability, mandatory per-fold
  upper-level rebuild, FST rank machinery and 1.25–30 GB modelled sizing spread all served range
  decomposition over per-value postings, and there are no per-value postings to decompose over.

**String.** Flat entity-indexed `utf8` (offsets + bytes; chunked extents keep offsets 32-bit).
Equality, IN, prefix (`starts_with`), and — restored — **substring** (`memchr::memmem` under the
mask), all by scan with verification against the stored bytes the column now provides. The #44
cut's premise ("a filter-only attribute has no stored value to verify a trigram superset
against") is void: `entity → value` is one array index. A trigram *accelerator* remains #44's to
adopt if substring at breadth ever measures hot — and note its shape honestly: a trigram
conjunction is corpus-wide before the mask enters (§4). **No dictionary, no FST, no
`max_distinct_values`, no listing surface, no value enumeration** — a string is row data
(filter-index §2.3's data-model argument survives and is now matched by the storage). A string
column that is really an enum should be declared a category; the plan step warns on sampled low
cardinality rather than the reverse warning r3 carried.

**Multi-valued lists.** Entity-indexed list column (offsets + values buffer). `contains(v)` by
scan; for list-of-category the per-`(item, value)` postings remain available as the accelerator
(the build's grouping loop running more than once per item, as r3 said — that part was right).
Decision 0039's fence is untouched: never `render`, and no derived hot column.

---

## 4. The `M_auth` push-down, precisely (question B)

§8.2's "the mask goes in first" is honoured *literally* only by structures whose work the
candidate bounds. Per structure:

| Structure | Push-down class | Work is a function of | Channel |
|---|---|---|---|
| Masked flat scan | **genuine pre-filter** | candidate entities × candidate shape (**measured**: 2.9 / 22 ns per entity) | none new — cost tracks the principal's own coverage, C14's accepted class |
| Keyed category posting, intersect | partial | min(candidate, posting) containers | a nonexistent code answers faster than a broad hidden one — see below |
| Candidate-driven posting probe | **pre-filter, probes equalised** | Θ(candidate containers) probes; per-container work still varies | residual named below |
| BSI (escalation) | **pre-filter** | k × candidate containers | value-independent (k is per-column, public schema) |
| Zone-map skip | **value-dependent pruning** | data distribution incl. invisible rows | timing distinguishes "no visible match" from "no match at all" per block — C4's shape, unregistered |
| Level tree (cut) | post-filter | corpus-wide node unions | one reason it is cut |
| Trigram conjunction (#44) | post-filter, then masked verify | corpus-wide trigram postings | must be priced when #44 lands |

Two of these need a sentence each.

**The category intersection channel, and its cheap narrowing.** `posting(v) ∧ M_auth` costs
O(min-containers): a code with no record returns instantly, a broad hidden value costs up to the
candidate's own container count. That is strictly narrower than filter-surface §3.2's registered
channel (bounded by the principal's *own* mask, not by the corpus). The **candidate-driven
probe** — iterate the candidate's containers and probe the posting per container — narrows it
further, and its claim must be stated exactly: it equalises the *number* of probes at
Θ(candidate containers) whatever the value, but **not the work inside each** — a failed lookup
is cheap and a hit does a real container intersection, so per-container time still varies with
the hidden value's overlap with the principal's own containers. The residual is a
second-order timing difference bounded by the candidate's container count, against the
superseded design's first-order channel proportional to the value's corpus-wide membership.
Recommended as the evaluation form for any `per_viewer` category, with that residual registered
rather than claimed away; the straightforward intersection is fine for `public` ones. On this
basis filter-surface §2.1's supersession of per-point-attributes §3.8 can be **reversed for the
scanned families** (outcome *and* work, by construction) and **nearly so for categories**, where
"in work" holds up to the residual above.

**Zone maps are the one structure here that trades a channel for speed.** The skip consults
unmasked extrema, so a block whose *visible* rows don't match scans slower than one where
*nothing* matches — timing reveals whether invisible rows in a block fall in the range. This is
I7's licensed shape (precomputed unmasked structure as a fast path with an exact fallback) for
correctness, and C4's shape (response time varies with corpus density including unauthorised
rows) for disclosure — but C4's row does not currently cover it and the register must, or the
structure must not ship. **Escalation 3 in §9.**

Row-space, unchanged in principle: an operand result is entity-space and ⊆ the candidate; per
tile it meets the composed mask exactly as filter-surface §5.1 rules (above composition, per
range, never folded into the base — that section is right and keeps). What changes is scale: with
the mask pushed in, the thing to project is `M_sel` itself, O(|M_sel|). The per-set-bit constant
is **not settled**: `probes/results.md` §6 measured 8.8 s for a 69.3×10⁶-entity mask (≈127
ns/bit at that cardinality), while architecture §10.4's 10.7 s at 10⁹ implies a several-fold
smaller constant at larger cardinality — the two are measured points that disagree per-bit, so
the constant is cardinality-dependent and derived figures carry that spread: ~1–13 ms at 10⁶,
~0.1–1.3 s at 10⁷ (**modelled** from those two measured points; filter-surface's flat "~127
ns/set bit" should not be quoted as a constant). A per-tile membership-test fallback for broad
results is the alternative, crossover measured by §8's arm 4. The elaborate shared-projection cache of
filter-surface §4 (two-axis admission, canonical-node identities, prefix digests, the eviction
channel) is **deferred, not rebuilt**: most of its machinery served unmasked shared operands and
level-tree nodes, and both are gone.

---

## 5. Table layout (question C)

**The question is not voided — it is answered, and differently per presence shape.** r1 claimed
no `entity_id` is stored anywhere because dense IDs make the array index the ID; that is
single-slice reasoning. write-path §4.2 makes an extent's range ascending-*with-holes* under
deletes and under commit windows that interleaved slices, so what a column stores beside its
values is an **addressing structure**, and the layout probe measured the candidates (§2.1;
**measured** at 10⁶–10⁹, ranking stable):

- **Presence universal within the extent → bare array**, zero addressing bytes, fastest
  everywhere. Legal for a single-slice deployment's build extent and for slice-pure flushes with
  no holes.
- **Presence partial → Roaring presence bitmap + compact values.** 36 KB per 10⁹ column
  slice-blocked, 1.25 B/present scattered; the only layout stable under a broad candidate.
- **Explicit `(entity_id, value)` pairs — refuted**: never optimal on either axis, and 4 B/entity
  per column (64 GB for 16 columns at 10⁹). The "shared entity column" framing of the question
  dies here: the pairs layout is the only one that *has* an entity column to share, and it loses
  to the presence bitmap even on storage once presence has any structure at all.
- **Run tables — refuted for this use**: 12 B per column but a rank binary-search per candidate
  entity, 2.9–10.2 s on broad candidates at 10⁹. Do not choose an addressing structure on its
  storage column.

Variable-width families keep an offsets buffer, rank-addressed through the same presence bitmap.
Deleted entities cost a cleared presence bit, not a burned value slot — the presence layout also
retires r1's "slots burned at column width forever" concession.

What remains of the grouping question, with the shared-entity-column motivation gone:

| Axis | Per-column files | One grouped file per extent |
|---|---|---|
| Scan paging | reads only the column | **identical** — Arrow buffers are column-contiguous, a one-column scan touches only its buffer's pages |
| Compact values under partial presence | natural — each column's value count is its own | **broken for a record batch** — Arrow batch columns must share a length, and per-column presence makes lengths differ; grouping works only for universal-presence fixed-width groups |
| Add a `filter` column later | one new file; others untouched | rewrite the group |
| Digest / first-touch | defer per column | first touch of any column digests the group |
| Ingest append | one small file per column per flush | one file per flush |

**Recommendation:** one file per column per extent as the default — partial presence makes
compact value arrays per-column-length, which a shared record batch cannot hold — with grouping
available for universal-presence fixed-width groups if extent-file count ever bothers anyone
(columns × extents is tens × tens, nowhere near contracts §2.4's inode objection). Category
postings stay one file per column as built. **No invariant or leak-register row is sensitive to
the choice**: r3's per-column-file argument was about shared *ordinal* address spaces colliding
under ingest (C11), and values addressed by entity position or pinned code have no address space
to collide. The `attrs/` versus `terms/` separation (filter-index §2.1) is unaffected and stays —
that argument is about descriptor collision with the auth dictionary, and it concerns the
category postings, which remain.

---

## 6. Where the scan stops being good enough (question D)

**Measured**, `probes/2026-08-08-filter-layout/` (`u32` column, warm, in-memory,
single-threaded; linear in *n* from 10⁶ to 10⁹ at 9.5–11.2× per decade, no cliffs). The unit is
**candidate entities**, not bytes: the constant is per-candidate and independent of the column's
total size, so r1's "bytes of column touched" framing is retired with the bandwidth model it
came from.

| Candidate | Cost, bare column at 10⁹ | Tag |
|---|---|---|
| per entity, contiguous | **~2.9 ns** (2.87–2.92 at 10⁹, 2.56 at 10⁸) | measured |
| per entity, scattered | **~22 ns** (22.1 at 10⁹, 24.1 at 10⁸) — 7.6× cache-miss penalty | measured |
| 10⁷ contiguous (1% principal) | 28.7 ms | measured |
| 2.5×10⁸ contiguous (25% principal) | **730 ms** | measured |
| 10⁷ scattered (1%, surname-shape policy) | 221 ms | measured |

Against the measured selection operating point (p99 158–191 ms at 10⁹) and a ~50 ms budget for
one operand, the affordability thresholds are:

| Candidate shape | Visible entities before 50 ms |
|---|---|
| Contiguous | **~1.7×10⁷** |
| Scattered | **~2.3×10⁶** |

Below those, the scan is inside budget with room to spare at every presence shape measured, and
no accelerator, cache or index earns anything. Above them sits one corner — the broad principal
— where the scan alone is 4× the whole selection budget and the accelerator must answer: the
built category postings for equality/IN (comparison unmeasured — §8 arm 1), and one of zone maps
or BSI for numeric ranges (arms 5–6). What contiguity buys is the 7.6× between the two rows, and
whether a principal gets it is policy-dependent as everywhere (**measured** 8.9–36.7× posting
compression category-like, ~1.0× surname-like, at 2.4×10⁶; the 10⁹ ratio is **modelled**).

Bounds on the measurement, from the probe's own caveats: value width other than `u32` unmeasured
(a `u8` should improve the scattered case more than the contiguous — **assumed**); strings
unmeasured; single-threaded (a parallel scan moves the absolute budget, not the layout ranking —
**assumed**); in-memory (the 2.9 ns constant does not transfer to a cold scan). The unfolded
extent scan (§2.3) adds ≤ ~17 ms contiguous / ~130 ms scattered at a day's ingest for a 25%
principal (**modelled** from the measured constants) and is not the binding term.

---

## 7. What this changes in the corpus (question F)

Pre-release, decision 0048: change, don't version. Artefacts are recreated.

**`filter-index.md` — rewrite around the flat column.**
- *Keep:* §1's frame (one artefact closes `per_viewer` as a consequence; slice-invariance); §2.1
  (separate files, untyped format core — still needed for category postings); §2.3's
  string-is-not-category data model and decision 0039 restatement; §2.5's keyed-format-for-
  scattered-codes argument and the built emit; §6's Rule S / Rule F table and §6.1's
  composed-verdict argument (unchanged in substance); §7 (slices); §9's crate placement and layer
  edges.
- *Cut:* §2.2's per-column ordinal-space rationale (no ordinals outside categories — the file
  layout survives on §5's grounds, not these); §2.4's string/numeric dictionary route and its
  decision-0042 extension; §3 entire (level tree, BSI-as-default, order-preserving key,
  big-endian, resolution declaration); §5.1–§5.2 (resolution at admission, promotion,
  `max_distinct_values`, the FST rebuild job); §5.3 (attribute delta tiers); §4's bit-sliced
  build terms and the 4 GB scatter floor.
- *Rewrite:* §4 (build = the flat emit plus the built category grouping; the streaming concern
  becomes "write extents in entity order", which the emit loop already does); §6.2 (fold =
  blank-and-rewrite flat columns + rebuild accelerators; the mandatory per-fold upper-level
  rebuild is gone with the tree); §8 (flat columns join the digest sweep at O(bytes); contracts
  deviation 9's first-touch deferral carries over unchanged); §9 (the conformance relation *is*
  the artefact).

**`filter-surface.md` — shrink.**
- *Keep:* §2 (the operand contract, empty-operand rule); §5 in full (composition above the
  composed mask, the two thresholds, count rules — nothing here depended on the storage); §6; §7.
- *Reverse:* §2.1's supersession of per-point-attributes §3.8 — "in outcome and in work" is
  restorable by construction for every scanned family, and for categories up to the
  per-container residual §4 names (which must be registered, not claimed away).
- *Cut or defer:* §3.1–§3.3 (the two-mode fork — push-down becomes the default and only mode
  pending measurement); §3.2's accepted timing channel and its register row (the channel does
  not arise; the owner ruling that accepted it should be revisited rather than silently carried);
  §4's cache (admission grid, canonical nodes, prefix digests, eviction channel) — deferred until
  §8's arms 1 and 4 show the broad corner needs it; the three-rung ladder stays *recorded* (its
  three measured rungs are real) for whatever projection cache eventually exists.
- §8's register table: rows 3 (range summaries → "computed by the masked scan"), 5 and 6
  (shared-cache channels) rewritten or dropped; a zone-map row added if that escalation is taken.

**Normative amendments owed:**
- **architecture §10.5 r21** — the routing rule reads "data read once per query belongs in an
  entity-space bitmap behind the filter contract". Amend to: *…belongs in entity space behind the
  filter contract (§8.2) — a keyed posting where the value is categorical, a flat entity-indexed
  column scanned under the candidate mask otherwise.* §8.2 itself is untouched.
- **architecture §8.3** "where each filter lives" gains the same sentence; Appendix A gains the
  flat-column rows (1 GB per byte-width per 10⁹, arithmetic).
- **per-point-attributes §2.1** — "level tree below ~10⁶, BSI above; report the choice" becomes
  "scan below §6's measured thresholds; a derived accelerator where coverage can be broad" (the
  report-don't-ask rule survives).
- **contracts §2.4** — gains the `attrs/` flat-column entry: extent layout, the presence-bitmap
  addressing rule of §5 (bare where presence is universal, Roaring presence + compact values
  where partial, pairs and run tables refused — the layout probe's result made format), digest
  rule; the anticipated attribute `dict_extents` counterpart is never added.
- Decisions 0042 and 0050 are untouched (auth dictionary; fold invalidation) — their attribute
  extensions simply never materialise, and 0050 is **not** the ground for the fold's blanking of
  filter slots (§2.3: that is retention, a distinct argument, so the two retirement routes stay
  unconflated).

**Contradiction to resolve rather than inherit:** memo 2026-07-29 §6.6 and per-point-attributes
§2.1 both carry the level-tree/BSI rule this memo cuts. Both are non-normative; the rewrite
should cite this memo's supersession explicitly so the rule is not re-derived from them.

---

## 8. The measurement, re-scoped (question G)

`probes/2026-08-08-filter-index-cardinality/` existed to fix a level-tree-versus-BSI crossover; it was never run and is deleted, its two surviving questions folded into `probes/2026-08-08-filter-layout/results.md`.
**That question is dead** — neither structure is the default, and one no longer exists in the
design.

**What `probes/2026-08-08-filter-layout/` has already settled**, and must not be re-run as an
open question: the masked-scan cost model (per-candidate constants, 2.9/22 ns, linear 10⁶→10⁹);
the addressing-structure choice (bare / presence bitmap; pairs and run tables refuted); the
affordability thresholds (~1.7×10⁷ contiguous, ~2.3×10⁶ scattered at 50 ms); and — by derivation
from the constants — the unfolded-extent scan bound r1 had as its own arm. r1's masked-scan
latency grid (its arm 1) is therefore closed for `u32`; what survives of it is the width and
string question the probe explicitly did not test.

What remains, in priority order:

| # | Arm | Decides |
|---|---|---|
| 1 | **The accelerator at the broad corner** — the built keyed category postings against the bare/presence scan at coverage {10%, 25%, 50%} × both mask shapes, equality and IN(k), k ∈ {1, 4, 16, 64}; plus the candidate-driven probe form and its per-container residual | the one comparison the layout probe deliberately did not run: whether postings close the measured 730 ms gap, at what coverage they overtake the scan, and what the probe form's equalisation costs |
| 2 | **Width and strings** — the layout probe's harness re-run at `u8`/`i64`, and a `utf8` arm at mean lengths {8, 32, 128} incl. substring via `memchr` | whether the 2.9/22 ns constants move with width (probe's stated caveat: scattered should gain more than contiguous — currently **assumed**); the string-scan budget |
| 3 | **Parallelism and cold** — the 25%-broad cell parallel across candidate containers, and once cold-mmap | whether the single-thread 730 ms is the real ceiling or 12 cores divide it; the probe says ranking is unaffected (**assumed**) and the budget moves — by how much decides how often arm 1's accelerator is reached for |
| 4 | **Row-space step** — project-the-result (O(\|M_sel\|)) vs per-tile membership test, against \|M_sel\| ∈ {10⁴…10⁸}; re-measure the per-set-bit projection constant, which the two existing measured points (8.8 s at 6.9×10⁷; 10.7 s at 10⁹) show is cardinality-dependent | whether any shared projection cache is needed at all |
| 5 | **BSI gate** — build one `u32` BSI at 10⁸: bytes (validates/refutes the modelled 4 GB per column at 10⁹ — never measured), masked range and min/max vs the scan | whether the numeric-range escalation is worth its bytes; broad numeric ranges are blocked on this or arm 6 |
| 6 | **Zone maps on a near-monotone column** — skip fraction and latency for time-range predicates on batch-ordered `ingested_at` | whether the structure is worth its channel; feeds the owner's C4 disposition |

**Report crossovers in candidate entities per shape** — the unit the measured constants are in.
Dead and not to be run: the tree/BSI crossover, FST-rank arms, the big-endian float-key
monotonicity gate (no key exists), upper-level sizing, the `u32` scan grid (measured), the
unfolded-extent arm (derived), and the projection extend/rebase entry-clone arm (deferred with
the cache).

---

## 9. Escalations — rulable as stated

1. **Adopt the flat entity-indexed column (bare / presence-bitmap addressing, §5) as the
   artefact of record for every family, categories included, with category postings a derived,
   base-only, fold-rebuilt accelerator.** *Re-examined against the refuted bandwidth model,
   because r1 recommended this partly on affordability arithmetic that was 5–7× optimistic.*
   **The recommendation survives, and the reason is that its grounds were mostly never
   bandwidth**: the lifecycle simplification (dictionaries, promotion and delta tiers deleted
   unbuilt), the value-independent-work property closing the §3.8 channel, the conformance
   dividend (the artefact *is* the oracle's relation), and the layout economics — which the
   probe **strengthened**: pairs refuted, presence bitmaps measured at 36 KB–125 MB per 10⁹
   column, and the scan measured fastest below the thresholds on the very layouts recommended.
   What the refutation does change is the accelerator's standing: it is **not an escalation for
   a rare corner but a required component wherever coverage can be broad** — a 25% principal is
   730 ms measured on the scan alone — so adoption is conditional on §8 arm 1 confirming the
   built postings close that gap, and broad numeric ranges stay unpromoted until arm 5 or 6
   answers. *Consequence of no (postings stay the record for categories):* the built code stands
   as-is, but categories keep a separate lifecycle needing per-flush tiers, and entity→value for
   filter-only categories needs the side relation after all. **Recommend yes, with the
   conditions named.**
2. **Defer the filter-surface §4 shared-projection cache and its two accepted-channel rulings
   until §8 arms 1 and 4 report.** *Yes:* the 2026-08-08 owner rulings accepting the cold-cost
   channel and the eviction channel are set aside as moot rather than carried; a broad×broad
   result may bring a narrower cache proposal back. *No:* the cache design is kept warm against
   measurements that may show it unnecessary. **Recommend yes.**
3. **Zone maps: accept the C4-shape timing channel (block-granular, reveals whether invisible
   rows fall in a queried range) in exchange for near-free time-range predicates, or decline the
   structure.** *Accept:* register a new Appendix C row; adopt after arm 6. *Decline:* broad
   time ranges then depend on the BSI gate (arm 5), since the measured scan does not serve them
   at breadth. No recommendation until arms 5–6 run; the base design does not depend on the
   choice, but broad numeric ranges now do depend on one of the two.
