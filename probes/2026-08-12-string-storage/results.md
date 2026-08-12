# A string column's bytes: the flat column, the encodings that shrink it, and the index that beats both

**Date:** 2026-08-12 · **Harness:** [`schemes.py`](schemes.py), [`tokens.py`](tokens.py) ·
**Machine:** WSL2, single-threaded
**Corpus:** real arXiv titles and author surnames from the Kaggle snapshot (v296). `schemes.py`
reads 300,000 titles as four contiguous blocks; `tokens.py` reads the first 2,400,000 records in
snapshot order, which is submission order, which is entity order.

Commissioned by a question with a simple form — *the column is 83 B/value against a source that
stores the same text in 34, so what are we doing wrong* — and the answer changed twice under
measurement. The memo reading against this is
[`2026-08-12-record-and-searchability.md`](../../docs/evidence/memos/2026-08-12-record-and-searchability.md).

## Results

**The headline is arm 3, not arm 1: a token index is 3.5× *smaller* than the flat column it would
replace, and the ratio is stable in scale.** Every encoding in arms 1 and 2 is an attempt to shave
a structure that should not be the structure.

### Arm 3 — a token index against the flat column (`tokens.py`)

Roaring postings built and **serialised**, not modelled. Scales are prefixes in entity order
(`probes/dataset.md` §5 rule 1). B/entity throughout.

**title, tokenised (prose):**

| scale | vocab | tokens/entity | singletons | top-1000 share | postings | dict | **index** | flat | ratio |
|---|---|---|---|---|---|---|---|---|---|
| 250,000 | 57,033 | 9.9 | 45.8% | 69.9% | 22.44 | 0.88 | **23.33** | 78.06 | 3.35× |
| 1,000,000 | 124,758 | 10.1 | 48.2% | 68.1% | 22.23 | 0.47 | **22.70** | 80.27 | 3.54× |
| 2,400,000 | 269,801 | 10.4 | 59.5% | 66.3% | 23.12 | 0.42 | **23.55** | 83.56 | 3.55× |

**authors, per surname (names):**

| scale | vocab | names/entity | singletons | top-1000 share | postings | dict | **index** | flat | ratio |
|---|---|---|---|---|---|---|---|---|---|
| 250,000 | 100,777 | 3.5 | 33.8% | 24.4% | 17.22 | 1.73 | **18.95** | 34.20 | 1.80× |
| 1,000,000 | 231,447 | 4.3 | 33.3% | 26.4% | 18.88 | 0.97 | **19.86** | 40.96 | 2.06× |
| 2,400,000 | 424,168 | 4.7 | 34.1% | 33.1% | 20.18 | 0.73 | **20.91** | 42.99 | 2.06× |

**The two scale effects cancel, which is the result that makes the number usable.** A head token's
posting densifies as the corpus grows (cost per entry falls toward the bitmap-container rate); a
tail token's entries spread across more container keys (cost per entry rises toward one container's
overhead each). Titles' singleton fraction climbs 45.8% → 59.5% over a 9.6× scale range while the
index holds at 22.7–23.6 B/entity. The flat column meanwhile *grows*, because later arXiv titles are
longer.

**Prose is a different vocabulary shape from names, and the top-1000 share is where it shows:**
66–70% of title postings entries fall in a thousand tokens, against 24–33% for surnames. That fat
head is what amortises postings, and it is what
[`probes/2026-08-12-filter-placement/`](../2026-08-12-filter-placement/) arm 3's surname-shaped
result could not speak to.

**⊘ The extrapolation to 10⁹ is not measured.** The trend is flat across 9.6×; 2.4M → 10⁹ is a
further 400×. Nothing here says the cancellation continues to hold there, and the vocabulary keeps
growing (57k → 270k over the same range) rather than saturating.

