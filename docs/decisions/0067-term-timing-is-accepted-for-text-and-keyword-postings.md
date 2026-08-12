# 0067 — The term-postings timing channel is accepted for text and keyword terms

**Date:** 2026-08-12 · **Status:** Settled (owner ruling)
**Reads with:** [`records-and-search.md`](../design/records-and-search.md) §4.3–§4.5, §8;
[`filter-index.md`](../design/filter-index.md) §2.2–§2.3; per-point-attributes §3.8;
architecture Appendix C (C4, C8, C22, C24); decision
[0063](0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md);
[`2026-08-09-text-contains-acceleration.md`](../evidence/memos/2026-08-09-text-contains-acceleration.md) §5.

## Context

A per-term Roaring posting resolves over the whole corpus and is then intersected with the
candidate, so its cost tracks the term's **total** member set — pre-mask, including members the
principal cannot see. Measured over the shipped readers: a hidden value with members costs
**1.26–2.1 ms** against **0.000 ms** for a value with no members (Appendix C row C24 carries both
figures). A caller timing a filter can therefore distinguish *this term exists somewhere in the
corpus, with coarsely this many carriers* from *this term does not exist*.

For a category, decision 0063 bounds the route by declaration: postings answer a filter only under
`listing = "public"`, where the vocabulary is served to every principal anyway, so the timing
distinguishes nothing that was withheld. A **keyword or text term has no listing** — no surface
publishes its value set — so the same route over string terms discloses a corpus-wide, pre-mask
fact that nothing else serves: a C4-shape row with C8-adjacent content, the class on which trigram
postings were refused.

The records-and-search design needs the route twice: `text` has no alternative — its record is
block-compressed, no scannable column exists, and its aggregate surface (coarse-zoom counts under
I2) is only affordable from postings — and `keyword` needs it to close the one scan cell outside
the budget (a scattered broad principal) and to meet the 100 ms target.

## The decision

**The channel is accepted, for text terms and for keyword per-term postings — one register row
covering both.** Comparable systems carry the identical channel ambient and unregistered — every
posting-list engine's query time tracks term frequency — and here it is registered, bounded and
conscious.

What bounds it:

- The quantity is **existence plus coarse carrier count of a term the caller must already
  possess** — never membership, never which items, and nothing about any principal's `M_auth`.
  It is the class the register already accepts at C22 and C24.
- The route is a function of the **declaration**, fixed at schema time and identical for every
  principal — never the request, the principal, or a statistic (§8.2's rule, as 0063 held it).
- The scan routes keep per-point-attributes §3.8 structurally — including
  records-and-search §4.3's rule that an unresolved needle still scans — so the channel exists
  only where the postings route is taken.

## Conditions

1. **The Appendix C row lands with the first implementation** of a keyword- or text-postings
   route — a condition of this ruling, as C24 was of 0063, not a follow-up.
2. **The row's figures are measured at a real token vocabulary** (≥250k terms, 10⁸⁺ entities)
   over the shipped reader shape — records-and-search §11 item 1. The 1.26–2.1 ms above is the
   category shape; the token shape is expected to match and is measured, not assumed.

## The alternative, recorded so it is not re-litigated

**Refuse the channel and keep §3.8 universal**: keywords stay scan-only (250 ms–2.4 s at 10⁹ by
candidate shape, missing the 100 ms target in the scattered-broad cell) and the text family is out
of scope entirely, since no scan route to it exists. A coherent, slower, smaller system — declined
with eyes open rather than found unworkable.
