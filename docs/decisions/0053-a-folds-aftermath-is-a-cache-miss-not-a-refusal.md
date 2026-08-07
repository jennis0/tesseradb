# 0053 — A fold's aftermath is a cache miss, not a refusal

**Date:** 2026-08-06 · **Status:** Settled (owner ruling)

## Context

`compaction.md` §6.2 sized the fold's flip as a **76 s–3 minute window in which `/v1/viewport` is
refused** for every session the post-swap refresh has not yet reached, and for every session
established during it. That was inherited from decision 0044's third arm — *"a bounded 429 residual
only for same-key racers during a merge's refresh window"* — and from its review finding F5, which
established that abandoning the shed gate replaces a bounded 429 with an unbounded inline-rebuild
herd.

F5 was measured against a **merge**, whose refresh pass is ~0.7 s. It was then inherited by a
**fold**, whose pass is 76 s–3 minutes, without anyone re-checking whether it still held.

## The decision

**A fold's publication does not arm the shed.** After the flip, a session whose projection is
missing takes an ordinary cache miss and rebuilds, exactly as a cold session does. No 429.

**And the rule that decides it, stated so the next publication kind does not have to re-litigate:**

> Shed only while the refresh pass is **shorter** than the rebuild it would save.

Flush and merge satisfy it — a ~0.7 s pass against a measured 4 550 ms rebuild, so shedding turns a
4.5 s inline build into a 1 s retry. A fold inverts it by two orders — a 180 s pass against a 10.7 s
build — so shedding refuses everyone for minutes to avoid a burst that would have cleared in
seconds. Flush and merge keep the gate unchanged.

## Why

**The total work is the same or less.** The refresh pass rebuilds every resident entry; the
miss-driven path rebuilds only entries someone actually asks for, so idle sessions never pay at all.
The shed does not save work — it moves who waits, and converts a delay into an error.

**The herd F5 named is already bounded three times over**, by machinery that predates this:
`ComputeGate` bounds requests inside the engine, `single_flight` stops two requests building the
same key, and `RowProjection::new` already fans out across the whole compute pool — so concurrent
rebuilds contend rather than multiply, and *N* of them cost roughly what doing them serially costs.
What the shed adds on top of those three is the refusal.

**0043 forbids exactly this.** *"It must not be observable in a viewer's latency **or in a viewer's
errors**."* A minutes-long population-wide refusal is the second of those, and the earlier text's
claim to satisfy 0044 *verbatim* was wrong: 0044's word is *only*, and it scopes to same-key racers
during a merge.

## What this retires

**`compaction.md` §6.3's retained-row-space migration.** It existed to remove a window this ruling
removes more cheaply. It was the largest structural change the document proposed — two live row
spaces, and a discipline across every row-space read path where a fail-open would hide — and the r5
review added two further obstacles (the row space is selected before the session's identity is
known; `KEEP_SUPERSEDED_GENERATIONS = 1` prunes the old entries ~90 s into a 3-minute window). It is
recorded as declined rather than deferred.

## What follows, and is not ruled here

**A staging projection list is licensed as an optimisation, not required.** Because a miss is now
merely a miss, precomputing the new row space's entries during the fold is best-effort: it reduces
how many sessions pay, and if it does nothing the ruling above still holds. Shape, if built —

- a **second `RowProjectionCache` with its own byte budget**, so warming cannot evict the entries
  still serving, which was r3's surviving objection to the pre-swap refresh (D2);
- populated **most-recently-used and filtered to recently-active sessions**, which is what bounds
  its memory — a fraction of the serving cache rather than a copy of it;
- **swapped in at the flip, with the old list dropped**, which makes stale-serve's unsoundness
  across a fold structural rather than a rule: there is no superseded entry for rung 2 to find;
- with a **coverage check at the swap** — see the trap below.

**The trap, recorded because it fails closed but wrong.** `session_geometry`'s rung 1 returns a
`Peek::Ready` entry *without* checking `extends_to`; an entry under the live key is assumed to be
over the live row space. A precomputed entry built before a flush landed would not cover the extents
carried forward at the flip, so serving it would answer an **incomplete mask** — items missing, no
error. The check belongs at the flip, on the executor, which is the same thread that publishes
flushes and therefore sees a fixed extent set: extend the short ones (a measured 44.6 ms) or drop
them to a miss.

**Not built without the invariants lens.** A precomputed mask for a session is where "I can't see a
disclosure" is not the same as "there isn't one" — the same caution `compaction.md` §13 records
against its own rows-frozen safety claim.

## What was considered and declined

- **Keep the shed and shorten the window.** Draining the projection cache before a fold shortens it
  proportionally (§6.3 already prices this). Declined as the primary answer: it reduces a refusal
  window rather than removing it, and the sessions it drops pay inline rebuilds anyway — which is
  the very outcome this ruling accepts, arrived at with extra machinery.
- **Retained-row-space migration.** Above.
- **Pre-swap refresh instead of post-swap** (decision 0044's D2 as originally ruled). Refuted at r3
  and still refuted; the staging list above is the *additive* form, which keeps the post-swap path
  and does not depend on the warm set being complete.
