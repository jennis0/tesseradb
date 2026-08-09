# 0060 — A category's postings serve a `public` listing and never a `per_viewer` one

**Date:** 2026-08-09 · **Status:** Ruled by the owner.
**Reads with:** [`filter-index.md`](../design/filter-index.md) §2.3,
[`filter-surface.md`](../design/filter-surface.md), `per-point-attributes.md` §3.3 and §3.8,
architecture §4 (I2) and Appendix C (C4, C8, C11),
[`probes/2026-08-08-filter-layout/`](../../probes/2026-08-08-filter-layout/) arms 2 and 9.

## The decision

A category column's derived per-value postings may answer a filter **when the column's vocabulary
is `listing = "public"`**. Where it is `listing = "per_viewer"`, the filter is answered by the
**masked scan**, and the postings serve only the membership question `/v1/categories` asks of them.

The routing rule is a function of the **declaration**, not of the request, the principal, or any
statistic — so it is fixed at schema time and identical for every viewer.

## Why the postings cannot serve a `per_viewer` column

Postings resolve over the **whole corpus** and are then intersected with the composed candidate,
where the scan takes the candidate as its input. That difference is invisible in the answer and
visible in the timing.

Arm 2 measured a hidden *correlated* value as flat — 0.000 ms whether it had no members or 250
million, because Roaring's intersection short-circuits on container keys — and recorded the
*scattered* case as unmeasured. Arm 9 measured it:

| Value the principal cannot see | intersect |
|---|---|
| Does not exist (no members) | **0.000 ms** |
| Hidden, 10⁷ members, scattered | **2.1 ms** |

A scattered value's members fall in every container, so the containers meet the candidate's even
though no bits do: the work is container-proportional while the result is empty.

per-point-attributes §3.8 requires a value the principal cannot see to be indistinguishable **in
work** from a value that does not exist. Under `listing = "per_viewer"` that is the whole control —
the surface exists to hide which values are there — so a 2.1 ms difference is a disclosure of
exactly what the declaration withholds, reachable by ordinary operation. The scan has no such
channel by construction: its work is a function of `(candidate, column)` and never of the value.

## Why they may serve a `public` one

Under `listing = "public"` the vocabulary is served to every principal alike, so what the timing
distinguishes — this value exists and has members somewhere, against this value does not exist —
is a fact the client was already handed by `/v1/categories`. Nothing is learned that was withheld.

That is a **C4-shape row with C8-adjacent content and it must be registered**, not waved through:
the quantity is corpus-wide and pre-mask, and the leak register is exhaustive because the query
surface is enumerable (§8.2). Registering it is a condition of taking this decision, not a
follow-up.

## What this costs and what it buys

**Buys**, measured at 10⁹ against the shipped scan: a selective category operand goes from 74–280 ms
to 0.13–49.5 ms, and an unselective `in` over a scattered vocabulary from 3,641 ms to 287 ms at 25%
coverage. The one cell the postings lose is a set naming most of the vocabulary (664 → 872 ms),
which the routing rule can decide from the named share — a public quantity.

**Costs**, measured: **2.01 GB per fully scattered 10⁹ category column**, twice the `u8` column it
accelerates, against 217 KB when the values correlate with the entity order. Sixteen scattered
columns is ~32 GB. The postings are already built and digested for every category column, so this
decision spends read-path complexity rather than new disk — but a deployment declaring many
scattered categories is paying that disk today whether or not it filters.

## What was considered and refused

**Postings everywhere, with a constant-work probe.** Arm 2 measured a candidate-driven probe that
is flat across hidden and absent values (~1.1 ms either way), which would restore §3.8 for
`per_viewer` too. Refused because it is *slower than the scan it would replace* on the selective
operands that dominate, and it buys back a property the scan already has for free.

**Postings everywhere, accepting the channel.** Refused: `per_viewer` exists to make a value's
existence unobservable, and an accelerator that makes it observable in timing defeats the control
it is layered under, whatever the latency.

**Deciding per request from the operand's cardinality.** Refused: §8.2 forbids a statistics-driven
route, because it makes execution time a function of how much the principal can see.
