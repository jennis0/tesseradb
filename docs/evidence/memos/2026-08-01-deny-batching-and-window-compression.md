# Deny batching, and what a commit window collects

**Date:** 2026-08-01 · **Build:** the write path with both lanes group-committing · **Corpus:** 250 000-item `categories-subclass` fixture unless stated

Two measurements taken while group-committing the deny lane and building the fragmentation figure.
Both are counter-based where the claim rests on counters, which is what makes them usable on a box
that was not always quiet.

---

## 1. Deny batching

One `POST /control/changes` request carrying N suppressions. `wal_fsyncs` read from
`GET /control/status`, same harness on both sides.

| N | fsyncs before → after | elapsed before → after | denies/s before → after |
|---|---|---|---|
| 10 | 10 → **1** | 33.7 ms → 16.3 ms | 297 → 613 |
| 100 | 100 → **2** | 321 ms → 20.8 ms | 311 → 4 814 |
| 1 000 | 1 000 → **1** | 3.289 s → 31.9 ms | 304 → **31 314** |
| 10 000 | — | → 187 ms | → **53 488** |

**The fsync counts carry the claim; the box was not quiet, so the rates are reported as measured
and no latency budget is claimed.** The before column reproduces the ~310 denies/s that prompted
the work to within 3%, which is what makes the two columns comparable.

At the previous rate a million-item revocation would have taken about fifty-three minutes.

Three costs were removed, and only the first was anticipated: the per-item fsync; an overlay clone
per item, which made a batch quadratic in its own size; and per-item external-ID resolution, which
became the dominant term once the other two were amortised and now uses the batched resolution the
ingest path already had.

---

## 2. What a commit window collects

`run_ratio` is mean posting run length over the run length a random assignment of the same density
would give, so 1.00 is no better than random and larger is better. It is the **entity-space**
quantity — posting run length — not the row-space mask run ratio, which normalises the same way
over a different set and is not comparable with it.

### Against the full-sort ceiling

4 000 rows, 20 signatures, two terms per row, one corpus assigned at three scopes:

| scope | `run_ratio` | runs | share of ceiling |
|---|---|---|---|
| grouping disabled | 1.00 | 8 000 | — |
| 500-row window | 41.00 | 176 | **13%** |
| full-corpus sort | 327.36 | 22 | 100% |

### On a real engine, varying in-flight submissions

`tessera-bench ingest-concurrent`, 10 000-row window bound, 100-row submissions, quiet box (load
0.66 under the exclusive benchmark lock):

| in-flight submitters | entries per window | postings | runs | `run_ratio` |
|---|---|---|---|---|
| 1 | 1.00 | 1 000 | 40 | **16.91** |
| 4 | 2.50 | 4 000 | 64 | **41.91** |
| 16 | 13.33 | 16 000 | 48 | **222.42** |

Same corpus, same window bound, same server: **a thirteenfold difference in collected compression,
from submission overlap alone.** No other figure on the status endpoint shows it — segment counts,
watermark and overlay size all stay healthy while union cost climbs.

### What these figures do not say

- **`run_ratio` is window-scoped.** It compares one allocation run against a random assignment of
  *that same window*, so it is identically 1.00 at a one-row window for every corpus, and
  fragmentation *between* allocation runs is invisible to it by construction. It is not comparable
  with the probes' full-corpus 8.9–36.7×.
- **The window collects no container-count win.** Containers spanned fall only once a window
  exceeds roughly 2¹⁶ divided by term density — about 65 000 rows at full density and 3.3 million
  at the corpus's own 2%, against a window bound of 10 000. Since bitmap operations cost
  O(containers touched) rather than O(cardinality), this is compression of posting **storage**, not
  of authorisation latency.
- **Concurrency is what was varied, not what matters.** Rows per window is the product of
  submission size and in-flight count; this arm held submission size at 100 against a 10 000-row
  cap. Which input dominates is **assumed, not measured** — see the ingest-profiling issue.
