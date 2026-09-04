# 0121 — The suggestion walk probes per request, and its timing channel is accepted and registered

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

How a `derived` category's suggestion surface decides which values a viewer may be offered. Two
constructions were drawn in `value-suggestion.md` §6: a per-`(session, column)` set of visible
values computed once, or a probe per value walked, per request. This is ruling D of its §10, and it
is the one that touches the leak register.

## The decision

**(a) Per-request probes, with a walk budget.** A prefix becomes a contiguous range of the
suggestion index; each value in it is tested against the composed candidate until `limit` values
are emitted or **`selection.max_suggestion_walk`** (default 10⁵) values have been examined. ⊘ Not
built.

**(b) The timing channel is accepted and registered as C31.** A request's service time — and
`more` on a spent budget, which is a thresholded pre-mask count of the same quantity on the wire —
is a function of how many values sit under the prefix the caller typed, hidden ones included. That
is a corpus-wide, pre-mask fact about a value set `derived` exists to withhold, at a resolution the
enumeration's whole-set walk never offered.

**(c) The per-session visible-value set is the priced lever**, not the construction: a Roaring
bitmap over dense value positions, 13 KB at 0.06% density and 1.25 MB saturated, built by a pass
over the value column under the candidate. It is what closes the channel if the acceptance is
reversed.

## Why

The channel is registered rather than closed on decision
[0067](0067-term-timing-is-accepted-for-text-and-keyword-postings.md)'s reasoning for term
postings: comparable systems carry the identical channel ambient and unregistered, and here it is
bounded and conscious. What is carried is a **count of values**, never a name, never a member
count, never membership — measured, and flat in a value's size: a hidden value costs 0.06–0.13 µs
at the median whatever its member count, 3.4 µs p99, 103 µs worst
(`probes/2026-09-02-value-suggestion/` arm 2). The lever exists and is priced, so the acceptance is
reversible at a known cost rather than structural.

The budget default is measured, not chosen: at 10⁷ values a viewer seeing 0.01% of entities needs a
mean 32k–37k probes to find twenty visible values under a one-character prefix, so the 10⁴ budget
the draft proposed **never fills that viewer's page**. At 10⁵ every measured page fills, at
1.9–10.4 ms median and 21 ms p99, inside the owner's 10–100 ms.

## What this changes elsewhere

**It widens decision [0063](0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md).**
That decision says the derived postings "serve only the membership question `/v1/categories` asks".
They now serve three: that membership question, the suggestion walk's per-value probe, and — on
request — the per-row count decision 0122 puts on the suggestion page. What 0063 refused is
unchanged: the postings still never answer a filter operand on a `derived` column, which is
answered by the masked scan.

**It narrows per-point-attributes §3.8.** That section says an invisible value is indistinguishable
from an absent one "in outcome *and* in work". The in-work half is a **filter's** property,
obtained because a `derived` column's operand never touches the postings (C24). Both listing
surfaces do touch them, so §3.8 now claims outcome alone and points at C31 for the rest.

**A read failure mid-walk refuses the whole column**, not the value it failed at: refusing at the
value would make the refusal a function of the caller's prefix. `PostingsReader::open` validates
every record at open, so a later failure is a host IO fault rather than a shape of the data.
