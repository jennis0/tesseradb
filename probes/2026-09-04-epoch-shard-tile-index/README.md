# Epoch shards and the tile index

**Date** 2026-09-04. **Branch** `worktree-agent-adfa08f0fcde3fcce`. **Bin**
[`epoch_shard_tile_index`](../../crates/tessera-bench/src/bin/epoch_shard_tile_index.rs). **Box** WSL2,
12 cores, 47 GB, with another session's rung 5 ingest running throughout; every run under `nice -n 10`
with `RAYON_NUM_THREADS=3`. **Raw numbers** [`result.json`](result.json). Every figure below is
measured unless marked modelled.

## The result

At N = 8 on the rung 3 bundle's `mesh/descriptors` level (35.9 M points, 30,217 artifacts,
1.66 × 10⁹ memberships), against N = 1:

| | contiguous | hash | threshold |
|---|---:|---:|---|
| index bytes summed over shards | **8.83×** | **8.53×** | |
| ordinals with a `Span` extent, summed over shards | **6.21×** | **7.94×** | |
| candidacy at depth 12, time summed over shards | **6.35×** | **6.46×** | 2× |
| candidacy at depth 12, candidates summed over shards | 5.97× | 7.80× | |
| histogram walk summed over shards | 1.25× | 0.93× | |

**The 2× threshold at depth 12 is reached at N = 2 and passed by 3× at N = 8.** The reason is the
`everywhere` set. 29,723 of the level's 30,217 artifacts straddle the coarsest index node at N = 1,
and a shard keeps almost all of them: 27,000 to 28,700 per shard under the contiguous rule and
29,037 to 29,081 under the hash rule. A depth-12 viewport of a few dozen rows returns about 29,900
candidates from one shard, so it returns about N × 29,900 from N shards, and each candidate then
owes a masked probe that this probe did not time. The index bytes grow by N because the extent
column is 8 bytes per ordinal per shard and the node bitmaps hold the same ordinals N times over.
The histogram walk is unchanged because the same 1.66 × 10⁹ entries are walked whichever shard they
sit in.

The two spatial k-means layers give the same answer under the hash rule (7.5 to 8.7× bytes, 7.5 to
8.0× span ordinals, 5.2 to 7.6× depth-12 candidacy) and a smaller one under the contiguous rule on
PaperSeek (6.1×, 3.0×, 2.3×), where entity order is correlated with map position and a contiguous
range of ids holds part of the map. That is the optimistic case and it is still above 2×.

## Question

Splitting a corpus into N epoch shards inside one process gives each shard its own `u32` entity
space and its own Morton-ordered `u32` row space. Points are assigned to a shard by when they were
allocated, so a spatial cluster has members in every shard. The tile index
(`crates/tessera-engine/src/tile_index.rs`) would then hold one `Extent` per (shard, ordinal) and
its `own` and `subtree` bitmaps once per shard, and `TileIndex::candidates` and `TileIndex::inside`
would run once per shard per request. The question is what that costs in index bytes, in how many
artifacts touch every shard, in candidacy time at four viewport depths, and in the histogram walk
(`RowColumn::histogram_over`).

## Method

A simulation in one bench binary; nothing in the engine changes. Both fixtures are opened read-only.

- A shard is a real `Permutation` file, written with the store's own writer
  (`write_permutation_iter`) over the shard's rows renumbered densely in their existing Morton
  order, loaded with `Permutation::load` and wrapped as a `RowSpace`. Its index is
  `TileIndex::project` over the level's records with each membership restricted to the shard's
  entities. At N = 1 the written file is byte-identical to the bundle's `permutation.bin` on both
  fixtures (`checks.permutation_identical`).
- Two shard rules. `contiguous` takes equal ranges of entity id: a build assigns ids in
  term-signature order, so the members of a term-defined artifact sit close in id space, and this
  is the optimistic model. `hash` assigns each id by a splitmix64 mix and is the null model for
  epochs whose contents are uncorrelated with any artifact. N = 1 is the same shard under either
  rule and is run once.
