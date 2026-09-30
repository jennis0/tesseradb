# Value suggestion at 10⁷ values: the index fits in ~100 MB of file, the probe walk fits in 10–100 ms, and the lever is cheap

**Date:** 2026-09-02 · **Harness:** `suggestprobe/` (standalone crate, not a workspace member)
**Raw:** `logs/arm1.log`, `logs/arm2-{zipf,uniform}.log`, `logs/arm3-{zipf,uniform}.log`, `logs/gen-*.log`,
`logs/prep.log`, `logs/arm4-decomp-{1e6,1e7}.log`; `logs/overlapped/` holds a first pass of arms 2–3 that ran concurrently with arm 1 (same
numbers within noise, kept as the repeat).
**Machine:** WSL2 on an AMD Ryzen 9 5900X (12 cores), 47 GB, kernel 6.18.33.2-microsoft-standard-WSL2;
every run single-threaded, one process at a time except where stated. Page cache warm throughout: the
fixtures were written minutes before being read, so **device-cold first-touch latency is NOT measured**.
**Deps:** `fst 0.4`, `croaring 2`, `memmap2 0.9`, `unicode-normalization 0.1`, and path dependencies on
`tessera-filter` (`SortedDictWriter`/`SortedDict`, `ColumnPostings::narrow`), `tessera-authz`
(`PostingsSpool`, `encode_posting`, `PostingsReader`) and `tessera-types` — the shipped readers and
writers, not re-implementations.

Measures what `docs/design/value-suggestion.md` §6 modelled from 171 values, at V = 10⁵, 10⁶, 10⁷ values
and N = 10⁸ entities. Every figure below is **measured** unless marked *modelled* or *assumed*.

---

## Results

### Arm 1 — index residency, build and prefix lookup

Fixture: distinct folded GeoNames names (`name` ∪ `asciiname`, 10,132,181 after the fold; 14.9 B/key on
average; 2.20 entries per value once word starts are added — the design's "~4" is for descriptive titles,
and GeoNames names average 1.64 words). Key-only and key+word-start entry sets; one process per cell.

**Bytes at 10⁷ values** (heap = counting-allocator growth across the build; file = on-disk size, mapped
at read; peak RSS is the process's `VmHWM`, which includes the 149 MB of loaded keys and the sort's
scratch):

| Structure | key-only: heap / file | build | key+word-start (22.0M entries): heap / file | build | peak RSS (words) |
|---|---|---|---|---|---|
| (a) `BTreeMap<String,u32>` | **705 MB** / — (70 B/key) | 4.7 s | **1,599 MB** / — (73 B/entry) | 13.1 s | 2,458 MB |
| (b) sorted arena + `u32` offsets + positions | 328 MB† / — | 1.3 s | 671 MB† / — | 2.9 s | 899 MB |
| (b′) arena, word starts as (rank, offset) refs into the key arena | 395 MB† / — | 1.1 s | 490 MB† / — | 1.3 s | 813 MB |
| (c) FST (`fst` 0.4), value = position or run start | 0 / **95.0 MB** (9.5 B/key) | 8.6 s | 134 MB† / **131.7 MB** | 12.6 s | 647 MB |
| (d) front-coded block dict (`SortedDictWriter`, K=16) | 0 / **90.8 MB** (9.1 B/key) | **2.4 s** | 201 MB† / **124.4 MB** | **4.2 s** | 675 MB |

† As built, with `Vec` doubling: the counting allocator reports capacity. Tight figures (*modelled* from
the measured element counts): arena key-only 22.9 B/key = 229 MB; FST side array 88 MB (22.0M × 4 B);
dict side arrays 88 MB positions + 4 B per distinct entry string. Build times are the median of three and
exclude the sort that every structure shares (next paragraph); the FST's 8.6 s is dominated by
automaton construction, the dict's 2.4 s by writing 91 MB through a `BufWriter`.

**The shared cost is the sort, and at 10⁷ it is the build.** Deriving and sorting the entry list —
`sort_unstable_by` over indices with string comparisons through the arena — measured **4.7–6.7 s** for
10⁷ key-only entries and **34–38 s** for 22.0M key+word-start entries (five runs each). At 10⁶ it is
0.3 s / 1.2–1.5 s. The design's "sorting 4×10⁶ short strings is on the order of a second *(assumed)*"
holds at 10⁶ and is **refuted at 10⁷**: the cache-missing indirect comparison sort is 25× the dict
build itself. A parallel sort on the pool, or sorting (hash, index) pairs, is the fix; it is not a
structure question.

