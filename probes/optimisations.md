# Optimisations and design decisions from Phase 0

What the measurements imply for what gets built. `dataset.md` describes
the corpus, `results.md` holds the evidence, `phase0-memo.md` is the
go/rework/stop verdict; this document is the engineering distillation.

Each entry is labelled **DECIDED** (measured, argued, ready to build),
**PROPOSED** (measured but touching the design documents, so it needs
independent review first), or **DEFERRED** (available, but not yet —
with the condition that would trigger it).

Each also states **what layer it touches**, because the stakes differ by
orders of magnitude and it is easy to file a bundle-size win next to a
latency win and imply they matter equally:

| Layer | What it costs if you get it wrong |
|---|---|
| *serving* | authorise or retrieve latency — the budget the design exists to defend |
| *session memory* | masks × concurrent sessions, the binding 10⁹ constraint |
| *bundle / build* | artifact size and rebuild time; off both request paths |

**Only optimisations that change the shape of the postings the mask
build reads touch serving.** Entity ordering (§2.1) and small-term
representation (§2.2) do. Pair-relation encoding (§2.4) and
factorisation (§2.3) do not — the pair relation is read at build time,
not at authorise time, once §1.1 reassigns the semi-join.

---

## 0. One cost model explains most of it

**Bitmap operations cost O(containers touched), not O(cardinality).**
Almost every headline in `results.md` is a corollary:

- A term with 100k postings spans ~15,259 containers when scattered over
  a 10⁹ universe and ~2 when contiguous — a ~7,500× difference in merge
  work for *identical* cardinality.
- Mask union at 25% coverage costs 21.7 ms or 2,885 ms depending only on
  posting shape, for the same 125 MB mask (§4.2).
- Viewport counting is flat at 0.1–0.3 ms across every zoom level,
  because the mask has only ~15,000 containers and any set of ranges is
  bounded by one traversal (§6).
- Dictionary scale is free on the authorise path: 117M terms costs the
  same as 10M, because union reads granted postings, not the vocabulary
  (§4.3).

Two practical consequences. **Contiguity in entity space is the single
highest-leverage property in the index** — everything in §2 below is a
way of buying it. And **any benchmark that varies cardinality while
holding container structure constant will mislead**; the ratios that
matter come from varying shape.

---

## 1. The mask-build kernel

### 1.1 Union of per-term postings is the authorise path — **DECIDED** *(serving)*

The pair-relation semi-join is **build-cadence machinery and the
differential oracle**, not the authorise budget. This reassigns plan
§4.2, which named the semi-join as the budget: measured, it trips its
own multi-second criterion at scale (3.9–10.3 s for w=10⁴ at 250M) while
the postings union stays at 588 ms for the realistic worst case at 10⁹.

*Lands in:* `tessera-authz`. *Evidence:* results §4.1, §4.2.

### 1.2 The kernel needs two algorithms, not one — **DECIDED** *(serving)*

Roaring n-way union costs O(terms × containers spanned); concatenating
sorted posting arrays, radix sorting and bulk-constructing costs O(total
postings). Neither dominates — the crossover moved by 400× across the
configs measured. For many small scattered terms (the surnames
`random w=10⁴` shape: 308k postings, 93.5 ms by union) concatenation
should win outright.

The concatenate path is **already specified elsewhere in the design** —
§10.4 prescribes exactly that construction for the permutation. This is
the same primitive in a second place.

*Choose on:* **container density** — concatenate-and-sort when a granted
term's postings average few per container (Roaring pays per-container
dispatch for a handful of elements), Roaring union when containers are
dense (its fixed 8 KB bitmap pass is bandwidth-efficient). Measured, the
per-container constant differs by **13×** between those regimes
(114 ns sparse vs 1.54 µs dense). The crossover is essentially Roaring's
own array/bitmap threshold.
*Lands in:* `tessera-authz`, Phase 2. *Evidence:* results §4.2.

### 1.3 Probe-to-dictionary ratio stays in lookup territory — **DECIDED**

≤0.03 in every realistic scenario, so per-term lookups win under
Druid's 0.12 heuristic; no sorted-merge machinery is needed at these
vocabularies. *Evidence:* results §4.1.

---

## 2. Index representation and entity ordering

