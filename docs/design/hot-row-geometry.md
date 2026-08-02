# Hot-row geometry — cell plus residual

**Date:** 2026-08-02
**Status:** **Normative.** The §6 amendments are folded into `architecture.md`, `contracts.md`
and `conformance.md`; the representation is built end to end from the importer to the client. One
piece named here is specified and not built and says so at the claim: the binding between a bundle
and the points file the oracle reads it against (§5.2).
**Reads against:** architecture §4 (I9, I10), §5.3, §7.2, §10.3, §10.5, Appendix A; contracts §0.3
(deviations 5, 9, 10), §0.4, §2.5, §2.6, §3.2, §3.4; `conformance.md` §1, §7.
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **spec §n**.

---

## 1. Summary

A point's position is stored in **two columns, neither of them a packed 64-bit code**: the 32-bit
Morton cell code in `morton.u32` — sorted, searched, byte-identical to today — and a 32-bit
`residual` column in `columns.arrow` replacing the `x`/`y` `f32` pair. Concatenating them yields the
64-bit interleave, which is what goes on the wire (§4) and nowhere else.

Storage capacity rises to **32 bits per axis** (§2.2 — what a given corpus realises depends on the
corpus), `columns.arrow` falls from 18 to 14 B/row, and the client receives the Morton cell
alongside the position for free.

---

## 2. The representation

```
morton.u32              [ x31..x16 interleaved with y31..y16 ]   cell: sort key, tile lookup
columns.arrow.residual  [ x15..x0  interleaved with y15..y0  ]   sub-cell position
wire code             = (morton as u64) << 32 | residual
```

The split is coherent because interleaving is bit-local: bit *i* of an axis lands at a fixed
position of the code regardless of the other bits, so the high half of a 64-bit interleave over
32-bit coordinates *is* the existing 32-bit interleave over the high 16-bit halves — unchanged in
construction as well as in width.

`residual` is an Arrow column rather than a raw file beside `morton.u32` because it is gathered by
row index: no new mapping on the gather path, no manifest entry, no digest, and positional alignment
with `tessera_id` comes free from the record batch.

**Row order is unchanged** — `(morton, tessera_id)`, so the residual varies arbitrarily within a
cell and is not a sort key. §7.2's `served(T)` is the *m* smallest by `tessera_id`, and the
selection route reserves an exact sub-Σvisible evaluation that rests on the identity column being
sorted within a cell (⊘ specified, not built). Both depend on that order; neither is affected here.

### 2.1 Why not one packed column

A single `code: u64` doing both jobs is **sound** — worth stating, because the obvious objection to
it is wrong and has been made twice. Binary search needs a *partitioned predicate*, not a sorted
array: every search target is a cell boundary, so `code >> 32 < lo` is exactly `morton < lo`, and
`morton` is non-decreasing. The column being unsorted within a cell is below the resolution the
search can see.

It is rejected anyway. It saves one cache line per gathered point — a few thousand misses against a
request measured in tens to low hundreds of milliseconds — and costs a file that is simultaneously
*in row order* and *searched as though sorted*, reconcilable only by the argument above. Split, the
searched file is genuinely sorted and the reader validates it: the file's invariant and its storage
order are the same thing.

### 2.2 Precision, and where it is actually lost

**The capacity is 32 bits per axis. Whether a deployment gets it depends on its corpus, not on
this design.**

The importer already refuses to fake precision it was not given: the Morton-input branch requires
the identity extent `[0, 65536)`, under which `cell(v) = v` and the reproduction is *exact*, and
errors under any other extent rather than re-quantising cell indices as though they were
coordinates. That guard is tested.

The loss is upstream. `build_scaled_corpus.py` reads `["entity_id", "morton"]` from
`geometry.parquet` — which carries `x`/`y` — and its 64-bit sort key is `morton << 32 | entity_id`,
so **no sub-cell information survives into any 10⁸ or 10⁹ corpus and none is recoverable from one.**
Those corpora hold 16 bits per axis and the pipeline is faithful to them.

So on a 360-unit extent the step is **5.5×10⁻³** units for a Morton-sourced corpus against
**8.4×10⁻⁸** for a coordinate-sourced one (arithmetic, not measured). `geometry.parquet` itself
takes the `x`/`y` branch and already gets `f32`; only the scaled corpora are grid-resolution.

Realising the gain at scale therefore means **regenerating the scaled corpora from coordinates**,
which is probe-script work, not engine work. This design supplies the capacity and the 4 B/row
saving regardless; it does not by itself make any existing corpus more precise.

**Neither wire bounds the result.** The points batch carries the full code, so nothing is clipped
outbound; and the batch build reads Parquet directly, so `read_f32_column`'s `Float64 → f32`
narrowing is an importer choice, not a contract. `/control/ingest`'s `f32` fields cap **streamed**
points at a 24-bit mantissa — below what storage holds, separately fixable, noted so the asymmetry
between built and streamed points is not discovered later.

### 2.3 Cost

