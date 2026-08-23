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

Every driver is in this directory and every figure has a CSV in the campaign directory beside it.

| step | script | what it measures |
|---|---|---|
| fixture | `fixture.py` | materialise, author, build, verify, delete inputs — wall and peak RSS at each |
| census exactness | `census.py` | served counts against the closed-form oracle, **exact** equality both ways, every artifact |
| the serving grid | `grid.py` | layer × principal × viewport, cold and p50/p99 warm, single client |
| the concurrency sweep | `concurrency.py` | 1/8/32/128 sessions at mixed breadths, sustained panning |
| serving during a fold | `fold_under_load.py` | latency before/during/after, the fold's own duration, freshness across it |
| ingest during serving | `ingest_during_serving.py` | read path quiet against under-ingest, the write path's own latency, freshness at the flush |
| the layout question | `layout_after_fold.py` | which layout each level is served by, before its first fold and after |
| a whole tier | `run-tier.sh` | the read-only steps then the writing ones, in the order that keeps them comparable |
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

### The tiers, and the one that did not fit

| tier | points | artifacts (partition / flat) | materialise | build | peak build RSS | inputs | bundle | data |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| **10⁷** | 10 000 000 | 99 997 | 8.8 s | 89.6 s | 5.51 GB | 1.46 GB | 1.21 GB | `1e7-fixture.csv` |
| **2.5×10⁸** | 250 000 000 | 2 499 998 | — | — | — | — | — | `2.5e8-fixture.csv` |

⊘ **The 10⁹ tier does not fit on this box, and the arithmetic is not close.** Both figures scale
linearly in *n* and both are measured at 10⁷: the materialised inputs are 1.46 GB and the built
bundle 1.21 GB, so at 10⁹ they are **146 GB and 121 GB**. The build reads every input while writing
the bundle, so the transient requirement is their **sum, 267 GB**, against **147 GB free**. Deleting
the three member files (36% of the inputs) leaves 214 GB; `--no-oracle-pairs` takes another 22 GB
off the bundle and leaves 192 GB. Nothing available brings it under the floor, so the tier is
**recorded as a disk refusal rather than attempted** — starting a build that will die two hours in
with a full disk costs the tier twice and tells nobody anything.

That is a *different* limit from the one the probe campaign hit at the same corner: that one was
memory (45 GB resident with all 12 GB of swap gone, killed after three hours with no phase
progress). Two independent walls at the same cell, on the same box.

### The principal ladder as measured (`1e7-principals.csv`)

| rung | terms | visible at 10⁷ | measured fraction | analytic |
|---|---:|---:|---:|---:|
| broad | 131 072 | 9 375 624 | 0.93756 | 0.93750 |
| — | 65 536 | 7 501 541 | 0.75015 | 0.75000 |
| — | 38 390 | 4 999 670 | 0.49997 | 0.50000 |
| — | 17 560 | 2 500 555 | 0.25006 | 0.25000 |
| — | 6 312 | 940 803 | 0.09408 | 0.09399 |
| narrow | 2 048 | 311 288 | 0.03113 | 0.03101 |
| single term | 1 | 160 | 0.000016 | 0.000015 |

The construction and the corpus agree to four figures at every rung, which is what licenses quoting
the ladder by its target names in the tables below.

### 1. Census exactness at 10⁷ — **35 of 35 cells exact** (`1e7-census.csv`)

Served counts against `tessera corpus artifact-census`, at the whole map, **every artifact, both
directions, exact equality**. Five layer shapes × seven principals.

| principal | terms | visible | flat | partition-enumerated | partition-attribute | boundary | treed |
|---|---:|---:|---:|---:|---:|---:|---:|
| broad | 131 072 | 9 375 624 | 99 997 ✓ | 99 997 ✓ | 99 997 ✓ | 37 ✓ | 998 ✓ |
| — | 65 536 | 7 501 541 | 99 997 ✓ | 99 997 ✓ | 99 997 ✓ | 37 ✓ | 998 ✓ |
| — | 38 390 | 4 999 670 | 99 997 ✓ | 99 997 ✓ | 99 997 ✓ | 37 ✓ | 998 ✓ |
| — | 17 560 | 2 500 555 | 99 997 ✓ | 99 997 ✓ | 99 997 ✓ | 37 ✓ | 998 ✓ |
| — | 6 312 | 940 803 | 99 997 ✓ | 99 989 ✓ | 99 989 ✓ | 37 ✓ | 998 ✓ |
| narrow | 2 048 | 311 288 | 98 883 ✓ | 95 478 ✓ | 95 478 ✓ | 37 ✓ | 998 ✓ |
| single term | 1 | 160 | 325 ✓ | 159 ✓ | 159 ✓ | 18 ✓ | 128 ✓ |

