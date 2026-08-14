# Hidden versus absent: the channel is the posting's size, not the vocabulary's

**Date:** 2026-08-14 · **Harness:** [`hiddentiming/`](hiddentiming/) · **Machine:** WSL2, AMD Ryzen
9 5900X · **Corpus:** the arXiv snapshot (v296), abstracts, at 400k and 1M entities — 265,867 and
478,282 terms, both past the ≥250k the issue asks for · **Raw:**
[`raw/hiddentiming.txt`](raw/hiddentiming.txt)

Decision 0067 accepted a timing channel between *"this word is in the corpus but you may see none
of the documents carrying it"* and *"this word is not in the corpus"*. Appendix C row **C25**
registers it with no figures. This campaign measures it through `FilterColumns::resolve` — the
entry a served request takes — and it is the third campaign on this branch to state that gate
explicitly, because the `utf8` retirement fence measured a route that did not exist and the
`contains` recovery probe repeated the mistake.

## Results

Nanoseconds per `resolve`, median and minimum over 9 independent runs of 2,000 calls. The *absent*
arm queries a token no document carries; the *hidden* arm queries a real term against a candidate
holding none of its carriers; the *visible* arm is the same term against a candidate that does.

| entities | terms | stratum | posting | absent med/min | hidden med/min | visible | hidden/absent | separation |
|---|---|---|---|---|---|---|---|---|
| 400,000 | 264,919 | singleton (`halfcar`) | 1 | 498 / 489 | 524 / 511 | 681 | 1.05× | **+22 ns** |
| 400,000 | 264,919 | small (`decentralised`) | 22 | 503 / 493 | 794 / 790 | 1,583 | 1.58× | +297 ns |
| 400,000 | 264,919 | mid (`psi`) | 2,362 | 501 / 489 | 2,325 / 2,306 | 2,664 | 4.64× | +1,816 ns |
| 400,000 | 264,919 | head (`this`) | 221,655 | 508 / 501 | 8,059 / 7,987 | 19,485 | 15.85× | +7,485 ns |
| 1,000,000 | 476,423 | singleton (`hammingdistance`) | 1 | 478 / 471 | 718 / 701 | 913 | 1.50× | **+230 ns** |
| 1,000,000 | 476,423 | small (`wads`) | 21 | 475 / 468 | 594 / 587 | 1,611 | 1.25× | +119 ns |
| 1,000,000 | 476,423 | mid (`indicators`) | 2,392 | 474 / 468 | 2,917 / 2,885 | 3,821 | 6.15× | +2,417 ns |
| 1,000,000 | 476,423 | head (`new`) | 158,126 | 477 / 466 | 11,450 / 11,364 | 43,363 | 24.02× | +10,898 ns |

**1. The channel is real, and quieter than the route that first measured it.** Separation runs
**+22 ns to +10.9 µs**, a ratio of 1.05× to 24×. The appendix below is the A/B: the first pass at
this campaign measured the same channel at +155 ns to +46 µs and 1.30× to 88×, against a route
that materialised each term's whole corpus-wide posting before narrowing it. Reading those arms is
what prompted the rewrite; these are the shipped route's own figures and they supersede the others
everywhere, including in Appendix C.

**2. What sets the cost is the term's posting, not the corpus's vocabulary.** The absent arm is
flat at **466–508 ns across a 1.8× vocabulary growth** — a failed binary search over a front-coded
dictionary grows logarithmically and the growth is invisible at this range — while the hidden arm
tracks the postings it reads. **⊘ This is not what the corpus implied.** C25 and §4.4 framed the
channel as a property of vocabulary scale, which is why the issue asked for it "at a ≥250k-term
vocabulary"; vocabulary size is the term that does *not* move, and the answer to "how loud is this
channel" is "as loud as the term is common".

