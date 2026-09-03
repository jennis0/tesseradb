# A change to a large level's records rebuilds its row form on the next request — and a flush leaves ingested rows out of it

**Status:** problem statement with the fix ruled, 2026-09-03. Found by the measurement campaign's
ingest cycle on rung 3 (MedCPT) at 3.6×10⁷ rows; the trigger was first misattributed to the
flush and settled by `probes/2026-09-03-growth-trigger/` the same day. The fix below is agreed
with the owner and **⊘ not yet built**.

## The problem

Two defects, one level. Both were seen on `mesh/descriptors` — 30,217 artifacts, a 1.66×10⁹-row
membership, served row-major.

**1. Any change to the level's records discards its held row form, and the next request rebuilds
it from scratch.** A stored level's row form is cached under the prefix, the view and the level's
*version*, and the version moves on every publish, growth or retirement
(`ArtifactStore::bump`). On a miss the engine asks the prefix for the fold-written column; that
column is keyed to the version it was written at and is refused once the version has moved
(I11: never adapted), which is correct. What follows is the full route — project every membership
through the base permutation and compose the row-major list column from the result — inside the
request, and at this level's size that is **94–113 s** in the campaign and **177 s** in the probe,
against `serve.stream_deadline_ms` = 60 s (decision 0060). The stream is shed after the build
finishes and the second attempt serves from the cache.

The trigger in the campaign was the ingest driver's own layer probe: one row carrying three
descriptor keys, ingested and deleted before the run began. One growth, one artifact touched, the
whole level rebuilt. The probe reproduced it with no flush at all.

**2. A flush publishes rows the stored level's form does not contain.** The form projects the
**base rows only** (`MembershipRows::put` → `project_base`), for memberships and generating sets
alike; the flushed segment's rows lie above the base and nothing projects them into the form until
the fold rewrites the level. An ingested membership is therefore absent from its artifact's masked
count between the flush and the fold — hours under the nightly gate (compaction §9). This is a
0091 gap (build and ingest are one functionality) whatever the latency of the first defect, and the
spatial path already does not have it: a flush resolves its segment against every spatial level in
its own unit of work, before the swap (`crate::shapes`, polygon-membership §6.3).

## What this is not

- Not a wrong answer once served: the counts are exact for the base rows (the 0091 census on the
  folded deployment is exact at zoom 0 and on every box).
- Not the flush's cost: the first defect opens at the record change, whenever the next layered
  request arrives, flush or no flush. The memo's earlier title said otherwise.
- Not the fold's problem: the fold re-runs the artifact pass and both defects close at it.
- Scale-dependent: at 10⁶ rows the rebuild is seconds. The row-major composition and the
  projection were not separated; the cold-start probe put the projection alone at 23 s.

## The fix, as ruled

The cost is a wholesale rebuild for a change that touched one artifact. The shape of the fix is
the one the spatial path already has: a form maintained by the same operations that change the
level, never rebuilt because something moved.

- **(a) Apply the delta to the held form.** The store knows the exact change per ordinal — a
  growth is one artifact and a joining entity set, a publish one new ordinal, a retirement one
  slot dropped. The engine applies the same change to the held `ArtifactRows`: project the
  joining set (O(its members)), OR it into that ordinal's row bitmap, and where the level is served
  row-major set the label at those rows. The version still bumps and the persisted file is still
  refused across a restart; the in-memory form tracks the version because it applied the change.
  Removes the rebuild rather than moving it.
- **(c) A flush produces a piece for every stored level, as it does for every spatial one.** In
  the flush's unit of work, for each artifact, the members that fall in the new segment are
  projected through the segment's own permutation; the generation joins the piece with the row
  base applied, as `ShapeLevel::joined` does. Small, on the pool, before the swap. The flush
  becomes the second place a piece is produced and the ingested rows reach their counts at the
  flush rather than at the fold.
- **(b) The fallback: whatever rebuild remains is warmed at the publication, not at the request.**
  A level with no held form, or one the fold flipped, is built on the pool when the generation
  moves and the previous form is served meanwhile, as the mask cache already does across a
  generation move. The open-time warm (`Engine::warm_artifact_projections`) is the model. A safety
  net under (a), not a substitute for it.

Running the build's artifact pass at every flush — the direction the first draft of this memo
implied — would write gigabytes per flush for the same rebuild and is declined.

## Where the evidence is

`probes/2026-09-03-growth-trigger/` (the reproduction, no flush), `probes/2026-09-02-cold-start/`
(the projection's cost alone), `test_corpora/common/ingest_cycle.py` (`probe_layers`,
`probe_layers_after_ingest`), `data/ladder/.measure/medcpt36/ingest-0.10.json`
(`layers_after_ingest`, `served: false`), `crates/tessera-engine/src/artifacts.rs`
(`ArtifactProjections::get_or_build`, `MembershipRows::put`), `crates/tessera-engine/src/shapes.rs`
(the per-segment piece pattern), `crates/tessera-lifecycle/src/membership.rs` (`bump` and its
callers), `docs/decisions/0094-…`, `docs/decisions/0060-…`, `docs/decisions/0091-…`.
