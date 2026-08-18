# The filter index's lifecycle, on real data

**Date:** 2026-08-10 · **Harness:** [`lifecycle/`](lifecycle/), [`make_fixture.py`](make_fixture.py) ·
**Raw:** [`raw/`](raw/) · **Machine:** WSL2, 12 cores, 47 GB RAM, single-threaded engine passes
**Corpus:** the real arXiv corpus (2,422,486 items) and a 25,000,000-item prefix of the scaled
corpus, both carrying **real** attribute values (§1)

The three write-side paths for the filter index — the per-flush extent (`filter-index.md` §5), the
extent coalesce (§5.2) and the fold's attribute pass (§6.2) — had only ever been exercised by
synthetic fixtures in unit tests. This walks one bundle from a fresh build through 64 flushes, a
coalesce and a fold at each of two scales, checking every answer against the source parquet at
every transition.

## Results

**All three paths are correct on real data at both scales.** Every filter answer at every stage
equals what the corpus says, masked by the principal's own composed candidate; a coalesce changes
no answer at all; a fold changes exactly one thing; and a folded column is **byte-identical** to
what a fresh build over the same live entities writes. The only finding either run produced is one
the harness raised against *itself* (§3.5).

**The lifecycle curve returns to the fresh-build baseline, and the coalesce is what does most of
it.** Against the s0 build, the same filters after 64 flushes, after the coalesce and after the
fold:

| | 2,422,486 | 25,000,000 |
|---|---|---|
| open and compose, fresh build | 6.2 ms | 57.2 ms |
| … after 64 flushes (65 layers/column) | 12.2 ms (**1.95×**) | 63.3 ms (**1.11×**) |
| … after the coalesce (2 layers/column) | 6.6 ms (**1.05×**) | 58.4 ms (**1.02×**) |
| … after the fold (1 layer) | 6.6 ms (**1.05×**) | 60.1 ms (**1.05×**) |
| resident after open, fresh build | 39.2 MB | 391.4 MB |
| … after the fold | 41.2 MB (**1.05×**) | 393.6 MB (**1.01×**) |
| worst query ratio, s1 vs s0 | 34.6× (0.014 → 0.484 ms) | 12.9× (0.057 → 0.734 ms) |
| worst query ratio, s3 vs s0 | 3.45× (0.147 → 0.507 ms) | 1.24× (1.47 → 1.81 ms) |

The large *ratios* are all on queries whose absolute cost is a fraction of a millisecond: the
layered penalty is an additive per-layer constant, so it dominates a cheap query's ratio and
disappears into an expensive one's. Everything here is orders of magnitude inside §2.2's
0.5–1 s filter budget.

**Four figures the design carries as modelled are now measured.** Two are confirmed, one is
refined, and one does not describe what the pass actually does:

- **§5.1's ~9 µs per layer the candidate meets: confirmed.** Measured 7.3 µs (2.4M) and
  10.6 µs (25M) per layer on the scan-routed columns.
- **§5.1's 28 ms per column of open-and-compose at 960 extents: confirmed in order, slightly
  pessimistic.** Measured **18 µs per extent**, at both scales, which is ~17 ms per column at 960.
- **§6.2's fold IO model — "seconds per column, minutes to tens of minutes at ~16 columns at
  10⁹": consistent.** The attribute pass moved 707 MB over five columns in 3.4–3.9 s at 25M, an
  effective ~360–420 MB/s of read-plus-write; 16 `u32` columns at 10⁹ is 128 GB by that rate, 5–6
  minutes.
- **§5.2's "repeated width-8 coalescing walks the same size-tier ladder": NOT what happens — do
  not claim it is.** The decay is **linear, seven extents per column per pass**, not geometric
  (§3.3). The *conclusion* §5.2 draws from it — a steady-state layer count in the tens rather than
  a day's ~960 — still holds, and for a stronger reason than the ladder.

**Two things the campaign found that the design does not say.** Neither is a correctness bug; both
matter to the owner's "resident bytes" objective and to an operator:

- **A `utf8` filter column is fully resident from the moment it is opened**, where a fixed-width
  one is not. §8's residency table was measured over `u32` columns and generalises to them only.
  Measured per column at 25M: a 203 MB `i64` column resides 98 KB at open; a 365 MB `utf8` column
  resides **362 MB** (§3.6).
