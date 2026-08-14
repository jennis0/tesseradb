# 0064 — An absent number is a presence bitmap beside the column, not a sentinel and not a validity buffer

**Date:** 2026-08-10 · **Status:** Settled (owner ruling)

## Context

A plain numeric column has no representation for "this item has no value". An item with no score is
stored as `0` and marked present, so it **matches a range containing zero** — a wrong answer, not a
missing feature. The other two families already have somewhere to put "nothing": a category spends
the code its vocabulary reserves (`0`), and a string carries an explicit absence. A number has no
spare value to spend, because every bit pattern is a legal value.

The gap runs from the build's decode, which copies numeric values out of the source array and drops
the validity buffer, through to both consumers of those values — the filter column in `attrs/` and
the render tail in `columns.arrow`.

## Decision

**Absence is recorded in a presence bitmap held beside the column, for render columns as it already
is for filter columns.** The values array stays flat, dense and non-nullable; the bitmap says which
slots mean anything.

Three alternatives were considered and declined:

- **An Arrow validity buffer on `columns.arrow`** — the textbook answer, and wrong here. Contracts
  R4 makes every column in that file contractually non-nullable and the reader **rejects a nullable
  column outright**, which is what lets it hand back flat zero-copy slices and gather per viewport
  with no per-value branch. Breaking that costs the hot path a branch and 125 MB per 10⁹ per column,
  to buy what a bitmap beside the column buys without touching the contract.
- **A declared per-column sentinel** (`absent = -1` in `schema.toml`), mirroring a category's code
  0. Declined because a category's sentinel is safe *only* because the vocabulary reserves it out of
  the value space before any data exists. A number has no spare value by nature, so this asks the
  caller to guarantee one — and the filter must then know to exclude it, or the bug moves rather
  than goes.
- **A float NaN payload** — genuinely standard for floats (R's `NA_real_` is exactly this) and free,
  since NaN fails every comparison. Declined for **consistency**: it solves one family of one
  column type and leaves every integer width needing the bitmap anyway, so it buys a second
  mechanism rather than removing one.

**`−∞` is not a null representation and must not be used as one.** For integers it does not exist;
for floats it is a legal value that ordinary arithmetic produces, so a corpus may hold it and a
range filter over it is meaningful.

## Why

The mechanism already exists and is understood: `presence.roaring` beside a filter column is a
validity bitmap in all but name, and Roaring makes it near-free when absence is rare or clustered —
which is the shape a sparse attribute actually has. Reusing it keeps one mechanism for absence
across both artefacts, keeps R4 and the flat-slice read path intact, and needs no new file format.

The render column's bitmap differs from the filter column's in one way worth stating: a filter
column *compacts* — absent entities occupy no slot — while a render column is row-indexed and dense,
so an absent row keeps its slot and the slot's contents are meaningless rather than zero-as-a-value.

## What this obliges

Not designed here, and each needs stating before code: what the file is called and where the
manifest names and digests it; how the wire expresses absence in the points batch (contracts §2.6
and §3.2); how merge, flush and the compaction fold carry it; and whether `/control/ingest` accepts
a null numeric, which today it does not distinguish. Contracts R4 stays as written — the column
remains non-nullable — and that is the point of the ruling rather than an exception to it.

**Landed, in part** (2026-08-13): the filter half, and the *server side* of the render half — the
bitmap beside `columns.arrow` (`tessera_store::render_presence`), written by both builds, flush,
merge and the fold, and read by the row-space filter route, so a rendered number is filterable and
an item with no value matches no range. What stays deferred is the rest of the render half as
described below: the points batch still cannot say "absent", so a client still draws an absent
number at zero. The split follows this section's own reasoning — the deferral's stated blocker is
the client, and nothing in the server-side bitmap touches it.

**The two halves are separable, and the order is deliberate** (owner, 2026-08-10). The *filter*
half — keep the source's validity through the build, and let an absent number occupy no slot in the
filter column, exactly as an absent string already does — is entirely server-side and fixes the
wrong answer on its own. The *render* half is what needs the bitmap beside `columns.arrow`, a way to
say "absent" in the points batch, and a client that understands it; it is **deferred while the
client is under active development**. Until it lands, a render column continues to show zero for an
absent number, and the two artefacts therefore disagree — the filter says an item has no score while
the map draws it at 0. That is a narrowing disagreement rather than a leaking one (**I12**), and it
is stated here so the first person to notice it finds it recorded rather than surprising.

## Evidence

Stated at `write_column_values` (`crates/tessera-build/src/pipeline.rs`) and at
`filter-index.md` §2.1. `BatchColumn::decode` (`crates/tessera-build/src/input.rs`) is where the
validity is dropped. Contracts §2.6 carries R4 and the non-nullability the reader enforces;
per-point-attributes §3.6 carries the category's reserved code 0 and the argument this ruling
extends.
