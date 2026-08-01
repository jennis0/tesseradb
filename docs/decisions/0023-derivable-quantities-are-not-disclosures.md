# 0023 — A quantity derivable from published data is not a disclosure

**Date:** 2026-08-01 · **Status:** Settled

## Context

The viewport response carries a per-tile `served` count: how many of that tile's points are in the
response. It has no leak-register row, and the argument for why it is safe lives in a source
comment.

It exists because under the density rule the per-tile count is
`min(min(cap, max(k_min, C_θ)), visible)`, which a reader cannot reproduce arithmetically. Without
it the differential oracle cannot split the points batch, and a client cannot truncate per tile to
its own budget — which the nesting argument requires it to do.

## Decision

**No row.** Instead, the rule is stated once in Appendix C's preamble: **a quantity a client can
already compute from data the service publishes is not a disclosure, and does not need a row.**

`served` qualifies. Every point carries its coordinates, `/v1/meta` publishes the quantisation
extents, and the containing tile follows from the Morton code and the zoom. The reference oracle
recomputes exactly that today. The field removes a recomputation; it does not add a capability.

## Why the rule rather than the row

The register is exhaustive by construction, and that property is worth more than completeness of
enumeration: a disclosure not in the table is a bug. But rows for quantities that disclose nothing
dilute it, and the next derivable field would raise the same question again. A stated rule answers
the class.

## The limit of the rule

Derivability is what closes this, and it is not a general licence. A field that is genuinely
underivable from published data is a new disclosure and needs a row, whatever its convenience
argument. The source comment that introduced `served` says the same thing and should stay: *do not
let this field be cited later as precedent that some other quantity must go on the wire because it
is otherwise underivable.*

## Evidence

Register row A6. `crates/tessera-engine/src/viewport.rs`; leak register C2, which is the precedent
for recording that something was checked and found not to leak.
