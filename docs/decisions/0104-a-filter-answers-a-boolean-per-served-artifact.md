# 0104 — A filter answers a boolean per served artifact, and it answers about what is in view

**Date:** 2026-08-28 · **Status:** Settled (owner ruling, 2026-08-28) — **drafted for the owner; the
scope clause in §*What is in view, and why not the whole membership* is the part to check.**

## The decision

**A `/v1/viewport` request carrying `filter` gets one boolean per served artifact: whether any
member of it that this principal may see, and that lies inside the request's tiles, matches the
filter.** In set terms, `membership ∩ viewport ∩ M_auth ∩ M_sel ≠ ∅`.

**It is a boolean and never a count.** A filtered count beside the masked count would put two
numbers on one artifact and make the client choose which it is showing — the ambiguity
`annotation-representation.md` §6.3 raises, not an answer to it.

**Existence and the masked count stay anchored on `M_auth`.** A filter still cannot make an
artifact appear or vanish, and cannot move the number beside it. The bit is the *only*
filter-dependent thing in the artifacts frame.

**Absent when the request carries no filter** — not `false`. There was no question, and a `false`
would say there were no matches.

**A dependent artifact carries its target's bit.** A label describes its cluster, so *does anything here match* is a
question about the cluster; a label's own membership is a slice of it at best, and answering from
that would print `false` beside a cluster reading `true`. The target is in the same response — a
dependent whose target was not served is absent entire — so the value is derivable from the frame
and discloses nothing new ([decision 0023](0023-derivable-quantities-are-not-disclosures.md)).

*Amended 2026-09-18 (owner ruling).* This rule was first written beside a second one, that a
dependent's row carried its target's masked count. That rule is withdrawn: a dependent's row names
its target by `tessera_id` (contracts §3.2, r96) and carries its own count. The bit rule rests on
the argument above and not on the count. Whether the copied bits stay, a client now being able to
read them from the target's own row, is open.

## Why a bit at all

A viewer filters to find something, and the map answers with points. The clusters beside those
points are the part of the answer that says *where else* — and today they cannot say it, because
every artifact is served identically whether or not anything in it matches.

**The client cannot compute it.** The points it holds are a *sample* of `matched`, so deriving
*which clusters still hold a match* from the sample gives false negatives for exactly the small
clusters a filter is used to find. That is the sample-as-set error, stated at `client-interaction.md`
§2. A bit computed by the server against the whole visible membership in view is the only correct
answer available.

## What is in view, and why not the whole membership

The obvious spelling is `membership ∩ M_auth ∩ M_sel` — the whole visible membership, no viewport
— and it is **not** the one taken, for a reason that is about routes rather than about taste.

A filter's result reaches row space by one of three routes, and two of them are *silent* outside the
request's tiles rather than negative there: the per-tile crossing tests only the rows the request
spans (`FilterRows::Viewport`), and the render-column route (decision 0068) evaluates over the same
domain and has no entity-space answer to fall back to when its column lives only there. So a
whole-membership bit is exactly computable on the projecting crossing and *not computable* on the
other two without evaluating the filter over the whole view — the corpus-scale cost the row route
exists to avoid, paid on every request that names both a filter and a layer.

The alternative to a single meaning would be a bit whose extent depended on which route the engine
happened to take. That is the failure `FilterRows` carries its domain to prevent, one level up: a
field that is exact on Tuesday and quietly narrow on Wednesday, with nothing on the wire saying
which.

**So the bit means the same thing on every route**, at one hoisted intersection —
`viewport ∩ M_auth ∩ M_sel`, composed once per request and asked per artifact — which is the same
construction, and the same argument, as the three candidacy routes: *what the layout chooses is the
route, never the answer* (`artifact-serving-at-scale.md` §4.1).

**The cost of that choice is a stated inconsistency**: an artifact's masked count and its geometry
are over its whole visible membership, never clipped to the viewport, while its bit is clipped. An
artifact drawn at the edge of the screen whose only matches sit just off it reads `false`. That is
the honest cheap answer rather than a wrong one — the viewer pans and it becomes true — but it is a
difference between two fields of one row and it belongs on the wire's face, not in a comment here.

## Consequences

- **No leak-register row.** Appendix C's inclusion test: *a row exists only where a viewer, reading
  responses they are entitled to, can end up knowing something about data they were not served*,
  and *data the service serves never qualifies*. The bit is a property of a subset of this
  principal's own visible members, computed from inside `M_auth`, over rows the same response's
  tile counts already report a `matched` figure for.
- **I3 and I12 are untouched, and the bit is what keeps them cheap to check.** The filter reaches
  exactly one field. `MaskedSet::count_intersection` stays filter-blind, `visible_total` stays θ's
  unfiltered anchor, and the existence criterion still reads the same masked count it always did.
- **A held artifact payload survives a filter change.** Key, masked count, centroid, box, hull,
  content, parent and level are a function of `(artifact, M_auth, generation)` and of nothing in the
  request; the bit is the only part that moves. So the client's artifact cache
  (`artifact-cache-handover.md`) may hold payloads across a filter change and take the bit fresh,
  and client-obligations rule 7 — which drops held sets on a filter change — narrows accordingly.
- **The content key does not move.** It answers *is what you hold still true*, and a filter changes
  no held payload. Putting the filter into it would discard a valid cache on every keystroke, which
  is 0103's trap in a second dress.
- **The identifier route carries no bit**, `/v1/artifacts/{id}` carrying no filter to answer about.

## What this costs

Unmeasured, and not to be quoted until it is. The probe is one early-exiting intersection per served
artifact against a set the request already composed, so it is cheap where there **is** a hit and a
full pass over the artifact's containers where there is not — and a selective filter makes the
second the common case. A scattered artifact's membership spans 734–1,524 blocks
(`artifact-serving-at-scale.md` §7), which is the cell to measure.

## What this does not decide

**The wire affordance for a client's held set** (`artifact-cache-handover.md` §4) — how a client
says what it holds — is untouched here, and so is whether a client may hold a level whole and pick
from it locally.
