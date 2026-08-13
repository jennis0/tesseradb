# The `utf8` retirement fence: what the keyword family costs, operator by operator

**Date:** 2026-08-13 · **Machine:** WSL2 on Linux 6.18, AMD Ryzen 9 5900X (Zen 3, 12 cores, 32 MiB
L3), 47 GB RAM, single-threaded · **Harness:**
Harness: `utf8_retirement_fence.rs`, deleted with the baseline it measured (closing note) ·
**Corpus:** the three real arXiv columns prior campaigns used — `id` (near-unique), `submitter`
(repeat-heavy), `doi` (sparse) — read as prefixes in snapshot order (= entity order,
`probes/dataset.md` §5 rule 1) from the Kaggle snapshot (v296).

Both string formats exist in the tree at this moment and only at this moment. The shipped flat
`utf8` column is the baseline records-and-search §4.3 retires; once it is deleted no measurement of
the swap is possible and every claim about its cost becomes unfalsifiable. This is that
measurement: same corpus, same candidates, same needles, one variable.

The memo reading against this campaign is
[`2026-08-13-utf8-retirement-fence.md`](../../docs/evidence/memos/2026-08-13-utf8-retirement-fence.md).

## What was compared

**Flat** is the shipped column — `ValueColumn` over `Codes::Text`, answering `eq`, `prefix`, `in`
and `contains` by walking stored bytes under the candidate. **Keyword** is §4.3's family: the
shipped `SortedDict` over the column's distinct values at the shipped `K = 16`, plus a `u32`
ordinal per present entity as a `Codes::U32` column. Both artefacts are written by the shipped
writers, read back through the shipped reader, and both carry the same presence bitmap.

The scans are shipped code. The two `contains` routes are not — §4.3 marks them unbuilt — so the
harness assembles them from `SortedDict::walk` and `SortedDict::key_of` and says so at every figure.

**Correctness precedes timing.** Every keyword route is asserted to return the flat scan's exact
bitmap, over the whole corpus as candidate, for every needle including a deliberately absent one,
before any cell runs. All three columns pass at both scales.

**Needles are positional and were fixed before the first timing run**: `eq` at five fixed fractions
of the value list, `in` at eight, `prefix` and `contains` cut from the midpoint value by a fixed
rule. Picking by position draws frequency-weighted, which is what a caller's own query does.

## Results

### 1. `contains` is slower under the keyword family on every shape measured — by 1.5× to 71×

The regression is the finding. Best keyword route against the flat scan, 25% candidate, 2.4M
entities (*measured*):

| column | candidate | flat | best keyword route | keyword ÷ flat |
|---|---|---|---|---|
| `id` | contiguous | **0.55 ms** | 39.03 ms — broad, bitset | **71×** |
| `id` | scattered | 8.72 ms | 42.41 ms — narrow | 4.9× |
| `submitter` | contiguous | **0.50 ms** | 14.11 ms — broad, bitset | **28×** |
| `submitter` | scattered | 14.35 ms | 21.27 ms — broad, bitset | 1.5× |
| `doi` | contiguous | 6.68 ms | 23.91 ms — broad, bitset | 3.6× |
| `doi` | scattered | 10.36 ms | 26.13 ms — broad, bitset | 2.5× |

The flat scan is faster in **every cell of both campaign scales**, including every point of the
crossover sweep in §3.

The two costs have different shapes, which is why the ratio moves so much. The flat scan is
**linear in the candidate**: 0.91 ns per candidate entity contiguous, 14.5 ns scattered — and a
contiguous run is where `scan_text_contains` searches the run's concatenated bytes as one region at
SIMD throughput instead of value by value. The broad keyword route is **flat in the candidate and
linear in the vocabulary**: the dictionary walk is essentially the whole cost and does not care how
many entities were asked about. So the swap costs most where the candidate is small — the
interactive per-keystroke cell — and least where it is the whole corpus and scattered.

