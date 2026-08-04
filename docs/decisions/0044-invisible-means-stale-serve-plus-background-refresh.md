# 0044 — "Invisible" means stale-serve plus background refresh; merge splits, and publishes as its own swap

**Date:** 2026-08-04 · **Status:** Settled (owner ruling) · **Mechanism: built the same day** —
write-path §4.6 and §7; `crates/tessera-engine/src/{refresh,coalesce,merge}.rs`. D4's "measure
first" ran and **P2 refuted the model it rested on**; see the consequences below

## The rulings

Three, resolving decision [0043](0043-geometry-maintenance-never-blocks-a-request.md)'s open
mechanism and the merge review memo's D1–D3
(`docs/evidence/memos/2026-08-03-merge-design-review.md`).

**D1 — the budget.** The owner's constraint, recorded verbatim (2026-08-04):

> *"The thing we absolutely have to avoid is what happens on continuous streaming ingest — a
> visible ingest penalty that becomes almost unworkable. The client either has to pay only a
> small penalty (< 0.2 ms), none, or we need to ensure that longer (multisecond) delays are much
> rarer than the flush/merge rate."*

Under continuous ingest the flush tick fires forever, so any per-publication cost charged to a
request thread is a *steady-state* cost. The ruling therefore lands as:

- **Update-induced request-path work is zero in the steady state.** A request never builds a
  projection or a fragment because geometry moved; it is served from the freshest entry the
  background refresh has produced. **Stale-serve is sound for a flush**: a flush appends — no
  existing row moves — and the deny mask and overlay are composed live per request, so serving a
  one-refresh-stale projection or fragment is fail-closed staleness (the session sees the newly
  flushed items one refresh later) and never a deny miss.
- **Eager background refresh at each geometry publication**, over *resident cache keys* — one
  pool task per publication, O(cache residency) never O(sessions) (decision 0035's shape).
- **A bounded 429 residual only for same-key racers during a merge's refresh window.**
  Stale-serve is *not* sound across a merge (the merged span's row ids change meaning — I11), so
  a racer inside the bounded span-rebase window is shed with `Retry-After: 1`. Merges are much
  rarer than flushes, which is what satisfies the "much rarer than the flush/merge rate" arm.
- **Full builds happen only at session establishment** — not update-induced, outside 0043's
  scope.

**What this rules out, measured rather than argued:** the current inline append-patch. It clones
the session's projection bitmap — measured 125.12 MB for a wide grant at 10⁹ — which is tens of
milliseconds on the request thread per publication per session: two orders over the budget, in
exactly the continuous-ingest regime the ruling names. "Small penalty inline" is not an available
design point.

**D2 — merge splits into two publications.** The entity-space half — delta-tier, external-id-run
and dictionary-extent coalescence — is a content-preserving re-encode that touches no row space,
bumps no `segments_version` and rotates no cache key: conforming with 0043 by construction, and
it bounds the two axes with per-row steady-state cost (the O(runs) ingest duplicate-check scan,
the per-tier fragment-build probes). It lands first (Task 22a). The row-space segment merge
(Task 22b) is the only half that invalidates projections and is gated on D1's mechanism.

**D3 — a merge publishes as its own swap.** The one-cadence rule ("a completed merge rides the
next flush publication") lost its stated justification when pin retention was deleted
(decision [0041](0041-pins-become-a-staleness-stamp.md)), and under 0043 the coupling is
harmful: it makes the flush's zero-cost path carry the merge's refresh. The empty-buffer-tick
rider rule is deleted with it. Cost if wrong: one extra `segments_version` bump per merge — one
more background refresh round, nothing a viewer observes.

## The probes this was gated on — **run 2026-08-04**, `probes/2026-08-04-refresh-ladder/`

- **P1 — the projection ladder at 10⁹, 25% grant.** Rebuild 4 550 ms; the patch's bitmap **clone**
  40.9 ms; the union over one new extent 0.24 ms; a span rebase 44.6 ms. So D1's "two orders over
  the budget" holds as measured rather than inferred, and the reason is structural: the cached
  value is immutable (lifecycle §7), so a patch must **copy** before it unions, and no inline
  arrangement escapes the copy.
- **P2 — the fragment build against tier count: ~200 ms, and flat** (199 ms at 1 tier, 198 ms at
  512). **This refuted the model.** The corpus carried the build as "unmeasured, modelled seconds
  — the largest unpriced request-thread term"; it is a bounded term, three orders over the budget
  rather than four, and it does **not** grow with the tier count the entity-space coalesce bounds.
  By this ruling's own D4 the incremental fragment form therefore does **not** land: it would
  trade a 200 ms build for a ~41 ms clone, on work that had to move off the request thread anyway,
  by the same background refresh the projection needed.

One defect the probe found rather than review: `SegmentExtent::project` walked the mask from its
start, so projecting one flush extent cost O(grant cardinality) rather than O(extent span) — the
patch's cost was a function of the *grant's width* rather than of the flush's size.

## Consequences

- Task 22 splits (22a now, 22b gated); the superseded flush design's §1.3 one-cadence rule and its §1.2
  empty-buffer-tick rule are superseded (carried by `write-path.md`'s supersession map).
- The write path's caches acquire a design obligation: a refresh mechanism whose request-side
  face is stale-serve, with the 429 residual stated in a testable conformance obligation.
- `Engine::viewport`'s "the session that asks pays, once" paragraph — already overruled by 0043
  — is now overruled with a stated budget.

## Provenance

Owner rulings, 2026-08-03 (D2 "happy to split", D3 deferred to the consolidator and taken as
own-swap) and 2026-08-04 (D1, quoted above), during review of the write-path consolidation
(`docs/design/write-path.md`).
