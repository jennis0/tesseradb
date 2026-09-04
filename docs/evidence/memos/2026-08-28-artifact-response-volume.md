# A 464,655-artifact tiered layer — what the GeoNames rung found

**Status:** Evidence, never normative. Measurements taken 2026-08-28 against the GeoNames bundle
built by [`../../../test_corpora/geonames/`](../../../test_corpora/geonames/README.md); the code
claims are read off the tree at `b1cb81b`. **This decides nothing.** It exists to be the background
for an owner design pass, so §5 poses questions rather than answering them.

**Reads with:** [`../../design/annotation-representation.md`](../../design/annotation-representation.md)
§6 (the zoom→level map, and the cost model this contradicts),
[`../../design/artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md) (the
evaluation cost, which is not the problem),
decision 0083,
decision 0087 and
decision 0092.

---

## 1. The result

The first real corpus with a published administrative hierarchy makes the demo unusable at its
opening view, and the cost is **entirely the artifact response**. Two axes matter and both are
measured below: how much of the corpus the principal may see, and how wide the viewport is.

**By principal**, at zoom 0 over the whole extent:

| Principal | Visible | Points only | With `layers: "all"` |
|---|---|---|---|
| narrow (TK) | 7,115 | 4 ms · 4 KiB | 34 ms · 0.03 MiB |
| sparse | 242,509 | 4 ms · 4 KiB | 69 ms · 1.1 MiB |
| medium | 1,626,089 | 11 ms · 4 KiB | 165 ms · 2.5 MiB |
| heavy | 6,996,069 | 50 ms · 4 KiB | 751 ms · 17.4 MiB |
| **full** | **13,463,857** | **65 ms · 4 KiB** | **1,887 ms · 49.0 MiB** |

**By viewport**, full principal, a square window of side 2⁻ᶻ over western Europe:

| Zoom | Points only | With `layers: "all"` |
|---|---|---|
| 0 | 67 ms · 4 KiB | **2,894 ms · 48.99 MiB** |
| 1 | 99 ms · 6 KiB | 2,954 ms · 48.99 MiB |
| 2 | 48 ms · 9 KiB | 1,079 ms · 22.85 MiB |
| 3 | 10 ms · 16 KiB | 669 ms · 16.19 MiB |
| 4 | 5 ms · 26 KiB | 423 ms · 10.87 MiB |
| 5 | 15 ms · 33 KiB | 125 ms · 3.26 MiB |
| 6 | 2 ms · 32 KiB | 71 ms · 2.27 MiB |
| 8 | 2 ms · 43 KiB | 55 ms · 0.29 MiB |
| 10 | 1 ms · 26 KiB | 46 ms · 0.06 MiB |

**Three things follow, and the third is the finding.**

**The point path is untouched by either axis** — 1 to 99 ms and under 45 KiB throughout. Every
millisecond above that is artifacts.

**The tile index prunes, and prunes well** — 49 MB at zoom 0 to 0.06 MB at zoom 10, a factor of 800
across the zoom range. The serving structure is doing its job.

**The failure is the opening view, where there is nothing to prune** — and there, every level is
returned regardless of zoom. The shipped viewer opens at whole-extent zoom 0, which is the worst
cell of both tables.

⊘ **Corrected 2026-08-28: the viewer does not send `layers: "all"`, and not on every request.** An
earlier revision of this memo said so here and in §4.4. `clients/ts/viewer/src/main.ts` names **one**
layer — `meta.layers[0]`, or whichever the picker selects — and the point path sends `layers: []`;
the artifact request is a separate `k = 0` call issued once per *settled* view on a 200 ms debounce
(`clients/ts/core/src/artifactChannel.ts`). The volume result is unchanged, because on this corpus
one layer **is** the 464,655 artifacts; what is wrong is the framing *per pan*, which should read
*per settled view, for one layer*.

**This is not a leak.** The response scales with the visible set and with the viewport and with
nothing else, which is §4's **I2** behaving as specified. It is a volume problem.

**The design's per-artifact sizing is confirmed rather than contradicted.**
`annotation-representation.md` §6 sizes what reaches a client at *"of order a hundred bytes an
artifact"*; 464,655 artifacts × 100 B ≈ 46 MB against 49.0 MB measured. The bytes per artifact are
right. What is wrong is the granularity, and how often they are sent.

## 2. The corpus that produced it

`admin/hierarchy` is a `tiered` layer of five levels over 13,463,857 points, minted from a member
file — GeoNames' own country → admin1 → admin2 → admin3 → admin4, country-qualified.

| Level | Artifacts | Mean members | `everywhere` | Blocks/artifact | Declared `zoom` |
|---|---|---|---|---|---|
| 0 Country | 254 | 52,979 | 0.453 | 4.6 | 0–4 |
| 1 Admin 1 | 4,842 | 2,784 | 0.147 | 1.8 | 3–7 |
| 2 Admin 2 | 51,951 | 189 | 0.024 | 1.1 | 6–10 |
| 3 Admin 3 | 175,963 | 22.8 | 0.007 | 1.0 | 9–13 |
| 4 Admin 4 | 231,645 | 3.6 | 0.001 | 1.0 | 12–16 |

**464,655 artifacts in one layer**, of which levels 3 and 4 are 407,608 — 88%. Beside it,
`features/taxonomy` is 688 artifacts across two levels and costs 249 ms / 75 KiB, which is
unremarkable.

**The overshoot at the opening view is the corpus's own declaration.** At zoom 0 the declared
zoom→level map says level 0 applies and nothing else: **254 artifacts are relevant and 464,655 are
served**, a factor of 1,830. The map that would have prevented this is already written, already
compiled, and already published on `/v1/meta`.

**Every level is served `ArtifactMajor`** — one row-space bitmap per artifact, spelled `rows` in the
configuration surface — and the build wrote `0 row-major column(s)`. That is the correct choice and
not a missed one: the trigger is the level's `everywhere` fraction
(decision 0094),
these levels are spatially localised (0.453 falling to 0.001), and the row-major column exists for
levels too wide for any node of the tile index. None of these are. **The zoom sweep above is that
choice being vindicated** — a level with `everywhere` of 0.001 is one the tile index can prune
almost perfectly, and it does.

**So neither the storage nor the evaluation is wrong.** This is not
[`artifact-serving-at-scale.md`](../../design/artifact-serving-at-scale.md)'s problem: that
document's cost is evaluation, and evaluation here is fine.

## 3. What is working correctly, so a design pass does not re-solve it

- **The mask bounds the response.** 7,115 visible → 0.03 MiB; 13.5M visible → 49 MB. Proportional.
- **The tile index bounds it too**, by a factor of 800 across the zoom range.
- **The layout choice is right**, and the build reported the numbers that say so.
- **`/v1/categories` is cheap even on a 231,645-value `derived` vocabulary** — 3 ms, because it
  pages. The combination flagged in advance as expensive is not the problem.
- **Evaluation cost is not the bottleneck.** The whole cost is response volume, at low zoom.

## 4. The gaps, and only the last two are unambiguously defects

### 4.1 The budget is inert on a tiered layer, and that is a ruling

Decision 0087 is explicit:
a tiered layer's edges are *information — what contains what*, not *roll-up — the ladder a cut
climbs*. `artifact_budget` is inert, `prune_children` has nothing to prune, and **a coarser view is
another level, chosen by the client**. `crates/tessera-engine/tests/artifact_hierarchy.rs` asserts
it by name (`a_budget_is_inert_on_a_tiered_layer`) with the reasoning attached: climbing the edges
would substitute a state for its counties and draw one polygon across a region whose neighbours are
still counties.

**Measured, and the ruling holds exactly:** `artifact_budget` of 10, 100, 1,000 and 10,000 each
return a byte-identical 50,099 KiB.

Nothing here is broken. It is recorded because it removes the obvious fix.

### 4.2 A client cannot choose a level, so the ruling's remedy is unreachable

The remedy 0087 names is the client choosing a level. **There is no level selector on the wire.**

- `LayerSelection` (`crates/tessera-engine/src/viewport.rs`) is `All` or `Named(&[&str])` — layer
  *names*, nothing finer.
- `ViewportRequest` carries `view`, `zoom`, `bbox`, `k`, `underlay_offset`, `filters`, `layers` and
  `artifact_budget`. No level, at either the engine or the HTTP boundary.
- `/v1/meta` publishes the zoom→level map, which
  [`annotation-representation.md`](../../design/annotation-representation.md) §6 calls **advisory,
  the client's choice**, and whose purpose for a tiered layer is exactly this.

So a client can decide what to **draw** and has no way to say what to **fetch**. The server serves
all five levels, and a client following the zoom→level map at zoom 0 discards 464,401 of the 464,655
artifacts it was sent — **99.95% of the payload, at the one viewport where the tile index cannot
prune instead**.

**This is the whole of the observed problem.** The two bounds that do work — the mask and the tile
index — are both bounds on *which artifacts are in range*. Neither is a bound on *which levels the
client wanted*, and at zoom 0 over a whole-world extent nothing is out of range, so the level is the
only axis left and it is the one axis a request cannot name.

⊘ **Unverified:** whether this was ever intended to be expressible and was dropped, or was never
designed. The corpus contains no ⊘ marking a level selector as specified-but-unbuilt, which suggests
the second.

### 4.3 The artifact ceiling the design leans on does not exist

[`annotation-representation.md`](../../design/annotation-representation.md) argues that optimising
the fine-level regime buys nothing, because such a request *"will be refused on its artifact
ceiling regardless of which structure would have answered it faster"*, and calls that regime's
output **unservable**.

**There is no ceiling.** No key in `tessera-server`'s config, no enforcement in the engine, and the
only `max_artifacts` in the tree is a flag on `crates/tessera-bench/src/bin/membership_residency.rs`.
Decision 0092
declined a *declared* bound on a layer, which is a different object from a *per-request* ceiling —
but its own §2 records that `artifact-delivery.md` had carried an owed item, *"the per-request bound
must refuse on the layer's declared artifact count before any evaluation"*.

⊘ **Corrected 2026-08-28.** An earlier revision said that item did *"not appear to have been
re-homed"*. It was **withdrawn**, explicitly and in writing:
artifact-delivery.md §2 reads *"The second item on this list is
withdrawn… No bound machinery is owed by any stage"*, and §8's row gives the reason — *"what costs
is row-space locality rather than the count, so a threshold on the count refuses the cheap layer and
admits the dear one"*.

**That reason is about evaluation and does not carry to volume.** A response carries one row per
served artifact, so the served *count* is what predicts its size where it is the wrong predictor of
time. The 110 B a row costs here is **this layer's**, not a constant — it follows what the layer
declares, and a level declaring a hull has no bound at all, the rings being a function of the
membership. That is why decision 0103
has the build report the artifact count and no byte estimate. A response-volume ceiling would therefore have been a different object from both
0092's declared bound and the withdrawn evaluation bound — and the owner has now declined it too
(decision 0103):
a large response is slow rather than wrong, so it is reported at the build and served. §5's question
3 is answered *nowhere, and deliberately*.

So the regime the design assumed would be refused is instead served, at 49 MB — and is now bounded
by the level instead.

### 4.4 The cost model and the shipped client disagree about how often artifacts are fetched

`annotation-representation.md` §6 is explicit that the sizing rests on a client behaviour:

> **The client caches, so a level is replica sync rather than request cost** … Sizing a level as
> though every viewport re-fetched it is the wrong model.

**The shipped viewer re-fetches the artifacts for every settled view**, one layer at a time, and
nothing reconciles a held artifact against a version coordinate.

⊘ **Corrected 2026-08-28** — see §1. The claim that `main.ts` passes `'all'` was wrong; it names one
layer, and the re-fetch is per settled view rather than per request. The rest holds: there is no
artifact replica.

**Checked since**, which this memo had not done.
[`client-obligations.md`](../../design/client-obligations.md) rule 6 states the artifact channel
asks for itself, and rule 7 that *"a held whole-layer artifact set goes when the content key it was
fetched under rotates"* — so a held set **is** a written client obligation. And the machinery is
half-built: `SessionArtifactTable` already keeps artifact payloads across responses, refcounted and
surviving a pan, which is the store a replica needs. What is missing is that the channel replaces
its served set wholesale and re-fetches every payload with it.

**Two facts make that fixable rather than merely desirable**, and both were verified in the tree
rather than assumed. An artifact's payload — key, count, centroid, box, hull, content, parent — is a
function of `(artifact, M_auth, generation)` and of nothing in the request:
`crates/tessera-engine/src/derived.rs` computes it from `mask.visible_rows(members)`, the artifact's
**whole** membership intersected with the mask, never clipped to the viewport. And the **content key
hashes exactly those three things** and nothing from the request
(`viewport.rs::view_coordinates`), so it rotates precisely when a held payload goes stale and never
because a client panned or crossed a zoom band. A client-side artifact dictionary therefore needs no
new coordinate — and the level must never be put into the content key, or every zoom band crossing
would discard a cache that was still valid.

## 5. What a design pass has to answer

Posed, not answered. Each is genuinely open.

1. **Is the level a request parameter?** 0087 says the client chooses a level; nothing lets it say
   so. If the answer is yes, it is a contracts change to `/v1/viewport` and to `LayerSelection`.
   **The disclosure question looks free and should be confirmed rather than assumed:** 0083's
   argument that depth carries no control — every artifact passed its own test against `M_auth`, so
   no depth reveals anything per-artifact testing did not already permit — appears to transfer to
   level selection unchanged, but it was written about a treed cut and not about levels.
2. **Or is the defect in the client?** If artifacts are meant to be replica-synced once and
   reconciled, then the wire is right, the viewer is wrong, and the work is in
   `clients/` plus an obligation written down. These two answers are not exclusive: a replica client
   still has to sync 464,655 artifacts once, and 49 MB once is a different problem from 49 MB per
   pan but not obviously an acceptable one.
3. **Where does the ceiling live, and what does it refuse on?** Artifact count, response bytes, or
   per (layer, level)? It must be fail-closed — the design's own §6.1 rules out meeting a limit by
   sampling, since dropping half the boundaries gives a wrong map rather than half a map — so the
   only options the representation leaves are *serve them all*, *serve ancestors* (unavailable on
   tiered) and *refuse*.
4. **Does the zoom→level map stay advisory?** It is advisory by ruling, and it is already
   declared, compiled and published — at zoom 0 this corpus's map names level 0 alone, 254 artifacts
   against the 464,655 served. If a level becomes requestable, the map is what a client would drive
   it from, and two questions become live: whether the server may reject a level for a zoom, and
   whether a request that names no level should default to the map rather than to everything.
   ⊘ The second would change what an existing client receives, which is a contracts question and not
   only a defaulting one.
5. **Is 464,655 artifacts in one layer a corpus Tessera should serve, or a declaration that should
   be different?** Decision 0092 declined a bound partly on the premise that the un-helped shape's
   *"realistic counts are thousands"*. This layer is two orders of magnitude past that and is a
   published boundary set — the case
   [`polygon-membership.md`](../../design/polygon-membership.md) §1 names first. The premise is
   worth re-examining even though this layer is not the shape 0092 was declining to help.
6. **What is the interaction with polygon membership?** The same boundary sets arrive there as
   polygons rather than as an enumerated hierarchy. If a design answers response volume for one and
   not the other, the second will re-raise it.

## 5a. What was decided and built, 2026-08-28

**Answered by the owner the same day, and built** — decision 0103,
contracts r41, `annotation-representation.md` r8, and S10's row in
client-delivery.md.

- **Q1 — is the level a request parameter?** Yes. `/v1/viewport` takes `levels`, and its **absent
  case is the layer's own declared zoom→level map** against the request's depth. The declaration and
  `/v1/meta` already carried the ranges and the request already carried the same 0–16 coordinate;
  nothing joined them. The disclosure question was confirmed rather than assumed and 0083's argument
  does transfer: every artifact passed its own criterion against `M_auth` before the selection runs,
  so fewer levels serve strictly less and more serve only artifacts that had already cleared their
  own test.
- **Q2 — or is the defect in the client?** Both, and this is the half that was on the wire. The
  client half stands (§4.4).
- **Q3 — where does the ceiling live?** **Nowhere.** Degraded service beats none: the response
  discloses nothing and a rerun costs nothing, so the build reports the whole-layer artifact count
  per level and the response is served.
- **Q4 — does the zoom→level map stay advisory?** No: it becomes the **default**, overridable by
  naming `levels`. The client still chooses; following the published map stopped being an intention
  with no expression.
- **Q5 and Q6 — is 464,655 a corpus to serve, and what about polygon membership?** Not decided here.

**A second defect fell out and is fixed with it.** The response said nothing about which level an
artifact was at, so the client counted `parent_id` links — the depth of the chain that reached it
*in that response*, which is a different question. On `clusters/toponymy` the two disagreed on 490
of 797 artifacts, 186 drawn at level 0 where 16 are declared. The *artifacts* frame now carries a
non-nullable `level`.

**Measured on this bundle after the change**, same machine, one run each, `k = 0`, `layers: "all"`,
whole extent:

| Request | Time | Bytes |
|---|---:|---:|
| zoom 0, `levels` omitted (the declared map) | 579 ms | **0.03 MiB** |
| zoom 0, `levels: "all"` | 6,087 ms | 50.82 MiB |
| zoom 3, `levels` omitted | 415 ms | **0.50 MiB** |
| zoom 3, `levels: "all"` | 1,716 ms | 50.82 MiB |

⊘ **These are single runs on a machine serving three other bundles**, so the absolute times are not
comparable with §1's and none should be quoted as a performance result. The *ratio within one run*
is what the conclusion rests on, and it is three orders of magnitude on bytes at the overview. The
saving is not only bytes: the level check sits above the projection build, so a skipped level pays
no candidate walk, no masked probe and no derived geometry over its members.

**What remained after that, in the levels a request does serve, was the artifacts' own names**: the
supplied content of every served artifact was read one zstd block at a time, ≈163 µs each, which is
why a level of 23,821 artifacts cost 3.75 s here whatever the response carried. A level's contents
are now read once per level and held — S18 in client-delivery.md
carries the before-and-after.

## 6. Reproducing

```bash
~/venvs/ingest/bin/python -m test_corpora.geonames.prepare
cd "$TESSERA_LADDER/geonames" && tessera check && tessera build
./run_demo.sh --bundle "$TESSERA_LADDER/geonames/bundle" \
  --terms "$(cat "$TESSERA_LADDER/geonames/country-terms.txt")" \
  --ranks "$TESSERA_LADDER/geonames/country-ranks.json" \
  --label 'GeoNames' --no-viewer
```

Then POST `/v1/viewport` with and without `layers`, at each preset in the `datasets.json` that
`run_demo.sh` writes under the viewer's `public/` directory — generated per machine and not
committed, which is why it is described rather than cited; the presets are measured per bundle and
carry the term lists — and at a square window of side 2⁻ᶻ for the zoom sweep.

⊘ **These are single-run figures on a machine also running three other `tessera serve` processes.**
They are an order of magnitude apart from each other, which is what the conclusion rests on; none of
them should be quoted as a performance result.
