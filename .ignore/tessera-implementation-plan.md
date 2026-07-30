# Tessera — Implementation Plan

**Companion to** `tessera-architecture-design.md` (r18) and `tessera-system-architecture.md` (r4), which — with the contracts spec, lifecycle design and conformance design — carries the component decomposition, contracts and mechanisms this plan's phases build. That document specifies *what* the system guarantees; this one specifies *how it gets built, in what order, and what would make us stop*.

**Chosen stack:** Rust throughout the engine — serving core *and* build pipeline, one binary with a batch mode. Python is a first-class packaged consumer (SDK, supervisor) and the language of the test-only reference oracle, never a component. (Amended from "Python build pipeline"; the argument is in 2.2.)

---

## 1. How to read this alongside the design

The design document is written for a reviewer asking "is this correct?". This one is written for two other readers: the engineer asking "what do I do on Monday", and whoever is funding this asking "when do we know if it works".

Three things follow from that split. Section numbers of the form §n refer to the design document, not to this one. Nothing here restates a design decision — where this document mentions an invariant it is to say how the invariant is *enforced or tested*, not to re-argue it. And the phase ordering is driven by risk retirement rather than by the design's section ordering, so the two documents deliberately do not run in parallel.

The single most important structural point: **Phase 0 can kill the architecture, and it is cheap.** Nothing else should start until it finishes.

---

## 2. Language and runtime decisions

### 2.1 Rust for the serving core

The decisive argument is not throughput, it is the shape of the trusted computing base. The query path is a gather loop over memory-mapped columnar data, driven by row IDs derived from a bitmap intersection. A buffer over-read in that loop discloses exactly the bytes the entire design exists to protect, and it is the class of defect that passes every functional test and every code review. C++ gives the same libraries — CRoaring and Arrow are both native there — and gives up the one guarantee that matters most in this specific loop.

Rust also makes **I4 a compile-time property rather than a discipline**. Entity IDs and row IDs are distinct newtypes over distinct integer widths (`EntityId(u64)`, `RowId(u32)`) with no arithmetic or `From` conversions between them; the only path from one to the other is through an explicit, slice-versioned permutation object. Every place the design says "permissions in entity space, geometry in row space" becomes a type error if violated. This is worth more than it sounds — I4 is the invariant most likely to be broken by a well-meaning optimisation six months in, and it is the one a reviewer is least likely to catch.

The cost of Rust is the absence of Lucene, discussed in 2.4.

### 2.2 The build pipeline is the engine in batch mode

An earlier revision of this plan put the build pipeline in Python, arguing interoperability with UMAP, HDBSCAN and Toponymy. That argument dissolves on inspection: those tools belong to the *caller's* model pipeline, which is out of scope per §2.1 of the design — the build step only consumes their outputs, which arrive as Parquet, which `arrow-rs` reads natively. What a Python build pipeline actually costs was surfaced during the system architecture reviews: a second tiler that must agree bit-for-bit with the serving engine's streaming tiler, a second WASM plugin host running `terms_of_label` — a divergence hazard on the I5-critical path in its own right — and a second interner and allocator to coordinate.

So the build pipeline is `tessera build`: the same engine, batch mode, composing the same tiler, plugin host, interner and allocator that serving uses. One implementation of each. The Python wheel's `tessera.build()` drives this binary; `pyroaring`/PyArrow byte-compatibility remains relevant to the *reference oracle* (10.3), which stays Python deliberately — test-only, and independently derived is the point.

The boundary rule survives intact and gets simpler to state: **Python drives, never implements.** No Python in any request path, in artifact production, or in the trusted computing base.

### 2.3 WebAssembly for the authorisation plugin

The plugin boundary (§6.1) accepts caller-supplied code into a security-critical path, and the design's contract requires that code be **deterministic**. Both problems have the same answer: compile the plugin to WebAssembly and run it under `wasmtime` with no WASI capabilities granted.

Determinism stops being a documented obligation the caller might violate and becomes a property of the execution environment — no clock, no filesystem, no network, no address-space randomness, nothing to be non-deterministic *with*. Sandboxing means a defective plugin cannot corrupt the mask layer or read memory belonging to other tenants. And the module hash is a natural plugin version identifier, which §6.1 needs for two different cache keys anyway.

