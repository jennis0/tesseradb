# Merge design review — what to cut, what to change, and what decision 0043 does to the shape

**Date:** 2026-08-03
**Status:** Evidence — review memo, never normative. The design under review is
`flush-and-merge.md` §5 and the merge half of §1–§2; the implementation reviewed is
`MergePolicy::select` and `execute_merge` (`tessera-store/src/merge.rs`), `coalesce_delta_tiers`
(`tessera-authz/src/tier.rs`) and `unsplit32` (`tessera-spatial/src/morton.rs`), all verified in
the tree, with their tests. Written against decision
[0043](../../decisions/0043-geometry-maintenance-never-blocks-a-request.md), which arrived
mid-review and changes the publication half of the answer.

---

## 1. Results and recommendations

1. **Split merge into two publications, and land the entity-space half first.** What the design
   calls "merge" bounds three growth axes with one mechanism: delta-tier count (spec §5.2),
   external-id run count (spec §5.2b), and segment count (spec §5.1). The first two are
   content-preserving re-encodes of entity-space files — they touch no row space, move no row,
   rotate no cache key, and are therefore conforming with decision 0043 *by construction*. Only
   the third — the row-space merge — invalidates projections and needs 0043's unbuilt mechanism.
   Publishing tier-and-run coalescence on its own, without a `segments_version` bump, delivers
   most of merge's near-term benefit at a small fraction of its risk. §3 prices this.

2. **Cut the base locator repair — it guards a case the implementation cannot reach.** Merge
   consumes flush segments, and the build run (`external_id_runs[0]`) belongs to no flush
   segment, so no merge ever coalesces it; and every base locator ordinal is below the build
   run's own length, so it resolves inside run 0 whatever happens to later runs. Spec §5.2b's
   repair obligation should become a two-line invariant plus an assertion (§4). No format change,
   no rewrite.

3. **Cut the one-swap coupling; a merge publishes as its own swap.** §1.3's justification (the
   pin relation) is gone with decision 0041, and under 0043 the coupling is actively harmful:
   it makes the flush's cheap append-patch window carry the merge's expensive re-projection.
   Separate swaps also delete §1.2's empty-buffer-tick special case. The owner already leans
   this way; §6 D3 records it as a ruling.

4. **The 10.7 s fleet-wide rebuild is an artefact of the projection type, not of merge — and the
   plan's finding overstates the API gap.** `RowProjection` records only a covered-extent count
   and the boundary `seg_id`, so any merge fails `extends_to` and falls to the full rebuild
   (**measured 10.7 s at 10⁹**). But I11's own wording is the licence for a cheap patch: *"no
   later extent's `row_base` moves … inside the merged span a row id names a different entity"* —
   so outside the merged span's row range the old bitmap is exact, and the patch is
   `remove_range` over the span plus one extent projection. The cache API already expresses this:
   `get_or_derive` hands the derivation closure the source entry (`viewport.rs`), and what is
   missing is a method on `RowProjection`, not cache surface. The plan's "the cache API cannot
   express it (`extend` only appends)" conflates the two. §5 gives the construction and its
   soundness argument.

