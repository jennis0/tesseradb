# Handover: term images in the bundle

**Date:** 2026-09-17. **Status:** Handover, ephemeral: delete once the work is merged and the decision record
and design documents carry it. Design settled with the owner (Joe); nothing built.
**For:** the agent implementing it. **Evidence:** [`2026-09-14-term-images.md`](2026-09-14-term-images.md),
`probes/2026-09-16-project-transient/`, and branch `probe/projection-build`
(the projection-build probe README and the bench binary `term_images_probe.rs`; read them with
`git show probe/projection-build:<path>`). Line numbers below are to `main` at `1e8d0559` and will drift.

## 1. What and why

A session's row projection is built by walking one permutation slot per entity in its fragment: 5 to 7.5 ns per
entity after the bucket fix, 7 to 14 s for principals above 45% access at rung 6 (3.5×10⁹ rows), measured. Users
hold 5 to 100k terms and every access set is near-unique, so whole-profile caching does not remove it.

A **term image** is one authorisation term's base posting projected into one view's row space, run-optimised,
stored in the bundle and read mapped. A session unions the images of its terms that have one and walks the rest.
Measured at rung 6 with stand-in terms: 300 years at 65% access 16.3 s to 2.1 s; 1k size-weighted species at 49%
12.9 s to 4.1 s; the broadest (100k species, 90%) is won only by the complement route, 19.8 s to 4.6 s.

## 2. Owner rulings (settled; do not reopen)

1. Images live **in the bundle**, written by build and by fold, covered by the bundle's digests. `BUNDLE_FORMAT`
   11 to 12 (decision 0048: no compatibility, stale bundles refused).
2. **Keep rule:** an image is kept only above **30 rows per container**. A constant, stamped in the file header
   (ruling A).
3. Images are **read mapped** as frozen `BitmapView`s, never built in the serving process.
4. **One implementation** writes images at build and at fold (decisions 0091, 0139).
5. **Extents get no images** (ruling B). Rows flushed or merged since the last fold are walked; served sets are
   identical either way; an ingest-only deployment gets images at its first fold.
6. **The complement route is included**, chosen by cost, not by a 50% share (ruling C).
7. **Rung 6 is rebuilt for measurement** with each row's access list `[country, "y:"+year, "s:"+specieskey]`
   (ruling D), about 1.4×10⁶ terms and about 10.5×10⁹ pairs, modelled. Check postings build time and disk before
   committing to the build and report them.
8. **Register:** widen Appendix C row C19 with two branches rather than add a row (ruling E): (a) the route choice
   is a function of the principal's own grant, pre-overlay, the same shape as the walk's time; (b) per-term
   page-cache warmth of the image file across principals, the per-term cache timing channel the owner ruled very
   minor.
9. **Filter postings** (category, text) are out of scope (ruling F).
10. **View groups** pay table plus payload per key's view; accept it and report image bytes per view in the build
    report (ruling G).

## 3. Design

### 3.1 What a term is, and which images exist

- Terms are the authorisation term space: the dictionary `terms/postings.arrow` is indexed by, reached through the
  plugin (`point_visibility.field`, `terms_of_labels`, architecture §6.1). Not value columns.
- `build_fragment_with_deltas` (`crates/tessera-authz/src/fragment.rs:76`) is a plain **union** over the satisfied
  terms of base posting plus each live delta tier's posting, then `run_optimize`. The exactness argument below
  rests on this union; if a plugin ever composes terms any other way, the route is wrong. Assert it in a test.
- Images are keyed by (partition, view, incarnation, term) for every view with a base, including `group:key` views.
  A view created while running has none until its first fold. Terms interned after the base (dictionary extents)
  have no base posting and are always residual.

### 3.2 Exactness

Let P_t be term t's base posting, D_t the union of its delta postings, F the fragment, T the satisfied terms,
B = `RowSpace::project_base`, X = `project_extents_from(·, 0)`. Today the projection is B(F) ∪ X(F)
(`permutation.rs` ~1595). B maps entities to rows pointwise, so it distributes over union. For kept K ⊆ T, each
P_t ⊆ F, so B(F ∩ P_t) = B(P_t) = I_t, and

    projection = fast_or(I_t : t ∈ K) ∪ B(S) ∪ X(F),   for any S with  F \ ∪_K P_t  ⊆  S  ⊆  F.

