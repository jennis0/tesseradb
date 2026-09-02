# 0118 — Value suggestion is categories only; a keyword-shaped need is declared as a category

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

A viewer typing into a category filter should be offered the values that complete what they typed.
The question was whether that surface also covers `keyword` and `text` columns, which have
dictionaries a prefix could be searched in. `value-suggestion.md` §2 put it; this is ruling A of
its §10.

## The decision

**Categories only.** `GET /v1/categories/{column}/suggest` exists for a column with a vocabulary
and for nothing else. A need shaped like keyword autocomplete is met by declaring the column an
**open, `derived` category** — `value_set = "open"`, keys minted at ingest, a code per row, derived
postings per value, gated listing — which already exists and already carries the gate.

**This confirms two standing refusals rather than amending either.** `filter-index.md` §1.1 refuses
prefix autocomplete over a string column because it "would manufacture a value set for a type that
has none", and `records-and-search.md` §4.3 says the same for `keyword`: no value set, no listing,
no autocomplete, the dictionary never served. Neither document changes.

## Why

Nothing structural prevents a keyword suggestion — a per-layer sorted dictionary gives a prefix an
ordinal range. Three things make it the wrong design anyway.

- **The size.** A keyword vocabulary is open-ended and per layer: 542,489 distinct `submitter`
  values in 2.4M items, one `doi` per item. A per-session visible set over 10⁹ ordinals is 125 MB
  per session per layer, against the **1.25 MB** a 10⁷-value category's set saturates at
  (measured, `probes/2026-09-02-value-suggestion/` arm 3).
- **The identity is layer-scoped.** Suggestions would be unioned across every layer's dictionary
  and deduplicated by string per request, with no stable identifier to hand back — a keyword
  ordinal never crosses the trust boundary. What comes out is a category with the codes taken out.
- **The declaration already says it.** A column whose values are worth suggesting has a value set
  the operator cares about, which is what a vocabulary is.

**Text** is refused more firmly: a token index is an analysed artefact, and a suggestion over it is
a surface over data-derived tokens nobody authored.

## What this does not change

`filter-index.md` and `records-and-search.md` are untouched. **Tags** — several values per item —
stay behind decision [0039](0039-multi-valued-categoricals-are-slow-path-only.md)'s fence; a
multi-valued category gets this surface for free when it is built, the gate and the index being per
value.
