# 0068 — A row-space operand bounded by the request's domain is admitted, and a rendered column is filterable

**Date:** 2026-08-12 · **Status:** Settled (owner ruling)
**Reads with:** architecture §8.2; [`records-and-search.md`](../design/records-and-search.md) §6.2;
the placement memo [`2026-08-12-filter-placement.md`](../evidence/memos/2026-08-12-filter-placement.md)
§2–§3, §5; [`probes/2026-08-12-filter-placement/`](../../probes/2026-08-12-filter-placement/);
decisions [0062](0062-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0064](0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md).

## Context

§8.2's contract says every filter returns an entity-space bitmap. A filtered viewport answered
from the **render column** — evaluated over the request's own rows, against the hot column —
returns a row-space set instead, exact only over the request's domain. Measured at 10⁸ it is
7–1,269× cheaper than the entity route on the request shape a viewer actually issues
(probe-route constants; the built version pays the segment boundary on top), and it needs no
entity-space artefact and nothing on the write side. The type it produces already exists:
`FilterRows::Viewport { rows, domain }`, consumed by `EffectiveMask::with_filter`.

## The decision

**`FilterRows::Viewport` is admitted as a second operand kind under §8.2.** Three rules travel
with it, all from the placement memo's measured design:

- **`render = true` makes a fixed-width column filterable** — categories now; numbers and
  datetimes when decision 0064's render half lands, and refused until then, because the hot
  column stores an absent number as zero and a range containing zero would match every item
  with no value.
- **The route rule** where a column affords both routes: row space while
  `rows_in_ranges ≤ |M_auth|`, entity space past it — both quantities the caller could compute,
  never a statistic about the principal's data.
- **Composition keeps one crossing per request** (0062's tree): evaluate the entity-space
  sub-tree, cross once by the measured rule, evaluate row-space leaves over the crossing domain,
  combine in row space. The candidate is the composed verdict, as for every route.

## Why this does not weaken the contract

Every answer is exact over its domain — `filter_routes_agree_over_the_domain` asserts route
agreement — and a row-space result is a subset of the request's own authorised rows, so **I12**
holds structurally exactly as it does for entity-space leaves. The work is a function of the
request's ranges and the column, never of the value. What the operand kind cannot do is stated
where it bites: it is per slice, it cannot serve a caller outside a viewport, and it cannot
answer the membership question — which is why a category's entity-space structures exist
whatever its flags say (records §4.2).

## Consequences

- architecture §8.2 gains the second operand kind and the route rule, in the design's
  promotion amendments pass (records §13).
- Store-once follows for rendered numbers and datetimes: no entity-space copy is built for them
  (records §6.2); the alternative — a second copy of every rendered column, 1–8 GB per column at
  10⁹ — is what this ruling declines.