**The counts move with the principal, which is what makes this a census of the masked surface.** A
grant seeing 3.1% of the corpus is served 95 478 of the partition layer's 99 997 artifacts — the
other 4 519 have no member it can see and are absent, not zero — and a single-term principal is
served 159. The two spellings of the partition relation agree artifact for artifact at every rung,
which is Stage 6's twin-equality check taken at scale over HTTP rather than in a unit test.

**The same is true at 10⁶** (`tessera corpus materialise --n 1000000`), run first as the pipeline's
smoke: 35 of 35 exact over the same five shapes.

### 2. The serving grid at 10⁷ (`1e7-grid.csv`)

p50 in milliseconds, nine iterations after one discarded cold request, single client, `k = 0`.

**`generator/partition-attribute` — the row-major predicate route:**

| principal sees \ viewport | 100% | 75% | 50% | 25% | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|---:|---:|---:|
| **93.8%** | 135.5 | 143.1 | 147.7 | 116.5 | 88.9 | 25.3 | 3.3 |
| **75.0%** | 124.4 | 121.7 | 122.2 | 105.7 | 87.3 | 23.6 | 2.8 |
| **50.0%** | 111.9 | 118.7 | 113.1 | 102.5 | 92.0 | 18.4 | 2.3 |
| **25.0%** | 97.9 | 98.2 | 96.7 | 91.7 | 83.7 | 10.2 | 1.8 |
| **9.4%** | 94.9 | 89.8 | 90.4 | 84.5 | 64.2 | 5.0 | 1.3 |
| **3.1%** | 84.9 | 85.7 | 84.9 | 72.0 | 31.5 | 2.0 | 1.0 |
| **0.0016%** | 1.2 | 1.0 | 1.1 | 1.1 | 1.1 | 1.0 | 1.1 |

**`generator/partition-enumerated` — the same relation as a stored member list:**

| principal sees \ viewport | 100% | 75% | 50% | 25% | 6.25% | 0.39% | 0.024% |
|---|---:|---:|---:|---:|---:|---:|---:|
| **93.8%** | 243.3 | 228.6 | 222.8 | 714.1 | 376.9 | 121.3 | 58.2 |
| **75.0%** | 224.2 | 222.2 | 224.7 | 736.1 | 381.9 | 112.4 | 58.7 |
| **50.0%** | 233.8 | 236.0 | 231.3 | 749.5 | 396.5 | 103.2 | 59.1 |
| **25.0%** | 235.0 | 240.8 | 236.2 | 717.2 | 383.7 | 85.6 | 56.2 |
| **9.4%** | 250.1 | 247.1 | 247.6 | 715.4 | 334.2 | 74.8 | 54.9 |
| **3.1%** | **1 054.1** | 1 064.2 | 1 059.4 | **1 463.9** | 546.3 | 74.3 | 55.6 |
| **0.0016%** | 148.4 | 149.5 | 148.2 | 335.8 | 147.3 | 63.5 | 53.6 |

**`generator/flat` — an overlapping enumerated membership** (interval plus scatter): the same shape
again and dearer, worst cell **2 430 ms** at the narrow principal and the 25% viewport, 281 ms at
the broad principal and the whole map.

**`generator/treed` — a `nested` lineage over 998 nodes:** 15.7 ms at the broad principal and the
whole map, 134 ms at its worst cell (narrow principal, 25% viewport), 1.4 ms at its best.

**`campaign/boundary` — the spatial predicate:** **1.0–1.5 ms at every one of its 49 cells.** 37
artifacts, each a `count_range` over the covering Morton ranges; neither the mask nor the viewport
moves it measurably.

Four things this grid says, and two of them disagree with the probe-side one.

