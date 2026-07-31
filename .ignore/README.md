# Tessera

A permission-masked point service: an interactive, pannable and zoomable map over a large document corpus, where **what a viewer may see determines not just which items they retrieve, but every count, density, cluster and summary they are shown**.

*A tessera is a single tile of a mosaic, and — in Rome — a token presented to be recognised and admitted. Both readings apply: the unit of storage is a tile, the unit of access is a token, and every viewer assembles a different mosaic from the same tiles without any of them seeing the whole picture.*

---

## The one-paragraph version

Every surveyed system with per-document security permits aggregates over records the viewer cannot read — documented as a limitation by one vendor, shipped as a feature by another, and demonstrable through query plans in a third. The field draws its line at retrieval and lets everything derived leak past it. Tessera moves the line: a viewer's visible set is materialised once per session as a Roaring bitmap, and every spatial query, count, density, sample and label decision is computed from that set alone. Geometry is stored in Morton order so a quadtree tile is a contiguous row-ID range, which makes exact masked counts bitmap arithmetic rather than a scan. The cost of a query scales with screen area, not corpus size.

**It answers how many, where, whether, and which examples — over exactly what a given viewer may see.** It is a counting engine, not a general aggregation engine, and it is an index rather than a database.

---

## Documents, in reading order

**1. `tessera-architecture-design.md` (r23) — the specification.** What the system guarantees. Start with §2.6, which walks a request end to end and points at the section governing each step; then §4, the thirteen invariants. Appendix C is the leak register, Appendix D the prior art, Appendix E a reference authorisation plugin, Appendix H the general framing and its boundary.

**2. `tessera-implementation-plan.md` — how it gets built.** Language and runtime decisions, the dependency register, six phases ordered by risk retirement rather than by architecture, the conformance suite mapped to invariants, and effort sizing. **Phase 0 can kill the architecture and takes two to four weeks. Nothing else should start until it finishes.**

**2a. `tessera-system-architecture.md` (r5) — the shape of the built system.** Backend component architecture: processes and planes, the crate decomposition, the five contracts, the ingest/compaction lifecycle, configuration and packaging. Sixteen recorded decisions; its Appendix R holds the review trail. Written after and governed by the specification.

**2b. `tessera-contracts-spec.md` (r8) — the byte level.** Schema- and byte-precise definitions of the four interchange contracts: bundle format, service API, plugin ABI, wire. Its organising rule — a contract exists only where a second reader exists — and its §0.3 deviations govern where it and the system architecture's sketches differ.

**2c. `tessera-concurrency-lifecycle.md` (r4) and `tessera-conformance-design.md` (r3) — the mechanism level.** The first: generations, pins, the three retirement rules (deletion / suppression / predicate-fold — they differ, and conflating them is fail-open), the WAL, merge-versus-snapshot, the router/worker protocol. The second: the harness that makes the invariants enforceable — the definitions-oracle, canonicalised canary comparison, the byte-scanner with positive controls, and eight scripted interleavings. Both carry Appendix R review records.

**3. `tessera-visualisation-architecture.md` — the client.** Two deployment profiles, GPU and thin-client, over one data contract and one interaction model. Organised around reuse: what exists, its licence, and what remains to be written. §8 records the rejected alternatives, several of which will be proposed again.

**4. `tessera-scaling-analysis.md` — analysis, not specification.** Residency tiers, scaling to 10¹⁰–10¹², and permission-signature partitioning. Everything in it is conditional on measurements nobody has taken; it leads with its assumptions so the numbers are arguable rather than asserted. Models in `analysis-models/`.

**5. `prior-art-*.md` — the survey.** Four domain reviews plus a synthesis, with primary sources. The body of evidence behind "no existing technology can replace this build". Read the synthesis; go to the domain reviews when you want the citation.

---

## What is settled and what is not

