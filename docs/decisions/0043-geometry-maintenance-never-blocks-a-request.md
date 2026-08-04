# 0043 — Flush, merge and compaction never block a request

**Date:** 2026-08-03 · **Status:** Settled (owner ruling) · **Mechanism: built 2026-08-04** —
decision [0044](0044-invisible-means-stale-serve-plus-background-refresh.md) resolved what
"never blocks" means and the mechanism landed with it (write-path §4.6)

## The rule

**Flush, merge and compaction may not block the request thread. Their effects must be invisible to
the viewer: built in the background, then made live by a pointer swap or an equivalent technique.**

"Invisible" is the operative word. It is not enough that maintenance runs off the write executor —
it must not be observable in a viewer's latency or in a viewer's errors.

## What it overturns

The corpus currently states the opposite, deliberately and in two places, and both are now wrong:

- `Engine::viewport`, at the row-projection cache: *"The patch, and why it is on the ordinary request
  path rather than in the publication … Derived here instead: the session that asks pays, once."*
  The reasoning it gives is sound about *cost*; the ruling says the cost may not be charged there.
- The superseded flush design's §3.3 rejected forcing rebuilds at publication because doing so
  *"synchronises the most expensive operation in the request path across the session population"*.
  The ruling agrees that is bad and forbids the alternative it settled on.

Neither is a small correction. Every geometry publication rotates `segments_version`, which is a
component of the row-projection cache key, so **every** publication makes every live session's next
request do work. Today that work is:

- **after a flush** — `RowProjection::extend`, a bitmap clone plus a projection over the new extents
  only. Small, but on the request thread;
- **after a merge** — a full rebuild, **measured 10.7 s at 10⁹**, on the request thread, because
  `RowSpace::collapsing` shortens the extent list and `RowProjection::extends_to` then refuses the
  patch. A concurrent request from the same session gets `EngineError::ProjectionBuilding`.

> **Correction, 2026-08-03, same day.** This entry as first written said `ProjectionBuilding` maps
> to a fail-closed 500 and that its 429 mapping was unbuilt. **That is wrong.** It maps to **429
> `backpressure` with `Retry-After`**, in `map_engine_error`'s explicit arm, pinned by
> `map_engine_error_takes_projection_building_to_backpressure`. The error came from trusting a ⊘
> marker on `EngineError::ProjectionBuilding` that had gone stale when the mapping was built — the
> exact failure decision 0013 exists to prevent, arriving from the other direction: not an absent
> marker for present machinery, but a present marker for machinery that had since arrived. The two
> stale markers are removed. Recorded here rather than edited away, because the reasoning below
> does not depend on it: a viewer waiting 10.7 s is what this rule forbids, and a retryable 429 is
> a better symptom of it than a 500, not an absent one.

So the ruling is violated by merge severely and by flush mildly, and the second reading matters: if
it binds flush too, it is a much larger change than merge alone.

## Why the rule, and why it is not merely a preference

A viewer cannot distinguish "the corpus is being maintained" from "the service is broken". A 10.7 s
viewport, or a 500 on the request after it, is a maintenance schedule leaking into the product —
and it leaks worst exactly when the deployment is busiest, because that is when maintenance runs.
The architecture already holds this line everywhere else: publication is a pointer swap over an
immutable generation, precisely so a reader never waits for a writer.

## What the rule does **not** settle

**It moves the work; it does not remove it.** A rebuild that no longer happens on the request thread
still happens, and the total is unchanged: O(live sessions × rebuild). At a 90 s flush cadence, an
eager per-session rebuild does not obviously fit, and §3.3's original objection survives the move.
**The mechanism is therefore open and is not specified here.** Candidates, none evaluated:

- **span-local re-projection** — re-project only the merged span's entities, O(span) rather than
  O(fragment), making the patch cheap enough to be invisible rather than absent. The cache's API
  cannot express it today (`extend` only appends);
- **serve the superseded generation** until a background-built projection for the new one is ready.
  Close to what pin retention did before decision 0041 deleted it, and `KEEP_SUPERSEDED_GENERATIONS`
  still retains depth 1 for the patch source. **Carries a fail-open risk that must be settled first:**
  the deny mask is row-space and per-generation, so serving older geometry with its older mask would
  drop a deny accepted since. Live deny state composed over older geometry may not be expressible;
- **eager background rebuild per live session** at publication — the honest reading of the ruling,
  and the one whose total cost §3.3 already argued against.

## Consequences

- **Merge must not be scheduled until the mechanism exists.** Tasks 20 and 21 are built; Task 22
  (publication) is what would make the cost reachable, and it is blocked on this.
- `EngineError::ProjectionBuilding`'s 429 is a user-visible symptom of what this rule forbids. It
  is the *right* answer to a build in flight; the rule's objection is that a viewer should not meet
  one at all.
- Compaction inherits the rule before it is designed, which is the cheapest moment to be told.
- Whether the rule binds **flush** as strictly as merge is the first thing the mechanism's design
  must answer, because it sets that design's size.

## Provenance

Owner ruling, 2026-08-03, given while reviewing merge's cost:
*"flush/merge/compaction CANNOT block the request thread. Any updates must be invisible to the user
(e.g. by building in background and doing a pointer swap or similar technique)."*