The gather is **neutral**: a position needs `morton[idx]` and `residual[idx]`, the same two loads as
`x[idx]`/`y[idx]`, sweeping the same row span.

The saving is residency. Against §10.5's 0.93 GiB per byte per row per 10⁹ items, `columns.arrow`
falls 18 → 14 B/row: **18 GB → 14 GB** at 10⁹.

*`morton.u32` appears in neither Appendix A table, though it is mapped and searched on every
request — 4 GB at 10⁹. That predates this design and is not fixed here.*

---

## 3. Format and versions

**No `bundle_format` bump** — contracts §0.3 deviation 5, format 1 has never been published. Safe in
place because `validate_schema` compares `FIXED_COLUMNS` by name *and* type, so an old bundle
against a new reader is a typed error at open, not a misread.

**`api_version` stays at 1**, annotating deviation 10 rather than adding a deviation. Deviation 10's
first argument is that `api_version = 1` has no published reader outside this repository, and
contracts r13 has already applied it to a breaking change on that basis. Its exit condition is
inherited: *a deployment that has published an API must bump instead.*

In-repo consumers to update in lockstep: `clients/ts/core` and the viewer's layer;
`reference/oracle/wire.py`; `crates/tessera-server/tests/http.rs`; the conformance modules in §5.3.

---

## 4. On the wire

The points batch carries `code: uint64` in place of `x`/`y` — same 16 B/point, so a precision and
capability change rather than a size one. Contracts §3.2's note that
`morton_of(x, y, extent) >> (32 − 2·zoom)` yields the containing tile becomes a shift rather than a
recomputation, which is directly useful for hover bucketing and client-side clustering.

Precision can reach the GPU through deck.gl's `fp64` emulation, but **the split does not carry
across**: `position64Low` takes the floating-point residual `x − fround(x)` in layer units, so the
client deinterleaves and scales to `f64` first and the hi/lo split is recomputed, not carried.

`/v1/meta` continues to publish the quantisation extents, which is what makes the code
interpretable.

---

## 5. Conformance and the oracle

### 5.1 Why the oracle needs a new input

`reference/oracle/bundle.py` recomputes the Morton codes and the row order from `(x, y)` read out of
the bundle, so that a build which emits a wrong column and then sorts consistently by its own wrong
values fails the differential rather than agreeing with itself. Removing `x`/`y` would degrade that
to `code == interleave(deinterleave(code))` — a tautology.

**The oracle therefore takes the source Parquet**, recomputing from the coordinates the build
consumed. This is stronger than what it replaces: the oracle's input moves upstream of the build.

### 5.2 What that costs

**A third input class.** `conformance.md` §1 states the oracle's inputs are two and that the count is
load-bearing, and `reference/tests/test_oracle_layering.py` enforces it by forbidding the
definitional modules from importing the fixture modules. So `bundle.py` cannot fetch the source; a
driver hands it in, as the fixture already hands in the identity key. That escape must be designed
and `conformance.md` amended.

**A join.** The oracle indexes geometry by bundle row; the source is keyed by source id, and entity
IDs are assigned in term-signature order, so the two differ. The route is `row → entity_id`
(permutation) `→ external_id` (sidecar) `→ source row`. That makes every geometry differential
depend on `entities/external-ids-<k>.arrow`, which contracts **§0.4** does not list in the Phase-1
conformance burden and must. The alternative is to require the source to carry the internal entity
id, which no current fixture does.

**A binding.** ⊘ **Specified, not built.** Nothing ties a Parquet to a bundle — no digest, no
manifest entry. `catalogue.py` records this drift having already bitten the suite once. Under this
design it is a whole-suite geometry failure that reads as an engine bug, and nothing structurally
prevents a runner regenerating the "source" from the bundle, restoring the tautology. A source
digest in `MANIFEST.json` closes it; until it exists the binding is the harness's discipline, and
`conformance.md` §1 says so where a reader meets the third input.

### 5.3 Tests, and the rest of the blast radius

- **Quantiser coherence** — `cell16(v) == fixed32(v) >> 16`, including both clamps. This keeps
  `morton.u32` byte-identical and contracts §2.5 true, and is a different property from bit-locality.
  It holds in exact binary FP, but `cell()`'s `floored >= 65535.0` clamp is where a 32-bit sibling
  would diverge.
- **Split coherence**, anchored on the shipped construction rather than a test-only helper:
  `(morton_of(x,y) as u64) << 32 | residual_of(x,y) == full_code_of(x,y)`, exhaustive over the 64
  single-bit positions plus a seeded property test.
- **Bit-for-bit oracle agreement.** The point-set differential currently compares `f32` with a
  tolerance; an integer code admits none, so `reference/oracle/morton.py` must match Rust exactly.
  This is why §6 requires contracts §2.5 to pin `fixed32` as precisely as it already pins `cell()`.

Beyond `bundle.py`, `x`/`y` are read by `reference/oracle/wire.py`, `viewport.py`, `catalogue.py`
and `canary_fixture.py`, and by the point-set multisets in `conformance/tests/test_i7_selection.py`
and `test_overlay_journal.py`.

