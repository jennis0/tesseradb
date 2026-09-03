# After a flush, a level's row form is rebuilt on the request path — handover

**Status:** problem statement, 2026-09-03. Found by the measurement campaign's ingest cycle on
rung 3 (MedCPT) at 3.6×10⁷ rows. No solution is proposed here.

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