**The row-major predicate route is the best-behaved of the five, and it is the only one monotone in
both axes.** Cost falls as the principal narrows (135 → 85 ms) and as the viewport closes
(135 → 3.3 ms), which is the shape the design argues for. The two enumerated routes are neither.

**The cost inversion is *not* fixed on the artifact-major routes.** At 3.1% visible the enumerated
twin pays 1 054 ms where the broad principal pays 243, and the flat layer pays 1 646 against 281 —
a narrow `M_auth` is a more fragmented row-space set and the artifact-major walk pays per container.
`artifact-serving-at-scale.md` §7.1 records this inversion as *fixed* for the hoisted route; what is
measured here is that it is fixed **for the row-major layout and not for the artifact-major one**,
and the campaign's layers are served by whichever the heuristic picked (`ArtifactMajor` for flat,
partition-enumerated and treed; `RowMajorLabel` for partition-attribute — the server's own log
records the decision at every level build).

**The grid is not monotone in viewport area on any artifact-major layer: the ridge is at 25%, not at
the whole map.** Every enumerated cell at zoom 2 is 3× its zoom-0 neighbour — 714 against 243 on the
enumerated twin, 985 against 281 on flat, 41 against 16 on treed. A whole-map request resolves to
one tile and the extent test is trivially satisfied; a 25% request at zoom 2 resolves to sixteen and
every artifact's extent is intersected against them. §7.1's "the worst cell is a ridge just inside
the grid … at a mask below one and the widest viewport" holds for the mask axis and **not** for the
viewport axis on this path.

**The wire is a sixth of a wide request and no more.** The whole-map broad cell on the predicate
route is 135.5 ms total, of which the trailer reports **21.6 ms** of Arrow serialisation for an
11.2 MB body carrying 99 997 artifacts. `x-tessera-server-us` — post-admission to first-flush-ready
— is 5.6 ms, so the artifact channel's own work sits between the tiles frame and the trailer and is
the remaining ~108 ms.

**Cold is one to two orders above warm and belongs in the record** (whole map, broad principal):

| layer | cold | warm p50 | ratio |
|---|---:|---:|---:|
| `generator/flat` | 13 879 ms | 281.5 ms | 49× |
| `generator/partition-enumerated` | 9 617 ms | 243.3 ms | 40× |
| `generator/partition-attribute` | 1 877 ms | 135.5 ms | 14× |
| `generator/treed` | 571.8 ms | 15.7 ms | 36× |
| `campaign/boundary` | 1.9 ms | 1.1 ms | 1.7× |

The first request at a (layer, level) builds the row form and the tile index; every one after it
reuses them. A deployment's first viewport after a restart or a fold pays this, and on the two
enumerated layers it is ten to fourteen seconds.

**Against the probe's grid** (`ratio_to_probe` and `over_2x_probe` in the CSV, compared against
§7.1's 10⁹/10⁶ table as the nearest published configuration): 107 of 245 cells run above 2× their
probe-side neighbour — **37 of 49 on flat, 32 on the enumerated twin, 21 on the predicate route, 17
on treed and 0 on the spatial one**. The comparison is loose and the README says so: the probe's
grid is `hoisted` over a synthetic membership at ten times the artifact count and a hundred times
the corpus, measured inside a bench binary with no HTTP, no session, no frame encoding and no
gather. What the flags are good for is the *shape* — the layers that clear it are the two that do
not walk artifacts.

### 3. The concurrency envelope at 10⁷ (`1e7-concurrency.csv`)

45 seconds of sustained panning per level on `generator/partition-attribute`, mixed breadths, one
`M_auth` per session, driven by the Rust load arm.

| sessions | mix (broad/median/narrow) | throughput | p50 | p99 | server cores | peak RSS | shed |
|---:|---|---:|---:|---:|---:|---:|---:|
| **1** | 1 / 0 / 0 | 6.7 rps | 145 ms | 185 ms | 1.00 | 2.06 GB | 0 |
| **8** | 1 / 1 / 6 | **28.9 rps** | 354 ms | 553 ms | 4.84 | 2.75 GB | 0 |
| **32** | 2 / 6 / 24 | **29.1 rps** | 1 535 ms | 2 072 ms | **9.39** | 5.19 GB | 0 |
| **128** | 8 / 25 / 95 | 16.5 rps | 9 821 ms | 13 691 ms | 7.36 | **11.76 GB** | 7 |

