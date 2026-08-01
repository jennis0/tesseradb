# Design memo — conveying density without breaking the nesting guarantee

**Date:** 2026-07-29
**Status:** **direction of travel, not a specification.** Affects §7.2, §7.3, Appendix A,
Appendix C, the contracts spec (candidate-list width) and the conformance suite — but none
of those should be amended yet. See §0.
**Origin:** owner observation during white-paper review, worked up with an independent
analysis pass.

---

## 0. What this memo is for, and when to act on it

**The approach recorded here is the intended one; the parameters in it are not yet
decisions.** Every number that matters — *k*min, *K*max, *K*B, the stratum ratios, the θ
anchor — is chosen against a perceptual argument that nobody has tested. The binding
constraint identified in §4 is overplot legibility, which is precisely the kind of claim
that cannot be settled by reasoning.

So: **do not amend the design documents on the strength of this memo, and do not tune these
parameters before there is something to look at.** The work it implies is deferred until
enough infrastructure exists to render a real masked viewport over a real corpus — at
minimum the query path, the permutation, per-tile masked counts and a client that can draw
marks.

At that point, run visual experiments and settle:

- Whether mark count reads as density at all, and over what range, under real overplot at
  80×80 px per tile.
- Where mark count stops being legible — the §4 estimate of 50–100 marks per tile is a
  guess, and *K*max follows directly from wherever it actually lands.
- Whether the strata genuinely add perceived range above the cap, or whether heavy marks
  fuse into the light-mark mush sooner than §4 claims. This is the assumption the 3–3.5
  decade figure rests on, and it is the one most likely to be wrong.
- How the luminance ladder survives H.264 4:2:0 on Profile B, and whether *K*B = 32 is the
  right truncation.
- Where the underlay should fade out at deep zoom.

**What should not wait for the experiments** is the structural decision, because it is
expensive to retrofit and cheap to adopt now: the selection rule is a floor ∪ threshold ∪
cap over a fixed-hash priority, evaluated inside the mask, with θ per depth and anchored
per session. Everything in §6 — the nesting proof — holds for any parameter values, so
adopting the *shape* early costs nothing and keeps the door open. The candidate-list width
in §7 is the one structural choice with a real price attached, and it is the one thing here
that may need deciding before the experiments if the bundle format is being frozen.

---

## 1. The problem

§7.2 defines a tile's sample as the *k* lowest-priority items in that tile inside the
governing mask. All tiles at a given depth cover equal screen area, so **any tile holding
at least *k* visible items draws exactly *k* marks**. Across a region where every tile is
saturated the rendered marks are uniformly distributed regardless of the underlying data
density: a tile holding twelve visible items and one holding four million render
identically.

§7.3 already names this failure and answers it in a single clause — "drive a density
underlay from the count pyramid, or modulate mark alpha and *k* by count". That clause is
carrying more weight than it can bear, and one of the two levers it names is unsound.

**The information is not lost.** Exact per-tile masked counts are free (§7.1) and measured
at 0.1–0.3 ms for a whole viewport at every zoom depth. The question is purely how the
drawn marks convey them.

### Why the obvious repair fails

Modulating *k* by count **breaks the nesting guarantee**. §7.2's stability property is that
an item among a parent tile's *k* lowest-priority visible items is necessarily among the
child's *k* lowest, because the child's visible set is a subset. That argument requires
*k*(child) ≥ *k*(parent). A child holds roughly a quarter of its parent's count, so under
*k* ∝ count we get *k*(child) < *k*(parent) and representatives drawn at one zoom level
vanish on zoom-in — reintroducing exactly the popping failure §7.2 records the
bit-reversal design having, by a different route.

Modulating by count *per unit screen area* rather than raw count is better — on descent a
child has a quarter the area and roughly a quarter the count, so density and therefore *k*
are unchanged where the distribution is locally uniform — but it still breaks nesting
wherever a child is genuinely sparser than its parent.

---

## 2. The reframing that resolves it

§7.2's rule is a **bottom-*k* sketch**, and a bottom-*k* sketch is fixed-size by
construction, so its size cannot carry information. The same fixed-hash priority machinery
also supports a **threshold (Bernoulli) sketch** — "every visible item with priority below
θ" — whose size *is* proportional to the visible count, and which nests for free because
the threshold is a constant rather than a rank.

Priorities are uniform hashes of the entity ID, so P(priority < θ) = θ and the expected
threshold-sample size in a tile is **θ·n**, where *n* is the tile's visible count. That is
the density signal drawn directly, rather than recovered presentationally.

