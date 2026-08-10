# Crossing entity space and row space for a filtered viewport

**Date:** 2026-08-11 · **Harness:** [`crossing/`](crossing/) · **Raw:**
[`run-crossing-1e8.csv`](run-crossing-1e8.csv) · **Scale:** 10⁸, single-threaded, four pinned cores
on a loaded machine — read the *ratios*, not the absolute milliseconds.

## Results

**1. `filter-surface.md` §4's crossover rule is wrong, and wrong in the direction that costs
latency.** The rule says *project only when the result is smaller than about a quarter of the
viewport's row count*. Measured here, project stays cheaper until the result is **1× the viewport's
rows** (contiguous results) or **3–5×** (scattered) — so the rule sends work to the per-tile route
between 4× and 20× too eagerly.

**2. The reason is that arm 3 measured a route the system cannot build.** Its per-tile test indexes
a materialised `row_to_entity` array, and no such array exists: `permutation.bin` is entity→row, and
row→entity has to go through the row's `tessera_id` and `IdentityKey::invert`. That inversion is a
4-round Feistel and it costs **~17–25 ns per row**, which is the dominant term wherever the rest of
the loop is cheap:

| per-tile, per viewport row | idealised (`row_to_entity`) | real (`tessera_id` + `invert`) |
|---|---|---|
| contiguous result | **2.2–9.2 ns** | **19.7–29.1 ns** |
| scattered result | 34.4–59.6 ns | 56.9–106.1 ns |

Arm 3's published 6–22 ns/row is the *idealised, contiguous* cell. The real route is **3–9× dearer**
there, and the realistic regime for an ingest-ordered column — a scattered result — is dearer again.

**3. A Morton-cell → entity pre-filter is refuted, on both axes.** The idea: a second bitmap per
coarse Morton cell holding the *entities* whose rows fall in it, so a filtered viewport becomes
`result ∧ (cells the viewport touches)` and only the survivors are projected — everything in entity
space, no per-row crossing. Measured it is **the slowest route at every one of the sixteen cells**,
by 4–400×, and its structure is **larger than the inverse permutation it emulates**:

| cell width | cells | serialised | per entity |
|---|---|---|---|
| 1,000 rows | 100,000 | 751.3 MB | **7.88 B** |
| 10,000 rows | 10,000 | 307.1 MB | **3.22 B** |

against 4 B/entity for a plain `row_to_entity` array carrying the same information. The cause is the
one the structure cannot escape: **a cell's entity set is scattered in entity space**, because entity
ids are assigned in permission-signature order and are uncorrelated with position. Scattered is
Roaring's worst case — the union over the viewport's cells touches essentially every container, so
the pre-filter costs O(corpus) rather than O(viewport), which is the whole thing it was meant to buy.
The property that makes authorisation postings compress works directly against it.

*(A first run sized the cells at a fixed 4,096 regardless of tile width, making the pre-filter 24×
too coarse at 1,000-row tiles. The table above is the re-run with cells matched to the tile width,
which is the structure's best case.)*

## The corrected picture

| | cost | scales with |
|---|---|---|
| project | ~20–30 ns per set bit *(≈ arm 3's 27 ns — the two arms agree here)* | the result |
| per-tile, real | ~20–29 ns per row contiguous, ~57–106 ns scattered | the viewport |
| coarse pre-filter | ~256–450 ns per viewport row | neither, in practice |

At a 300,000-row viewport and 10⁸ items, scattered:

| result | project | per-tile (real) |
|---|---|---|
| 10⁴ | **1.16 ms** | 18.3 ms |
| 10⁵ | **5.22 ms** | 17.5 ms |
| 10⁶ | 35.4 ms | **24.2 ms** |
| 10⁷ | 216.2 ms | **31.8 ms** |

So the two-route design still stands and the per-tile route still wins decisively for broad filters —
216 ms against 32 ms at a 10⁷ result. What changes is **where the switch sits**: between 10⁵ and 10⁶
against a 300,000-row viewport, not at the 75,000 the quarter-rule would give.

## What this does not settle

- **Scale.** 10⁸, not 10⁹. Arm 3's project constant agrees across the two scales, which is the one
  cross-check available; the per-tile constants are unverified at 10⁹.
- **The `tessera_id` read is modelled as an indexed gather over a `Vec<u64>`**, not as a read of a
  mapped Arrow column with a candidate-driven access pattern. The Feistel cost is real; the memory
  term around it is optimistic if anything.
- **The candidate is universal.** Every route here tests every row in the viewport, where the engine
  would test only the visible ones. That favours no route in particular but shrinks every absolute
  figure.
- **Single-threaded**, on a machine running a browser at ~800% CPU throughout. Ratios were stable
  across three rounds; the absolutes are a ceiling on a quiet machine.
- **Whether the per-tile route can amortise the inversion.** 17–25 ns per row is a Feistel per row.
  Nothing here tested inverting once per *tile* into a reusable buffer, or a cheaper crossing.
