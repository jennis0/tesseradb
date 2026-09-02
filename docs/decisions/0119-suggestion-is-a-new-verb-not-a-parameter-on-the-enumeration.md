# 0119 — Suggestion is a new verb, not a `?q=` parameter on the category enumeration

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

`GET /v1/categories/{column}` already lists a vocabulary's values. Adding `?q=` to it is the
obvious first draft of a typeahead. `value-suggestion.md` §5.2 put the question; this is ruling B
of its §10.

## The decision

A new verb, **`GET /v1/categories/{column}/suggest?q=&limit=[&counts=][&view=]`**, uncursored,
ordered by what matched, returning at most `limit` values with a `match` span each and a `more`
flag. ⊘ Not built.

## Why

The enumeration is a legend's endpoint and a typeahead is not, on four counts.

- **Order.** The enumeration orders by key because a key is a total order and therefore a cursor. A
  typeahead orders by what matched: a title match on `machine learning` must sort where the user
  expects `m`, not where `stat.ML` falls in key order.
- **Paging.** Typeahead never pages — the user types another character. A cursor over matches
  invites a client to walk the whole match set, which is the enumeration by another name.
- **Titles.** The enumeration's cursor is the key; a title match has no place in that order.
- **Composition.** `codes`, `after` and `q` together have no sensible meaning, and the contract
  would spend its words on which combinations are `422`.

**The two verbs share one gate and not one shape.** The same `visible(code)` predicate, the same
candidate composition, the same `500 fail-closed` refusal, resolved at the same address-resolution
site — a gate reached by one door and not the other is the existence oracle by another route.

## What this does not change

`api_version` stays at **1** and `bundle_format` does not move: a route and two `selection` fields
are additive. No `suggest` capability flag is published — it would be a constant, and there is no
older server to read it defensively against (decision
[0048](0048-no-deployments-exist-so-delete-rather-than-support.md)). A cross-column
`GET /v1/suggest` is deferred (decision 0123).