This section is where the cost model cashes out. All three items buy the
same thing — contiguity in entity space — by different means, and they
compose.

### 2.1 Signature-sorted entity allocation, in the Phase 1 allocator — **DECIDED** *(serving + session memory)*

§11.1 already prescribes assigning entity IDs sorted by term signature
within each ingest batch. Two independent measurements say how much it
is worth:

| | Gain |
|---|---|
| Posting storage | **8.9×** (categories) to **36.7×** (t=100) |
| Cached session fragment | **6×** to **1,614×** |
| Mask union at equal coverage | up to **130×** |

And it is *not* free from arrival order: created-order entity IDs give
run lengths of 1.00–1.26 against a 1.000 random baseline, i.e. nothing.

**This must be in the Phase 1 allocator, not added later.** I9 makes
entity-ID assignment permanent, so shipping created-order in the walking
skeleton locks in an uncompressed index until a full re-allocation
rebuild. It is a small amount of code with a large, measured payoff and
a hard deadline.

*Lands in:* `tessera-lifecycle` (allocator), Phase 1.
*Evidence:* results §2, §4.2, §4.4.

### 2.2 Small terms as sorted int32 arrays, not Roaring — **DECIDED** *(serving + session memory)*

§6.2 already says this; measurement makes it load-bearing rather than an
optimisation. At 117M terms, **34.4% are singletons** and the median
posting is 3 entries. As a Roaring bitmap per term that is ~29 GB of
container and object overhead alone; as CSR it is 17.5 GB of postings
plus 0.94 GB of offsets — inside Appendix A's ~20 GB budget.

ClickHouse's threshold of 32 is the calibration point to start from.

*Lands in:* `tessera-authz` postings reader, bundle format.
*Evidence:* results §4.3.

### 2.3 Factor the pair relation by signature — **DEFERRED** *(bundle / build)*

> **Status: DEFERRED** *(independently reviewed 2026-07-27 against contracts r3 §0/§2.4 and design r16 §11.2/§2.5)*. Rejected as a bundle-format change now: the surviving benefit is bundle bytes and rebuild time on an artifact off both request paths; the saving is unmeasured and policy-conditional (surnames measured 1.54M signatures over 2.42M items, where factoring degrades to explicit pairs); and the mechanism adds a derived-representation consistency chain — postings ↔ signature map ↔ signature dictionary, oracle decode, signature-ID allocation across flushes, compaction fold-away — with no second reader to justify it under contracts §0.1. The capability argument does not hold at the bundle level: overlay entries already carry term sets inline (§11.2), and §2.5's node term-distribution belongs to the Phase 3 nodes/labels artifacts (contracts §2.9) or to an engine-local in-memory map, which is out of contract and may be built without review. **Adopt only if**, at a real deployment: (a) the real-label rerun measures `pairs.arrow` under §2.4's encoding as the dominant bundle cost at that deployment's terms-per-item *and* its measured signature count is ≪ its entity count (order 1% or less); or (b) the Phase 3 term-distribution artifact design concludes it needs a bundle-level signature dictionary — i.e. a second reader actually materialises. Either trigger reopens this entry for independent review before the format changes.

*Original proposal, retained for the record:*

**Scope correction.** An earlier draft of this entry claimed a large
win; most of that was double-counting §2.1. The pair relation is read at
*build* time — to construct postings, and by the DuckDB oracle — not at
authorise time, so factoring it saves bundle bytes and rebuild time, not
query cost. The serving-path way to exploit signature structure is
§2.1 (ordering makes postings runs) and §4 (row layout makes counting
range-based); this adds little on top of them.

The mechanism: store `entity → signature_id` plus
`signature_id → term list`, so `|entities| × t` becomes
`|entities| + |signatures| × t`. At 10⁷ entities, ~130 terms each and
~10⁴ distinct signatures that is ~45 MB against ~10 GB of explicit
pairs — a real saving at high *t*, on an artifact that is otherwise the
largest thing in the bundle.

The narrower argument that survives: a `signature → term set` dictionary
is a compact answer to *"what terms does this item carry"*, which the
overlay needs (§11.2 keeps term sets inline in overlay entries) and
which §2.5's node term-distribution capability needs. That is a
capability argument, not a performance one.