The performance objection does not survive contact with the cost model. `terms_of_auth` runs once per authorisation, where a few microseconds against a mask build measured in hundreds of milliseconds is invisible. `terms_of_label` runs per item, but only at ingest, which is batch: at roughly 1–10 µs per call, 10⁹ items is single-digit hours single-threaded and well under an hour across cores — acceptable for a full reindex, which §6.1 already establishes is the expensive path.

The fallback, if a caller's policy engine cannot target WASM, is an out-of-process plugin over a pipe. Compiling caller code directly into the server binary should be reserved for the reference plugin and for tests.

### 2.4 What we lose by not using Java, stated fairly

Lucene solves the segment lifecycle properly and we will be reimplementing a subset of it: `TieredMergePolicy`'s parameter set and its separation of natural, forced and deletes-driven merges; `BPReorderingMergePolicy`'s decorator shape, which turns a scheduled Morton re-rank into a continuous property of sufficiently large merges; and `SearcherLifetimeManager` with `SnapshotDeletionPolicy`, which is precisely I11's pinning, already hardened.

Estimated cost of reimplementation: 3,000–5,000 lines of subtle concurrent code, and the bugs will be in merge-versus-snapshot races rather than anywhere interesting.

We accept that cost because the alternative is adopting a whole runtime to obtain a merge policy — Lucene's query layer, its codecs and its scoring are all unused here — while giving up memory safety in the gather loop and adding GC to a sub-millisecond path. **Read Lucene's implementations before writing ours.** The design is the right one; only the language is different.

This decision should be revisited if the team turns out to be a JVM shop with existing Lucene expertise. It is a genuine trade, not a rout.

---

## 3. What we do not build

Every dependency here replaces work we would otherwise do. Licences are stated because one of them has already been a decision point.

| Component | Choice | Licence | Replaces |
|---|---|---|---|
| Bitmap kernel | CRoaring via `croaring` crate / `pyroaring` | Apache-2.0 | The entire mask layer |
| Columnar storage | `arrow-rs` + `memmap2`; PyArrow in the SDK and reference oracle | Apache-2.0 | On-disk format, zero-copy gather |
| Renderer, tiles, picking, labels (GPU profile) | deck.gl | MIT | WebGL scatterplot, tile lifecycle, GPU picking, collision-filtered labels |
| Tile grid, pan-as-transform (thin-client profile) | Leaflet or OpenLayers | BSD-2 | Slippy-map machinery with no GPU dependency |
| Transport / decode | `apache-arrow` (JS) | Apache-2.0 | Client-side columnar decode |
| Policy evaluation behind `terms_of_auth` | OPA (partial evaluation) or AWS Cedar | Apache-2.0 | Policy language, residual-DNF compilation |
| Plugin sandbox | `wasmtime` | Apache-2.0 | Determinism and isolation for caller code |
| Clustering and labelling pipeline | UMAP, HDBSCAN, Toponymy | BSD-3 / BSD-3 / see repo | Out of scope per §2.1 |
| Label grammar | `accumulo-access` ABNF, reimplemented natively | Apache-2.0 | Predicate syntax design |
| **Test-only** — label oracle | `accumulo-access` on the JVM | Apache-2.0 | The only check on **I5**'s label half |
| **Test-only** — mask oracle | DuckDB over the pair table | MIT | An independent mask-build implementation |

**Verify before committing.** The `croaring` crate must expose the frozen view family (`frozen_serialize` / `frozen_view`); the pure-Rust `roaring` crate does not, and frozen views are the whole mask-loading story. If that binding has regressed, the fallback is a thin FFI shim over CRoaring directly, which is a day of work, not a redesign.

**Explicitly rejected.** deepscatter is CC-BY-NC-SA — NonCommercial turns on the character of the use rather than on whether anything is sold, and ShareAlike would force any release to carry a licence that is not open source by the OSI definition. Separately, its quadfeather tiler assigns points to tiles in fill order with no per-point priority, which is exactly the mechanism the design replaces, so the half that makes it architecturally close is the half that cannot be used. Borrow the manifest-with-per-tile-ranges shape and the sidecar-column split as patterns, not as code.

