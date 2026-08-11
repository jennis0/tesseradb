# 0063 — The inverse permutation is stored, because a filtered viewport asks it per row

**Date:** 2026-08-11 · **Status:** Settled (owner ruling)

## Context

Architecture §5.1 said the entity→row permutation is stored and "the inverse direction is not stored
at all", because row→entity is derivable: `columns.arrow` carries the row's `tessera_id`, the
identity is a keyed bijection, and inverting it is a pure function needing no file and no
consistency obligation. That reasoning is sound for `/v1/items`, which inverts one identifier.

A **filtered viewport** asks the same question of every row it is about to draw. `filter-surface.md`
§4's cheap route for a broad filter — test the viewport's rows against the filter's entity-space
result, rather than projecting the whole result — is a per-row crossing, and the bijection is four
Feistel rounds at *measured* **~17.5 ns per row**. Against ~0.4 ns to read the `tessera_id` beside
it, the inversion is the entire cost of the route: 6.0 ms rather than 0.7 ms over a 300,000-row
viewport on a clumped result, 18.5 ms rather than 11.1 ms on a scattered one
([`probes/2026-08-11-viewport-crossing/`](../../probes/2026-08-11-viewport-crossing/)).

Batching does not recover it — inverting a whole tile into a scratch buffer before testing
membership measures *slightly worse* than interleaving (6.22 ms against 6.00 ms), because the cost is
the rounds and not a stalled pipeline. A coarse Morton-cell → entity pre-filter, which would avoid
the crossing entirely, was measured in the same campaign and is **refuted**: slowest of the three
routes at every point, and larger than the array it emulates.

## Decision

**`row-entity.u32` is a bundle artefact**: a raw `u32` per row of a slice's base segment, giving the
entity that occupies it. Written wherever `permutation.bin` is written and nowhere else — the batch
build and the compaction fold. A merge writes neither: it emits *segments*, whose row mapping lives
in an extent rebuilt at open, so the operation that renumbers rows most often does not touch the
base.

The cost is 4 bytes per row per **slice**, not per column: this is a property of the slice's
geometry, so sixteen filterable columns need no more of it than one does. Mapped rather than read, a
viewport touches only the rows it draws — ~1.2 MB for 300 tiles of 1,000 rows.

**A slice without the file is served by projecting**, not refused. `RowSpace::entity_of` answers
`None`, which means *ask another way* and never *this row has no entity*; reading it as an absence
would drop rows from a filtered viewport silently, so the distinction is asserted in tests rather
than left to a comment.

## Why this does not weaken I10

**I10's structural half is about what the gather can reach**, and it is untouched. `columns.arrow`
is the only artefact the point gather reads; it carries `tessera_id` and no entity ID, so a served
point cannot carry one. `permutation.bin` and `row-entity.u32` are index structures consulted by
masking and filtering, never by serialisation.

Nor does the file disclose anything to a **bundle-holder**. `permutation.bin` already holds the
whole bijection; both directions of a permutation are one fact, and a holder who can read one can
compute the other. The objection to storing the inverse was always cost, never leakage — and an
earlier draft of this ruling that claimed otherwise was wrong.

What the wording of I10 needed was a correction, not a weakening: "no request-path artifact stores an
entity ID" becomes "no artifact the gather reads stores an entity ID", in architecture §5.1 and
§11.1, system architecture §5.3 and §9, and contracts §0.3 deviation 6. The substance those passages
were each relying on is what the corrected sentence says.

## Alternatives declined

- **Derive it from `tessera_id` per row.** The status quo, and the thing measured: ~17.5 ns per row,
  irreducible by batching, and the whole gap between the route's idealised and real cost.
- **A Morton-cell → entity pre-filter.** Refuted by measurement, above — and it costs 3.2–7.9 bytes
  per entity against this file's 4, because a cell's entity set is scattered in entity space and
  scattered is Roaring's worst case. The property that makes authorisation postings compress works
  directly against it.
- **A base-plus-extent arrangement**, a small table written per merge and fused into the base at the
  fold, mirroring the attribute extents, the dictionary extents and the external-id runs. Considered
  against a merge that rewrites the base — and unnecessary, because a merge does not write the base
  permutation either.

## Evidence

[`probes/2026-08-11-viewport-crossing/results.md`](../../probes/2026-08-11-viewport-crossing/results.md)
carries the measurements and the two refutations. `crates/tessera-store/src/row_entity.rs` carries
the design argument at the file; `crates/tessera-engine/src/viewport.rs`'s
`cross_filter_into_row_space` carries the route rule and its constant. Contracts §2.6 specifies the
layout; architecture §5.1 and `filter-surface.md` §4 carry the reasoning in the corpus.
