# Selection-route chooser and the gather probe

**Goal:** land the free win in §7.2's selection path (`V ≤ k` needs no selection at all),
record the storage-order scan as a third route with its cost model, and respecify the
gather probe so it measures the axes that decide between them.

**Status of the evidence:** the cost model below is *modelled*. Phase 0 measured every
bitmap primitive and no column read (`optimisations.md` §3.5). Nothing here should be
treated as measured until Task 2 reports.

---

## What is settled without measurement

**The storage-order scan needs no unmasked read.** The mask is keyed by row index and
§10.3's organising property is that the row ID *is* the array index, so the walk is
`for row in lo..hi { if mask.contains(row) { emit } }` — bitmap iteration plus a gather
for rows already established as visible. Accumulo must decode each cell because the
visibility label lives in the data; Tessera does not. §10.4's structural rule holds
unmodified, and no exception is needed.

**Timing is C4, already open.** Scan length is k/coverage, correlating with the
principal's *own* coverage — benign by C14's accepted reasoning. This widens C4's scope
and does not open a new channel. C4 already owes a quantification.

**§10.4's rule is not type-enforced today.** `ColumnsRef` exposes `x()`, `y()`,
`priority()` as raw `&[T]` over the whole column. That is a pre-existing gap, not
something the scan introduces. Out of scope here; noted so it is not rediscovered.

## The cost model

E = extent rows, V = visible rows in extent, R = priority-sorted runs in the extent
(= 4^(leaf depth − tile depth)), pages of 4 KB.

| route | pages |
|---|---|
| **CL** candidate list | ~k scattered gather; yields k only above coverage 1/c |
| **MD** mask-driven direct | `min(V, E/2048)` priority read **+** ~k scattered gather |
| **SS** storage-order scan | ~R run heads **+** `min(k, k/(512·coverage))` contiguous gather |

Worked at depth 6 on the 10⁹ bundle (E = 266k, R = 16, k = 30):

| coverage | V | MD | SS |
|---|---|---|---|
| 25% | 66,000 | 160 | **16** |
| 1% | 2,660 | 160 | **22** |
| 0.1% | 266 | 160 | **46** |
| 0.01% | 27 | **54** | 43 |

Consequences: the crossover axis is **V in absolute terms**, not coverage — MD pays for
the whole priority block above V ≈ E/2048. SS dominates MD across the realistic range
near leaf depth. CL may survive only where R is large.

## The chooser

Conditions are disjoint; evaluate top to bottom.

| condition | route |
|---|---|
| V ≤ k | gather all visible in range — **no selection** |
| R large (coarse zoom) | CL, or the session-established coarse summary |
| V ≤ E/2048 (below the priority block's page count) | MD |
| otherwise | SS |

Every condition is decided from quantities already in hand: V is free from §2.6 step 6,
k is the request, R is a property of tile depth. No new structure, no new index.

**A priority index was considered and rejected.** A per-node priority-sorted row-ID list
*is* the candidate list with the width parameter removed (already sized at ~17 GB for
c=100 at 10⁹, rejected). The transposed form — 65,536 Roaring buckets by priority value,
walked from 0 until k hits — is a candidate list of dynamically chosen width, and beats
MD only at high coverage, which is where SS wins by 10×.

---

## Task 1 — the `V ≤ k` fast path

No measurement needed; correct at any k, and under the drawn-mark budget it becomes the
common case (at k=10⁷ a working-zoom viewport holds ~3×10⁶ visible rows, so V ≤ k across
the board and the whole selection machinery is bypassed).

- [ ] In the selection path, branch on `V <= k` before choosing a route: emit every
      visible row in range, read no priority column, build no candidate list.
- [ ] Test: for a tile with V ≤ k, output equals the full visible set and the priority
      column is not touched.
- [ ] Differential: the fast path's output must equal the existing route's output
      exactly for the same (tile, principal) — both compute §7.2's definition.

## Task 2 — respecify the gather probe (`optimisations.md` §3.5, drawn-mark P4)

The probe as written sweeps k at fixed zoom. That misses the axes the model turns on.

- [ ] Sweep **V in absolute terms**, not coverage — the two diverge with extent, and the
      crossover is stated in V.
- [ ] Sweep **R via tile depth** — it decides whether SS exists at all and no current
      document models it.
- [ ] Sweep **k** — it scales SS's gather and CL's width but not MD's priority read.
- [ ] Establish **what fraction of the duty cycle is V ≤ k** first. If it is most of it
      under a large mark budget, the CL/MD/SS crossover matters far less than the corpus
      assumes, and the rest of the sweep can be scoped down.
- [ ] Keep the scattered-vs-signature-clustered control: that is where the sign of the
      clustering effect flips, and it is what §4's retrieval argument rests on.

## Task 3 — SS as a third route

Blocked on Task 2. Implement only if the measurements hold.

- [ ] `for row in lo..hi { if mask.contains(row) { emit; break at k } }`, with an R-way
      merge across priority-sorted runs above leaf depth.
- [ ] Differential against MD for every (tile, principal): identical output, row for row.
      This is the real safety net — the third route is self-checking against the first.
- [ ] Record the C4 widening in Appendix C.

## Not in scope

- Type-enforcing §10.4 (`ColumnsRef` raw accessors). Pre-existing; separate decision.
- Retiring candidate lists. The model suggests they survive only at coarse depth; that
  is a conclusion for after Task 2, not a task.
- The coarse-zoom summary path. Named in the chooser, designed elsewhere.

Provenance: brainstorming session 2026-07-30. Companion to
`docs/design-memos/2026-07-30-priority-as-identity-prefix.md`, which shares Task 1's
call site.
