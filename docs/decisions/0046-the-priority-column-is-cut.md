# 0046 — The `priority` column is cut from `columns.arrow`

**Date:** 2026-08-04 · **Status:** Settled (owner ruling)

## Context

`priority` — the high 16 bits of an item's `tessera_id` — is a *quantity* the selection
definition uses (§7.2) and a *column* the format stored. The two came apart at r7: the shipped
comparator reads the full `tessera_id`, because "lowest by priority then by identity" is
identically "lowest by identity" and the single-column form is the one that is obviously
correct. Contracts §2.6 recorded the consequence honestly — **the column is written and unread
at query time** — and kept it as the physically-contiguous prefix a future prefix-scan
optimisation would need, with a trigger for revisiting (`w ≈ log₂(V_max/k)`).

That keep was re-raised by the write-path consolidation's audit with one new fact: **the
asymmetry runs the other way.** Format 1 has never been published, so removing a column now is
free; the reader matches fixed columns by name and type, so *re-adding* one later is an additive
change — while keeping it means 2 B/row written and stored (2 GB at 10⁹, in a per-viewport file)
indefinitely for an optimisation nothing exercises, or a breaking format change later if it
never arrives.

## Decision

**The column is cut.** `columns.arrow`'s fixed columns are `tessera_id` and `residual` — 12
B/row before declared scalars. A bundle carrying the old three-column schema is a typed error at
open (the reader's name-and-type check), never a silently ignored column.

Nothing about the *quantity* changes: `priority` remains the high 16 bits of `tessera_id`,
derived at the one definition site (`TesseraId::priority`), used by §7.2's definition, by the
build's sort-comparator prefix optimisation (an in-memory key, never stored), and permitted on
the wire as before (a keyed prefix of an identity the payload already carries in full — I10's
rule about *unkeyed* derivatives is untouched). §7.2's revisit trigger stands: the day a
measured prefix-scan optimisation asks for a physically-contiguous 2-byte column, re-adding it
is additive.

## Consequences

- Every `columns.arrow` writer (build, flush, merge — one writer path) drops the column; the
  reader's fixed-column check narrows to two; the bench gather arm's `pos+id+priority` mode is
  deleted and `full` aliases `pos+id`, with the r21 18 B/row figure marked historical.
- Appendix A's hot-column row falls 14 → 12 B/row (140 → 120 MB at 10⁷; 2 GB saved at 10⁹).
- Existing local bundles must be rebuilt — pre-alpha, the same posture as every format 1
  in-place correction (contracts §0.3 deviation 5's precedent).
- The Python oracle is untouched: it derives row order from source geometry and the identity
  key, never from stored columns, and never read this one.

## Provenance

Audit finding 1 of the write-path consolidation (2026-08-04); owner: *"I'm good to cut
priority."* Supersedes the keep recorded at contracts §2.6 r7.
