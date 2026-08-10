# Filtering: what is not finished, and what the next person needs to know

**Date:** 2026-08-10 · **Status:** Handover memo — evidence, not normative. Names open work; rules
nothing.
**Reads with:** [`filter-index.md`](../../design/filter-index.md) (r7, Provisional — the
specification for the artefact and its lifecycle), [`filter-surface.md`](../../design/filter-surface.md),
decisions [0039](../../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0060](../../decisions/0060-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0061](../../decisions/0061-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
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

**The cause was found on 2026-08-11 and it is instruction-address alignment**
([`scan-constant-sensitivity`](2026-08-11-scan-constant-sensitivity.md)): the hot loops' *addresses*
move relative to 64-byte boundaries, the emitted code being identical. So the constant is bimodal —
~0.25–0.28 or ~0.42–0.44 ns, nothing between — and the published figure is the favourable draw.
Only the compute-bound cells are affected; every memory-bound one is immune.

**A crate containing only the scan is *not* the answer**, which is what this memo said before the
cause was known: a crate boundary re-rolls the layout rather than pinning it, which is exactly why
moving the symbol "recovered about half". The remedy is `-C llvm-args=-align-all-functions=6`, and
it is **applied** — `.cargo/config.toml`, which carries the argument.

So the trap is narrower than it was, and the part that remains is the part you are most likely to
walk into: **an edit to `values.rs` or `pack.rs` still relocates their own blocks**, so anything
touching the hot files still wants **A/B interleaved against a `HEAD` build, medians of three,
before any number is believed** — `probes/2026-08-08-filter-layout/layoutprobe`'s `realscan` and
`textscan` are the harness, run-to-run drift is ~5% on a quiet machine, and this one is often not
quiet: a background browser or a resident `tessera serve` has been measured taking the same binary
across a 1.75× spread.

## 2. Unbuilt operands and one family

Each refuses by name today, which is the fail-closed shape; none is a silent gap.

**`none_of`** is fenced by [decision 0060](../../decisions/0060-filters-compose-as-a-boolean-tree-inside-the-candidate.md)
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
`per_viewer` visibility predicate under decision 0061, which is a disclosure control. Deferral is
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

Two of the five are closed (2026-08-10). `ValueColumn::open` now takes an `Access` rather than a
`mmap` bool, so the fold's pass 4a takes `MADV_SEQUENTIAL` on the layers it streams once — and
`FilterColumns::open` keeps its bool precisely so the hint stays unexpressible on the request path,
which is decision 0052 enforced by a signature instead of a comment. The pass's bytes read and
written are in the dispatch log line and in `/control/status` as `last_attr_bytes_*`. Three remain:

- **The in-prefix orphan sweep** (compaction §7), pre-existing and made heavier by this axis: a
  failed coalesce or fold leaves a directory per column, and only a fold reclaims it. Measured at
  ~18 MB after nine passes, tracking ingest volume rather than corpus size.

  **It is not the small job its size suggests, which is why it is still here.** The startup sweep
  that exists deletes orphaned *prefixes* — whole trees `CURRENT` never named, unambiguous because
  the live prefix is known. An *in-prefix* sweep deletes files from the bundle that is being served,
  and its correctness rests entirely on enumerating every file any reader could still name. That set
  is not one manifest: contracts §2.3 keeps older `SEGMENTS-<n>.json` alive precisely so a step-down
  can serve one, so `named` is a union across them, and the dead-bytes gauge already learned this
  the expensive way — summing one map alone reported a 1065× orphan ratio. Get the enumeration
  wrong and the failure is a deleted live file, which is silent until a reader asks for it. It also
  needs a rule about *when* it may run, since a coalesce writes its files before naming them and a
  sweep between those two steps deletes the output of the pass in flight. **A specification of
  `named` and a ruling on the window are what it wants first**; both are owner-shaped, and neither
  is written.

- **The text-offset bounds check**, implemented, tested and **reverted** — it cost 70% of the scan
  for the reason in §1, which is now understood and pinned. It is redundant while Arrow validates
  offsets on decode; restore it if the R1 digest deferral lands, at which point it stops being
  redundant. Not before: a redundant check bought at any price is still redundant.

- **Numeric absence**, and it is a wrong answer rather than a missing feature. An item with no value
  for a numeric column **matches a range containing zero**. The cause is one step upstream of where
  the ⊘ sits: `BatchColumn::decode` copies numeric values out of the Arrow array with `.values()`
  and **drops the validity buffer**, so absence is indistinguishable from a stored zero by the time
  `write_column_values` sees it. Text and category columns do check — a null string becomes
  `ScalarValue::Null`, a null key becomes the reserved code 0 — and the presence bitmap is already
  there to receive a third case.

  **The fix is shaped but it crosses a decision.** Carrying the validity buffer and skipping nulls
  into presence closes the *filter* half exactly as the other two families do it. But `by_entity`
  feeds the **render** tail as well, and a render column is fixed-width with no room for a validity
  mask (per-point-attributes §2.2) — so what a viewer sees for an absent numeric is a question this
  fix forces and does not answer. Today it is zero, silently. Someone has to rule whether it stays
  zero, explicitly, or whether absence is representable there at all.

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