- Bytes are `TileIndex::as_bytes().len()` (the extent column, 8 bytes per ordinal plus a 16-byte
  header) plus the Portable serialised size of every `own` and `subtree` node bitmap and of the
  `everywhere` set. The engine keeps the node maps private, so the bin places each extent by the
  engine's rule (`tile_index_shifts`, the finest level whose block holds both ends of the span) and
  asserts its `everywhere` count equal to the engine's `TileIndex::everywhere`. Serialised size,
  not allocator residency: Roaring's in-memory form is larger by its container headers.
- A viewport at depth d is one tile's row range, found by binary search over `morton.u32` for the
  tile holding a row drawn at random (seed `0x5EED`); eight distinct tiles per depth at 4, 8 and
  12, the whole map at 0. The per-shard viewport is that range mapped to the shard's local rows.
  The timed prefix is `candidates` and then `inside` for every candidate the walk did not settle,
  which is what `ArtifactRows::candidate_in` does before its masked probe. Each cell is the median
  of five repetitions of the sum over shards, averaged over the depth's tiles.
- The histogram is `RowColumn::histogram_over` on a row-major list column with a mask of the whole
  shard, median of five. A column is composed per shard with `RowColumn::project` where the
  modelled transient (4 bytes per entry, 8 per row, the packed column) plus the resident set stays
  under the budget; at N = 1 on the mesh level the fold-written column is opened instead, because
  composing one is an 11 GB transient.

N = 1 reproduces the engine's index. On both k-means layers the projected index is byte-identical
to the fold-written `tile-index-*.tsti` the manifest lists at the current level version, and to
`TileIndex::build` over `MembershipRows::build` of the same records (`checks.fold_written`,
`checks.build_identical`). The mesh level is served row-major and has no fold-written index; its
N = 1 index is `TileIndex::project`, the route the fold takes, which `TileIndex::build` equals by
construction (the row form stores `project_base`'s output and the extent is its minimum and
maximum). `TileIndex::build` was not run on that level because the row form is a further 3 to 4 GB
resident.

Run, from the repository root, with `$B` the built binary and `$R` the rung 3 bundle:

```
RAYON_NUM_THREADS=3 nice -n 10 $B --fixture $R --layer mesh/descriptors --scratch /tmp/perm \
  --json mesh.json --histogram --histogram-budget-gb 7 \
  --n1-row-column $R/v00000/partitions/default/row-column/row-column-000000-000.tsll
RAYON_NUM_THREADS=3 nice -n 10 $B --fixture $R --layer clusters/kmeans --scratch /tmp/perm \
  --json kmeans.json --histogram --verify-build
RAYON_NUM_THREADS=3 nice -n 10 $B --fixture data/ladder/paperseek/bundle-10m --layer clusters/kmeans \
  --scratch /tmp/perm --json paperseek.json --histogram --verify-build
```

## Rung 3, `mesh/descriptors`

35,920,666 rows, 30,217 ordinals, all live, 1,658,437,802 memberships, served row-major. Peak
resident set 5.97 GB (6.11 GB for the second process, below).

**(a) index bytes summed over shards**

| rule | N | extents | node bitmaps | total | vs N=1 |
|---|---:|---:|---:|---:|---:|
| contiguous | 1 | 0.242 MB | 6.9 kB | 0.249 MB | 1.00× |
| contiguous | 2 | 0.484 MB | 77.9 kB | 0.561 MB | 2.26× |
| contiguous | 4 | 0.967 MB | 0.144 MB | 1.111 MB | 4.47× |
| contiguous | 8 | 1.934 MB | 0.262 MB | 2.196 MB | 8.83× |
| hash | 2 | 0.484 MB | 61.6 kB | 0.545 MB | 2.19× |
| hash | 4 | 0.967 MB | 53.9 kB | 1.021 MB | 4.11× |
| hash | 8 | 1.934 MB | 0.186 MB | 2.120 MB | 8.53× |