---

## 4. Phase 0 — Two measurements that can kill the design

**Duration:** 2–4 weeks. **People:** one engineer plus whoever owns the real access labels. **Language:** Python throughout. **No serving code is written in this phase.**

Both experiments consume real predicates and real grant sets. Synthetic data cannot answer either question, because both are questions about the distribution of your actual labels.

### 4.1 DNF expansion factor

Run `terms_of_label` over the real corpus and record the distribution of terms per item after normalisation. Fontoura et al. (SIGMOD 2010) measured DNF normalisation becoming infeasible beyond nesting depth 2 and exceeding available RAM at depth 3; §6.2's sixty-four-term cap with default-deny overflow is the right *shape*, but its adequacy is entirely unmeasured and it is the highest-risk assumption in the design.

Report: median, p99 and max terms per item; the fraction of items exceeding the cap and therefore landing in the overflow list; total distinct terms; and the nesting depth distribution of the source predicates.

**Also hash the term set.** While the DNF pass has each item's term set in hand, hash it into a **permission signature**, count distinct signatures, and plot the group-size distribution. This is nearly free — the term sets are already computed — and it is the highest leverage-per-effort measurement anywhere in this plan, because one histogram answers three otherwise separate questions: whether permission-aligned partitioning is available (a signature group is wholly visible or wholly invisible to any principal, which collapses masks from bitmaps into runs); how large masks will actually be; and, since it is the same root cause, most of the spatial-autocorrelation question in 4.2. If signatures are near-unique per item none of it is available and the design proceeds as specified — so this is a search for upside, not a risk check.

**Kill criteria.** If more than a low single-digit percentage of items overflow the cap, the overflow list stops being an exception and becomes an availability problem — a meaningful share of the corpus is invisible to everyone. If total distinct terms exceeds the design's assumed 10⁵–10⁶ by an order of magnitude, the term index sizing in Appendix A is wrong and mask build cost has to be re-derived. Either result means the term-index architecture needs rework *before* anything is built on it, which is the entire point of doing this first.

*Post-measurement note (2026-07-27).* Both criteria are superseded by the Phase 0 results (probes/): the cap-with-exclusion was dropped in design r16 (exclusion answered breadth with invisibility; measured authorise cost is unchanged in shape at ~130 terms/item), making the "% over cap" criterion a measure of a naive normaliser, near-vacuous under plugin-side minting; and dictionary scale was measured free on the authorise path to 117M terms — two orders past the assumption — with the pressure landing on index *storage* (CSR + small-term arrays, within budget) instead.

### 4.2 Mask characteristics under Morton order

Build real masks from real grant sets with `pyroaring` and measure four things.

*Build time* for a 10⁴-grant authorisation, **against an exploded `(entity_id, term_id)` pair table using a hash semi-join** — not array containment over a per-item list column, which measures roughly three orders of magnitude slower and is the formulation everyone writes first (§6.3). This is the `authorise` stage's budget; if it is seconds rather than hundreds of milliseconds, the two-stage split still works but the caller's token refresh policy has to change. Build the pair table in this phase too and record its size: two integers per (item, term) at roughly ten terms per item sets a real storage line item at 10⁹.

*Mask cardinality and serialised size*, which sets the per-session memory cost and therefore how many concurrent sessions a node holds.

*Spatial autocorrelation under Morton order* — the §16 open question, and the one most likely to embarrass the design. Sort entities into Morton order, then measure the mean run length of set bits in the mask against a random-permutation baseline. The candidate-list fast path assumes authorised items cluster spatially; if the ratio is near 1, authorised items are scattered uniformly, tile ranges are sparse everywhere, and every query falls through to descent.

*Probe-to-dictionary ratio*, to choose the mask-build strategy. Druid switches between per-term lookups and a single sorted merge at a ratio of about 0.12; 10⁴ grants against 10⁶ terms is 1% so lookups win clearly, but against 10⁵ terms it is 10% and sits right at the boundary. Measure rather than assume.