5. **Decision 0043 binds flush too, and the larger flush exposure is the fragment path, not the
   projection patch.** `Engine::fragment_for` runs a full `build_fragment_with_deltas` per
   credential per tick on the request thread — its own doc says "**This is the rebuild**" and
   marks the incremental form ⊘ (plan Task 12's remainder). Its cost at 10⁹ is **unmeasured**;
   architecture §13.1's "posting union over billions of postings takes seconds" is the modelled
   scale. Under 0043 that is update-induced, request-visible work every 90 s. Task 12's
   remainder is thereby promoted from "whenever convenient" to load-bearing. §5.2 prices the
   conforming mechanism for both caches.

6. **One record correction.** Decision 0043's text, the plan's finding 1 and
   `session.rs`'s ⊘ docstrings all state that `ProjectionBuilding` maps to a fail-closed 500
   today. The tree disagrees: `map_engine_error` has explicit arms taking `ProjectionBuilding`
   **and** `FragmentBuilding` to `ApiError::Backpressure` — HTTP 429 with `Retry-After: 1` —
   and two tests assert exactly that (`error.rs`,
   `map_engine_error_takes_projection_building_to_backpressure` and its fragment twin). The
   ⊘ markers in `session.rs` are stale and should come out; decisions are immutable, so 0043's
   error is corrected here rather than in place. The substance of 0043 survives the correction —
   a 10.7 s build window answered with repeated 429s is still visible — but the symptom is
   retryable backpressure, not a 500.

7. **One defect to fix before Task 22: `execute_merge`'s watermark is a regression waiting for a
   publisher.** The code sets `FlushOutput::watermark` and `entity_id_high_water` to
   `entity_hi + 1` *of the merge's own inputs*, while the comment beside it claims both are "the
   caller's current values". For any merge not containing the newest segment those differ, and a
   publication honouring them would move the watermark **backwards** — flushed entities above it
   would be excluded from every fragment (invisible, silently), and §7.1's buffer reconstruction
   and rotation reasoning both assume the watermark never regresses. Fix: `MergeSpec` carries
   the current values, and the publication refuses a regression outright. Small, and it must
   land with Task 22, not after it.

8. **Keep the rest as built.** The three policy knobs, the power-of-two tiering, first-window
   selection, re-sort-through-`write_segment`, and the tier coalesce's read-everything shape are
   all right, for reasons §2 states — none is LSM cargo culting, and under 0043 the re-sort's
   constant factor is entirely off the request path where the owner's tolerance applies.

## 2. What the code does today, verified

- `MergePolicy::select` (`merge.rs`) takes the first window of `tier_width` list-adjacent
  segments with strictly increasing, non-overlapping entity ranges, equal power-of-two size
  class over `max(size, floor)`, total within `max_merged_segment_bytes`; 11 test functions in
  `merge_selection.rs`. Adjacency is `hi < lo`, deliberately not `hi + 1 == lo` (deleted-at-flush
  gaps).
- `execute_merge` (`merge.rs`) reads its inputs' Morton codes, residuals, columns and runs,
  recovers axes bit-exactly via `unsplit32` (round-trip asserted in `morton.rs`'s tests),
  re-sorts through `sort_batch` and writes through `write_segment`; row-count preserving,
  refuses out-of-order inputs and foreign-shard identities; coalesces runs by caller key and
  writes a run-local locator extent. Tests in `merge_execution.rs` cover row count, sort,
  point identity (multiset over `(tessera_id, code, residual)`), extent mapping, run order and
  refusal.
- `coalesce_delta_tiers` (`tier.rs`) unions `(term, entity)` pairs across tiers into a
  `BTreeMap`, dedups, re-encodes through `write_delta_tier`; nothing dropped, nothing consulted.
- Merge publication does not exist (⊘, Task 22), so every hazard below is latent, not live.
- On the request path today: a flush rotates every row-projection key; the first request per key
  derives via `RowProjection::extend` (append-only patch) or rebuilds in full; concurrent
  same-key requests get 429 `backpressure`, `Retry-After: 1`. A stale session's fragment is
  rebuilt in full per credential per tick via `fragment_for`, same 429 behaviour for racers.

**Three knobs, adjudicated.** Tiering is not imported complexity: without size classes the
policy re-merges the one big segment with each tick's new small one — write amplification
quadratic in corpus growth rather than logarithmic — so `tier_width` and the class rule earn
their place. The floor is one `max` clamp whose absence is a *silent* never-merges failure when
flush sizes straggle across class boundaries (the module doc says exactly this). The cap is what
excludes the base segment (spec §5.3's relation) and bounds one merge's pool time. Flush
segments are near-uniform only at steady ingest — **assumed, not measured**; the floor is the
cheap hedge for when they are not. First-window-not-best trades a marginally better choice for a
policy predictable from the manifest — right, per CLAUDE.md's audit-before-performance rule.

**Re-sort versus k-way merge.** The k-way merge saves an O(n log n) → O(n log k) constant on a
background task bounded by `max_merged_segment_bytes`, at the price of a second writer that
knows segment layout. The single-writer argument (`write_segment` stays the one place the format
lives) is the reviewability trade CLAUDE.md instructs, and 0043 removes any request-path stake in
the constant. Keep. One hardening note: `gather_scalars` silently drops a schema column an input
segment lacks (`filter_map` over `columns.scalar(name)?`) — unreachable for segments written by
`write_segment`, but it should be an error, not a skip.

## 3. Which growth axis actually hurts, and what that licenses

Nothing on any of the three axes is measured — stated plainly, with the probe that would settle
it named in §7. Modelled:

- **Run count (write path).** Every `/control/ingest` row's duplicate check searches every run
  whose own key bounds admit the key (contracts §2.4); with uncoordinated external ids most
  bounds admit most keys, so the check is effectively O(runs) binary searches *per ingested
  row*. At ~1,000 runs (one day at 90 s) this is the axis with per-row, steady-state cost on the
  write path. `/v1/items` drill-down pays the same scan per lookup.
- **Tier count (authorise path).** A fragment build pays one binary-search miss per satisfied
  term per tier — O(satisfied × tiers × log(terms/tier)), milliseconds at 1,000 tiers against a
  base-postings union modelled in seconds. Smaller than the r1 review's framing suggests, but it
  recurs in every per-credential rebuild every tick, so bounding it is cheap insurance taken at
  the same time as the runs.
- **Segment count (tile path).** Each tile resolves one range per live segment (§11.3); a
  viewport of ~100 tiles over 1,000 segments is ~10⁵ binary searches plus per-segment
  `range_cardinality` calls — tens of milliseconds, degrading linearly at ~960 segments/day.
  Real, but the slowest-burning of the three, and the only one whose cure invalidates
  projections.

**The licence this gives:** coalesce tiers and runs (with their locator extents) as a
publication of their own — a manifest edit over `deltas`, `external_id_runs`, `locator_extents`
and the `files` map, with **no row-space field touched and no `segments_version` bump**, riding
the same swap machinery as an overlay publication. Cached fragments stay valid because the
union across tiers is content-identical and the disk key carries the watermark, not the tier
list; row projections are untouched by construction. `MergePolicy::select` and the coalesce
halves of `execute_merge`/`coalesce_delta_tiers` are reusable as-is. The coalesced tier and run
need ids from a never-reused namespace, exactly as `seg_id` (contracts §2.1), for the same
ABA-safe rebase. The row-space merge then runs at whatever (much lower) frequency the tile-path
probe justifies, gated on §5's mechanism.

## 4. The base locator: an invariant, not a repair

Spec §5.2b requires a merge to "either rewrite the base locator or convert it to the same form",
because base ordinals are positions in the listed-order concatenation of runs. Verified in
`sidecar.rs`: base ordinals are indeed concatenation positions, resolved by walking runs in
listed order; flush locator extents are run-local. But two facts make the repair unreachable:

1. The build wrote `ext-locator.u32` when the build run was the only run, so **every base
   ordinal is below the build run's length** and resolves inside run 0 regardless of what later
   runs coalesce into.
2. A merge's inputs are flush segments, and the build run belongs to none, so **no merge ever
   consumes run 0** — and the sidecar's path derivation from `external_id_runs[0]` stays valid
   provided publication keeps run 0 listed first.

So the obligation reduces to: *the build run is never a merge input, stays listed first, and
base ordinals never index past it* — stated in §5.2b in place of the repair, enforced by a debug
assertion at publication and one test. This is the same shape as the base segment's exclusion
(§5.3): not a rule, a consequence — and cheaper than either the repair or a format change.

## 5. Satisfying decision 0043

### 5.1 The span-local patch is sound, and smaller than the plan thinks

Claim: after a merge, a projection over the new row space equals the old bitmap with the merged
span's row range cleared and the merged extent's projection OR'd in.

Soundness, from properties the code already has: (a) merge is row-count preserving, so no later
`row_base` moves (`execute_merge`, asserted by `collapsing_adjacent_extents_preserves_every_row_id`);
(b) the merged extent's rows are exactly `[row_base, row_base + row_count)`, one contiguous
range; (c) the base permutation and every unconsumed extent are untouched; (d) the fragment is
entity-space and merge changes no entity's membership. So bits outside the span are exact, and
inside the span the re-projection is definitionally what a full rebuild would produce there.
Cost: `remove_range` is O(containers in the span) and the re-projection is O(span ∩ mask) — the
span is bounded by `max_merged_segment_bytes`, so the *policy* bounds the patch. All **modelled**;
the byte-equality property test (the same obligation as flush §3.4's) is what makes it true.

What it needs: `RowProjection` carries the covered `seg_id` list (bounded by segment count, a
few hundred strings) instead of only `boundary_seg_id`; a `rebase(fragment, rows)` method that
diffs the covered list against the live extents, clears the replaced window's row range and
re-projects from the divergence; and one more branch in `viewport.rs`'s derive closure.
`get_or_derive` already delivers the source entry — no cache surface changes. Contained in
`compose.rs` + `viewport.rs` + `cache.rs`.

Note the fallback ladder this creates, cheapest first: append patch (`extend`, flush) →
span-local rebase (merge) → extents-only re-projection (multi-generation gap: clear all rows
above the base region, re-project every extent — never re-projects the 10⁹-row base, which is
what the 10.7 s actually buys) → full build (cold key only). The full build then occurs only at
session establishment, which is not update-induced and outside 0043's scope.

### 5.2 The mechanism: eager background refresh over resident keys

At each geometry publication the executor submits **one** pool task (decision 0035's
no-O(sessions)-executor-step rule is kept: submission is O(1)); the task walks the resident
row-projection keys at `segments_version − 1` and derives each into its new key via the ladder
above. Work per tick is bounded by **cache residency, not session count**: the dominant cost is
the source-bitmap clone, so a tick's refresh is at most the cache byte bound in memcpy
(**modelled**: ~2 GiB bound → ~16 wide-grant entries at the measured 125.12 MB each →
sub-second aggregate on the pool) plus the extent unions. A request racing the refresh falls
back to today's inline derive — milliseconds under the ladder — or a 429 `Retry-After: 1` for a
same-key racer. That residual is the whole of what remains user-visible, and whether it counts
as "invisible" is D1, the ruling this memo asks for.

The same mechanism carries the fragment axis: the disk fragment cache's resident credentials are
refreshed in background at publication using §11.2's incremental form
`old ∪ (delta ∩ satisfied)` — plan Task 12's ⊘ remainder, now load-bearing rather than
convenient, with its byte-equality test as specified there. Until it lands, flush itself is out
of conformance with 0043 on its largest term, and no merge work changes that.

### 5.3 Serve-the-superseded-generation is expressible but fail-open as conceived, and I
recommend against it

The attraction: a session whose projection is not yet refreshed is answered from the previous
generation — strictly zero added latency, and no visibility regression, because a flushed item
was *invisible* while buffered (arch §11.2: no row, no contribution), so serving the old
geometry only extends ack→visibility for that session by the refresh time.

The fail-open, confirmed in the tree: a deny window updates the **live** generation's `denied`
mask (`write.rs`, incremental clone-and-add) and re-derives at geometry publications; nothing
maintains a superseded generation's mask after the swap. Serving old geometry with its frozen
mask drops every deny accepted since — the exact shape lifecycle §3's rules exist to prevent.
It is fixable — additions fan out to each retained generation via `row_of` over its own row
space (an entity with no old row is invisible there entirely, so contributing nothing is
fail-safe), any unsuppress re-derives per retained generation — but that is a second
denied-maintenance path plus a retained-generation registry, which is the drain-list shape
decision 0041 deleted three days ago. Take it only if D1 rules "invisible" as strictly zero.

## 6. Decisions needing an owner ruling

**D1 — Define "invisible" (0043's operative word).**
Options: **(i)** a stated budget — update-induced request-path work bounded by one derive
(milliseconds under §5.1's ladder), with a 429 `Retry-After: 1` residual for same-key racers
during a refresh window; **(ii)** strictly zero — requires §5.3's serve-superseded machinery
with deny fan-out to retained generations.
Recommendation: **(i)**, with the budget stated in the conformance obligation so it is testable.
Cost if wrong: a rare visible derive or 429 under (i); retrofitting (ii) later reuses all of
(i)'s derive work and adds only the retention machinery — no rework, just addition.

**D2 — Split merge into an entity-space coalesce publication (no `segments_version` bump) and a
row-space merge publication (gated on D1's mechanism).**
Options: split as §3, or keep the single mechanism and gate all of it on D1.
Recommendation: split. The coalesce is 0043-conforming today, bounds the write-path and
authorise-path axes now, and shrinks the row-space merge to the one hazard it actually is.
Cost if wrong: two publication paths where one would have done; if the tile-path probe shows
segment count bites faster than modelled, the row-space half lands sooner and the split cost
two smaller reviews instead of one large one.

**D3 — A merge publishes as its own swap.** §1.3's one-cadence rule loses its stated
justification with decision 0041, and under 0043 the coupling makes the flush window carry
merge's cost.
Recommendation: adopt; delete the empty-buffer-tick rider rule with it.
Cost if wrong: one extra `segments_version` bump per merge — one more background refresh round,
nothing a viewer observes.

**D4 — Promote Task 12's incremental fragment form to a 0043 prerequisite, after measuring.**
`build_fragment_with_deltas` per credential per tick on the request thread is the largest
unmeasured term in flush's own 0043 conformance.
Recommendation: run the probe (§7, P2) first; if the figure is seconds-scale at 10⁹, Task 12's
remainder lands before merge publication.
Cost if wrong (the build measures cheap): a small task done early that was owed anyway, with its
byte-equality test.

## 7. Measurements that do not exist, and the probes that would produce them

- **P1 — `RowProjection::extend` and the §5.1 rebase at 10⁹**: clone-versus-union split, per
  entry; sizes the refresh tick and D1's budget. Extend the `2026-07-31-1e9-rebuild` fixture.
- **P2 — `build_fragment_with_deltas` at 10⁹** per credential, against tier counts 1/10/100:
  the D4 figure.
- **P3 — the three growth axes**: viewport p50 vs live segment count, ingest duplicate-check
  cost vs run count, fragment build vs tier count, at 10/100/1,000 each on a synthetic
  multi-segment bundle. Sizes coalesce and merge frequency; until it runs, every figure in §3
  is modelled.
- **P4 — `execute_merge` + `coalesce_delta_tiers` wall time at the policy cap**: pool
  occupancy and the skip-alarm threshold (spec §1.1), not a request-path figure.

## 8. What changes in the corpus

- `flush-and-merge.md`: §1.2/§1.3 (merge as its own swap; empty-buffer-tick rule deleted);
  §5.2b (repair obligation replaced by §4's invariant); §9 (request-path derive superseded by
  the 0043 mechanism; the ladder and refresh stated); §14 (obligations: span-local rebase
  byte-equality; watermark non-regression under merge publication; the D1 budget as a testable
  bound); a new section or cross-reference for the coalesce publication's manifest semantics.
- `.superpowers/sdd/…/plan.md`: Task 22 splits (22a coalesce publication, 22b row-space
  publication + mechanism); Task 12's remainder re-sequenced per D4; finding 1's cache-API claim
  corrected per §1.4.
- Code comments (follow-ups, none made in this review): `session.rs`'s two stale ⊘ markers on
  `ProjectionBuilding`/`FragmentBuilding`; `viewport.rs`'s "the session that asks pays"
  paragraph (overruled by 0043); `merge.rs`'s watermark comment with its fix (§1.7);
  `gather_scalars`' silent skip (§2).
- Decision records: the D2/D3 outcomes as one record; 0043's 500-claim corrected by citation to
  this memo (decisions are immutable).
- Leak register: nothing new — a coalesce changes query cost and nothing else (timing-only,
  the C19-adjacent note), and the row-space merge's observables are already covered by I11.

## Appendix — claims verified against the tree, with sites

`extends_to` refusal on a shorter extent list and the one-string boundary check
(`compose.rs::RowProjection`); derive-from-previous-generation and inline `pool.install` build
on the requesting thread (`viewport.rs`, row-projection cache block); 429 mapping and its two
tests (`error.rs`); full fragment rebuild per credential per flush and its ⊘
(`session.rs::fragment_for`); incremental deny-mask update on the live generation only
(`write.rs`, deny-window arm); retention depth and pruning (`cache.rs::KEEP_SUPERSEDED_GENERATIONS`,
`prune_generations_below`); base-locator ordinal semantics and the run-0 path derivation
(`sidecar.rs`); merge watermark values (`merge.rs::execute_merge`, tail of the function);
`with_merged`/`collapsing` positional-divergence hazard already handled by `seg_id` keying
(`viewport.rs::segments_with_row_bases` comment). Figures: 10.7 s and 125.12 MB are **measured**
(`2026-07-30-viewport-hot-path-and-bundle-size-review.md`, `cache.rs`); every §3 axis figure and
every §5 cost is **modelled**; flush-segment uniformity is **assumed**.