Build S as `build_fragment_with_deltas(T \ K) ∪ ∪_{t∈K} D_t`, then **`and_inplace(F)`**. The intersection makes
S ⊆ F by construction: `generation.delta_postings` can hold tiers newer than those F was built from, and without it
S could carry entities outside the fragment, an I2 disclosure. No assumption that delta entity ids sit above the
base bound.

The complement route: walk `[0, bound) \ F` and subtract from `[0, rows)`; valid only where `dense_rows` is set and
`bound == rows` with no extents (as `9956360c` checked). Take the arithmetic from branch `store/complement-walk`
commit `9956360c` (parked); not its 50% threshold. Its referee asked for: transient bound stated (up to 880 MB),
C19 wording not overstating what the principal holds, the `read.rs` premise sentence, the architecture C19 line.
Apply those.

### 3.3 Deny, suppression, deletion

Images are pre-overlay, like `RowProjection` (`compose.rs:1-27`). Only how `RowProjection.rows` is computed
changes. `EffectiveMask` still composes deny and buffer on every request, so a suppression applies from acceptance
(decision 0041). A suppressed entity stays in P_t and I_t as it stays in F. A deletion leaves P_t and I_t only at
the fold that executes it: fold pass 2 drops it from postings, pass 2b derives images from those postings. No third
removal route (write-path §5.4).

### 3.4 Derivation, one function

A new module `term_images.rs` in `tessera-store`, beside `derived.rs`:
`derive_term_images(space, dict_len, posting: PostingWalk, stamp, out) -> TermImageSummary`, using
`derived::PostingWalk` so the store does not depend on `tessera-authz`. Per term: skip postings of 30 entities or
fewer (they cannot pass); else `project_base_with` with one reused `ProjectScratch`, `run_optimize`, statistics;
write frozen bytes if kept; always write the table row. Rayon over term ranges with a scratch each, concatenated in
term order; bytes identical to sequential (test it). Probe cost: 184 s for 1.4×10⁶ terms warm at rung 6, of which
52 s was value-column passes postings replace (measured).

- **Build:** `crates/tessera-build/src/pipeline.rs` step 10b, per view, beside `artifact_pass::run` (~2159), after
  `permutation.bin` is written and fsynced (~2153). Add to `artifact_paths` so step 11 digests it (~2241).
- **Fold:** new pass 2b in `compact::execute` (`crates/tessera-engine/src/compact.rs` ~1154), on the fold thread,
  after pass 1 (permutation) and pass 2 (postings), from the fold's own new files. Into `written` so pass 5 digests
  it; entries carried on `CompletedFold` into `SEGMENTS-n` in `publish_fold` (`write.rs` ~7828). **Not** in
  publish_fold's derived block (~8279): that runs on the executor and minutes there stall acks. `plan_fold`'s
  estimate (~660) gains the largest image as built (at most 8 KiB × row-space containers, about 437 MB at 3.5×10⁹
  rows) plus the scratch pool (at most 82 MiB), modelled.
- **Flush, merge, coalesce:** write no images; side-manifests copy the image list unchanged.

### 3.5 File

One file per (partition, view): `partitions/<phash>/term-images/term-images-<n:06>-<idx:03>.timg`, named through
`DerivedIndex` (`derived.rs` ~1025) with a new counter.

| part | contents |
|---|---|
| header, 128 B | magic `TSMIMG01`; header version; `keep_rows_per_container` (30); `dict_len`; `table_offset`; `payload_offset`; stamp: prefix, base `seg_id`, view incarnation, base `row_count`, permutation `bound` |
| table, 40 B × `dict_len` | per term, dense by id: `offset u64` (0 = not kept), `len u32`, `rows u64`, `containers u32`, `arrays u32`, `runs u32`, `bitsets u32`, pad |
| payload | kept images in CRoaring frozen form, each at a 32-byte-aligned offset, in term order |

The dense table gives the chooser sizes for unkept terms too: 56 MB per view at 1.4×10⁶ terms, 10 KB for 254
countries (modelled).

**Manifest:** `SegmentsManifest.term_image_extents: Vec<TermImageExtent { path, view, incarnation, dict_len,
keep_rows_per_container }>` beside `tile_index_extents` (`manifest.rs` ~1925), `deny_unknown_fields`, no
`serde(default)`. File listed in `files`, so `Verification::Digests` (`read.rs` ~394) covers it with no new code.