At 10⁹, whole-corpus contiguous candidate, unique vocabulary (*modelled* — ×417 from the 2.4M
constants, see §8's caveat): **flat ~1.1 s, keyword broad ~16 s, keyword narrow ~68 s.**

### 2. `eq`, `prefix` and `in` improve or tie — but only through the right entry point

Contiguous 25% candidate, ns per scanned entity, best keyword route (*measured*):

| column | operator | flat | keyword | flat ÷ keyword |
|---|---|---|---|---|
| `id` | `eq` | 2.237 | **0.275** | 8.1× |
| `id` | `eq`, absent needle | 0.411 | **0.225** | 1.8× |
| `id` | `prefix` | 2.563 | **0.617** | 4.2× |
| `id` | `in`, 8 needles | 8.837 | **1.654** | 5.3× |
| `submitter` | `eq` | 1.172 | **0.207** | 5.7× |
| `submitter` | `prefix` | 2.708 | 2.672 | 1.01× — level; see §7 |
| `submitter` | `in` | 4.442 | **1.636** | 2.7× |
| `doi` | `eq` | 20.971 | 19.602 | 1.07× |
| `doi` | `prefix` | 23.522 | 21.778 | 1.08× |
| `doi` | `in` | 31.192 | 20.973 | 1.49× |

Scattered candidates compress every ratio to 1.0–1.5×: the cost there is bitmap traversal and one
cache line per entity, which both formats pay identically.

**On `doi` the format barely matters, and the reason is presence.** A sparse column is
rank-addressed, so the scan's slot-run merge over a scattered presence bitmap dominates both sides
— about 9 ns per *candidate* entity whatever is stored. The swap neither gains nor loses there.

**The `eq` gain depends on which shipped scan the route calls, and on a column the scan actually
walks the difference is 2.1–4.0×.** `ValueColumn::scan_num_eq` treats the ordinal as a number and
reaches the traversal through a one-element `binary_search`; `ValueColumn::scan_eq` treats it as a
code and reaches the same traversal through a direct compare. At 2.4M, contiguous:

| column | `scan_num_eq` | `scan_eq` |
|---|---|---|
| `id` | 0.585 | **0.275** |
| `submitter` | 0.828 | **0.207** |
| `doi` | 20.091 | **19.602** |

`scan_eq` reproduces filter-index §2.2's 0.25–0.28 ns fixed-width constant; `scan_num_eq` does not,
and through it the keyword route is **slower than the flat scan on an absent needle** (0.583
against 0.411 on `id` — a needle whose length no stored value matches is rejected by the flat scan
on its length alone). Entered through `scan_eq` that regression disappears. For `in` the two entry
points are the same code on a `u32` column and measure identically (1.69 against 1.65).

### 3. §4.3's 0.15 crossover holds, and it is conservative by up to 1.6×

The sweep varies the candidate's cardinality across the range where the two routes swap. Crossover
by linear interpolation between the two sweep points that bracket the sign change, expressed as
**present candidate entities ÷ dictionary size** (*measured*):

| column | candidate | crossover |
|---|---|---|
| `doi` | scattered | 0.155 |
| `doi` | contiguous | 0.165 |
| `submitter` | contiguous | 0.193 |
| `submitter` | scattered | 0.199 |
| `id` | contiguous | 0.237 |
| `id` | scattered | ~0.26 *(modelled — see below)* |

**Measured range 0.155–0.237, against the design's 0.15.** The rule sits at the bottom of the
range, so on every shape measured it switches to the broad route at or before the point the costs
actually cross — it never selects the more expensive route. What it costs is the band between 0.15
and the true crossover, where the broad route it picks is up to **20% more expensive** than the
narrow one it declines (`id` contiguous at 0.20: 40.01 ms against 33.45 ms). Erring this way is
also the safer direction: the broad route's cost is bounded by the vocabulary, while the narrow
route's grows without limit in the candidate.

Two qualifications, on the denominator and on the one row that was not reached.

**The denominator matters on a sparse column, and §4.3 does not say which one it means.** Against
the *raw* candidate cardinality the same crossings land at **0.193–0.368** rather than 0.155–0.237
— `doi` alone moves from 0.165 to 0.368, because only ~43% of its candidate entities carry a value
and the narrow route only probes those. Both readings stay above 0.15, so the rule is safe either
way; the present-entity denominator is simply the tighter one. **Whether a route rule may read the
candidate's intersection with presence is not settled here** — it is a quantity about the
principal's own visible data, which §8.2 constrains — and this campaign does not rule on it.

**The `id` scattered crossover was not reached.** A stride-4 scattered candidate saturates at a
quarter of the corpus, and `id`'s dictionary is the whole corpus, so the sweep stops at 0.25 with
the narrow route still ahead by 1.25 ms. The 0.26 above is a linear fit of the two routes' measured
slopes, extrapolated one step, and is *modelled*.

### 4. The dictionary walk with the substring search costs 10.7–25.1 ns per key

The broad route's walk decodes every key and searches it. Per key, both scales, both needle
lengths (*measured*):

| column | 4-byte needle | 16-byte needle |
|---|---|---|
| `id` | 16.1–16.7 | 10.7–10.8 |
| `submitter` | 24.8–25.1 | 19.8–19.9 |
| `doi` | 16.6–17.9 | 17.0–16.8 |

**10.7–25.1 ns/key**, against the dictionary campaign's **11.0–18.8 ns/key for decode alone** — the
gap is the substring search that campaign's walk omitted. The constant is flat across a 4× corpus
range (`id` 16.7 → 16.1 from 600k to 2.4M keys). At 10⁹ unique keys it multiplies to **11–25 s
single-threaded** (*modelled*), against §6.4's row of "11–19 s decode alone"; the upper end of that
row should move to 25 s.

**The walk's cost varies with the needle's *length*, not with what it matches**: `id`'s walk is
16.1 ns/key for a four-byte needle and 10.8 for a sixteen-byte one, because a longer needle lets
`memmem` skip further per step. A needle's length is a property of the query the caller wrote, so
this is not a channel about stored data — but it does mean the broad route's cost cannot be quoted
as one number per column.

> **Correction, 2026-08-13** ([`contains-recovery`](../2026-08-13-contains-recovery/results.md)):
> the sentence above names the mechanism, and in doing so names the discrepancy — the shipped
> broad route did not call `memmem`, it called `str::contains`, which builds a two-way searcher per
> key. This harness's walk arm hoisted a searcher and so measured 1.14–2.03× *faster* than the tree
> it stood for; four of its per-key cells match the later campaign's hoisted arm within 6% and none
> matches its shipped arm. Every `contains` row in this campaign is a floor for the shipped route,
> and the shipped band was **2.5–144×**, not 1.5–71×. The hoist has since landed, so these figures
> now describe the tree.

### 5. Building the broad route out of `scan_num_in` costs it a further 64%

The broad route yields a set of matching ordinals and then scans for them. Assembled from the
shipped `ValueColumn::scan_num_in` — §4.3's own "O(log k) per slot" — the ordinal test costs about
10 ns per candidate entity, because *k* here is the number of *keys* containing the substring
(22,500 for `id`'s needle), not the eight a caller typed. A dense bitset over the dictionary's
ordinals answers the same question in O(1) per slot. At 2.4M, whole-corpus contiguous candidate on
`id` (*measured*): **63.88 ms through `scan_num_in` against 38.86 ms through a bitset.**

The bitset variant is **bench-local, not shipped**. The finding is that the broad route needs an
ordinal-set test that is constant per slot, and that `scan_num_in` is not it.

### 6. Stored bytes: the family is 2.16–3.56× smaller, on disk, presence included

On-disk file sizes through the shipped writers at 2.4M, per present entity (*measured*):

| column | present | flat `values.arrow` + presence | dictionary + ordinals + presence | flat B/e | keyword B/e | ratio |
|---|---|---|---|---|---|---|
| `id` | 2,400,000 | 42,913,970 | 19,862,107 | 17.88 | 8.28 | **2.16×** |
| `submitter` | 2,399,904 | 53,749,605 | 15,116,830 | 22.40 | 6.30 | **3.56×** |
| `doi` | 1,026,531 | 34,464,738 | 11,305,118 | 33.57 | 11.01 | **3.05×** |

This is an independent confirmation of the dictionary campaign's 2.18 / 3.61 / 3.13, reached a
different way: on-disk files including Arrow framing and the presence bitmap on both sides, rather
than an in-memory accounting with presence excluded. The two agree to within 2%. **The headline
stands as 2.2–3.6× measured**, and it is the one unambiguous win in this campaign. At 600k the
ratios are 2.08 / 3.38 / 3.13, so the figure is stable in corpus size.

⊘ The parent campaigns' caveat transfers unchanged: these are arXiv-shaped keys. A prefix-free key
set (UUIDs, hashes) front-codes to nearly its raw bytes and the layout then merely ties the flat
column. No such column exists in this corpus.

### 7. `prefix`'s advantage is not stable in corpus size, and the campaign did not isolate why

Keyword `prefix` on `submitter` measures **0.695 ns per entity at 600k and 2.672 at 2.4M**, while
the flat scan over the same column and candidate is stable across the same step (2.537 and 2.708).
So the same comparison is a **3.6× win at one scale and level at the other**, and the instability
is in the ordinal range scan rather than in the corpus or the harness.

Two things are ruled out and one is not. Not the format: the same column's `eq` through `scan_eq`
is 0.217 and 0.207 at the two scales. Not build layout: the pinned and unpinned builds agree on
both values. What it is — `scan_range`'s predicate against `scan_eq`'s, the match distribution (272
hits at 600k against 3,059 at 2.4M), or something else — this campaign does not establish, and no
mechanism is asserted here. `doi` shows a similar `prefix` − `eq` gap at 2.4M (21.778 − 19.602 =
2.18 ns/entity) where `id` does not (0.617 − 0.275 = 0.34), which is consistent with several
stories and decides between none.

The consequence for the fence: **keyword `prefix` is not reliably better than the flat scan on a
repeat-heavy column.** On `id` its 4.2× advantage is solid at both scales.

### 8. The sub-nanosecond figures carry the known code-alignment sensitivity

The campaign was run both without `-C llvm-args=-align-all-functions=6` and with it
([`raw/fence-2400000-unpinned.txt`](raw/fence-2400000-unpinned.txt) against
[`raw/fence-2400000.txt`](raw/fence-2400000.txt)); rows in the 0.2–3 ns band move by up to 1.3×
between the two, and
[`scan-constant-sensitivity`](../../docs/evidence/memos/2026-08-11-scan-constant-sensitivity.md)
documents that this class reaches 65% on byte-identical code. **Ratios below about 1.3× in §2
should be read as ties.** Nothing in §1, §3, §5 or §6 is inside that band.

⊘ **The flag the design says closes that channel is not in the tree.** `filter-index.md` places
`-C llvm-args=-align-all-functions=6` "in `.cargo/config.toml`"; no such file exists in this
worktree or in the main tree, and `.cargo/` is listed in `.git/info/exclude`, so a fresh checkout
builds without it. The pinned runs above set it through `RUSTFLAGS` instead. Reported, not fixed —
the path is outside this track's allowlist.

## What this does not measure

- **Scale.** 2.4M real records is what the corpus has; there is no larger real string column. Every
  10⁹ figure here is a multiplication of a constant measured over structures that fit in 32 MiB of
  L3, and a 10⁹-key dictionary does not. §11 item 4 still owes the out-of-cache walk, and no
  synthetic key set was invented for it here: a fabricated vocabulary front-codes according to how
  it was fabricated, which would bias the very constant being measured.
- **Concurrency.** Single-threaded throughout. §6.4's `÷ cores` verdict on the `contains` row is
  untested here.
- **Paging.** Both columns are read into owned buffers and the dictionary through `from_vec`, so
  everything is resident and heap-backed — one variable rather than two. This is not the request
  path's `Access::Mapped`.
- **The write side.** §11 item 7's, and §4.3's *first* named retirement price. Nothing here
  measures the sort, the front-code, or the coalesce's dictionary merge.
- **Postings.** Decision 0067's term postings serve `eq` and `in` and would change §2's picture
  entirely; the scan routes are what this compares, because they are what the flat column has.

## Files

| | |
|---|---|
| [`raw/fence-2400000.txt`](raw/fence-2400000.txt) | the campaign, 2,400,000 records, five repeats, function starts pinned |
| [`raw/fence-600000.txt`](raw/fence-600000.txt) | the same at 600,000, for the scale trend |
| [`raw/fence-2400000-unpinned.txt`](raw/fence-2400000-unpinned.txt), [`raw/fence-600000-unpinned.txt`](raw/fence-600000-unpinned.txt) | the same two without `-align-all-functions=6`; the alignment control of §8. These predate `scan_eq`/`scan_in` being measured beside `scan_num_eq`/`scan_num_in`, so their `eq` and `in` rows carry only the numeric entry point |
| `*.err` | the record count, and the assertion that every keyword route agrees with the flat scan |

Re-run:

```bash
CARGO_INCREMENTAL=0 RUSTFLAGS="-C llvm-args=-align-all-functions=6" \
  cargo build --release -p tessera-bench --bin utf8_retirement_fence
./target/release/utf8_retirement_fence \
  --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json \
  --limit 2400000 --repeat 5
```

---

## The harness is deleted, and this campaign cannot be re-run

The harness under `crates/tessera-bench/src/bin/` was removed in the same change that merged
this campaign. It measured the flat `utf8` column against the keyword family, and the flat column
no longer exists — the retirement it priced deleted `Codes::Text`, `scan_text_eq`,
`scan_text_prefix`, `scan_text_in` and `scan_text_contains`, which are the whole of the baseline
half. A harness that cannot compile against the tree it lives in is not a harness, and keeping it
would state that this comparison is reproducible when it is not.

**That is the point of the fence rather than a defect in it.** The measurement existed precisely
because there is exactly one moment at which it is possible: after both implementations exist and
before one is deleted. The numbers above are the record, the raw output beside them is the
evidence, and neither can be regenerated. Anything that wants to re-check them must first restore
the flat column from history.

What *is* reproducible is the keyword half alone — `probes/2026-08-13-keyword-dict/` measures the
dictionary's own constants against nothing, and `performance-suite.md` specifies the arms that
should carry those figures forward.
