# Denies as a separate mask — `M_auth ∧ ¬denied`

**Status:** Design note, 2026-08-03. Evidence memo, never normative. Nothing here is adopted.
Adoption would touch `architecture.md` §11.2, `caching.md`, and `tessera-engine`'s `compose` and
executor; none of which this memo edits.

## Recommendation

**Factor the two session-independent deny rules out of per-request composition and into a row-space
bitmap carried on the `Generation`.**

```
denied[slice]  = { row_of(e) : e ∈ deleted ∪ suppressed }     built on the executor
effective      = compose(evaluate, buffer, satisfied, base) ∧ ¬denied[slice]
```

`deleted` and `suppressed` stay exactly as they are in the overlay — entity-space Roaring sets,
snapshotted to the WAL at rotation, the authoritative durable form. `denied[slice]` is **derived**
from them, never a second source of truth, and never persisted.

## Why: two of the four rules do not depend on the viewer

`compose`'s precedence is `deleted > suppressed > evaluate > buffered`. Split by what each one
consults:

| rule | consults `satisfied`? | same answer for every session? |
|---|---|---|
| `deleted` | no | **yes** |
| `suppressed` | no | **yes** |
| `evaluate` | yes | no |
| buffered | yes | no |

Today all four are recomputed **per viewport request, per session**. Two of them have one answer for
the whole deployment.

**The cost that makes this worth doing is not the redundancy, it is the growth.** `compose` walks
`overlay.touched()` — every entity any store has an opinion on — and does a `row_of` per entity.
Deletion denies never retire (the stamp ledger is ⊘) and predicate entries never retire (the fold is
⊘), so the set the walk covers grows monotonically with every deny the deployment ever accepts. The
per-request cost of composition is therefore **O(denies ever accepted)**, and nothing in the current
design bounds it. `overlay_soft_limit` alarms on the depth; it does not reduce it.

Under this change the per-request term becomes O(active predicate changes + buffer depth), and the
deny term is paid once per change to the deny set or the row space.

*(Modelled from the code's shape. Neither variant has been benchmarked — overlay depth in every
fixture is far below where either term could matter, and a measurement wants a deny-heavy corpus
that does not exist yet. See "What should be measured".)*

## What the latency ruling buys, which is most of the simplicity

**Owner ruling, 2026-08-03: suppression latency is acceptable at exactly the level delete's is —
architecture §3's seconds-to-minutes write-path budget. Lower is better, but higher latency is a
fair trade for better overall performance or greater simplicity.**

That is the whole reason this design is small rather than awkward. Without it, `denied[slice]`
would have to be a *cache*: keyed on `(slice, segments_version, overlay_version)`, invalidated on
every accepted deny, rebuilt lazily on whichever read path hit it first, with an eviction policy and
a single-flight guard so a burst of sessions did not rebuild it concurrently. That is the
row-projection cache's whole apparatus, acquired for a second artefact.

With the ruling, it is not a cache at all. It is **built on the executor thread and published with
the generation**, on the two events that can invalidate it:

- **a deny window is applied** — `Executor::apply_changes` already clones the overlay and publishes
  one new generation per window
- **geometry is published** — `publish_flush` / `publish_geometry`, where the row space changes

Both are already executor events that produce a new `Generation`. Nothing on the read path ever
builds it, so there is no cache, no key, no eviction, no single-flight, and no staleness question.

**The ack contract is untouched, and this is the rule the budget must not be read as relaxing.**
Architecture §3: *"a deny's acknowledgement stays coupled to its application — hold the 200 until the
entry is fsync'd and swapped, never acknowledge a deny that is not yet in force."* The build happens
before the swap, so the 200 still follows the application. What the budget permits is spending
longer **before** the ack; it never permits acking ahead of the effect.

## Three things it buys beyond the cost