**The knee is at 32 sessions and the binding resource is the machine.** Throughput rises 4.3× from
one session to eight and then stops; at 32 the server is using 9.4 of 12 cores and every further
session is queueing. Beyond that it **regresses** — 16.5 rps at 128, with cores *falling* to 7.4 —
and the compute gate sheds 7 requests, which is its 48-permit admission and 96-deep queue working
rather than failing. Latency past the knee is pure queueing: p50 rises 6.4× from 32 to 128 sessions
while the work done falls.

**Memory is ~76 MB per session and it is the resource that will bind first at a larger tier.**
2.06 GB at one session, 11.76 GB at 128, on a bundle of 1.21 GB — the difference is 128 principals'
`M_auth` over 10⁷ entities plus their cached row projections (169 entries, 133 MB, no evictions
against a 2 GB bound; the fragment cache never rose above 3 entries).

**Session establishment is not the cost the term space made it look like.** A 131 072-term grant
authorises in **0.06 s**; 128 sessions establish in 0.5 s in total.

⊘ **The first sweep of this campaign was wrong and the wrong numbers are in the git history**
(commit `6631ec8`). A Python driver with a thread per session reported 4.5 rps at 128 sessions with
the server using 1.6 of 12 cores; both halves were the harness, which was decoding every 11 MB
artifacts frame into a hundred thousand tuples under the GIL. It was caught by sampling the
**driver's** CPU beside the server's — 1.4 cores at eight sessions, which is Python's ceiling — and
the load arm was rewritten in Rust. Every load figure above comes from the Rust arm.

### 4. Serving during a fold at 10⁷ (`1e7-fold-under-load.csv`, `1e7-fold-unloaded.csv`)

32 mixed sessions panning; 500 000 rows ingested carrying both a `partition` value and a membership
column; `POST /control/flush`; `POST /control/compact`. The unloaded run is the identical sequence
with no sessions at all.

| | loaded (32 sessions) | unloaded | ratio |
|---|---:|---:|---:|
| fold wall time, `POST` → counter | 102.6 s | 101.0 s | 1.02× |
| the fold's own `elapsed_ms` | **82.7 s** | **84.1 s** | **0.98×** |
| the artifact pass inside it | 22.8 s | 23.0 s | 0.99× |
| `staircase_rss` | 7.56 GB | 3.86 GB | 1.96× |
| requests shed | 0 | — | — |

**The fold under load costs what the fold costs.** 0.98× of its unloaded duration, and the artifact
pass inside it 0.99× — against an acceptance bar of 2×. The memory is the part that moves: the
staircase peak is 1.96× higher with 32 sessions live, and that difference is the sessions' own
resident state rather than the fold's.

**The median request does not notice the fold; the tail notices it enormously** — latency by window,
from the load arm's own per-request record:

| window | requests | p50 | p99 | max |
|---|---:|---:|---:|---:|
| before | 1 062 | 1 302 ms | 1 683 ms | 1 735 ms |
| ingest and flush | 678 | 1 060 ms | 10 247 ms | 10 355 ms |
| **during the fold** | 762 | **1 290 ms** | **58 924 ms** | 59 130 ms |
| after | 1 134 | 1 298 ms | 1 714 ms | 1 759 ms |

p50 is flat to within 1% across all four windows. p99 goes from 1.68 s to **58.9 s** during the
fold — a 35× tail excursion, with the worst single request at 59.1 s — and returns to 1.71 s
immediately afterwards. Nothing was shed and nothing errored: the requests waited. **This is the
named gap's answer and it is two answers**: the fold's own cost is unaffected by load, and a live
session's tail is not.

**Freshness across the sequence**, watching one artifact's masked count for the broad principal:

| | before | after the flush | after the fold |
|---|---:|---:|---:|
| `generator/partition-attribute` (predicate) | 92 | **500 092** | 500 092 |
| `generator/partition-enumerated` (stored) | 92 | 92 | **500 092** |

Both are correct and the difference is the documented one. A predicate layer reads a value column,
so a point ingested with value *v* counts at its flush — **12.2 s after the `POST /control/flush`
with 32 sessions live, 5.6 s unloaded**. An enumerated membership projects through *base* rows, so a
growth counts at the fold that makes them base rows and **understates until then** — fail-closed,
`annotation-write-cycle.md` §4.1's posture, and the count is exactly right on the far side.

