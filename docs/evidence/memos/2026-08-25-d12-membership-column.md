# The per-point membership column (D12) — built, measured, and the leak-register row proposed

**Date:** 2026-08-25 · **Track:** `s3` of the client-components campaign · **Status:** built and
tested on `server/s3-membership`; the register row below is **proposed** and awaits the owner's
ruling (design-process step 4). Two further rulings landed on the same branch and are recorded at
the end: `layers` omitted means none (D9), and a dependent artifact carries its target's masked
count (D13), the second with its own proposed register note.

## What was built

With D12, a `/v1/viewport` request that names layers gets, in every points frame, one nullable
`u64` column per layer the response served artifacts from, named `membership:<layer>`, appended
after the render scalars in the request's layer order. The value is the `tessera_id` of the
**deepest served** artifact of that layer the point belongs to *in that response*; `null` where no
served artifact of the layer holds the point. No layer served, no column: an absent column and an
all-null one say the same thing, and only one costs bytes.

*Deepest served* is resolved after the artifact pass has settled the served set — after the
verdicts, the cut and the dependent drop — and against nothing else. On a level served row-major
the point's leaf label is read off the row column and the lineage is climbed to the first served
ancestor, a few steps per point. On a level served artifact-major or spatial there is only
artifact→rows, and the inversion is done by bitmap intersection: the response's gathered rows
become one bitmap, each served artifact's rows are intersected with it — O(containers touched)
per served artifact, never point × artifact — and each row hit takes the ordinal if it is deeper
than what it holds. Across a levelled layer the finer level wins (a tiered layer's edges run from a
coarser level to a finer one; a stacked layer's are independent analyses at increasing index);
within a level the deeper in the lineage wins. A treed layer sits at level 0 and has only the
second half.

A dependent layer (`depends_on`, a label layer) gets a column like any other, resolved over its
artifacts' **own** membership rows: a label published with the members it describes names itself
on those points; one published with no members has no visible member in any viewport, is a
candidate nowhere and is absent from the frame and the column alike. A label whose target the
response does not hold is dropped with it (decision 0089) and so names nothing.

Rows above a level's base — points ingested since the last fold — resolve through a row-major
label column's live tail and are null on an artifact-major level, whose row form is the base
projection. That is the same edge the masked count already has on that layout and it closes at
the fold.

## The measurement

`cargo test -p tessera-engine --test membership_column measure_the_column_cost --release -- --ignored --nocapture`,
on the engine tests' 10,000-point fixture with a four-deep planted tree (4 + 16 + 64 + 256 nodes,
340 artifacts, leaves of ~39 points), whole map at zoom 2 (16 tiles), every point served
(`k = 10,000`). The column's cost is isolated by differencing — the artifact pass is paid whether
or not any point is served and the gather whether or not any layer is named — so
`(layers, k) − (layers, 0) − ((no layers, k) − (no layers, 0))` is the column alone. Thirty runs
each, release build, one warm run discarded.

| layout | budget | served | points | column (ms) | whole request (ms) |
|---|---|---|---|---|---|
| artifact-major | none | 340 | 10,000 | 2.0 | 4.1 |
| artifact-major | 64 | 20 | 10,000 | 1.2 | 2.7 |
| artifact-major | 16 | 4 | 10,000 | 0.6 | 2.2 |
| row-major (list) | none | 340 | 10,000 | 2.0 | 4.1 |
| row-major (list) | 64 | 20 | 10,000 | 1.5 | 3.0 |
| row-major (list) | 16 | 4 | 10,000 | 1.6 | 3.0 |

So the column is 0.06–0.2 µs per served point at this scale, a third to a half of the request it
rides on, and grows with the served set on the artifact-major route and with the points on the
row-major one — the shapes the two routes predict. **Measured on a small fixture; not measured at
the scale campaign's 10⁷ artifacts.** The artifact-major cost is bounded by the cut and the
budget on one side and the viewport on the other, so the figure to check at scale is the row-major
climb, which is O(served points × chain length) — the tree above is four deep. No fixture with a
treed layer at scale was cheap to make inside this track (`tessera-bench` is another track's
file); the number is stated at the scale it was taken.

## The leak-register row, proposed

