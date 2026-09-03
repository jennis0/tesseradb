# Text `contains`: where the 96 ns goes, and what actually moves it

**Date:** 2026-08-09 · **Status:** Evidence memo — recommends, does not rule.
**Reads with:** [`filter-index.md`](../../design/filter-index.md) §1.1, §2.2,
2026-08-09-filter-performance-options.md §7,
[`probes/2026-08-08-filter-layout/`](../../../probes/2026-08-08-filter-layout/) arms 6, 10 and the
two arms this memo adds: `textdecomp` and `textaccel`.

Everything quoted as measured is a median of three whole-process runs on the campaign machine
(WSL2, 12 cores, 47 GB, single-threaded), at 10⁸ over `textscan`'s surname-shaped column unless
marked, with every accelerated arm asserted bitmap-equal to the shipped
`ValueColumn::scan_text_contains` before its timing is recorded. Parallelism is a known deferred
lever and is not proposed here.

## 1. The decomposition — the cost is memory *and* algorithm, in that order on a scattered candidate

`textdecomp` times the shipped traversal with the substring search replaced by successively more
of the real work, mirroring what `ceiling` did for the unselective case. Per candidate entity, at
10⁸, needle `-000`:

| Arm adds | contiguous 1% | broad 25% | scattered 1% | scattered increment |
|---|---|---|---|---|
| traversal only | 0.44 ns | 0.51 ns | 3.6 ns | — |
| + the two offsets (length reject possible) | 0.76 | 0.66 | 15.3 | **+11.7 — the offsets cache line** |
| + one byte of the value | 1.13 | 1.22 | 34.8 | **+19.5 — the value's cache line** |
| + scan value for the first byte, no verify | 7.96 | 8.20 | 71.9 | **+37.1 — the scan of the value** |
| + full verification (`byte_contains`) | 8.88 | 8.94 | 86.8 | +14.9 |
| shipped `scan_text_contains` | 10.24 | 10.91 | 92.5 | +5.7 reimplementation gap |

Three findings, and the third corrects what the first appears to say:

- **On a contiguous candidate ~80% of the cost is the byte-scan-plus-verify loop** — memory is
  prefetched and nearly free (1.1 of 10.9 ns). An option that vectorises the search is the right
  shape there, and one that only cuts memory traffic is not (arm 10 already refuted the
  offsets-width version of that from the other direction).
- **On a scattered candidate the two random cache lines cost ~31 ns** (offsets + value bytes)
  before any comparison runs. That is a floor no per-value structure can go below by more than one
  line: any layout that must consult *something* per candidate entity pays ~13–15 ns for the first
  random line. At 10⁹ that alone is ~1.4 s for a 10%-coverage scattered principal — **no
  candidate-driven scan puts a broad scattered principal inside the 0.5–1 s budget**, whatever the
  inner loop does.
- **The +37 ns "scan" increment on scattered is mostly latency, not arithmetic.** The same SWAR
  search that removes the loop's compute (`swar-value` below) recovers only ~10 of it, because
  scanning a ~14-byte value touches its second cache line and the loads are dependent. The
  decomposition's labels separate *what work was added*, not *which resource it binds on* — on
  scattered, everything after the offsets is dominated by the value bytes' latency.

**One correction to the record while the multiplication is on the table.** `filter-index.md` §2.2
calls the scattered candidate "the one cell outside the budget". The same arm-6 constants say a
**broad contiguous** candidate is outside it too: 10.9 ns × 2.5×10⁸ (a 25% principal at 10⁹) is
~2.7 s, and the erg-shaped needle 15.9 ns is ~4 s. Confirmed directly at 10⁹ below. The scattered
cell is the *worst* cell, not the only one over.

## 2. Ranked options

