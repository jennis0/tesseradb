# 0041 — A pin becomes a staleness stamp, not retained geometry

**Date:** 2026-08-03 · **Status:** Settled

## Context

A pin was a `(prefix, segments_version)` pair returned on every viewport response and presented on
the next request. Presenting one caused that request to be answered against **that** geometry rather
than the live one, which obliged the server to keep superseded generations resolvable: a drain list
of up to `drain_depth_max` entries, each holding an `Arc<Bundle>` and therefore its memory-mapped
segment files, for up to `pin_ttl_secs`.

That was affordable while geometry moved rarely. **Flush makes it move every tick.**

The mechanism was imported with a justification that does not transfer. §10.4 named it *"the
standard session-pinning pattern from mature search engines"*, and in a search engine session
pinning has one canonical purpose: **pagination** — pages 2..n must come from the searcher that
produced page 1, or results shift, duplicate or vanish between pages. This API has no pagination. A
viewport request takes a bbox and a zoom and returns every covered tile's counts plus the selected
marks in one self-contained response.

Nor does a client hold any identifier that needs old geometry to resolve. A tile is a Morton prefix
and a depth, resolved against any segment's own sorted codes by binary search; under
[decision 0040](0040-quantisation-is-slice-scoped-index-config.md) that prefix is a *permanently*
stable address. An item is a `tessera_id`, invertible to `(shard, entity)` independently of geometry,
with the row looked up last. So a re-issued request re-locates everything it needs.

## Decision

**The round trip is kept and its meaning is changed. A pin becomes an advisory staleness stamp.**

- The response carries the generation stamp it was answered from, as before, in `x-tessera-pin`.
- The request may carry one back. It **never selects geometry, never expires, and never produces an
  error**. The server compares it against the live generation and reports whether anything moved, in
  `x-tessera-stale`.
- Nothing is retained on its behalf. A superseded generation is held by the requests already in
  flight against it, through the `Arc` each loaded at its start, and is freed when the last completes.

**Deleted:** `PinManager`, `PinnedGeometry`, `DrainEntry`, `Reclaimed`, `PinStats`, the reclaim,
retire and drained-resolve paths; `pin_ttl_secs`, `pins_per_session_max`, `drain_depth_max`,
`DRAIN_DEPTH_ALARM`, and the startup relation `pin_ttl_secs < drain_depth_max × flush_max_age_secs`;
`EngineError::PinExpired` (→ 410 `pin-expired`) and `EngineError::PinCapExceeded` (→ 422), with their
server mappings and the code's row in contracts §3.1's closed list.

**Kept:** I11's **within-request** rule, unchanged and free — a request resolves its generation once
and uses it for tile ranges, columns, row space and mask alike. `check_publishable`, whose
justification changes without weakening: from *"outstanding pins would answer against new geometry"*
to *"the row-projection cache keys on `segments_version`, so a non-increasing version serves a
projection built against one row space to a request answered from another"*.

The full argument, its three reviews and their disposition are in
[`docs/design/geometry-pinning.md`](../design/geometry-pinning.md), now normative.

## Consequences

**What this buys.** ~94 GB of mapped files stop being retained at lifecycle §2.2's own depth-2
sizing, against a *measured* 47.02 GB live bundle. ~2,000 lines across six crates and three
languages, an error code, three config keys and a startup relation go with them.

**What it does not buy, stated because an earlier draft claimed it.** It does **not** free
`flush_max_age_secs`. The 75 s floor the startup relation imposed is gone, but the binding cost is
the row-projection rebuild — a *measured* 10.7 s at 10⁹ per session — which only the projection
patch removes, and that patch is needed whatever happens to pins.

**I11 loses its test coverage, and this is a regression rather than a reclassification.**
Conformance §4.4 tested I11 *through the pin*, calling the pin "the presentable proxy" precisely
because no client can present a row-space artefact directly. With the proxy gone, the surviving
within-request rule has no black-box surface at all — a suite cannot observe how many times a handler
loaded a pointer. I11 moves from covered to **uncovered** in the conformance inventory, with the two
routes to re-covering it named there.

**Prefix retention loses its operand.** Lifecycle §2.2 made a local prefix copy deletable only after
`marker + session-pin TTL`. The replacement condition is "no in-flight request holds it", which is an
`Arc` strong count rather than a clock — and nothing can observe a strong count reaching zero, so the
mechanism is a registry of `Weak` handles plus a poller. Roughly 100 of the deleted lines come back
the day prefix deletion lands. Scoped to prefixes (compactions) rather than to every flush.

**The leak register is corrected as well as re-scoped.** C15's mitigation column claimed *"pin values
are per-session scrambled so no cross-session correlation"*; that was **false in code** — the stamp
is a plaintext `(prefix, segments_version)`, identical for every principal, and always was. Under the
owner's ruling of 2026-08-02 that **knowing data has been ingested is not a security leak**, the row
is accepted without mitigation and now says what is true. That ruling is also what lets the staleness
signal ship in its **broadcast** form; `client-interaction.md` §6.1's argument for per-session
scoping survives as an efficiency argument, not a security one.

## Alternatives

**Keep it.** Rejected: the retention is paid on every tick for a capability nothing exercises.

**A frame-window retention** — prefix-scoped, depth 1–2, seconds rather than 300 s. The reviewer's
fallback, and the honest middle path: it keeps most of the stability benefit at a fraction of the
cost and blast radius. Put to the owner alongside full deletion; full deletion was chosen. **If
`geometry-pinning.md` §11's objections turn out to bite, this is the thing to reintroduce**, and it
is cheaper than what was removed.
