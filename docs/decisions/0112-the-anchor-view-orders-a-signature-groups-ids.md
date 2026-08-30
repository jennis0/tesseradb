# 0112 — A declared anchor view orders ids within a signature group

**Date:** 2026-08-30 · **Status:** Settled (owner ruling, 2026-08-30) · **Extends:** [0073](0073-entity-ties-are-ordered-by-morton-code.md)

## Context

Entity ids are assigned once, in signature-sorted order — the property nearly all the measured
value lives in, untouched here. Decision 0073 breaks ties within a signature group by Morton
code and then source row order, which assumed one source and one geometry. A multi-view build
reads several sources; an item holds a different position in every view it joins and a row
number in every file, so neither half of the tie-break denotes anything.

## The decision

**The tie-break within a signature group is the item's Morton code in a declared anchor view,
then the caller's `external_id` bytes.** The anchor is `[defaults].allocation_view`, naming a
declared view; it is **required when the declaration carries more than one view** and refused
absent, naming the candidates — explicit rather than positional, so reordering declaration
blocks cannot silently re-key a rebuild. With one view it defaults to that view. An item absent
from the anchor takes its Morton code in the first-declared view that holds it; the external id
orders what remains.

Rejected: the positional form (first-declared view as anchor) — the same locality with a silent
sensitivity to block order; and dropping the spatial tie-break for `(signature, external_id)` —
simplest, but it forfeits the within-group locality for a saving nobody measured.

What this does not touch: ingest allocation (above the high-water, signature-sorted per commit
window, no geometry), the wire, and the mask arithmetic. The choice decides only which ids are
neighbours inside a signature group.

`views.md` §7 carries the build's two passes; `configuration.md` §1 the key.
