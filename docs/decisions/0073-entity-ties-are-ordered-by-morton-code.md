# 0073 — Within a signature group, entity IDs are ordered by Morton code

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

Entity IDs are allocated `(signature, source_id)` within each commit window (§11.1). **The minor key
becomes the Morton code**, so allocation is `(signature, morton_code, source_id)` — the last
component for totality, since two items may share a cell and the order must be deterministic.

The **major** key is unchanged, and nothing about the prohibition on Morton-ordered assignment moves:
that forbids Morton **rank** as the scheme, on the grounds that a rank is not permanent and that the
ordering is already spent on posting compression. Both survive intact. A Morton **code** is a
function of an item's coordinates and the published quantisation bounds, fixed for its life,
computable the moment a row arrives with no lookup and no global state — so it renumbers nothing,
and the thing being spent here is the tie, which §11.1 measures as worth nothing where it stands
(run lengths 1.00–1.26 against a 1.000 random baseline).

## Why

**It is free in the format, and measured so** ([M3](../../probes/2026-08-15-artifact-representation/),
2026-08-15, over the real 2.42M corpus with the real signatures and the real term structure):

| | `(signature, source_id)` | `(signature, morton)` | |
|---|---:|---:|---|
| artifact membership | 8.893 MB | **2.181 MB** | **4.08×** |
| term postings | 0.557 MB | 0.557 MB | **1.00× — byte-identical** |

The posting win is untouched because a term's postings are the union of the signature groups
carrying it, and each group stays a contiguous run whatever orders its interior.

**The permutation becomes piecewise monotone, and that half is arithmetic rather than measurement.**
Within a signature group, ordering by Morton code makes `entity → row` monotone increasing *by
construction*. Over the real corpus — 54,794 signatures across 2,422,486 entities — the mean
monotone run moves from **~1 to ~44**. §5.1 keeps `permutation.bin` flat and uncompressed
*"precisely because entity order and row order are unrelated, making the values maximum-entropy"*,
and points at [`deferred-signature-major-layout.md`](../design/deferred-signature-major-layout.md)
as what would change that. **This reaches the same precondition from the entity side and leaves row
space untouched**, so that sketch stays unapproved and unneeded.

**It cannot be taken later.** Entity IDs are permanent within an identity; a build that ordered items
differently is a different corpus. There are no deployments (decision 0048), so "cannot be
retrofitted" means "decided before the first build that writes an artifact" — which is now, ahead of
[`docs/artifact-delivery.md`](../artifact-delivery.md) Stage 1.

## What it is worth, bounded

**The hot path is row space and this is not on it.** Artifact membership at request time is
row-space, where it is already 28–118× smaller than the entity form (M1, on real membership). The
4.08× applies to the *disk* form and to the projection's input. It is a real saving on a real
artefact, and it is not a request-path optimisation — a reader who quotes it as one has
double-counted against M1.

## The one safety question, and why it is closed

A spatially-correlated entity order would be a disclosure if anything a viewer sees were ordered by
entity ID. **Nothing is.** §7.2's sampler comparator is the full `tessera_id` — a keyed bijection
over 2⁶⁴, uniform and uncorrelated with entity order — and `priority` is a prefix of it. That was
adopted for the mirror-image reason: while the tiebreak *was* the entity ID, signature-ordered
allocation put a permission correlation inside the sample above V ≈ 2×10⁶ (§7.2). **The same change
that closed the permission channel closes the spatial one**, and this ruling would not have been
safe before it.

I10's structural half is likewise unaffected: no request-path artefact stores an entity ID, so the
gather cannot produce one whatever orders them.

## What it costs, where the measurement did not look

⊘ **Inspection, not measurement** — the campaign measured the *format*, and the build was not
in its scope. The signature sort holds one 12-byte record per item (`key_hi`, `key_lo`, `ordinal`),
and the batch plan's residency model is explicit and enforced: a batch costs
`8·pairs + 12·items + 4·items + items/8` beside a loop-wide floor, and the largest feasible batch
under the memory budget is chosen from it. A Morton code is a fourth field, so either the record
widens to 16 bytes or the code displaces the source ordinal and the ordinal is recovered another
way. **The first shrinks the feasible batch under a given budget, which means more batches** — and
signature assignment is batch-scoped, so a smaller batch collects slightly less of the posting win
the major key exists for. The code must also be available at the sort, which today runs before the
geometry scan.

None of that changes the ruling: the effect is second-order against a 4.08× and a permanent
decision, and the direction is a knob (`--memory-budget`) rather than a wall. It is named because it
is the part someone will meet in the code and not find in the design. **Sizing it belongs to
Stage 1**, along with the choice between the two record layouts.

## What has to move with it

- **Every fixture is rebuilt.** Postings are byte-identical, so mask-build and authorise figures
  stand unchanged. **Projection figures do not**: `Permutation::project` gathers rows that now
  arrive in ~44-long sorted runs rather than scattered, so its measurements are re-baselined rather
  than carried. What that is worth is ⊘ **unmeasured** — the campaign's arm for it did not run.
- **Both build paths.** `tests/build_equivalence.rs` holds the linear and batched builds to
  byte-identical bundles; the comparator change moves both or the test says so.
- ⊘ **An amendment is owed to architecture §11.1**, whose measured paragraph is about the major key
  and is silent on the minor one. It is owed at the artifact designs' promotion, not before: the
  section's prohibition and its argument are unchanged, and what it gains is the tie's disposition
  and the permutation consequence.

## What this does not settle

**Which slice's order is taken, when there is more than one.** An entity has one Morton code *per
slice* and one entity ID, so the tiebreak optimises one slice's spatial order and leaves the others
arbitrary. Today there is one slice, so nothing is chosen by accident and nothing is lost. When
slices land, the comparator must **name** the slice it orders by — a declared primary, or the first
slice an entity joins — and it cannot be revisited for entities already allocated. Recorded here so
that it is decided rather than defaulted, which is exactly what taking the tiebreak silently would
have done ([`annotation-representation.md`](../design/annotation-representation.md) §2.2.1).

**What a near-monotone permutation is worth compressed**, and what piecewise-sorted input does to
`Permutation::project`. Both were in the campaign's plan; neither ran, because its fixtures were
deleted mid-campaign. The source parquets are present again, so `m7_core_rows.py` is runnable and
this is cheap to close before Stage 1 commits.

## Evidence

[`probes/2026-08-15-artifact-representation/`](../../probes/2026-08-15-artifact-representation/) —
M3 (the ordering, on the real corpus), M7 (what else in entity space moves: postings 1.00×,
membership 4.08×, row space byte-identical), M1 (the row-space bound on what this is worth).
Recommended in [`annotation-representation.md`](../design/annotation-representation.md) §12; ruled by
the owner on 2026-08-15.
