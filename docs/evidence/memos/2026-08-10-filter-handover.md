# Filtering: what is not finished, and what the next person needs to know

**Date:** 2026-08-10 · **Status:** Handover memo — evidence, not normative. Names open work; rules
nothing.
**Reads with:** [`filter-index.md`](../../design/filter-index.md) (r7, Provisional — the
specification for the artefact and its lifecycle), [`filter-surface.md`](../../design/filter-surface.md),
decisions [0039](../../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0059](../../decisions/0059-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0060](../../decisions/0060-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
and the measurement campaigns in [`probes/2026-08-08-filter-layout/`](../../../probes/2026-08-08-filter-layout/)
(arms 1–16) and [`probes/2026-08-10-filter-lifecycle/`](../../../probes/2026-08-10-filter-lifecycle/).

Filtering is built end to end: every family, all nine operators, the wire surface, and the whole
write side — a flush writes per-column extents, the coalesce bounds them, the fold folds them back
and rebuilds the derived postings. All three write paths are verified on real data at 2.4×10⁶ and
2.5×10⁷ against an oracle that decodes the source parquet independently.

What follows is everything that is *not* done, with enough context to resume without reading this
session's history.

## 1. Read this first — the trap that will cost you a day

**The scan's published constants are a property of `tessera-filter`'s contents, not of the scan's
code** (§2.2, probe arm 16). Seven times during this work an unrelated addition to that crate moved
them 30–70%; three of those were from code that never runs during a scan, and once from a function
that was never *called*. Deleting the call and leaving the symbol reproduced the regression exactly.

No remedy measured is general: `#[inline(always)]` fixes the case in front of you, one codegen unit
fixes the presence path and not the packing path, moving the symbol to another crate recovers about
half. **A/B interleaved against a `HEAD` build, medians of three, before believing any number** —
`probes/2026-08-08-filter-layout/layoutprobe`'s `realscan` and `textscan` are the harness, and
run-to-run drift is ~5%.

The structural answer — a crate containing *only* the scan, so there is nothing left to perturb it —
is named at arm 16, unbuilt and unpriced. **It is the first thing worth doing if more work is
planned in this crate**, because it changes the cost of everything after it.

## 2. Unbuilt operands and one family

Each refuses by name today, which is the fail-closed shape; none is a silent gap.

**`none_of`** is fenced by [decision 0059](../../decisions/0059-filters-compose-as-a-boolean-tree-inside-the-candidate.md)
— a negation over a gated vocabulary is an existence oracle, and its C11 rule must be built with it.
**There is a second reason, added later, that is easy to miss and more dangerous**: §5 records
*positivity* as a load-bearing property. Every "this failure degrades safely under **I12**" argument
in the design — a lost layer, a lagging flush, a blanked slot, a missing extent — holds *only*
because every shipped operand is positive, so an entity whose value is unreachable matches nothing
and the result narrows. Under `none_of` those same failures **widen**. Whoever lifts that fence must
revisit layer composition and every start-up failure mode in §6.2 under the inverted sign, not just
the vocabulary gating.

**`match`** is specified and unbuilt; nothing depends on it.

**Multi-valued / list attributes** ([decision 0039](../../decisions/0039-multi-valued-categoricals-are-slow-path-only.md))
need their own addressing before anything else. §2.1's value column maps one presence bit to one
slot, and the affine-rank traversal, the fold's blanking and the coalesce's merge are all built on
that. §2.6's older claim that "lists cost no format work" was withdrawn for this reason; §6.2 and
§5.2 now scope themselves to the shipped families explicitly.

## 3. Decisions waiting on the owner

**First-touch digest deferral for `attrs/`** (§8, an amendment contracts §2.4 already records as
owed). This is the one with a real win: `verify_files` hashes every named file in full at open, and
for a text column that is the *first* of two passes over its bytes. The asymmetry that makes it
rulable: a corrupt **value column** can only narrow `M_sel`, because the scan runs inside the
candidate and I12 holds structurally — but a corrupt **posting** now feeds `/v1/categories`'
`per_viewer` visibility predicate under decision 0060, which is a disclosure control. Deferral is
arguable for value columns and not obviously safe for postings.

**Where the fold's flip opens `FilterColumns`.** §6.2 says the columns join the rotation the way the
postings reader and the external-id sidecar do — and that rotation runs *after* the `CURRENT` flip,
so a failure there is an alarm and a restart onto the committed bundle. An implementer was briefed
to carry opened columns so publication could not fail after the manifest edit, which is the opposite
order. It followed the document; the conflict is unresolved and stated here rather than buried.

**Whether the crate-isolation of §1 is worth doing**, and whether `codegen-units = 1` is an
acceptable interim (measured: it costs the baseline ~15% and removes the presence path's
sensitivity, not the packing path's).

**A contracts edit landed without minting a revision letter** — three tree lines adding
`coalesced/<id>/attrs/<column>/`, no field or behaviour change. r19 documented a comparable tree
addition, so whether this owes an r-note is a small call nobody has made.

## 4. Promotion

`filter-index.md` is **Provisional at r7**, with its adversarial round dispositioned and §6.3's
rulings closed. Two things stand between it and normative:

- **§2's constants confirmed at a value width other than `u32` and on a string column.** The
  campaign swept `u32` and text separately and never crossed them.
- **Surface §4's project-vs-per-tile rule**, which is its own ruling: arm 3 measured the crossover
  (a projection costs ~27 ns per set bit and scales with the *result*; a per-tile membership test
  ~6–22 ns per viewport row and scales with the *viewport*) and only the projecting route is built.

Two amendments are owed to **normative** documents and should ride their own reviews, not this one:
compaction §2's table and §3's pass list gain the attribute pass and the band budget; write-path §7
and contracts §2.1 gain the coalesce's fourth axis.

## 5. Known gaps, each marked ⊘ at its claim

- **`MADV_SEQUENTIAL`** on the fold pass's own mappings. The *ownership* half of decision 0052 is
  honoured — the pass maps its own inputs and never advises the request path's maps — but the hint
  itself needs a route through `ValueColumn::open`, which means touching the hot file (see §1).
- **Attribute byte terms** in the fold's dispatch log line and `/control/status`, which §6.3's
  reported-never-triggered ruling asks for. The pass reports its time and RSS; the byte terms are
  not there.
- **The in-prefix orphan sweep** (compaction §7), pre-existing and now heavier: a failed coalesce or
  fold leaves a directory per column, and only a fold reclaims it. Measured at ~18 MB after nine
  passes, tracking ingest volume rather than corpus size.
- **The text-offset bounds check**, implemented, tested and **reverted** — it cost 70% of the scan
  for the reason in §1. It is redundant while Arrow validates offsets on decode; restore it if the
  digest deferral above lands, at which point it stops being redundant.
- **Numeric absence.** A plain numeric column has no representation for "no value" — every bit
  pattern is legal — so an item with no score is stored as zero and **matches a range containing
  zero**. Stated at `write_column_values` and in §2.1. Fixing it needs the null-aware attribute
  reader the string column's own ⊘ already names.

## 6. Not measured, and what each would settle

- **A fold under concurrent read.** §6.2's non-disruption claim is reasoning from shape — that the
  pass resembles P4's 1.05–1.18× rather than P3's 2.03× — and has not been measured.
- **A scattered, high-coverage principal.** The lifecycle campaign's coverage was 11.6–11.8% and
  comparatively contiguous. The measured ~13–15 ns-per-candidate random-access floor says this is
  the shape that stays over budget for text `contains`, and nothing has exercised it end to end.
- **10⁹ lifecycle**, and **text-column residency at 10⁹** (extrapolated at ~14 GB of clean page
  cache from a 2×10⁷ measurement; three extrapolations in this campaign were already wrong).
- **Cold / on-disk scan.** Every constant is RAM-resident and arm 5's mapped figures are
  warm-page-cache.
- **The `filter` bench arm**, declared in measurement §7 and still not built. The lifecycle probe is
  deliberately not a substitute — it walks one bundle through a lifecycle; the arm is a cell in a
  matrix and belongs in `bench/matrix.toml`.

## 7. Things that will bite you

- **Rule S and Rule F must never be conflated** (write-path §5.4). A suppression retires only on
  unsuppress and touches no attribute artefact ever; a deletion retires only at the fold that
  executes it. A coalesce retires **nothing** — a deleted-but-unfolded value rides through it, and
  there is a test asserting exactly that.
- **Publishing an attribute artefact is two obligations**: the files *and* the manifest's
  `attr_extents` list. Doing one without the other produces a bundle that opens cleanly and silently
  answers filters missing entities — strictly worse than refusing. Both the flush and the coalesce
  do them in one manifest write; the fold's earlier failure to do either is what this session fixed.
- **The conformance differential does not catch everything you would expect.** Dropping `∧ candidate`
  from the routed postings path does *not* fail it, because the viewport's row-space composition
  masks the difference downstream. Only the engine-level assertion catches that. Recorded so a green
  suite is not read as covering it.
- **The conformance baseline is 3 failed / 81 passed / 1 skipped / 2 errors** — pre-existing WAL and
  overlay failures in `test_overlay_journal.py` and `test_restart_replay.py`, unrelated to
  filtering. Confirm the count is unchanged; do not fix them as part of filter work.
- **`scripts/fmt-file.sh`, never bare `rustfmt`**, which follows `mod` declarations and reformats
  whole crates.

## 8. Where the work lives

`crates/tessera-filter` (read side: value columns, the scan, the packed result sink),
`crates/tessera-filter-write` (merge and fold/coalesce emit — a separate crate for the reason in §1),
`crates/tessera-engine/src/filter.rs` (`FilterColumns`, layers, routing), `flush.rs` (extents),
`coalesce.rs` (the fourth axis), `compact.rs` (the fold's pass 4a), and
`crates/tessera-server/src/filter_dto.rs` (the trust boundary).

Several items above are issue-shaped rather than loose ends — §2's three operands, §3's rulings and
§1's crate isolation each need an owner or are deliberate deferrals. None has an issue yet.
