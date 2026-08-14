# A keyword column's four layouts, and the list mechanism re-run on real values

**Date:** 2026-08-12 · **Harness:** [`keyword.py`](keyword.py) · **Machine:** WSL2, single-threaded
**Corpus:** real arXiv columns from the Kaggle snapshot (v296), read as prefixes in snapshot order
(= submission order = entity order, `probes/dataset.md` §5 rule 1): `id` (a fully unique
identifier), `submitter` (a repeat-heavy short name), `doi` (a sparse, near-unique identifier),
`authors_parsed` surnames (a keyword list), `categories` (a category list).

Commissioned by the records-and-search design, to settle two of its open questions: what a
**keyword** column (short exact-match strings, no vocabulary) should be stored as, and the re-run
of `2026-08-12-filter-placement/` arm 3 on real values that
`2026-08-12-record-and-searchability.md` §4 asked for before the authors mechanism is fixed.
Postings are built with pyroaring and **serialised, not modelled**, as in both parent campaigns.

## Results

### 1. A sorted dictionary plus an ordinal column beats every other keyword layout, 2.9–4.4×

B per present entity at 2.4M (the trend is flat from 250k — read the harness output for all three
scales):

| column | distinct | singletons | flat | dict | roaring | hybrid | zstd record | **FLAT** | **DICT+C** | **IX+REC** |
|---|---|---|---|---|---|---|---|---|---|---|
| `id` | 2,400,000 | 100% | 17.8 | 2.1 | 18.0 | 4.1 | 8.6 | 17.8 | **6.1** | 14.8 |
| `submitter` | 542,489 | 39.4% | 22.3 | 1.7 | 9.7 | 8.5 | 15.7 | 22.3 | **5.7** | 25.8 |
| `doi` | 1,024,837 | 99.8% | 33.2 | 3.7 | 18.0 | 4.2 | 15.7 | 33.2 | **7.7** | 23.6 |

The three whole layouts: **FLAT** is the shipped `utf8` column (bytes + i64 offsets), searched by
the text scan. **DICT+C** is a front-coded sorted dictionary plus one u32 ordinal per present
entity — the record is an ordinal lookup, `eq`/`prefix` resolve in the dictionary and become an
ordinal-range test inside the **fixed-width** scan, and `contains` is a byte scan over the
dictionary itself. **IX+REC** is the dictionary plus per-term postings plus a 256 KiB-zstd
block-compressed record — the layout the token index uses for prose.

**DICT+C wins on every column, and the win has three independent sources**: front coding compresses
the identifier's shared prefixes 4.7× (`id`: 9.8 → 2.1 B/key — arXiv IDs share date prefixes),
interning removes the repeats (`submitter`: 542k distinct in 2.4M), and a u32 ordinal replaces an
i64 offset. IX+REC — the right layout for prose — is the **wrong** layout here: a singleton-heavy
vocabulary has no repetition for postings to amortise, and the compressed record duplicates bytes
the dictionary already holds. On `submitter` IX+REC is *worse than the flat column*.

**⊘ The front-coding figure is arXiv-ID-shaped.** A key set with no shared prefixes (UUIDs, random
hashes) front-codes to nearly its raw bytes, and DICT+C degrades to ~data + 4 B/entity against
FLAT's data + 8 — still never worse, but the 2.9× headline does not transfer. The `doi` row
(3.7 B/key at 1M distinct) is the least prefix-shared column measured.

### 2. Inline singletons cut a unique vocabulary's postings 4.4×

A serialised Roaring bitmap costs ~18 B for a single entity; a bare u32 costs 4. Storing a
singleton term's one entity inline (one flag bit per term) takes `id`'s postings from 18.0 to
4.1 B/entity and `doi`'s from 18.0 to 4.2. This is what makes per-term postings *affordable* on a
near-unique vocabulary at all — but per §1 they are still not the keyword layout; the hybrid
encoding matters where postings are wanted anyway (lists, and the un-folded tail).

### 3. The list re-run: the synthetic storage inversion does not survive real values

B per entity at 2.4M:

| column | distinct | v/ent | flat joined | CSR+dict | postings+dict (hybrid) | CSR + postings |
|---|---|---|---|---|---|---|
| `surnames` | 424,168 | 4.75 | 41.9 | 23.7 | **20.1** | 43.1 |
| `categories` | 156 | 1.78 | 23.3 | 11.1 | **3.3** | 14.5 |

Arm 3 of `2026-08-12-filter-placement/` measured CSR at 24.0 B/entity against postings at 32.3 on
a *synthetic* surname-shaped column and read it as a storage inversion at large sparse
vocabularies. On **real** surnames the postings cost 20.1 (hybrid; 20.9 Roaring-only, agreeing
with `2026-08-12-string-storage/`) against CSR's 23.7 — **the inversion is gone**, for two reasons
the synthetic column lacked: the real vocabulary's head is fatter (33% of entries in the top
1,000), and the hybrid encoding removes the singleton tail's per-bitmap overhead. Postings now win
or tie **both** axes on both list shapes measured — arm 3's timing verdict (postings 10–40×
faster in every cell, under a skew *less* favourable to them) already transferred.

What keeps CSR in the design regardless is not this comparison: postings alone cannot answer
`entity → values`, so the record must exist in some form, and CSR-of-ordinals is that form. The
choice this table settles is what serves the *filter*: postings, on both axes.

### 4. The compressed record, for reference

256 KiB zstd blocks reach 2.1× on `id` and 2.1× on `doi` (against flat including offsets) — below
arm 1 of `2026-08-12-string-storage/`'s 2.44× on titles, because short identifiers give zstd less
context. Under DICT+C no keyword column needs it: the ordinal column plus dictionary *is* the
record, at less than the compressed blob's own cost.

## What this does not measure

- **Timing.** Every layout's scan and intersect constants are taken from
  `probes/2026-08-08-filter-layout/` and `2026-08-12-filter-placement/`, which measured them on
  the same shapes (u32 codes, Roaring postings). Nothing here re-times them. The one residual:
  arm 3's list timing was under synthetic skew; real skew moves postings' constants in their
  favour (denser head containers), so the 10–40× is a floor in direction, not re-measured.
- **Scale.** 2.4M, trend flat across 9.6×; 10⁹ is a further 400× and the extrapolation carries
  the same ⊘ as the string-storage campaign's.
- **Prefix-poor key sets** (§1's caveat) — no UUID-shaped column exists in this corpus.
- **Multi-scale dictionary growth under ingest** — per-extent dictionaries are the design's
  answer; their coalesce cost is a mechanism question, not a storage one, and is priced in the
  design from the authorisation dictionary's measured merge.