**Kill criteria.** A run-length ratio near 1 does not kill the design but does kill the candidate-list optimisation, which makes descent the hot path and changes the latency budget materially — that must be known before Phase 2 is scoped. A 10⁴-grant mask build taking longer than a few seconds means the mask-build kernel needs its own optimisation phase.

*Post-measurement note (2026-07-27).* The semi-join named above as "the authorise stage's budget" is not: measured, it trips the multi-second criterion at scale while the postings union stays at 588 ms for the realistic worst case at 10⁹ — so the union is the authorise path and the semi-join is build-cadence machinery and the DuckDB oracle's input (probes, results §4.1; optimisations §1.1). The run-length ratio measured essentially 1 for realistic principals (1.03–1.15), so direct evaluation is the main selection route with candidate lists serving only head principals' dense cores — the Phase 2 scoping consequence this criterion existed to surface.

### 4.3 Deliverable

A short memo with the numbers and an explicit go / rework / stop recommendation. If it says rework, the design document gets revised before Phase 1 starts. This memo is the most valuable artifact in the plan relative to its cost.

---

## 5. Phase 1 — Walking skeleton

**Duration:** 6–10 weeks. **Data:** the Phase 0 corpus (`probes/dataset.md`) — **10⁹ points is the day-one target, not a later checkpoint** (owner decision, 2026-07-28). The corpus exists at 10⁹ with seven label configurations, its measurements are 10⁹ measurements, and the design's claims are 10⁹ claims; the entity-prefix property gives the 250k / 2.4M / 250M scales from the *same artifact* for iteration speed, so smaller scales are a debugging convenience, never a milestone. An earlier revision targeted 10⁷ synthetic points because no larger corpus existed; it does now.

Scope is deliberately narrow: one temporal slice, one partition, no filters, no labels, no sampling beyond a placeholder. The goal is one end-to-end path proving the latency budget at full scale, not a feature.

Build, in order: the tiler in Rust (Morton sort, row-ID assignment, Arrow segment write, manifest with per-tile row-ID ranges, sidecar column split — one implementation, exercised by `tessera build` now and shared with streaming ingest later); the entity-ID allocator **with signature-sorted assignment within batches from day one** — §11.1's ordering is measured at 8.9–36.7× posting compression and up to 130× on union cost (probes, results §2, §4.2, §4.4), and I9 makes assignment permanent, so shipping created-order in the skeleton locks in an uncompressed index until a full rebuild; the write-ahead log and its ack contract, which belong in the walking skeleton because durability semantics are load-bearing for deletions (system architecture §6.2); the Rust segment loader with mmap and frozen-view mask loading; the term index and mask build; the viewport query resolving a tile to a contiguous row-ID range, intersecting with the mask, and gathering; the opaque per-session handle allocator (I10); and a frontend fetching tiles from the service, built on whichever profile applies (see the visualisation architecture).

**Exit criteria.** Server-side: p99 viewport latency under 10 ms **against the 10⁹ corpus with a real mask applied**, and zero entity IDs observable in any wire payload — the latter tested, not asserted (see 10.2). Client-side criteria (a pan across the served corpus at interactive frame rates for the GPU profile; transform-only panning for the thin-client profile) apply when the frontend lands — the backend is being built first, and a minimal test client suffices for Phase 1 so long as the server-side criteria are measured over the real wire format.

The placeholder sampler here should be deliberately naive and obviously wrong (first *k* in range), so that nobody mistakes it for the real thing and so that Phase 2's differential test has something to disagree with.

---

## 6. Phase 2 — The sampler and the conformance suite

**Duration:** 8–12 weeks. **People:** two to three engineers.

These are one phase because the sampler is where I7 gets violated by a plausible-looking optimisation, and because a sampler without a differential test against its own definition is not evidence of anything. The history here is instructive: the first sampling scheme proposed for this design was confidently defended and provably wrong, and only a counterexample settled it.

Build the hash-derived per-point priority, then both evaluation routes for the selection definition (§7.2): direct evaluation from the mask, and precomputed candidate lists for high-coverage principals, with the per-tile crossover driven by the masked count the count step already produces. Then build the conformance suite in section 10.