**3. A hidden term is now consistently *cheaper* than a visible one, and the sign is the
rewrite's** — it reversed. At every stratum the hidden arm is below the visible one (11.5 µs
against 43.4 at the head, 2.9 against 3.8 at mid), because the running set narrows to nothing and
every step after that is trivial; the pre-rewrite route had the hidden case *more* expensive at
mid, its excluding candidate being the more fragmented bitmap. What this changes for the register
is the shape of what leaks: the loud comparison is no longer hidden-against-absent but
**hidden-against-visible**, and that one distinguishes *how much of a term's carriers this
principal can see* — a quantity about their own visible set, not about anyone else's, and one the
answer's own cardinality already gives them.

**4. Network observability spans three orders of magnitude, and that is the practical finding.**
A singleton's 22–230 ns is deep inside LAN round-trip jitter, so distinguishing it remotely takes
many samples and a stable path. A head term's 7.5–10.9 µs is at the edge of a LAN RTT's own
variation. So the channel's severity runs *opposite to its value*: rare words — the ones whose
existence is revealing — are hardest to read, and common words, whose presence in a million
abstracts is not a secret, are easiest. That inverse relation is the strongest thing that can be
said in the channel's favour, and it is measured rather than argued.

**5. Multi-word conjunctions**, over the corpus's commonest terms — the least selective query a
corpus of this size admits, and therefore the worst case:

| entities | words | postings read | ns med/min | answer |
|---|---|---|---|---|
| 400,000 | 1 | 393,834 | 3,959 / 3,865 | 393,834 |
| 400,000 | 2 | 785,063 | 77,167 / 76,895 | 386,737 |
| 400,000 | 4 | 1,500,737 | 44,052 / 43,916 | 317,505 |
| 400,000 | 8 | 2,799,206 | 81,475 / 80,264 | 165,162 |
| 1,000,000 | 1 | 986,409 | 8,247 / 8,050 | 986,409 |
| 1,000,000 | 2 | 1,967,520 | 152,057 / 149,165 | 970,877 |
| 1,000,000 | 4 | 3,787,361 | 98,465 / 97,961 | 820,969 |
| 1,000,000 | 8 | 3,787,361 | 121,103 / 120,810 | 453,807 |

**Cost tracks the answer, not the token count.** Four words cost *less* than two — 98 µs against
152 — because the running set has narrowed from 971k entities to 821k by then and every later
intersection touches fewer containers. The expensive step is the second word, where two
near-universal sets meet; past it the query gets cheaper as it gets longer. A conjunction is
therefore bounded by its most selective term, not by how many terms it names, which is the
property the accumulator buys and the reason a long query is not a denial-of-service shape on its
own. (It says nothing about *repeated* clauses across an expression tree — see issue #121, whose
amplifier is unaffected by any of this.)

## Appendix: the conjunction rewrite, measured both ways (2026-08-14)

The figures above are the shipped route's. This appendix is why they are not the figures this
campaign first produced.

The first pass measured a route that materialised each term's **whole corpus-wide posting** on the
heap and then intersected it down to the part the principal may see. Reading those arms showed the
hidden case paying for a set it built only to discard, which is the shape a masked index is in most
of the time. Three changes followed, and this is the A/B that settled them — both arms run in one
session on one machine, at 1M entities.

1. **A running set, narrowed token by token**, starting from the candidate, instead of every
   token's answer held at once and intersected at the end (owner's suggestion).
2. **Intersecting against the mapped posting *view*** rather than an owned copy of it, so the
   corpus-wide set is never assembled: peak becomes O(answer) instead of O(posting), and what
   remains resident is file-backed page cache rather than anonymous heap.
3. **Narrowing the running set in place**, which the measurement forced — see below.