*Cost:* a predicate change moves an item between groups. Degrades to
explicit pairs when signatures are near-unique. *Unreviewed, and it
changes the bundle format.* *Evidence:* results §3, §7; the saving
itself has not been measured — it is derivable from the `hiterms`
overlap sweep.

### 2.4 Pair relations: sorted, delta-encoded — **DECIDED** *(bundle / build)*

Sorting by `(term_id, entity_id)` and encoding with
`DELTA_BINARY_PACKED` gives **3.8× smaller and 3× faster to read** than
unsorted. Sorting alone buys nothing — it is the combination. Consumers
also get to skip a per-batch sort.

Applies to the bundle's `pairs.arrow` as well as to the test corpus,
but note the stakes: after §1.1 that file is read to rebuild postings
and by the oracle, **not on either request path**. This is bundle size
and rebuild time.

*Lands in:* build pipeline. *Evidence:* dataset §4.4; measured directly.

---

## 3. The query path

### 3.1 Direct evaluation is the main selection route — **DECIDED** *(serving)*

Candidate lists serve only the dense cores of high-coverage principals.
At working coverages, 12–99% of occupied depth-6 tiles fall below the
~5% crossover; for tail-only principals essentially every tile does.
r14 §7.2 already made the per-tile choice; this supplies the duty cycle.

**Corollary with teeth:** deleting the exact path "to simplify" would
not degrade sparse principals' maps, it would blank them — tippecanoe's
empty-tile failure, for exactly the users least able to report it.

*Lands in:* `tessera-spatial`, Phase 2. *Evidence:* results §5.

### 3.2 The *projected mask* must stay cached — **DECIDED** *(serving)*

Permuting a 69M-item mask from entity to row space costs **8.8 s** at
10⁹. §10.4 already requires caching it per *(token, slice, pin)*; the
number makes the consequence concrete — if it ever drifts onto the
per-viewport path, the system is dead. Worth a comment at the call site,
not just a line in a document.

**What is cached is the projected mask**, per *(token, slice, pin)* — not
`permutation.bin`, which is read once per session and never per viewport.
The earlier heading said "the permutation", which reads as the file.
Clarified 2026-07-29; no decision changes.

*Evidence:* results §6.

### 3.3 Tile → *set* of ranges in the interfaces — **DECIDED**

§11.3's multi-segment reality forces this regardless, and it is what
keeps the signature-aligned row layout (§4 below) available later
without a serving-core rewrite. Costs nothing now.

*Lands in:* tile table and count path, Phase 1.

### 3.4 Counting is free at every zoom — **no action, but stop worrying**

Whole-viewport exact masked counting is 0.1–0.3 ms at 10⁹ *including
depth 0*, where one call counts a billion rows in 191 µs. §13.3's
concern about zoomed-out overviews is about mask materialisation and
shard fan-out, not counting. *Evidence:* results §6.

### 3.5 The gather was never measured — **PROBE OWED** *(serving)*

Phase 0 measured every bitmap primitive and no column read. The whole
selection path downstream of `range_cardinality` is therefore modelled,
not observed, and it is where §4's retrieval argument lives.

Two distinct reads, and conflating them understates the second:

| Read | Rows touched | Sensitive to row clustering? |
|---|---|---|
| Output gather (x/y/scalars) | *k* ≈ 30 per tile | Barely — sampled rows scatter regardless |
| **Priority read under direct evaluation** | **every visible row in the tile range** | **Yes — this is the one** |

*Probe:* priority-column reads over a tile-sized row range, scattered
vs signature-clustered visible set, swept over coverage
(0.01%–25%) and tile depth; alongside the output gather as a control.
Pre-Phase-2, cheap, and it decides whether §4 has a retrieval case at
all or only a mask-projection one.

---

## 4. Row-space signature-major layout — **DEFERRED**

Sorting rows by (signature, morton) for the largest signature groups,
with a Morton-only residual. Top 500 groups cover 82.4% of the corpus;
the knee is K ≈ 250–1,000 aligned groups, implying a size threshold of
~0.02–0.05% of corpus.

