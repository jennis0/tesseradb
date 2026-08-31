# Tessera — Conformance Suite Design

**Status:** Draft r17 — **the pinned-leaf cases pin by key alone** (r17, 2026-08-31): ordinals are
removed (decision 0113), so the differential's `@#n` arm is gone and `conformance/suite`'s roster
check asserts the field is absent rather than ascending. No coverage row moves. r16 stands
otherwise — **the multi-view differential is written**: `views.md` §11's conformance row, over four views of one entity space, with its coverage recorded in the existing I1, I2, I7, I10 and I12 cells and the four things it does not reach — the gate, the gate's work-indistinguishability, the ingest-time join, a scoped category or text family — named below §4.6's table. **No row is added and none moves position.** r15 stands otherwise — **the suite ran green**: 432 of 432, for the first time since the configuration rework, and the 27 failures r14 recorded are fixed rather than merely diagnosed. *(That sentence is r15's, and both its halves went stale the day after it was written; §0's first table carries what is true, and the count it states is now checked against the suite rather than maintained by hand.)* Two causes and one fixture consequence, all of them the mask catalogue's assumptions rather than defects (§0, and `oracle/catalogue.py`'s own header). r14 stands otherwise — **I3 moved to covered**, both halves, in `conformance/tests/test_label_containment.py` over a fixture built backwards from the property's edge: two principals **one entity apart**, that entity inside the widest generating set and nowhere else, with the artifact drawn from it **absent whole** for the narrower principal and an artifact of the same layer over the same membership served to them in the same response (§4.4, §4.6). It is the first row to move because a test was written rather than because machinery arrived, which is what r13 said was available to do. Two things a reader must carry with it. **The suite around it did not run green when the row moved**, and does now *(r15)*: the configuration rework changed `tessera serve`'s flag, so no module could spawn a server at all until 2026-08-20, and with that fixed 27 of 432 tests failed for two causes outside this row — `public` interned at term 0 shifting every catalogue descriptor's dictionary id by one, and [decision 0073](../decisions/0073-entity-ties-are-ordered-by-morton-code.md)'s Morton tiebreak breaking the mask catalogue's `entity_id == source_id` identity, which `verify()` could not see because its block check compares sets and a within-block permutation preserves them (§0, §6). **And the I11 route named at r13 is still unwritten.** r13 stands otherwise — a correction, not a design change: the **annotation machinery is built**, and this document's reasons for three uncovered rows were written when it was not. I3's containment test, generating sets (I8) and the existence criterion are built, enforced and filter-blind; the *frontier* is withdrawn as a concept rather than missing (decisions 0080, 0082, 0083). I3, I8 and I12's frontier half move from *no implementation to test* to **untested machinery**, which is a testing gap where it previously was not one (§0, §4.1, §4.2, §4.4, §4.6). I6 is untouched — there is still no wasmtime host. **No coverage row moves.** r12 stands otherwise — the I12 row names the **existence criterion** rather than the deleted `min_visible_members` key (Appendix R); r11 stands otherwise — §5's marker refreshed to decision 0071's state of the world: five pause sites, the seam three landed by extending the switchboard as the marker demands, and the gate now "no default-features build", the feature being declarable for the correctness suite's faults build. §5's eight points remain unbuilt. r10 stands otherwise — a correction, not a design change: the compaction fold is **built**, and three of this document's claims that it does not exist are wrong. Scripts 2, 3 and 6 move from "no machinery to test" to **untested machinery**, which is a testing gap where it previously was not one (§0, §2, §5). **No coverage row moves** — §4.6 is untouched, per decision 9. r9 stands otherwise: I12's mask half is covered by the attribute-filter differential in the form filter-surface §9 specifies, its frontier half blocked on the label service with I3; script 5 is unreconstructable and the stamp-ledger and retirement-floor scripts are void (Rule S / Rule F, write-path §5.4)

**Owns:** the design of `conformance/` and `reference/` — harness architecture, oracle interfaces, fixtures, the invariant matrix's concrete test forms, the interleaving machinery, and what "pass" means. The implementation plan (§10.1) is blunt that the suite is the deliverable; this document exists so it is designed, not accreted.

**Two principles.** *Test the served surface*: spawn the real binary, build real bundles, speak the real API; internal access only where interleaving control requires it. *Invent no framework*: cargo test/proptest + pytest, with two pieces of bespoke machinery — the pause-point-and-command harness (§5) and the byte-scanner (§4.3), each reviewable by reading.

**One build caveat, stated rather than fudged:** wire-shape and byte-scan tests are specified to run against a **feature-free build** and interleaving tests against a `conformance` build. The two-binary split is a documented limitation, not an accident.

> **⊘ Specified, not implemented.** There is no `conformance` cargo feature; every test runs against one ordinary build. The split is an obligation on the stage that builds the interleaving harness, not a property of the suite today — and it is now that stage's obligation alone, since crash realism turned out not to need the feature (§5).

`I1`…`I13` are the invariants of design §4; each is restated where the matrix in §4.6 uses it. `M_auth` is a viewer's authorised visible set.

---

## 0. What exists

The suite is designed here and built in part. Both halves matter to a reader deciding what the system's evidence is worth, so the measured position leads.

| | Designed | Built |
|---|---|---|
| Directory layout | `conformance/{fixtures,differential,invariants,lifecycle,oracles,compile-fail}` | one flat `conformance/tests/`, **eighteen** pytest modules, plus `conftest.py` — and `conformance/suite/`'s nine beside them. None of the six designed directories exists in any form *(count corrected 2026-08-30; it read seven, which was true when written and had not moved with the suite)* |
| Definitions oracle | `reference/` | **built** — `reference/oracle/`, ~7,100 lines over fifteen modules, conformant on every load-bearing point (§1) *(the size was last written at ~3,600 over twelve and had not moved with the package)* |
| Differential families | five (viewport, region, labels, drill-down, authorise-effective-visibility) | **two** — viewport and drill-down. The other three have no server route to test |
| Byte-scanner (I10) | §4.3 | **built, exceeding its design on reach (§4.3), and its control is now a plant** |
| Canary comparator (I2) | §4.2, §4.4 | **built** — three fixture states, canonicalised byte comparison, and a state it must reject (§4.2, §4.4) |
| Compile-fail rows (I4, I8) | §4.1 | **I4 built** — three `trybuild` rows with `.stderr` fixtures. I8's generating sets are now built, but its immutability is a publication rule rather than a type-level one, so there is no mutation for a compile-fail row to refuse (§4.1) |
| External oracles (I5) | §4.5 | none. No JVM, no `accumulo-access`, no DuckDB |
| Interleaving scripts | eight (§5) | **zero as scripts**, but **three of the eight pause points exist** and are driven from `conformance/`: the write path's fault switchboard (`crates/tessera-lifecycle/src/faults.rs`) names `after_fsync` — §5's `after_wal_fsync`, shortened — `before_manifest_publish` and `before_current_flip`, alongside `before_ack` and `before_merge_publish` that §5 did not ask for, and `conformance/suite/test_crash_atomicity.py` arms three of the five, kills the server at each, applies §12.3's per-seam discard rule and checks the landing against an entitlement. *(Corrected 2026-08-30: this read "none of the eight pause points", which was true when written — §5 predicted that the eventual mechanism and this one would "share no vocabulary", and they now share it exactly.)* No `conformance` feature; the switchboard reaches a build through the default-off `fault-injection` feature instead. Of the two remaining commands only `evict_fragment` exists — as a Rust method with no route, unreachable from the Python suite (`fsync_offset()` was retired rather than built, decision 0038) |
| Crash realism | truncate-to-`fsync_offset()` | **built** — the WAL's own `.sync` sidecar supplies the offset, so no command was needed (§5) |
| CI | per-PR gate, nightly, release gate (§6) | **per-PR gate built**; nightly and release do not exist (§6) |
| The suite running green | the whole point | **677 of 681, with 4 skipped and 0 failing** *(measured 2026-08-31, serially, on a quiet box, after the day's seven merges)*. Earlier the same day the serial run showed 675 with 2 failing — both `test_text_differential.py::…[phrase on … -full_100pct]`, both a 30-second HTTP **read timeout** under a load average above 30 from concurrent agent work, and both gone the moment the box was quiet; the machine, not the suite. Recorded as measured rather than as green — a suite whose failures are explained away in prose and rounded off in the number is one nobody reads twice. The count marker moved by **six** for the new cases and by **two more** that the marker was already carrying *above* what pytest collected. *(673 of 677 earlier that day, 634 of 638 on 2026-08-30, and the 39 that arrived between them are the multi-view differential.)* This read "**432 of 432** *(r15, 2026-08-20)*", and both halves of that were true when written and had stopped being true the day after. The **count** had not moved with the suite — ten further modules and the correctness suite's own battery had been added, and `conformance/suite` was excluded from the r15 measurement on a Python-version caveat that no longer holds; it is checked against pytest now rather than maintained by hand. The **green** stopped on 2026-08-21 and was restored on 2026-08-30, and the shape of that outage is worth keeping: one commit's declaration outgrew three of its readers, and every test in `conformance/suite/test_total_verification.py` errored in the module fixture — seven, including both of that module's negative controls. `Corpus::config_toml` gained the artifact sources and a `partition` column; the module's corpus shim did not write those sources, so `tessera build` exited 1, and `conformance/suite/test_materialisation.py` now holds the declaration and the shim to one file set, so the next divergence fails in a second naming the file rather than as a build's swallowed exit code. `Corpus::ingest_batch` and `tessera corpus items` then emitted no `partition` column, so `/control/ingest` refused the body and the row half had no expectation for a column it compares. **That last one was not a transcription**: the partition value was the one generated column whose stride was derived from a count that was itself derived from *n*, so an entity moved artifact as the corpus grew and `items` — which is constructed at `n = 0` — could not answer for it. The generator was inverted rather than the verb weakened: the stride is a constant, the count is derived from it, and the value is a function of `(seed, layer, e)` at every size (`tessera-corpus`'s `partition.rs`) |

**The case count is checked, not maintained.** The line below is compared against what pytest
collects, by a step in CI's `conformance` job; a hand-edit that disagrees with the suite fails
there. Edit it when the suite's size changes and the step tells you the number, not to make a
sentence read better — and read it as a size, never as a coverage claim, which is §4.6's business.

    conformance-cases = 690

The invariant-by-invariant position is §4.6, and it is this document's most important output.

Two things a reader should carry from this table. First, **every differential and every sweep here now has something that makes it fail**: the canary comparator was the last one without, and its third fixture state closed that. That claim is deliberately narrower than "every test" — several structural checks remain pass-only, and the restart-replay module's controls needed a deliberately damaged WAL and so live outside the suite (§5). Second, **a pause mechanism already exists** — in the write path's fault-injection module, with different vocabulary and a different home (lifecycle §7.3). §5's interleavings must be built by extending it, not beside it; that has not happened, and it is the largest thing this document still describes and the system does not have.

**The suite was red for two causes, and both were the mask catalogue's assumptions rather than
defects** *(r14 found them, r15 fixed them)*. Neither was a leak, and neither was in the artifact
work that found them.

**First, `public` is interned at term `0`** (`per-point-attributes.md` §3.8), so every catalogue
descriptor's dictionary id is one higher than the corpus's own term id. The two numbers name
different blocks and neither file complains, so a grant resolved with the wrong one answers about
the wrong entities — which is what the byte-scan's drill-down control was doing. `Block.term_id` is
now the corpus's, `Block.dict_term_id(bundle)` resolves the dictionary's from the bundle, and
`verify()` checks the interning **rule** rather than an assumed equality.

**Second, [decision 0073](../decisions/0073-entity-ties-are-ordered-by-morton-code.md) made the
within-signature tiebreak the Morton code**, and the mask catalogue was designed so that
`entity_id == source_id` — which held only while the tiebreak was the source id. Every block's
posting *set* is still exactly its intended range, which is why `verify()` passed that check and
reported nothing: **a within-block permutation preserves a set**, so the check whose comment said it
re-derived the identity never tested it. What failed instead was every oracle indexing a planted
value by an entity id. The fixture now carries the join it always should have —
`Bundle.source_of_entity`, over the external-ID sidecar — and `verify()` gains **check 3b**, which
compares the two spaces item by item and is the check a set comparison cannot pass vacuously.

**One consequence of that tiebreak is a fixture-shape fact worth carrying**, because it is not a
translation and no bridge fixes it: an entity id's position inside its block now tracks its position
on the map. So a test that denied "the lowest 2,400 entities" was denying a contiguous *region*, and
it emptied whole tiles — which the overlay differential's own tile-count precondition caught, in
exactly the way such a precondition is supposed to. That set is now strided.

**Both causes are the failure `--carry-id-key-from`'s warning describes**, arriving from inside
rather than from a rebuild: a term interned earlier renumbers the dictionary, and a changed tiebreak
renumbers entity ids. Both are recorded at the fixture's own header as well, because that is where
the next person will be standing.

**What did not move, and why it did not.** *(r14: I3 moved — see the status line and §4.6. The paragraph below is r13's, with I3 struck from its lists.)* Five invariants remain uncovered *(r14; six)*, and **two of them — I8 and I11 — are gaps in this suite** *(r13; it was one)* — which is a correction, not a movement: no row of §4.6 moves, and what changes is why each stands where it does. **One — I6 — has no implementation to test**: there is no wasmtime host, so nothing loads a guest module and nothing sandboxes one. **I8 no longer belongs in that sentence** *(r13; I3 did not either, and at r14 it is covered)*. The annotation machinery is built: artifacts carry ranked contents with generating sets, containment (`|G ∩ M| == |G|`, all or nothing) is evaluated on every serving route, and the existence criterion is a live control. I8 is therefore untested machinery here — the engine's own fold tests cover it in substance, and this suite does not drive it. (I12 left this list at r9: the filter surface landed and its mask half is covered by the attribute-filter differential. Its **frontier half** is a third case again *(r13)*: the frontier is not missing, it is **withdrawn** — every artifact is tested on its own and the root-down descent is gone (decisions [0080](../decisions/0080-the-frontier-is-a-per-artifact-test.md), [0082](../decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md), [0083](../decisions/0083-the-frontier-is-a-request-time-budget.md)) — so what survives of I12 on the artifact side is what architecture §8.4 states: a filter never touches containment and never relaxes the criterion, both running against `M_auth` alone. That is built and blind to the filter by construction, and untested here.) I5 needs an authorisation plugin whose two functions can genuinely diverge before any oracle could disagree with it (decision [0027](../decisions/0027-i5-is-unverified.md)). I13b needs a required-set gate. Of the eight interleaving scripts, two (4 and 5) drive a stamp ledger and a retirement floor that are deleted from the spec rather than merely unbuilt (owner-ruled 2026-08-03; Rule S / Rule F at write-path §5.4), so they will never be written in the form specified. **The other three — 2, 3 and 6 — are a testing gap now, and were not when this section was first written.** They wait on the compaction fold, and the fold is built, retires and is scheduled ([`compaction.md`](compaction.md), normative). What blocks them is this suite's own unbuilt harness — the pause points and `ledger_state()` of §5 — which is the same thing blocking script 8. A reader should read those three as untested machinery, not as machinery that does not exist. **I11 is the third**: the pin that carried it is deleted (decision [0041](../decisions/0041-pins-become-a-staleness-stamp.md)) and neither replacement §4.4 names is written, which §4.6 records as a regression in coverage rather than a reclassification.

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

> **⊘ Specified, not implemented.** `reference/` exists as designed. Under `conformance/` there is one flat `tests/` directory of **seventeen** pytest modules — byte scan, canary, filter differential, I7 selection, keyword differential, keyword layers, label containment, mask catalogue, overlay journal, record blob, region leaf, restart replay, schema refusals, shape membership and its WGS84 sibling, text differential, text layers — plus `conformance/suite/`'s nine, and nothing else. *(Corrected 2026-08-30: this read "seven" and listed seven, a count accurate when written that ten modules had since outgrown. A register that understates what exists teaches a reader to discount it, the same way one recording built machinery as absent does.)* `lifecycle/`, `oracles/` and `compile-fail/` do not exist in any form; `fixtures/`, `differential/` and `invariants/` exist as content inside those modules and inside `reference/oracle/`, not as a layout. The layout is worth keeping as a target because it is what makes coverage countable by looking; nothing else depends on it.

**The oracle implements definitions, not algorithms** (per-entity visibility walks, literal subset tests, sort-and-take-k; no bitmaps, no caching). **Its inputs are three, and the distinction is load-bearing:** the bundle (read only through the contracts spec — every differential run doubles as a contract check) for build-time state; the **fixture-owned journal of acked control operations** for runtime state, because dispositions and unflushed entities live in the overlay and WAL, which are out of contract and invisible in any bundle file; and the **points file the build consumed**, for geometry. A 500'd control call is *not* journalled as applied.

**Why geometry is a third input rather than a column.** `columns.arrow` holds a position as a cell code plus a residual, both written by the build (contracts §2.6). Recomputing "the" code from those would be `code == interleave(deinterleave(code))` — a tautology, which a build that emitted a wrong column and then sorted and served consistently by its own wrong values passes. Taking the source instead moves the oracle's geometry input *upstream* of the build, which is strictly stronger than the `x`/`y` columns it replaces. The join from a row to a source row is `row → entity_id` (permutation) `→ external_id` (sidecar) `→ source row`, which is why contracts §0.4 lists the external-ID sidecar in the Phase-1 conformance burden.

**The layering escape, specified.** §1's rule is that the definitional modules never reach a driver or a fixture builder, and it is enforced by a test. A definitional module therefore may not *find* the points file: a driver resolves the path and hands it in, exactly as the fixture already hands in the identity key. A bundle with no source attached **refuses** to derive a code rather than falling back to the stored column — the fallback is the tautology this input exists to remove, and it would fire silently precisely when a harness forgot to wire the source up.

> **⊘ The binding is unbuilt.** Nothing ties a points file to a bundle: no digest, no manifest entry. Handing in the wrong file is a whole-suite geometry failure that reads like an engine bug, and nothing structurally prevents a runner regenerating the "source" from the bundle and restoring the tautology. A source digest in `MANIFEST.json` closes it and is not specified here; until it is, the binding is the harness's discipline.

**Barriers cover the asynchronous operations only.** Overlay changes need none: the 200 follows the generation swap (lifecycle §4), so the caller's next query observes its own change by the ack contract. Flush and compaction are `202`-async, and there the fixture polls `/control/status` until the per-partition `segments_version`/`watermark` reflect the operation — fields the contract already exposes — before querying. A merge is observable the same way, since it publishes its own geometry version; **the entity-space coalesce is not**, because it deliberately moves no row and so bumps neither field (write-path §7), and a test that needs to observe one must barrier on something else. "Bundle-only" is claimed for build-time families exactly, and for nothing else.

The built oracle is conformant on all three of those points. It adds one thing this design did not ask for and should have: **the three-group layering is enforced by a test, not by prose.** A test walks the definitional modules' imports and asserts they reach no driver — no harness, no journal, no fixture builder. A definitions oracle that quietly acquires a dependency on the thing it is checking stops being a second implementation, and that is not a property comments can hold.

> **⊘ Partially implemented — the differential families.** Two of the five designed families exist: viewport (including a byte-for-byte Morton check and a live-θ variant) and drill-down. Region, labels and authorise-effective-visibility have no server route to test, so their absence is downstream of the engine, not of the suite. Two runtime families the design did not enumerate exist alongside them: suppression driven over the control plane, and a mixed-change composition stress — both exercising the acked-journal input this section specifies.

## 2. Fixtures

Deterministic from seeds in test names. Small by policy — the probes cover scale; conformance covers *shape* at 10³–10⁵ items where failures are hand-inspectable.

**Adversarial mask catalogue:** empty; single item; ~0.01% coverage; 100%; straddling the ~5% crossover; **container-boundary masks, achieved by deliberately sparse entity allocation** (allocation gaps spread IDs across 2¹⁶ boundaries at fixture scale — stated because at 10⁴ dense IDs no mask straddles anything); all-in-one-tile; watermark-straddling; overlay-heavy; and the two post-deletion states, since a deletion leaves the overlay only at the fold that executes it (Rule F, write-path §5.4): accepted-and-invisible with its entry standing, and executed-and-retired after a fold. Both are reachable — the fold is built — and the catalogue carries only the first.

**Join-key scalars.** Every fixture item carries a unique planted declared scalar (`fx_key`). This is the legitimate handle-resolution mechanism: the points batch serves it, so the harness joins handle→item without any reverse map, extra endpoint, or external ID on the viewer plane.

> **⊘ Specified, not implemented.** The scalars are planted, and the points batch does not serve them — the build writes an empty declared-scalar set. The gap is pinned by a **strict xfail** on the test that would consume them, so the day the build serves them the suite fails and someone reads the note rather than the mechanism quietly staying unused. Where a test needs to join a response back to a fixture item today it does so through the drill-down's external ID.

**Canary items** (I2 — every aggregate must be computable from inside `M_auth` alone) with allocation rules that guarantee zero *legitimate* influence: canary entity IDs allocated after all real IDs (no displacement of hash-derived priorities), coordinates at the Morton-maximal corner (sort last; no rank shifts), canary terms interned last (no term-ID displacement), and **canaries belong to no cluster node and no generating set** — a canary in a generating set makes a label *legitimately* withheld in the canary state (containment fails), and one in a node perturbs build-time geometry; either would make the comparator flag fixture perturbation as disclosure. **Without these rules the canary test measures fixture perturbation, not disclosure.**

## 3. The differential harness

**Prerequisite: the bundle under test must carry `partitions/<p>/terms/pairs.parquet`.** The mask differential is the whole reason the oracle is a second implementation rather than a transcription: the engine derives a viewer's authorised set from the compressed postings, `reference/oracle/mask.py` derives it from the flat `(entity_id, term_id)` pair relation by direct scan, and agreement is the test. The file is an *optional* build input for a serving deployment (contracts §2.4, design §6.3) and is **required for a conformance run** — a bundle built without it cannot be checked, and the failure will read as a missing file rather than as missing coverage unless this is stated.

Engine vs oracle, five families (viewport, region, labels, drill-down, authorise-effective-visibility), **exact equality** — no floating-point exists in the counting path. Selections compare as sets of `fx_key`s; ties are broken by entity ID in both implementations by definition. Property-based generation with shrinking; overlay mutations drive the I1 differential across randomised acked-journal states.

> **⊘ Partially implemented.** Two families, per §1. Comparison is exact. Property-based generation with shrinking is **not** used on the Python side — `hypothesis` appears nowhere in the suite, and the generated cases are seeded enumerations. `proptest` *is* used, on the Rust side, for Morton properties, the commit window and the allocator; so the technique is in the tree, on the components rather than on the differential.

## 4. The invariant matrix, concretely

**4.1 Compile-time (I4 — permissions live in entity space, geometry in row space, meeting only at an explicit permutation; and I8 — a label's generating set is immutable once supplied).** trybuild compile-fail: entity-ID↔row-ID conversion attempts and generating-set mutation must fail to compile.

> **Built for I4; I8 has nothing to test.** Three `trybuild` rows in `tessera-types`, each with its `.stderr` fixture: the `into()` conversion, reuse of the raw value across the width boundary, and direct field access. The pairing with `.stderr` is what stops the harness rotting into a tautology — a row failing for an unrelated reason would still "fail to compile". Adding a `From<EntityId> for RowId` makes the first row fail, which was measured rather than asserted. The case had previously lived as a commented-out line annotated "MUST NOT compile"; a comment cannot fail.
>
> I8 gets no row, and *(r13)* the reason has changed rather than gone away. Generating sets are built, and the rule holds: a set is written once at publication, is never grown, and shrinks only where a **permissive** layer's fold rewrites it (`annotation-write-cycle.md` §2.1, §3.2). But it holds because no route adds to a set, not because a type refuses the assignment — so a compile-fail row has nothing to point at, and this row's *form* is what is unavailable. The invariant's behavioural form is available and is not written here. A placeholder would report green while checking nothing the invariant is about.

**4.2 Canary aggregates (I2), with a canonicalisation procedure.** Two fixture states differing only in canaries; **logically identical query streams** — request parameters that carry handles (drill-down, label filters) are resolved per state via `fx_key` and label text, since handle bytes cannot match across sessions; compare **canonicalised** responses: strip `x-tessera-pin` and transport artifacts; rewrite each item handle to its `fx_key`; rewrite each `node_handle` to the **fixture-unique planted label text served in the same row** (nodes are not items and carry no `fx_key` — their label text is the join key); **sort rows by their canonical key** before comparison (batch emission order under a parallel gather is not contract, and a flaking byte-compare gets "fixed" by weakening); then byte-compare — **explicitly including the points batches (the per-user sample) and the labels batches (the ladder output)**, alongside tile counts, density, region breakdowns, frontier depth and node geometry. Raw wire comparison is impossible, since per-session handle and pin bytes never match; canonicalise-then-compare preserves the "no aggregate moves by any amount" strength on every surface. The extractive tier gets the same treatment against its fixed reference corpus (design Appendix C5).

**Byte comparison rests on response determinism, and that is legitimate only because the suite pins its own configuration.** The same request produces byte-identical responses at any `compute_threads` today, but design §10.4 records that as an implementation detail rather than a guarantee, and downstream must not rely on it. The suite is not downstream in the relevant sense: it fixes the deployment configuration it runs against, so what it depends on is determinism *at one thread count* — a far weaker thing than stability across configurations, and a variable it controls. **A run at a different thread count is outside what has been argued**, and is the likely cause of an otherwise unexplained comparison failure. Canonicalisation is what makes the comparison independent of that; until it exists, the pinned configuration is the whole of the argument, and it must be pinned deliberately rather than by accident.

> **Built — and the canonicalisation is simpler than this subsection specifies, because the column it was written to defeat no longer exists.** §4.2 above specifies a handle→`fx_key` rewrite on the grounds that raw comparison is impossible while per-session handle bytes are in the body. Decision [0006](../decisions/0006-per-session-handles-retired.md) retired that column and contracts r6 replaced it with `tessera_id`, so the points batch is `(tessera_id, x, y, declared scalars…)` — every column a deterministic function of the bundle, none a function of the session. The only per-session bytes left are the token and `x-tessera-pin`, both headers.
>
> So the join `fx_key` was wanted for is an equality on a column the wire already carries: the fixture builds every state under one identity key, and the allocation rules keep each base item's entity id identical across builds. What is implemented is: the points batch compared as **its own bytes in served order** (contracts §3.2 orders the served points ascending by `tessera_id` within each tile, so comparing it unsorted is stronger than sorting it); the tile batch sorted by tile id and re-serialised, since emission order is not contract; and nothing stripped from the body. That last is asserted rather than assumed — a test reissues one request under a second independently-authorised session and requires identical bytes, so the day something session-dependent enters the body, the premise fails loudly.
>
> `fx_key` remains planted and unserved, and its strict xfail remains the marker (§2). **The labels half is not compared** — and *(r13)* that is now a gap in this comparator rather than in the system. A served artifact reaches a client on its own frame, carrying its masked count, its derived geometry and the one content this viewer contains; the canonicalisation this section specifies applies to it unchanged, since an artifact's identifier is opaque per session and its content text is the join key the fixture can plant. Until the comparator covers that frame, the canary claim is bounded to the surfaces it does compare, and §4.6's I2 row says so.

**4.3 The byte-scanner (I10 — entity IDs never cross the trust boundary) — with positive controls, against the boundary identity.** Fixtures plant distinctive-encoding entity IDs; the scanner sweeps all wire payloads and log output from a full differential run for any encoding of them, plus the deployment identity key and external IDs outside the one designed viewer-plane exception (the `/v1/items` drill-down response, design Appendix C's D4), and token bytes outside `Authorization`. **Positive controls:** every run includes a harness-injected planted-ID emission (a synthetic log line and a synthetic payload) that the scanner *must* flag, and a known, actually-transmitted `tessera_id` that the scan's own byte-window mechanism must independently recover from the raw response it was decoded from — a scanner that cannot fail, or that never finds anything real, proves nothing.

**This subsection is built, it exceeds its design on reach, and its control is now a plant.** The identity-key sweep covers wire, sub-cell stream and log output, in integer halves, hex and raw bytes; the external-ID sweep carries its own positive control that a real drill-down *does* return the external ID, so the sweep is shown to be looking in the right place; and C17's cross-session stability is asserted positively (below).

The plant this subsection specifies now exists, and it lives **in the scanner rather than on the wire**. Each sweep mechanism is handed a synthetic payload carrying a planted entity ID in the encoding that mechanism owns — the identity column at its own stride, the coordinate lanes, the sub-cell columns, and the log in both binary and decimal — and must return it; each is also handed a clean payload and must return nothing, so a scanner that flagged everything would not satisfy it either. Building a feature-gated route that deliberately emitted an entity ID would put the plant on real traffic at the cost of constructing the leak the invariant forbids, and the falsifiability question is a question about the scanner.

**The gap between the plant and the pre-existing real-traffic control was measured, not argued.** Filtering the window scan to values `>= 2^32` — the shape of a plausible "suppress obviously spurious small windows" change — leaves the real scan **passing**, since every genuine `tessera_id` is a uniform 64-bit value, and fails the plant immediately, since an entity ID is exactly the small value such a filter discards. Both controls are kept: one shows the mechanism works on traffic that really was transmitted, the other shows it fires on the value class that must never be.

**The correlation check this subsection specified is retired, not merely superseded, and the retirement is recorded here rather than left implicit.** The pre-r6 design asserted that two sessions with identical visibility resolve the same items via `fx_key`, and that **handle values are uncorrelated across sessions** (no equality, no fixed offset) — a property of the per-session `handle: u32` that the boundary-identity change retires from the viewer plane entirely (design Appendix G r21; contracts §0.3 deviation 8). Under the `tessera_id` that replaces it, cross-session equality for the same item is not a bug the scanner must catch — it is the **accepted, intended behaviour** design Appendix C's **C17** records: `tessera_id` is a stable keyed permutation, deliberately linkable across sessions and principals (bounded to items the probing principal already sees), because that stability is what lets a client bookmark, share or reconcile a point across sessions. Asserting the old decorrelation property against a stable identity would assert something the design now says is false by construction; **a conformance test that keeps checking a retired prohibition reports green while checking nothing real.**

**What replaces it:** the byte-scanner continues to assert the property that never changed — no *entity id* crosses the boundary, in any encoding, at any width safe to check without manufacturing its own false positives — and a companion assertion, stated positively rather than as a decorrelation check, that the **same** item resolves to the **same** `tessera_id` across two independently-authorised sessions with overlapping visibility: authorise twice, fetch the same admitted entity's `tessera_id` both times, assert equality — precisely because C17 says this must hold, not despite it. Both halves are built.

**4.4 Behavioural rows.** *I3 (labels are served iff their generating set is a subset of `M_auth`), both halves, black-box:* **written, and this is the form it took** *(r14; `conformance/tests/test_label_containment.py`)*. The containment property across principals and zoom tiers, over `oracle.label_fixture` rather than the mask catalogue — the catalogue is built backwards from the selection invariant's adversarial mask shapes and is shared by six modules, and what this row needs instead is two principals a **single entity** apart, which is a property of a corpus designed for it. Three artifacts of one layer over one membership, differing only in which generating sets their ranked contents were drawn from, so that the same response carries an absence and its control: the artifact whose only content spans the split entity is absent whole for the narrower principal, the ranked one falls back to the content it does contain, and the third is served to both. The layer is `public` with an `inherited` artifact gate and no existence criterion, so containment is the only conjunct that can fail. And the cache half **behaviourally, not by hook**: warm every design §8.5 cache tier on a token — *(r13)* of which the servable-label tier is not built, containment being evaluated per request, so what a warming order actually exercises is the mask fragments, the session's resolved visibility and the row-space artifact projections — apply an overlay change removing a generating-set member, re-query on the *same token* immediately, assert the label is withheld — repeated across cache-warming orders. A green run proves no cache above the check outlived the change, without enumerating internals. **⊘ The pin is not re-presented** *(r14)*: this row was written when a pin was the presentable proxy for a row-space artifact, and [decision 0041](../decisions/0041-pins-become-a-staleness-stamp.md) made it advisory and never authorisation, so presenting one is an ordinary request with an ordinary answer and could not hold a suppression out either way. What carries session state across the change is the token, and that is what the test holds fixed. *I6 (authorisation comes only from the token):* plugin sandbox denied all capabilities; a module requesting any import fails instantiation. *I7 (sampling happens after masking):* sampler differential across the catalogue + cross-zoom nesting. *I9 (entity IDs are append-only and never reused):* allocator fuzz including crash-replay and router-journal divergence. *I11 (row-space artifacts are versioned together):* **⊘ NOT COVERED, and the loss is stated rather than left silent.** This row used to read "drained pins return `410` everywhere; hold a pin, drive compaction and retirement via §5, assert `410` and never wrong rows" — and its own parenthetical said why that was the best available: *no client can present a row-space artifact; the pin is the presentable proxy*. The pin is deleted (`geometry-pinning.md`), so the proxy is gone, and what remains of I11 is the **within-request** rule — a request resolves geometry once and uses it throughout — which has **no presentable surface at all**. A black-box suite cannot observe how many times a handler loaded a pointer. Two routes exist and neither is written: a publication-boundary test asserting a request spanning a row-space-moving publication never mixes row spaces — **which no longer waits on compaction**, since a merge permutes row space inside its merged span and bumps `segments_version` (write-path §7); it waits on `before_fragment_acquire`, which does not exist — or a white-box assertion in Rust that the generation is loaded exactly once per request. What `crates/tessera-engine/tests/merge.rs` does assert is that a merge bumps the geometry version and that the refresh replaces the cached projection; it drives no request across the swap, so it is not this row. **Until one is written, I11 is uncovered.** *I12 (filters narrow rendering, never authorisation):* **⊘ the frontier-depth form named here is withdrawn with the frontier itself** *(r13; decisions 0080, 0082, 0083)* — there is no depth for a filter to move. What replaces it is architecture §8.4's surviving half, and it is a black-box property: an artifact's existence verdict, its masked count and its containment are identical with and without any filter, since both tests run against `M_auth` alone. The machinery is built and filter-blind by construction; no test here drives it. *I13b and I13c:* both directions of the asymmetry — unreachable-by-authorisation contributes nothing satisfied, unreachable-by-outage is an error.

**Canary/comparator positive control:** a third fixture state whose extra items are *visible* to the test principal must produce differing canonicalised responses — proving the comparator can fail.

> **Built.** The third fixture state exists: the same extra item, in the same commit window at the same Morton-maximal corner under the same identity key, carrying a term the tested principals **do** hold. The comparison against it must fail, and does. The visible state passes the allocation-rule check too, because a control whose extra item displaced something would make the comparator disagree for a reason unrelated to visibility — the same fixture-perturbation failure in the opposite direction.
>
> **The control drives the comparator, not a copy of it.** The comparison was inline in the test module, so a control written beside it would have exercised a second code path and proved that path — the failure mode the control exists to rule out, reproduced one level up. It is one function now, called twice. Not asserted: that *every* grant set disagrees. The zero-visibility token cannot, and requiring it to would be requiring a leak; the differences are reported per grant set so that a disagreement arising for the wrong reason is legible rather than merely green.
>
> The suite's other two controls are unchanged: the I7 differential runs a first-*k* storage-order stub and asserts the differential *disagrees* with it; the overlay-journal differential runs two deliberately defective engines — one sampling from a pre-overlay mask, one anchoring θ on a pre-overlay projection — and asserts both are rejected.

Of the other rows in this subsection: I7 is built and is the best-covered invariant in the suite. I12's mask half is covered at r9 in the form filter-surface §9 specifies, and its artifact half is the untested-machinery case above (§4.6). I3 is built here *(r14)*. I6 and I11 are not — see §4.6 for what covers I11 elsewhere, and for which of these have no implementation to test at all, which *(r13)* is I6 alone.

**4.5 An independent second implementation, for I5.** I5 holds only if the two authorisation functions agree about what a term means, and nothing downstream can check that — so checking it requires a second implementation of the same policy language to disagree with.

**Which one is open** (decision [0027](../decisions/0027-i5-is-unverified.md)). Two candidates, neither chosen: an external implementation of the access-expression grammar, whose independence is the argument for it; or one written alongside this suite, which is the pattern §4.4's mask differential already uses. Either needs a plugin whose two functions can genuinely diverge.

Whatever is chosen is CI-only and never shipped, with the dependency-graph check enforcing it.

> **⊘ Not implemented, and not yet designed.** There is no I5 oracle of any kind and no CI to host one. Note that this is narrower than it first reads: the **mask** differential of §4.4 does exist, in Python rather than the DuckDB form once sketched here — it derives a viewer's set from the flat pair relation by direct scan while the engine derives it from compressed postings. What is absent is an oracle for the *plugin-consistency* obligation specifically. So I5 is **unverified**: it rests on the caller's semantic obligation and nothing independently checks it, which design §6.1 now states plainly rather than pointing at a mitigation. The only authorisation plugin built is a passthrough, for which both functions are the same string comparison — I5 is trivially true of it and no differential could disagree, so the gap cannot even be measured today. **Which oracle eventually checks I5 is open** (design §6.1): an external implementation of the grammar and a second implementation written alongside this suite are both live, and this subsection's choice of `accumulo-access` is one candidate rather than a settled route. Either needs a plugin whose two functions can genuinely diverge, and none exists.

### 4.6 The measured invariant matrix

This is the coverage claim the suite can actually support. "As designed" means the test form this document specifies, in the suite; "in substance" means the property is tested, elsewhere and in another form.

| Invariant | Position | Evidence |
|---|---|---|
| **I1** one effective mask, composed before use | **covered as designed** | the viewport differential against the definitions oracle, driven across randomised acked-journal states, plus the overlay-journal differential with its two defective engines as negative controls. **And, since 2026-08-31, in its multi-view form** (`views.md` §1's factoring): one mask derived once in entity space from `terms/pairs.parquet` — `oracle.mask.mask_of` takes term ids and a path, so there is no view parameter for a view-dependent branch to hide in — met against four views' memberships, with `served(view) == mask ∩ members(view)` asserted as an **equality** per principal per view, and the union across every view equal to the mask. A negative control pins that the four views disagree with each other, so the comparison cannot pass against an engine serving one row space under four names |
| **I7** sampling happens after masking | **covered as designed — the best-covered invariant here** | the selection differential across the mask catalogue, with a first-*k* storage-order stub as a negative control that the differential must disagree with. **Contracts §2.6's within-tile order is now asserted in every view** *(2026-08-31)*, which is a claim about each row space separately — a build ordering the plain view correctly and a group's view by anything else passes a single-view check. That module runs θ **saturated** on purpose and adds no θ coverage: its subject is which entities a view may serve, and a live threshold would leave every equality in it a subset check |
| **I10** entity IDs never cross the trust boundary | **covered as designed — the most thorough test in the repository** | the byte-scanner (§4.3): wire, sub-cell stream and logs; identity-key and external-ID sweeps; C17 asserted positively; and a planted control on every sweep mechanism, alongside the real-traffic one. **C17's multi-view half is driven** *(2026-08-31)*: the view is not an input to the keyed bijection (`views.md` §9), so an entity present in several views is served the same `tessera_id` in each — asserted together with its control, that the same entity's **position** differs in every view, without which the identity half is satisfied by an engine answering four views from one permutation |
| **I9** entity IDs append-only, never reused | covered **in substance, in Rust rather than the suite** | allocator and commit-window property tests; no crash-replay or journal-divergence fuzz, since neither a second journal nor a router exists |
| **I11** row-space artifacts versioned together | **cross-request half covered in substance, in Rust; within-request half not covered** *(2026-08-30; was: not covered)* | The **cross-request** half was rebuilt in a form the pin's deletion did not take with it: every row-space artifact carries the generation it was built against, tested in both directions, and `crates/tessera-engine/tests/artifact_fold.rs`'s `a_merge_that_renumbers_extent_rows_disturbs_no_artifacts_count` drives a real merge permuting row space inside a prefix. The region cache states the same obligation at its own key (`crates/tessera-engine/src/region.rs`, `segments_version`) and `crates/tessera-engine/tests/region_leaf.rs`'s `a_region_answer_is_re_taken_at_the_generation_a_merge_renumbered_its_rows_in` discharges it — red when the key term is held constant, which is reachable because superseded decompositions are deliberately retained. The **within-request** half has no black-box surface and no white-box test; §4.4 records the two routes to covering it. **That half is a regression in coverage, not a reclassification** — the ~1,000 lines of pin tests went with the pin retention they tested (`geometry-pinning.md`), and nothing replaced them there. *(The whole row read "not covered" until 2026-08-30, written when the pin's deletion was recent and before the artifact side had rebuilt its half.)* |
| **I2** derived quantities are functions of visible data only | **covered as designed** | three canary fixture states compared as canonicalised bytes across all three served surfaces — tiles sorted, the points batch in served order, and the §3.3 density underlay's masked per-cell counts — with the visible-items state as a positive control the same comparator must reject, and which must move **every** surface (§4.2, §4.4). **Bounded to the surfaces it compares, and one served surface is outside them** *(r13; was: there is no labels batch)*: served artifacts travel on their own frame with a masked count, derived geometry and content, and the comparator does not read it. **The multi-view form is covered separately** *(2026-08-31)*: `conformance/tests/test_multiview_differential.py` compares masked tile counts per view against the oracle answering through that view's own permutation, Morton order and frame — three principals, four views, two viewports each, the plain view's frame and the group's deliberately different so a reader taking either for the other disagrees on the first tile |
| **I13a** a failed or cancelled request yields no partial answer | **covered incidentally, not by the designed test** | single-flight panic and cancellation tests in the engine and authorisation crates. This is the property the code annotates; the designed asymmetry test does not exist |
| **I13b** a partition not consulted fails closed | **not covered** | one hardcoded partition, no required-set gate, no test. A reviewer grepping `I13` finds only I13a's annotations and must not read them as covering this |
| **I3** labels gate on `M_auth` | **covered — both halves** *(r14; was: not covered — a testing gap)* | `conformance/tests/test_label_containment.py` over `oracle.label_fixture`: two principals **one entity apart**, that entity planted inside the widest generating set and nowhere else, and the difference asserted from the masked counts before anything rests on it. The artifact whose only content was drawn from that set is **absent whole** for the narrower principal — no identity, no count, no stripped description ([decision 0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)) — while an artifact of the same layer over the same membership is served to them **in the same response**, which is what makes the absence containment rather than the layer gate, the artifact gate or the existence criterion; the layer declares none of those. Ranked contents fall back rather than failing the artifact: one artifact serves its wide content to the wider principal and its narrow content to the narrower one. Checked at four zoom tiers. The **cache half behaviourally**: warm every tier a request touches on one token, suppress that one generating-set member, re-ask on the **same token** — the label is withheld at the ack, the ranked one falls back, and the response is now byte-for-byte the narrower principal's, since containment is a function of the mask and not of how an entity left it. ⊘ The pin is not re-presented (§4.4) |
| **I4** entity space and row space meet only at the permutation | **covered as designed** | three `trybuild` compile-fail rows with `.stderr` fixtures (§4.1); adding a `From<EntityId> for RowId` makes one fail. Proves the conversion is absent, not that no path derives a row ID by arithmetic — `check-layers.sh` carries that half |
| **I5** the two authorisation functions agree | **not covered** | passthrough plugin only; the differential oracle that would test it does not exist (§4.5) |
| **I6** authorisation comes only from the token | **not covered — nothing to test** | no plugin host, no sandbox. **Unchanged at r13**: there is no wasmtime host, nothing loads a guest module, and the only plugin a deployment can run is the built-in passthrough |
| **I8** generating sets immutable | **covered in substance, in Rust rather than the suite** *(2026-08-30; was: not covered)* | Both directions are now driven. **Growth**: `crates/tessera-engine/tests/artifact_growth.rs`'s `a_growth_does_not_enter_the_generating_set` publishes content with a generating set, grows the membership with entities a narrow principal cannot see, and asserts the joiners did not enter the set while the membership did — red against a fold that `or_inplace`s the joiners. **Deletion**: the fold tests cover the strict withdrawal and the permissive shrink, the latter from a principal that fails the surviving set rather than from full coverage, which contains every set including an empty one; `a_permissive_layer_withdraws_content_whose_last_source_the_fold_deletes` pins the limit case (decision 0107). §4.1's *compile-fail* form remains unavailable — immutability is a publication rule, not a type-level one — so this is covered in substance and cannot be covered as designed. *(The position read "not covered" until 2026-08-30 on the strength of a claim in this cell that the fold tests covered both arms: they covered the deletion arms, and `artifact_growth.rs` published no content at all, so nothing could observe a growth touching a set.)* |
| **I12** filters narrow rendering, never authorisation | **covered — mask half, in the form filter-surface §9 specifies** *(r9; was: not covered — nothing to test)* | the attribute-filter differential: engine against `reference/oracle/filters.py` — a per-entity walk over the fixture's planted values, never the `attrs/` artefact — across the mask catalogue's principals. Per tile `matched ≤ visible` with `visible` unmoved; the filtered served set equals the oracle's brute-force `M_sel` exactly; hidden, hollow and nonexistent values byte-identical in outcome (C11), with a single-member positive control; composition and the empty-combinator identities against brute force; unknown column `422`, unknown value not. The **frontier half** is *(r13)* a testing gap rather than absent machinery, and its form has changed with the mechanism: the frontier is withdrawn (decisions 0080, 0082, 0083), so there is no depth to move, and what stands is architecture §8.4's surviving half — an artifact's verdict, masked count and containment are unmoved by any filter, both tests running against `M_auth` alone. That is built and blind to the filter by construction, and undriven here — alone, since I3 moved at r14. Rule S over filter counts (surface §9) is also not yet driven; the test module's doc says why. **The mask half now covers a group-scoped operand** *(2026-08-31)*, which is the one filter leaf whose evaluation depends on a view at all: `views.md` §5's pinned leaf, bare under a view of its group and pinned by key — a view's only address (decision 0113) — and across views of one group, against an oracle that evaluates the **pinned** view's column in entity space and projects the result through the **request** view's membership — two views in one answer, so an implementation using either for both is caught. Per-view presence is a case of its own (decision 0064): an entity with a hole in the pinned view and a value in another matches nothing pinned at the first and everything pinned at the second. The frontier direction — `matched` moves, `visible` does not — is re-asserted there per view, and the two refusals §5 names are pinned: an unpinned scoped leaf under a plain view is a `422` naming the group, a pin naming no view of it is the unknown-view `404` |

The plan calls the suite the deliverable; the table above is where it stands. **It carried a tally of itself here and no longer does** *(2026-08-30)*: the count was maintained by hand, in three places that had drifted apart — this sentence, `CLAUDE.md` and the roadmap gave six, seven and five covered rows respectively — and it is readable off the rows it summarised. A row is the record; a count of the rows is a second copy that goes stale silently, and the reader who needs the number is already looking at the table.

**Views are not an invariant and get no row of their own** *(2026-08-31)*. `views.md` §11 asks this document for "a two-view differential: the oracle answers per view; the pinned-leaf cases, the gate-failed pin among them; the gate's work-indistinguishability", and what that buys is coverage of existing invariants in a new dimension rather than a new property — which is why the evidence sits in the I1, I2, I7, I10 and I12 cells above and not in a fourteenth row. `conformance/tests/test_multiview_differential.py` and `reference/oracle/multiview.py` are where it lives: four views over one entity space of 6,144 items in six compartments, the plain view holding all of them and a group of three holding different subsets, each view with its own frame and its own independently drawn layout.

**Four things that row asks for are not reached here, and each is an absent implementation rather than an undriven one.** The **gate** (`views.md` §6) is unbuilt, so there is no visible-view set, no gate-filtered `/v1/meta`, and no gate-failed pin for the undeclared-column refusal to be indistinguishable from — the fixture's every view is `public` because a corpus carrying a gated one would be a corpus whose expected answers no implementation can produce. The gate's **work-indistinguishability** is a timing property besides, of the class §4.2's C11 note already declines to assert in this suite. **The ingest-time join** (§4: a known `external_id` naming a view the entity is not in) is driven in `crates/tessera-engine`'s own tests against a real flush and not here, for the reason `test_filter_differential.py` gives for Rule S — a server accepting writes mutates the bundle under every other module in this suite; `crates/tessera-server/tests/views_write.rs` is where the arms are driven. And a **scoped category, text or rendered** family (§5's own marker) has no serving artefact for an oracle to disagree with. Each of the four is a row this document owes when its machinery lands, named here so the absence is a record rather than an omission.

Durability ordering is not an invariant row, and it is recorded here because it is the row a reader will look for and not find. The restart-replay module truncates the WAL to its last-synced offset (§5), which establishes that replay is correct when the unsynced tail is discarded — **not** that the engine never acks before it fsyncs.

**Two of the three ways that could break are now falsifiable in Rust** *(2026-08-30)*, on a pause site armed **inside** `Wal::sync_data` rather than at its call site — the construction `PauseSite::BeforeAck` already argued for, where the point travels with the code instead of recording where a line used to be. A test parked there has the sync still ahead of it and reads the `.sync` sidecar off disc: an engine that **publishes the offset before syncing** arrives with its lie already written and fails on the sidecar's own contents, and an engine that **drops the sync** produces no arrival at all, so the case fails on the absence rather than passing vacuously. Both are pinned by `crates/tessera-lifecycle/tests/wal.rs`.

**What survives is this paragraph's own example in its literal form.** A `sync_data()` whose body is a no-op, while the offset is still published, passes — and so does stripping every fsync in `wal.rs` while the functions stand. That is a property of the seam and not an adjustable test: **a syscall that did not happen has no in-process observer**, so no test inside this process can see it, and only the correctness suite's truncating crash variant reaches it. Issue #71 narrows with the gap: what an end-to-end check would now buy is the syscall's *absence*, not the ordering around it.

## 5. Interleavings, commands, and crashes

**Pause points park a thread holding no lock.** `before_fragment_insert` in particular sits outside the cache's insert critical section and its single-flight guard — a pause inside either would wedge the lifecycle thread's `ledger_state()` and a compaction force-refresh landing on the same key. This is a rule on the hook implementation, stated because the deadlock is otherwise discovered in CI.

**Pause points** (park a named thread on a channel): `after_wal_fsync` · `before_generation_swap` · `before_deny_retire` (the fold's retirement step — under Rule F it is the only retirement event besides an unsuppress) · `compaction_snapshot_taken` · `before_manifest_publish` · `before_current_flip` · **`before_fragment_insert`** (request thread — the retirement-floor refusal it was specified for is deleted with the floor; what it still holds is a build in flight across a publication, which is what Rule F's identity match has to survive) · **`before_fragment_acquire`** (request thread; drives §1.1's request-ordering interleaving in the lifecycle design).

**Commands** (feature-gated RPCs, distinct from pauses, because a pause can only wait): `evict_fragment(key)` · `ledger_state()` (overlay entry states — so retirement is *observed*, not assumed; a deny-retirement test that cannot see retirement passes vacuously. Its stamp-count and retirement-floor components are **void**: the ledger they would report is deleted from the spec, and what a fold-era test must observe instead is that the entry left the overlay in the fold's own publication).

A third, `fsync_offset()`, was specified here and **retired rather than built** (decision [0038](../decisions/0038-fsync-offset-is-a-sidecar-not-a-command.md)): the WAL's `.sync` sidecar already publishes the number durably, so the crash tests read it from disk. Retiring it does not buy the crash tests any evidential strength — see the crash-realism marker below, which is blunt that neither route establishes ack ordering.

> **⊘ Mostly specified, not implemented — and two things matter more than the count.**
>
> **`fsync_offset()` is not needed and will not be built.** The crash tests below require the WAL's last-synced position, and the WAL already publishes it durably: the sidecar `<name>.sync` holds an 8-byte little-endian offset, written write-tmp-then-rename and fsynced with its directory entry after every WAL fsync, because replay itself needs it (lifecycle; `wal.rs`, "the durable prefix"). The harness reads that file. A command would have added a feature-gated introspection surface to expose a number already on disk — and worse, would have had the engine report on the very property under test. **Two of the three commands remain unbuilt:** `evict_fragment` exists as a Rust method used by Rust tests and reachable from no HTTP route, so the Python suite cannot call it; `ledger_state()` does not exist, and the stamp ledger it was specified to report is **deleted from the spec** rather than unbuilt (write-path §5.4), so what it must report is the overlay's entry states alone — above.
>
> **A different pause mechanism already exists.** None of the eight pause points exists and the `conformance` feature does not exist. What exists instead is the write path's fault switchboard, with five pause sites (two on the ack contract, three at the publication seams), WAL append and fsync failure injection, and a step log, reaching a build only through self dev-dependencies or the declared, default-off `fault-injection` feature on `tessera-server`/`tessera-cli` — the correctness suite's faults build (decision 0071; lifecycle §7.3). Different names, a different gate, and a home in the engine rather than in `conformance/`. It is the same mechanism, three stages early, and it carries a rule this section does not state — **an injected failure must be indistinguishable from a real one in variant and in order.** The stage that builds this section must extend that switchboard — as the correctness suite's seam sites already have, which is the precedent to follow rather than re-argue. Building a second pause mechanism beside the first is the outcome this marker exists to prevent, and it is the natural one, because the two designs share no vocabulary.


**The scripts** (the plan's seven, plus one):

1. **Restart-replay, two variants** (a single script deadlocks — the ack follows the swap, so "kill after ack at `before_generation_swap`" cannot occur): (a) kill −9 after the observed 200, no pause; restart; assert every acked deny survives. (b) pause at `before_generation_swap`, kill; restart; assert replay applies the deny *and* no 200 was ever emitted.
2. **Deny-retirement window** — delete; hold a pre-deletion fragment via a live token; compact; `ledger_state()` confirms retirement actually occurred; assert invisibility throughout.
3. **Suppression persistence** — suppress; `evict_fragment` everything and force refresh; compact; `ledger_state()` confirms the entry never retired; assert invisible until unsuppress.
4. **Stamp regression, two tests — ⊘ void, the machinery is deleted from the spec.** The retirement floor both halves drive does not exist and will not (Rule S / Rule F, write-path §5.4); what replaces 4(b) is a test that a build begun before a fold cannot be inserted after it, refused by the fold's *identity match* rather than by a stamp comparison. The specified form is kept until that one is written, because the interleaving it exercises is the same one. *(As specified —* a single script that confirms retirement before the pause *and* completes it during the pause can never fire the floor refusal, fragments building from current postings, so post-retirement builds already sit at or above the floor. **(a) rebuild-excludes** — delete → retirement confirmed via `ledger_state()` → `evict_fragment` → pinned request misses the cache → assert the rebuilt fragment excludes the item (no pause needed). **(b) floor refusal** — `evict_fragment` → a request begins its build at stamp *e*, paused at `before_fragment_insert` → delete (tombstone *d* > *e*) → force-refresh so no live fragment predates *d* → retirement raises the floor (confirmed) → release → assert the insertion is **refused**, the builder retries against current postings, and the item is excluded. The paused, not-yet-inserted fragment is correctly absent from the stamp counts, so retirement proceeds without it.*)*
5. **Fold variant of 4(b) — ⊘ void, and now unreconstructable**: it inherits 4's deleted floor, the `predicate` op it drives is withdrawn (decision 0047 — edit is delete plus re-ingest; naming it is a 422), and the evaluate machinery it folded is **deleted** (decision 0048), so nothing anywhere can produce the entry it folds — not even a legacy WAL, there being no deployment. A re-label's own coverage belongs on the ingest path. *(As specified —* predicate change → compaction folds at *f* → entry retires → the paused pre-fold build's insertion at stamp < *f* must be refused; the retried build reflects the changed terms.*)*
6. **Post-snapshot tombstone** — pause at `compaction_snapshot_taken`; delete; resume; assert survival of the deletion through the fold.
7. **Positional CRC** — corrupt one byte **within [last Flush record, `fsync_offset()`]** ("below the fsync point" alone could land before the replay start and assert nothing); assert recovery fails closed. Corrupt past `fsync_offset()`; assert clean truncation.
8. **Request ordering** — pause a request at `before_fragment_acquire`; evict, publish geometry, swap; release; assert the request's own generation still governs and the response is correct — the eviction-while-held window driven explicitly, as the lifecycle design promises. A **merge** is the sharpest publication to drive it with: it permutes row space inside the merged span, so a request that mixed generations would read rows naming other entities (write-path §7).

> **⊘ Specified, not implemented — none of the eight exists as a script here.** Restart-replay exists in two forms, script 1(a) and the crash-realism variant below, but neither drives a pause point. Scripts 2 through 6 test the deny-retirement ledger, the retirement floor and the compaction fold. **The first two are deleted from the spec** (Rule S / Rule F, write-path §5.4) and must be rewritten against the fold's identity match rather than resurrected. **The fold is not the blocker it was**: it is built, derives its executed set from what the publication demonstrably removed, and retires against it ([`compaction.md`](compaction.md) §4), so scripts 2, 3 and 6 now wait on this suite's harness alone — `ledger_state()` for 2 and 3, `compaction_snapshot_taken` for 6. Scripts 7 and 8 test machinery that *does* exist and are the two that could be written today — with the qualification that **script 7's property is already covered in substance, in Rust**: `crates/tessera-lifecycle/tests/wal.rs` corrupts a byte below the sync point and requires a fail-closed refusal, corrupts past it and requires clean truncation, and covers the missing, short and zero-length sidecar variants besides. Rebuilding it in Python would buy the layout, not the coverage. Script 8 is genuinely absent and needs `before_fragment_acquire`.

**Crash realism (the sharpest finding against this design's first draft):** SIGKILL loses nothing — the page cache survives process death — so kill-based tests alone verify replay logic, not durability *ordering*; **an engine that acked before fsync would pass them all.** The falsifying variant: after the kill, **truncate the WAL to `fsync_offset()`** before restart — simulating lost unsynced writes — and assert no *acked* operation is missing. Environment: because power loss is simulated by truncation rather than depended on, the suite may run on any filesystem including tmpfs; that reasoning is recorded here so the first flake does not relitigate it.

> **Built.** The suite used to contain the test this paragraph pre-emptively rejects, and only that: SIGKILL with no truncation. It now carries both. The truncating variant kills, reads the last-synced offset from the WAL's `.sync` sidecar, truncates to it, restarts, and requires every acked operation to have survived; the SIGKILL-only test remains beside it, described as replay coverage rather than durability coverage, because that is what it is.
>
> Two assertions carry the ack-ordering property, and **both were demonstrated to fail rather than argued to be capable of it**. That no acked byte lies beyond the sync point is checked *before* the truncation, because "the deny is still there afterwards" has several possible causes and "the log was already durable to its end" has one; appending 64 bytes after the last ack — the shape an early-acking engine produces — fires it. That the truncated bytes are load-bearing is shown by truncating 40 bytes *below* the sync point, whereupon the server refuses to start with `wal corruption before the last-fsynced offset`. Without the second, an assertion that nothing was discarded could be an identity between two names for the same number.
>
> The `Published` token type and the fault-injection pause site inside the ack function (lifecycle §4, §7.3) still hold the property in Rust, at a narrower scope. The two are complementary: one shows the ack path cannot be written wrongly, the other shows the deployed binary does not lose an acked operation to a power cut.

## 6. What pass means, and where

**Every PR:** compile-fail rows, matrix at one seed batch, the eight scripts (conformance build), and the byte-scan with positive controls (feature-free build — the parenthetical binds to the byte-scan only). **Nightly:** full catalogue × rotating seeds on both builds; time-boxed fuzzing (wire decoder, manifest parser, plugin boundary); external oracles. **Release gate:** all green on the release commit + dependency-graph assertions (no JVM, no DuckDB, no `conformance` feature) + criterion budgets.

A differential failure is a defect until proven a fixture bug. The oracle changes only alongside a design or contracts revision, under review. **Design Appendix C4 (timing)** is measured nightly (per-query distributions split by mask sparsity) and published; a threshold waits for its Appendix C owner.

> **⊘ Partially implemented — the per-PR tier exists; nightly and release do not.** `.github/workflows/ci.yml` runs on every pull request and every push to `main`: `cargo test --workspace`, `cargo clippy -D warnings`, `check-layers.sh`, `check-doc-links.py`, and the conformance suite against a release binary. `check-layers.sh` therefore runs as a gate rather than from an opt-in pre-commit hook that any worktree without a track marker skips.
>
> **What that gate does not include, deliberately.** The compile-fail rows run inside `cargo test`, not as a separate step. There is no `conformance` build to run the eight scripts against, and no scripts. `reference/tests` is not run: two of its five modules build from the Phase 0 corpus, which is not in the repository — so the viewport differential's realistic term distribution is not enforced anywhere, and I1's CI coverage comes from the overlay-journal differential instead. Nightly and release stay unbuilt rather than half-built; they carry rotating seeds over the full catalogue, time-boxed fuzzing, external oracles, criterion budgets, and the dependency-graph assertions that keep a `conformance` feature out of a release binary, and a nightly running one seed batch is the per-PR gate with a worse schedule.
>
> Measured wall-clock on a developer machine, so a much slower runner is recognisable as a difference rather than as normal: `cargo test --workspace` 94 s, `pytest conformance/tests` 24 s from deleted fixtures.

## 7. Decisions

1. Black-box first; hooks are pause points + a **two**-command introspection RPC, feature-gated; **wire tests certify the feature-free build** — the two-binary split is documented, not hidden. *(Unbuilt; a different pause mechanism exists — §5. `fsync_offset()` left the command list rather than being built: the number it would report is already on disk, and a command would have had the engine report on the property under test.)*
2. The oracle implements definitions; its inputs are bundle + acked-control journal + the build's own points file, behind explicit barriers. *(Built, and its layering is enforced by a test. The third input arrived with cell-plus-residual geometry — §1; its binding to the bundle has not.)*
3. **Canonicalise-then-compare** for I2. *(Built — §4.2. The canonicalisation is not the handle→`fx_key` rewrite this decision named: decision [0006](../decisions/0006-per-session-handles-retired.md) retired the column that rewrite existed to defeat, so `tessera_id` is the join and the points batch compares as its own bytes.)*
4. `fx_key` join scalars replace any handle reverse map — no extra endpoint, no I10 tension. *(Planted, not served; pinned by a strict xfail — §2.)*
5. Positive controls for both pass-only tests: the scanner must catch a planted emission; the comparator must flag a visible-items state. *(Both built — §4.3, §4.4. The scanner keeps its real-traffic control alongside the plant; the two answer different questions.)*
6. Pause points + commands + **truncate-to-fsync-offset** over a simulation framework — the truncation variant is what makes ack ordering falsifiable. *(Truncation built, and it needed no command: the WAL's `.sync` sidecar already publishes the offset. Pause points and the other two commands unbuilt — §5.)*
7. Exact equality; ties broken identically by definition. *(Built.)*
8. The oracle is the second implementation of record, versioned with the design corpus. *(Built.)*
9. **Coverage is reported, not claimed.** §4.6 is the matrix of record, and a row moves only when a test moves with it.

## Appendix R — Review record

**r17 — 2026-08-31. The pinned-leaf cases lose their ordinal arm.** Ordinals are removed
([decision 0113](../decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md),
`views.md` r16), so the multi-view differential pins by key alone and the `@#n` case is now one of
the malformed ids the unknown-view `404` covers. The I12 cell says so. `conformance/suite`'s roster
check changes direction with it: where it asserted a group's ordinals were ascending and distinct,
it now asserts the field is **absent** from every served view — the shape that would say the
removed machinery had come back. No coverage row moves and no case is lost: the arm that went was
a second spelling of an address, not a property.

**r16 — 2026-08-31. The multi-view differential, and no new row.** `views.md` §11 asked this
document for a two-view differential and the pinned-leaf cases. They are written —
`conformance/tests/test_multiview_differential.py` over a second designed corpus,
`reference/oracle/multiview.py` — and the coverage they buy is recorded in the **existing** I1, I2,
I7, I10 and I12 cells rather than in a fourteenth row, because a view is not an invariant. What
each cell gained, and what the three uncovered clauses are, is stated below §4.6's table.

**The corpus is a second fixture rather than a widening of the catalogue**, and the reason is worth
carrying: the catalogue is built backwards from the mask *shapes* §2 enumerates, so adding views to
it would change every entity id in every mask case for a question that is not about mask shape. The
multi-view corpus is built backwards from §1's factoring instead — a mask that must not vary with
the view, and four views that must not agree with each other. Both halves are checked: the served
set equals `mask ∩ members(view)` exactly in each view, and a negative control fails if any two
views serve the same per-tile counts.

**Three findings from the doing, all in the oracle and all fixed there.** The oracle laid a view id
down as a **single** path component, so `quarter:2026-Q3` named a directory no multi-view build
writes; `Bundle.view_dir` now nests the two, as `tessera_store::view_path` does. `Bundle` held one
source geometry for the whole bundle, which is a bundle-wide reading of a per-view fact — a driver
now attaches one file per view and every geometry re-derivation asks for its own. And
`viewport.counts` decoded a bbox against `bundle.extent`, which on a multi-view bundle raises
rather than answering; it asks `extent_of(view_id)` now. None of the three could have produced a
wrong answer on a single-view bundle, and none would have survived the first multi-view test.

**One defect found outside the oracle and not fixed here** *(reported, `tessera-build`)*: a view
group's declared `text` metadata field is written into `MANIFEST.groups[..].metadata` as
`"ty": "int"` — the build's mapping from the declaration's scalar type matches `Utf8` and falls
through `text` and `keyword` to the integer arm. `/v1/meta` serves the type off the stored *value*
so a build looks right, and the roster's own type check is what would refuse a `text` value on a
view created while the service runs.

**r15 — 2026-08-20. The suite runs green, 432 of 432.** r14 diagnosed the 27 failures and recorded
that fixing them was a fixture rework nobody had done; this is that rework. Nothing in the engine
moved — the diff is eleven Python files, and every change is the fixture learning something about
the build it had been assuming.

**What the fix actually is, in one line each.** `Block.term_id` is the corpus's term id and
`Block.dict_term_id(bundle)` resolves the bundle's, because `public` at term 0 made them different
numbers for the same block. `Bundle.source_of_entity` carries the join that decision 0073's Morton
tiebreak broke, and the planted columns — filter, keyword, text, record blob, `fx_key` — are keyed
by entity through it rather than by source id under an equality that no longer holds. The overlay
differential strides its denied set, because entity ids now track the map and a prefix of them is a
region.

**The check that was missing is the one worth remembering.** `verify()`'s block check compares
posting **sets** against the block's entity range, and a within-block permutation preserves a set
exactly — so the check whose comment said it re-derived `entity_id == source_id` had never tested
it, and the day the identity went, this function reported green while every per-item join in the
suite silently compared one item's planted value against another's. **Check 3b** now compares the
two spaces item by item. A precondition elsewhere did catch its own case honestly — the overlay
differential refused to draw a conclusion from 23 tiles — which is the shape the rest of this wants.

⊘ `conformance/suite` was not run: it imports `tomllib` and needs Python 3.11+, which this machine
does not have. It is the correctness suite's shared battery rather than a row of this matrix.

**r14 — 2026-08-20. The first row to move because a test was written.** r13 recorded that I3's
machinery was built and undriven, and named the two halves §4.4 asks for. Both are now written, in
`conformance/tests/test_label_containment.py` over a new `oracle/label_fixture.py`, and §4.6's I3
row moves to **covered**. Three choices in it are worth carrying: the fixture is its own rather than
the mask catalogue's, because what this row needs is two principals a **single entity** apart and
the catalogue is designed backwards from adversarial mask *shapes*; the absence is checked against a
control in the same response, so a deleted containment check cannot read green; and the **pin** §4.4
asks to re-present is not, because decision 0041 made it advisory and never authorisation — the
token is what carries session state across an overlay change, and it is what the cache half holds
fixed. §4.4's row records that substitution at its site.

**Two things found in the doing, both recorded rather than fixed.** The harness could not spawn a
server at all — `tessera serve` has taken `--deployment` since the configuration rework and the
harness passed `-c` — so no module had run since that landed; that one line is fixed. With it fixed,
27 of 432 tests fail, for `public` at term 0 and decision 0073's Morton tiebreak, neither of which
`verify()` catches: its block check compares posting *sets*, and a within-block permutation
preserves a set, so the check that says it proves `entity_id == source_id` does not. §0 carries the
diagnosis and what a fix would need. **A coverage row is not claimed on a green suite, and this one
is not**: the module moving I3 passes on its own fixture and does not touch the catalogue, which is
why the row moves while the suite stays red.

**r13 — 2026-08-19. A correction, in r10's shape.** This document said in five places that the
machinery behind I3, I8 and I12's frontier half does not exist: *no label service*, *no generating
sets*, *no labels batch*. It exists. Artifacts carry ranked contents with generating sets;
containment is `|G ∩ M| == |G|`, all or nothing, evaluated on every route that serves an artifact;
the existence criterion is a live control; a served artifact reaches a client on its own frame with
its masked count, its derived geometry and the one content that viewer contains. §0, §4.1, §4.2,
§4.4 and §4.6's reason column are corrected.

**The frontier is the third case, and it is not the same case.** "There is no frontier" is true, and
true for a reason the old sentence did not mean: the frontier was **withdrawn** rather than left
unbuilt — every artifact is tested on its own and the root-down descent is gone (decisions
[0080](../decisions/0080-the-frontier-is-a-per-artifact-test.md),
[0082](../decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md),
[0083](../decisions/0083-the-frontier-is-a-request-time-budget.md)), and the depth that remains is a
request-time budget, which is not a disclosure control. So §4.4's *frontier-depth property under
filters* names a test of a thing that no longer exists. What survives of I12 on the artifact side is
architecture §8.4's half: a filter never touches containment and never relaxes the criterion, both
running against `M_auth` alone. That is built, and blind to the filter by construction.

**I6 is untouched, and was checked rather than assumed.** There is no wasmtime host: nothing loads a
guest module, and the only plugin a deployment can run is the built-in passthrough. Its row stands
word for word.

**No coverage row moves** — those rows are invariant coverage, no test moved with them, and decision
9 governs. What moves is the arithmetic of the reasons: **one** uncovered invariant now stands for
want of an implementation where three did, and I3, I8 and I12's frontier half are gaps in this
suite. A suite that records built machinery as absent understates its own gap, which is the whole
of what this revision repairs.

**r12 — 2026-08-19. One word.** §4.6's I12 row named the frontier-depth threshold
`min_visible_members`, a config key that is deleted; the control it names is the **existence
criterion**, declared per layer ([decision 0085](../decisions/0085-the-existence-criterion-has-no-deployment-wide-form.md),
`annotations.md` §5). No coverage row moves and no status claim changes.


**r11** (2026-08-15) refreshes §5's marker to what exists after decision
[0071](../decisions/0071-fault-injection-reaches-a-served-binary-by-its-own-build.md): the fault
switchboard's pause sites went from two to five (the correctness suite's three publication-seam
sites landed **by extending the switchboard**, the route this marker demands), and its gate is no
longer "only through a self dev-dependency" — the feature is declarable on
`tessera-server`/`tessera-cli` for the faults build, and the guarantee is now "no default-features
build carries it". §5's own eight pause points remain unbuilt and the marker's instruction stands
unchanged; nothing else in this document moves.

**r10** (2026-08-14) is a correction. This document said in three places that the compaction fold
does not exist; it is built, normative and reviewed three times against its implementation
([`compaction.md`](compaction.md) r11), it derives its executed set from what the publication
demonstrably removed, and it retires against that set. §0, §2's fixture catalogue and §5's script
marker are corrected.

**One characterisation changes, and it changes against this suite.** §0 previously grouped scripts
2, 3 and 6 with the invariants that have nothing to test, under "none of that is a testing gap".
With the fold built that is no longer true: the machinery exists, this suite does not test it, and
the only thing in the way is the pause-point-and-command harness §5 has specified since r1. They
are named as a gap. **No row of §4.6 moves** — those rows are invariant coverage, no test moved
with them, and decision 9 governs.

The second post-deletion state (executed and retired) is now reachable, so §2's catalogue names
both states and records that it carries only the first.

**r9** (2026-08-09) moves one row of §4.6, with the test that moves it: **I12's mask half is
covered**. The filter surface landed (decision 0062; contracts §3.2 r26), and
`conformance/tests/test_filter_differential.py` runs the differential in the form
[`filter-surface.md`](filter-surface.md) §9 specifies, against a new definitional oracle module,
`reference/oracle/filters.py` — a per-entity walk over the **fixture's own planted values**,
which keeps it a second implementation: the engine reads `attrs/`, the oracle reads what the
synthesised corpus was given, and the two meet only at the served surface (the same construction
as the geometry input, §1). The mask catalogue's corpus gains two `filter`-only columns —
a `per_viewer` category and a string column (`utf8` then, `keyword` since that family replaced it) —
deliberately **decorrelated** from the grant structure, a precondition the suite asserts rather than assumes, because a correlated fixture
passes every cross-principal check while testing nothing. Surface §9's adversarial value shapes
are partially planted: a hidden value, a hollow (declared, memberless) value, a single-member
value; container-straddling membership comes free of the cycling values. Not planted: values
whose only member is deleted or suppressed (Rule S over filter counts — needs the overlay
machinery's private-bundle servers) and tier-straddling values (no attribute ingest exists).
§4.4's I12 row — the frontier-depth form — is untouched and still blocked on the label service,
with I3; the row's coverage claim names the distinction. One divergence was recorded when this
suite was written and is now resolved rather than pinned: a cross-family operator (`prefix` or
`contains` on a category) answered as an empty operand where `match` on the same surface refused
`422`. Contracts §3.2 rules it — an unknown column and an operator outside the column's family are
both `422`, an unknown *value* is an empty operand, the split being which side of the trust
boundary the fact lives on — and the differential asserts the `422` rather than carrying an xfail.
The report's second case, `in` on a string column, was itself the bug: `in` is `eq` over a list
rather than a category-only generalisation, so a string column takes it and it is no cross-family
operator at all.

**r8** (2026-08-06) applies decision
[0048](../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md). Script 5 was
already void twice over — it inherits script 4's deleted floor, and the `predicate` op it drives
is withdrawn — and r7 still allowed that "only legacy evaluate entries from pre-0047 WALs will
ever meet a fold". There are no such WALs, and the machinery that would have read one is deleted,
so the script is void a third time and **unreconstructable**: no state any deployment could reach
produces the entry it folds. §2's oracle-input note drops its predicate-change example for the
same reason. **No coverage claim moves** — a void script covered nothing before this and covers
nothing after it, and §4.6's matrix is untouched.

**r7** (2026-08-04) carries [`write-path.md`](write-path.md)'s promotion into §5, and it
**removes** obligations rather than adding any. Two of the five unwritten interleaving scripts
tested a deletion stamp ledger and a fragment-insertion retirement floor; both are **deleted from
the spec**, not deferred (owner-ruled 2026-08-03 — Rule S / Rule F at write-path §5.4), so they
must be rewritten against the fold's identity match rather than resurrected, and `ledger_state()`'s
stamp-count and floor components are void with them. The compaction fold the other three need is
still unbuilt, so the five remain unwritable — but for one reason now instead of three, and the
one that remains is honest.

A second pass over the same promotion caught four more sites the first missed, all of the same
kind — machinery named at the claim that the ruling had already deleted. §2's fixture catalogue
asked for "post-deletion states at every ledger stage", of which there is now exactly one; §5's
deadlock rule and its `before_fragment_insert` entry still named the retirement scan and the floor
refusal; and **script 5 is void twice over**, because the `predicate` change it drives is
withdrawn as well (decision 0047), so nothing a post-0047 deployment can do produces the entry it
folds. Scripts 4 and 5 are marked ⊘ at the claim rather than only in the block below them, and
script 8 gains the observation that a **merge** is now the sharpest publication to drive it with.

**One coverage claim changes, downwards, and it is arithmetic rather than evidence.** §4.6's
summary line read "two in substance, six not covered" while the table above it listed one and
seven: I11 moved out of *in substance* at r6 and the line did not follow it. §0's "six invariants
remain uncovered" undercounted the same way. Both now match the table, and I11 is named in §0 as
the one uncovered row that is a genuine gap in this suite rather than a missing implementation.

**Nothing moves upwards.** The engine's new maintenance tests (`coalesce`, `merge`, `soak`,
`projection_patch`, `rotation_e2e`) discharge write-path §14's obligations, not §4.6's rows: no
row's position changes, and I11 in particular stays a **negative result** — the pin that carried
it is deleted (decision 0041) and neither replacement test exists. What did change about I11 is
its route: §4.4 said the boundary test needs compaction, and a **merge** publication now moves
row space, so the route waits on a pause point alone. §1's barrier note records that the
entity-space coalesce is the one publication `segments_version` cannot see.

**r6** records the falsifiability epic (#11's first, third and fourth gates). Four rows of §4.6 move and each moved with a test. **No test form was weakened, and one was retired as redundant:** script 7's positional-CRC property is already covered in Rust, in both directions and with the sidecar variants besides, so it is recorded as covered in substance rather than transcribed into Python for the sake of the layout.

Three findings this revision produced that the design did not anticipate, all in the direction of less machinery:

1. **§4.2's canonicalisation was specified against a column that no longer exists.** The handle→`fx_key` rewrite exists because per-session handle bytes made raw comparison impossible; decision 0006 retired that column two revisions before this document was last touched. `tessera_id` is the join, and the points batch compares as its own bytes. `fx_key` stays planted-but-unserved, and its xfail stays the marker.
2. **`fsync_offset()` did not need building.** The WAL's `.sync` sidecar already publishes the offset durably, because replay needs it. A command would have exposed a number already on disk *and* had the engine report on the property under test. It leaves the command list; the `conformance` feature is not required for crash realism, only for the interleavings.
3. **The catalogue could not supply the byte-scan's grant set.** Moving the scan off the Phase 0 corpus required a grant admitting and denying entity IDs above the scan's floor, and `filler_tail` was the only block straddling it — so every ID above the floor was admitted or denied together. The layout gains a `high_tail` block with no `MaskCase`, existing solely for that, and `verify()` refuses a corpus that stops satisfying it.

**r6 was independently reviewed under three lenses — invariant evidence, falsifiability, and prose against code — and four of its claims did not survive.** Recorded here rather than quietly fixed, because each was a claim this document made about its own strength:

1. **The crash test does not falsify ack-before-fsync, and §5 said it did.** Stubbing `sync_data()` to a no-op, so the WAL is never fsynced while the offset is still published, leaves both restart-replay tests passing. `discarded == 0` compares the engine's own published offset against the file size — it catches a sidecar that stops advancing, not a prefix that was never synced. §5 and decision 0038 now say so, and #71 tracks whether an end-to-end check is worth building.
2. **The canary comparator pinned no surface.** Returning one concatenated blob meant dropping the points batch left every canary test green, and so did dropping the tile batch: the control fired on whatever remained. §4.2's "explicitly including the points batches" was enforced by nothing. The comparison is now three separately-addressable surfaces and the control requires all three to move.
3. **The canary never asked for the §3.3 underlay**, whose per-cell masked counts are the only derived aggregate in the system besides tile counts — so an I2 defect confined to that path would have moved nothing this compared. §4.6's I2 row claimed no such aggregate existed. Both fixed.
4. **The x/y plant sat at lane 0**, so it was caught by any stride dividing 8 and did not pin the straddling window it exists to justify; it also never wrote to `y`. Both sabotages passed. The halves now straddle lanes 1|2 and each column is planted separately.

**What r6 did not change:** every uncovered row's reason. Six invariants were uncovered at r5 and five are now; the one that moved (I4) moved because a harness was built, not because the system gained a feature. I3, I6, I8 and I12 still have no implementation to test, I5 still needs a plugin whose two functions can diverge, and I13b still needs a required-set gate.

**r5** applies three owner rulings. No test form changes and §4.6's matrix is untouched.

**The differential has a stated prerequisite** (decision [0028](../decisions/0028-postings-requirement-and-the-pair-relation.md)): the bundle under test must carry `terms/pairs.parquet`. It is optional for a serving deployment and required here, because it is the flat relation the oracle scans while the engine unions postings — without it the second implementation has nothing to work from and I1's coverage disappears. §3 says so.

**The canary comparison's reliance on determinism is argued rather than assumed** (decision [0030](../decisions/0030-determinism-is-not-a-guarantee.md)). Byte-identical responses across thread counts are a documented implementation detail of the engine, not a guarantee (design §10.4), so a suite leaning on them needs a reason. It has one: **the suite pins its own configuration**, and relying on determinism at a fixed thread count is far weaker than relying on stability across them. §4.2 states it, because the first person to run the suite at a different thread count would otherwise get an unexplained failure. Canonicalisation, unbuilt at r5, was what would remove the dependency; it was built at r6 and does not, because it canonicalises order rather than serialisation — the dependency on determinism at a pinned thread count stands.

**I5's oracle route is open** (decision [0027](../decisions/0027-i5-is-unverified.md)). §4.5 keeps its design, and its marker now records that the design specification no longer elects `accumulo-access` — an external implementation and a second implementation written alongside this suite are both live, and both need a plugin whose two functions can genuinely diverge before either can be built.

r1 was independently reviewed (verdict: needs-rework — architecture right; the two central mechanisms unimplementable as specified). r2 resolved all twelve findings: canonicalisation replaced raw byte comparison, with the canary allocation rules stated; the `fx_key` join replaced the reverse map; oracle inputs split into bundle + acked journal with barriers; restart-replay split into its two coherent variants; two request-path pause points, three commands and the eighth script added; truncate-to-fsync-offset made durability ordering falsifiable; I3's cache half given its behavioural black-box form; positive controls added for scanner and comparator; I11's second clause restated in drivable form; CRC corruption bounded to the live replay range; container-boundary masks achieved via sparse allocation. r3 split the stamp-regression script into the two tests whose order actually exercises the floor refusal, added the pause-outside-locks rule and the labels-batch canonicalisation key, scoped the barrier to async operations only, and retired the C17 decorrelation check.

**r4** is the audit pass against the built suite. **No design decision was changed and no argument withdrawn**; what changed is that the document now reports what exists. §0 is new and leads with the measured position; §4.6 is new and is the coverage claim of record; decision 9 is new and says coverage is reported rather than claimed.

Marked **⊘** in r4: the `conformance` build split (preamble); the directory layout and three of five differential families (§1); `fx_key` service (§2); property-based generation on the differential (§3); compile-fail rows (§4.1); the canary canonicalisation (§4.2); the comparator's third fixture state (§4.4); external oracles (§4.5); the pause points and two of three commands (§5); crash realism (§5); the whole of §6.

Three findings r4 produced that the design did not anticipate — **the first two were closed at r6**, and they are left as written because a review record that quietly drops what it found stops being one:

1. **The suite contains the test its own §5 pre-emptively rejects.** Restart-replay is SIGKILL-only — the variant this document calls insufficient, in the words "an engine that acked before fsync would pass them all". Marked at §5.
2. **The canary comparator has no proof it can fail**, and the other two differential suites carry theirs. That makes it a specific omission rather than a suite-wide habit, which is why §4.4's marker names the two controls that do exist.
3. **A second pause mechanism is about to be built.** The write path's fault switchboard is §5's mechanism under different names, in a different crate, behind a different gate, and three stages early. §5's marker and lifecycle §7.3 both say so, in both directions, because either document read alone leads to the duplicate.

One divergence resolved in the suite's favour: **the byte-scanner exceeds its design** on reach (identity-key and external-ID sweeps across wire, sub-cell stream and logs; C17 asserted positively) while falling short on falsifiability (its negative control demonstrates the byte-window mechanism on a real transmitted `tessera_id` rather than on a planted entity ID). Both halves are recorded at §4.3 rather than netted off, because they answer different questions.
