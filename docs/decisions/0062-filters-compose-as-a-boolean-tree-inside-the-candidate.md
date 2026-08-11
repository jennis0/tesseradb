# 0062 — Filters compose as a boolean tree evaluated inside the candidate

**Date:** 2026-08-09 · **Status:** Settled (owner ruling)

## Context

`architecture.md` §8.2 makes **intersection the only top-level operator** over filter operands, and
`filter-surface.md` recorded negation as out of scope with three reasons. Both were written for an
index that evaluated an operand *unmasked* and intersected afterwards.

That index is gone. [0058-adjacent work](../evidence/memos/2026-08-08-filter-index-decisions.md)
made the flat value column the artefact of record and the **mask the scan's candidate**, so an
operand is evaluated inside `M_auth` from the first byte read. Two of the three reasons against
negation were properties of the old shape and did not survive it:

- *"Principal-dependent, so it can never share a cache"* — every operand result is principal-specific
  now, which is why `filter-surface.md` §4's shared projection cache is superseded. Negation is no
  worse on this axis than equality.
- *"Needs a not-null bitmap only BSI carries"* — the presence bitmap is part of the column
  (`filter-index.md` §2.1), so an item carrying no value is exactly representable and correctly
  excluded.

The third — *no corpus document defines it* — was true and is what this decision addresses.

Separately, a viewer needs **disjunction across columns**: `department = eng OR region = emea`.
Disjunction *within* a column is already `in`; across columns there was no expression at all.

## The decision

**§8.2's composition rule becomes: any boolean combination of leaf predicates, evaluated within the
candidate.** The wire form is `all_of` / `any_of`, nesting to a bounded depth, with a leaf naming a
declared column.

**This does not widen what a filter can return.** Every node evaluates to a subset of the candidate,
because every leaf does and union and intersection of subsets are subsets. So **I12** holds
structurally rather than by check, and the leak register stays enumerable — the *leaves* are
enumerable, and the combinators cannot produce a set outside the principal's own mask.

**Negation is specified but not built** (`none_of`), and it carries one rule that must be built with
it rather than after it. See below.

## Why the Elasticsearch spelling is not adopted

The wire shape was drafted as ES's `bool` with `filter` and `should`. That is wrong on ES's own
terms: `filter` and `must` are both *required* and differ only in scoring, while `should` is
**optional** — its clauses affect the score, and become "at least one must match" only when no
required clause sits beside them, under `minimum_should_match`. So `{filter: [A], should: [B, C]}`
means *"must match A; B and C affect ranking"*, the opposite of the disjunction it was borrowed for.

§8.3 already rules **filter, do not rank**. The whole ES bool vocabulary is scoring-shaped, so
borrowing its words while discarding its semantics would mislead precisely the readers who recognise
them. `all_of` / `any_of` / `none_of` say what they do and carry no imported meaning.

The same reasoning keeps the operator names plain — `eq`, `in`, `prefix`, `contains` — rather than
ES's `term` and `match`, which differ on *whether the value is analysed*. That distinction belongs
to the column's declared type, not to the operator (below).

## `none_of` over a `per_viewer` category is a C11 disclosure

**Not measured, not reviewed — reasoning, recorded so it is checked before `none_of` is built.**

`/v1/categories` filters the offered vocabulary to the values a principal may see, which is what
`listing = "per_viewer"` buys. A set-complement negation defeats it directly:

> `none_of: [every value I was offered]` returning a non-empty set proves there exist values the
> principal was not shown.

**The rule to build with it:** over a `per_viewer` category, `none_of` is evaluated **within the
visible vocabulary** — *carries some value I may see, other than these* — rather than as complement
over the column. One extra intersection, and the channel closes.

It does not arise for a `public` vocabulary (the value set is published anyway) or for a string
column (no value set exists, so its complement discloses nothing about one).

## Text becomes a column type, not an operand family

`text` is deleted as a named operand. A text field is an **attribute with a declared type**, which
gives a document as many text fields as it declares rather than the single global body a top-level
`text` operand could address.

The distinction ES spells `term` versus `match` is kept, as a property of the column:

| Type | Matches | Operators | State |
|---|---|---|---|
| `utf8` | stored bytes | `eq`, `prefix`, `contains` | built |
| `text` | analysed tokens | `match` | ⊘ specified, not built — [#44] |

§8.3's text paragraph is deleted rather than amended: its embedded-index framing presumed a single
corpus-wide index, and its global-caching argument was already void under masked evaluation.

## What is not decided here

**`labels` keeps its name for now**, and the rename the owner raised (`visibility_labels`) is
deferred with a note: **"label" is overloaded in this corpus** — §8.1's labels are cluster
annotations gated by generating sets (**I3**), while the house vocabulary uses *conservative label
join* for the authorisation side. Any rename should disambiguate the two concepts rather than
retitle one of them, or the collision simply moves.

**No `attributes` wrapper.** An earlier draft namespaced attribute predicates so a column named
`labels` could not collide with the label operand. Neither the label nor the text operand exists, so
that is a shape whose only justification is a future state — which
[0048](0048-no-deployments-exist-so-delete-rather-than-support.md) forbids. A leaf is a column name
directly. `all_of`, `any_of` and `none_of` become reserved and are refused as column names at schema
parse, which is a build-time refusal rather than a request-time ambiguity.

**Nesting is bounded.** Unbounded depth is unbounded per-request work from one authenticated call —
§7.3's sub-cell budget makes the same argument.

## Consequences

- `architecture.md` §8.2's composition rule is amended; §8.3's text paragraph is deleted.
- `contracts.md` §3.2's `filters` object gains the tree and loses `text`; families-compose-by-
  intersection is no longer a rule because there are no longer families.
- `filter-surface.md`'s negation paragraph is replaced by this decision and the C11 rule above.
- The schema gains a `text` type, refused at parse until #44 (`filter-index.md` §2.3).

[#44]: https://github.com/jennis0/tessera-index/issues/44
