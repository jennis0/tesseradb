# A tiered layer's levels reach the client only as a parent chain

**Date:** 2026-08-26 · **Track:** `toponymy` of the client-components campaign · **Status:**
investigation; measured against `data/notebook-2m4-live/` and the bundle served on viewer 37590 on
the day it was written. Nothing was changed — the fault is not in the data or in the notebook that
wrote it.

## Result

`clusters/toponymy` on `notebook-2m4` is declared tiered with four levels of 16, 46, 161 and 574
artifacts, and **the data holds exactly that**. The layer's parquet, the pipeline's manifest and
`/v1/meta` all agree, and the server serves all 797 artifacts of all four levels in one response
whatever `artifact_budget` says — which is what
[decision 0087](../../decisions/0087-cross-level-edges-are-information-not-rollup.md) requires of a
tiered layer, a budget having no ladder to climb.

**What no part of the response says is which level an artifact is at.** The artifacts frame
(contracts §3.2) carries `layer, tessera_id, key, masked_count`, the geometry columns, `content`
and `parent_id`, and there is no `level` column; the viewport request carries `layers` and
`artifact_budget`, and there is no level selector. So the client derives a level, and the only
material it has is `parent_id`: `clients/ts/core/src/artifactTable.ts` defines an entry's `level`
as *"how many known parent links sit above this entry — 0 for a root of what was served"*, and
`levelForBudget` then picks a level from counts taken over that number.

Chain depth is the declared level only where a tiered layer's edges are complete and every one of
them steps a single level. Toponymy's are neither, for reasons that are properties of the
clustering rather than defects, so the two diverge:

| | level 0 | level 1 | level 2 | level 3 |
|---|---|---|---|---|
| declared, and in the data | 16 | 46 | 161 | 574 |
| what the client's chain depth produces | **186** | 210 | 218 | 183 |

307 of 797 artifacts land at their declared level; 490 do not. At the overview
`levelForBudget([186, 210, 218, 183], 48)` returns 0, so the client draws those 186 — the
observation this investigation started from.

The same client code draws `taxonomy/arxiv` correctly: 38 archives and 171 subject classes, every
child parented, every edge one level. A two-level layer with complete single-step edges is the case
in which chain depth and declared level cannot disagree, which is why the defect was invisible
until a four-level layer was published.

## The three candidate causes, decided

**1. The level assignment in the writer is wrong — refuted.** The artifact table
(`clusters-toponymy.parquet`, 797 rows) carries 16 rows at level 0, 46 at 1, 161 at 2 and 574 at 3.
The member table (5,945,267 rows) carries members for 16, 46, 161 and 574 distinct keys at those
levels. The pipeline's `manifest.json` records the same four counts, and `/v1/meta` publishes the
four levels with their titles. Every edge in the file runs from a strictly coarser level to a finer
one, and no parent key resolves in two coarser levels.

**2. The containment edges are missing or partial — true of the data, and not a fault.** 170 of the
797 artifacts carry a null `parent` in the published table: 14 at level 1, 49 at level 2 and 107 at
level 3. They are not parents withheld by the disclosure rule — under the full principal
(all 176 categories) **zero** artifacts in the response name a `parent_id` the response does not
contain, so the earlier reading of this symptom as *"170 name a parent that is not served alongside
them"* is wrong: they name no parent at all.

No parent exists to name. For each of the 170, the intersection of its membership with **every**
artifact at **every** coarser level is empty — 170 of 170, not one point shared. The pipeline's
own figures say why: the layered clustering leaves 45.3% of the corpus unclustered at level 0 and
46.5% at level 1, against 33.3% and 29.5% at levels 2 and 3, so the coarse rungs cover *less* of
the corpus than the fine ones (1,324,715 points at level 0 against 1,707,342 at level 3). A cluster
whose members are all noise at every coarser rung has no container, and the largest such orphan
holds 35,247 papers. Giving it a parent would mean publishing a containment edge the data does not
support: the 611 edges that do exist are exact — **no child holds a member its parent does not**,
across every edge, which is the property the build reports on.

The edges also skip: of the 611, ninety span more than one level (17 span two levels from level 2;
35 span two and 38 span three from level 3). Decision 0087 admits that explicitly — *"a city sitting
directly under a country because that country has no states is a fact about the data rather than a
hole in a ladder"*.

**3. The client reads a tiered layer as though it were nested — this is the cause**, and the client
cannot currently do otherwise. Both halves of the divergence above come from chain depth standing
in for the declared level: the 170 orphans are roots of what was served and so count as level 0
beside the 16 real ones, and the 90 skipping edges put a level-2 or level-3 artifact at a depth
shallower than its level. The fix belongs in the client, and the client has nothing to read.

## What the server would have to publish for the client to be right

⊘ **Specified, not built.** `configuration.md`'s `[[layer.levels]]` table says of the advisory
`zoom` key that *"a tiered layer's response is bounded by the level asked for"*, and decision 0087
says a tiered layer's coarser view is *"another level, chosen by the client"*. Neither is
expressible on the wire today: the viewport request has no field naming a level, and the response
has no column saying which level a row came from. What happens instead is that every level is
served on every request — 797 artifacts and 181,286 bytes at every budget on this corpus — and the
client reconstructs a resolution from the edges.

Either of two additions would settle it, and choosing between them is the owner's:

- a **`level` column on the artifacts frame**, so the client keeps receiving the whole layer and
  cuts to a level exactly; or
- a **level selector on the request**, so a client that has chosen a level pays for one level's
  rows — which is what the two documents above describe, and which also bounds the response.

The first is the smaller change and repairs the drawing; only the second repairs the bytes.

## Method

`data/notebook-2m4-live/clusters-toponymy.parquet` and `-members.parquet` read directly with
pyarrow; the served side taken from `POST /v1/viewport` on 127.0.0.1:37590 under a token minted for
all 176 corpus categories, whole extent, `zoom: 0`, `k: 0`, `layers: ["clusters/toponymy"]`, at no
budget and at 20 and 48. Counts are exact, not sampled.
