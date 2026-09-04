# 0124 — The suggestion route may follow the viewer's own cardinality

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

Whether category value suggestion may choose *how* it answers — a per-session set of the values a
principal can see, or a posting probe per value walked — on a quantity that varies with the
principal. `architecture.md` §8.2 says it may not: *statistics-driven reordering would make
execution time a function of how much the principal can see*. This ruling narrows that rule on one
surface. It shapes decision
[0121](0121-the-suggestion-walk-probes-per-request-and-its-timing-is-accepted.md), which made the
probe the construction and the set a lever held in reserve.

## The decision

**The set is the second route, built on demand, and the route follows the viewer's own
cardinality.** A suggestion is answered from a per-`(session, resolved column, generation,
overlay_version)` bitmap over dense value positions **iff** the composed candidate's cardinality is
at or under `selection.max_suggest_set_entities` — a deployment constant published on `/v1/meta`,
recommended default 10⁷ — and the column has an entity-space value column to sweep. Every wider
viewer, every blob-resident column, and every keystroke before that session's set has finished
building take the probe route of `value-suggestion.md` §6.2.

⊘ **Not built.** No per-session set exists in the tree; every request today takes the probe route,
so the choice this decision authorises is not yet made anywhere.

**Two things were settled in the same ruling** on the probe route's own measured stages
(`probes/2026-09-02-value-suggestion/` arm 4, which puts 68–72% of a probe in the binary search for
the record): a **4.2 MB bucket table** over the code's top 20 bits, taken as a fix-it-now item on
the probe route (⊘ not built — the sparsest viewer's spent budget falls from 62–82 ms to ~28–40 ms,
modelled), and a **per-record container-key sidecar**, declined — 12–17% at best, nothing at all
against a scattered candidate, and 82 MB per column.

## Why it is admissible

**The quantity consulted is the caller's own, and they already have it.** `/v1/viewport` at zoom 0
over the full extent returns the composed cardinality of the caller's own mask exactly, as
`visible` (contracts §7.1). A route keyed on that number discloses a self-disclosure: at most, which
side of a published constant it falls on. The constant itself is a deployment parameter identical
for every principal, not a corpus statistic and not anyone else's mask — so neither half of what
§8.2's rule protects against is present. What that rule forbids is a route chosen by *statistics
over data the principal cannot see*, which makes execution time a channel about the corpus or about
another viewer's clearance; this is neither.

**The page is identical either way.** Both routes evaluate the same predicate against the same
composed candidate, so the values, their order, their spans and their counts match. The one
observable difference is `more`: exact from the set route, and possibly a spent walk budget from the
probe route — one bit of the quantity already accepted and registered as C31.

**What it costs if it is wrong.** A viewer learns which side of a public constant their own
cardinality falls on, by timing or by an exact `more`. Nothing about another principal's `M_auth`,
no value name, no member count, no corpus statistic.

## What this changes elsewhere

**`architecture.md` §8.2** gains one sentence naming this as its single exception, and **C31's
disposition is amended rather than reversed**: still accepted, and **closed for an indexed column
once that column's per-session set is warm** — a walk over visible positions alone carries no count
of hidden values. It stays open on the probe route: the first keystrokes on a session-column pair,
viewers wider than the constant, blob-resident columns, and `/v1/categories` in every case.

**`contracts.md` §3.2** gains `selection.max_suggest_set_entities` and the note that `more`'s
spent-budget form is the probe route's. **`value-suggestion.md` §6.3** stops being a lever held in
reserve and becomes the second route, with the four rules that govern the set — a key that no longer
matches the live generation and overlay version is discarded rather than served, which is
fail-closed; one build in flight per key; this cardinality rule; and no set for a column with
nothing to sweep.

**Decision 0093 is
honoured, not widened.** Nothing is materialised per session in advance: the build is dispatched by
the first suggest on the pair, that request is answered by the probe route meanwhile, and residency
and eviction stay under 0093's byte budget.
