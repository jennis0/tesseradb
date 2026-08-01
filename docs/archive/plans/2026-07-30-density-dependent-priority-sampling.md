> **ARCHIVED 2026-08-01 — EXECUTED. Its header says 'for re-review before any code is written'; that was already false when archived — the selection definition landed at 263ba72 in select.rs.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# Density-dependent priority sampling — implementation plan

**Date:** 2026-07-30
**Revision:** 2 (r1 reviewed adversarially; three blockers and ten should-fixes folded in)
**Branch:** `density-sampling` (worktree `.claude/worktrees/density-sampling`, based on `3f4011d`)
**Status:** plan, for re-review before any code is written

**Goal.** Replace the engine's placeholder first-*k* sampler with the real selection definition
evaluated over `tessera_id`; make the drawn mark count carry masked density (floor ∪ threshold ∪
cap); land the `V ≤ k` fast path in the only form that survives the density rule; serve the §3.3
underlay sub-cell counts; and re-point the reference oracle so the differential compares against
the definition rather than the placeholder.

**Sources, and their standing:**

| Source | Standing |
|---|---|
| `docs/evidence/memos/2026-07-30-priority-as-identity-prefix.md` | Owner decision. Storage/build half already landed (`8cb3e02`…`3f4011d`); the **selection comparator** row is what remains |
| `docs/evidence/memos/2026-07-29-density-under-nesting.md` | "Direction of travel, not a specification." Owner instruction 2026-07-30: implement **§3.1 selection** and **§3.3 underlay**; **not** §3.2 stratum tags |
| `docs/archive/plans/2026-07-30-selection-route-chooser.md` | Task 1 (`V ≤ k`) **is** in scope by owner instruction. Tasks 2+ (CL/SS route chooser, cost model) are **out** — owner: those routes "had a subtle error in reasoning" |
| `docs/archive/plans/2026-07-29-drawn-mark-budget-design.md` | Probe spec. Nothing here implements a probe; its `k`-cap calibration is another plan's and is not touched |

## Owner decisions, recorded so they are not re-litigated

1. **Density scope is §3.1 + §3.3.** §3.2 stratum tags are out.
2. **θ is the server's closed form only** — `θ_d = m_target · 4^d / V_total`. No request field, no
   per-session measured anchor, no histogram. The mid-zoom cap-flat band this produces on
   clustered corpora is **accepted**, recorded, and backstopped by the §3.3 underlay.
3. **`K_max` is a server setting; a client may request `k ≤ K_max`.** The effective cap is
   `min(k, K_max)`, applied *inside* the definition so it bounds the selection heap and the
   output gather.
4. **The drop in drawn marks for sparse principals is the explicit intent, not a regression.**
   Owner: "Constant *k* hides the actual density of cells under a map with a large number of
   points; Bernoulli sampling regains some of that visual density." Recorded in §7.2 as intent.
5. **Design §7.2/§7.3 and Appendix C are amended now** (design r22), overriding the density memo's
   §0 "do not amend the design documents". Parameters are recorded as *provisional* pending §0's
   visual experiments.

---

## Global constraints

- **Design corpus lives in `docs/design/`,** which default file-search tooling skips — pass the path
  explicitly. Precedence: architecture design (**r21**) > contracts spec (**r6**) > system
  architecture (r4). `§n` unprefixed means the architecture design.
- **If plan and design disagree, STOP and report to the owner.** Do not resolve silently. Every
  design document carries an Appendix R review trail — read it before re-litigating.
- **Never `git add -A` or `git commit -am`.** Every commit step names its paths explicitly. The
  main checkout carries untracked owner files; this worktree does not see them.