---

## 3. What is adopted

Three mechanisms, **all shipping from the start**. They are complementary by construction:
the selection change carries density across the range where the eye actually counts marks,
the strata extend that past the cap, and the underlay carries the decades beyond any mark
scheme.

### 3.1 Selection — floor ∪ threshold ∪ cap

Replace §7.2's definition with:

> A tile's sample is the union of (a) the *k*min lowest-priority visible items in that
> tile, and (b) every visible item in that tile whose priority is below θ_d, capped at the
> *K*max lowest-priority such items — where θ_d is the per-depth threshold and both
> quantities are evaluated inside the governing mask.

The floor clause **is** today's rule at a smaller *k*, so the sparse-principal protection
that I7 exists to provide cannot regress. The threshold clause supplies the density signal.
The cap bounds work and wire.

θ is anchored **once per session** from the viewer's own total visible count — a mask-only
quantity — and progresses per depth as θ_{d+1} = 4·θ_d. The ×4 is what makes the per-tile
expectation depth-stable: a child holds ~n/4 items, so 4θ_d · n/4 = θ_d · n.

**θ must be per-depth and session-anchored, never recomputed per viewport.** A
viewport-recomputed θ falls on pan and sheds marks, which is the churn the whole priority
scheme exists to avoid.

### 3.2 Presentation — stratified weight

Fix four per-depth thresholds θ_d/64, θ_d/16, θ_d/4, θ_d, each scaling ×4 per depth, and
render mark weight by stratum: lowest-priority items heaviest — largest and brightest, a
**luminance ladder rather than a hue ladder**, which is what Profile B's H.264 4:2:0 chroma
subsampling requires.

This is a presentational classification of an unchanged selection: no selection change, no
storage cost, no wire cost beyond a 2-bit stratum tag per mark.

**Ship the tag, not the priority.** Priority is a hash of the entity ID; shipping even a
truncation of it opens surface adjacent to I10 for no benefit. Tag server-side, or derive
the stratum from per-tile boundary indices.

Below the cap, strata only redistribute — total mark count is already count-proportional
there. **Above the cap they genuinely add range**: when θ·n > *K*max the total is pinned,
but the served prefix still contains every visible item below θ_d/64 for as long as
θ·n/64 < *K*max, so the heavy-stratum count stays exactly count-proportional up to 64×
the cap density.

### 3.3 Presentation — log-ramped density underlay

Render a continuous shaded field beneath the marks, built from exact masked counts at depth
d+3 or d+4 (64–256 sub-cells per screen tile), each one a `range_cardinality` over a
contiguous Morton range. Map count to colour through an **explicit log transfer function**.

This is why additive mark alpha saturates and this does not: alpha accumulation is an
implicit *linear* transfer, and the quantity spans 6.6 decades.

Build cost zero. Query cost: the sub-cells tile the same row span the whole-viewport count
already walks, so cost is O(containers touched) once plus one boundary rank per sub-cell.

**Fade-out rule required, not yet designed.** At deep zoom, where tiles hold few rows, the
sub-cell counts quantise against Roaring container granularity and the underlay degenerates.
It must fade in favour of the marks themselves. This is a presentation-spec gap.

---

## 4. Parameters

| Parameter | Value | Rationale |
|---|---|---|
| *k*min | 2 | Screen-level floor ≈ 600 marks over ~300 occupied tiles; pairs give local shape; legible on Profile B |
| *K*max | 128 | Overplot-bound, not machine-bound. ~38k worst-case marks, ~0.8 MB wire |
| *K*B (Profile B, client-side) | 32 | Prefix truncation; payload unchanged |
| θ anchoring | Per session, chosen so the median occupied tile at entry depth draws ~16 marks, derived from the viewer's total visible count | Mask-only quantity; no pan churn; window centred on the viewer's own density scale |
| Per-depth progression | θ_{d+1} = 4·θ_d, and likewise every stratum threshold | Depth-stable expectation; monotone, which is what nesting needs |
| Strata | θ_d/64, θ_d/16, θ_d/4, θ_d → four-step luminance and size ladder, 2-bit tag | Extends count-proportional encoding 64× past the cap at no storage cost |
| Tie-break | Full 64-bit hash, not the 16-bit priority | The served set must be a well-defined total-order prefix |

### What binds, and what does not

