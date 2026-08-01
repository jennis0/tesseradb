# 0019 — I13 names three lettered properties, and I13a is an addition

**Date:** 2026-08-01 · **Status:** Settled · **Refines:** the S7 ruling

## Context

I13 read "a partition not consulted fails closed". Thirty-five annotations in the code cite I13 —
and every one of them is about something else: single-flight caching, cancellation, and poisoned
shared slots. A reviewer grepping the number found coverage and concluded the partition property
was tested. It is not implemented at all.

The first fix lettered the invariant into I13a and I13b. An independent review then established
that this was not a clean split: **I13b is a verbatim renumbering of the old I13, but I13a is new
content.** Single-flight, cancellation and poisoned slots appear nowhere in the previous §4. The
justification offered — that the code annotations concern that half — has the authority backwards:
those annotations were mislabelled, and the fix promoted a code convention into the specification
under an existing number.

The review also found a third property that neither letter covers. Two documents cite I13 for
**outage asymmetry** — a partition unreachable through *failure* is an error, never an empty
contribution, which is distinct from one unreachable through *authorisation*.

## Decision

**I13 becomes a headline with three lettered properties**, and the addition is recorded as an
addition rather than a clarification.

- **I13a** — a request that fails or is cancelled yields no partial answer. **New.** It is a real
  property, it is enforced, and the code already relies on it; it simply had no place in §4 before.
- **I13b** — a partition not consulted fails closed. Unchanged, and unimplemented.
- **I13c** — a partition unreachable through outage is an error, never an empty contribution.
  Recovered from two documents that were citing I13 for it.

The headline sentence — an answer that was not computed is a refusal, never a vacuous success —
states what the three share.

## Why not the alternatives

Demoting I13a to a §10.4 or lifecycle rule is truest to what §4 has always meant, but it costs
re-lettering roughly fifty references and demotes a property the code genuinely depends on. Opening
I14 separates it most cleanly, but the three properties are one idea seen in three places, and
splitting them across two numbers loses that.

## What this obliges

Every unlettered `I13` reference in the code, the conformance suite and the corpus must be
lettered. The whole point is that confirming one property cannot be read as covering another, and a
bare `I13` defeats that at the first grep.

## Evidence

Register row S7 and the loss-detection review of architecture r25.
