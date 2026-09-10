# 0112 — The allocation tiebreak is the source-id ordinal

**Date:** 2026-09-10 · **Status:** Settled (owner ruling, 2026-09-10)

## Context

`views.md` §7 cites decision 0112 twice for the entity-id allocation key: within a signature
group, ties order by the item's Morton code in the declared anchor view — `[defaults].allocation_view`
— "then by `external_id` bytes." No decision file with that number was ever written:
`docs/decisions/` runs from 0107 to 0114 with a gap where 0112 belongs, and 0073, which `views.md`
says 0112 extends, is absent too. This decision is that missing record, written now because the
text it is meant to back disagrees with what the build does.

`SortRec::order()` (`crates/tessera-build/src/pipeline.rs`) returns the four-field tuple every
allocation tie is sorted on: `(key_hi, key_lo, morton, ordinal)`. The first two fields are the
item's two-term signature key; `morton` is the anchor view's Morton code, as `views.md` says. The
fourth and final field is `ordinal` — the item's position in `SourceIds`, the sorted,
duplicate-free union of every view's source ids that pass one builds before any entity id is
assigned. That is numeric order over the union's index, not byte order over `external_id`.

The two differ, and the difference is not cosmetic. `ExternalIdRow::new` byte-swaps `source_id`
before using it as a sort key, so that numeric order over the swapped halves equals byte order
over the little-endian encoding the external-ID sidecar writes to disk — R4 requires that sidecar
sorted by the id's bytes, and the swap is what makes a numeric comparator produce that order.
`SortRec` applies no such swap. If the allocation tiebreak were really `external_id` bytes, it
would need the same swap and does not have it; what it holds instead is a plain ordinal with no
byte-order contract to satisfy.

This is settled before release: no bundle exists that allocation has run against, so correcting
the text costs nothing today ([decision 0048](0048-no-deployments-exist-so-delete-rather-than-support.md)).
It has to be settled once, because an id assigned under either reading is permanent from that
point on (**I9**) — a deployment built under one description cannot be reconciled with a client
that assumed the other.

## The decision

**A. The union ordinal — the item's index in the sorted, duplicate-free union of source ids
`SourceIds` holds.** Ruled. This is what `SortRec::order()` computes; `views.md` is corrected to
describe it.

The tiebreak must be a total order — the conformance suite's byte-identity oracle checks that a
build reproduces the same bundle byte for byte, and a tiebreak that could leave two orderings
valid would fail it — and it is compared at every signature-and-Morton tie across the whole
corpus, so it has to be cheap. The ordinal satisfies both without effort: `SourceIds`'s
deduplication makes it unique by construction, it is 4 bytes, and it already sits in `SortRec`
before any tie is examined, so reading it costs nothing a tie was not already going to pay.

**B. `external_id` bytes, as `views.md` currently reads.** Declined. Byte order over a
little-endian `u64` sorts on the value's low byte first, which is the byte a numeric id varies
least in — an order nothing downstream reads and no invariant needs. The phrase reads as R4's
sidecar-search rule carried into a context it was never written for: R4 governs how the external-ID
sidecar is binary-searched, not how entity ids are assigned, and the two do not have to agree.

Keying on `external_id` would also cost more for the return above. It needs the full 8-byte value
rather than the ordinal's 4, which would grow `SortRec` from 16 bytes to 20. `SortRec`'s own
documentation already records what widening this record once costs: adding the 4-byte Morton code
(12 → 16 bytes) forced `plan_build`'s residency model to widen with it, because a batch is sized
against the memory budget it plans against. A second growth of the same kind would repeat that
cost for a key with no locality argument behind it (below), where the Morton field it would sit
beside has one.

## Why the tiebreak buys nothing below the Morton code

Ordering ties by anything past the anchor Morton code cannot win locality, because a tie at that
point has almost nothing left to order. Measured on GBIF, a Morton cell holds 7.37 rows on
average; a Roaring container spans 65,536 entity ids, about 8,895 Morton cells. A tiebreak inside
one cell can only reorder a handful of items that were already going to land in the same
container — it cannot move an item into a different one. Whatever the final field is, it changes
nothing a container boundary would see.

The locality win this key holds is taken one field higher, at the Morton code itself. `SortRec`'s
documentation records adding it to the signature sort as measured 4.08× smaller on artifact
membership's disk form, with term postings byte-identical — the figure decision 0073 would record
if it existed. That measurement is why the Morton code is compared before the ordinal at all; it
gives the final field nothing left to buy.

## Consequences

- `views.md` §7's allocation-key sentence, at its two citations of decision 0112, is corrected
  from "then by `external_id` bytes" to describe the union ordinal.
- No `api_version` or `bundle_format` bump: the code this decision documents is unchanged, and
  every bundle is rebuilt before release regardless ([decision 0048](0048-no-deployments-exist-so-delete-rather-than-support.md)).
- 0073 stays absent. This decision does not write it; it only records that the figure `SortRec`
  attributes to it is measured, not assumed.