**The `∩ base` clamp disappears for the deny half.** `minus` needs an explicit intersection with
`base` because subtracting a row `base` never held puts a spurious −1 in every count over that tile —
an I2 concern, since a count that does not describe `M_auth` is not merely a cosmetic bug. `andnot`
is self-clamping by construction. The evaluate/buffered diff still needs both clamps; the deny half
stops being able to get them wrong.

**Precedence stops being transcribable for denies.** The ordering is single-sourced in one function
today, with a comment recording that two transcriptions of it is how a suppression stops
suppressing. Applied unconditionally *after* everything else, a deny has no precedence to lose.
Same move as the three-store overlay (`6491d19`): turn a rule someone must remember into a structure
that cannot express the violation.

**`visible_to` gets simpler, not harder.** It is an entity-space question and stays one: the
entity-space sets are consulted directly, with no row translation at all.

## What it costs

- **The entity→row translation does not vanish; it moves.** Building `denied[slice]` is the same
  `row_of` per denied entity that `compose` performs today, paid once per deny window and once per
  publication instead of once per request. Under sustained deny pressure with no reads, this is
  strictly more work than today — which the latency ruling is what makes acceptable.
- **A suppressed *buffered* item has no row**, so it cannot appear in `denied[slice]`. It cannot
  appear in a viewport either — a buffered item has no row anywhere — so the mask is complete for
  what it governs. `visible_to` consults the entity-space sets and is unaffected. This is worth
  stating because it is the one place the two forms are not interchangeable.
- **Memory: one Roaring bitmap per slice per live generation.** Roaring costs O(containers touched),
  so a scattered deny set is roughly 2 bytes per denied row; 10⁶ denials over 10⁹ rows models to
  single-digit MB. *Modelled, not measured.*
- **It helps `evaluate` and buffered not at all.** Those stay per-session, per-request, bounded by
  active predicate changes and buffer depth rather than by total denies.

## It is not a spec change

I1 defines `M_auth = (fragment \ L) ∪ direct_eval(L)`, and §11.2 states that the definition is a
definition — *"any implementation whose answers agree with it conforms"* — and that the normative
evaluation order is already the inverse of it. Factoring the session-independent denies out of `L`
yields the same set. So this is an implementation reordering plus a §11.2 note, not an invariant
change and not an Appendix C entry.

**It also sidesteps I11's row-space hazard rather than inheriting it.** The rule is that a row-space
artefact carries the stamp it was built against, and that no row-space artefact may key on the
prefix — a merge permutes row space inside the merged span. `denied[slice]` is rebuilt on every
geometry publication and never outlives its generation, so it cannot be stale by construction. That
is a strictly stronger position than the row-projection cache, which *is* retained across
generations and needs `extends_to`'s boundary check to stay sound.

## What should be measured before this is adopted

1. **The per-request term today**, as a function of overlay depth — the claim that composition is
   O(denies ever accepted) is read from the code, not measured. A deny-heavy corpus does not exist
   in the fixtures.
2. **The write-path term**, since it is the one the latency ruling is spending: `row_of` per denied
   entity per deny window. The crossover is where deny rate approaches request rate, which should be
   nowhere near any real deployment but should be stated as a number rather than assumed.
3. **Whether `deleted` belongs in it at all.** Deletions are specified to retire against the stamp
   ledger and to be reflected in postings as delta-tier tombstones (lifecycle §3.1). Once that
   exists, a deleted entity is excluded by the fragment itself and does not need masking. Suppression
   is the one that structurally never can be — *"no fragment rebuild ever excludes a suppressed
   entity"*. So the long-run shape may be `¬suppressed` alone, with deletions folded. Building it as
   `deleted ∪ suppressed` today is right (neither retires), but the union should be a detail of the
   derivation and not something callers depend on.

## What this does not change

The overlay's three stores, their three retirement rules, the WAL snapshot at rotation, and the
entity-space durability story are all untouched. This memo is about where the *read path* gets its
answer from, not about what the answer is.
