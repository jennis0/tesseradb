# After a flush, a level's row form is rebuilt on the request path — handover

**Status:** problem statement, 2026-09-03. Found by the measurement campaign's ingest cycle on
rung 3 (MedCPT) at 3.6×10⁷ rows. No solution is proposed here.

**Built 2026-09-03**, on the owner's ruling, in `crates/tessera-engine/src/artifacts.rs` — whose
module doc is the account of what is maintained and how:

- a **growth** or a **publication** applies its own delta to every held row form of the level it
  moved and moves the form's key with it (`ArtifactProjections::bring_forward`), from
  `Executor::commit_growth`, `Executor::commit_artifacts` and the ingest window's close. The level
  is no longer projected again by the request that follows a write;
- a **flush** extends every held form of its view by the segment it published
  (`ArtifactProjections::extend_flushed`), so a form covers the whole row space rather than the
  base alone and an ingested member counts from its flush rather than from the next fold;
- a form is checked against the row space at every cache hit (`ArtifactRows::covers`), which is
  what a *merge* — the one publication that renumbers extent rows — is caught by;
- the **row-major column takes the delta too** (`RowColumn::with_added`) rather than being composed
  again over the amended form. Composing it again cost ~100 s for one entity joining three
  artifacts at rung 3's `mesh/descriptors`, on the executor thread, where it blocks every ingest
  and every deny.

⊘ **(b), the warm at publication, is not built**, and every drop path that remains still puts the
whole-level projection on the next request: a **merge** (which renumbers the extent rows a form now
holds), a form at a level version its delta does not follow, and a form under another prefix. Each
is said at `warn` where it happens.

**Measured at rung 3** — `probes/2026-09-03-growth-trigger/after-the-fix/`, which carries the runs,
the log lines and the reason the bundle a run touches cannot be run against twice. Both binaries on
the same host over the same hour, each against its own copy of the all-in bundle:

| | before (882e46cb) | after |
|---|---|---|
| open | 28.1 s, 27.1 s | 24.6 s |
| growth returns | 0.01 s, 0.23 s | 0.04 s |
| the request after the growth | **shed at 134.3 s, 100.7 s** | served, **125 ms** |

The growth is the number to watch rather than the request: the work has moved onto the executor
thread, where it blocks every ingest and every deny, so what matters is that it stayed small. The
server says what it cost — `a level's held row form took a write's delta`, at `elapsed_ms=15`
against the level's 14 s projection.

⊘ **A copy of the level is still possible and is what that line's `cloned_ms` reports.** A write
amends a form requests are reading, so `Arc::make_mut` copies it; the memberships are behind one
`Arc` each, which made that copy 30,217 pointers (**9 ms** measured) rather than 1.66×10⁹ entries
(**2 576 ms** measured, before the change). The records, the generating sets and the tile index are
still copied whole, and nothing bounds that beyond their being small at this rung.

⊘ **The bundle a probe run leaves behind cannot adopt its own row column again**, on either binary:
a growth moves the level's version, the side manifest the next tick writes carries forward only the
derived extents whose version still matches (`Executor::artifact_coordinates`), and the fold-written
column is not one of them. A second run against the same bundle therefore opens with
`row_columns_named=0` and projects `mesh/descriptors` whole instead of transposing it — ~147 s. That
is **I11** doing its job rather than a regression, and it is what a bundle looks like after any
growth, not only after a probe.

## The problem

Decision 0094 puts the choice of a level's serving layout — and the writing of the derived row
form it needs — **at the build, re-evaluated at the fold**. The post-bundle artifact pass
(`tessera-build/src/artifact_pass.rs`) exists because, before it, "the first request naming such a
level built the row form *inside the response* and was truncated at the 60-second whole-stream
deadline" (its module doc, citing the artifact-scale campaign's finding 2).

A **flush** publishes a new segment and does not run that pass. The segment carries no adopted
artifact structures for its rows, so the first request that names a level over it composes the
level's row form on the request path — exactly the state 0094 removed from the build. Measured on
the 10% ingest cell (base 32.3M rows built, 3.6M ingested, flushed, not yet folded): a zoom-0
whole-extent viewport asking `layers: "all"` was **shed mid-body at 113 s** on the first attempt
and **94 s** on the second, against `serve.stream_deadline_ms` = 60 s (decision 0060). The driver
now records that request as its own failure-tolerant measurement (`layers_after_ingest`,
`served: false`) and its visibility poll asks for no artifact frames; the fold that followed took
110 s and the same request then served.

The same cost was seen from the other side by `probes/2026-09-02-cold-start/`: a fresh process
paid the projection build on whichever request came first (tens of seconds), and
`Engine::warm_artifact_projections` moved it to open — for the bundle's own segments. A flushed
segment is not covered by that warm.

## What this is not

- Not a wrong answer: the counts, once served, are exact (the 0091 census on the folded
  deployment is exact at zoom 0 and on every box).
- Not the fold's problem: the fold re-runs the pass and the request serves afterwards. The
  window is between a flush and the next fold — which under the nightly gate (compaction §9) is
  hours, during which every first request on a large level over the new segment is shed.
- Scale-dependent: at 10⁶ rows the same request served in seconds. Whether the cost is the
  level's size (30,217 artifacts, a 1.66×10⁹-row membership) or the segment's is not separated.

## Where the evidence is

`test_corpora/common/ingest_cycle.py` (`probe_layers_after_ingest`), the ingest cell's raw output
under `data/ladder/.measure/medcpt36/`, `test_corpora/medcpt/measurements.json`,
`crates/tessera-build/src/artifact_pass.rs` (module doc), `crates/tessera-engine/src/viewport.rs`
(`warm_artifact_projections`), `docs/decisions/0094-…`, `docs/decisions/0060-…`,
`docs/design/write-path.md` §4, `probes/2026-09-02-cold-start/`.
