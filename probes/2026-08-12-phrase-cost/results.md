# Exact multi-token match: bigram terms refuted, positions cheap in bytes, mechanism is the price

**Date:** 2026-08-12 · **Harness:** [`bigrams.py`](bigrams.py) · **Machine:** WSL2, single-threaded
**Corpus:** real arXiv titles (to 2.4M) and abstracts (to 250k), prefixes in snapshot order.
Postings serialised (pyroaring, hybrid singleton encoding), dictionaries front-coded, position
bytes exact delta-varints over the real token streams. Raw output:
[`run-output.txt`](run-output.txt).

## Results

B per entity; UNI is the token index as designed (hybrid postings + dictionary), BI is the whole
additional cost of indexing adjacent token pairs as terms, POS is the exact position payload,
TF is one u8 per (term, doc) entry:

| field | scale | tok/e | uni vocab | **UNI** | bi vocab | bi singl. | **BI** | **TF** | **POS** |
|---|---|---|---|---|---|---|---|---|---|
| title | 250,000 | 10.3 | 57,033 | 21.9 | 617,645 | 67.1% | 60.1 | 9.9 | 10.3 |
| title | 1,000,000 | 10.5 | 124,758 | 21.9 | 1,804,827 | 64.8% | 59.8 | 10.1 | 10.5 |
| title | 2,400,000 | 10.8 | 269,801 | 22.6 | 3,795,775 | 65.0% | 61.7 | 10.4 | 10.8 |
| abstract | 100,000 | 128.0 | 101,044 | 139.0 | 2,097,331 | 66.2% | 518.4 | 78.4 | 140.9 |
| abstract | 250,000 | 131.3 | 165,520 | 135.4 | 3,987,568 | 63.6% | 500.0 | 80.0 | 145.1 |

**Bigram terms are refuted as the phrase mechanism, by 3.5–6×.** The bigram vocabulary explodes
(3.8M distinct pairs over 2.4M titles, ~65% singletons at every scale and both shapes), so its
postings amortise nothing and its dictionary alone costs 9–112 B/e. At 61.7 B/e on titles, adding
bigrams to the 22.6 B/e unigram index costs back the entire storage win over the 83.6 B/e flat
column. Recorded so the "cheap adjacent-pair terms" idea is not re-derived.

**Position payloads are the cheap option in bytes** — 10.8 B/e titles, 145 abstracts, roughly one
byte per token occurrence (title positions are all one-varint; abstracts spill into two) — and
they subsume TF (a term's frequency is its position count), so a scored `match` and an exact
phrase want the same payload. **The real price of positions is the mechanism, not the bytes**: a
Roaring bitmap carries no payload, so positions need a lane beside the posting. The shape that
preserves the design's posting-is-a-bitmap rule: a per-term sidecar array aligned to the bitmap's
rank order — entry *k* belongs to the posting's *k*-th entity — leaving every existing consumer
(match, aggregates, the candidate intersection) reading the bitmap untouched, with phrase and
scoring walking survivors' ranks sequentially. Not designed here; the design document prices it.

**A scored `match` (TF only, no positions) costs +10.4 B/e on titles and +80 on abstracts**, on
top of which BM25 needs a per-entity field-length (1–2 B/e) and per-query masked document
frequencies, which are one `and_cardinality` per query term — the measured-cheap class.

## What this does not measure

- **Query cost** of any option: verify-against-record is result-bound (169 µs per block read,
  measured in `2026-08-12-string-storage/`); position-merge cost is unmeasured.
- **Tokeniser sensitivity**: the probe convention is `[a-z0-9]+` lowercase. English under a
  UAX #29 segmenter shifts these figures modestly; a CJK corpus under dictionary segmentation is
  a different vocabulary shape entirely and is NOT measured — flagged in the design as an owed
  measurement before the text family is normative.
- The 10⁹ extrapolation caveat of the parent campaigns applies unchanged.
