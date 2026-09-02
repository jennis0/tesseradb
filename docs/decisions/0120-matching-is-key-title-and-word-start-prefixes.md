# 0120 — Matching is a declared prefix over the key, the title and every word start of either

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

What counts as a match when a viewer types into a category filter. `value-suggestion.md` §4 put
three options — key prefixes only, key and title, or those plus every word start; this is ruling C
of its §10.

## The decision

**Key, title and word-start entries, in the first revision.** Each value contributes to one sorted
index: the whole folded key, the whole folded title where one exists, and the folded title (or the
key, where there is no title) from each word boundary after the first. A word boundary is a
transition into a letter or digit from anything else, after folding, so `machine_learning` also
yields `learning`. A match is a **prefix of an entry**, found by two binary searches.

**The fold is declared and fixed**: Unicode NFKC, then default case folding, then whitespace
collapsed to one space and trimmed. The first two are the `unicode` analyser's own normalisation
(decision [0070](0070-analysers-are-named-and-declared-per-column.md)), factored out of `Analyser` as a
public function rather than duplicated; the whitespace collapse and the word-boundary rule are the
suggestion surface's own.

**Deliberately not matched**: infix, fuzzy or edit-distance matches, stemming and synonyms.

## Why

The rule must be a function of the schema, not of the corpus or the caller, or it becomes a
judgement a reviewer has to litigate and a principal can probe. Word starts cost 2.20 entries per
value on real place names (measured, `probes/2026-09-02-value-suggestion/` arm 1) — a linear index
cost, with a per-vocabulary knob if a deployment does not want it — and cover the case infix is
usually wanted for. Everything refused is a judgement; adding one later is an index-shape change
and nothing else, because the visibility construction does not depend on how an entry was derived.

## What this does not change

Ordering is the matched text's own — ascending by folded entry string, ties by entry kind then by
key — and never the corpus's: decision
[0069](0069-filter-do-not-rank-sharpens-to-no-corpus-global-statistics.md) forbids frequency and
popularity, and the served count never orders anything (decision 0122). A script written without
spaces yields no word-start entries, which is a consequence of the rule and not a defect of it.