- **The coalesce grows the bytes on disc while shrinking the layers**, because the consumed extents
  are never unlinked — the in-prefix orphan sweep §5.2 marks ⊘ does not exist. Nine passes took the
  manifest from 320 extents to 5 and the directory from 649 files/72.7 MB to 739 files/**90.4 MB**
  (2.4M). The fold is what reclaims it (§3.3).

---

## 1. What was run, and against what

`make_fixture.py` writes one points file covering `entity_id < 25,200,000`; both scales are
`--limit` prefixes of it (probes/dataset.md §5 rule 1), and the campaign's ingest draws its values
from the rows above the prefix, so a flushed entity's value is as real as a built one's.

**Values are the arXiv corpus's own.** The scaled corpus is that corpus replicated 413 times with a
geometric transform per replica and entity ids assigned replica-block by replica-block, so
`entity_id mod 2,422,486` names the base item an entity replicates; attribute values follow that
identity. Every value at 25M is real and the marginal distribution is the corpus's, ten times over.
**What this does not reproduce is vocabulary growth with scale** — replica 4 introduces no category
the base corpus lacks — so nothing here measures value-set size against corpus size.

Five columns, one per route the design distinguishes:

| column | declaration | route under test |
|---|---|---|
| `archive` | category `u8`, `listing = "public"` | the derived postings (decision 0061) |
| `primary_category` | category `u16`, `listing = "per_viewer"` | the masked scan, and `/v1/categories`' membership |
| `secondary_category` | category `u16`, `listing = "public"` | **partial presence** — absent for 53% of the corpus |
| `first_author` | `utf8` | `eq`, `prefix`, `contains` |
| `submitted_at` | `i64` | `range`, open and closed |

All five are `used_for = ["filter"]` alone. Ten leaf operands plus one two-column conjunction are
answered at every stage; the values they name are taken from the corpus's own histogram, so no
operand is measuring an empty set.

**The oracle is the points file, decoded independently.** Comparing the index against another route
through the index passes for any writer that is wrong the same way twice. **The mask is the
engine's**: an answer is checked against `oracle ∩ candidate` where the candidate is what a real
session composes, because this campaign is about the filter artefact and not about `M_auth`. Where
the subject is the artefact rather than a served answer — the coalesce's content-preservation, the
fold's blanking — the candidate is instead the whole entity range, since a deleted entity leaves the
composed candidate and its *value* is precisely what has to be observed.

The principal holds three terms of the `categories-subclass` label set and sees **11.6%** of the
corpus at 2.4M and **11.8%** at 25M. Medians of three for the masked timings; the artefact-candidate
timings are single runs and are reported for shape, not for comparison.

**The walk**: build → 64 flushes of 2,000 rows (the coalesce held off, so the flush stage measures
layer accumulation alone) → 20,000 deletes → coalesce to exhaustion → fold.

### Why a probe rather than the owed `filter` bench arm

`measurement.md` §7 declares a `filter` arm that has never been built, and the matrix harness is the
right shape for a *cell*: one configuration, repeated, compared with its neighbours. A lifecycle
walk is neither — it is stateful and strictly sequential, each stage's input is the previous stage's
published output, and the quantities of interest are differences between stages of one bundle. As a
matrix arm each stage would rebuild the world, and the coalesce and the fold would have nothing to
act on. **The arm remains owed** for the question it was declared for — what a filter costs per
candidate shape and coverage — which is a cell and is not answered here.

---

## 2. Correctness

Every assertion below held at **both** scales, and at a third configuration built at 1,230,000
specifically to reach the post-build-value case (§2.5).

### 2.1 The fresh build

Ten operands and the conjunction return exactly the corpus's answer under the principal's composed
candidate, across all five families: category equality routed through postings, category equality
scanned, partial-presence category equality, text `eq`/`prefix`/`contains`, and numeric range with
open and closed bounds.

### 2.2 After 64 flushes

- Entities ingested since the build answer on **their own values**, in every family.
- The build's own entities answer **bitmap-identically** to what they answered before any flush —
  asserted as set equality over every operand restricted to `[0, build high-water)`, not by spot
  check, because the failure this guards (an extent's slots read against base entities) shifts
  values along entities and would leave most spot checks passing.

### 2.3 After the coalesce

**Every answer is identical.** Compared as bitmap equality over the whole entity range, per operand,
including the conjunction. The deletions were submitted *before* the coalesce on purpose: §5.2's
claim is that a coalesce retires nothing, and a deleted-but-unfolded entity's value is the case that
separates a content-preserving re-encode from one that quietly executed a removal. It held: those
entities' values were still in the answers after nine passes.

### 2.4 After the fold

**Identical except that the 20,000 deleted entities are gone from every predicate** — asserted as
`s3 == s2 ∖ deleted` per operand, again over the whole entity range.

**Byte-identity holds on real data, for all five columns and at both scales.** For each column the
harness assembles the expected column from the *source parquet* — the surviving entities' values, in
entity order — hands it to `write_value_column`, and compares the file byte for byte with what the
fold left on disc; the presence bitmap likewise, and its **absence** where presence is universal.
This is §6.2's "as close as possible to the single build is byte-identity rather than a tolerance",
checked at scale for the first time, and it is simultaneously the strongest content oracle in the
campaign: a folded column that held the wrong value for one entity in 25 million fails it.

It also settles retention: the folded file is the bytes a build over the survivors alone would
write, so a deleted entity's value bytes are not in it. The independent substring check on the text
column confirmed this at 2.4M; **at 25M it is inconclusive and says so** — under replication every
author name is carried by a surviving entity too, so no deleted value is unique enough to look for
(§3.5).

### 2.5 `/v1/categories` under `per_viewer`

The offered value set equals, exactly, the set of codes carried by an entity inside the principal's
composed candidate — checked after the flushes and again after the fold, against the corpus.

**Including values carried only by post-build entities**, which is what §2.3's extent sweep exists
for. The case is reached at 25M — **5 of the 159 offered values** were carried only by entities
ingested after the build, because the principal's visible set holds those values nowhere else — and
reached far more strongly by a third configuration built at `--limit 1230000`, chosen because
`cs.SY`, `eess.IV`, `econ.TH` and `econ.EM` first appear in the corpus just above it: **21 of 158**.
The offered set was exactly right in every case. At 2.4M the corpus cannot produce the case at all:
the build's prefix is the whole base corpus, so the replication rule leaves no value for a later
flush to introduce.

---

## 3. Cost

### 3.1 The write side

| | 2,422,486 | 25,000,000 |
|---|---|---|
| build (whole bundle) | 5.9 s | 44.4 s |
| attribute artefact written by the build | 69.0 MB in 9 files | 703.3 MB in 9 files |
| flush, median of 64 (2,000-row window) | **34.0 ms** | **32.0 ms** |
| flush, min / max | 30.0 / 56.0 ms | 29.0 / 46.2 ms |
| extent bytes per flush, all five columns | 57.1 KB | 60.0 KB |
| coalesce pass | 25–71 ms | 35–66 ms |
| fold, whole | 1.26 s | 14.4 s |
| fold, `4a attributes` | **417 ms** | **3,896 ms** |
| fold peak RSS (whole process) | 608 MB | 3,569 MB |

The peak RSS row is the **whole harness process**, which holds the oracle's own arrays (~250 MB at
2.4M, ~1.6 GB at 25M) alongside the engine; read it as an upper bound on the fold, not as the
fold's own figure. `raw/fold-*.csv` carries the per-pass resident set the engine reports itself.

**A flush's cost is a function of its window, not of the corpus**: 34.0 ms at 2.4M and 32.0 ms at
25M for the same 2,000 rows, a 10× corpus difference costing nothing measurable. **The attribute
share of that figure is NOT separated — do not claim it is**; the harness times the whole commit
window from `accept_ingest` to publication.

**A flush's extent is ~11 KB per column at 2,000 rows** — 57 KB across all five, values and
presence bitmaps together — two orders of magnitude below §5.2's size `floor` of 1 MiB. That is not a discrepancy with the design, it is the situation the floor was put
there for: without it every tick would mint its own size class and the pass would never fire.

The fold's attribute pass is 33% of the fold's wall clock at 2.4M and 27% at 25M, behind row space
and the external-id pass at the larger scale. The whole fold is reproducible to a millisecond
across the two 25M runs (14,395 and 14,396 ms); the *pass* split moves by ~15%, so read the
staircase as shares rather than as constants.

### 3.2 Query cost across the lifecycle

Masked filter latency, median of three, milliseconds. Full data in
[`raw/query-*.csv`](raw/), including the artefact-candidate variants.

**2,422,486** (principal sees 282,043 entities at s0, 410,043 at s1):

| query | s0 build | s1 +64 flushes | s2 coalesced | s3 folded | s1/s0 | s3/s0 |
|---|---|---|---|---|---|---|
| `archive_eq` (postings route) | 0.037 | 0.122 | 0.132 | 0.052 | 3.30× | 1.41× |
| `archive_in3` (postings route) | 0.245 | 0.749 | 0.656 | 0.243 | 3.06× | 0.99× |
| `primary_eq` (scan, `per_viewer`) | 0.147 | 0.518 | 0.429 | 0.507 | 3.52× | 3.45× |
| `secondary_eq` (partial presence) | 0.014 | 0.484 | 0.566 | 0.018 | 34.57× | 1.29× |
| `author_eq` | 0.458 | 0.641 | 0.584 | 0.794 | 1.40× | 1.73× |
| `author_prefix` | 0.845 | 1.241 | 1.223 | 1.246 | 1.47× | 1.47× |
| `author_contains` | 0.662 | 0.821 | 0.823 | 0.879 | 1.24× | 1.33× |
| `ts_range_closed` | 0.260 | 0.347 | 0.351 | 0.578 | 1.33× | 2.22× |
| `ts_range_open` | 0.256 | 0.353 | 0.336 | 0.612 | 1.38× | 2.39× |
| `composed` (two columns) | 0.238 | 0.350 | 0.299 | 0.385 | 1.47× | 1.62× |

**25,000,000** (principal sees 2,955,893 entities at s0):

| query | s0 build | s1 +64 flushes | s2 coalesced | s3 folded | s1/s0 | s3/s0 |
|---|---|---|---|---|---|---|
| `archive_eq` | 0.251 | 0.622 | 0.522 | 0.357 | 2.48× | 1.42× |
| `archive_in3` | 1.436 | 2.735 | 2.452 | 1.631 | 1.90× | 1.14× |
| `primary_eq` | 1.468 | 1.645 | 1.586 | 1.814 | 1.12× | 1.24× |
| `secondary_eq` | 0.057 | 0.734 | 0.789 | 0.065 | 12.88× | 1.14× |
| `author_eq` | 4.291 | 4.530 | 4.427 | 4.832 | 1.06× | 1.13× |
| `author_prefix` | 8.722 | 8.988 | 9.056 | 9.441 | 1.03× | 1.08× |
| `author_contains` | 5.427 | 5.737 | 5.567 | 6.075 | 1.06× | 1.12× |
| `ts_range_closed` | 2.792 | 2.896 | 3.004 | 3.222 | 1.04× | 1.15× |
| `ts_range_open` | 3.206 | 3.382 | 3.347 | 3.706 | 1.05× | 1.16× |
| `composed` | 2.126 | 2.730 | 2.717 | 2.497 | 1.28× | 1.17× |

Two things to read out of this rather than the ratios:

**The layered penalty is an additive constant per layer, and the ratios are an artefact of dividing
by a small number.** `secondary_eq` costs 0.014 ms fresh and 0.484 ms over 65 layers — a 34× ratio
and a 0.47 ms absolute cost, over 64 extra layers: **7.3 µs per layer**, which at 25M measures
10.6 µs. §5.1's ~9 µs, measured at 10⁹ on an unrelated corpus, is **confirmed**. The same additive
term is invisible on `author_prefix`, where it is 3% of an 8.7 ms scan.

**Drift bounds what can be read out of the folded column.** The 25M walk was run twice, end to end,
and the s3/s0 ratios move between them — `primary_eq` reads 0.77× in the first run and 1.24× in the
second, over identical inputs and the same fold. Treat anything inside ±30% at sub-5 ms as
unresolved by this campaign. What is *not* drift is the two cells where the layered cost was
visible: `secondary_eq` returns from 0.73 ms to 0.065 ms and `archive_in3` from 2.7 ms to 1.6 ms,
in both runs.

### 3.3 What the coalesce actually does

Nine passes at both scales, and the decay is **linear**:

| pass | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
|---|---|---|---|---|---|---|---|---|---|
| extents per column | 57 | 50 | 43 | 36 | 29 | 22 | 15 | 8 | **1** |

Each pass consumes exactly one width-8 window per column, so it removes seven and the count falls by
seven a pass — 64 → 1 in ⌈63/7⌉ = 9. **§5.2's size-tier ladder does not form**, because every extent
this workload produces is far below the 1 MiB floor and therefore in one size class; a coalesced
extent is eight times bigger and still in it. The design's *conclusion* survives, by a different
route: the pass drains seven per tick while ingest adds one, so any backlog is worked off at seven
per tick and the steady state is bounded by the tick, not by the ladder. At the 90 s tick a day's
960 extents would drain in ~3.4 h of ticks — bounded, and slower than the geometric reading of §5.2
implies.

**The bytes on disc grow while the layers shrink.** The consumed extents are not unlinked, because
the in-prefix orphan sweep §5.2 marks ⊘ does not exist:

| | manifest extents | attr files on disc | attr bytes on disc |
|---|---|---|---|
| fresh build | 0 | 9 | 69.0 MB |
| after 64 flushes | 320 | 649 | 72.7 MB |
| after 9 coalesce passes | **5** | **739** | **90.4 MB** |
| after the fold | 0 | 13 | 72.7 MB |

Each pass adds ten files (five columns × values + presence) and orphans the eighty it consumed. The
fold is what reclaims them, which is exactly the debt §5.2 states it is taking on — now with a
number: nine passes orphaned **17.7 MB at 2.4M and 17.8 MB at 25M** — the same figure, because the
orphans are the *extents*, whose size follows the ingest volume and not the corpus. Against the
artefact that is +26% at 2.4M and +2.5% at 25M, so the cost of the missing sweep is a function of
how much has been ingested since the last fold, and is worst on a small bundle under heavy ingest.

### 3.4 Open, compose, and residency across the lifecycle

| stage | 2.4M open | 25M open | layers/column | 2.4M resident after open | 25M resident after open |
|---|---|---|---|---|---|
| s0 build | 6.24 ms | 57.21 ms | 1 | 39.2 MB | 391.4 MB |
| s1 +64 flushes | 12.17 ms | 63.26 ms | 65 | 43.6 MB | 395.8 MB |
| s2 coalesced | 6.58 ms | 58.36 ms | 2 | 41.6 MB | 393.7 MB |
| s3 folded | 6.56 ms | 60.11 ms | 1 | 41.2 MB | 393.6 MB |

Composing 320 extents costs 5.9 ms at 2.4M and 6.1 ms at 25M — **18 µs per extent**, independent of
corpus size, as a per-file cost should be. §5.1's modelled 28 ms per column at 960 extents is the
right order and a little pessimistic: 17 ms by this constant.

**The coalesce alone restores the open path to the fresh-build baseline**; the fold adds nothing
measurable to it. That is the sharpest operational result here — the file-count axis genuinely no
longer waits for a fold, which is what §5.2 was built to achieve.

### 3.5 The one finding the campaign raised

`25000000/blanking/first_author: no deleted entity carried a value unique to it, so the
bytes-are-gone check proved nothing`. This is the harness refusing to report a vacuous pass, not a
defect in the system: under 10× replication every deleted author name is also carried by a survivor,
so its bytes are legitimately in the folded column and a substring search proves nothing either way.
The claim it was checking is covered by byte-identity (§2.4), and the substring check *did* run
conclusively at 2.4M.

### 3.6 Residency is a function of the column's family, not of its size

Measured per column at 25M, opening each on its own between two reads of the process's resident set
([`raw/residency-25000000.csv`](raw/residency-25000000.csv)):

| column | file | resident after open |
|---|---|---|
| `archive` `u8` | 28.1 MB | 143 KB |
| `primary_category` `u16` | 53.1 MB | 111 KB |
| `secondary_category` `u16` (partial) | 25.0 MB | 115 KB |
| `submitted_at` `i64` | 203.1 MB | **98 KB** |
| `first_author` `utf8` | 365.5 MB | **362.5 MB** |
| the three postings files | 28.5 MB | 28.5 MB (read, not mapped) |

**A fixed-width column behaves exactly as §8's table says**: mapped, ~100 KB resident at open
whatever its size. **A `utf8` column does not** — 99.2% of it is resident the instant it opens,
because the reader constructs a `LargeStringArray` over the mapped buffer and that validates UTF-8
across every byte (`values.rs`, `read_values`). The validation is deliberate and is what lets the
scan view by offset without re-validating; what is not stated anywhere is that it makes a text
column's residency its *size*. At 10⁹ that is ~14 GB for one text column, against the 2 MB §8's
table would lead a reader to size for.

The derived postings are read rather than mapped, so they are resident in full too — 28.5 MB here,
and proportional to the vocabulary and the corpus.

Neither is a correctness problem and neither changes across the lifecycle. Both belong in §8's
table, which currently generalises a `u32` measurement to every family.

---

## 4. What this did not measure

Stated plainly, because each is a claim someone could otherwise read this campaign as supporting:

- **The attribute share of a flush**: NOT separated from the commit window's total.
- **Peak RSS per column** during the fold: NOT measured. The staircase gives per-*pass* figures
  (`raw/fold-*.csv`) and the harness samples the process peak; neither attributes memory to a column.
- **Non-disruption**: the fold's cost to a *concurrent* reader is NOT measured — this campaign is
  strictly sequential, so §6.2's "behaves like P4's fold rather than P3's reader" remains reasoning
  from shape, exactly as it is marked.
- **A scattered, high-coverage principal**: NOT measured. Coverage was 11.6–11.8% and the label set
  is category-derived, so the candidates here are comparatively contiguous — the corner §2.2 names
  as the one an accelerator would address is untouched.
- **Vocabulary growth with scale**: NOT measurable on this corpus (§1).
- **10⁹**: not attempted. Every figure at that scale in this document is an extrapolation and is
  labelled as one.
- **A partial-presence column that is genuinely scattered**: `secondary_category`'s presence
  compresses to 8.5 KB over 25M entities, because the entity order is the authorisation signature
  sort and the labels are category-derived, so presence falls into runs. That is a real property of
  this corpus and a favourable one; a deployment whose filter columns do not correlate with its
  labels would pay §2.1's 1.25 B/present instead.

## 5. What to measure next

1. **The residency of a text column at 10⁹**, and whether the UTF-8 validation at open can be
   deferred to first touch as §8's digest-sweep deferral is. This is the largest gap between what
   §8 promises and what a schema with one string column costs.
2. **The owed `filter` bench arm**: cost per candidate shape and coverage, over the shipped scan, as
   a matrix cell — which is what would put a scattered high-coverage principal on record.
3. **A fold with a concurrent reader**, which is the half of "without disrupting serving" no
   campaign has touched for this pass.
4. **The coalesce under continuous ingest** rather than a drained backlog: the policy's steady state
   is the interesting number and this campaign only measured its transient.
5. **The orphan bytes over many fold cycles** — 17.7 MB per nine passes here, but the campaign ran one
   fold, and the question is whether the sweep's absence compounds.

---

## Re-running

```bash
# 1. The fixture: one points file, three vocabularies, one schema. ~20 minutes cold,
#    seconds with geometry.parquet in page cache.
reference/.venv/bin/python probes/2026-08-10-filter-lifecycle/make_fixture.py \
    --data /path/to/data --total 25200000

# 2. The walk, per scale. ~12 minutes at 2.4M, ~45 at 25M.
cd probes/2026-08-10-filter-lifecycle/lifecycle && cargo build --release
./target/release/lifecycle --scale 2422486  --limit 2422486  \
    --flushes 64 --batch 2000 --deletes 20000 --out ../raw --work /path/to/work
./target/release/lifecycle --scale 25000000 --limit 25000000 \
    --flushes 64 --batch 2000 --deletes 20000 --out ../raw --work /path/to/work

# 3. The post-build-value configuration, and the per-column residency measurement.
./target/release/lifecycle --scale 1230000-novel --limit 1230000 \
    --flushes 16 --batch 2000 --deletes 4000 --out ../raw --work /path/to/work
./target/release/lifecycle --scale 25000000 --limit 25000000 --residency \
    --out ../raw --work /path/to/work

# 4. The tables above.
python3 probes/2026-08-10-filter-lifecycle/summarise.py raw
```

`raw/verdict-<scale>.csv` is the correctness result: `all checks passed`, or one line per
disagreement naming the stage, the operand and the size of the difference. A run that finds nothing
prints `ALL CHECKS PASSED` and exits 0.

The harness needs `tessera-engine`'s `fault-injection` feature, which is what lets the flush stage
hold the coalesce off; a curve measured against a policy that fired mid-stage cannot separate the
two. It touches no shipped code.