- **Quality gates at every task boundary:** `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
  `bash scripts/check-layers.sh` — all clean before committing.
- **TDD.** Every task writes its failing test first and runs it to confirm a red before the
  implementation lands. A compile failure is a valid red.
- **British spelling** throughout prose and comments.
- **Baseline in this worktree:** 215 tests passing, 0 failures, at `3f4011d`.

### Invariants that bear on this plan

- **I7 — sampling happens after masking.** Every rank and count is taken over the viewer's own
  visible set. The floor clause `k_min` is what keeps the sparsest principals' maps non-empty; it
  may not be removed as an optimisation. Per owner decision 4, a *reduction* in marks relative to
  today's flat *k* is intended; **emptiness is not**.
- **I2 — every aggregate computable from inside `M_auth` alone.** θ's anchor is the **composed**
  visible cardinality — see the θ section for why the cheaper pre-overlay figure is an I2
  violation, and for the concrete channel it opens. Sub-cell counts are exact masked
  cardinalities. **Truncation option (b) of density memo §7 is not implemented** and must not be:
  it would make the drawn count depend on unmasked density.
- **I10 — entity IDs never cross the trust boundary.** Selection reads the `tessera_id` column; no
  entity ID is gathered on this path. Unchanged from today.
- **I11 / pins fix geometry, never authorisation.** θ is viewport-invariant but
  generation-dependent (below). A suppression still acts through `compose`'s diffs on every
  request, pin or no pin.

---

## The definition being implemented

Let *T* be a tile at depth *d*, `vis(T)` its visible row set ordered ascending by the row's
`tessera_id`, and `P_d` the depth-*d* threshold as a cut point over the identity space.

```
cap    = min( request_k , K_max )                      // K_max is server config
C_θ(T) = |{ i ∈ vis(T) : tessera_id(i) < P_d }|        // masked count below the cut
m(T)   = min( cap, max( min(k_min, cap), C_θ(T) ) )
served(T) = the min(m(T), |vis(T)|) smallest members of vis(T) by tessera_id
```

**Five notes, each load-bearing.**

1. **The comparator is the full `tessera_id`, with no prefix fall-through path.** Because
   `priority` is `high16(tessera_id)`, "k lowest by priority then by `tessera_id`" is *identically*
   "k lowest by `tessera_id`", so there is no composite comparator to get wrong. Design §7.2 (r21)
   instructs that no runtime prefix-scan-then-fall-through path be built in Phase 1; this plan
   honours that. **But the premise of that instruction is now gone** — §7.2 says it *because* the
   placeholder "never reads the priority column at query time", and this plan makes the query path
   read `tessera_id`. Consequence, recorded rather than hidden: the per-viewport *scanned* column
   goes from `priority` at 2 B/row to `tessera_id` at 8 B/row, a **4× rise in page traffic** —
   2 GB → 8 GB at 10⁹ — which contradicts the drawn-mark spec §7 residency table and probes §3.5's
   two-reads framing. Contracts §2.6 already permits comparing the prefix first as an optimisation;
   the trigger is the design's own `w ≈ log₂(V_max/k)`. **Not built now; the cost is written down.**

2. **The cap is outermost, and `k_min` is clamped to it.** The memo has
   `m = max(k_min, min(K_max, C_θ))`, which exceeds the cap whenever `k_min > cap` — a request of
   `k = 1` against `k_min = 2` produces exactly that. The forms are identical whenever
   `k_min ≤ cap`. Confirmed by review: for `k_min ≤ cap` both equal `clamp(C_θ, k_min, cap)`, and
   memo §6 clause 3 survives — every item with a smaller id in `vis(T')` also lies below `P_{d+1}`,
   so `C_θ(T') ≥ rank_{T'}(i)`, hence `m(T') ≥ rank_{T'}(i)`, and the outer `min(m, |vis|)` clamp
   cannot bite since `rank_{T'}(i) ≤ |vis(T')|`.

3. **Applying the client's `k` inside the definition is free, and is a work bound.** `served(T)` is
   a `tessera_id` prefix, so computing at `min(k, K_max)` and computing at `K_max` then truncating
   to `k` give **identical output**. Doing it inside bounds the selection heap and the output
   gather. **It does not bound the scan:** `C_θ` is a *masked* count, so the tile's visible rows
   must be walked regardless. Do not claim otherwise anywhere in the code comments.

4. **`K_max` and `max_k` are two different ceilings and must stay separate.** `max_k` (currently
   200, `crates/tessera-server/src/config.rs:146`) is the **machine** ceiling — GPU, transport,
   handle table — and the drawn-mark plan's Task 7 owns calibrating it, blocked until the identity
   plan's Task 15 runs. `K_max` (new, `k_max_marks`, provisional **128**) is the **overplot**
   ceiling from memo §4: "Overplot-bound, not machine-bound". Conflating them would mean that
   calibrating `max_k` upward silently dissolves the cap clause and with it the per-tile work
   bound. Effective cap is `min(request_k.min(max_k), k_max_marks)`.

5. **Ties are impossible and the corpus establishes it.** Contracts §2.6: "`tessera_id` is a
   bijection over 2⁶⁴ and there is one row per entity, so the order is total." The scheme's
   determinism rests on this, so add a `debug_assert` that no id repeats within a tile rather than
   relying on it silently.

### θ: the closed-form anchor

Priorities are uniform over the identity space by construction, so `P(id < P) = P / 2⁶⁴` and the
expected served count in a tile of *n* visible items is `n · P_d / 2⁶⁴`. Requiring the mean
occupied tile at depth *d* to draw `m_target` marks, under a uniform-occupancy approximation where
depth *d* has ~4^d occupied tiles each holding `V_total / 4^d`:

```
P_0 = ((m_target as u128) << 64) / (V_total as u128)      // u128: the u64 form cannot hold this
P_d = Saturated                     if P_0 ≥ 1u128 << 64
      Saturated                     if P_0.leading_zeros() < 2d
      Cut((P_0 as u64) << 2d)       otherwise
```

`P_{d+1} = 4·P_d` is exactly the memo's `θ_{d+1} = 4·θ_d`, so the per-tile expectation is
depth-stable and θ is monotone in depth — what nesting clause 3 needs.

**The overflow test must be `leading_zeros`, not `checked_shl`.** `u64::checked_shl(n)` returns
`None` only for `n ≥ 64`; for `n < 64` it performs a *wrapping* shift and discards the high bits.
So `P_0 = 2⁶³` at `d = 1` yields `Some(0)` → `Cut(0)` → `C_θ = 0` in every tile at every depth →
every tile draws exactly `k_min`, forever, with no error raised anywhere. There is no
`saturating_shl` in std. A test must sit one bit below the boundary at successive depths; a test
that only checks "`P_{d+1} == 4·P_d` until saturation" passes under the broken form for small `P_0`.

**Representation.** `enum Threshold { Cut(u64), Saturated }`. `Saturated` is a first-class state,
not a rounding artefact: `Cut(u64::MAX)` would wrongly exclude `id == u64::MAX`, whereas
`Saturated` means "admits every id", which is what makes the fast path exact at the boundary.

**The anchor quantity is the COMPOSED visible cardinality**, `|base| − |minus| + |plus|`, not
`|base|`. This was a blocker in review r1 and the reasoning must not be lost:

> `base` is `RowProjection::new(&session.fragment, …)` — `M_auth` *before* the overlay diff
> (`compose.rs:44-48`). After any accepted delete/suppress, `base ⊋ M_auth`; after a predicate
> widening, `base ⊊ M_auth`. I2 requires an aggregate be computable from inside `M_auth` alone, and
> `|base|` is not. The channel is concrete: the viewer receives the exact masked `visible` for
> every tile, and mark count is ≈ `V_tile · m_target / anchor` with `m_target` a fixed constant.
> Aggregating over a few hundred tiles the viewer solves for the anchor, differences it against its
> own summed `visible`, and obtains **a running estimate of how many of its own items have been
> denied** — a count of items outside `M_auth`. No Appendix C row covers that.

The composed figure costs almost nothing: `minus`/`plus` are tiny by construction, so it is
`base.cardinality()` plus O(containers in the diffs).

**θ is viewport-invariant but generation-dependent, which is what the memo actually requires.** It
depends on the session's mask and the generation, *never* on `bbox` or `zoom`, so it does not move
on pan — the churn memo §3.1 forbids. It does move when an overlay swap changes the composed
count. That is accepted: overlay swaps are rare against pans, and because `served(T)` is a prefix,
a small θ move perturbs only the marks nearest the cut.

**Edge cases.** `V_total == 0` → `Saturated` (every tile is empty and skipped). `V_total ≤ m_target`
→ `P_0 ≥ 2⁶⁴` → `Saturated`; correct, a viewer who can see almost nothing should be shown all of
it. `anchor == 0` with a non-empty composed mask **is reachable** — a predicate widening onto an
entity outside the frozen fragment lands in `plus` (`compose.rs:229`) — and yields `Saturated`,
which is benign; test it.

**The accepted approximation, stated in full because it is what the owner accepted.** The anchor
assumes the viewer's items spread over ~4^d occupied tiles. Real corpora cluster, so the true
occupied-cell count `O_d` is smaller and **actual marks per tile = `m_target · 4^d / O_d`**. For a
point set of box-counting dimension *D*, `O_d ~ 2^(D·d)`, so the inflation is `2^((2−D)d)` —
geometric in depth. Worked: 10⁶ visible, `m_target` 16, `K_max` 128, depth 6. Even spread → 4,096
occupied tiles of ~244 items → 16 marks each, as designed. Clustered into 400 tiles → 2,500 items
each → 164 marks → **pinned at the cap**, so a tile with 2,500 visible and one with 250,000 render
identically.

It is a **band, not everywhere**: at coarse zoom nearly every cell is occupied so the assumption
nearly holds; at fine zoom θ has saturated and the mean occupied tile holds fewer than `K_max`
anyway. Depths ~5–8 in the worked example.

What θ does *not* do is set the width of the proportional window. A cell with *n* visible draws
`θ·n` marks — exactly proportional to density, since cells are equal screen area. Proportionality
holds while `k_min ≤ θ·n ≤ K_max`, a density ratio of `K_max/k_min` = 64, **independent of θ**. θ
positions that window on the density axis; the floor and cap set its width. The deficit therefore
mis-*positions* the window rather than narrowing it. Memo §9 already accepts floor-flat and
cap-flat regions and backstops them with the underlay, which this plan ships.

### The `V ≤ k` fast path, in the only form that survives

The selection-route plan's Task 1 reads "branch on `V <= k` … emit every visible row in range,
read no priority column". **Under the density rule that condition is unsound**: the threshold
clause deliberately serves fewer than *V*, so a tile with `V = 100` and `C_θ = 5` serves 5.
Serving 100 would destroy the density signal that is the entire point.

Serving all of `vis(T)` is exact iff `m(T) ≥ |vis(T)|`. `C_θ ≤ V` and is unknowable without reading
the column, so exactly two conditions discharge it from quantities already in hand:

```
fast path  ⟺  V ≤ min(k_min, cap)                (the floor alone covers the tile)
           ∨  ( P_d is Saturated ∧ V ≤ cap )     (θ ≥ 1 ⟹ C_θ = V by construction)
```

Both are **exact, not conservative** — confirmed in review. Limb A gives `m ≥ min(cap, k_min) ≥ V`;
limb B gives `C_θ = V` hence `m ≥ min(cap, V) = V`. `Saturated ⇒ C_θ = V` holds at the exact
boundary because `Saturated` admits every `u64`, which `Cut(u64::MAX)` would not.

**θ saturates early exactly where the probes say it matters.** `P_d` saturates at
`d ≥ log₄(V_total / m_target)`: depth ~2 for a viewer with 10² visible, ~7 at 10⁵, ~13 at 10⁹. So
for tail principals most of the zoom range serves every visible row with no selection pass at all
— the case `probes/results.md` measured as dominant. **The drawn-mark budget's framing that "at
k=10⁷ … V ≤ k across the board" does not survive the density rule**, because that rule exists to
draw fewer than *V*; the win arrives through θ saturation instead. Record it; do not quietly drop it.

**What the fast path saves.** Not the `tessera_id` read — `PointOut` carries `tessera_id`, so the
gather reads it per emitted row regardless. It saves the threshold-counting pass and the bounded
selection over rows that will not be emitted; when it fires there are none, so it saves the whole
selection step.

**Appendix C.** This is a per-tile branch keyed on the viewer's own `V` and θ, i.e. a **C4
widening** — the selection-route plan's own Task 3 owed that entry and this plan inherits the
obligation. Scan length correlates with the principal's own coverage, which C14's reasoning already
accepts as benign; the point is that it must be *written down*.

### The general route: one pass, bounded memory

`m(T) ≤ cap` always, so the `cap` smallest ids in the tile contain the served set for *any* `m` the
counting pass can produce. One pass suffices:

```
for row in mask.iter_range(range):
    id = columns.tessera_id()[row]
    if threshold.admits(id) { c_theta += 1 }
    push (id, row) onto a max-heap, evicting the largest when len > cap
m = min(cap, max(min(k_min, cap), c_theta)).min(V)
emit the m smallest of the heap, ascending by tessera_id
```

O(V) time, **O(cap) memory**. Review confirmed no off-by-one. Two guards:

- **`cap == 0` must early-return.** `crates/tessera-engine/benches/viewport.rs:177` calls
  `viewport(..., k = 0, ...)` as the count-only arm. Without an early return neither fast-path limb
  fires and every tile runs the full counting pass, so `tile_sweep_k0` silently stops measuring
  counting and the recorded baselines become incomparable with no test failing.
- **O(cap) memory stops being a virtue if `max_k` is later calibrated upward** — at 10⁷ the heap is
  120 MB per tile. `k_max_marks` bounds it today; note the dependency.

**Pre-existing cost, not introduced here:** `EffectiveMask::iter_range` materialises `to_vec()`
(`compose.rs:99`), so it already allocates O(V) `u32`s per tile, and today's placeholder pays that
too. Not changed and not fixed here — noted so it is neither mistaken for a regression nor for the
binding allocation.

### Point ordering, and the per-tile boundary

**Points are emitted ascending by `tessera_id` within each tile, on both routes** — memo §5/§6
clause 4 needs a client's own truncation to be truncating a prefix, and a route-dependent payload
order would be a differential-oracle landmine. In the fast path this sorts at most `cap` items.

**The tiles batch gains `served: uint64`.** This was a blocker in review r1. Today the points
stream is a flat undelimited concatenation and every reader recovers the per-tile grouping
arithmetically as `min(k, visible)` — `reference/tests/test_differential.py:134`, and the contiguous
per-tile append at `viewport.rs:315`. Under the new rule the per-tile count is
`min(min(cap, max(k_min, C_θ)), V)`, and `C_θ` is derivable from nothing the response carries.
Without `served`: the differential's splitter cannot be written; memo §5's Profile-B "draws the
first *K*B marks **per tile**" is unimplementable client-side, which retires clauses 4–5 of the very
proof this plan carries. `served` is a masked-derived quantity — no disclosure beyond §7.1, which
already gives the viewer exact per-tile masked counts.

### §3.3 — the underlay sub-cell counts

Exact masked counts at depth `d + s`, each a `count_range` over a contiguous Morton range.

**Why it is an I2 no-op, stated with the argument that makes it one:** a depth-`d+s` sub-cell count
is exactly what a `zoom = d+s` viewport request already returns (§7.1: the exact count of visible
items in any tile at any zoom). The underlay saves round-trips and discloses no new quantity.
Omitting empty sub-cells conveys `count == 0`, itself a masked count, exactly as the existing tile
skip at `viewport.rs:303-306` does. Cross-zoom and cross-pan differencing yields only differences
of masked counts.

**Opt-in per request.** `underlay_offset: Option<u8>`; absent or `Some(0)` → none served. The
visualisation client does not exist yet (deferred, backend first), so always-on would inflate every
response for no reader.

**Three bounds, all required:**
- `max_underlay_offset` config (default 4), and `d + s ≤ 16`.
- **A total sub-cell cap per response.** `tiles_for_bbox` (`crates/tessera-spatial/src/morton.rs:180-187`)
  has no cap, and this multiplies it by `4^s`. At `s = 4` over ~300 tiles that is ~77k
  `count_range` calls — against probes' 0.1–0.3 ms for ~300 whole-viewport calls, ~25–75 ms,
  versus plan §5's "p99 viewport latency under 10 ms" exit criterion. Cap the total and record the
  latency figure.
- **Echo the effective offset in the response.** A Morton prefix does not encode its depth
  (contracts §2.5), and the existing tiles batch is interpretable only because the client supplied
  `zoom`. Clamping `d + s ≤ 16` silently while *rejecting* an out-of-config offset is two rules for
  two bounds; pick one and echo what was used.

**Wire: additive, no `API_VERSION` bump.** Review established that nothing in the tree is versioned
off `API_VERSION` beyond the field `/v1/meta` echoes (`crates/tessera-types/src/lib.rs:55`,
`viewer.rs:79`), there is no `x-tessera-api` validation, and no test asserts version 1. The break
would be caused purely by *inserting* a length prefix. Appending instead — `u32 tile_len ‖ tile ‖
points ‖ subcells` — is backward-compatible for any reader that stops at Arrow's end-of-stream
marker, which both `pyarrow.ipc.open_stream` (`reference/oracle/wire.py:40`) and arrow-rs's
`StreamReader` do; "zero trailing bytes" is exactly "not requested". Contracts §0.3 deviation 5 is
the standing precedent for recording a shape change without a bump where there is no published
reader. **Specify explicitly that "not requested" means zero trailing bytes, not a schema-only
stream** — the two decode differently.

**What *does* need a contract revision is `k`'s meaning**, which goes from "at most `k` points per
tile" to "the cap clause of a density rule". Contracts §1: removals and semantic changes bump.

**The fade-out rule stays undesigned and stays the client's.** Memo §3.3 flags that at deep zoom
sub-cell counts quantise against Roaring container granularity and the underlay degenerates. The
server serves exact counts; recorded as an open item.

### `GET /v1/meta` gains the selection constants

`k_min`, `k_max_marks` and `theta_target_marks` are viewer-independent constants and disclose
nothing. Two reasons to expose them, the first not merely test convenience: a client **cannot
interpret mark count as density without knowing where the floor and cap sit**, and the reference
oracle cannot reproduce the definition without them (review finding 11 — the r1 plan told the
oracle to compute `C_θ` while giving it no way to learn `m_target` or `k_min`). Additive, so no
version bump.

---

## File structure

**Task 1 — the selection definition (engine)**
- Create: `crates/tessera-engine/src/select.rs`
- Modify: `crates/tessera-engine/src/lib.rs` (declare module),
  `crates/tessera-engine/src/session.rs` (`EngineConfig`: `k_min`, `k_max_marks`,
  `theta_target_marks`, `max_underlay_offset`, `max_underlay_cells`),
  `crates/tessera-engine/src/viewport.rs` (delete `sample_tile`; `TileCount.served`)
- Modify (behavioural rewrites, **not** just config literals):
  `crates/tessera-engine/tests/viewport.rs:253` (`points.len() == 5`, "k=5 caps sampled points"),
  `:292`, `:428-435` (exact `tessera_id`s **in first-k order**),
  `crates/tessera-server/tests/http.rs:419`
- Modify (config literals only): `crates/tessera-engine/benches/viewport.rs` (+ the `cap == 0`
  guard), `crates/tessera-engine/examples/open_rss.rs`
- Create: `crates/tessera-engine/tests/selection.rs`

**Task 2 — the `V ≤ k` fast path**
- Modify: `crates/tessera-engine/src/select.rs`, `crates/tessera-engine/tests/selection.rs`

**Task 3 — §3.3 underlay, `served`, and the wire**
- Modify: `crates/tessera-engine/src/viewport.rs` (`SubCellCount`, `ViewportOut.sub_cells`),
  `crates/tessera-wire/src/payload.rs` (`served` column; appended third stream),
  `crates/tessera-server/src/viewer.rs` (`underlay_offset`; `/v1/meta` constants),
  `crates/tessera-server/src/config.rs` (new `[serve]` keys)
- Modify (tests): `crates/tessera-wire/tests/wire.rs`, `crates/tessera-server/tests/http.rs`

**Task 4 — reference oracle and conformance**
- Modify: `reference/oracle/viewport.py`, `reference/oracle/morton.py`, `reference/oracle/wire.py`,
  `reference/oracle/harness.py`, `reference/tests/test_differential.py`,
  `conformance/tests/test_canary.py`

**Task 5 — the design corpus**
- Modify: `docs/design/architecture.md` (§7.2, §7.3, §12.3, Appendix A, Appendix C,
  Appendix G r22), `docs/design/contracts.md` (§2.6 note, §3, §0.3, revision block r7)

---

## Task 1 — the selection definition

**Interfaces produced:**
- `Threshold::{Cut(u64), Saturated}`; `Threshold::anchor(v_total: u64, m_target: u64) -> Self`;
  `Threshold::at_depth(&self, depth: u8) -> Self`; `Threshold::admits(&self, id: u64) -> bool`
- `served_count(c_theta: u64, k_min: usize, cap: usize, visible: u64) -> usize`
- `select_tile(mask, segment, range, threshold, k_min, cap) -> Vec<u32>` — rows, ascending by
  `tessera_id`

- [ ] **Step 1: failing tests for the arithmetic.** `P_{d+1} == 4·P_d` until saturation; **a `P_0`
      one bit below the boundary at successive depths** (the `checked_shl` trap); saturation is
      sticky; `V_total ≤ m_target` and `V_total == 0` both anchor `Saturated`; `served_count` obeys
      floor, cap, the `k_min > cap` clamp, and never exceeds `visible`; `cap == 0` yields 0.
- [ ] **Step 2: the discriminating regression test — this is the test that proves the fix.** In
      `crates/tessera-engine/tests/selection.rs`, build a fixture whose tile spans **at least two
      leaf Morton cells**, with the lowest `tessera_id`s in the *second* cell. Within one leaf cell
      storage order already *is* `tessera_id` order, so first-*k* and bottom-*k* agree there — the
      defect is only visible across cells. Assert the served set is the bottom-*m* by `tessera_id`
      and **not** the first *m* in row order, and assert the two differ for this fixture (a fixture
      where they coincide silently passes and proves nothing).
- [ ] **Step 3: run both, confirm red.**
- [ ] **Step 4: implement `select.rs`** — the one-pass bounded-heap route, the `leading_zeros`
      overflow test, the `u128` `P_0`, the `cap == 0` early return, and the no-duplicate-id
      `debug_assert`.
- [ ] **Step 5: wire into `viewport.rs`.** Delete `sample_tile` **and its doc comment**, which
      still quotes the retired `priority(e) = splitmix64(e) >> 48` and the "deliberately wrong"
      framing; update the module doc, which advertises the placeholder. Compute the composed anchor
      once per request, then `at_depth(zoom)`. Populate `TileCount.served`.
- [ ] **Step 6: config.** `k_min` (2), `k_max_marks` (128), `theta_target_marks` (16).
      **`max_k` keeps its value of 200** — the drawn-mark plan's Task 7 owns that number and the
      identity plan blocks it until its Task 15. Note in the coordination section that this plan
      changes what `max_k` *means* even though it changes no digit.
- [ ] **Step 7: behavioural tests.** Floor (sparse tile draws `min(k_min, cap)` — the I7
      guarantee); cap (dense tile draws exactly `cap`); **threshold over a *clustered* fixture,
      asserting the mark count is not pinned at the cap** (the r1 test as worded could only fail
      that case, never diagnose it); nesting over a randomised fixture; θ depth-stability; **θ does
      not move on pan**; `anchor == 0` with a non-empty composed mask.
- [ ] **Step 8: gates and commit.**

## Task 2 — the `V ≤ k` fast path

- [ ] **Step 1: failing differential test.** Where the fast path fires, the served set must equal
      what the general route returns for the same `(tile, principal)` — compared against a
      test-only forced-general-route entry point, so the two implementations are actually compared
      rather than inspected.
- [ ] **Step 2:** each limb fires (`V ≤ min(k_min, cap)`; `Saturated ∧ V ≤ cap`), and
      `Saturated ∧ V > cap` does **not**.
- [ ] **Step 3: confirm red; implement; confirm green.**
- [ ] **Step 4: gates and commit.**

## Task 3 — underlay, `served`, and the wire

- [ ] **Step 1: failing tests.** Sub-cell counts sum to the parent tile's `visible`; a suppressed
      item decrements the containing sub-cell (**I2 — masked, not raw**); `d + s ≤ 16`; `None` and
      `Some(0)` serve none; offset capped by config; **total sub-cell cap enforced**; effective
      offset echoed; `served` matches the emitted point count per tile.
- [ ] **Step 2: engine** — `SubCellCount { cell: u64, count: u64 }`, skip empties.
- [ ] **Step 3: wire** — `served` on the tiles batch; the sub-cell stream **appended**, framing
      unchanged, "not requested" = zero trailing bytes. Round-trip test. **No `API_VERSION` bump.**
- [ ] **Step 4: server** — `underlay_offset` on the request; `/v1/meta` exposes `k_min`,
      `k_max_marks`, `theta_target_marks`; new `[serve]` keys; out-of-range offset handled by the
      single chosen rule, matching the existing `zoom > 16` style.
- [ ] **Step 5: gates and commit.**

## Task 4 — reference oracle and conformance

- [ ] **Step 1:** replace `first_k` with `served`, implementing the definition **independently** —
      sort the tile's visible rows by stored `tessera_id`, count below the cut, take
      `min(cap, max(k_min, C_θ))`. It must not import the Rust arithmetic's shape or the
      differential proves nothing. Read the constants from `/v1/meta`.
- [ ] **Step 2:** delete `reference/oracle/morton.py`'s `priority()` — the retired unkeyed
      `splitmix64(e) >> 48`, the last live trace of the pre-fold definition and on the priority
      memo's own change list. `identity.py:227` mentions it only in prose.
- [ ] **Step 3:** delete the `PHASE2-TODO` block in `viewport.py`; its own text says leaving it
      unresolved past this landing "is a bug, not a style note".
- [ ] **Step 4:** `wire.py` decodes the appended third stream; the differential's splitter uses
      `served`.
- [ ] **Step 5: the canary's comparison strength.** `conformance/tests/test_canary.py:52` rests on
      "K = 500 is comfortably >= N_BASE_ITEMS (400), so no tile's point set is ever truncated" —
      void under the density rule: with 400 items and `m_target = 16`, θ saturates only at `d ≥ 3`,
      so zooms 0–2 truncate. Conformance §4.2 requires canonicalised comparison "explicitly
      including the points batches". The fixture must force `Saturated` (or raise `k_min`) to keep
      the comparator at full strength — otherwise that surface silently degrades from
      full-membership to truncated-prefix with no test failing.
- [ ] **Step 6:** run the differential and the conformance suite; commit.

## Task 5 — the design corpus

- [ ] **Step 1: §7.2** — the new definition; memo §6's proof; the cap-outermost form and why; the
      closed-form θ anchor **with the occupancy-deficit approximation and the worked numbers**; the
      composed-anchor I2 argument; the full-`tessera_id` comparator, the lost premise for "no
      fall-through in Phase 1", and the **4× page-traffic cost** with the `w ≈ log₂(V_max/k)`
      trigger; **owner decision 4** — that fewer marks for sparse principals is intent, because
      constant *k* hides density; and the corrected multi-segment rule. §7.2's closing line
      ("allocate *k* across segments in proportion to visible count") is **wrong** for a prefix
      definition — proportional allocation is not the bottom-*m* of the union. Replace with "sum
      `C_θ` across segments, then serve the global bottom-*m* of the union".
- [ ] **Step 2: §7.3** — strike the `k`-by-count lever. **Unsound**, not merely imprecise: it
      inverts `k(child) ≥ k(parent)` and reintroduces the popping failure §7.2 records the
      bit-reversal design having. Replace with the threshold clause and the underlay.
- [ ] **Step 3: §12.3** — record that θ's anchor must be the session-global visible total across
      partitions. Review established that with a *global* θ, per-partition
      `min(cap, max(k_min, C_p))` plus a coordinator global-bottom-*m* is exact in all three
      regimes; it is a *partition-local anchor* that would break composition. Unreachable in
      Phase 1 (one partition), but this plan amends §7.2 and must not leave §12.3 stale.
- [ ] **Step 4: Appendix C** — two rows. (a) Mark count now tracks masked visible count more
      directly, and sub-cell counts disclose it at finer grain; both derive from the exact masked
      count already disclosed by §7.1, **and a depth-`d+s` sub-cell count is exactly what a
      `zoom = d+s` request already returns** — that sentence is what makes it a no-op rather than
      an assertion. Record that truncation option (b) of memo §7 is *not* implemented and would
      *not* be a no-op. (b) The **C4 widening** from the per-tile fast-path branch.
- [ ] **Step 5: Appendix A** — the underlay's per-request sub-cell cost and measured latency; the
      residency correction from the 4× scanned-column rise.
- [ ] **Step 6: contracts r7** — §3's request/response shape (`served`, `underlay_offset`, the
      `/v1/meta` constants, the appended stream and its zero-byte "absent" encoding); **§3's `k`
      semantics**, which is the change that actually warrants the revision; a §0.3 deviation for
      the appended stream, citing deviation 5 as precedent; a §2.6 note that `priority` is now
      written and unread at query time, with the optimisation trigger.
