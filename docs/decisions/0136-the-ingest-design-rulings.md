# 0136 — The ingest design: seventeen rulings, one model

**Date:** 2026-09-07 · **Status:** Settled (owner ruling) · ⊘ not yet built; tracks T0–T7 in
[`ingest.md`](../design/ingest.md) §8

## What this answers

Decision 0134 ruled that anything a build can create, live ingest can create and extend at any
scale, and asked for a design pass over ingest under that rule. [`ingest.md`](../design/ingest.md)
was drafted, reviewed under three lenses (security, performance, implementability), revised, and
re-reviewed once on the sections whose shape changed. The owner ruled on the first round's
findings and on the draft's ten open questions; this records those rulings so they exist outside
the review thread. Two standing preferences governed every choice: **simplicity and
understandability over cleverness**, and **minutes of ingest-to-publish latency is acceptable for
large data**.

## The rulings

1. **JSON is the default wire for every kind**, newline-delimited or an array of objects, coerced
   against the declared column types with a refusal naming row and column; integers parsed exactly;
   Arrow optional by content type on the same routes. Decision 0133 holds at the JSON door.
2. **Every route publishes a record count beside its byte cap**; both are pagination units, any
   size of data is as many requests as it takes, and no kind needs a session-like upload.
3. **Content whose declared generating set is empty is not served** (0107 re-made at the caller's
   door under 0135); joins are applied before leaves within a page; across pages the ordering is
   the caller's, and a page that empties a set withdraws the content and says so in its ack.
4. **Exclusion memberships are admissible only under a published bound on the list**; above it,
   the inclusion spelling, which pages.
5. **The WAL pin is per record**: a growth above a level's high-water is released at the next tail
   pack; one below it waits for the fold.
6. **A level's row forms are published by the executor at the flush tick** from the deltas
   accumulated since; a request never builds one; a served form may lag the store by up to one
   tick, stale and never unsafe. The containment test reads only the operator and cardinality
   published together; a delta holding any leave re-derives that operator from entity truth.
7. **A fill on a flushed entity is read** by the record stack locating the one layer that claims
   the column.
8. **One visibility moment for every kind**: the next flush tick. The ack means durable; a subject
   exists for resolution at the ack.
9. **Fixed parts are write-once**; `view` is part of an artifact's identity on a group-scoped
   layer; creating an entity is not monotone and the resume rule is the batch id within retention.
10. **R1** page sizes on `/control/status`. **R2** values is its own route. **R3** the fill rule
    replaces the held-key refusal, with a stored digest for content values. **R4** lineage may be
    filled, the cycle walk over the layer as one graph. **R5** a supplied-content layer publishes
    without content and reports the count. **R6** an annotation on C7 stating the bound. **R7**
    memberships never shrink. **R8** the three per-object bounds stand and are published. **R9** a
    plain view has a create route, last in order. **R10** a runtime render column reads absent
    until the fold, answered from the segment schema.

## Consequences

- `ingest.md` is normative from this date. The order of work is its §8: T0 formats as one commit,
  then T1 caps and wire, T2a/b/c artifact record, generating-set pages, exclusion and view
  identity, T4 attributes before T3 values, T5 vocabularies, T6 groups and views, T7 conformance
  over served answers.
- `write-path.md` and `annotation-write-cycle.md` are rewritten under T7; until then their
  superseded sentences carry the markers decisions 0134 and 0135 placed.
- The register: C7 annotated with the growth-withdrawal bound; no new row.
