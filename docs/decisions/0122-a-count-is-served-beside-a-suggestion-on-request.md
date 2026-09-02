# 0122 — A count is served beside a suggestion on request, and never orders the page

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

Whether a suggested value carries a number — how many items the viewer can see that carry it.
`value-suggestion.md` §3 recommended against it; the owner ruled the other way. This is ruling E of
its §10.

## The decision

**`?counts=true` serves `count` per suggested value**: `|members(v) ∩ candidate|`, exact, the
viewer's own number, computed per request against the composed mask and never precomputed. Absent
without the flag — never `0` or `null` as a stand-in. ⊘ Not built.

**The count never orders anything.** Ordering is lexical over the matched text, fixed at index
build and the same for every principal.

## Why

It is C8's quantity on a new surface, not a new register row: C8 already says a legend with counts
is an `and_cardinality` against `M_auth`, computed per request and never precomputed, and that a
stored per-value total would be the corpus-wide count it forbids. Every condition of the row is met
here — it is the viewer's own number, it is computed only for the values the page serves, so the
work is bounded by `limit`, and the extents half is counted in the sweep that already finds
post-build membership.

**The count is admissible as data and not as an order.** It would be a lawful sort key under I2,
being the viewer's own quantity, but a count-ordered page is a top-*k* over the prefix, which §8.2
forbids for the reason it forbids top-*k* filters: the result would depend on which values were
examined before the walk budget ran out. Frequency and popularity are refused outright by decision
[0069](0069-filter-do-not-rank-sharpens-to-no-corpus-global-statistics.md).

## What this does not change

**The enumeration still serves no counts** (contracts §3.2), and that is not an inconsistency: its
`?codes=` form is a legend's resolve, and a count beside every code a client drew is the
per-viewport breakdown surface §8.2 owns. A suggestion page is a prefix the caller typed and at
most `limit` values. Both numbers would be the same quantity; what differs is whether the caller
chose the rows. C8's cell in Appendix C records both halves.