**Open:** map; check magic, version, table bounds, alignment, ascending non-overlapping offsets, threshold 30, and
the stamp against the prefix, base `seg_id`, incarnation and `row_count`. On any failure warn and set that view's
images to `None`: the walk serves, answers unchanged. Attach as `ViewData.term_images: Option<Arc<TermImages>>`
(`read.rs` ~55), carried by `with_segment`, `with_merged`, `with_manifest`.

**Version:** `BUNDLE_FORMAT` 11 to 12 (`crates/tessera-types/src/lib.rs:162`, and its test at ~186). Correct
contracts.md §2's stale `bundle_format = 7` heading in the same change.

### 3.6 The session route

- `RowProjection::new` (`compose.rs` ~88) takes `ProjectionInputs { fragment, satisfied, postings, deltas, images }`.
  Callers: `session_geometry` (`viewport.rs:2737`) and refresh rung 3 (`refresh.rs:207`). Rungs 1 and 2 (`extend`,
  `rebase_extents`) unchanged. **Do not touch `select.rs` or the viewport sweep.**
- Order: whole-domain range answer first (existing, `Permutation::project_with`); else `choose`; then walk, split
  (`fast_or` over `BitmapView`s, `or_inplace` B(S), `or_inplace` X(F), drop views, `Self::over`), or complement.
- `tessera_authz::residual_fragment(...)` builds S in entity space; no `RowId` enters authz. The union and
  `choose` live in `tessera-store`; the engine assembles inputs.

**Chooser**, from the fragment and the table only, before any route runs, ties to the walk:

- `held = F.range_cardinality(0..bound)`; kept terms: satisfied, `< dict_len`, `offset != 0`.
- residual estimate: Σ `rows` of unkept satisfied base terms + Σ cardinalities of satisfied terms' delta postings
  (an overcount, biased toward the walk).
- walk = 6.5 × held; split = 350 × (arrays + runs) + 1,000 × bitsets + 11 × residual;
  complement = 11 × (bound − held), only when valid. ns, modelled from the 2026-09-16 probe; the walk constant
  predates the bucket fix (walk now 15 to 29% faster). One constants table; stage 6 re-takes it.

**Memory:** view headers 22 to 24 B per kept container, up to 406 MB for the broadest principal, dropped after the
union; union peak is the projection itself; residual walk adds 68 to 72 MiB (all measured). Bounded by the existing
`ComputeGate`. Held memory unchanged, about 430 MB for broad scattered principals. Headers are not cached across
sessions (about 400 MB permanent at species scale, modelled). Cold start reads up to 3.5 GB and 4.8 s creating
views, up to 2.85 GB more in the first union; warm mapped union 1.05 to 1.19× built (measured).

Add a `projection_builds_by_route` gauge to `/control/status`.

## 4. Verification

- **Store:** round trip; every open refusal (magic, version, bounds, alignment, overlap, threshold, each stamp field)
  yields `None`, never a panic; keep boundary (exactly 30 × containers not kept, +1 kept); proptest over random
  permutations (absent slots, an absent page), postings and kept sets that
  `fast_or(I_K) ∪ B(S) == B(F)` for S = R, S = F and S between; parallel derivation byte-identical.
- **Engine** (a new `term_images_route.rs` integration test in `tessera-engine`): a built fixture with a dense kept term, a
  scattered unkept term and many small terms; principals from 0.1% to 95%. A private forced-route override; every
  route's `RowProjection` equals `RowSpace::project`, as bitmaps and as serialised bytes after `run_optimize`.
  Repeat after: a flush under kept and unkept terms; a merge; an accepted delete and a suppression (whole `viewport`
  responses equal across routes); a fold executing the delete, reopened by `open_written_prefix` and `open_bundle`;
  a view created while running then folded; refresh rung 3.
- **Fold:** each image equals an independent `project_base(P_t)`; a fold with no deletions over a fresh build gives
  table and payload byte-identical to the build's (stamp differs).
- **Build against ingest:** one corpus built, one ingested into an empty database and folded; served sets equal per
  principal through `tessera_id` (entity ids differ, decision 0091).
- **Integrity:** a flipped image byte refuses `open_bundle` with `FileVerificationFailed`; a stamp mismatch serves by
  walk with equal answers.
