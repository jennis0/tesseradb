# The consistency pass

**Status:** the sole authority for this pass's status, by owner direction (2026-09-11). The plan
is [`evidence/memos/2026-09-11-duplication-and-consistency-pass.md`](evidence/memos/2026-09-11-duplication-and-consistency-pass.md);
the capabilities it surfaced and does not build are in
[`evidence/memos/2026-09-11-capability-gaps.md`](evidence/memos/2026-09-11-capability-gaps.md);
the rulings are [decision 0140](decisions/0140-the-consistency-pass-rulings.md); the rule they
apply is [decision 0139](decisions/0139-one-implementation-between-build-and-ingest-and-across-a-type-family.md).
A step's status moves in the change that moves the work. Each step is one worktree off the
seam, under [`agents/parallel-work.md`](agents/parallel-work.md), except step 0, which is done
directly.

## Order

| # | Step | Lands | After | Size | Status |
|---|---|---|---|---|---|
| 0 | seam | `Default` for `BuildArgs`, `ViewArgs`, `LayerDeclaration`, `EngineConfig`; a `SegmentsManifest` constructor; `Overlay::touches` at every site; the build's `ABSENT_CODE` deleted; `Cargo.toml`'s binary count | | small | not started |
| 1 | T1a, one rule at both doors | memo appendix A, T1a: the attribute compiler, the reserved sets and every declaration validator in `tessera-types`, both entry points calling them; keyword admission; the both-doors test, one case per register row | 0 | small | not started |
| 2 | T1b, the declared shapes | memo appendix A, T1b: metadata carries its width (ruling B); the layer gate a list through the plugin (ruling I); the view-resolution and manifest `with_*` collapses; one manifest version bump | 1 | medium | not started |
| 3 | T5 and G, the test surface | memo appendix A, T5: `tessera-testkit`, then build and store, engine, and server after step 2 merges; the corpus check widened to dev edges | 0; server tests after 2 | large, low risk | not started |
| 4 | T2, one placement rule | memo appendix A, T2: nineteen items, from the placement predicates to the batch parsers; `stream!` and the build's writers survive | 2 | medium | not started |
| 5 | E, one cache | memo appendix A, E: `tessera-cache`; the fallible form survives, one test suite | 0 | small | not started |
| 6 | F, one measurement crate | memo appendix A, F: the examples become bench binaries over one sweep scaffold | 0 | small | not started |
| 7 | T3, one family table | memo appendix A, T3: eleven items, from the type move to the record kinds table | 4 | medium | not started |
| 8 | T4, one skeleton per shape | memo appendix A, T4: twelve items, from the fold writers to the scoped-entity trait | 4, 7 | large | not started |
| 9 | T6, the record | the capability map's disagreement table; `system-architecture.md` §3 for the moved types; the design amendments each ruling implies; every comment the tracks made false; the stale `artifact-delivery.md` citations | 8 | small | not started |
| 10 | close | the bundles rebuilt once for the manifest and WAL changes of steps 2 and 7; the full check list in `CLAUDE.md`; the demo run | 9 | | not started |

Steps 4, 5 and 6 run together; their files are disjoint. Step 3 runs beside steps 1 and 2 on the
crates they do not touch.

## Why this order

Steps 1 and 2 first because the register is the live defect list: an attribute named `record`
addresses the record blob's directory, an empty keyword becomes a dictionary key, a metadata
value past its width is stored, `inherited` is accepted as a label, and a layer gate bypasses the
plugin. Each is closed by moving one rule down, and the both-doors test then holds the register
closed. Step 2 is separate from step 1 because it changes the manifest format, and the bundles
are rebuilt once for it and for step 7 together, at step 10.

Step 3 next because every later step edits tests; consolidating first means they write against
the shared harness, and fewer binaries make every later check run faster. It carries no risk to
the product.

Step 4 before step 7 because both would edit `flush.rs` and `pipeline.rs`, and step 4 closes the
second defect class, the placement and width rules whose disagreement makes a bundle one reader
cannot open. Step 7 then moves the type definitions and rebuilds the family tables on a settled
placement rule.

Step 8 last among the code steps because `write.rs` is 17,000 lines and the collapses there save
the most lines for the least correctness gain; it wants steps 4 and 7 settled so it edits each
region once.

Step 9 after the code so the documents describe the built system once. Step 10 runs the full
check list once, as one campaign.

## Record

Rulings taken during the pass go to `decisions/`; measurements to `evidence/memos/`; leftovers are
fixed, or one issue, or deleted, in that order.
