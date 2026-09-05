# 0127 — A membership grows through the control plane

**Date:** 2026-09-05 · **Status:** Settled (owner ruling) · Built 2026-09-05 as `PATCH /control/layers/{name}/artifacts` (contracts §3.4 r77); the ingest driver publishes then grows

## What this answers

`PUT /control/layers/{name}/artifacts` takes an artifact whole or not at all: the batch is the
commit unit, an artifact's membership has no smaller spelling than its whole member list, and the
body is capped at 64 MiB. At about fifteen bytes per base64 external id that is some 4.4 million
members, and the ingest campaign's rungs hold artifacts past it (measured 2026-09-05, rank-null
member rows):

| rung | layer | artifacts over the cap | largest |
|---|---|---|---|
| 3 MedCPT | `mesh/descriptors` | 45 of 30,217 | `eukaryota`, 27,171,642 members, ~389 MiB |
| 4 PaperSeek | `topics/openalex` | 11 of 4,798 | `3`, 40,846,248 members, ~584 MiB |
| 5 TreeOfLife | `clusters/kmeans` | 3 of 256 | `km-000106`, 5,803,417 members, ~83 MiB |

The 45 descriptors are the DAG's roots and hold 544.9 million of its 1.66 billion membership rows.
The ingest driver declines such an artifact per artifact and records it, so decision 0091's
equivalence test never covers a hierarchy's roots. Three options were put: a growth request on
the control plane; a denser member spelling, which reaches rung 5 and not the others; leaving the
artifacts declared and empty.

## The decision

**An existing artifact's membership may be grown by a control-plane request addressed by the
artifact's key.** The request carries the layer, the level, the addressing, and per artifact the
key and the members joining; it is resolved and applied through the engine's existing growth
path — `LayerRegistry::prepare_grow`, `IncomingGrowth`, the `ArtifactGrow` record
([artifacts-from-points.md](../design/artifacts-from-points.md) §6.1) — which is the path a
point's layer column at ingest already takes. A publisher of an artifact larger than the cap
publishes it once, with its key, its content, its parents and as many members as fit, then grows
it in requests of at most the cap.

What growth keeps from its existing definition: a key the level does not hold is refused, never
minted; a deleted member refuses the batch; a suppressed member joins and stays outside every
mask; nothing joining is a no-op; a growth adds members and never lineage or content; the record
is pinned in the log until the fold. The batch remains the commit unit and the cap stays at
64 MiB. No ordinal is claimed, so a refusal spends nothing.

The route's shape — a second body form on the publication route or a sibling route — is the
implementer's, within the contracts document's conventions, and is recorded there when built.

## Why

The mechanism exists and is durable, replayed and tested; the only thing missing is a request that
reaches it. A denser spelling would not reach a 389 MiB membership, and a cap that carries one is
no longer a bounded buffer. Leaving roots empty makes the one test that says build and ingest agree
silent about exactly the artifacts whose counts matter most.

A viewer may observe an artifact between its publication and its last growth with fewer members
than it will have. That is what any ingest in progress looks like and discloses nothing: every
count is computed inside the viewer's mask from whatever has landed.

## Open

The owner asked what else can be done for very large memberships in general. Two structural
answers are recorded here and not decided:

- **Closure computed rather than shipped.** Every over-cap artifact in the table is a hierarchy
  root whose membership is the closure of its descendants'. MeSH's member table is 3.1×10⁸ rows
  explicit and 1.66×10⁹ closed upward; the closure is 81% of the rows, the whole of the WAL and
  wire cost, and the whole of the over-cap problem. An engine that derived a parent's membership
  from its children's would take the explicit rows only. What it costs is where the union is
  taken — at query time, or at the fold as a precomputed closure — and that is a design question
  for [dag-hierarchies.md](../design/dag-hierarchies.md).
- **Predicate membership where the value is a column.** A cluster assignment is one value per
  row; a layer whose membership is `{ attribute = … }` is evaluated and publishes nothing, which
  is how rung 5's `publishers/source` already works.

## What this changes elsewhere

Contracts §3.4 gains the growth request when built; `client-interaction.md` says nothing new, an
artifact under growth being an artifact under ingest. The ingest driver
(`test_corpora/common/ingest_cycle.py`) publishes then grows an over-cap artifact instead of
declining it, and `publish.declined` should then be empty on every rung.