The achievable count window is *K*max / *k*min = **64, about 1.8 decades**. The binding
constraint is **overplot legibility, not machinery**: a 300-tile viewport gives roughly
80×80 px per tile, and 128 marks at 2–3 px is already 15–20% ink coverage. Mark count stops
being readable as density somewhere around 50–100 marks per tile.

Raising *K*max to 256 or dropping *k*min to 1 buys encoded range the viewer cannot decode.
They are not recommended. Storage, GPU and wire are all far from binding.

With strata, encoded count-proportional range reaches 64 × 64 = 4096. **Perceived** range
is the honest figure and is lower: roughly **3 to 3.5 decades from marks alone** — about
1.5 decades read from total mark count below saturation, and a further 1.5–2 read from
heavy-mark rate against light-mark mush above it, since heavy marks stay individually
countable long after light ones fuse. The underlay carries the remainder of the 6.6.

---

## 5. The two render profiles

The service is deliberately profile-unaware and serves identical payloads either way, so
*K*max cannot differ by profile server-side. This resolves through a structural property of
the new definition rather than a special case.

**The served set is always a priority prefix of the tile's visible set.** Both clauses are
prefixes — the floor is bottom-*k*min, the threshold∩cap clause is
bottom-min(*K*max, |{p < θ_d}|) — and a union of prefixes is a prefix. Intra-leaf order is
already by priority (§10.3) and candidate lists are stored priority-sorted, so the payload
already arrives per tile in priority order at zero extra bytes.

Therefore: **serve *K*max sized for Profile A; Profile B draws the first *K*B marks per tile
and discards the rest client-side.** The payload is identical, no profile difference reaches
the authorisation path, and the subset is deterministic per (tile, mask, depth) — no pan
churn, and nesting holds (clause 5 below).

Do not size *K*max for the weaker profile; that discards Profile A's range for no
invariant-side benefit. Profile B's count window narrows to *K*B/*k*min = 16 and recovers
range through the strata and the underlay, both of which are luminance-friendly by design.

---

## 6. Nesting proof

Order vis(T) — the visible set of tile T at depth d — by priority under the fixed total-order
tie-break. Define

- m(T) = max(*k*min, min(*K*max, |{i ∈ vis(T) : p_i < θ_d}|))
- served(T) = the bottom-m(T) prefix of vis(T)
- drawn(T) = the bottom-min(m(T), *K*B) prefix of vis(T)

Let T′ be the child of T containing item i, with i ∈ drawn(T).

1. **Rank monotonicity.** vis(T′) ⊆ vis(T), so rank_{T′}(i) ≤ rank_T(i).
2. **Floor clause.** If rank_T(i) ≤ *k*min then rank_{T′}(i) ≤ *k*min, so i ∈ served(T′).
3. **Threshold clause.** If p_i < θ_d and rank_T(i) ≤ *K*max, then p_i < θ_d ≤ θ_{d+1} (θ is
   monotone in depth by construction) and rank_{T′}(i) ≤ *K*max, so i falls in T′'s
   threshold∩cap clause.
4. **Prefix closure.** Each clause is a priority prefix of vis, so their union served(T) is a
   prefix. Hence "the client's bottom-*K*B of served(T)" equals "the bottom-min(m(T), *K*B)
   of vis(T)" — client truncation commutes with the definition.
5. **Client clause.** rank_T(i) ≤ *K*B ⇒ rank_{T′}(i) ≤ *K*B; with clauses 2–3 giving
   i ∈ served(T′), we get i ∈ drawn(T′), provided *K*B is non-decreasing in depth (it is
   constant).
6. **List-route clause.** A width-W candidate list admits exactly
   {i ∈ vis(T) : p_i < cutoff_W(T)}, where cutoff_W(T) is the W-th smallest *unmasked*
   priority in T — itself a priority prefix of vis(T). T′ ⊆ T implies
   cutoff_W(T′) ≥ cutoff_W(T), so list survival is monotone under descent. With exact
   fallback (§7 below) the route never changes the served set, only its cost.
7. **Strata.** stratum(i, d) compares a fixed p_i against thresholds rising ×4 per depth, so
   a mark's weight is non-decreasing on descent: marks darken and grow on zoom-in, never
   lighten and never vanish. Presence is untouched.

Every clause is monotone under subset plus per-depth-monotone constants.

**Masking.** Every rank and count is taken over vis(T); θ is anchored on a mask-only session
quantity; *k*min, *K*max, *K*B and the stratum ratios are viewer-independent constants. I7
holds by construction, as it does for today's rule.

---

## 7. The candidate-list storage position