### 5. Ingest during serving at 10⁷ (`1e7-ingest-during-serving.csv`)

32 sessions panning for two minutes, sustained ingest through the second half.

| | quiet | under ingest |
|---|---:|---:|
| requests | 2 138 | 1 424 |
| p50 | 1 284 ms | 961 ms |
| p99 | 1 682 ms | 1 696 ms |

**The read path does not degrade.** p99 moves by 0.8% and p50 falls, which is the viewport mix
rather than the ingest; nothing was shed and nothing errored while the server ran at 10.6 of 12
cores serving 34.4 requests a second.

**The write path stays inside its posture and says so when it is full.** 1 000 000 rows accepted at
**47 421 rows/s**, batch p50 **21.4 ms**, p99 202 ms, max 790 ms — three orders inside the
seconds-level write-latency budget. The offered load was 2 000 000 rows, and **500 of the 1 000
batches were refused with `429 backpressure` and a `retry_after_s` of 90**. That is the write
queue's admission control working: the driver does not retry, so the accepted rate above is the rate
the path sustained under a full serving load, and the refusals say how far the offer ran ahead of
it.

Freshness: the same 92 → 1 000 092 on the predicate layer, **7.3 s after the flush request**.

### 6. The build's layout, and what the first fold does to it (`layout-after-fold-1e7.json`, `1e7-postfold-grid.csv`)

**The grid in §2 is measured on a bundle whose enumerated layers are served by the wrong layout, and
that is not a mistake in the measurement — it is the state a freshly built bundle is in.**

The 10⁷ grid's two enumerated layers cost three to twelve times their predicate twin, and the
server's own log says why: it is serving them `ArtifactMajor`. `layout::choose`'s rule says they
should not be. `layout_after_fold.py` asks directly — read the layout off the log, fold, read it
again:

| layer | after the build | at the first fold | blocks/artifact | the rule's answer |
|---|---|---|---:|---|
| `generator/flat` | `ArtifactMajor` | **`RowMajorList`** | 108.0 | row-major (overlapping ⇒ list) |
| `generator/partition-enumerated` | `ArtifactMajor` | **`RowMajorLabel`** | 73.3 | row-major (disjoint ⇒ label) |
| `generator/treed` | `ArtifactMajor` | `ArtifactMajor` | 153.0 | **artifact-major — 998 artifacts, below `ROW_MAJOR_MIN_ARTIFACTS` (1 000)** |
| `generator/partition-attribute` | `RowMajorLabel` | `RowMajorLabel` | — | a predicate's form is never re-derived |

The treed row is the rule working: 153 blocks per artifact is well over the locality threshold, and
the level has **998** artifacts against a floor of 1 000, so it stays artifact-major. The other two
are the finding: the build reports 108.0 and 73.3 blocks per artifact — decision 0092's figure — and
records a layout that its own reported figure contradicts. `RowMajorLabel`/`RowMajorList` arrive at
the first fold, which is
[0094](../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)'s
re-evaluation doing what it says. What 0094 also says is that the layout is *chosen at the build*,
and on this evidence the build does not choose it. **It is a report, not a fix** — the campaign
writes no engine code.

**What the fold is worth, measured.** The same fixture, rebuilt, folded once with nothing ingested,
and the grid re-run — p50 in milliseconds:

| layer | pre-fold worst cell | post-fold worst cell | pre-fold broad/whole-map | post-fold broad/whole-map |
|---|---:|---:|---:|---:|
| `generator/flat` | **2 430** | **221** | 281 | 221 |
| `generator/partition-enumerated` | **1 464** | **139** | 243 | 133 |
| `generator/treed` | 134 | 114 | 15.7 | 13.5 |

**11× and 10.5×**, and the shape changes as well as the size: post-fold both layers are **monotone
in both axes**, the 25% ridge is gone, and the cost inversion is gone with it — the narrow principal
is now the cheap one on every layer. That is the design's claim, reproduced end to end on the built
engine. Cold stays expensive: 11 957 ms and 8 424 ms for the first whole-map request.