The extents are exactly N × 8 bytes per ordinal. The node bitmaps grow faster than N because a
shard's row space has a coarser ladder: at 4.49 M rows the coarsest level is shift 20 (five root
blocks of 1 M rows) where the whole space has shift 24 (three root blocks of 16.8 M rows), so the
same artifacts spread over more nodes.

**(b) ordinals with a `Span` extent, per shard**

| rule | N | per shard | sum | vs N=1 | in every shard | `everywhere` per shard |
|---|---:|---|---:|---:|---:|---|
| contiguous | 1 | 30,217 | 30,217 | 1.00× | 30,217 | 29,723 |
| contiguous | 2 | 30,123, 28,164 | 58,287 | 1.93× | 28,070 | 22,859, 12,903 |
| contiguous | 4 | 29,757, 30,059, 28,013, 26,023 | 113,852 | 3.77× | 25,539 | 29,143, 29,667, 27,186, 23,536 |
| contiguous | 8 | 29,178, 29,606, 29,789, 27,968, 27,908, 17,243, 26,023, 3 | 187,718 | 6.21× | 3 | 27,822, 28,530, 28,662, 27,113, 26,718, 15,911, 23,087, 0 |
| hash | 2 | 30,185, 30,190 | 60,375 | 2.00× | 30,158 | 20,229, 20,272 |
| hash | 4 | 30,116, 30,112, 30,123, 30,124 | 120,475 | 3.99× | 29,966 | 29,688, 29,704, 29,690, 29,709 |
| hash | 8 | 29,984, 29,985, 29,997, 29,986, 29,982, 29,986, 29,977, 29,988 | 239,885 | 7.94× | 29,565 | 29,060, 29,066, 29,037, 29,045, 29,046, 29,070, 29,081, 29,068 |

Under the contiguous rule the eighth shard holds 18 memberships and 3 spans: the last eighth of
this build's entity ids is the articles with no MeSH descriptor, which sort last by signature. That
one shard is the whole of the difference between the two rules at N = 8. The `everywhere` count per
shard at N = 2 (about 20,000 of 30,000) is lower than at N = 1 because a 17.96 M-row space has one
root-block boundary at 16.8 M rows instead of two; at N = 4 and 8 the ladder steps down and the
count returns to about 29,000.

**(c) candidacy summed over shards: walk plus extent test, in µs, mean over tiles of the median of five**

| rule | N | d0 µs | vs N=1 | d0 candidates | d4 µs | vs N=1 | d4 candidates | d8 µs | vs N=1 | d8 candidates | d12 µs | vs N=1 | d12 candidates | vs N=1 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| contiguous | 1 | 25,266 | 1.00× | 30,217 | 384 | 1.00× | 29,924 | 356 | 1.00× | 29,944 | 332 | 1.00× | 29,921 | 1.00× |
| contiguous | 2 | 13,814 | 0.55× | 58,287 | 794 | 2.07× | 56,707 | 678 | 1.90× | 56,659 | 654 | 1.97× | 56,622 | 1.89× |
| contiguous | 4 | 18,002 | 0.71× | 113,852 | 1,356 | 3.53× | 110,077 | 987 | 2.77× | 109,941 | 982 | 2.96× | 109,734 | 3.67× |
| contiguous | 8 | 18,757 | 0.74× | 187,718 | 2,966 | 7.73× | 179,705 | 1,710 | 4.80× | 179,241 | 2,109 | 6.35× | 178,596 | 5.97× |
| hash | 2 | 15,342 | 0.61× | 60,375 | 802 | 2.09× | 57,617 | 708 | 1.99× | 60,031 | 728 | 2.19× | 60,028 | 2.01× |
| hash | 4 | 20,803 | 0.82× | 120,475 | 1,433 | 3.74× | 119,008 | 1,046 | 2.94× | 118,945 | 1,042 | 3.14× | 118,886 | 3.97× |
| hash | 8 | 37,049 | 1.47× | 239,885 | 2,651 | 6.91× | 233,920 | 2,001 | 5.62× | 233,373 | 2,145 | 6.46× | 233,516 | 7.80× |

