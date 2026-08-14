# The text index on disk: the model holds for titles, and prose length drives it

**Date:** 2026-08-13 · **Harness:** [`textbytes/`](textbytes/) · **Machine:** WSL2, AMD Ryzen 9
5900X · **Corpus:** the arXiv snapshot (v296) read in entity order — `title` and `abstract`, the
two real prose shapes it carries · **Raw:** [`raw/textbytes.txt`](raw/textbytes.txt)

`records-and-search.md` §4.4 quotes the token index at **23.6 B/entity** from
[`string-storage`](../2026-08-12-string-storage/) arm 3. That figure is an *accounting* — a sum of
modelled per-posting costs — and a model of a format is not the format: the same gap cost the
dictionary campaign 2.0–2.9 B/key when the decodable bytes were finally written. This replaces it
with `write_sorted_dict` and `write_postings`' own output, through the analyser that actually
indexes.

## Results

| column | entities | terms | flat B/e | dict B/e | postings B/e | **index B/e** | ratio |
|---|---|---|---|---|---|---|---|
| `title` | 250,000 | 60,855 | 74.90 | 1.39 | 21.58 | **22.97** | 3.26× |
| `title` | 1,000,000 | 134,092 | 77.17 | 0.76 | 20.99 | **21.75** | 3.55× |
| `title` | 2,400,000 | 285,046 | 80.51 | 0.67 | 21.92 | **22.58** | 3.56× |
| `abstract` | 250,000 | 198,145 | 825.12 | 4.25 | 137.03 | **141.28** | 5.84× |
| `abstract` | 1,000,000 | 476,423 | 906.29 | 2.56 | 142.27 | **144.82** | 6.26× |
| `abstract` | 2,400,000 | 991,126 | 1,025.36 | 2.28 | 156.15 | **158.43** | 6.47× |

**1. The model holds for titles, and is slightly pessimistic.** Measured **21.75–22.97 B/entity**
against arm 3's modelled 22.7–23.6. §4.4 quotes the top of that band, so the design's number is
conservative by up to 8% rather than optimistic — the safe direction, and the first time this
family's headline has been checked against bytes on disk rather than against an accounting.

**2. The scale-cancellation arm 3 predicted is real.** The index holds within 5% across a 9.6×
range while the term count grows 4.7× and the flat column *grows* 7.5% (later arXiv titles are
longer). A head token's posting densifies as a tail token's spreads, and the two cancel.

**3. The dictionary is not where the bytes are — the postings are, by 30×.** Front coding puts the
term dictionary at **0.67 B/entity** on titles at 2.4M, falling as scale grows because a fixed
vocabulary amortises over more entities. Any future work on this format's size belongs in the
postings; the dictionary is already free.

**4. ⊘ The per-entity figure is *title-shaped*, and the design does not say so.** An `abstract` is
~13× the bytes of a title and its index is **158.43 B/entity — 7× a title's**. The ratio to flat
improves (6.47× against 3.56×, because a longer document repeats more head tokens) but the
absolute cost does not: §4.4's "~59 GB at 10⁹" is a titles number, and the same corpus of abstracts
would be ~158 GB for the index alone. **The index cost scales with prose length, not with entity
count alone**, which is the caveat a reader sizing a deployment needs and which no figure in the
corpus currently carries.

## What this does not measure

- **The flat column is computed, not written.** `utf8` is retired and its writer is deleted, so the
  `flat B/e` column here is value bytes plus a 4-byte Arrow offset per row, calculated rather than
  measured. It lands 3 B/entity below arm 3's 83.6 at 2.4M, which is consistent with that campaign
  charging a wider offset; the ratio is the robust half of the comparison and the absolute flat
  figure should be quoted from arm 3, not from here.
- **Nothing at 10⁹, and no scale trend past 2.4M.** The three scales are within a 9.6× range and
  every 10⁹ figure derived from them is a multiplication.
- **Not the blob.** A text column's values live in the record blob and are accounted separately
  (§3); this is the index alone, which is what §4.4's 23.6 was.
- **One analyser.** `unicode/icu4x-2.2/p1`. A different analyser is a different token stream and
  therefore a different index size; that is what decision 0070's per-column identity records.