**The key is the signature — the item's whole term set — not a single
term.** Items carry ~130 terms each (results §7), so a term-major layout
is not a partition of row space: it requires either duplicating each row
~130× in geometry or nominating a "primary" term. Duplication is fatal
independently of cost, because a masked count stops being the
cardinality of a bitmap and starts needing a dedup — which is the
property I2's aggregates rest on. A signature *is* a partition (each
item has exactly one), so the layout is a permutation of rows, and it is
what makes the whole-group visibility shortcut available at all.

### 4.1 What it buys

1. **Mask projection into row space.** The measured 8.8 s to project a
   69M-item mask (results §6) is dominated by container count. Cluster
   the authorised rows and the projected bitmap collapses from ~15,000
   containers toward runs: cheaper to build, smaller to cache per
   *(token, slice, pin)*, cheaper for every subsequent range operation.
   This is the strongest leg.
2. **The whole-group visibility shortcut** — every item in a signature
   group is visible to exactly the same principals, so a group can be
   admitted or skipped without consulting the mask. Invariant-bearing;
   see below.
3. **Permutation encodability.** Today `permutation.bin` is a flat
   `u32 × bound` array (~4 GB/slice at 10⁹) and is left uncompressed for
   a *deliberate* reason, not an incidental one: §11.1 forbids assigning
   entity IDs in Morton order (leak C6), so entity order and row order
   are unrelated by construction and the values are a maximum-entropy
   permutation — delta-coding buys ~17% (log₂(n!)/n ≈ 26.6 bits vs 32)
   on a structure already off the per-viewport path. Signature-major
   row layout breaks that: entity IDs are *already* signature-sorted
   (§2.1, permanent under I9), so `entity_to_row` becomes near-monotone
   within each group — long increasing runs, the regime where
   Elias-Fano or delta-plus-bitpacking wins outright, and it cuts the
   8.8 s projection at the same time. **The permutation's
   incompressibility is a consequence of the current layout, not a
   property of permutations.** Not an independent option; it arrives
   with §4 or not at all.
4. **The priority gather under direct evaluation** — see §4.2. Real,
   but unmeasured.

### 4.2 What it does *not* buy, and one thing that is unmeasured

**It does not help the output gather.** Per viewport that is a few
hundred tiles × *k*≈30 marks, columnar, ~10 pages per column per tile
(design §10.4). The sample is the *k* lowest-**priority** items and
priority is a hash of the entity ID — uncorrelated with everything — so
the sampled rows are scattered within a tile under any layout.
Signature-major clusters *authorised* rows, not *sampled* rows, and it
makes this read slightly worse by fragmenting each tile into K
sub-ranges.

**It plausibly does help the priority read, which is the larger one.**
Direct evaluation — the *main* route at working coverages (§3.1) —
reads the priority column for **every visible row in the tile's range**,
not for *k* rows. At 1% coverage of a 266k-row depth-6 tile that is
~2,700 scattered reads over the tile's priority block. That read scales
with visible count and is exactly what clustering would make contiguous.

**Neither read was measured in Phase 0.** results §1–8 cover mask build,
n-way union, `range_cardinality`, autocorrelation and per-item breadth;
there is no gather measurement anywhere. The probe that would settle
§4's retrieval argument is in §3.5.

### 4.3 Ruled out, so they are not re-derived

- **Priority as a major sort key at some tile depth.** The ordering is
  already `(morton-prefix-to-leaf, priority)` — design §5.2 puts
  priority in place of deeper Morton bits below leaf depth, and §10.4
  stores columns that way. That does give a contiguous prefix read at
  *leaf* depth. Moving priority outside the Morton prefix at any
  coarser depth *d* makes depth-*d* tiles contiguous at the cost of
  every depth below *d*, and `tiles_for_bbox` needs contiguity at every
  zoom. One depth or all depths; not both.
