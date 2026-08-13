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
| 400,000 | 265,867 | singleton (`hamlitonians`) | 1 | 562 / 549 | 741 / 728 | 803 | 1.32× | **+179 ns** |
| 400,000 | 265,867 | small (`cxb`) | 22 | 555 / 546 | 1,439 / 1,422 | 1,646 | 2.59× | +876 ns |
| 400,000 | 265,867 | mid (`newly`) | 2,361 | 556 / 544 | 4,216 / 4,179 | 2,859 | 7.58× | +3,635 ns |
| 400,000 | 265,867 | head (`this`) | 221,655 | 531 / 523 | 21,863 / 21,140 | 21,013 | 41.19× | +20,617 ns |
| 1,000,000 | 478,282 | singleton (`hanzer`) | 1 | 525 / 516 | 684 / 671 | 741 | 1.30× | **+155 ns** |
| 1,000,000 | 478,282 | small (`vinylidene`) | 21 | 526 / 518 | 1,928 / 1,910 | 2,315 | 3.67× | +1,392 ns |
| 1,000,000 | 478,282 | mid (`ti`) | 2,392 | 531 / 519 | 4,834 / 4,817 | 3,632 | 9.11× | +4,298 ns |
| 1,000,000 | 478,282 | head (`new`) | 158,126 | 533 / 522 | 47,110 / 46,753 | 46,202 | 88.40× | +46,231 ns |

**1. The channel is real at every stratum, and the separation is far outside this machine's noise.**
Median and minimum differ by under 2% in every cell — the singleton arms spread ~13 ns run to run
against a separation of 155–179 ns. Locally, one call distinguishes the two answers. This is a
positive result: the channel decision 0067 accepted on structural grounds is measurable, and the
smallest case is the one an adversary most wants, since a singleton term is *"does any document at
all contain this string"*.

**2. What sets the cost is the term's posting, not the corpus's vocabulary.** The absent arm is
flat at **516–562 ns across a 1.8× vocabulary growth** — a failed binary search over a front-coded
dictionary grows logarithmically and the growth is invisible at this range — while the hidden arm
tracks the posting it decodes and intersects, from 671 ns at one carrier to 46.8 µs at 158k.
**⊘ This is not what the corpus implies.** C25 and §4.4 frame the channel as a property of the
family and of vocabulary scale, which is why the issue asked for it "at a ≥250k-term vocabulary";
the measurement says vocabulary size is the term that does *not* matter, and the answer to "how
loud is this channel" is "as loud as the term is common".

**3. A hidden term can cost *more* than a visible one.** At the mid stratum, 4,216 ns hidden
against 2,859 ns visible (400k), and 4,834 against 3,632 (1M). The hidden arm's candidate is
`everything \ carriers`, which is a more fragmented Roaring bitmap than the whole corpus, so its
intersection touches more containers — the measured cost model exactly (bitmap operations cost
O(containers touched)). A defence that tried to make the hidden case *look like* the visible case
would therefore have to make it slower, not faster.

**4. Network observability differs by three orders of magnitude across the strata, and this is the
practical finding.** A singleton's 155 ns is far below LAN round-trip jitter, so distinguishing it
remotely takes many samples and a stable path. A head term's 20–46 µs is comparable to a LAN RTT's
own variation and is separable in a handful of requests. So the channel's severity is a function of
*which* word is asked about: rare words — the ones whose existence is most revealing — are the
hardest to read, and common words — whose existence in a corpus of a million abstracts is not a
secret — are the easiest. That inverse relation is the strongest thing that can be said in the
channel's favour, and it is measured rather than argued.

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