The depth-0 column is the extent test: with the whole map in view every `everywhere` artifact
passes `inside`, whose `contains_range` walks the artifact's span, and a shard's spans are shorter.
At the other depths nothing is settled and nothing passes `inside`; the time is the walk over a few
dozen nodes and the union of the `everywhere` set with the nodes touched, once per shard.
Candidate counts are exact; the times carry run-to-run noise of about ±30% on this box (a second
process, below, measured 258 µs for the N = 1 depth-12 cell against 332 µs here, and 1,376 µs for
contiguous N = 4 against 982 µs).

**(d) histogram walk summed over shards, `histogram_over` on the whole shard, three rayon threads**

| rule | N | ms | vs N=1 | entries | column |
|---|---:|---:|---:|---:|---|
| contiguous | 1 | 1,137 | 1.00× | 1,658,437,802 | fold-written `.tsll` opened |
| contiguous | 2 | – | – | – | not composed: modelled 6.83 GB transient over 3.85 GB resident |
| contiguous | 4 | 766 | 0.91× | 1,658,437,802 | composed per shard, second process (N = 1 there: 838 ms) |
| contiguous | 8 | 1,420 | 1.25× | 1,658,437,802 | composed per shard |
| hash | 2 | – | – | – | not composed: modelled 5.12 GB transient over 4.45 GB resident |
| hash | 4 | 900 | 0.79× | 1,658,437,802 | composed per shard |
| hash | 8 | 1,057 | 0.93× | 1,658,437,802 | composed per shard |

The fold-written column is at level version 1 and the level is now at version 3; its entry count
equals the current membership count, so the walked work is the same. The contiguous N = 4 row is
from a second process running that combination alone with a 7.5 GB budget; its candidacy figures
are in `result.json` under `mesh_contiguous_4_rerun`. The N = 2 columns were not composed because
the engine's `project_row_column` holds one `u32` per entry and the packed column at once, and the
process was kept under 8 GB resident.

## Rung 3, `clusters/kmeans`, and PaperSeek 10⁷, `clusters/kmeans`

Spatial k-means levels, 256 and 250 artifacts, one membership per row. Both indexes at N = 1 are
byte-identical to the fold-written index and to `TileIndex::build`. Peak resident 1.02 GB and
0.27 GB. The µs cells are small enough that their ratios are noisier than the candidate counts.