| query | before | after | change |
|---|---|---|---|
| common word (158k carriers), **candidate holds none of them** | 41,096 ns | 11,592 ns | **−72%** |
| uncommon word (21), candidate holds none | 1,556 ns | 601 ns | **−61%** |
| mid word (2,392), candidate holds none | 4,773 ns | 3,066 ns | −36% |
| rare word (1), candidate holds none | 865 ns | 716 ns | −17% |
| **8-word conjunction, all head words** | 317,791 ns | 125,105 ns | **−61%** |
| 4-word conjunction, all head words | 150,692 ns | 102,889 ns | −32% |
| uncommon word, candidate holds its carriers | 1,935 ns | 1,561 ns | −19% |
| rare word, candidate holds its carrier | 916 ns | 849 ns | −7% |
| 1-word, head term, whole corpus visible | 8,248 ns | 8,385 ns | +2% |
| mid word, candidate holds its carriers | 3,694 ns | 3,912 ns | +6% |
| 2-word conjunction, both head words | 145,851 ns | 156,842 ns | **+8%** |
| head word, candidate holds its carriers | 38,934 ns | 41,983 ns | **+8%** |

**1. The masked case is where the saving is, which is the case this system is for.** A principal
who may see none of a term's carriers pays 11.6 µs where they paid 41.1 — the old route's whole
cost there was building a set to discard it. The saving tracks how much of the posting the
candidate excludes, so it is largest exactly where the mask is doing the most work. **It is also
what re-measured C25 downwards**: the channel's headline separation fell from +46 µs to +10.9 µs
because the arm being measured got faster, not because the channel closed.

**2. ⊘ Two cells are ~8% slower and are not spun.** A two-word query over the corpus's two
commonest words, and a single head term against a fully-visible corpus. Both are the shape where
the answer is nearly the corpus: there is nothing for a running set to narrow, and the old route's
per-token run-optimise gave its operands a faster representation than the new route's single
optimise at the end. 146 → 157 µs on the worse of them, on the least selective query a million-
document corpus admits. Recovering it means re-optimising an intermediate the next intersection is
about to shrink, which costs more on every longer query; not taken.

**3. The second and third changes were each forced by a measurement that contradicted the
reasoning.** Both are worth recording because both looked obviously right:

- Starting the running set as `candidate.clone()` reads well and cost **~800 ns per query** — most
  of a one-word query's entire budget, at a million entities. The candidate is borrowed for the
  first step instead.
- Chaining the *out-of-place* narrowing — the natural expression of change 1 — measured **19–51%
  slower than the route it replaced** on the 4- and 8-word conjunctions, the allocation per step
  swamping the memory win. In place, the same queries are 32% and 61% *faster*. The intermediate
  version would have shipped a regression while carrying a correct-sounding argument for why it
  was an improvement.

**4. The memory half is argued, not measured.** Peak resident is O(answer) rather than O(posting)
by construction — the mapped view is never copied — but no figure here reads RSS. What would
settle it is the fold's `resident_set` shape applied to a request, which no harness takes.

## What this does not measure

- **Not over a network.** Every figure is an in-process call on one machine. The claim in finding 4
  about remote observability is a comparison against typical LAN jitter, not a measurement of it —
  a campaign that drove the HTTP surface would be the one to settle it.
- **Not the flushed-layer shape.** One layer, the base. A corpus with unfolded flush extents
  resolves the token in each layer's dictionary, so the absent arm's cost multiplies by the layer
  count while the hidden arm's does not — which narrows the ratio. The direction is known; the
  magnitude is not measured.
- **Not m-of-n.** `minimum_should_match` counts over the union of the per-token answers, so its
  cost model differs; only the plain conjunction is here.
- **Nothing at 10⁹.** The largest vocabulary measured is 478,282 terms. Finding 2 says the
  vocabulary term is flat over the range tested, which is an argument for extrapolating the absent
  arm and no argument at all about the hidden one, whose cost is the posting's.
- **One analyser** (`unicode/icu4x-2.2/p1`) and one language mix. A different analyser is a
  different vocabulary and therefore a different posting-size distribution.
- **No mitigation is measured or proposed here.** This campaign reports; it does not rule. What a
  defence would cost — a constant-time dictionary probe, a padded posting read — is not in scope
  and belongs to the owner.