- **Build-time hierarchical LOD levels** (Potree/Cesium style: assign
  each point the coarsest depth at which it makes the top-*k*, order by
  `(level, morton)`, so the visible set at depth *d* is a prefix). This
  is the correct answer in an unmasked system and is **closed to this
  one by invariant, not by cost**: the top-*k* is computed unmasked at
  build, which is "derived from the full dataset and then gated" — the
  I2 shape §7.2 opens by rejecting. The system's build-time LOD
  structure already exists and is already correctly bounded: the
  candidate list, which yields *k* survivors only above coverage 1/*c*.

### 4.4 Why not now

The whole-group visibility shortcut is invariant-bearing (sound only
while a group is untouched by overlay and live set), so it wants the
conformance suite watching. Its costs are real — per-tile segment
fan-out at ~6–8× the design's budget, a group-aware merge policy, and
predicate changes becoming physical row moves. It also degrades to
nothing on author-like policy (1.54M signatures over 2.42M items), so it
is a per-deployment build decision, not a core commitment.

**Trigger** *(aligned to plan §14, 2026-07-29)*. Design r18 retired the
real-label rerun permanently — no real access-labelled corpus is
available to this project — so the signature histogram is **deployment
guidance**, not a gate this project can pass. The two live gates are:

1. **A working conformance suite** (Phase 2, plan §10.1). The
   whole-group visibility shortcut is sound only while a group is
   untouched by the overlay and the live set — invariant-bearing, and
   exactly the class of change that passes every functional test while
   leaking.
2. **The gather probe** (§3.5, re-scoped to large *k* by the drawn-mark
   budget spec's P4). Phase 0 measured no column read at all, so the
   retrieval half of the case is modelled.

A deployment that *does* have real labels re-runs the Phase 0
measurements and checks its own signature histogram for the knee before
enabling the layout. *Evidence:* results §3, §6; scaling analysis §5.3.
**Open decision:** plan §14.

---

## 5. The policy boundary

### 5.1 Where each optimisation belongs

**The plugin controls how many terms an item has; the core controls how
cheaply it stores whatever it is given.** That keeps §6.1's boundary
intact — the core never learns policy semantics — while leaving it free
to exploit *statistical* structure (repeated term sets, contiguity)
that needs no semantics at all.

Plugin-side, because they need meaning: minting synthetic terms for
subexpressions; term subsumption (if satisfying A implies B, B is
redundant on any item carrying A).

### 5.2 Prefer auth-side breadth to item-side breadth — **DECIDED (guidance)**

Minting is a conservation law: it trades terms-per-item against
satisfied-terms-per-token. Both ends are now measured, and they are not
symmetric — the auth side is cheap (w=10⁴ builds in 236–588 ms), the
item side is expensive in storage and linear (~130 pairs/item is ~200 GB
at 10⁹). So plugins should sit toward the auth-heavy end. This is
evidence, not preference.

### 5.3 Drop exclusion as the per-item cap's response — **PROPOSED**

Full argument in `phase0-memo.md` action 4. In brief: a predicate is a
disjunction, so more terms means *broader* intended visibility, and
exclusion answers "visible to many" with "visible to none" — a resource
guard producing an authorisation-shaped outcome. Nothing in
I2/I3/I5/I7/I13 depends on terms-per-item, and mask build is unaffected
in kind at 130 terms/item.

Keep instead a declared bound as a **sizing hint** (§6.1's actual
purpose) and a runaway guard at 10⁵–10⁶ that **warns** rather than
excludes.

*Not an argument that high-*t* is normal.* It is expected to be rare;
the term-based optimisations stay and remain conditional by design. The
principle is **performance degrades with data shape; availability does
not.**

*Touches §6.1, §6.2, §16 — review before revising the design.*

### 5.4 What not to do

Any scheme that stores a subset of an item's terms and evaluates the
rest at query time. It reintroduces the array-containment formulation
measured at ~3,000× slower, and it puts term evaluation back onto the
retrieve path, which is currently clean of it.

---

## 6. Fold back into the design documents

Each of these is a companion-document change these measurements imply:

1. **Plan §4.2** — the semi-join is not the authorise budget (§1.1).
2. **Plan §4.1** — the "% over cap" kill criterion measures a naive
   normaliser's failure rate, not the system's; under minting it is
   close to vacuous (memo action 4).
3. **Design §6.1/§6.2/§16** — the cap proposal (§5.3), and the phantom
   64-term constant: the design makes the cap plugin-declared while two
   companions cite "§6.2's sixty-four-term cap" as if specified.
4. **Design §16** — spatial autocorrelation is now measured (bracket
   1.00 to 5.11); §11.1's compression baseline is recorded.
5. **Architecture §4.1** — frozen mask buffers need 32-byte alignment,
   a bundle-format requirement.
6. **Design §7.2** — record the direct-vs-candidate duty cycle (§3.1).