**⊘ The authors row does not settle CSR versus postings, and does not contradict arm 3.** That arm
measured a *synthetic* surname-shaped column (mean 5 values, domain 400,000) at CSR 24.0 B/entity
against postings 32.3; this measures *real* surnames at mean 4.7 and domain 424,168 and gets
postings at 20.9. The parameters line up and the numbers do not, because the synthetic values'
width is not real surnames' width. **Settling it means re-running arm 3's harness on real values.**
Until then, treat authors' mechanism as open.

### Arm 1 — block compression, on the flat column (`schemes.py`)

Ratio and decompression cost move in opposite directions across block size, and no size makes both
acceptable:

| block | ratio | B/value stored | decomp GB/s | ns/value, block read | ns/value, one value |
|---|---|---|---|---|---|
| 4 KB | 1.87 | 48.2 | 0.59 | 127.4 | 6,955 |
| 16 KB | 2.12 | 43.4 | 0.99 | 75.8 | 16,546 |
| 64 KB | 2.25 | 41.3 | 1.46 | 51.5 | 44,895 |
| 256 KB | 2.44 | 38.8 | 1.54 | 48.6 | 169,468 |
| 1 MB | 2.43 | 38.9 | 1.72 | 43.6 | 594,444 |

Against `filter-index.md` §2's measured scan constants — **1.7 ns/entity** contiguous `contains`,
**96 ns** scattered — the best ratio (2.44×) costs 48.6 ns per value *with the whole block read*,
29× the contiguous scan. 4 KB blocks limit the random-access damage but reach only 1.87× at
127 ns/value, worse than the uncompressed scattered figure. Single-value random access is 7–594 µs
against 96 ns.

zstd decompresses at 1.5–1.7 GB/s here while filter-index §2 records the uncompressed scan already
at ~6 GB/s and at bandwidth: **compression makes the column CPU-bound where it was bandwidth-bound,
at a quarter of the rate it already had.**

**This result is what makes an exactly-answering index a *storage* argument and not only a speed
one.** A column that must be scanned cannot be compressed. A column no query touches can be, at
2.44×.

### Arm 2 — per-value encodings that keep random access (`schemes.py`)

| scheme | B/value | vs today |
|---|---|---|
| flat column: bytes + i64 offset (today) | 83.0 | 1.00× |
| + `u16` length, per-block i64 base | 77.0 | 1.08× |
| FSST-like symbol table | 53.3 | 1.56× |
| front-coded sorted dictionary + ordinal | 66.5 | 1.25× |

The FSST figure is a **floor** — the table construction here is a few counting rounds rather than
the published one, which reports ~2× on text. Decode *speed* is not measured and is not claimed: a
Python inner loop says nothing about a C implementation.

**Front coding is weak on this corpus for a specific reason: 1.19× on the sorted dictionary, because
titles share no prefixes.** Do not carry that forward as front coding's number in general — the
surname dictionary in arm 3 front-codes to 0.73 B/entity at 424,168 keys.

**Dictionary encoding contributes nothing to a near-unique text column, measured**: 299,846 distinct
values in 300,000, and parquet's dictionary saves nothing over plain zstd (33.9 against 33.8). That
transfers to any scheme hoping to intern this column and does *not* transfer to a category, where
interning is the design.

**Parquet, the reference the question compared against**: 33.8 B/value with zstd, 2.46× smaller than
the flat column — and no random access, so not an option for a scanned column.

## What this does not measure

- **Query cost of the index.** Arm 3 is storage only. What a token lookup plus a Roaring intersect
  costs against `M_auth` is the category-postings shape, measured elsewhere, but not measured here
  for a 270k-token vocabulary.
- **FSST decode throughput**, and its interaction with the four string operators. `eq` and `in`
  compare in the compressed domain, `contains` cannot — that is argued from published mechanics in
  the memo, not measured. Settling it means encoding the 2.42M real titles and re-running
  `probes/2026-08-08-filter-layout/` arms 6 and 12.
- **Tokenisation as a semantic choice.** Lowercase, split on non-alphanumerics, no stemming, no
  stopword list. Stopwords would cut the head, so their absence makes the postings figure
  conservative rather than flattering — but a real tokeniser is a conformance surface and this is
  not a proposal for one.