This is the one genuine cost, and it should not be framed away.

**The framing "hold list width at c·k and simply draw fewer where less dense" is true only
at coverage 1.0, and it silently swaps which density is meant.**

A width-W list filtered at coverage *cov* yields ~W·cov survivors — equivalently it serves
the visible prefix up to cutoff_W(T). The definition demands a prefix of length
m(T) ≤ *K*max. So the list route satisfies the definition only where W·cov ≳ m(T). At
W = 128: full coverage serves the cap exactly; 50% coverage serves only 64; at the c = 4
effectiveness floor of 25%, only 32 — and the tiles demanding m near the cap at high
coverage are precisely the dense cores the lists exist to serve.

"Draws fewer where less dense" is already the design's behaviour with respect to the
viewer's **visible** density, m = θ·n — that is deliberate and is the whole point. What a
constant width adds is "draws fewer where **unmasked** density is high and coverage is
partial", which is a different and viewer-adverse condition. That is where the framing
breaks.

Two ways to hold width constant, neither free:

- **(a) Fall back to direct evaluation** whenever survivors < m(T). The definition stays
  exact and every proof holds — but the fallback fires at high-coverage dense tiles, which
  is exactly where the lists were introduced to remove the O(n) scan. Correctness preserved,
  performance argument broken.
- **(b) Truncate** — serve only the survivors. This remains a prefix, so nesting and
  determinism survive, but it is a **definitional change**: the drawn count then depends on
  an unmasked quantity, which is (i) a mild I7 regression — a partial-coverage viewer on a
  dense tile is under-served relative to the definition, the same failure mode as
  sample-then-filter in attenuated form — and (ii) a new Appendix C row, because the
  truncation point weakly discloses unmasked tile density through the mark count.
  **Do not ship (b).**

**The honest position.** Serving the exact definition at today's coverage floor needs
W = c·*K*max = 512, a 4× multiplier on a small number. Against Appendix A: levels 0–6 goes
**2.8 MB → ~11 MB**; levels 0–9 goes **179 MB → ~716 MB**.

For the shallow configuration the constant-width framing is unnecessary frugality — pay the
11 MB. If the deep configuration's ~716 MB matters, the honest compromise is W = 2·*K*max =
256: full cap down to 50% coverage, m ≤ 64 down to 25%, with fallback (a) below that — a
measured performance cost at a small set of tiles rather than a silent definitional one.

**Holding W = 128 does cap the list-served window, and therefore the achievable ratio on the
list route.** That is the finding, stated plainly.

---

## 8. Consequences for other documents

- **§7.2** — the definition changes. Both evaluation routes must compute the new definition
  identically; the definition is the contract between them.
- **§7.3** — replace the single modulation clause; the *k*-by-count lever named there is
  unsound and should be removed rather than qualified.
- **Appendix A** — candidate-list rows change under the chosen W.
- **Appendix C** — a new row. Mark count now tracks visible count more directly, and stratum
  counts disclose it at four rates rather than one. All are derived from the exact masked
  count, which is already a first-class disclosed product, so this should be a **no-op
  entry — but it must be written down rather than assumed**. Truncation option (b), if ever
  revisited, is *not* a no-op entry.
- **Contracts spec** — candidate-list width is a bundle-format field; the 2-bit stratum tag
  is a wire-format addition.
- **Conformance** — the differential oracle and the §7.2 canaries must encode the
  prefix-with-clauses definition, including the client-truncation commutation (clause 4),
  since both routes and both profiles must agree.

---

## 9. Open — all of it deferred to the visual experiments in §0

Everything below is a question for the experiments, not a blocker on adopting the shape.

- **Perceptual multiplicativity of strata × count is asserted, not measured.** The claim that
  heavy-mark rate stays readable against light-mark mush for a further ~1.5–2 decades needs
  an eyeball test under real overplot. If it fails, perceived range falls back toward 2
  decades and the underlay carries more.
- **Profile B's narrowed window (16×)** leans on strata and the underlay, neither tested
  under H.264 4:2:0 specifically.
- **Anchor versus intra-slice variance.** A viewer whose visible density spans much more than
  the combined window still sees floor-flat and cap-flat regions. Accepted, backstopped by
  the underlay. Per-region adaptation would reintroduce pan churn and is rejected.
- **The underlay's deep-zoom fade-out rule** is undesigned.
- **θ anchoring** — "median occupied tile at entry depth draws ~16 marks" needs to become a
  precise, testable rule before implementation.