**`test_byte_scan.py` (I10).** It sweeps 8-byte-aligned windows and checks `tessera_id`'s against the
full target set, `x`/`y`'s against the floor-filtered set (`SAFE_ID_FLOOR`), because a genuine `0.0`
coordinate yields a colliding window. `code` takes the floor-filtered treatment for the same reason:
a point at the extent origin yields `code == 0`. Its negative controls become one `u64` rather than
two `f32` halves. One thing genuinely changes — a build writing a `tessera_id` into the `code` column
is now a plausible bug shape, which the float columns made implausible; floor-filtering catches it
for every ID at or above the floor, the same guarantee `tessera_id`'s own column carries.

---

## 6. Amendments

- **contracts §2.5** — 32 bits per axis; the cell code is the high half; `fixed32` specified as
  exactly as `cell()` is, clamp thresholds included (§5.3).
- **contracts §2.6** — `columns.arrow` loses `x`/`y`, gains `residual: uint32`; the row-order clause
  notes the residual is not a sort key.
- **contracts §3.2** — the points batch carries `code: uint64`.
- **contracts §0.3 deviation 10** — annotated (§3).
- **contracts §0.4** — the conformance burden gains the external-ID sidecar.
- **conformance.md §1, §7** — the oracle's inputs become three, with the layering-test escape
  specified.
- **architecture Appendix A** — `Hot columns (18 B/row) | 180 MB` → `(14 B/row) | 140 MB`; at 10⁹
  `18 GB | 281 MB` → `14 GB | 219 MB`. The `Coordinates only` row wants **renaming, not halving**: a
  coordinate still costs 8 B/row, split across `columns.arrow` and the untabulated `morton.u32`. The
  wire note's "~18 B/point, matching the hot-column width" goes stale. Leave the r21 note's
  "22 B/row → 18 B/row" alone — it records a different change.
- **architecture §5.3** — the hot-column list.

---

## 7. Benchmarks

Residency is the only benefit, and **it is not measured**. `arms/gather.rs` now reads both halves
of a position — it takes the whole `SegmentData`, so the `morton.u32` load the real path makes is
in the measurement rather than missing from it, and its column axis names what it reads (`pos`,
`pos+id`, `pos+id+priority`). That closes the arm's *fidelity* gap and not the comparison one:
measuring this shape against the previous one needs a second column source and a build able to emit
both, and the bench has no memory-pressure mechanism at all — the `cold_*` arms are a token-cache
split, not a page-cache one. A residency saving becomes latency only under contention, so it would
not show up here even with both shapes present.

**The saving is therefore arithmetic against Appendix A, not a measurement, and must not be quoted
as one.** The arithmetic is sound. Note that the arm's own widths did not change: a position is
8 B/row either way, and what moved is which files it is split across.

Open-time cost — `MortonSlice::load`'s ascending scan — is likewise untimed, and §2.1 spends it as
an argument.

---

## 8. What this does not do

- **No `f64` exactness.** That needs 64 bits per axis and an `f64` wire.
- **No change to row order or to the tile path.** `morton.u32`, `tile_ranges` and every existing
  search are untouched.
- **No fix for `morton.u32`'s absence from Appendix A** (§2.3).
- ⊘ **Nothing streaming-side is exercisable end to end.** Residual geometry can be carried through
  the WAL and buffer, but there is no flush: buffered rows have no row in `columns.arrow` and
  `Engine::item` returns `Ok(None)` for them. `/control/ingest` accordingly still takes `x`/`y`
  `f32` — §2.2's noted asymmetry between built and streamed points, unchanged here.
- **No new precision in any existing corpus.** The generators quantise the way the engine does and
  emit a residual, but every corpus predating that must be rebuilt to carry one, and the scaled
  corpora hold 16 bits per axis whatever they are rebuilt from (§2.2). Rebuilding also moves ~25%
  of points between cells, because the generators previously quantised as `round(t × (2¹⁶ − 1))`
  where the engine floors at `2¹⁶` — so benchmark figures re-baseline. Inherent, not a choice.

---

## Appendix R — review trail

**Reviewed 2026-08-02**, three lenses (conformance/oracle, performance, implementability) across
successive drafts.

The findings that changed the design: removing `x`/`y` destroys the oracle's independent geometry
input, which is what §5 answers; a packed single column is sound and is rejected on reviewability
rather than on correctness (§2.1); and the scaled corpora carry only 16 bits per axis,
which makes the precision claim a statement about storage capacity rather than about any existing
bundle (§2.2).

Corrected against the corpus: the residency arithmetic (Appendix A's row counts `columns.arrow`
alone, and `morton.u32` is untabulated); the `api_version` argument (deviation 10 already carries
the no-published-reader case and has been applied to a breaking change); and `test_byte_scan.py`'s
treatment of `x`/`y`, which is floor-filtering rather than exclusion (§5.3).
