# Tessera — Conformance Suite Design

**Status:** Draft r4 — audited against the built suite; §0 and §4.6 state the measured position, and every unbuilt mechanism is marked at the claim (Appendix R)

**Owns:** the design of `conformance/` and `reference/` — harness architecture, oracle interfaces, fixtures, the invariant matrix's concrete test forms, the interleaving machinery, and what "pass" means. The implementation plan (§10.1) is blunt that the suite is the deliverable; this document exists so it is designed, not accreted.

**Two principles.** *Test the served surface*: spawn the real binary, build real bundles, speak the real API; internal access only where interleaving control requires it. *Invent no framework*: cargo test/proptest + pytest, with two pieces of bespoke machinery — the pause-point-and-command harness (§5) and the byte-scanner (§4.3), each reviewable by reading.

**One build caveat, stated rather than fudged:** wire-shape and byte-scan tests are specified to run against a **feature-free build** and interleaving tests against a `conformance` build. The two-binary split is a documented limitation, not an accident.

> **⊘ Specified, not implemented.** There is no `conformance` cargo feature; every test runs against one ordinary build. The split is an obligation on the stage that builds the interleaving harness, not a property of the suite today.

`I1`…`I13` are the invariants of design §4; each is restated where the matrix in §4.6 uses it. `M_auth` is a viewer's authorised visible set.

---

## 0. What exists

The suite is designed here and built in part. Both halves matter to a reader deciding what the system's evidence is worth, so the measured position leads.

