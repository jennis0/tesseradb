# Recovering keyword `contains`: the searcher was built once per key, and the fence never saw it

**Date:** 2026-08-13 · **Harness:** [`recover/`](recover/) · **Machine:** WSL2 on Linux 6.18, AMD
Ryzen 9 5900X (Zen 3, 12 cores, 32 MiB L3), 47 GB RAM, single-threaded, built from the repository
root so `.cargo/config.toml`'s alignment pin applies · **Corpus:** the same three real arXiv
columns every campaign in this family has used (`id`, `submitter`, `doi`), read as prefixes in
snapshot order from the Kaggle snapshot (v296) · **Raw:** [`raw/`](raw/)

Commissioned by the question the retirement fence left open: `contains` under the keyword family
was measured 1.5–71× slower than the flat `utf8` column it replaced, and the whole of that cost is
the dictionary read. This campaign asks what is recoverable. Both routes turned out to be paying
for work they discard — the broad one rebuilding a searcher per key, the narrow one decoding a
block per candidate entity to keep one key of it — and both fixes have landed. On the way it found
that the fence itself was not measuring the code the engine shipped.

Every arm is asserted to select exactly the ordinals the arm it replaces selects, before either is
timed. Figures are the median of three timings inside one process; the broad tables reproduce
across five whole-process runs to within 3% (`raw/recover-2400k-{1..5}.txt`). Runs 1–3 predate the
block-walk arm and runs 1–4 predate the shipped-probe arm — see the correction in result 3.

## Results

### 1. The shipped broad route built a substring searcher once per dictionary key, and hoisting it recovers 1.14–2.03×

`contains_broad` walked the dictionary as `if key.contains(needle)`. `str::contains(&str)`
constructs a two-way searcher from the needle on **every call**, and the walk calls it once per
key — a few tens of bytes of haystack against a setup proportional to the needle. Per dictionary
key at 2.4M (*measured*):

| column | needle bytes | matching keys | decode only | shipped `str::contains` | hoisted `memmem::Finder` | recovered |
|---|---|---|---|---|---|---|
| `id` | 3 | 197,128 | 12.04 | 32.47 | **17.77** | 1.83× |
| `id` | 4 | 107,455 | 12.04 | 31.28 | **17.12** | 1.83× |
| `id` | 6 | 1,080 | 12.04 | 26.77 | **16.89** | 1.58× |
| `id` | 8 | 10 | 12.04 | 19.92 | **15.61** | 1.28× |
| `submitter` | 3 | 444 | 19.35 | 43.83 | **26.18** | 1.67× |
| `submitter` | 4 | 284 | 19.35 | 43.67 | **25.72** | 1.70× |
| `submitter` | 6 | 5 | 19.35 | 41.70 | **25.95** | 1.61× |
| `submitter` | 8 | 2 | 19.35 | 38.57 | **25.31** | 1.52× |
| `doi` | 3 | 399,554 | 12.79 | 19.71 | **17.35** | 1.14× |
| `doi` | 4 | 286,679 | 12.79 | 20.58 | **17.40** | 1.18× |
| `doi` | 8 | 224,855 | 12.79 | 22.56 | **17.93** | 1.26× |
| `doi` | 16 | 9,055 | 12.79 | 38.25 | **18.87** | 2.03× |

**The needle-length dependence is the searcher's construction, not the search.** The shipped cost
swings by 1.6–1.9× across needle lengths on every column and in *both* directions — down on `id`
and `submitter` as the needle lengthens, up on `doi` — while the hoisted cost is flat to within
±10% on `id` and `submitter` and within ±9% on `doi`. A cost that moves with the needle in
opposite directions on different columns is not a property of the corpus; it is the interaction
between two-way's setup and the key length it is amortised over.

Decode alone is 12.04–19.35 ns/key, reproducing the dictionary campaign's 11.0–18.8. **After the
hoist the search costs 3.6–6.8 ns/key on top of the decode, and the decode is the remaining
majority of the walk.** No further constant-factor work on the search is worth doing; the decode is
where the walk now lives.

### 2. The retirement fence did not measure the shipped walk — its broad arm already hoisted the searcher

The fence's own prose explains the needle-length effect by saying "a longer needle lets `memmem`
skip further per step". The shipped route did not use `memmem`; it used `str::contains`. Reading
the fence's per-key walk figures against both arms of this campaign says which one it timed:

| column | needle | fence's walk | this campaign, shipped | this campaign, hoisted |
|---|---|---|---|---|
| `id` | 4 B | 16.1–16.7 | 31.28 | **17.12** |
| `id` | 16 B | 10.7–10.8 | *no key is long enough* | — |
| `submitter` | 4 B | 24.8–25.1 | 43.67 | **25.72** |
| `submitter` | 16 B | 19.8–19.9 | *no key is long enough* | — |
| `doi` | 4 B | 16.6–17.9 | 20.58 | **17.40** |
| `doi` | 16 B | 17.0–16.8 | 38.25 | **18.87** |

**Four comparable cells, four matches to the hoisted arm within 6%, and four misses against the
shipped arm by 1.14–2.03×.** The fence's broad-route arm was a reimplementation that hoisted its
searcher, and the shipped route it stood for was slower than the number recorded.

**Confirmed by reading it.** The harness was deleted from the working tree with the baseline it
measured, and this campaign first reported the above as inference from the figures and the prose.
That was wrong: git holds it, at `7a24315:crates/tessera-bench/src/bin/utf8_retirement_fence.rs`.
`broad_ordinals` opens `let finder = memchr::memmem::Finder::new(needle.as_bytes());` outside
`dict.walk`, and `narrow_contains` opens the same outside its probe loop — so **both** fence arms
hoisted, where the shipped routes built a searcher per key and per candidate entity respectively.
A deleted file in a repository is not an unreadable one, and reading it would also have caught the
narrow arm, which the inference missed (see the correction to result 3).

Correctness is unaffected: the fence asserted every keyword route bitmap-equal to the flat scan
before timing, and a reimplemented walk that selects the same ordinals passes that check. What was
wrong was the attribution, not the answer.

**What the fence's `contains` band therefore means.** Composing its per-cell totals with the
per-key walk delta measured above (*modelled from measured parts*), against the flat scan:

| what | `id` contiguous | `submitter` contiguous | `doi` contiguous | scattered cells | band |
|---|---|---|---|---|---|
| shipped as the fence ran | 144× | 57× | 4.2× | 2.5–9.8× | **2.5–144×** |
| shipped after the hoist | 82× | 38× | 3.7× | 1.8–5.9× | **1.8–82×** |
| + the unbuilt ordinal bitset | 71× | 28× | 3.6× | 1.5–5.1× | **1.5–71×** |

The last row is the band `records-and-search.md` §4.3 records. It is the cost of a route that is
**not built**: the fence flags its bitset arm as bench-local, and its walk arm was hoisted. The
design's figure was therefore a floor for two changes rather than a measurement of the tree.

### 3. The narrow route probed the same key many times, and reading blocks instead recovers 1.9–6.1×

`contains_narrow` called `key_of` once per candidate **entity** and searched each returned key with
`str::contains`. Three costs compound: entities sharing a value probe the same ordinal repeatedly,
every probe decodes from its block's restart point and discards about `restart_interval / 2` keys,
and every search builds its own two-way searcher. Deduplicating removes the first, handing the
sorted result to `SortedDict::walk_ordinals` removes the second, and `KeyMatcher` removes the third.
Ns per candidate entity, dictionary work only (*measured*):

| column | candidate | entities | distinct ordinals | blocks touched | **shipped probe** | hoisted probe | probe per distinct | **block walk** | recovered |
|---|---|---|---|---|---|---|---|---|---|
| `id` | 25% contiguous | 600,000 | 600,000 | 37,500 of 150,000 | 83.5 | 72.4 | 75.3 | **19.4** | 4.30× |
| `id` | 25% stride-4 | 600,000 | 600,000 | 150,000 of 150,000 | 74.5 | 60.7 | 62.6 | **37.1** | 2.01× |
| `submitter` | 25% contiguous | 599,976 | 151,821 | 32,267 of 33,906 | 157.6 | 147.6 | 38.3 | **26.0** | 6.05× |
| `submitter` | 25% stride-4 | 599,976 | 278,380 | 33,905 of 33,906 | 145.1 | 165.2 | 60.7 | **33.3** | 4.35× |
| `doi` | 25% contiguous | 256,632 | 256,144 | 22,075 of 64,053 | 118.0 | 116.9 | 91.0 | **35.1** | 3.36× |
| `doi` | 25% stride-4 | 256,633 | 256,518 | 63,392 of 64,053 | 114.5 | 115.4 | 96.9 | **59.8** | 1.91× |

**Every cell improves, and the three parts do different work.** Deduplication is the whole gain on
`submitter`, whose candidate carries 151,821 distinct values across 599,976 entities, and nothing at
all on the near-unique `id` and `doi`. The block walk is what improves the near-unique columns, and
most where the candidate is contiguous and its blocks dense: `id` contiguous touches 37,500 blocks
for 600,000 ordinals and recovers 4.30×, where `id` stride-4 touches every block in the dictionary
and recovers 2.01×. The searcher hoist is worth only 1.00–1.15× here — block decode dominates a
probe as it does not dominate a walk — and on `submitter` stride-4 the hoisted arm measures
*slower* than the shipped one (165.2 against 145.1), which is this campaign's one cell where the
two orderings invert and is reported rather than smoothed.