| fixture | rule | N | bytes vs N=1 | span sum (vs N=1) | in every shard | d12 µs (vs N=1) | d12 candidates (vs N=1) | histogram ms (vs N=1) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| rung 3 kmeans | – | 1 | 4.5 kB | 256 | 256 | 2 | 116 | 108 |
| rung 3 kmeans | contiguous | 2 | 1.98× | 503 (1.96×) | 247 | 5 (2.39×) | 277 (2.37×) | 102 (0.94×) |
| rung 3 kmeans | contiguous | 4 | 3.77× | 1,000 (3.91×) | 243 | 9 (3.98×) | 472 (4.06×) | 105 (0.97×) |
| rung 3 kmeans | contiguous | 8 | 7.52× | 1,923 (7.51×) | 182 | 13 (6.01×) | 720 (6.18×) | 140 (1.29×) |
| rung 3 kmeans | hash | 2 | 2.01× | 512 (2.00×) | 256 | 5 (2.36×) | 296 (2.54×) | 102 (0.94×) |
| rung 3 kmeans | hash | 4 | 3.86× | 1,022 (3.99×) | 255 | 9 (4.08×) | 460 (3.95×) | 104 (0.96×) |
| rung 3 kmeans | hash | 8 | 7.76× | 2,041 (7.97×) | 254 | 16 (7.60×) | 859 (7.37×) | 144 (1.33×) |
| PaperSeek kmeans | – | 1 | 4.0 kB | 250 | 250 | 2 | 121 | 30 |
| PaperSeek kmeans | contiguous | 2 | 1.94× | 420 (1.68×) | 170 | 3 (1.51×) | 168 (1.38×) | 35 (1.20×) |
| PaperSeek kmeans | contiguous | 4 | 3.35× | 538 (2.15×) | 2 | 4 (1.71×) | 176 (1.45×) | 70 (2.38×) |
| PaperSeek kmeans | contiguous | 8 | 6.06× | 743 (2.97×) | 0 | 5 (2.30×) | 218 (1.80×) | 83 (2.81×) |
| PaperSeek kmeans | hash | 2 | 2.04× | 497 (1.99×) | 247 | 4 (1.76×) | 206 (1.70×) | 35 (1.20×) |
| PaperSeek kmeans | hash | 4 | 4.33× | 984 (3.94×) | 241 | 7 (3.16×) | 379 (3.13×) | 70 (2.38×) |
| PaperSeek kmeans | hash | 8 | 8.70× | 1,943 (7.77×) | 236 | 12 (5.19×) | 638 (5.26×) | 89 (3.03×) |

The full (a) to (d) tables for both are in `result.json`. On PaperSeek the contiguous rule places
44 to 236 clusters per shard and no cluster in every shard at N = 8: that build's entity order
follows the term signature, which for a topic-clustered map follows position, so a range of ids is
a region of the map. The PaperSeek histogram rises with N for a different reason: `histogram_over`
chunks the row space at 2²¹ rows per chunk, so a 1.25 M-row shard walks on one thread where the
10 M-row space walks on three, and the sum of wall times rises although the work does not.

## What it says

1. Every per-shard structure the request touches is repeated N times, and on a term-defined layer
   nothing drops out: the hash rule keeps 99% of the artifacts in every shard, and the contiguous
   rule keeps 93% outside the one shard that holds the unindexed tail. The bytes ratio is N and the
   span-ordinal ratio is close to N.
2. Candidacy at depth 12 is about 6× at N = 8 on every fixture and rule except PaperSeek
   contiguous (2.3×), and it passes 2× at N = 2. On the mesh level it is the `everywhere` set
   returned N times over; the masked probes those candidates owe, which this probe did not time,
   scale with the candidate count, which is 6 to 8× at N = 8.
3. The histogram walk is about 1× on the large level. It is a walk over entries and sharding moves
   entries between shards without adding any. On a small level the sum of wall times rises because
   a shard falls below the parallel walk's chunk size.
4. The index is small in absolute terms at this scale: 2.2 MB for eight shards of the 30,217-artifact
   level. The cost that grows with N is the per-request candidate set, not the bytes.

## Caveats

The shards are simulated over the one build segment of each bundle, with no flushed extents and no
tail, and a shard's row space is the bundle's Morton order restricted to its entities, which is
what an epoch that was built as one segment would hold but not what a sequence of flushes and
merges would. The hierarchy bytes are re-derived outside the engine by the same placement rule and
checked against the engine on the `everywhere` count only; the extent bytes are the engine's own.
Timings were taken on a box running another session's ingest at load 5 to 10, as medians of five
over eight tiles; two processes on the same cells differed by up to 40% in time and by nothing in
candidate count, so the candidate columns are the ones to rely on. The masked probe per candidate,
which is what a candidate costs downstream, was not timed. The N = 2 histogram cells on the mesh
level were not run for memory, and its N = 1 histogram is over a column at an earlier level version
with the same entry count. The contiguous rule's advantage on this build comes from where its
entity order put the unindexed articles and from PaperSeek's topic order, and neither says anything
about how a running system's epochs would be filled; the hash rule is the figure to plan against.