| # | Option | Measured gain (10⁸, median of 3) | Memory at 10⁹ | Disk at 10⁹ per column | Security | Cost, risk |
|---|---|---|---|---|---|---|
| **1** | **Search the run's bytes as one region** (`memmem-concat`): a contiguous slot run's values are contiguous bytes, so run `memmem::find_iter` over the region once and map hits back through the offsets, discarding boundary-straddling matches; single-slot runs keep the per-value loop | Contiguous/broad: **10.3–11.4 → 1.6–1.7 ns** (needle `-000`, 6.7×), 0.4–0.7 ns absent needle, 5.9–7.5 ns for a 25%-matching needle. Scattered: 92.5 → 83 ns (it degenerates to the per-value loop) | None — mapped column only, no new structure | **0** | **None new** — §5. | Small-medium. `walk_text` gains a region interface (the traversal already hands runs); the straddle/dedupe mapping is the bug surface — same differential-test condition as every scan change. `memchr` is already in `tessera-filter`'s transitive tree via `arrow`; promoting it to a direct dependency of a TCB crate needs saying, not hiding |
| **2** | **A packed per-value descriptor** (`desc64`): one derived `u64` per value — `offset:36 \| len:8 \| 16-bit trigram bloom` — so a scattered candidate's reject path touches **one** random line instead of two, and only bloom survivors touch the value bytes | Scattered: **92.5 → 39 ns** (2.4×) needle `-000`; 64 ns absent-trigram needle; 90 ns (≈nothing) for a 25%-matching needle. Contiguous/broad: 3.6–3.9 ns (2.9×) | Mapped; resident is what a scan touches | **+8 GB** (the descriptor; the Arrow offsets stay in the record). ×4 text columns: +32 GB | None new — §5 | Medium. A derived, fold-rebuilt structure exactly per the organising rule; values > 255 B need an escape to the offsets; build + manifest + digest plumbing |
| 3 | **desc64 + a second-stage 32-bit trigram bloom** (`desc64+tri32`, independent hash, consulted by desc64 survivors only) | Scattered: **92.5 → 30 ns** (3.1×) needle `-000`; absent needle 95 → 50 ns; 25%-matching needle ~85 ns | as above | **+12 GB**; ×4 columns: +48 GB | None new — §5 | As option 2 plus one array |
| 4 | **Trigram postings** (the design cut to [#44]) | **NOT measured — not built.** The only route under the ~13–15 ns/candidate random-access floor for a broad scattered principal | Postings resident under load | **Modelled from measured counts** (§6): 12.25 posting entries/value at ~1.25–2 B/entry scattered ≈ **15–25 GB** for this 14-byte-value column; scales ≈ linearly with value length — a ~100-byte column ≈ 110–180 GB, which is where the old 100–200 GB figure lives | **Breaks §3.8 as a class** — arm 9 measured 2.1 ms hidden-vs-absent on exactly this posting shape; a needle's trigram statistics are corpus-wide quantities (C4-shape, C8-adjacent content). Needs a registered row and an owner ruling, not a footnote | Large: build, fold rebuild, extents, ingest; plus the verify path (which the flat column does supply — the #44 blocker is gone, the price is not) |

Options 1 and 2/3 **compose**: the traversal hands long runs to the region search and single-slot
runs to the descriptor path — each covers exactly the shape the other cannot. Together, at 10⁹
(§4; the first three rows are directly measured, the last two modelled from the measured
constants):

| Cell at 10⁹ | shipped | after 1 + 3 | inside 0.5–1 s? |
|---|---|---|---|
| 25% contiguous, needle `-000` | 2,868 ms measured | **410 ms** measured | yes — was not |
| 25% contiguous, 25%-matching needle | 3,890 ms measured | 1,663 ms measured | **no** — result-bound |
| 1% scattered (10⁷ candidates) | 971 ms measured | **261 ms** measured | yes — was borderline |
| whole-corpus contiguous | ~11 s | ~1.7 s | needle-dependent — absent/rare yes, common no |
| 10% scattered (10⁸ candidates) | ~9.7 s | **~2.6 s** | **no — see §1's floor** |

The residual over-budget corner is therefore **a broad scattered principal**, and for a
common-substring needle **any large scattered candidate**: the first is bounded by memory latency
per candidate entity (§1), the second by touching the true matches' bytes, which no prefilter can
skip. Postings are the only measured-cost-model escape for the first; nothing scan-shaped escapes
the second (a posting route still verifies its superset). Parallelism is the deferred lever that
would divide both.

## 3. Evidence — the arms at 10⁸

Medians of three (`run-textaccel-1e8-{1,2,3}.csv`), ns per candidate entity. Needles: `-000`
(arm 6's, matches ~10⁻³ of values), `erg` (interior of two stems, matches ~25% — the adversarial
case for a prefilter), `qzx` (absent from the corpus — a prefilter's best case and where its
false-positive rate is nakedly visible).

**Scattered 1% candidate** — the target cell:

| Arm | `-000` | `erg` | `qzx` |
|---|---|---|---|
| shipped | 110.8 (92.5 in the same-session decomp run; the scattered cell's spread is real — quote 92–111) | 111.4 | 95.1 |
| memmem-value | 111.3 | 108.8 | 108.7 |
| swar-value | 83.1 | 90.7 | 72.4 |
| bloom8 | 91.7 | 83.9 | 79.4 |
| tri16 | 46.1 | 100.7 | 76.0 |
| desc64 | **39.0** | 89.9 | 63.8 |
| desc64+tri32 | **30.1** | 84.8 | **49.8** |

**Contiguous 1% / broad 25%** (the two agree within noise; broad shown):

| Arm | `-000` | `erg` | `qzx` |
|---|---|---|---|
| shipped | 11.4 | 15.9 | 8.7 |
| memmem-value | 12.5 | 15.6 | 12.6 |
| memmem-concat | **1.7** | **7.5** | **0.7** |
| swar-value | 11.6 | 14.5 | 8.5 |
| desc64 | 3.9 | 13.8 | 7.8 |
| desc64+tri32 | 3.2 | 13.2 | 6.1 |

Reading it:

- **The 16-bit descriptor bloom is ~53% dense** at ~12 trigrams per value, so a single-trigram
  needle (`qzx`, `erg`) passes half of everything; that is why `qzx` scattered lands at 64 ns
  rather than near the ~15 ns one-line floor, and why the +4 B second stage (joint single-trigram
  FP ~17%) buys `qzx` down to 50. A needle with two-plus trigrams (`-000`) compounds to ~28% /
  ~3% and lands at 39 / 30. The bloom's value is needle-statistics-dependent and the memo's
  numbers bracket it with the best and worst realistic shapes.
- **`erg` is the honest bad case**: 25% of values genuinely match, verification must touch their
  bytes, and every option is within ~25% of shipped on the scattered candidate. A prefilter
  cannot help a needle most values contain.
- **The first-byte scan loop is not the win it looks like.** `swar-value` — the shipped loop with
  an 8-bytes-at-a-time first-byte search — recovers only 10–20% on scattered and nothing on
  contiguous. The decomposition's +37 ns "scan" increment is latency on the value's second cache
  line, not arithmetic (§1). The region search wins on contiguous candidates by amortising across
  values, not by scanning any one value faster.

## 4. Evidence — 10⁹ confirmation

Medians of three (`run-textaccel-1e9-{1,2,3}.csv`, `lite` mode), ns per candidate entity. Every
constant is within noise of its 10⁸ counterpart — the scan stays linear in *n* at this cell as it
did in arm 1's sweep — and the milliseconds are now direct measurements of the budget cells
rather than multiplications:

| Arm | contig 1% `-000` | broad 25% `-000` | broad 25% `erg` | broad 25% `qzx` | scattered 1% `-000` | scattered `erg` | scattered `qzx` |
|---|---|---|---|---|---|---|---|
| shipped | 10.98 | 11.47 (2,868 ms) | 15.56 (3,890 ms) | 9.02 (2,255 ms) | 97.2 (971 ms) | 98.8 | 71.9 |
| memmem-value | 13.24 | 12.79 | 16.11 | 12.95 | 101.3 | 110.7 | 110.3 |
| memmem-concat | **1.67** | **1.64 (410 ms)** | **6.65 (1,663 ms)** | **0.69 (172 ms)** | 77.5 | 88.8 | 73.0 |
| swar-value | 10.69 | 10.60 | 13.70 | 8.10 | 76.2 | 90.5 | 75.8 |
| desc64 | 3.54 | 3.52 | 14.11 | 7.75 | 32.4 (324 ms) | 90.2 | 71.7 |
| desc64+tri32 | 3.15 | 3.18 (795 ms) | 13.25 | 6.12 | **26.1 (261 ms)** | **84.5** | **51.3** |

The scattered candidate here is 10⁷ entities (1% of 10⁹), so its shipped 971 ms *is* the "~1 s
per 10⁷ candidate entities" the campaign has been quoting, measured directly.

**The baseline these cells were taken against.** The `pack` result-sink change
(commit 7e8daed) landed in `values.rs` while this campaign ran; its diff is confined to
`walk_typed`'s fixed-width block packing and does not touch `walk_text`, and an A/B re-run of the
shipped text cells after it landed showed ratios at parity — the absolute numbers of that re-run
are not quoted because a concurrent ~10-core load was on the machine by then. The tables above
are from the pre-pack binary on the quiet machine.

## 5. Security disposition

**Options 1–3 add no leak-register row, by the same construction as the shipped scan.** The
traversal is unchanged: work remains a function of `(candidate, presence)` plus the *bytes of
values inside the candidate* and the needle. A value the principal cannot see is never loaded —
descriptor, bloom and region reads are all indexed by the candidate's slots — so a hidden value
and a nonexistent one remain indistinguishable in work (per-point-attributes §3.8), exactly as
`filter-index.md` §2.2 argues for the scan today. What does vary with the needle — bloom pass
rates, first-byte density, match density in the region search — varies only over values the
principal is entitled to enumerate through the filter surface already (an `eq`/`prefix` probe walk
discloses the same bytes deliberately, masked). The shipped `byte_contains` already early-exits on
those same visible bytes; no new *class* of dependence is introduced.

**Option 4 is different in kind, and arm 9 already measured its shape.** A trigram posting is
derived from the full column; its intersection cost is container-proportional even when the
visible result is empty. Arm 9 measured 2.1 ms for a hidden 10⁷-member scattered posting against
~0.000 ms for an absent value — and a text needle's decomposition into trigrams makes the signal
richer: timing reveals which of a needle's trigrams exist anywhere in the corpus and coarsely how
widely, a monotone corpus-wide quantity nothing publishes (`/v1/meta` publishes types and
operands, identically to every principal — this is not derivable from it). A string column has no
`listing` control to hide behind and §2.2 restores §3.8 for every scanned family. Under the
owner's stated tolerance this is exactly the trade to put up explicitly: a registered C4-shape row
with C8-adjacent content, bought only if the broad-scattered-principal corner is worth 15 GB+ per
column and the row. This memo does not recommend it.

## 6. Trigram postings, priced from measured counts

`textaccel stats` at 10⁸: **1,210 distinct trigrams; 1.22×10⁹ posting entries; 12.25 per value**
(measured — a corpus-wide pass, deterministic). The postings themselves were **not built**; their
size is **modelled** from arm 9's measured 1.25–2.0 B/entry for scattered Roaring postings:
12.25 × 10⁹ entries ≈ **15–25 GB per 10⁹ column** for these 14-byte values, on top of the
~22.5 GB column. Entries grow ≈ (value length − 2), so a prose-length column is where the earlier
100–200 GB estimate lives; that estimate was never measured and still is not. Query cost, honest
version: intersect the needle's rarest trigram postings, then verify each surviving entity against
the flat column — the verify is one random access per survivor, so a common needle's postings
route converges back to the scan's cost while still paying the disk.

## 7. Refused, and negative results

- **Per-value `memmem::Finder`: refuted at this value length.** 111 ns scattered, 12.5 ns broad —
  at or *above* the shipped scalar loop on every cell (the finder's per-call dispatch exceeds the
  search on a ~14-byte haystack). A first pass also verified prefilter survivors through the
  finder and it cancelled the prefilter's whole win (`qzx` scattered 120–135 ns, worse than
  shipped); prefiltered arms verify with the SWAR loop for that reason. SIMD substring search
  earns its keep **only across values** (option 1), not within one.
- **A SWAR inner loop alone: NOT a fix — do not claim it is.** 10–20% on scattered, nothing on
  contiguous (§3). The scalar loop was never the binding term; arm 6's "close to a floor" reading
  of 3.5 ns text equality extends to `contains` on contiguous candidates only via the region
  search, not via a better per-value loop.
- **The 1-byte character bloom (`bloom8`): worthless, as predicted, now measured.** 84–92 ns
  scattered — every value draws from the same small alphabet, so the needle's byte-set is present
  in nearly every value's mask. Recorded so the "cheap one-byte summary" is not re-derived.
- **`i32` text offsets: refuted previously** (arm 10, memo 2026-08-09 §5) — equality 5–15%
  slower, `contains` within noise, capacity capped at 2 GiB concatenated bytes. Unchanged here;
  the decomposition explains it: the offsets line is ~12 of 92 ns on the cell that matters, and
  narrowing it does not remove the line.
- **`textscan`'s distribution was kept, unaugmented.** The surname shape is the adversarial case
  for the *comparison* (shared long prefixes) and a realistic one for the *bloom* (~12 trigrams a
  value); a second distribution was considered and dropped because the three needles already
  bracket the prefilter's best and worst cases, which is what a second distribution would have
  varied. A prose-length-value distribution would change the arithmetic (longer scans, denser
  blooms) — NOT measured, and §6's postings scaling is the only claim made about it.

## 8. What this did not get to

- **The 10⁹ confirmation of the composed path** (§4 carries the three-run medians of the arms;
  the *composed* option-1-plus-3 scan is inferred from the per-shape arms, not run as one binary).
- **Partial presence.** Every cell here is a universal column; the descriptor path composes with
  the affine-rank run merge in the obvious way but no partial-presence text arm has run.
- **Case folding / normalisation.** Everything here is byte-exact, as the shipped scan is. A
  case-insensitive `contains` needs a normalised shadow of the value bytes (≈ doubling the
  column's bytes) or per-candidate folding (≈ doubling the scan) — assumed, not measured, and it
  would double the descriptor's bloom inputs too.
- **Minimum needle length as surface contract.** Needles under 3 bytes get no trigram prefilter —
  the arms degrade to `swar-value`, gracefully but fully. Whether the surface should require ≥ 3
  bytes (as most trigram systems do) is a contract question for `filter-surface.md`, not settled
  here; nothing measured forces it.
- **Run-emission into the result** for the region-search arm on very common needles — its hits
  are pushed per entity; the packed-result-container work landing in `values.rs` concurrently
  with this memo would change that term.

## Method

`textdecomp` and `textaccel` live beside the other arms in
[`layoutprobe/`](../../../probes/2026-08-08-filter-layout/layoutprobe/); raw CSVs are
`run-textdecomp-1e8-{1,2,3}.csv`, `run-textaccel-1e8-{1,2,3}.csv`, `run-textaccel-1e9-{1,2,3}.csv`
and `run-textaccel-stats-1e8.txt` in the probe directory. Values, candidates and the primary
needle are `textscan`'s exactly. Every accelerated arm asserts bitmap equality with the shipped
scan on every (needle, candidate) cell before its timing is taken; `swar_contains` is additionally
fuzzed against `str::contains` over 200,000 random value/needle pairs including empty needles,
needles longer than the value and values spanning the 8-byte SWAR boundary
(`textaccel <n> fuzz`). The 10⁹ runs use a `lite` mode that drops the standalone `bloom8`/`tri16`
arms so the flat column, descriptor and second-stage bloom fit in RAM together; the shipped
baseline runs first and the `ValueColumn` is dropped before the probe's structures are built, so
the peak stays under the machine's 47 GB. Scattered cells are the noisy ones (±10–20% across
runs); differences under ~10% here are not treated as findings anywhere above.

[#44]: https://github.com/jennis0/tessera-index/issues/44