The route is **never worse than the probe loop it replaces**, up to the sort: a candidate with no
duplicates whose ordinals share no block decodes exactly what `key_of` decoded.

> **Correction, 2026-08-13.** The first four runs of this campaign measured the "shipped probe"
> column with a **hoisted** searcher, which is the retirement fence's `narrow_contains` and not the
> route the engine shipped — the same defect this campaign was commissioned to report in the fence
> (result 2), repeated in the report of it. `raw/recover-2400k-{1,2,3,4}.txt` carry the understated
> baseline and a 1.63–5.03× band; `raw/recover-2400k-5.txt` adds the genuinely shipped arm and both
> columns above. The error was conservative — it understated what the replacement recovers — and it
> was found by an adversarial review of this campaign, not by the campaign.

**This makes the crossover's constant a bound rather than an estimate.** `NARROW_PROBE_NS = 100`
priced one `key_of` per candidate entity; the route now costs 19.4–59.8 ns per entity across these
six shapes and still ~100 in the worst case the constant must cover. Keeping it at 100 errs towards
the broad route, whose cost is capped by the vocabulary. The price of that safety is measured: on
`id`'s contiguous 25% candidate the rule takes the broad route at 41.1 ms where the narrow route
now costs 11.6 ms. Closing it needs a rule that reads the candidate's distinct ordinal count, which
is the fence's stop-and-report A — **an §8.2 admissibility question, unruled, and not closed here.**

### 4. The ordinal test was `O(log k)` in a corpus quantity, and the table that replaces it is flat

Both `contains` routes end by testing each candidate slot's ordinal against the matching set.
Assembled from `ValueColumn::scan_in` that is a binary search per slot, and its *k* is not the
caller's — it is **the number of dictionary keys carrying the substring**, a corpus-wide count that
includes keys no visible entity carries and that a caller moves by choosing a fragment. Ns per
candidate slot, the test alone, 25% contiguous candidate (*measured*):

| column | needle | matching keys | sorted list | domain table | recovered |
|---|---|---|---|---|---|
| `id` | 3 B | 197,128 | 13.53 | **0.34** | 39.3× |
| `id` | 6 B | 1,080 | 7.53 | **0.37** | 20.3× |
| `submitter` | 3 B | 444 | 5.41 | **0.38** | 14.3× |
| `submitter` | 6 B | 5 | 1.41 | **0.37** | 3.8× |
| `doi` | 3 B | 399,554 | 27.72 | **0.35** | 79.5× |
| `doi` | 6 B | 234,025 | 20.29 | **0.35** | 57.7× |
| `doi` | 9 B | 224,855 | 20.08 | **0.35** | 57.7× |

**The list's cost moves 1.41 → 27.72 ns with the match count and nothing else; the table's is
0.34–0.38 across the whole span.** On `submitter` the two needles differ only in how much of the
vocabulary they hit — 444 keys against 5 — and cost 5.41 against 1.41 ns per slot, which over a
600,000-entity candidate is 2.4 ms of difference readable from outside. That is a fragment
statistic about the whole corpus, arriving in the timing of a request whose traversal was already
identical: `take_scan_work` counts runs and slots, and those never differed. The table answers in
O(1) per slot, is the same size whatever matched, and is what both routes now use.

It is also 3.8–79× faster, which is the ordinary reading of the same change and the one the
retirement fence reported (its bench-local bitset arm, 64% of the broad route's cost). The
security reading was not noticed until this branch's adversarial review.

## What this does not measure

- **Nothing at 10⁹, and nothing out of cache.** Every dictionary here fits in 32 MiB of L3 at 2.4M
  keys. The fence's §11 item 4 — the out-of-cache walk — is still owed, and this campaign's ns/key
  constants inherit that limit exactly as the fence's did.
- **The ordinal scan.** Both routes end in one, the fence measured it, and no arm here re-times it.
- **Parallelism.** `decode_block` refuses a first entry with a non-zero shared prefix, so every
  restart block decodes from empty and blocks are independent by construction — but no arm here
  runs a parallel walk, and `÷ cores` remains untested for this operator as it does for every
  other.
- **The route rule against the routes it now chooses between.** Section 3 prices one cell where the
  rule chooses the more expensive route; no arm sweeps the crossover to find where that starts and
  stops, because the rule that would use the answer needs a ruling first.
- **The write side.** Unchanged by anything here.
