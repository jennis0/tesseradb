# What the build still writes, and what is left to take

**Date:** 2026-09-10
**Status:** Provisional. A work brief, not a design. Nothing here is decided; every item names what
it would cost to be wrong.

A build of rung 6's shape holds **100.9 bytes an item** on the disk at its peak, measured over four
prefixes of `data/ladder/gbif` between 16.3×10⁶ and 125.8×10⁶ items
([`probes/2026-09-10-build-disk/`](../../../probes/2026-09-10-build-disk/README.md)). It held 154.1
before that probe's changes. The corpus that pays it declares one `u8` category, one `u16`, two
`keyword`s and one tiered layer of three levels over a single view — so the per-item constant is
several times the payload, and that is what this memo is about.

⊘ **Nothing here was measured above 1.26×10⁸ items.** Every rung-6 figure is that fit extrapolated.
Three builds have already died on a model that read half the truth, so treat a rung-6 number here
as an argument for doing the work, never as a result.

## Where the 100.9 B/item sits

Measured at 125,789,091 items, at the peak, after the 2026-09-10 audit.

| | GB | B/item |
|---|---|---|
| the declared columns and their arenas | 8.36 | **66.5** |
| the bundle written so far | 2.83 | 22.5 |
| each view's geometry by ordinal | 1.01 | 8.00 |
| the ordinal→entity map | 0.50 | 4.00 |
| | **12.70** | **100.9** |

## In flight — do not take this one

**A blob-resident string's arena.** A `keyword` or `utf8` column with neither `index` nor `render`
has no dictionary and no text index, so its only reader is the record blob — the reader a `text`
column already avoids the arena for. Measured at 45.3 B/item spent to deliver 12.3. An agent is on
it as of 2026-09-10; it is most of the 66.5 B/item row above.

## What is left

### 1. The source-id array on a contiguous corpus — 8.00 B/item

`source-ids.u64` is written in pass one, sorted, and read by the layer publication. **On a corpus
whose ids are a contiguous range the resolver reads only the range's first id and its length** — the
array itself is never consulted. Every ladder corpus but `multiview` numbers its rows from zero.

At rung 6 that is 26.7 GB written, sorted and page-cached to answer two integers.

⊘ **The obstacle is that contiguity is not known until the ids are sorted.** A presence bitmap at
`n/8` bytes decides it exactly, at a sixty-fourth of what it removes; the `mix64` anchor already in
the build does not, being a hash. The sort itself may still be owed for the duplicate check — that
is the thing to establish first, because if it is, the array exists anyway and only its retention is
in question.

### 2. A string value's fixed overhead — 8 B of offset plus a record header

Every string value carries an entity-indexed offset into an arena, and the arena record carries a
length. The audit took the entity out of the record (28 GB at rung 6); the offset and the length
remain. At 3.5×10⁹ rows any string column costs about 28 GB before one character is stored.

Worth asking whether an entity-indexed offset array is owed at all where values arrive in a known
order, or whether it is the same materialisation the `text` route already declines.

### 3. Ordinal geometry, 8.00 B/item, and the ordinal→entity map, 4.00 B/item

Both are now unlinked at their last reader rather than at the end of the build (the audit's change),
but both are still materialised whole. `source_ids` answered exactly this question with "no" on
2026-09-09 — it is now bounded by the budget rather than by `n` — and neither of these has had the
question put to it.

### 4. The forecast is now a ceiling, and a loose one

The disk model reads **1.33–1.68× the measured peak**, deliberately: it refuses against free space,
so over-charging is the safe direction. But it will now refuse builds that fit. Three stated
ceilings carry most of the margin, the loosest by far:

| term | charged | actual, at 125.8×10⁶ items |
|---|---|---|
| postings and `pairs.parquet`, at 4 B/pair | 503 MB | **34 KB** |
| the blob's blocks, at half its characters | — | 0.26 of characters |
| a member entry, at 3 B | — | 2.24 B |

Tightening any of them is a judgement about how much a wrong refusal costs against a wrong
admission. The owner has not ruled.

## The pattern under all four

Three separate investigations on 2026-09-08, 09 and 10 found the same shape: **a structure
materialised in entity order because one consumer wanted random access, where that consumer either
does not exist for this schema or could take a streamed form.** The keyword dictionary, the ordinal
map and the source-id array have each been given a bounded form; a `text` column has always had one.
What is left above is the same principle applied inconsistently, and it is why the per-item constant
is as high as it is.

The general question worth settling, rather than the four special cases: **what decides that a build
structure is materialised at all** — and whether that decision can read a column's consumers the way
`postings_are_owed` already does, instead of its declared type.

## Not disk, but adjacent and unresolved

`distinct_of_ordinal` is the build's largest anonymous structure at 13,335 MiB at rung 6, held from
the pairs pass to the assignment. Mapping it is a small change, **but `plan_build`'s `loop_fixed`
carries its `4 * n` into `auto_batch`, so removing the term would give a budget-constrained corpus a
different permanent entity-id assignment**. That is an identity question under I9 and not a
performance one; it needs an owner ruling before anyone touches it.