- [ ] **Step 7: Appendix G r22** — the whole change, its provenance, the parameters' provisional
      standing, and the two review rounds.

---

## Coordination with in-flight plans

1. **`DEFAULT_MAX_K`'s value is not touched** (`config.rs:146`); the identity plan blocks it until
   its Task 15. But this plan **changes what that number means** — `max_k` becomes the machine
   ceiling only, with `k_max_marks` carrying the overplot ceiling. Drawn-mark Task 7 must see this
   before calibrating.
2. **`morton` is already `u32` in this worktree** (`read.rs:499`, `:551`, `tile_ranges` widening at
   `:939-945`), so this plan's silence on the narrowing is correct. `SubCellCount { cell: u64 }`
   matches contracts §2.5's wire prefix type.
3. **The selection-route plan** is superseded in part: its Task 1 lands here; its Tasks 2+ do not.
   Its Task 3 Appendix C obligation is discharged here.
4. **Neither the drawn-mark plan's files nor the identity plan's are edited here** except
   `docs/design/contracts.md`. **If either is in flight on that file, STOP and report**
   rather than merging revision blocks by hand.

## What this plan deliberately does not do

- **No CL/SS route chooser, no cost model, no priority index.** Only `V ≤ k` crosses over.
- **No §3.2 stratum tags**, no 2-bit tag on the wire.
- **No change to `max_k`'s value.**
- **No prefix-scan-then-fall-through comparator** — design §7.2 (r21) forbids it in Phase 1, and
  the 4× cost of not having it is recorded instead.
- **No probe work.** P1–P4 belong to the drawn-mark budget plan.
- **No parameter tuning.** `k_min = 2`, `m_target = 16`, `k_max_marks = 128`: all provisional
  pending memo §0's visual experiments, and all recorded as such.
- **No θ measurement mechanism** — closed form only, per owner decision 2.

## Residuals accepted, recorded not hidden

- **The mid-zoom cap-flat band** on clustered corpora (worked above). Accepted by the owner;
  memo §9 already accepts cap-flat regions; backstopped by the §3.3 underlay.
- **4× rise in per-viewport scanned column bytes** (`priority` 2 B/row → `tessera_id` 8 B/row).
- **θ moves on an overlay swap**, not on a pan. Prefix structure limits the perturbation to marks
  near the cut.
- **`iter_range`'s O(V) `to_vec()`** — pre-existing, unchanged, not the binding allocation.
- **Sparse principals draw fewer marks than today.** Owner decision 4: intent, not regression.