| | Designed | Built |
|---|---|---|
| Directory layout | `conformance/{fixtures,differential,invariants,lifecycle,oracles,compile-fail}` | one flat `conformance/tests/`, six pytest modules, plus `conftest.py`. None of the six designed directories exists in any form |
| Definitions oracle | `reference/` | **built** — `reference/oracle/`, ~3,400 lines over eleven modules, conformant on every load-bearing point (§1) |
| Differential families | five (viewport, region, labels, drill-down, authorise-effective-visibility) | **two** — viewport and drill-down. The other three have no server route to test |
| Byte-scanner (I10) | §4.3 | **built, and exceeding its design** (§4.3) |
| Canary comparator (I2) | §4.2, §4.4 | scaffold only — no third fixture state, no canonicalised byte comparison (§4.2) |
| Compile-fail rows (I4, I8) | §4.1 | none. No `trybuild`, no `ui/` directory, no `.stderr` fixtures |
| External oracles (I5) | §4.5 | none. No JVM, no `accumulo-access`, no DuckDB |
| Interleaving scripts | eight (§5) | **zero**. No `conformance` feature, none of the eight pause points, and of the three commands only `evict_fragment` exists — as a Rust method with no route, unreachable from the Python suite |
| Crash realism | truncate-to-`fsync_offset()` | SIGKILL only (§5, and the finding at §5's crash paragraph) |
| CI | per-PR gate, nightly, release gate (§6) | **none**. No CI of any kind exists |

The invariant-by-invariant position is §4.6, and it is this document's most important output.

Two things a reader should carry from this table. First, **the suite's negative controls are not uniformly absent**: two of the three suites that need one have it, and the canary comparator's absence (§4.2) is a specific omission rather than a habit. Second, **a pause mechanism already exists** — in the write path's fault-injection module, with different vocabulary and a different home (lifecycle §7.3). §5 must be built by extending it, not beside it.

## 1. Components

```
reference/            the oracle: brute-force Python implementing the DEFINITIONS
conformance/
  fixtures/           deterministic corpus/policy/query generators + the control journal
  differential/       engine vs oracle over generated cases
  invariants/         the design §10.2 matrix, one module per invariant
  lifecycle/          interleaving + crash tests
  oracles/            CI-only: accumulo-access (JVM), DuckDB — never shipped
  compile-fail/       trybuild: I4 and I8 as compile errors
```

> **⊘ Specified, not implemented.** `reference/` exists as designed. Under `conformance/` there is one flat `tests/` directory of six pytest modules — mask catalogue, I7 selection, overlay journal, canary, byte scan, restart replay — and nothing else. `lifecycle/`, `oracles/` and `compile-fail/` do not exist in any form; `fixtures/`, `differential/` and `invariants/` exist as content inside those six modules and inside `reference/oracle/`, not as a layout. The layout is worth keeping as a target because it is what makes coverage countable by looking; nothing else depends on it.

**The oracle implements definitions, not algorithms** (per-entity visibility walks, literal subset tests, sort-and-take-k; no bitmaps, no caching). **Its inputs are two, and the distinction is load-bearing:** the bundle (read only through the contracts spec — every differential run doubles as a contract check) for build-time state; and the **fixture-owned journal of acked control operations** for runtime state, because predicate changes and unflushed entities live in the overlay and WAL, which are out of contract and invisible in any bundle file. A 500'd control call is *not* journalled as applied.

**Barriers cover the asynchronous operations only.** Overlay changes need none: the 200 follows the generation swap (lifecycle §4), so the caller's next query observes its own change by the ack contract. Flush and compaction are `202`-async, and there the fixture polls `/control/status` until the per-partition `segments_version`/`watermark` reflect the operation — fields the contract already exposes — before querying. "Bundle-only" is claimed for build-time families exactly, and for nothing else.

The built oracle is conformant on all three of those points. It adds one thing this design did not ask for and should have: **the three-group layering is enforced by a test, not by prose.** A test walks the definitional modules' imports and asserts they reach no driver — no harness, no journal, no fixture builder. A definitions oracle that quietly acquires a dependency on the thing it is checking stops being a second implementation, and that is not a property comments can hold.

> **⊘ Partially implemented — the differential families.** Two of the five designed families exist: viewport (including a byte-for-byte Morton check and a live-θ variant) and drill-down. Region, labels and authorise-effective-visibility have no server route to test, so their absence is downstream of the engine, not of the suite. Two runtime families the design did not enumerate exist alongside them: suppression driven over the control plane, and a mixed-change composition stress — both exercising the acked-journal input this section specifies.

## 2. Fixtures

Deterministic from seeds in test names. Small by policy — the probes cover scale; conformance covers *shape* at 10³–10⁵ items where failures are hand-inspectable.

**Adversarial mask catalogue:** empty; single item; ~0.01% coverage; 100%; straddling the ~5% crossover; **container-boundary masks, achieved by deliberately sparse entity allocation** (allocation gaps spread IDs across 2¹⁶ boundaries at fixture scale — stated because at 10⁴ dense IDs no mask straddles anything); all-in-one-tile; watermark-straddling; overlay-heavy; post-deletion states at every ledger stage.

**Join-key scalars.** Every fixture item carries a unique planted declared scalar (`fx_key`). This is the legitimate handle-resolution mechanism: the points batch serves it, so the harness joins handle→item without any reverse map, extra endpoint, or external ID on the viewer plane.

> **⊘ Specified, not implemented.** The scalars are planted, and the points batch does not serve them — the build writes an empty declared-scalar set. The gap is pinned by a **strict xfail** on the test that would consume them, so the day the build serves them the suite fails and someone reads the note rather than the mechanism quietly staying unused. Where a test needs to join a response back to a fixture item today it does so through the drill-down's external ID.

**Canary items** (I2 — every aggregate must be computable from inside `M_auth` alone) with allocation rules that guarantee zero *legitimate* influence: canary entity IDs allocated after all real IDs (no displacement of hash-derived priorities), coordinates at the Morton-maximal corner (sort last; no rank shifts), canary terms interned last (no term-ID displacement), and **canaries belong to no cluster node and no generating set** — a canary in a generating set makes a label *legitimately* withheld in the canary state (containment fails), and one in a node perturbs build-time geometry; either would make the comparator flag fixture perturbation as disclosure. **Without these rules the canary test measures fixture perturbation, not disclosure.**

## 3. The differential harness

Engine vs oracle, five families (viewport, region, labels, drill-down, authorise-effective-visibility), **exact equality** — no floating-point exists in the counting path. Selections compare as sets of `fx_key`s; ties are broken by entity ID in both implementations by definition. Property-based generation with shrinking; overlay mutations drive the I1 differential across randomised acked-journal states.

> **⊘ Partially implemented.** Two families, per §1. Comparison is exact. Property-based generation with shrinking is **not** used on the Python side — `hypothesis` appears nowhere in the suite, and the generated cases are seeded enumerations. `proptest` *is* used, on the Rust side, for Morton properties, the commit window and the allocator; so the technique is in the tree, on the components rather than on the differential.

## 4. The invariant matrix, concretely

**4.1 Compile-time (I4 — permissions live in entity space, geometry in row space, meeting only at an explicit permutation; and I8 — a label's generating set is immutable once supplied).** trybuild compile-fail: entity-ID↔row-ID conversion attempts and generating-set mutation must fail to compile.

> **⊘ Specified, not implemented.** No `trybuild` dependency, no `ui/` directory, no `.stderr` fixtures. I4's separation is upheld in the type system, and nothing proves the violation does not compile; I8 has no implementation to test at all. These are the two rows a compile-fail harness is uniquely suited to, and they are the two rows with no evidence.

**4.2 Canary aggregates (I2), with a canonicalisation procedure.** Two fixture states differing only in canaries; **logically identical query streams** — request parameters that carry handles (drill-down, label filters) are resolved per state via `fx_key` and label text, since handle bytes cannot match across sessions; compare **canonicalised** responses: strip `x-tessera-pin` and transport artifacts; rewrite each item handle to its `fx_key`; rewrite each `node_handle` to the **fixture-unique planted label text served in the same row** (nodes are not items and carry no `fx_key` — their label text is the join key); **sort rows by their canonical key** before comparison (batch emission order under a parallel gather is not contract, and a flaking byte-compare gets "fixed" by weakening); then byte-compare — **explicitly including the points batches (the per-user sample) and the labels batches (the ladder output)**, alongside tile counts, density, region breakdowns, frontier depth and node geometry. Raw wire comparison is impossible, since per-session handle and pin bytes never match; canonicalise-then-compare preserves the "no aggregate moves by any amount" strength on every surface. The extractive tier gets the same treatment against its fixed reference corpus (design Appendix C5).

> **⊘ Partially implemented — scaffold only, and the gap is the one the design itself names as fatal.** Two fixture states exist and are compared. The comparison is of **decoded** structures — a tile map and a multiset of rounded point coordinates — not of canonicalised bytes, and handles are never compared at all. So the surfaces this subsection was written to protect (the points batch and the labels batch, byte for byte) are not covered, and neither is a canonicalisation procedure. See §4.4 for the missing positive control, which is what makes the current test pass-only.

**4.3 The byte-scanner (I10 — entity IDs never cross the trust boundary) — with positive controls, against the boundary identity.** Fixtures plant distinctive-encoding entity IDs; the scanner sweeps all wire payloads and log output from a full differential run for any encoding of them, plus the deployment identity key and external IDs outside the one designed viewer-plane exception (the `/v1/items` drill-down response, design Appendix C's D4), and token bytes outside `Authorization`. **Positive controls:** every run includes a harness-injected planted-ID emission (a synthetic log line and a synthetic payload) that the scanner *must* flag, and a known, actually-transmitted `tessera_id` that the scan's own byte-window mechanism must independently recover from the raw response it was decoded from — a scanner that cannot fail, or that never finds anything real, proves nothing.

**This subsection is built, and it exceeds its design.** The identity-key sweep covers wire, sub-cell stream and log output, in integer halves, hex and raw bytes; the external-ID sweep carries its own positive control that a real drill-down *does* return the external ID, so the sweep is shown to be looking in the right place; and C17's cross-session stability is asserted positively (below). One divergence, in the weaker direction: **the planted-ID negative control is not implemented as a plant.** The test takes a `tessera_id` decoded from a real response and requires the byte-window mechanism to recover it from the raw bytes it came out of. That demonstrates the mechanism against real traffic — which the design also asks for — but it does not demonstrate that the scanner fires on an *entity* ID it was never supposed to see, which is the property the plant exists to prove. Stronger than the design on reach; weaker on falsifiability, and the second is the one that decides what a green run means.

**The correlation check this subsection specified is retired, not merely superseded, and the retirement is recorded here rather than left implicit.** The pre-r6 design asserted that two sessions with identical visibility resolve the same items via `fx_key`, and that **handle values are uncorrelated across sessions** (no equality, no fixed offset) — a property of the per-session `handle: u32` that the boundary-identity change retires from the viewer plane entirely (design Appendix G r21; contracts §0.3 deviation 8). Under the `tessera_id` that replaces it, cross-session equality for the same item is not a bug the scanner must catch — it is the **accepted, intended behaviour** design Appendix C's **C17** records: `tessera_id` is a stable keyed permutation, deliberately linkable across sessions and principals (bounded to items the probing principal already sees), because that stability is what lets a client bookmark, share or reconcile a point across sessions. Asserting the old decorrelation property against a stable identity would assert something the design now says is false by construction; **a conformance test that keeps checking a retired prohibition reports green while checking nothing real.**

**What replaces it:** the byte-scanner continues to assert the property that never changed — no *entity id* crosses the boundary, in any encoding, at any width safe to check without manufacturing its own false positives — and a companion assertion, stated positively rather than as a decorrelation check, that the **same** item resolves to the **same** `tessera_id` across two independently-authorised sessions with overlapping visibility: authorise twice, fetch the same admitted entity's `tessera_id` both times, assert equality — precisely because C17 says this must hold, not despite it. Both halves are built.

**4.4 Behavioural rows.** *I3 (labels are served iff their generating set is a subset of `M_auth`), both halves, black-box:* the containment property across masks and tiers; and the cache half **behaviourally, not by hook**: warm every design §8.5 cache tier (servable labels, fragments, `M_sel`) on a token, apply an overlay change deleting a generating-set member, re-query the *same token and pin* immediately, assert the label is withheld — repeated across cache-warming orders. A green run proves no cache above the check outlived the change, without enumerating internals. *I6 (authorisation comes only from the token):* plugin sandbox denied all capabilities; a module requesting any import fails instantiation. *I7 (sampling happens after masking):* sampler differential across the catalogue + cross-zoom nesting. *I9 (entity IDs are append-only and never reused):* allocator fuzz including crash-replay and router-journal divergence. *I11 (row-space artifacts are versioned together):* drained pins return `410` everywhere; the "row-space artifact across a compaction" clause in its drivable form — hold a pin, drive compaction and retirement via §5, assert `410` and never wrong rows (no client can present a row-space artifact; the pin *is* the presentable proxy). *I12 (filters narrow rendering, never authorisation):* frontier-depth property under filters. *I13b and I13c:* both directions of the asymmetry — unreachable-by-authorisation contributes nothing satisfied, unreachable-by-outage is an error.

**Canary/comparator positive control:** a third fixture state whose extra items are *visible* to the test principal must produce differing canonicalised responses — proving the comparator can fail.

> **⊘ Specified, not implemented — and this is the sharpest single gap in the suite.** There is no third fixture state. The canary comparator is therefore **pass-only with no proof it can fail**: a comparator broken in any way that makes it always agree would report green for ever, and nothing in the suite would notice. This is a specific omission rather than a suite-wide habit — the other two suites that need a negative control carry one. The I7 differential runs a first-*k* storage-order stub and asserts the differential *disagrees* with it; the overlay-journal differential runs two deliberately defective engines — one sampling from a pre-overlay mask, one anchoring θ on a pre-overlay projection — and asserts both are rejected. The comparator is the one that was left without.

Of the other rows in this subsection: I7 is built and is the best-covered invariant in the suite. I3, I6, I11 and I12 are not built here — see §4.6 for what covers I11 elsewhere, and for which of these have no implementation to test at all.

**4.5 External oracles (I5 — the data and auth functions must agree on what a term means).** accumulo-access differential (native parser vs JVM); DuckDB semi-join vs engine postings union — two formulations and two implementations. CI-only; dependency-graph check enforced.

> **⊘ Specified, not implemented — none of it exists.** No `accumulo-access`, no JVM, no DuckDB, and no CI to host them. The design names I5 as its single largest unverifiable dependency and this subsection as the whole of the mitigation, so its absence is not a missing test but an unmitigated risk (design §6.1 carries the same marker). The only authorisation plugin built is a passthrough, for which I5 is trivially true and therefore untestable — which means the gap cannot even be measured today.

### 4.6 The measured invariant matrix

This is the coverage claim the suite can actually support. "As designed" means the test form this document specifies, in the suite; "in substance" means the property is tested, elsewhere and in another form.

| Invariant | Position | Evidence |
|---|---|---|
| **I1** one effective mask, composed before use | **covered as designed** | the viewport differential against the definitions oracle, driven across randomised acked-journal states, plus the overlay-journal differential with its two defective engines as negative controls |
| **I7** sampling happens after masking | **covered as designed — the best-covered invariant here** | the selection differential across the mask catalogue, with a first-*k* storage-order stub as a negative control that the differential must disagree with |
| **I10** entity IDs never cross the trust boundary | **covered as designed — the most thorough test in the repository** | the byte-scanner (§4.3): wire, sub-cell stream and logs; identity-key and external-ID sweeps; C17 asserted positively. Its one weakness is the un-planted negative control |
| **I9** entity IDs append-only, never reused | covered **in substance, in Rust rather than the suite** | allocator and commit-window property tests; no crash-replay or journal-divergence fuzz, since neither a second journal nor a router exists |
| **I11** row-space artifacts versioned together | covered **in substance, in Rust rather than the suite** | ~1,000 lines of pin tests, including a negative control (a pinned request must compose with the fragment's watermark, not the pinned one) and **I11's named failure reached through the API** rather than through an internal call. The compaction clause is untestable: there is no compaction |
| **I2** derived quantities are functions of visible data only | **scaffold only** | two canary fixture states, compared as decoded tile maps and point multisets. No canonicalisation, no byte comparison, no third state — so no proof the comparator can fail (§4.2, §4.4) |
| **I13a** a failed or cancelled request yields no partial answer | **covered incidentally, not by the designed test** | single-flight panic and cancellation tests in the engine and authorisation crates. This is the property the code annotates; the designed asymmetry test does not exist |
| **I13b** a partition not consulted fails closed | **not covered** | one hardcoded partition, no required-set gate, no test. A reviewer grepping `I13` finds only I13a's annotations and must not read them as covering this |
| **I3** labels gate on `M_auth` | **not covered — nothing to test** | no label service |
| **I4** entity space and row space meet only at the permutation | **not covered** | upheld in the type system; no compile-fail row proves the violation is refused (§4.1) |
| **I5** the two authorisation functions agree | **not covered** | passthrough plugin only; the differential oracle that would test it does not exist (§4.5) |
| **I6** authorisation comes only from the token | **not covered — nothing to test** | no plugin host, no sandbox |
| **I8** generating sets immutable | **not covered — nothing to test** | no generating sets |
| **I12** filters narrow rendering, never authorisation | **not covered — nothing to test** | no filter surface |

**Three covered as designed, two in substance, one scaffolded, one incidental, six not covered — four of those six for want of an implementation rather than for want of a test.** The plan calls the suite the deliverable; this is where it stands.

## 5. Interleavings, commands, and crashes

**Pause points park a thread holding no lock.** `before_fragment_insert` in particular sits outside the cache's insert critical section and its single-flight guard — a pause inside either would wedge the lifecycle thread's `ledger_state()` and retirement scan, and a compaction force-refresh landing on the same key. This is a rule on the hook implementation, stated because the deadlock is otherwise discovered in CI.

**Pause points** (park a named thread on a channel): `after_wal_fsync` · `before_generation_swap` · `before_deny_retire` · `compaction_snapshot_taken` · `before_manifest_publish` · `before_current_flip` · **`before_fragment_insert`** (request thread — where the retirement-floor refusal is exercisable) · **`before_fragment_acquire`** (request thread; drives §1.1's request-ordering interleaving in the lifecycle design).

**Commands** (feature-gated RPCs, distinct from pauses, because a pause can only wait): `evict_fragment(key)` · `ledger_state()` (stamp counts, retirement floor, overlay entry states — so retirement is *observed*, not assumed; a deny-retirement test that cannot see retirement passes vacuously) · `fsync_offset()` (the WAL's last-synced position — required by the crash tests below).

> **⊘ Specified, not implemented — and the important part is that a different pause mechanism already exists.** None of the eight pause points exists; the `conformance` feature does not exist. Of the three commands, `evict_fragment` exists as a Rust method used by Rust tests and reachable from no HTTP route, so the Python suite cannot call it; `ledger_state()` and `fsync_offset()` do not exist at all — and `ledger_state()` could not, since the ledger it would report does not exist either (lifecycle §3.2).
>
> **What exists instead:** the write path carries a fault switchboard with two pause sites, WAL append and fsync failure injection, and a step log, behind a feature enabled only through a self dev-dependency (lifecycle §7.3). Different names, a different gate, and a home in the engine rather than in `conformance/`. It is the same mechanism, three stages early, and it carries a rule this section does not state — **an injected failure must be indistinguishable from a real one in variant and in order.** The stage that builds this section must extend that switchboard. Building a second pause mechanism beside the first is the outcome this marker exists to prevent, and it is the natural one, because the two designs share no vocabulary.

**The scripts** (the plan's seven, plus one):

1. **Restart-replay, two variants** (a single script deadlocks — the ack follows the swap, so "kill after ack at `before_generation_swap`" cannot occur): (a) kill −9 after the observed 200, no pause; restart; assert every acked deny survives. (b) pause at `before_generation_swap`, kill; restart; assert replay applies the deny *and* no 200 was ever emitted.
2. **Deny-retirement window** — delete; hold a pre-deletion fragment via a live token; compact; `ledger_state()` confirms retirement actually occurred; assert invisibility throughout.
3. **Suppression persistence** — suppress; `evict_fragment` everything and force refresh; compact; `ledger_state()` confirms the entry never retired; assert invisible until unsuppress.
4. **Stamp regression, two tests** (a single script that confirms retirement before the pause *and* completes it during the pause can never fire the floor refusal — fragments build from current postings, so post-retirement builds already sit at or above the floor): **(a) rebuild-excludes** — delete → retirement confirmed via `ledger_state()` → `evict_fragment` → pinned request misses the cache → assert the rebuilt fragment excludes the item (no pause needed). **(b) floor refusal** — `evict_fragment` → a request begins its build at stamp *e*, paused at `before_fragment_insert` → delete (tombstone *d* > *e*) → force-refresh so no live fragment predates *d* → retirement raises the floor (confirmed) → release → assert the insertion is **refused**, the builder retries against current postings, and the item is excluded. The paused, not-yet-inserted fragment is correctly absent from the stamp counts, so retirement proceeds without it.
5. **Fold variant of 4(b)**: predicate change → compaction folds at *f* → entry retires → the paused pre-fold build's insertion at stamp < *f* must be refused; the retried build reflects the changed terms.
6. **Post-snapshot tombstone** — pause at `compaction_snapshot_taken`; delete; resume; assert survival of the deletion through the fold.
7. **Positional CRC** — corrupt one byte **within [last Flush record, `fsync_offset()`]** ("below the fsync point" alone could land before the replay start and assert nothing); assert recovery fails closed. Corrupt past `fsync_offset()`; assert clean truncation.
8. **Request ordering** — pause a request at `before_fragment_acquire`; evict, retire, swap; release; assert the request's own generation still governs and the response is correct — the eviction-while-held window driven explicitly, as the lifecycle design promises.

> **⊘ Specified, not implemented — zero of the eight exist.** One restart-replay test exists and is script 1(a) in weakened form (below). Scripts 2 through 6 test the deny-retirement ledger, the retirement floor and the compaction fold, none of which are built (lifecycle §3.2, §3.4, §5.3): they cannot be written before the machinery they check. Scripts 7 and 8 test machinery that *does* exist — the positional CRC rule and the request-ordering invariant — and are the two that could be written today.

**Crash realism (the sharpest finding against this design's first draft):** SIGKILL loses nothing — the page cache survives process death — so kill-based tests alone verify replay logic, not durability *ordering*; **an engine that acked before fsync would pass them all.** The falsifying variant: after the kill, **truncate the WAL to `fsync_offset()`** before restart — simulating lost unsynced writes — and assert no *acked* operation is missing. Environment: because power loss is simulated by truncation rather than depended on, the suite may run on any filesystem including tmpfs; that reasoning is recorded here so the first flake does not relitigate it.

> **⊘ Specified, not implemented — the suite contains the test its own design pre-emptively rejects.** The restart-replay module uses SIGKILL only. There is no truncation, and `fsync_offset()` does not exist to truncate to. **What that test proves is replay logic; what it does not prove, and reads as though it does, is that the engine never acks before it fsyncs.** The ack-ordering property is instead held by the write path's `Published` token type and by a fault-injection pause site placed inside the ack function (lifecycle §4, §7.3) — real evidence, in Rust, of a narrower property than this script would establish end to end.

## 6. What pass means, and where

**Every PR:** compile-fail rows, matrix at one seed batch, the eight scripts (conformance build), and the byte-scan with positive controls (feature-free build — the parenthetical binds to the byte-scan only). **Nightly:** full catalogue × rotating seeds on both builds; time-boxed fuzzing (wire decoder, manifest parser, plugin boundary); external oracles. **Release gate:** all green on the release commit + dependency-graph assertions (no JVM, no DuckDB, no `conformance` feature) + criterion budgets.

A differential failure is a defect until proven a fixture bug. The oracle changes only alongside a design or contracts revision, under review. **Design Appendix C4 (timing)** is measured nightly (per-query distributions split by mask sparsity) and published; a threshold waits for its Appendix C owner.

> **⊘ Specified, not implemented — this section is entirely aspirational, because there is no CI.** No per-PR gate, no nightly run, no release gate, no criterion budget gate, and no configuration for any of them. The nearest thing that exists is a layer-dependency script, `scripts/check-layers.sh`, which enforces the crate graph and runs from an opt-in pre-commit hook — not a gate, and not what this section describes. Every claim above about *when* a check runs should be read as an obligation on whichever stage introduces CI. The suite runs when someone runs it.

## 7. Decisions

1. Black-box first; hooks are pause points + a three-command introspection RPC, feature-gated; **wire tests certify the feature-free build** — the two-binary split is documented, not hidden. *(Unbuilt; a different pause mechanism exists — §5.)*
2. The oracle implements definitions; its inputs are bundle + acked-control journal, behind explicit barriers. *(Built, and its layering is enforced by a test.)*
3. **Canonicalise-then-compare** for I2 — handle→`fx_key` rewriting keeps "no aggregate moves" enforceable on the sample and labels, the two surfaces raw byte comparison would have silently dropped. *(Unbuilt — §4.2.)*
4. `fx_key` join scalars replace any handle reverse map — no extra endpoint, no I10 tension. *(Planted, not served; pinned by a strict xfail — §2.)*
5. Positive controls for both pass-only tests: the scanner must catch a planted emission; the comparator must flag a visible-items state. *(The scanner's control is present but demonstrates the mechanism on real traffic rather than on a plant; the comparator has none — §4.3, §4.4.)*
6. Pause points + commands + **truncate-to-fsync-offset** over a simulation framework — the truncation variant is what makes ack ordering falsifiable. *(Unbuilt; the suite uses SIGKILL only — §5.)*
7. Exact equality; ties broken identically by definition. *(Built.)*
8. The oracle is the second implementation of record, versioned with the design corpus. *(Built.)*
9. **Coverage is reported, not claimed.** §4.6 is the matrix of record, and a row moves only when a test moves with it.

## Appendix R — Review record

r1 was independently reviewed (verdict: needs-rework — architecture right; the two central mechanisms unimplementable as specified). r2 resolved all twelve findings: canonicalisation replaced raw byte comparison, with the canary allocation rules stated; the `fx_key` join replaced the reverse map; oracle inputs split into bundle + acked journal with barriers; restart-replay split into its two coherent variants; two request-path pause points, three commands and the eighth script added; truncate-to-fsync-offset made durability ordering falsifiable; I3's cache half given its behavioural black-box form; positive controls added for scanner and comparator; I11's second clause restated in drivable form; CRC corruption bounded to the live replay range; container-boundary masks achieved via sparse allocation. r3 split the stamp-regression script into the two tests whose order actually exercises the floor refusal, added the pause-outside-locks rule and the labels-batch canonicalisation key, scoped the barrier to async operations only, and retired the C17 decorrelation check.

**r4** is the audit pass against the built suite. **No design decision was changed and no argument withdrawn**; what changed is that the document now reports what exists. §0 is new and leads with the measured position; §4.6 is new and is the coverage claim of record; decision 9 is new and says coverage is reported rather than claimed.

Marked **⊘** in this revision: the `conformance` build split (preamble); the directory layout and three of five differential families (§1); `fx_key` service (§2); property-based generation on the differential (§3); compile-fail rows (§4.1); the canary canonicalisation (§4.2); the comparator's third fixture state (§4.4); external oracles (§4.5); the pause points and two of three commands (§5); crash realism (§5); the whole of §6.

Three findings this pass produced that the design did not anticipate:

1. **The suite contains the test its own §5 pre-emptively rejects.** Restart-replay is SIGKILL-only — the variant this document calls insufficient, in the words "an engine that acked before fsync would pass them all". Marked at §5.
2. **The canary comparator has no proof it can fail**, and the other two differential suites carry theirs. That makes it a specific omission rather than a suite-wide habit, which is why §4.4's marker names the two controls that do exist.
3. **A second pause mechanism is about to be built.** The write path's fault switchboard is §5's mechanism under different names, in a different crate, behind a different gate, and three stages early. §5's marker and lifecycle §7.3 both say so, in both directions, because either document read alone leads to the duplicate.

One divergence resolved in the suite's favour: **the byte-scanner exceeds its design** on reach (identity-key and external-ID sweeps across wire, sub-cell stream and logs; C17 asserted positively) while falling short on falsifiability (its negative control demonstrates the byte-window mechanism on a real transmitted `tessera_id` rather than on a planted entity ID). Both halves are recorded at §4.3 rather than netted off, because they answer different questions.
