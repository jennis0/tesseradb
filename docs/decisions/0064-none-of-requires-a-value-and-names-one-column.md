# 0064 — `none_of` requires a value and names one column, which is what makes a negation safe

**Date:** 2026-08-11 · **Status:** Settled (owner ruling — build `none_of`)

## Context

[Decision 0060](0060-filters-compose-as-a-boolean-tree-inside-the-candidate.md) specified `none_of`
and did not build it, behind one stated rule: over a `per_viewer` category a set-complement negation
is an existence oracle, because

> `none_of: [every value I was offered]` returning a non-empty set proves there exist values the
> principal was not shown.

A second fence was raised later and is the more dangerous of the two, because nothing enforced it.
`filter-index.md` §5 records **positivity** as load-bearing: every "this failure degrades safely
under **I12**" argument in that design — a lost layer, a lagging flush, a blanked slot, a buffered
entity with no reachable value — holds *only* because every operand is positive. An entity whose
value cannot be read matches nothing, so losing values under-reports, and under-reporting narrows.
Under a complement negation those same failures **widen**.

## Decision

**`none_of` means *carries a value in this column, and none of these predicates matches it*.**
Evaluated as `present ∩ candidate ∖ matched`, never `candidate ∖ matched`.

**Every sub-expression of a `none_of` names one and the same column**, refused otherwise with an
error naming the columns found and the composition to use instead.

## Why one requirement closes both fences

**Positivity is preserved rather than argued around.** An entity whose value is unreachable is
absent from `present`, so it matches no `none_of` — exactly as it matches no `eq`. Every failure
mode §5 and §6.2 enumerate keeps the sign it had: values lost, result narrower, **I12** absorbs it.
The property is asserted rather than asserted-about — `tests/filtering.rs` builds a *buffered*
entity, which is in the candidate and in no layer, and requires that it match no negation. Under the
complement reading that test fails, along with four others.

**The C11 oracle closes by construction, and needs no extra intersection.** 0060 prescribed
evaluating within the visible vocabulary — "carries some value I may see, other than these" — as a
separate mitigation. It is not separate: evaluation already happens inside the candidate, and an
entity in the candidate carrying value *v* is *itself* the witness that makes *v* visible under
C11's derivation (a value is visible iff at least one of its members is in `M_auth`). So every value
reachable in a `none_of` result was offered, and `none_of: [every offered value]` is empty. The
presence requirement **is** the intersection 0060 asked for, arrived at from the other side.

The fixture pins that this is not vacuous: the subset principal sees `e % 3 == 0`, every item
carrying `legal` falls outside it, so `legal` is a value the corpus genuinely carries and that
principal is genuinely not offered — and the oracle still returns empty.

## Why one column, and why nothing is lost

A negation has to require presence *in the column it negates*, and a multi-column `none_of` gives
two answers to which column that is. Requiring presence in **all** columns mentioned makes
`none_of: [A, B]` narrower than the natural reading; requiring **any** re-opens the fail-open
direction, since a lost layer in one column would leave an entity matching on the strength of the
other. Neither is a defensible default, so the shape is refused.

It costs no expressiveness. `all_of: [{none_of: [A]}, {none_of: [B]}]` is *exactly* the all-columns
reading — `(P_A ∖ A) ∩ (P_B ∖ B) = (P_A ∩ P_B) ∖ (A ∪ B)` — and says which presence each clause
requires. The multi-column form was only ever a shorthand for a set the surface already expresses,
which is what makes refusing it a narrowing of the query surface rather than of the query language.

## What this means for a viewer

An item carrying no department is **not** in `none_of: [department = eng]`. That is the intended
reading — *"in a department other than engineering"* is a claim about items that have one — and it
is the only reading that keeps the failure arithmetic. A viewer who wants "no department, or a
department other than eng" is asking a different question, and the surface does not currently
express "carries no value"; adding it would be a new positive operand (`absent`), not a change here.

## Consequences

- A negation always takes the **scan** route, never a category's derived postings (decision 0061).
  Presence from postings would be a union over every code — O(values) file reads for what the value
  column answers in one intersection per layer — and would cover only the base, since no flush
  writes postings.
- `filter-index.md` §1.1's "negation is not an operand" and §5's positivity tripwire are rewritten;
  the tripwire's demand ("the review that ever lifts that fence must revisit layer composition and
  every start-up failure mode under the inverted sign") is discharged by the sign never inverting.
- `MAX_FILTER_DEPTH` counts a negation like any other node.
- The wire gains `none_of` beside `all_of`/`any_of`; the one-column rule is enforced in the engine
  rather than the DTO, so an embedder building a `FilterExpr` directly meets the same refusal.

## Evidence

`crates/tessera-engine/src/filter.rs` (`FilterExpr::NoneOf`, `check_negations`, the `NoneOf` arm of
`eval`), `crates/tessera-filter/src/values.rs` (`ValueColumn::present_in`), and five tests in
`crates/tessera-engine/tests/filtering.rs` — semantics, positivity against a buffered entity, the
C11 oracle, the multi-column refusal and its composition, and nesting. All five were verified to
fail under the complement reading before being relied on.