**Direct evaluation is not the fallback and must not be treated as one.** Tippecanoe's multiplier clusters are the closest published analogue and they degrade to empty tiles once a mask's pass rate drops below roughly 1/N. Candidate lists have the same shape: a list of width *c·k* only yields *k* survivors above coverage 1/*c*, which at *c*=4 is 25% and describes almost no realistic principal. Direct evaluation from the mask is what makes the selection exact, it is bounded by the tile's priority block, and it gets *cheaper* as coverage falls — so below roughly 5% coverage it is also the faster route. Deleting it to "simplify" reintroduces tippecanoe's failure mode silently, for exactly the users least able to report it. This warrants a comment in the source, not just a line in a document. The scaling analysis carries the numbers.

**Phase 2 also lands the serve-time publication package, as one unit** (owner-confirmed 2026-07-28, from Phase 1 planning): streaming flush, side-manifest publication, the immediate-publication rule for deny-disposition changes (contracts §2.3), and the `readyz` freshness gate. These are one mechanism split across three contract clauses — implementing flush without the deny-publication rule is precisely the replica fail-open the contracts forbid — so none of the four ships without the others. Phase 1 defers all four together: its bundle is build-published only and no replicas exist, and its WAL restart-replay test covers the single-node deny story meanwhile.

**Exit criteria.** The sampler agrees with its brute-force definition on every generated case; nesting across zoom holds under adversarial masks; and the full invariant suite runs in CI.

---

## 7. Phase 3 — Labels and containment gating

**Duration:** 4–6 weeks.

Generating-set storage, the containment test against `M_auth`, the fallback ladder to coarser ancestors, and the label-regeneration notification back to the caller.

**One decision must be made explicitly and recorded, not defaulted.** Toponymy builds its prompts from *sampled* documents, so "every item the label was generated from is visible" is a check over a small recorded sample set rather than over whole cluster membership. Gating on the prompt sample is cheap and leaks less about cluster size; gating on full membership protects more. They differ materially and the choice is a security decision, not an implementation detail. **Decided (design r17): the prompt sample**, recorded in the bundle manifest's provenance; Phase 3 implements that, with full-membership declaration remaining a per-deployment strict mode.

Watch for **label creep**, which is the named and predicted failure mode of any label lattice: if clusters form on semantic similarity alone, the intersection of readers over a cluster collapses toward empty at 10⁷ and above, and most labels become unservable to almost everyone. Per-(cluster, term) generating sets already mitigate this by construction. If the mitigation proves insufficient, the escalation is ACL-aligned clustering — cluster by permission-equivalence class first, then semantically — which is a change to the caller's pipeline and therefore needs lead time.

---

## 8. Phase 4 — Filters and the two-mask model

**Duration:** 6–10 weeks.

`M_sel = M_auth ∧ filters`, the filter contract as order-independent set producers, sidecar indices for text and vector similarity, and the frontier behaviour under filtering.

The security-relevant property is small and easy to state: a sparse `M_sel` is sent in full rather than sampled, labels are gated on `M_auth` and never on `M_sel` (I3), and the frontier may move up but never down (I12). All three are cheap to test and each is a plausible refactoring casualty.

---

## 9. Phase 5 — Partitions and slices

**Duration:** 8–12 weeks.

Compartmented partitions with required-set derivation, the ALL gate, per-partition metadata, the token's reachable-partition set, and cross-partition query composition. Then multiple temporal slices with per-slice permutations against slice-independent tokens.

I13 is the subtle one and deserves its own attention: the natural implementation — check only the partitions actually queried — serves labels it should withhold. The test in 10.2 is written before the feature.

---

## 10. Testing

### 10.1 The suite is the deliverable

If this system is ever released or handed to another team, the conformance suite is what makes that responsible. The performance architecture is attractive and separable; a partial implementation that keeps the Morton and Roaring machinery and quietly drops I2, I7 or I13 passes every functional test while leaking through cluster existence and density. The tests are the only thing that makes those invariants enforceable rather than aspirational.

### 10.2 Invariant conformance matrix

| Invariant | How it is enforced or tested |
|---|---|
| I1 | Differential: compose the mask over overlay and watermark, compare against direct evaluation of every entity, on randomised overlay states |
| I2 | **Canary items.** Inject unauthorised items at extreme coordinates and assert no aggregate — centroid, hull, count, density, frontier depth — moves by any amount |
| I3 | Property: for random labels and masks, served iff generating set ⊆ `M_auth`; separately assert no cache sits above the check |
| I4 | **Compile-time**, via distinct newtypes with no conversion except the versioned permutation |
| I5 | Property-based against an independent reference implementation of the policy; unverifiable in general, so this is the only defence (§6.1) |
| I6 | Architectural: no network or filesystem capability in the authorise path; tested by running the plugin sandbox with all capabilities denied |
| I7 | Differential: sampler output versus the brute-force definition over the visible set, under adversarial masks including very sparse ones |
| I8 | Generating sets stored content-addressed and immutable; mutation attempts are a type error |
| I9 | Fuzz the allocator over interleaved add/delete; assert monotonicity and no reuse |
| I10 | **Byte scan.** Serialise payloads for known entity IDs and assert no encoding of those IDs appears anywhere in the output bytes |
| I11 | Assert a row-space mask applied across a compaction boundary is *rejected*, not silently applied to arbitrary rows |
| I12 | Property: frontier depth under any filter is never greater than depth without it |
| I13 | Make a partition unreachable and assert the label is withheld rather than treated as vacuously satisfied |

### 10.3 Other testing

Differential testing against a deliberately slow, obviously-correct reference implementation in Python — brute-force scans, no bitmaps — is the highest-value harness in the project and should exist from Phase 1.

Two further oracles come off the shelf, and both are **test-only**. For label semantics, `accumulo-access` is a spec-backed, zero-dependency implementation of exactly the access-expression grammar Appendix E adopts: generate random expressions and authorisation sets, evaluate in both it and the native parser, assert agreement. That is the only mechanism in the project that attacks **I5**'s label half, since nothing else can. For the mask build, DuckDB over the pair table produces an independently-derived bitmap to compare against the Rust kernel's — worth having because a wrong mask is a disclosure bug rather than a wrong answer.

Neither ships. The JVM that `accumulo-access` needs and the DuckDB dependency both live in CI and never in a deployed process; if either appears in the serving binary's dependency graph, something has gone wrong.

Seven lifecycle tests join the suite from the system architecture and lifecycle-design reviews (their Appendix R sections; concrete scripts in the conformance design §5), all attacking fail-open change handling. Five from the lifecycle design, including a fold-variant of the epoch-regression test (a predicate-change fold retires its overlay entry; a pre-fold-epoch fragment insertion must be refused — the r3 retirement-floor generalisation). Four of the five: a **suppression persistence test** — suppressed items stay invisible through every fragment refresh and compaction until explicit unsuppress (suppressions never touch postings, so no rebuild may be treated as covering them); an **epoch-regression test** — after a deletion's deny entry retires, a pinned request missing the fragment cache must rebuild from current postings and still exclude the item; a **post-snapshot tombstone test** — a deletion accepted during compaction survives the fold; and a **positional CRC test** — WAL corruption below the last fsync point fails recovery closed rather than truncating acked denies. And the original two, both attacking fail-open deletion: a **restart-replay test** — crash the process after acking deletions and suppressions, restart, and assert every deny-disposition overlay entry survives, since an unpersisted overlay silently re-exposes suppressed items; and a **deny-retirement window test** — delete an item, hold a pre-deletion mask-fragment epoch live on a token, run compaction, and assert the item stays invisible until no servable fragment epoch predates the deletion (the monotone epoch advance can never remove an entity, so early retirement of the deny entry is the leak).

Beyond that: `proptest` for the invariant properties, `criterion` for the latency budget with regression gates in CI, and fuzzing on the wire decoder, the manifest parser and the plugin boundary.

---

## 11. Sizing and staffing

| Phase | Elapsed | Engineers |
|---|---|---|
| 0 — Measurements | 2–4 weeks | 1 |
| 1 — Walking skeleton | 6–10 weeks | 2 |
| 2 — Sampler and conformance | 8–12 weeks | 2–3 |
| 3 — Labels | 4–6 weeks | 2 |
| 4 — Filters | 6–10 weeks | 2 |
| 5 — Partitions and slices | 8–12 weeks | 2–3 |
| Hardening to 10⁸ | 8–12 weeks | 2–3 |

Roughly **10–15 months elapsed with two to three engineers** to something you would put real data behind. Code volume is plausibly 18,000–30,000 lines of Rust for the engine — serving core and batch build alike, since both are one binary (2.2) — a thin Python SDK and supervisor, a Python reference oracle sized by the definitions rather than the algorithms, and a few thousand lines of frontend integration.

These are estimates from component counts, not from a plan anyone has executed. Treat the ordering and the dependencies as the useful content; treat the durations as a starting point for negotiation.

---

## 12. Scale checkpoints

An earlier revision said "prove 10⁸ before anyone says 10⁹ out loud" — written when no 10⁹ corpus existed. It does now, and Phase 1 serves it from day one (§5), so the claim nobody in the field has demonstrated — 10⁹ *identifiable, filterable, labelled* points (Falcon and Mosaic aggregate to bins; deepscatter renders a static unmasked catalogue; systems claiming 10⁹ rasterise, discarding the thing this design exists to deliver) — is tested continuously rather than approached by stages. Beyond 10⁹ the placement changes but the query algorithm does not — sharding, mask residency and the cardinality ceiling are worked through in the scaling analysis, which should be read before any commitment above 10⁹ is made.

Three things are expected to break first, and each has a known direction of fix: the Morton sort stops fitting in memory (external radix sort over disk-backed chunks); the row-ID space exhausts u32 per slice (per-segment row IDs with a segment-major addressing scheme, which the permutation already accommodates); and mask memory per session becomes the binding constraint on node capacity (frozen views over shared mmap rather than per-session copies).

---

## 13. Risks

The three that determine whether this works are all in Phase 0 or Phase 2. **DNF expansion** is the first, and the whole reason Phase 0 exists. **Mask spatial autocorrelation** is the second: it governs how many pages a tile's direct evaluation touches, how large masks are per session, and — via the permission-signature histogram — whether permission-aligned partitioning is available at all. **Sampler correctness** is the third, and the project's own history is the argument — the first scheme was wrong and only a counterexample established it, so no sampler ships without a differential test.

Below those: I5 is unverifiable by the service, so a plugin bug is a silent authorisation bug and the property test is the only net; label creep may make most labels unservable at scale; the segment lifecycle reimplementation will produce merge-versus-snapshot races that are hard to reproduce; and deck.gl's `TileLayer` has not been verified against a non-geographic `OrthographicView`, which is the assumption most of the renderer choice rests on.

One non-technical risk worth recording. Three well-resourced teams built an embedding-map product and exited: Arize Phoenix removed its embedding visualisation entirely in v13.0.0 while the project itself thrived, Lilac was archived about sixteen months after the Databricks acquisition, and Aquarium Learning wound down. The differentiator here is not the scatterplot. It is the access control — which is also the only part nobody else has built, and therefore the only part that justifies the build.

---

## 14. Decisions deferred

Of the items this section once deferred, two are now settled: the mask-build tier is **in-process Rust** — the postings union measured at 588 ms for the realistic worst case at 10⁹ makes an out-of-process round trip pointless (probes, results §4.2), retiring the `pg_roaringbitmap` alternative; and generating sets gate on the **prompt sample** (design r17, recorded in manifest provenance). Still genuinely deferred: sharded index placement (§13.3 — now with a measured lean toward entity-space construction plus exchange, decided at 10¹⁰ scale); the small-term threshold, where ClickHouse's battle-tested constant of 32 is the contracts spec's default pending Phase 1 calibration; and **row-space signature-major layout**, below.

**Row-space signature-major layout (probes, optimisations §4) — needs deciding, not before Phase 2.** Sorting rows by (signature, morton) for the largest signature groups with a Morton-only residual. It is a *per-deployment build decision*, but it is listed here because two of its consequences reach the core and one of them is a format change.

*What is settled.* The key is the signature, never a single term — items carry ~130 terms each, so term-major would duplicate geometry rows and cost I2 the property that a masked count is a bitmap cardinality. Priority cannot be promoted above the Morton prefix to recover contiguity: that buys one tile depth at the cost of every depth below it. Build-time hierarchical LOD levels are closed by invariant, not cost — the top-*k* would be computed unmasked, which is the I2 shape §7.2 opens by rejecting.

*What decides it.* Three inputs, none of which exist yet:
1. **A working conformance suite** (Phase 2, §10.1). The whole-group visibility shortcut is sound only while a group is untouched by the overlay and the live set — invariant-bearing, and exactly the class of change that passes every functional test while leaking.
2. **The gather probe** (probes, optimisations §3.5). Phase 0 measured no column read at all, so the retrieval half of the case is modelled. The read that matters is the *priority* column under direct evaluation, which touches every visible row in a tile range rather than *k*.
3. **A real-label signature histogram showing the knee.** Unavailable to this project (design r18) and therefore deployment guidance; on the synthetic corpus the top 500 groups cover 82.4%, while author-like policy (1.54M signatures over 2.42M items) degrades it to nothing.

*What it touches if adopted.* Per-tile range fan-out at ~6–8× the design's budget — already absorbed, since Phase 1 types a tile as a **set** of ranges (§5, contracts §2.6); a group-aware merge policy; predicate changes becoming physical row moves; and `permutation.bin`'s encoding. That last is the format consequence: the permutation is a flat uncompressed `u32` array because §11.1 deliberately keeps entity order and row order unrelated — spending the entity ordering on term-signature grouping rather than on geometry *(design r22; this parenthesis previously read "leak C6", which is the argument r21 relaxed and r22 removes from load-bearing duty — the ordering is unrelated because it is **spent**, not because a gap on the wire would disclose anything)* — making the values maximum-entropy. Signature-major breaks that — entity IDs are already signature-sorted under I9, so `entity_to_row` becomes near-monotone within a group and Elias-Fano-class encoding becomes worth having. **Phase 1 must not assume the permutation's representation beyond the contracts spec's reader interface**, or this arrives as a bundle-format break rather than a build flag.

---

## Appendix A — Package manifest

**Rust serving core.** `croaring` (CRoaring FFI, frozen views), `arrow` (arrow-rs), `memmap2`, `rayon` for the parallel mask build, `tokio` with `axum` or `tonic`, `wasmtime` for the plugin sandbox, `rustc-hash` for the term dictionary, `parking_lot`, `serde` with `postcard` for manifests, `tracing` for observability. Dev: `proptest`, `criterion`, `cargo-fuzz`.

**Test-only.** `accumulo-access` (JVM, invoked from CI) as the label-semantics oracle; DuckDB as the mask-build oracle. Neither belongs in a deployed dependency graph. `accumulo-access` is at `1.0.0-beta3` with no GA release, which is acceptable for a test oracle and would not be for a shipped dependency.

**Python (SDK and reference oracle).** `pyarrow` for the SDK's Table returns; `pyroaring`, `numpy` and `polars` in the reference oracle and test fixtures; `hypothesis` for plugin property tests. The clustering pipeline — UMAP, HDBSCAN, Toponymy — is the caller's and out of scope per §2.1; the engine's batch mode consumes its Parquet outputs (2.2).

**Frontend.** deck.gl (MIT) for the GPU profile, Leaflet or OpenLayers (BSD-2) for the thin-client profile, `apache-arrow` for decode, over a shared headless core carrying opaque handles rather than entity IDs. See the visualisation architecture document.

## Appendix B — Repository layout

A single workspace. The language rule: Python drives, never implements (2.2). The authoritative crate map is the system architecture document's §3; the sketch here shows the shape only.

```
crates/        Rust engine: types, plugin, authz, store, spatial, labels, filter,
               lifecycle, engine, wire, server, build, cli — one binary, serve
               and batch modes (system architecture §3)
python/        the wheel: SDK, supervisor, tessera.build()/tessera.serve() wrappers
reference/     deliberately slow, obviously correct Python — the differential oracle
conformance/   the invariant suite (section 10.2)
  oracles/     accumulo-access (JVM) and DuckDB harnesses — CI only, never shipped
frontend/      shared headless core + per-profile renderer shells
```

`reference/` and `conformance/` are not test utilities filed out of the way. They are the two directories that make the rest of it defensible.
