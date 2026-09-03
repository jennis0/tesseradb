# DAG membership cannot be ingested — handover

**Status:** resolved 2026-09-03 by [decision 0125](../../decisions/0125-a-dag-list-column-is-membership-not-lineage.md) — a `dag` list column is plain multi-membership at both entry points. Problem statement kept as written, found by the measurement campaign's ingest cycle on rung 3 (MedCPT).

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

- the ingest driver **declines** the layer by default and says so beside every count; the folded
  deployment then serves **1,792 descriptors against the all-in build's 1,926** at 10⁶ and
  **10,328 against 10,685** at 3.6×10⁷ (`test_corpora/medcpt/measurements.json`);
- carrying the column anyway (`--carry-lineage-layers`, since narrowed to `--carry-nested-layers`) lands the memberships, drops the edges,
  emits a warning per row, and costs **31,445 items/s against 157,338** at 10⁶ — the edge checks
  scale with the corpus.

## Postscript, 2026-09-03: the publication route had no `parent` field

0125 spells a DAG's edges **only** on the artifact row's `parent` column, at both entry points —
the build's artifact rows, and the roster published on `/control/layers`. The second half of that
did not exist: `IncomingArtifactBody`, the JSON `PUT /control/layers/{name}/artifacts` takes,
carried `key`, `members`, `content`, `attached_to` and the shape fields and nothing else, so a
`dag` or `nested` layer published on the wire came out flat however many parents its roster
declared. `IncomingArtifact::parent_keys` and the registry beneath it already held as many parents
as a `dag` child names; only the field was missing, and it was added the same day.

Found by the campaign's ingest cycle, which after the same day's ruling builds the base from points
and declarations alone and publishes every artifact on the wire — so on `medcpt-1m` it publishes
29,229 MeSH descriptors carrying 40,075 parent edges, of which 8,881 artifacts name more than one.

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

`test_corpora/common/ingest_cycle.py` (`declined`, `--carry-lineage-layers`),
`test_corpora/medcpt/measurements.json`, `crates/tessera-engine/src/write.rs` (the warning),
`crates/tessera-types/src/layer.rs` (the lineage rule), `docs/design/configuration.md`
(the list-column paragraph), `docs/design/dag-hierarchies.md` §4 and §8.