- **Conformance:** the Python oracle is unaffected (M_auth from `pairs.parquet`). conformance.md §4.6: no row moves;
  I1 and I2 cells gain a sentence that fixtures exercise the split route, which needs a principal with a kept term;
  assert via the gauge.

## 5. Stages

One worktree per stage from current `main`, own `CARGO_TARGET_DIR`; check the base is main's head (tool-made
worktrees have been 1,000+ commits behind). A separate referee agent per branch before merge, invariants first.

1. **Store format and derivation:** `term_images.rs` (writer, reader, stamp, `derive_term_images`, `choose` and its
   constants), store tests. No bundle change.
2. **Build writes, open maps:** step 10b, `TermImageExtent`, format 12, `ViewData.term_images`, open validation.
   Tests: images equal independent walks; digest refusal; stamp drop; parallel byte identity. Nothing serves from
   images yet. Rebuild test fixtures and the committed small bundles.
3. **Fold writes images:** pass 2b, `CompletedFold`, publish assembly, `plan_fold` term. Tests from §4 Fold.
4. **Session route:** `residual_fragment`, split union, `ProjectionInputs`, both callers, gauge, forced-route suite,
   conformance run, C19 text.
5. **Complement route:** port `9956360c` into the chooser with the referee's fixes; its five tests plus a
   forced-route case in the stage 4 suite.
6. **Measurement at rung 6:** first the postings build time and disk check for the rebuilt corpus (ruling D), then
   the build. Report added build wall and image bytes per view, added digest time at open, and for the eleven
   principals the chosen route against every forced route, warm and cold, with first and repeat wall, anonymous
   peak and bytes read. Re-take the chooser constants. Time pass 2b on a fold of `gbif-64p`, rung 6 modelled from it.
   Done when the chosen route is within a stated margin of the fastest forced route for every principal, figures in
   a probe README.

**After stage 6: a review by a Fable agent** (`model: fable`) of the whole piece before it is called done,
invariants first (I2 above all: no image, residual or complement may contribute a row the fragment does not grant;
deny and deletion rules; build equals ingest), then correctness of the chooser and the fold, then quality. Joe asked
for this explicitly.

**Then the records:** a decision record (next free number; 0141 and 0142 are taken) with rulings 1 to 10, and the
design documents: architecture §6.3, §10.3, §11.3, §14, Appendix C19; contracts §2.1 to 2.3; compaction §3 pass 2b,
memory table, §4, §12, §14; write-path §4.6, §7, §8; caching §5 S2, §11, §13; views; ingest; conformance §4.6. Move
the term-images memo's §6(a) to decided. Sonnet drafts document prose; check its figures.

## 6. Standing rules for this work

- **Rung 6 bundle** (`data/ladder/gbif/bundle`): never open it uncapped. Use
  `systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G`, `nice -n 10`. Before any run, check
  nothing else is using it (`pgrep -af tessera`, serves on 8141 to 8143, a battery); stop and report if something is.
- Kill only by pid, never `pkill` or by name. Start long runs with `setsid` and wait for them; do not end a turn with
  a build or run in flight, and brief subagents the same way.
- Do not touch `crates/tessera-engine/src/select.rs`, the viewport sweep, or branch `probe/identity-bands`.
- Gate per branch: targeted tests with `--no-fail-fast`, clippy `-D warnings`. One full gate (CLAUDE.md list) per
  merge to main. The Python check needs `TESSERA_BIN` pointing at a current binary; a stale `target/release/tessera`
  produced 26 false failures on 2026-09-17.
- House style `docs/agents/writing.md`: British spelling, figures marked measured, modelled or assumed, no TODO or
  FIXME. Commit trailer `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`.
- Stop and ask Joe where an answer would set an invariant or something not ruled above. Messages to him in plain
  English, results first, lettered options.

## 7. Loose ends from the investigation

- Branch `probe/projection-build` (head `c03bac66`) is unmerged. Its README no longer links the deleted handover.
  Merge it or leave it; it is evidence only.
- Branch `store/complement-walk` stays unmerged; stage 5 supersedes it. Delete both it and its worktree after
  stage 5 merges.
- Worktree `.claude/worktrees/agent-a4375cad08ea2cbef` holds an untracked `target-probe/`; remove the worktree once
  the probe branch is dealt with.
- The term-images memo (`docs/evidence/memos/2026-09-14-term-images.md`) is untracked; commit it with stage 1.