**Scaling** (heap or file, words entry set): 10⁵ → 10⁶ → 10⁷ — BTreeMap 15.4 → 156 → 1,599 MB; arena
5.9 → 72 → 671 MB; FST file 2.3 → 18.6 → 132 MB; dict file 2.0 → 16.3 → 124 MB. Bytes per entry are
scale-stable for every structure (the FST's 9.5 B/key at 10⁷ against 12.2 B/key at 10⁶ improves with
scale, as dictprobe's structured namespaces did).

**Prefix-range lookup at 10⁷, key-only, 2000 random prefixes each drawn from real keys** (median / p99,
µs). Every structure returns the same `[lo, hi)` (checksums agree in the log); the BTreeMap has no rank,
so its figure is positioning the iterator and reading the first entry only:

| prefix chars | (a) BTreeMap (start only) | (b) arena | (b′) arena-ref | (c) FST | (d) dict (with its fail-closed checks) |
|---|---|---|---|---|---|
| 1 | 0.15 / 0.56 | 0.22 / 0.47 | 0.24 / 0.79 | 0.65 / 1.47 | 1.15 / 2.45 |
| 2 | 0.48 / 1.79 | 0.94 / 1.48 | 1.27 / 2.05 | 0.55 / 1.65 | 1.85 / 3.70 |
| 3 | 0.88 / 2.04 | 1.00 / 1.54 | 1.33 / 2.37 | 0.40 / 1.56 | 2.17 / 3.77 |
| 4 | 1.11 / 1.84 | 1.05 / 1.83 | 1.54 / 2.92 | 0.69 / 1.49 | 2.70 / 4.25 |
| 8 | 0.94 / 1.68 | 0.95 / 1.59 | 1.52 / 2.76 | 1.20 / 2.30 | 2.27 / 3.43 |

With word-start entries the medians move by under 1 µs (dict 3.7 µs at 3 chars is the largest). **The
lookup is irrelevant to keystroke latency**: it is three orders of magnitude under the walk that follows
it. Range widths at 10⁷ key-only: a 1-char prefix covers a median 486k values (p99 944k), 2 chars 72k,
3 chars 5.9k, 4 chars 655, 8 chars 4.

**Adversarial arm — 32-char random hex, no shared prefixes** (key-only):

| V | BTreeMap heap | arena heap† | FST file (build) | dict file (build) |
|---|---|---|---|---|
| 10⁶ | 87.6 MB | 41.6 MB | 33.0 MB, 33.0 B/key (3.0 s) | 30.5 MB, 30.5 B/key (0.17 s) |
| 10⁷ | 876 MB | 617 MB | 319 MB, 31.9 B/key (**29.6 s**) | 297 MB, 29.7 B/key (**3.5 s**) |

Where keys share nothing the FST stops winning, as dictprobe found: it is ~7% larger than the dict and
8.5× slower to build, with lookups of 1.5–2.2 µs against the dict's 0.9–2.7 µs.

### Arm 2 — keystroke latency on the probe route (design §6.2)

Fixture: V = 10⁷ values over N = 10⁸ entities, written with `encode_posting` (`small_term_threshold` = 32)
through `PostingsSpool` into `postings.arrow` (522 MB), read through `ColumnPostings::open(_, mmap =
true)`; the probe is `narrow(value, candidate).is_empty()` — `column.rs`'s route, allocating only the
intersection. Two distributions:

- **Zipf**, exponent s = 1.0 over popularity ranks, a floor of one member per value (the row that
  minted it), a random 2% of values emptied (the record a suppressed or deleted value leaves), ranks
  scattered over positions by a random permutation. Head value 10.9M members; 169,039 values above the
  threshold (tag 1, Roaring); 9.63M tag-0 arrays; 200,000 empty.
- **Uniform**, each entity's value drawn uniformly: Poisson(10) members, max 31, so **every record is a
  tag-0 array** and no Roaring view is ever built.

Candidates at 0.01%, 0.1%, 1% and 10% of entities, each as one contiguous range and as a scattered
Bernoulli sample (1,524–1,526 containers). Dense position = rank in folded key order = posting ordinal.

**Per-value probe cost, Zipf, ≈6,300 sampled values per candidate plus the 300 largest** (µs, median /
p99; each value probed once to warm its record and classify it, then timed):

| candidate | hidden, no members | hidden, members disjoint | visible (narrow) | visible via `Bitmap::intersect` |
|---|---|---|---|---|
| 0.01% contiguous | 0.06 / 0.07 | 0.06 / 0.29 | 3.3 / 5.2 | 3.2 / 4.1 |
| 0.01% scattered | 0.06 / 0.09 | 0.11 / 3.4 (max 103) | **105 / 219** (max 442) | **16.6 / 74** |
| 0.1% contiguous | 0.06 / 0.07 | 0.06 / 0.25 | 3.4 / 7.4 | 3.2 / 3.6 |
| 0.1% scattered | 0.06 / 0.10 | 0.12 / 2.5 | **135 / 477** (max 775) | **9.5 / 37** |
| 1% contiguous | 0.08 / 0.10 | 0.09 / 0.29 | 6.8 / 13 | 4.2 / 4.8 |
| 1% scattered | 0.06 / 0.08 | 0.13 / 1.8 | **321 / 1,027** (max 1,860) | **5.9 / 25** |
| 10% contiguous | 0.06 / 0.07 | 0.07 / 0.15 | 0.16 / 26 | 0.15 / 8.8 |
| 10% scattered | 0.06 / 0.08 | 0.09 / 0.25 | 0.21 / 530 (max **7,902**) | 0.21 / 46 (max 54) |

By member count (Zipf, scattered 1%): tag-0 records 0.13 µs hidden / 0.24 µs visible; 33–10³ members
2.9 / 7.8 µs; 10³–10⁵ members — 343 µs; > 10⁵ members — 580 µs median, 1.2 ms p99 (all via `narrow`).
Uniform (all tag 0): 0.08–0.11 µs hidden and 0.15–0.20 µs visible under a contiguous candidate,
0.27–0.52 µs under a scattered one.

Two things the design did not model:

- **The expensive probe is a *visible* head value under a *scattered* candidate, not a hidden one.**
  `narrow` computes `candidate.and(view)`, whose cost is the containers the two share — 1,526 for a
  scattered candidate — and then allocates the result. `Bitmap::intersect` on the same view answers the
  same boolean by short-circuiting at the first common container: 5–20× cheaper at the median, 140× at
  the worst case (7.9 ms → 54 µs). The predicate the walk needs is a boolean, so it should take the
  boolean route.
- **A hidden value with members is cheap when the candidate is small**, whatever the value's size:
  0.06–0.13 µs median, ≤ 3.4 µs p99, 103 µs at the very worst — a `narrow` against a disjoint view
  touches only the containers whose keys coincide. The design's "1.26–2.1 ms for a value with many
  members the candidate is disjoint from" (C24's figure, over 2.4M items) did not reproduce here at any
  sparsity; see "Confirms / refutes" below.

**The budgeted walk** — fold, two binary searches, then probe each value in the range in order until 20
are visible or the budget is spent; 100 random real-key prefixes per row; wall time per keystroke
including the lookup, page cache warm, no per-value warm-up (ms, median / p99):

Zipf, sparse viewers (the case the budget exists for):

| candidate | prefix | budget 10³ | budget 10⁴ | budget 10⁵ |
|---|---|---|---|---|
| 0.01% contiguous | 1 char | 0.06 / 0.08 — 0/100 pages filled, 0.7 found | 0.60 / 0.63 — 0/100 filled, 6.0 found | **1.9 / 2.5** — 100/100 filled after a mean 31.8k probes |
| 0.01% contiguous | 2 chars | 0.06 / 0.08 — 0/100 | 0.60 / 0.65 — 0/100 (10 ranges exhausted) | 1.8 / 2.8 — 74/100 filled, 26 exhausted |
| 0.01% contiguous | 4 chars | 0.06 / 0.08 — 0/100 (49 exhausted) | 0.07 / 0.67 — 0/100 (81 exhausted) | 0.06 / 2.0 — 6/100 (94 exhausted) |
| 0.01% scattered | 1 char | 0.30 / 0.41 — 0/100, 0.4 found | 2.9 / 3.7 — 0/100, 5.3 found | **10.4 / 21.1** — 100/100 filled after a mean 36.9k probes |
| 0.01% scattered | 2 chars | 0.27 / 0.46 — 0/100 | 2.8 / 3.3 — 0/100 | 9.6 / 14.3 — 76/100 |
| 0.01% scattered | 4 chars | 0.21 / 0.38 — 0/100 (49 exhausted) | 0.27 / 3.2 — 0/100 (81 exhausted) | 0.28 / 11.8 — 3/100 (97 exhausted) |
| 0.1% contiguous | 1 char | 0.06 / 0.07 — 0/100, 4.4 found | **0.33 / 0.55** — 100/100 after 4.2k probes | 0.25 / 0.43 — 100/100 |
| 0.1% scattered | 1 char | 0.33 / 0.42 — 0/100 | **1.7 / 3.6** — 100/100 after 4.2k probes | 1.5 / 2.4 — 100/100 |
| 0.1% scattered | 3 chars | 0.29 / 0.48 — 0/100 | 1.1 / 2.2 — 57/100 (43 exhausted) | 1.1 / 1.8 — 57/100 |

Zipf, 1% and 10% viewers: every page fills inside the 10³ budget (mean 83–590 probes), at 0.01–0.06 ms
contiguous and 0.03–0.26 ms scattered (p99 ≤ 0.74 ms). Uniform: same shape, 0.01–0.07 ms medians at 1%
and 10%; at 0.01% the 10⁵ budget fills the page in 1.5 ms contiguous / 9.8 ms scattered (p99 2.9 / 14.7),
the 10⁴ budget fills 3 of 100 pages, the 10³ budget none.

"Range exhausted" means the prefix genuinely had fewer than 20 visible values — the correct answer,
found at the cost of walking the range. A 4-char prefix at 10⁷ covers a median 655 values, so at 0.01%
sparsity most 4-char pages cannot fill whatever the budget.

**What decides the budget.** At 10⁷ values a 0.01% viewer has ~0.06% of values visible (Zipf) and needs
a mean 32k–37k probes to find 20 of them under a 1-char prefix. The 10⁴ budget the design recommends
therefore **never fills that viewer's page** for 1–2 character prefixes (0/100) and shows six values
with `more: true`; the 10⁵ budget fills every one at a median 1.9 ms (contiguous) to 10.4 ms
(scattered), p99 21 ms. Both are inside the owner's 10–100 ms. The 10⁵ budget's cost ceiling — a
viewer who sees nothing under a broad prefix — is 10⁵ probes at the tag-0 constant: 6 ms contiguous /
30 ms scattered *(modelled from the measured 0.06 / 0.30 µs per probe; the measured 10⁴-budget rows are
0.60 / 2.9 ms, which scale linearly)*.

### Arm 3 — the lever, priced (design §6.3)

Same fixture. The per-session visible-value set built by both routes, median of three; the resulting
bitmap's bytes; and the per-keystroke iteration of visible bits inside a prefix range.

| candidate | (i) value-column pass: Roaring `add` / plain bitset / collect+sort+`add_many` | (ii) all-postings pass: `narrow` / `intersect` | visible values | Roaring bytes | V bits / 4V bits |
|---|---|---|---|---|---|
| 0.01% contiguous | 0.4 / 0.3 / 0.2 ms | 0.60 s / 0.30 s | 5,938 | 13.1 KB | 1.25 MB / 5 MB |
| 0.01% scattered | 1.3 / 0.4 / 0.3 ms | 2.89 s / 1.83 s | 5,796 | 12.8 KB | |
| 0.1% contiguous | 4.9 / 1.0 / 1.4 ms | 0.58 s / 0.28 s | 47,070 | 95 KB | |
| 0.1% scattered | 13.1 / 4.0 / 2.1 ms | 3.17 s / 2.01 s | 46,986 | 95 KB | |
| 1% contiguous | 63 / 5.1 / 14 ms | 0.66 s / 0.31 s | 347,799 | 697 KB | |
| 1% scattered | 119 / 40 / 36 ms | 4.52 s / 2.56 s | 347,006 | 695 KB | |
| 10% contiguous | 349 / 46 / 148 ms | 1.21 s / 0.42 s | 2,224,699 | 1.25 MB | |
| 10% scattered | 368 / 61 / 167 ms | 5.07 s / 1.48 s | 2,225,037 | 1.25 MB | |

Uniform is the same picture with more visible values (a 10% candidate sees 63% of values): column pass
0.5–330 ms, postings pass 0.72–6.8 s.

- **Route (i) is the lever's route.** It is bounded by the candidate: 0.2 ms for a 10⁴-entity viewer,
  ~60 ms for a 10⁷-entity one through a plain bitset (the Roaring `add`-per-entity form is 6× slower
  and `collect+sort+add_many` sits between). Route (ii) is bounded by V: **0.3–6.8 s at 10⁷ values
  whatever the candidate**, 30–630 ns per value, because it opens every record. A `derived` vocabulary
  without a value column in entity space cannot take the lever on the token cadence at this scale.
- **The bitmap is small.** Roaring over dense positions is 13 KB at 0.06% density and saturates at
  1.25 MB (= V bits) once a fifth of values are visible; `run_optimize` gained nothing on these. The
  design's "E ≈ 4V bits, ~500 KB at 10⁶" is 5 MB at 10⁷ and is the wrong unit — the set is over
  *values* (V bits, 1.25 MB) and the index range maps entries to values, so the 4× is not paid.
- **Per keystroke it is microseconds**: `reset_at_or_after` at the range start then up to 20 bits — 0.33
  µs (1 char) to 2.0 µs (3–4 chars) median, p99 3–7 µs on dense sets. On a very sparse set (0.06%)
  the p99 rises to 60–85 µs when the iterator crosses many empty containers; a plain V-bit bitset scan
  is 0.25–0.5 µs median, p99 ≤ 1.3 µs, everywhere.

**Startup — `PostingsReader::open` over 10⁷ records** (the design's §9 item), median of three, page
cache warm:

| fixture | mmap | read into memory |
|---|---|---|
| Zipf (169,039 Roaring records round-tripped) | **1.41–1.57 s** | 1.70–1.98 s |
| Uniform (10⁷ tag-0 records, length check only) | 0.048–0.057 s | 0.35–0.37 s |

The cost is the Roaring validation (~8.5 µs per tag-1 record), not the record count.

---

### Arm 4 — where the shipped probe's time goes, and what (b) and (c) would buy

**Date:** 2026-09-02 · **Harness:** `crates/tessera-bench/src/bin/suggest_walk.rs --decomp` (in the
workspace, on branch `vs/bench-decomp`) · **Raw:** `logs/arm4-decomp-1e6.log`, `logs/arm4-decomp-1e7.log`
**Machine:** the same WSL2 host as arms 1–3. One thread, one process; `pgrep -af cargo` was checked
before the run and **no other cargo or bench process was running** — the host was quiet, and the two
scales ran one after the other, never together. Fixtures under a scratchpad tmpdir, deleted after.

**The question.** §6.2 measures the shipped walk at **61–68 ms** for the sparsest viewer at 10⁷ values
where r1 had measured `Bitmap::intersect` alone at 1.9–10.4 ms. This arm splits one probe into the three
things `ColumnPostings::intersects` does, over **the codes a real budgeted walk actually probes** — 26
one-character prefixes at budget 10⁵, the gate closure recording each code as `Engine::suggest` would
pass it — so the access pattern is the walk's own and not a scan of the code array.

The three stages are reached through three `#[cfg(feature = "bench-timing")]` accessors added for this
arm (`ColumnPostings::bench_base_record_index` / `bench_base_posting_at_index` / `bench_hits`, and the
two `DeltaTier` entries under them). Each calls the shipped code, so the stages sum to the whole call up
to the instrument; nothing is transcribed. The feature is off by default and enabled only by
`tessera-bench`, as `tessera-engine/bench-timing` is.

**Per probe, 10⁷ values over 10⁸ entities, ns, median / p99** — the 0.01% viewer, whose probes are
99.99% *hidden* values (2.22–2.25M probes each; 322 and 303 visible). `search` is the binary search over
the keyed base's 10⁷-entry code array; `view` is `read_posting` — the record slice, and the portable
`BitmapView::deserialize` where the record is Roaring; `test` is `hits`, the existential intersect;
`whole` is `intersects`, timed in its own pass.

| Viewer | class | search | view | test | whole | search share |
|---|---|---|---|---|---|---|
| 0.01% contiguous | hidden | **550 / 950** | 190 / 520 | 20 / 30 | 730 / 1,162 | **72%** |
| 0.01% contiguous | visible | 550 / 910 | 200 / 4,539 | 110 / 620 | 851 / 6,598 | 64% |
| 0.01% scattered | hidden | **550 / 950** | 190 / 510 | 70 / 160 | 811 / 1,329 | **68%** |
| 0.01% scattered | visible | 560 / 970 | 200 / 3,940 | 80 / 14,317 | 890 / 19,397 | 67% |

At 10⁶ values the same probes are 130–210 / 160–230 / 30–100 ns for a whole call of 280–480 ns: the
search is where the scale shows, and it is the only stage that moves between 10⁶ and 10⁷.

**Instrument.** `Instant::now()` cost 19.7–21.1 ns on the host, measured in the same run, so each
bracketed stage carries ~20 ns of it. Net of that the three stages are ~530 / ~170 / ~0–50 ns and sum to
700–750 against a whole call of 730–811 — they agree, and the residue is the instrument's own
perturbation. Percentages above are of the staged sum and are ±5 points.

**The walk around the probe is not where the time is.** The same 26 walks with a gate that answers
`false` without reading a posting cost **0.91–0.94 ms** per prefix over 10⁵ entries — **9.1–9.5 ns per
value examined**, against 62.25 / 82.39 ms median for the same walks with the shipped gate on the same
run. So the fold, the two binary searches over the index, the payload and code reads per entry and the
emitted set together are **~1.5% of a keystroke**; ~98.5% is the posting probe, and 68–72% of that is
the record search. (Those 62/82 ms reproduce §6.2's 61.2/68.2 ms at the median; the scattered figure is
higher here because this arm walks 26 distinct prefixes rather than cycling them over 200 repeats, so
it has 26 samples and its "p99" is a maximum.)

**This corrects §6.2's "13–29% of a probe" for the search, and both figures are measured.** That one
timed 10⁵ searches back to back with the code array warm in cache (299 ns of 1,037). Interleaved with
the view and the test, as the walk runs them, the same search is **550 ns of 730** — the probe evicts
the code array between searches. The interleaved figure is the shipped access pattern.

**Minor page faults** (`/proc/self/stat` field 10, around each pass):

| Pass | contiguous | scattered |
|---|---|---|
| first budgeted walk, mappings fresh | 395 (1.8 per 10⁴ probes) | 4 (0.0) |
| whole-call pass | 0 | 0 |
| staged pass | 0 | 0 |

So **cold-page cost is not what makes the walk 62 ms**: the first walk over a freshly mapped 1.06 GB
index and 281 MB postings file takes under two minor faults per 10⁴ probes, and every later pass takes
none. The 550 ns search is cache misses inside a resident mapping, not faults. **⊘ Device-cold cost is
still NOT measured** — the files were written seconds before being read.

**Candidate container counts**, which decide what a container-key sidecar could reject: the 0.01%
contiguous candidate (10,000 entities from `Bitmap::from_range`) is **1 container**; the 0.01% scattered
candidate (10,000 entities strided over 10⁸) is **1,526 containers — every container in the entity
space**.

**Postings shape, measured over the fixture's own records:** at 10⁷ values, 10⁷ records of which
**47,348 are Roaring** and the rest tag-0 arrays of ≤32 entities; **20,792,930 container keys** in all,
2.08 per record. At 10⁶ values: 47,348 Roaring of 10⁶ records, 11,792,930 keys.

#### What (b) and (c) would save — *modelled from the stages above*

**(b) a code → record map built at open — removes the search.** It replaces 550 ns of a 730–811 ns
probe with one lookup. A lookup is one hash and one random touch into a table larger than cache, which
this fixture prices at ~100–150 ns (the `view` stage is 190 ns for two such touches). So **(b) saves
~400–450 ns per probe, taking the sparsest viewer's keystroke from 62–82 ms to ~28–38 ms** *(modelled)*.
Its cost is the design's own objection: an open-addressed `(u32 code, u32 ordinal)` table at 50% load is
**160 MB per column** at 10⁷ records, which "memory as low as possible" declines.

**(b′) the same saving for 4 MB, and it is not in the design** *(modelled)*. The search is slow because
24 comparisons over a 40 MB sorted `u32` array miss cache on the last several. A bucket table over the
code's **top 20 bits** — 2²⁰ `u32` offsets, **4.2 MB** — leaves ~10 records per bucket, one or two cache
lines, so the search becomes ~1–2 misses rather than ~8: **~150–250 ns**, i.e. most of (b)'s saving for
2.6% of its bytes. It is a build-time-free structure (the code array is already sorted), it needs no map
from code to record, and it is worth pricing before (b) is.

**(c) a per-record container-key sidecar — removes the view, and only sometimes.** It answers "can this
record possibly meet the candidate?" from the record's container keys without deserialising the posting
body. Three measured facts bound it:

- It targets the `view` + `test` stages, which are **190 + 20–70 ns of a 730–811 ns probe: 26–32%**, and
  the search it does not touch is 68–72%.
- Reading the sidecar run is itself one random touch (~100–150 ns), so the saving where it *does* reject
  is **~90–140 ns per probe: 12–17%, or 62 ms → ~53–55 ms** *(modelled)*.
- **Against the scattered candidate it rejects nothing.** A 0.01% scattered viewer's candidate touches
  all 1,526 containers of the entity space, so every record's keys intersect it and the sidecar always
  answers "maybe" — the view and the test are paid anyway, plus the sidecar's own touch. **(c) is a
  small loss for the scattered viewer, who is the slower of the two (82 ms against 62 ms).**

**(c)'s bytes at 10⁷ records, measured on the fixture's postings:** 20,792,930 container keys ×
2 B = **41.6 MB**, plus one `u32` run offset per record = **40.0 MB**, so **81.6 MB per column** — half of
(b)'s and for a quarter of the saving, on the contiguous shape alone. Restricting it to the 47,348
Roaring records (the only ones where "constructing a view" is more than a slice) would carry ~7.5M keys
≈ **15 MB** *(modelled from the fixture's Zipf law)* plus a way to find them, and would save nothing on
the 99.5% of probes that land on a tag-0 array. **These byte figures are this fixture's Zipf membership
shape**, not a property of any vocabulary.

**The recommendation this arm supports: (b), and (b′) before it.** The sparsest viewer's keystroke is
not paying for Roaring arithmetic — the intersection itself is 3–9% of a probe and the whole walk
machinery around it is 1.5% of the keystroke. It is paying to *find the record*, twice over: 68–72% of
the probe is a binary search whose array does not fit in cache. (c) attacks the second-largest stage,
helps only a viewer whose mask is contiguous, and costs half of (b)'s bytes.

---

## Conclusions for the design

**Confirms, refutes, leaves open — §6.1–§6.3 figure by figure:**

- §6.1 arena "~130 MB at 10⁶ values, ~4 entries each" — *confirmed in shape*: 72 MB at 2.2 entries/value
  as built (35 MB key-only), which is the model's per-entry constant. At 10⁷ it is 671 MB (490 MB for the
  suffix-referencing form), 5× the dict or FST.
- §6.1 FST "~30–60 MB at 10⁶, 6.7 B/key" — *confirmed*: 18.6 MB file + 16.8 MB side array = 35 MB;
  12.2 B/key key-only at 10⁶, 9.5 B/key at 10⁷. The 6.7 B/key prior was decimal keys; real names cost
  more and the dict costs the same.
- §6.1 "the `SortedDictWriter` format is a third option" — *confirmed and it wins*: 90.8 MB at 10⁷
  (9.1 B/key), 3.6× faster to build than the FST, no new dependency, and its file is mapped and
  evictable like the FST's.
- §6.1 "sorting … on the order of a second *(assumed)*" — *confirmed at 10⁶ (1.2–1.5 s), refuted at 10⁷
  (34–38 s)*. The build cost at target scale is the sort, single-threaded.
- §6.1 "FST build ~30 s for 1.17×10⁸" — *consistent*: 8.6 s for 10⁷ names, 12.6 s for 22M entries,
  29.6 s for 10⁷ hex keys.
- §6.2 "microseconds for a rare value" — *confirmed, and it is sub-microsecond*: 0.06–0.13 µs for a
  tag-0 or empty record, 0.3 µs under a scattered candidate.
- §6.2 "up to 1.26–2.1 ms for a value with many members the candidate is disjoint from" — **NOT
  reproduced at 10⁸ entities — do not claim it as the walk's constant.** The hidden-with-members
  probe never exceeded 103 µs at any sparsity or shape. What does cost 0.3–1.9 ms (p99 up to 7.9 ms) is
  a *visible* head value under a *scattered* candidate through `narrow`, and `Bitmap::intersect` cuts
  that to 6–54 µs. The C24 figure is over 2.4M items with a different candidate shape; this fixture
  did not exercise it.
- §6.2 "10⁴ probes at the sparse-value constant is tens of milliseconds" — *better than modelled*:
  0.6 ms contiguous, 2.9–4.8 ms scattered.
- §6.2 "at the worst measured constant ~10–20 s" — **NOT observed**: the worst keystroke measured at any
  budget was 21 ms (10⁵ budget, 0.01% scattered, 1-char prefix).
- §6.2 recommended budget default 10⁴ — *refuted as a default at 10⁷*: it leaves the sparsest viewers'
  pages unfilled on 1–2 character prefixes (6 of 20 found). See the recommendation.
- §6.3 "E ≈ 4V bits, ~500 KB at 10⁶" — *superseded*: the set is over values, V bits = 1.25 MB at 10⁷, and
  Roaring is 13 KB–1.25 MB with density; 10⁴ concurrent sessions at 10⁷ values would be ≤ 12.5 GB as
  bitsets and far less as Roaring for sparse viewers — decision 0093's byte budget still applies.
- §6.3 "first-keystroke setup of milliseconds at 10⁶ items to seconds for a scattered full-mask
  principal at 10⁹ *(modelled)*" — *confirmed for the value-column route*: 0.2 ms at 10⁴ candidate
  entities, 61 ms at 10⁷ through a bitset (368 ms through Roaring `add`); extrapolating linearly to a
  10⁹-entity full-mask candidate gives ~6 s *(modelled)*. The all-postings route is **0.3–6.8 s at any
  sparsity** and is not a token-cadence route at 10⁷.
- §9 "`PostingsReader::open` … 10⁶ deserialisations … a number to take" — *taken*: 1.4–1.6 s at 10⁷
  values with 169k Roaring records, 50 ms when every record is a small array. Proportional to the
  Roaring record count, not to V.

**Recommended index representation: (d), the repository's front-coded block dictionary**, over the
distinct folded entry strings, plus a `u32` positions array (and a starts array for word-start runs)
where word starts are on. At 10⁷ values that is 91 MB of mapped file key-only, or 124 MB of file plus
~90–200 MB of side arrays with word starts (side arrays tighten to ~90 + 4×distinct-strings MB with
`shrink_to_fit`); 2.4 s / 4.2 s to write after the sort; 1–4 µs per prefix range including its
fail-closed checks. The FST is within 5% on bytes, 3.6× slower to build, and a new dependency; the
arena family is 3.6–5× the bytes as anonymous heap. The design's own recommendation of the arena for
a first build is defensible at 10⁶ (35–72 MB) and not at 10⁷ under "memory as low as possible".

**Recommended walk-budget default: 10⁵ examined values, with the probe taken as a boolean
(`Bitmap::intersect` on the mapped view) rather than `narrow(...).is_empty()`.** Measured at 10⁷ values
over 10⁸ entities: fills the page for every viewer down to 0.01% sparsity at 1.9 ms (contiguous) /
10.4 ms (scattered) median, 21 ms p99; costs at most 10⁵ × 0.3 µs = 30 ms *(modelled ceiling)* for a
viewer who sees nothing under a broad prefix. 10⁴ is the right default only if a sparse viewer being
told "type more" after six values is acceptable; it halves nothing that matters, since the 10⁵ walk
stops as soon as the page fills. Switching the probe to `intersect` is what makes the ceiling
independent of the candidate's container count — with `narrow`, a 1% scattered viewer paid 0.3–1.0 ms
per visible head value.

**Two fix-it-now items for the implementation, from the measurements:** the entry sort must not be a
single-threaded indirect sort at 10⁷ (34 s), and the probe must not materialise the intersection.

**Negative results, stated:** the 1.26–2.1 ms hidden-value constant: NOT reproduced here — do not carry
it into the register row as the walk's constant without re-measuring on the fixture that produced it.
Device-cold page-fault cost of the mapped dict, FST and postings: NOT measured. Concurrency (thousands
of sessions probing the same mapped file): NOT measured; every figure is one thread.

---

## Method

**Fixture.** `prep` reads `data/ladder/geonames/allCountries.txt` (13,463,857 rows), takes columns 1
(`name`) and 2 (`asciiname`), folds each — **NFKC (`unicode-normalization`), then `str::to_lowercase`,
then whitespace collapsed to one space and trimmed** — dedupes to 10,132,181 strings in 15 s, sorts,
then shuffles with a fixed seed and writes `vocab.txt`, so the first V lines are a uniform random
sample. `to_lowercase` is Unicode lowercase mapping, not default case folding; they differ on a handful
of characters (ß, final sigma, some ligatures) and the difference does not move a byte count at this
scale. Word starts follow §4: a transition into an alphanumeric from anything else, after folding.
Hex keys are 32 random hex characters from a seeded splitmix64.

**Arm 1.** One process per (structure, V, entry set). Keys are loaded into one arena, the entry list
derived and sorted once (`sort+derive` in the log), then the structure is built three times, dropped
between, with heap growth from a counting global allocator, file size from the filesystem, and RSS from
`/proc/self/status`. The BTreeMap holds `folded → entry index`, with duplicate word-start strings
suffixed `\0<index>`; the FST holds distinct strings with value = start of the string's run in a
positions array (key-only: the dense position itself); the dict holds distinct strings via
`SortedDictWriter::push` and is opened `Access::Mapped`, prefix ranges through `SortedDict::prefix_range`
(which decodes four boundary keys to check the range, included in the timing). Lookups: 2,000 prefixes
per length, each timed individually with `Instant`, after one warming pass over the same prefixes.

**Arm 2 / 3.** `gen` assigns member counts (Zipf s = 1.0 with the floor and 2% empties described above,
or uniform), expands them to a value column, shuffles it (Fisher–Yates), writes `values.u32` (400 MB),
bucket-sorts entities by value and encodes each value's ascending list with `encode_posting(_, _, 32)`
into a `PostingsSpool`, then `finish` writes `postings.arrow` — the same bytes `tessera-build` would
write. Arms 2 and 3 open it with `PostingsReader::open` (timed, three times each mode) and
`ColumnPostings::open(_, true)`, map `values.u32`, and build the key-only arena for prefix ranges.
Candidates: contiguous = `Bitmap::from_range` at a random start; scattered = Bernoulli(f) over all
entities via `add_many`. The per-value class split probes each sampled value once (warming its record)
and then times `narrow` and `intersect` once each; the budgeted walk is timed end to end with no
warm-up. Arm 3's routes are the median of three; its two visible sets are asserted equal.

**Overlap.** The first arm-2/arm-3 pass ran while the arm-1 sweep was still on another core
(`logs/overlapped/`); the quiet re-run (`logs/arm2-*.log`, `logs/arm3-*.log`) is what the tables quote.
The two agree within noise (e.g. the 0.01% scattered 10⁵-budget walk: 10.39 / 15.6 ms vs 10.40 / 21.1
ms median / p99).

## Re-run

```bash
cd probes/2026-09-02-value-suggestion/suggestprobe && cargo build --release
D=/home/user/code/tessera/data/ladder/probe-suggest        # gitignored under data/
B=./target/release/suggestprobe
$B prep /home/user/code/tessera/data/ladder/geonames/allCountries.txt $D
../run_arm1.sh                                           # every (structure, V, entries) cell + hex
$B gen $D 10000000 100000000 zipf; $B gen $D 10000000 100000000 uniform   # ~10 s each, 0.9 GB each
$B arm2 $D 10000000 zipf; $B arm2 $D 10000000 uniform
$B arm3 $D 10000000 zipf; $B arm3 $D 10000000 uniform
```

Long runs were launched as `setsid nohup … > log 2>&1 &` and polled by PID.
