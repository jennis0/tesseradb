# DAG membership cannot be ingested — handover

**Status:** problem statement, 2026-09-03. Found by the measurement campaign's ingest cycle on
rung 3 (MedCPT). No solution is proposed here; the wire form for multi-membership at ingest is an
owner design question.

## The problem

A `dag` layer's membership is a list column at both entry points — one row per point naming the
artifacts the point sits in. At a **build** that list is read as plain membership: `mesh.py`
emits a MeSH article's ancestor-closed descriptor set (decision 0117 ruling D) and the build
publishes each entry as one `(artifact, entity)` member, ~46 rows per article, 1.66×10⁹ in all.

At **ingest** the same column has a different meaning. `configuration.md` §layer declaration reads
a list under `nested` and `dag` as a **lineage** — entry *k* is the parent of entry *k+1* — so a
row's adjacent entries are edges, not siblings. A MeSH article names ~10.6 descriptors that are
unrelated to one another; sent through `/control/layers`, its list asserts ~9 parent edges the
tree does not hold. `LayerRegistry::check_edge` reports each as `Unrecorded` and the handler
warns: *"the memberships are applied and the edges are not"* (`tessera-engine/src/write.rs`).

So the entry points disagree on what the column means, and decision 0091 — one functionality at
both — does not hold for this layer:

- the ingest driver of the day **declined** the layer and said so beside every count; the folded
  deployment then served **1,792 descriptors against the all-in build's 1,926** at 10⁶ and
  **10,328 against 10,685** at 3.6×10⁷;
- carrying the column anyway landed the memberships, dropped the edges, emitted a warning per row,
  and cost **31,445 items/s against 157,338** at 10⁶ — the edge checks scale with the corpus.

Both figures are from the driver as it stood that morning. It no longer sends a membership column
at all — see the section below.

## 2026-09-03, later: the publication route cannot carry a parent either

The campaign's ingest cycle was rewritten the same day to publish artifacts on the wire after their
points, rather than to build them and grow their memberships from an ingest batch's column (owner
ruling; `test_corpora/common/ingest_cycle.py`). That removes the membership half of this problem
outright — `PUT /control/layers/{name}/artifacts` names an artifact's members as a set, so a MeSH
article's ~46 unrelated descriptors are 46 memberships and nothing is read as a lineage. It does
**not** remove the edge half, and it makes the shape of what is missing plainer.

`IncomingArtifact::parent_keys` exists in the service, holds as many parents as a `dag` child names,
and is the field the build fills from a roster's `parent` column. `IncomingArtifactBody` — the JSON
the route takes — carries `key`, `members`, `content`, `attached_to` and the shape fields. **There
is no parent field on the wire at all**, so the question is not whether a second parent can be
expressed but whether a first one can, and it cannot. A `dag` or `nested` layer published this way
comes out flat.

Measured on `medcpt-1m` (10⁶ articles, 29,229 descriptors with at least one member): the roster
declares **40,075 parent edges**, of which **0 were published**; 8,881 descriptors name more than
one parent. The layer's artifacts, memberships and content all land — the equivalence census agrees
with the all-in build on both — and the hierarchy does not exist on the ingested deployment.

Nothing is proposed here. The wire form for a published artifact's lineage is the same owner design
question as the one above, one step earlier: an artifact states its parents where it is published,
and the route that publishes it has nowhere to put them.

## What is and is not in question

- The DAG's *edges* are not: they are declared where an artifact is published and rung 3's roster
  carries them. What the wire cannot say is "this point is in these unrelated artifacts" for a
  layer whose kind makes a list mean lineage.
- Rung 5's `taxonomy/tree` is `tiered` — positions are levels and consecutive entries are
  containment edges the tree already holds — so it does not meet this. Rung 1's
  `places/containment` and any future `nested`/`dag` layer with multi-membership does.
- Whether the closure should travel on the wire at all (an ingested article's ~46 closed
  entries, or its ~10.6 explicit ones with the server closing) is the same question in another
  form and is part of what needs ruling.

## Where the evidence is

`test_corpora/common/ingest_cycle.py` (`publish`, `edges_not_expressible`),
`test_corpora/medcpt/measurements.json` and `measurements-medcpt-1m.json`,
`crates/tessera-server/src/control.rs` (`IncomingArtifactBody`),
`crates/tessera-lifecycle/src/membership.rs` (`IncomingArtifact::parent_keys`),
`crates/tessera-engine/src/write.rs` (the warning),
`crates/tessera-types/src/layer.rs` (the lineage rule), `docs/design/configuration.md`
(the list-column paragraph), `docs/design/dag-hierarchies.md` §4 and §8.
