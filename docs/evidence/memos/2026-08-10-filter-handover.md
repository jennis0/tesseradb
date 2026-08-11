# Filtering: what is not finished, and what the next person needs to know

**Date:** 2026-08-10 · **Status:** Handover memo — evidence, not normative. Names open work; rules
nothing.
**Reads with:** [`filter-index.md`](../../design/filter-index.md) (r7, Provisional — the
specification for the artefact and its lifecycle), [`filter-surface.md`](../../design/filter-surface.md),
decisions [0039](../../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0062](../../decisions/0062-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0063](../../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
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

One of the three is now built; the other two refuse by name, which is the fail-closed shape.

**`none_of` is built** (2026-08-11, [decision 0066](../../decisions/0066-none-of-requires-a-value-and-names-one-column.md)),
and the two fences came down together because one requirement answers both. It means *carries a
value in this column, and none of these matches it* — a **positive** predicate, so §5's failure
arithmetic never inverts: an entity whose value is unreachable is absent from `present` and matches
nothing, exactly as it matches no `eq`. That is also decision 0062's C11 mitigation, arrived at from
the other side — evaluation inside the candidate makes a carrying entity the witness for its own
value's visibility, so `none_of: [every offered value]` is empty by construction and the "one extra
intersection" 0062 anticipated is the presence requirement itself. A negation names one column,
refused otherwise. Five tests carry it, each verified to fail under the complement reading.

**`match`** is specified and unbuilt; nothing depends on it.

**Multi-valued / list attributes** ([decision 0039](../../decisions/0039-multi-valued-categoricals-are-slow-path-only.md))
need their own addressing before anything else. §2.1's value column maps one presence bit to one
slot, and the affine-rank traversal, the fold's blanking and the coalesce's merge are all built on
that. §2.6's older claim that "lists cost no format work" was withdrawn for this reason; §6.2 and
§5.2 now scope themselves to the shipped families explicitly.

## 3. Decisions that were waiting on the owner — all ruled

The four this section listed were ruled on 2026-08-10 and are recorded in
[`2026-08-10-filter-rulings.md`](2026-08-10-filter-rulings.md), which carries the arguments. In
short:

- **First-touch digest deferral for `attrs/`** — **declined**; the sweep is parallelised instead
  (`verify_files`, rayon over the file list, first-in-sorted-order error preserved), because the
  measurement said it was I/O-bound rather than hash-bound. Deferral would have traded a disclosure
  control for something a `par_iter` recovered.
- **Where the fold's flip opens `FilterColumns`** — **keep as built**, post-flip, as §6.2 says.
- **Crate isolation for the scan** — **neither**. The cause was instruction-address alignment, not
  crate layout; `-C llvm-args=-align-all-functions=6` is applied (§1).
- **An r-letter for the contracts tree lines** — **no letter**.

One consequence to carry: with deferral declined, §5's reverted text-offset bounds check stays
reverted. It was redundant while Arrow validates offsets on decode, and nothing has changed that.

## 4. Promotion

`filter-index.md` is **Provisional at r7**, with its adversarial round dispositioned and §6.3's
rulings closed. One thing stands between it and normative:

- **§2's constants confirmed at a value width other than `u32` and on a string column.** The
  campaign swept `u32` and text separately and never crossed them.

Surface §4's project-vs-per-tile rule was the second, and it is **ruled and built** (2026-08-11,
decision 0065): both routes exist, `row-entity.u32` carries the crossing, and the threshold is three
times the viewport's rows. Surface §4 itself is still Provisional pending an owner ruling on the rule
it now describes.

Two amendments are owed to **normative** documents and should ride their own reviews, not this one:
compaction §2's table and §3's pass list gain the attribute pass and the band budget; write-path §7
and contracts §2.1 gain the coalesce's fourth axis.

## 5. Known gaps, each marked ⊘ at its claim

Three of the five are closed. `ValueColumn::open` now takes an `Access` rather than a
`mmap` bool, so the fold's pass 4a takes `MADV_SEQUENTIAL` on the layers it streams once — and
`FilterColumns::open` keeps its bool precisely so the hint stays unexpressible on the request path,
which is decision 0052 enforced by a signature instead of a comment. The pass's bytes read and
written are in the dispatch log line and in `/control/status` as `last_attr_bytes_*`. The numeric
absence gap closed on 2026-08-11 and is recorded below with the half of it that remains. Two remain:

- **Orphaned attribute files between folds — and the fold is already what collects them.** A
  coalesce writes a merged extent and stops naming the eight it consumed; the consumed files stay on
  disc. Nothing unlinks them as they are orphaned, so they pile up while the corpus is served.

  **The reclamation is compaction's, and it works.** A fold writes a new prefix, carries forward only
  what a manifest names, and deletes the old prefix whole — so every orphan in it goes at once, with
  no special handling and nothing to enumerate. The lifecycle probe measures exactly that: 739
  attribute files and 90.4 MB before the fold, **13 files and 72.7 MB after**.

  So there is no missing sweep, and the earlier framing of this as one was wrong. What is left is a
  sizing question: **the exposure is one fold interval of coalesce output**, ~18 MB per nine passes
  and tracking ingest volume rather than corpus size. Against a nightly fold that is a day's worth of
  a small number, and the honest answer is probably that nothing more is needed. If it ever is, the
  cheap version is a fold that also unlinks what its own plan just superseded, not a general sweep of
  the live prefix — that would have to enumerate every file a step-down could still name across the
  older `SEGMENTS-<n>.json`, which is how the dead-bytes gauge once reported a 1065× orphan ratio
  from a single missing addend.

- **The text-offset bounds check**, implemented, tested and **reverted** — it cost 70% of the scan
  for the reason in §1, which is now understood and pinned. It is redundant while Arrow validates
  offsets on decode, and the digest deferral that would have made it non-redundant was declined
  (§3), so it stays out. A redundant check bought at any price is still redundant.

- **An item with no number no longer matches filters it should not** — fixed 2026-08-11, on both
  write paths, [decision 0064](../../decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md)'s
  filter half. The build kept the source's null buffer instead of dropping it, the ingest plane
  gained a `WalScalar::Null`, and absence lands in the presence bitmap beside the column — the same
  place a category's reserved code 0 and a string's explicit null already put it, so all three
  families now record absence one way. Verified by mutation: the tests fail if the decode drops
  validity, and fail if the flush gives an absent value a slot.

  **The render half is still open, and is the deferred one.** `columns.arrow` is non-nullable
  (contracts R4) and 0062 defers its bitmap while the client is under development, so an absent
  number still *draws* at the type's zero. The two artefacts therefore disagree — the filter says an
  item has no score while the map draws it at 0 — which is narrowing rather than leaking (**I12**),
  and the substitution now lives in one named place (`ScalarValue::or_render_placeholder`) so the
  render half has a single call site to delete. What 0062 still owes: the file's name, its manifest
  entry, how the points batch says "absent", and how flush, merge and the fold carry it.

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
