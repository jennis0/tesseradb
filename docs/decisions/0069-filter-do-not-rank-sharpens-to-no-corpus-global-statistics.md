# 0069 — "Filter, do not rank" sharpens to "no corpus-global statistics"

**Date:** 2026-08-12 · **Status:** Settled (owner ruling)
**Reads with:** architecture §8.2–§8.3, §4 (I2, I7), §10.4, Appendix D;
[`records-and-search.md`](../design/records-and-search.md) §4.5.

## Context

Architecture §8.3 rules *filter, do not rank*: relevance scores and rank shifts computed from
corpus-global statistics are a demonstrated channel for inferring the content of unreadable
documents (Appendix D). The text family needs scoring in some form — at minimum, choosing which
matches fill the mark cap when a `match` exceeds it — and the rule as phrased forbids more than
its own argument does.

## The decision

**The rule's load-bearing half is: no statistic a score reads may be corpus-global. Every
statistic is a function of `(M_auth, query)` — score as if the visible corpus were the whole
corpus.** Mask-local ordering applied **after** intersection is admissible; it is §8.2's own
threshold-then-top-k discipline. Concretely: mask-local document frequency is one
`and_cardinality` per query term against the candidate; a score so computed is inside **I2** by
construction, deterministic (so the served set stays a pure function of
`(mask, corpus state, k, viewport)`), and defined over the visible set (**I7**). Every threshold
and anchor stays on `M_auth`; the score orders only which of `M_sel`'s members are drawn.

What does **not** change: no ranked-list response shapes; no relevance scores on the wire in v1;
Appendix D's channel — corpus-global IDF and its relatives — stays excluded by construction, not
by review vigilance.

## Consequences

- architecture §8.3's statement is amended to this form in the records-and-search promotion
  pass (records §13), citing this decision.
- The first consumer is records §4.5's staged scoring: coverage (m-of-n) as the filter
  primitive, mask-local BM25 as the match layer's cap-selection order, a normalised threshold
  only on demonstrated need.