The precedent is C29, the parent identifier: the same disclosure inverted (point→artifact rather
than artifact→artifact), bounded the same way (to the response's own served set), and
structurally rather than by a check that could be forgotten. Rulable without reading code: the
resolver is handed the served set after it is settled and holds no route to anything else, so
the column can name only an identifier the artifacts frame of the same response already carries.

The row, in the register's form:

> | **C30** *(r45)* | A served point's membership in a served artifact, on the wire | The points frame carries, per layer the response served, the `tessera_id` of the **deepest served** artifact each point belongs to, or null. It relates two things the viewer was already served — a point the selection admitted after masking (I7) and an artifact that passed its own criterion against their mask — so it says which of two disclosed groupings a disclosed point sits in. For a layer declaring a hull or a box the served shape already draws that relation; for a layer declaring neither this is the first time a viewer learns it | Low | **Named only where the artifact is in the same response**, and structurally: the value is resolved against the served set after it is settled — after the verdicts, the cut and the dependent drop — and the resolver holds no route to an artifact the response withheld. **Deepest served, never the leaf**, so a point whose finer cluster was cut to its parent, failed its own criterion for this viewer, or was suppressed, names the parent and says nothing about the finer one: a withheld artifact is indistinguishable from one that never existed, the register's usual rule. Null covers *no served ancestor* whatever the reason. No ordinal (C8), no count, no unmasked quantity, no membership size: a viewer can count the served points that name an artifact, which is a subset of the masked count they were already served beside it. **The residual is that membership is corpus-derived where the clustering is**: a viewer learns the algorithm put this point in that cluster, which they could not otherwise infer for a layer with no served shape — accepted because both ends were already disclosed to them and the relation adds no member and no count | Accepted — bounded to the response's own served set |

What deepest-served keeps out, stated once: any artifact below the served frontier. The cut, the
criterion and a suppression all remove an artifact from the served set, and the column's walk
stops at the first artifact *in* that set, so it cannot distinguish the three reasons and cannot
name what any of them removed.

The one-line `inventory.md` diff for the owner:

```
| **C30** | A served point's membership in a served artifact, on the wire | Low | Accepted — bounded to the response's own served set |
```

## D9, on the same branch: `layers` omitted means none

Owner ruling 2026-08-25, the other way from the wire as it was: a request that omits `layers` —
or sends `[]` — answers for no layer and pays no artifact pass; the string `"all"` in place of the
array answers for every layer the principal reaches; a named list is still named ∩ reachable,
never unioned, so naming a layer is not a way to learn whether it exists. The membership column
follows the layers the request resolved to. `all` is refused as a layer name at declaration
validation (case-insensitively; `all/of/them` is fine), so the word is never ambiguous. The
engine's request carries the choice as `LayerSelection { All, Named }`; the Rust builder starts
at `All` and the server's mapping is the one place the wire's default is decided. No register row:
the ruling changes what a request asks for, not what a response may disclose.

## D13, on the same branch: a dependent artifact carries its target's masked count

Owner ruling 2026-08-25, **not** the target's identifier on the wire (the ruling recorded in
`orphaned_dependents`' doc stands). A label describes its cluster, so the number beside it in the
artifacts frame is the cluster's masked count as this principal sees it — not the label's own
membership, which a publisher may leave empty. Answering the question the ruling asked: an
artifact's `masked_count` is computed under the principal's mask **only**, never under the
request's filters (`MaskedSet::count_intersection` is filter-blind by construction — a filtered
count would make the existence criterion a function of the filter, which I12 forbids), so the
label's count is the target's filter-blind count and the two agree in one response.

The proposed register note, beside C30 rather than a row of its own:

> A dependent artifact's `masked_count` is its target's. The target is in the same response with
> that very count — the dependent drop (decision 0089) removes a label whose target the response
> does not hold — so the value is derivable from the artifacts frame and discloses nothing new
> (decision 0023's rule for derived quantities). No row moves.

**Left for a ruling:** the drill-down route (`POST /v1/artifacts/{id}`) on a label still serves
the label's own count. Its response holds no target, so copying the target's count there would be
the one place the number reached a viewer without the artifact it belongs to beside it — which is
not derivable from that response, and so not covered by the note above. It is reported rather
than changed.

## Tests

- `crates/tessera-wire/src/payload.rs`: zero, one and two columns; named, nullable, after the
  scalars, round-tripping through Arrow IPC; the size estimate covers the added bytes.
- `crates/tessera-engine/tests/membership_column.rs`: the join asserted on every response (every
  value is an identifier of that layer in the same response's artifacts frame); the deepest served
  ancestor under the frontier, under a budget that climbs, and with the whole tree served; the
  coarser ancestor for a masked principal whose leaves fail the criterion, with the leaf's
  identifier nowhere in their response; null under no served ancestor; the column set following
  the layers the response served; a dependent layer over its own members; the two layouts agreeing
  over principals × viewports × budgets; the label's count equal to its target's; and the ignored
  measurement.
- `crates/tessera-server/tests/viewport_membership.rs`: the column over HTTP — named, nullable,
  after the scalars, joined to the artifacts frame; `layers: []` and a response serving nothing
  carry none; omitted means none, `"all"` means every reachable layer, any other string is a 422;
  `all` refused as a layer name.