**Settled.** The core data model: entity space for permissions, row space for geometry, related by an explicit permutation. Morton ranking so tiles are contiguous ranges. Roaring masks built once per authorisation and reused. Priority-based level of detail that nests across zoom and composes across partitions. Containment-gated label serving. The two-mask split so filters narrow points without dissolving the map. Compartmented partitions with required-set gating. Rust serving core **and Rust build pipeline — one engine, two modes** (system architecture D4; this line read "Python build pipeline", which D4 superseded: Python is a first-class *consumer* — SDK, supervisor, the test-only reference oracle — and never a component).

**Measured on the synthetic 10⁹ corpus** (`probes/` — dataset, results, optimisations, memo; verdict **go**, including the lower-scale runs). The real-label rerun is retired by owner decision (design r18) — no real corpus is available to this project — with its caveat converted to deployment guidance: re-run Phase 0 against real labels before trusting the policy-dependent headlines in any deployment that has them. The DNF-expansion risk is retired twice over: plugin-side minting keeps terms-per-item linear at any nesting depth, and the term cap's exclusion behaviour is dropped entirely (design r16) — bounds warn, never exclude, and authorise cost is unchanged in shape at ~130 terms/item. Masks do **not** cluster under Morton order (run ratio 1.03–1.15 realistic): direct evaluation is the main selection route, with measured duty cycles, and there is no hidden spatial upside. Permission signatures collapse gradually, not cliff-wise — top 500 groups cover 82% on category-like policy, none of it on author-like — so signature-aligned layout stays a per-deployment decision. The headline mechanism throughout: bitmap cost is O(containers touched), not O(cardinality), and signature-sorted entity allocation (8.9–36.7× compression measured under both orderings; the ~130× on union is a ceiling on what contiguity is worth, measured *between label configurations* rather than between orderings) must ship in the Phase 1 allocator because I9 makes it permanent. **Read design §11.1 (r23) before quoting either figure**: the sort's scope is one batch and nothing repairs it, so the win is collected only in proportion to how much of the corpus arrives in large batches; and because the probe corpus assigns entity IDs in created order, every published union *timing* is already an un-banked measurement — multiplying one by a decay factor double-counts.

**Open.** Sharded index placement. Retroactive revocation across temporal slices. Whether label gating uses the prompt sample or full cluster membership — a security decision, not an implementation detail. The full list is §16 of the design.

---

## Day one

Phase 0, and it is deliberately not code. Two measurements plus one histogram, all in Python, all against **real** predicates and real grant sets rather than synthetic data, because all three are questions about the distribution of your actual labels:

1. **DNF expansion factor** — terms per item after normalisation; median, p99, max, and the fraction overflowing the cap.
2. **Mask characteristics** — build time for a 10⁴-grant authorisation over the exploded pair relation, mask cardinality and size, and the spatial autocorrelation of the mask under Morton order.
3. **Permission-signature histogram** — hash each item's term set, count distinct, plot the group sizes. Nearly free, and it decides whether the largest available optimisation is on the table.

The deliverable is a short memo with the numbers and an explicit go / rework / stop. If it says rework, the design changes before Phase 1 starts.

---

## Two things not to lose

**The conformance suite is the deliverable.** The performance architecture is attractive and separable, and a partial implementation that keeps the Morton and Roaring machinery while quietly dropping I2, I7 or I13 passes every functional test while leaking through cluster existence and density. Section 10.2 of the plan maps each invariant to how it is enforced or tested; two of those tests are enforced by the type system and two are byte-level assertions rather than behavioural checks.

**The narrow query surface is a safety property, not a stage to grow out of.** Appendix C can be exhaustive because the retrieval surface is about five shapes. A general expression endpoint cannot be enumerated that way, and the prior-art survey is a catalogue of systems whose generality is precisely where they leak. New capability enters through the filter contract in §8.2 — order-independent set producers composed by intersection — so that expressiveness never reaches the authorisation layer.
