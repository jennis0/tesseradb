# The artifact serving campaign, end to end

**Date:** 2026-08-22 · **Status:** Probe record — evidence, not normative. The stage it discharges
is [`artifact-delivery.md`](../../docs/artifact-delivery.md)'s **Stage 7**; the plan is
[the campaign memo](../../docs/evidence/memos/2026-08-21-artifact-scale-plan.md) §8–9; the design it
validates is [`artifact-serving-at-scale.md`](../../docs/design/artifact-serving-at-scale.md); the
closing memo is [`2026-08-22-artifact-scale-campaign.md`](../../docs/evidence/memos/2026-08-22-artifact-scale-campaign.md).

**What separates this from [the scale probe](../2026-08-20-artifact-serving-scale/README.md) beside
it.** That campaign measured the design *probe-side* — a bench binary owning its own control flow
over synthetic structures, before any of it was in the engine. This one measures the **built**
engine through the **real request path**: `tessera build` over a materialised corpus,
`tessera serve` over HTTP, sessions established as a client establishes them, and the wire frames
decoded by `reference/oracle/wire.py`, the independently written decoder the conformance suite
already trusts. Nothing between the socket and the store is stubbed.

---

## Fixture

`tessera corpus materialise --terms-per-level 65536` at each tier, then `tessera build` over the
generator's own declaration plus one authored layer. The materialised inputs are deleted the moment
the bundle verifies — one tier on disk at a time, with a 10 GB floor checked before every step that
writes.

**Six layers, five of them the generator's own**, and each is a different route through the engine:

| layer | membership | what it exercises |
|---|---|---|
| `generator/flat` | enumerated | an **overlapping** membership — interval plus scatter, so one entity can sit in two artifacts |
| `generator/partition-enumerated` | enumerated | the partition relation **as a stored member list** |
| `generator/partition-attribute` | `{ attribute = "partition" }` | the same relation **as a predicate over a value column** — the twin, and the row-major route |
| `campaign/boundary` | spatial | a **spatial predicate** decomposed to Morton ranges at the generator's own authored depth |
| `generator/treed` | enumerated, `nested` | a **lineage** whose masked count includes every descendant's |
| `generator/boundary` | spatial, no shape | the generator's own spatial declaration, which carries a roster of prefixes and **no geometry** — it builds and serves nothing, and is left in place rather than removed |

`campaign/boundary` is the campaign's own: `tessera-corpus`'s spatial arm carries authored tile
prefixes and no boxes (`boundary.rs`'s header says why — it was written while nothing read one), so
[`artifact_campaign_fixture`](../../crates/tessera-bench/src/bin/artifact_campaign_fixture.rs)
authors a box per authored tile — **its middle half**, asserted at emission to cover exactly that
one prefix and no neighbour. That assertion is what makes the served membership and
`artifact-census --layer boundary` the same set rather than two nearby ones; it is
`artifact_spatial.rs`'s own fixture rule at campaign scale.

### The principal ladder, and the first thing the target scenario refused

At `--terms-per-level 65536` the corpus's term space is **1 048 576 terms**, which is the campaign's
~10⁶-term target, and it changes what a principal *is*. A single term covers about a
ten-thousandth of a percent of the corpus, so a principal at a stated breadth is a term **set**, and
its size is itself a finding. The ladder is whole term levels plus a prefix of the next level's
slots — constructed analytically, then **measured** by an O(*n*) pass, because the analytic figure
is a probability.

⊘ **A whole-corpus principal is not expressible through `/session/authorise`.** It would hold all
1 048 576 terms; the route buffers `auth_data` under axum's 2 MB default body limit, which is about
150 000 seven-digit descriptors. The broadest grant that fits is two whole term levels — **131 072
terms, 93.75% of the corpus** — and that is the campaign's broad rung. It is not a leak and not an
irreversibility: it is a capacity limit of the session plane that only appears once the term space
is large, and it is recorded here because the design's §7 grid has a 100% row this campaign cannot
reach.

⊘ **`tessera corpus artifact-census` cannot state a broad principal either**, for an unrelated
reason: it takes its grant as one argv string and Linux caps a single argument at `MAX_ARG_STRLEN`
(128 KB), so every grant above roughly 13 000 terms is `Argument list too long`.
[`artifact_campaign_census`](../../crates/tessera-bench/src/bin/artifact_campaign_census.rs) reads
the grant from a file and calls the same `tessera-corpus` methods the verb calls.

---

## Method

Every driver is in this directory and every figure has a CSV in `data/` beside it.

| step | script | what it measures |
|---|---|---|
| fixture | `fixture.py` | materialise, author, build, verify, delete inputs — wall and peak RSS at each |
| census exactness | `census.py` | served counts against the closed-form oracle, **exact** equality both ways, every artifact |
| the serving grid | `grid.py` | layer × principal × viewport, cold and p50/p99 warm, single client |
| the concurrency sweep | `concurrency.py` | 1/8/32/128 sessions at mixed breadths, sustained panning |
| serving during a fold | `fold_under_load.py` | latency before/during/after, the fold's own duration, freshness across it |
| ingest during serving | `ingest_during_serving.py` | read path quiet against under-ingest, the write path's own latency, freshness at the flush |
| the bracket re-run | `run-bracket.sh` | the 2026-08-20 campaign's `blocks = 8` anomaly, four points and doubled iterations |
| collation | `collate.py` | the CSVs, and the flag on any cell above 2× its probe-side figure |

**Timing conventions.** Latency is the client's own wall clock around `POST /v1/viewport`, request
issue to last byte — so it carries the frame encoding and the transfer, which the probe-side grid
did not. `k = 0`: the campaign measures the **artifact** channel, and a wide viewport that also
gathered a million points would report the gather's cost as the artifact route's. Percentiles are
nearest-rank over the iterations, no interpolation, so a p99 over nine samples is honestly the
largest of them.

**Cold and warm are reported separately and neither is dropped.** The first request at a
(layer, principal) pays for a row form and a lineage that every request after it reuses, and the
gap is large — 14 s against 300 ms at the 10⁷ tier's broadest principal on the flat layer. A grid
quoting only warm medians would describe a state no first viewport is ever in.

---

## Results

*(filled below, per tier, with the raw CSV named beside each table)*

---

## Appendix R — review trail

| revision | date | what changed |
|---|---|---|
| r1 | 2026-08-22 | Created — the Stage 7 validation campaign |