The post-fold enumerated twin at 132.9 ms and the predicate spelling at 135.5 ms are **the same
number**, which is what one relation served two ways should cost. The list form's larger constant is
visible beside it: `generator/flat`, whose memberships overlap, pays 221 ms for the same work.

### 7. The design ceiling — 9 832 352 artifacts (`ceiling-grid.csv`)

The generator scales every closed-form arm at one artifact per hundred points, so 10⁷ artifacts
would want the 10⁹ tier that does not fit. `campaign/ceiling` reaches the artifact axis without the
corpus axis: an attribute predicate over `weight`, a keyed `u32`, which at 10⁷ rows has very nearly
10⁷ distinct values. The build minted **9 832 352 artifacts** in one level, served `RowMajorLabel`,
in a 1.57 GB bundle built in 118 s at 11.0 GB peak RSS.

**It has no census** — nothing in `tessera-corpus` states this relation — so it reports latency and
residency and claims nothing about correctness. p50 in milliseconds, three iterations:

| principal sees \ viewport | 100% | 25% | 6.25% | 0.024% |
|---|---:|---:|---:|---:|
| **93.8%** | **12 489** | 7 265 | 2 004 | 6.9 |
| **25.0%** | 3 480 | 2 015 | 430 | 4.4 |
| **3.1%** | 395 | 207 | 56.4 | 3.6 |
| **0.0016%** | 3.9 | 3.5 | 3.4 | 3.2 |

Artifacts served at the top-left cell: **9 219 239**, in a **954 MB** body, of which **2 843 ms** is
Arrow serialisation. Server RSS 5.52 GB at boot, 9.32 GB after the sweep.

**The one-second budget does not hold at ten million artifacts and a wide viewport**, and the reason
is not the counting. The route is flat in the mask below about 3% — 395 ms for a principal seeing
three percent of a ten-million-artifact layer is exactly the row-major route working — and the
dear cells are dear because the response *is* nine million artifacts. `artifact-serving-at-scale.md`
§7.1 models the unmeasured 10⁹/10⁷ cell at ~550–900 ms; that model is about the **count**, and this
measurement says the count is not what a request at that size pays for.

⊘ **There is nothing that bounds such a response.** `artifact_budget` is accepted and inert on a
flat layer (the wire contract says so — a budget is met by serving ancestors, and a flat layer has
none), measured here: at `artifact_budget = 100` the same request returns the same **254.6 MB**.
Whether a wide request over a ten-million-artifact flat layer should be answerable at all is an
owner question the campaign does not settle.

### 8. ⊘ A defect, with its reproduction — a cold request is truncated and nothing says so

**What happens.** The first `/v1/viewport` after boot naming a level whose row form is not yet built
is aborted mid-body when that build outruns the whole-stream deadline. The client receives a `200`
with a truncated chunked body and no trailer; **the server logs nothing** — neither
`viewport stream aborted mid-body` nor `viewport stream aborted before its trailer` fires.

**Reproduction**, from a clean boot on the ceiling bundle (10⁷ points, one attribute-predicate layer
over `weight`, 9 832 352 artifacts):

```
POST /v1/viewport  {"view":"s0","zoom":0,"bbox":[0,0,65536,65536],"k":0,
                    "layers":["campaign/ceiling"]}
```

| pass | shipped `stream_deadline_ms = 60000` | `stream_deadline_ms = 600000` |
|---|---|---|
| 1 (cold) | **fails at 111 356 ms**, `Response ended prematurely` | **succeeds at 110 719 ms** |
| 2 (warm) | 4.2 ms | 4.1 ms |
| 3 (warm) | 3.7 ms | — |

Raising the deadline is what identifies the cause: the level's row form and tile index take ~111 s
to build over 9.83M artifacts, that build happens **after** the response's first flush, and the
60-second whole-stream deadline therefore fires on the first send after it. It reproduces on every
boot and, by §6, after every fold that rebuilds a row form.

It is fail-closed at the client — `reference/oracle/wire.py` raises on a truncated stream rather
than decoding a plausible shorter response, which is what turned this up — and it is **silent on the
server**, which is the part worth fixing. Recorded, not fixed: this is a measurement track.

---

## Appendix R — review trail

| revision | date | what changed |
|---|---|---|
| r1 | 2026-08-22 | Created — the Stage 7 validation campaign |
