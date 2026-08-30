# Test audit R4 — `tessera-store` and `tessera-roaring`

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests, not
the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect in
shipped code was found. Track R4 of Wave 1 of the test-quality campaign
([`test-audit-campaign.md`](../../test-audit-campaign.md)); Wave 0's
[reachability memo](2026-08-30-test-reachability.md) is the input for which tests run.
`conformance.md` §4.6 is untouched — a row moves only when a test moves with it, and no test moved.

## Results

**197 tests assessed, and the surface is healthy.** `cargo test -p tessera-store -p tessera-roaring`
at the base commit runs **197** and all pass: 73 `#[test]` blocks across
`src/{membership,vocabulary,sidecar,manifest,derived,render_presence,locator,merge,fold,flush,read}.rs`,
117 integration cases across the seventeen files in `tests/`, and 7 in `crates/tessera-roaring/src/lib.rs`.

**L1 is clean and took one command.** Neither crate holds an `#[ignore]`, and neither holds a
`#[cfg(feature = …)]` anywhere in a test file — so the resolver-2 coupling Wave 0 found on the
engine cannot exist here, and `-p` and `--workspace` select the same set. Nothing further to say
about reachability.

**The conventions that make this crate auditable are the engine's, and they are used harder.** Most
tests carry a *"**Mutation:** …"* paragraph naming the specific defect the body kills
(`tests/fold_external_ids.rs`, `tests/merge_execution.rs`, `tests/reclaim.rs`, `src/locator.rs`), and
several carry a non-vacuity guard in the same breath — `tests/merge_execution.rs:436`'s *"the
fixture must place the live watermark above the merged range, or this proves nothing"* is the form.
Three tests run a genuinely **independent oracle** rather than a second call to the code under test:
`tests/permutation_project_parallel.rs`'s `serial_project`, `src/sidecar.rs`'s
`multi_extent_randomised_probes_match_a_naive_full_scan_oracle`, and
`tests/merge_execution.rs:328`'s concatenate-and-sort reference, which compares **whole segment
files byte for byte** rather than row content. Where a claim is about *how* something was produced
rather than what it produced, the assertion is on the artefact: `tests/reclaim.rs:38` asserts the
carried-forward file by **inode**, which is what separates a hard link from a copy.

The crate is also already awake to this campaign's own subject. `tests/segment_roundtrip.rs:245`
records, at the test, that *"asserting `is_err()` alone cannot tell the two orderings apart, which is
why the earlier version of this test passed for as long as it did"* — a 68 GB `/tmp` incident is
named as the cost.

**Three findings, one of them S1.** All three were confirmed by mutation rather than by reading
alone, because in each case reading left genuine doubt.

## Findings

### F1 — the fold's `row-entity.u32` is never checked against the permutation written beside it

**Claim:** compaction's pass 1 writes `permutation.bin` and `row-entity.u32` in one loop and they
must be inverses (I4's explicit entity↔row conversion; `tests/row_entity.rs`'s module doc states the
harm plainly). No test anywhere asserts that the fold's two outputs agree.

**Evidence.** `crates/tessera-store/src/fold.rs:282`–`:283` writes both from the same loop:

> `permutation.set(entity, row_count).map_err(perm_io)?;`
> `row_entity.push(entity_u32);`

and `crates/tessera-store/src/fold.rs:317` persists the second. `tests/fold_row_space.rs:66` names
`row_entity_path` when calling the fold and **never reads the file back** — its permutation test
(`:210`) checks `permutation.bin` alone, including the `0xFF` sentinel by raw bytes, and stops there.
`tests/row_entity.rs` does assert the inverse property, but over a hand-supplied `row_order` fed
straight to `write_row_entity`, never over a fold's output. The engine's two route-agreement
tests — `viewport.rs:6170` `filter_routes_agree_over_the_domain` and
`tests/filtering.rs:1478` `a_narrow_viewport_over_a_broad_filter_tests_its_own_rows` — would catch a
wrong table, but both build their row space from `write_row_entity` over a hand-made or built-bundle
order, so neither exercises the fold's production of it.

There is no structural guard downstream either: `crates/tessera-store/src/permutation.rs:638`
`with_row_entity` stores the table with no validation at all — not even a length check against
`base_rows`.

**Mutation run.** In a throwaway worktree, `row_entity.push(entity_u32)` was moved *above* the
tombstone `continue`, so the table gains one entry per **input** row rather than per **emitted** row
— every row after the first tombstone is attributed to the wrong entity, and the table claims more
rows than the segment holds. **`cargo test -p tessera-store` stayed green at 190/190**, and
`cargo test -p tessera-engine --test fold` stayed green at 28/28. Reverted; nothing committed.

**Class:** missing. **Severity: S1** — an invariant claim on the disclosure surface with no test.

**What a defect would let through:** `RowSpace::entity_of` is how the per-tile crossing route decides
which of a viewport's rows belong to a filter's verdict set
(`crates/tessera-engine/src/viewport.rs:5305`, reached from `:2531` and `:2597`). A table
misaligned with the permutation admits rows whose true entity is **not** in that set — and the set is
a subset of `M_auth`, so the error is in the widening direction. The route is chosen by a cost ratio,
so the same request answers correctly on one shape and wrongly on another.

Two things hold the practical risk below the severity, and both belong in the record. The two lines
are adjacent and read the same variable, so the defect needs a deliberate reordering rather than a
slip; and a view that publishes no table cannot take the route at all (`can_invert`), so the failure
mode is a *wrong* table, not a missing one.

**Confidence:** high, mutation-proven for `tessera-store` and for the engine's fold binary. The whole
workspace was deliberately not run — two other audit tracks were running concurrently — so the
absence is established for this crate and for the one engine binary that folds, not swept globally.

**Disposition:**

### F2 — the row-major columns' width validation cannot be distinguished from the length check

**Claim:** `membership.rs`'s label and list column readers each carry two width rules — the width
must be 1, 2 or 4, and it must be able to address the level's ordinals *and* the hole sentinel — and
the tests cannot tell either of them from the generic length check that follows.

**Evidence.** The two rules are `crates/tessera-store/src/membership.rs:1189` and `:1206`:

> `if u64::from(ordinals) >= u64::from(hole_at(width)) { … "{ordinals} ordinals cannot be addressed at width {width}" }`

with the length check at `:1212`. The only two corruptions aimed at them are in
`a_torn_or_foreign_row_column_refuses`: `membership.rs:2095` (`odd_width[6] = 3`) and `:2117`
(`narrow[6] = 1`, with the comment *"255 ordinals at width 1 leaves no value for the hole"*). Both
mutate byte 6 of a column whose good bytes are `pack_label_column(300, &[…4 labels])` — 16 header
bytes plus four labels at width 2, so 24 bytes. Changing the width to 1 or 3 changes the length the
header *describes* to 20 or 28, so `:1212` refuses either corruption on its own.

Every refusal on these paths is the same `StoreError::MalformedBundle { detail }`, so an error-kind
assertion could not separate them either; only a `detail` substring could, and none is checked.

**Mutation runs.** Two, each in a throwaway worktree, each with `cargo test -p tessera-store --lib`:
deleting `:1206`'s ordinals-vs-width check left **73/73 green**; deleting `:1189`'s width-set check
left **73/73 green**. Reverted; nothing committed.

The list column carries the identical pair at `membership.rs:1345` and `:1361` and the test aims
**no** width corruption at it at all — its three list faults are `backwards`, `wild_entry` and
`short_list`.

**Class:** under-discriminating (label), missing (list). **Severity: S3** — a real defect could pass.
Not S1: the failure it guards is an artifact-serving mis-attribution, not a mask.

**What a defect would let through:** a column whose declared width is internally consistent in
length but too narrow for the level. At width 1 over 300 ordinals the stored value 255 reads as
`ROW_COLUMN_HOLE`, so those rows silently belong to nobody and the artifacts holding them stop being
candidates — the direction the module doc at `membership.rs:1020` says the checks exist to refuse.
Removing the width-set rule instead lets `label()`'s `_ =>` arm read four bytes at a three-byte
stride, which is an out-of-bounds panic rather than a wrong answer.

**Confidence:** high, mutation-proven twice.

**Disposition:**

### F3 — `tessera-roaring` exists for a cost property and tests only correctness

**Claim:** the crate's reason to exist is `CLAUDE.md`'s measured cost model — *bitmap operations cost
O(containers touched), not O(cardinality)* — and none of its seven tests would notice a regression to
value-by-value insertion.

**Evidence.** All seven cases route through one helper,
`crates/tessera-roaring/src/lib.rs:231` `agrees`, whose whole body is *build the set both ways and
`assert_eq!` the bitmaps*. The module doc states the measured stakes — 3.4 s against a ruled 0.5–1 s
budget for the filter at 10⁹, 8,267 ms → 1,277 ms for the projection — and no test touches either
number. The gap is sharpest because the slow implementation is **already in the file and proven
equivalent**: `rebuild_staged` (`:189`) is exactly the per-value `add_many` path, and
`the_rebuild_fallback_agrees_with_the_format_path` (`:308`) asserts it agrees. A `push_block` rewritten
to stamp entities directly would keep every one of the seven green, and would also keep
`tests/permutation_project_parallel.rs`'s serial-oracle cases green, since those pin the answer and
not how it was reached.

**Class:** missing. **Severity: S3** — no disclosure and nothing irreversible; what regresses is a
measured budget the filter scan and the projection both sit inside. Recorded rather than escalated
because it is a *have not tested*, not a *cannot test*: the probes exist
(`probes/2026-08-08-filter-layout/`, `probes/2026-08-14-project-decomposition/`) and are simply not
in the gate.

**What a defect would let through:** a refactor that "simplifies" the hand-written portable-format
writer into insertions. Every mask stays correct; a session's projection at 10⁹ goes from ~1.3 s to
~8.3 s, and nothing in the suite says so.

**Confidence:** high, by reading. Mutation not used — the seven bodies assert set equality and
nothing else, which reading settles.

**Disposition:**

## The `is_err()` seam — 44 sites, and why two of them matter

The concentration is real: 44 of the repository's 95 bare `assert!(x.is_err())` are in this crate, 32
of them in `membership.rs`'s four `a_torn_or_foreign_*_refuses` tests. **Two matter, and they are
F2's**; the rest are sound, for a reason worth stating once rather than site by site.

These tests are built the way a kind-checking test would have to be built anyway. Each starts from a
`good` fixture, applies **one** byte-level corruption, and asserts a refusal — and the corruptions
are chosen so that each isolates exactly one check in the reader's `frame`, which is a straight-line
sequence of guarded early returns. Deleting any one check therefore turns exactly one `is_err()`
line red. Traced individually for `ContainmentPack` (`membership.rs:1852`, the canonical example the
brief names): its five headline corruptions reach the magic check, the version check, the id-width
check, the total-length check and the ordinal-span check respectively, and its three semantic ones —
a wild identifier, a clause overrunning its expression, a short ordinal span — reach the three checks
nothing else can reach. The `id_width = 3` case survives deletion of the width check with the length
arithmetic unchanged, so it is genuinely discriminating there, which is precisely what its label
sibling is not. Every one of the four tests also has a positive-control round-trip beside it
(`:1704`, `:1827`, `:1923`, `:2017`, `:2042`), so a reader that refused *everything* would go red.

The remaining twelve sit outside `membership.rs` and are each the single assertion of a
single-corruption test with a positive control elsewhere in the same file: `derived.rs:1811`–`:1819`
and its four siblings (six distinct rejections of a persisted shape-row form — another level version,
another segment, another row count, a row past the count, torn, over-long); `locator.rs:163`;
`render_presence.rs:198`; `merge_execution.rs:269`; `flush_segment.rs:238`; `membership.rs:1756`
and `:1762`.

**Where an error kind could not have helped anyway.** Every framing refusal in `membership.rs`
returns the same `StoreError::MalformedBundle { detail }`. On these paths a `matches!` on the variant
would be no stronger than `is_err()`; only a `detail` substring assertion discriminates, which is the
form `tests/bundle_read.rs` and `tests/reclaim.rs:98` already use where the variants differ.

## What was checked and found sound

Kept so the attacks are not re-run.

- **The bundle read protocol asserts dispositions, not "an error happened".** Every corruption case
  in `tests/bundle_read.rs` matches the variant *and* the reason:
  `open_bundle_fails_closed_on_a_corrupted_columns_arrow_byte` requires
  `NoVerifyingSegmentsManifest { highest_candidate_error: Some(reason) }` whose text names
  `columns.arrow` **and** describes a digest mismatch, with a `panic!` on every other variant. The
  same shape covers `UnverifiedFile`, `InvalidPermutation`, `UnsafePath` and the two projection
  refusals. The file's `add_segments_manifest` helper deliberately holds verification constant so a
  step-down can only have come from the honourable-state check.
- **The deny step-down walk is covered at depth, and knows what it cannot cover.**
  `the_walk_refuses_a_deny_it_reaches_only_after_stepping_down` puts the deny two steps down a
  four-manifest walk, and its doc states that no fixture can catch every truncation and why the
  general prohibition is a rule in `load_verifying_segments_manifest`'s doc instead. The negative
  control (`a_manifest_with_no_unhonourable_state_opens_at_the_highest_n`) is present, so a guard
  that refused nothing would not pass.
- **`Permutation::project` is differentialled against an independent oracle**, across both croaring
  container encodings, across more than one 2²²-row bucket (the case that makes the stamp's reuse and
  its clearing run at all), with sentinels and out-of-bound entities mixed in, and under one, two,
  four and eight rayon workers. `tests/permutation_project_parallel.rs`'s module doc says why the
  oracle is not a second call to `project`.
- **Rule F's fold — pass 1 — is pinned on the code and never on coordinates.**
  `every_surviving_points_triple_matches_its_input_exactly` compares `(tessera_id, morton, residual)`
  as a multiset read straight off the mapped files, so a dequantise-requantise round trip shows up
  even though row counts, membership and sort order would all still look right. The permutation's
  `0xFF` sentinel is asserted by raw bytes at a known offset, not only through `row_of`.
- **The k-way merge is compared to the superseded implementation byte for byte**, both segment files,
  from an interleaving fixture — not "it merges", and not "same rows".
- **The two writers of a membership extent are pinned to produce identical bytes**
  (`the_streaming_writer_produces_the_same_bytes_as_pack`), which is the only thing that stops a
  build's extents and a fold's diverging.
- **The mapped and in-memory readers are the same reader**, asserted per format
  (`ContainmentPack`, `TileIndexPack`, `LabelColumnPack`, `ListColumnPack` each compare `open` against
  `from_bytes` and then compare `as_bytes`).
- **`seg_id` is never reused, and both consequences are tested.** ABA safety at
  `tests/permutation_extents.rs:95` and `tests/incremental_bundle.rs:196` (a merge whose inputs are
  gone publishes nothing), and the link-onto-existing refusal at `tests/reclaim.rs:174`, whose doc
  cites contracts §2.1 as the reason the collision is a caller bug.
- **`reclaim_prefix`'s live-prefix refusal matches the variant deliberately**, and the test says why
  in its own words: a `contains("live")` assertion *"goes quietly green the day someone rewords the
  message"*. This is the only operation in the system that deletes bundle data.
- **The render-presence merge trap is tested from both sides** — a reversal that happens to look
  right, and then a rotation that does not — with the fail-open direction named at the test
  (`src/render_presence.rs:200`).
- **`sidecar.rs` carries a randomised full-scan oracle** alongside its open-at-most-once and
  order-preservation cases, and the "fatal trap" of a locator sized to the live entity space rather
  than the snapshot's has its own end-to-end test through the real reader
  (`tests/fold_external_ids.rs:281`).
- **Two thin tests in `tests/permutation_extents.rs` are weak without being misleading, and are not
  findings.** `RowSpace::project` is `base.project(mask)` unioned with `project_extents_from`
  (`src/permutation.rs:813`–`:815`), so `an_extent_free_row_space_projects_exactly_what_the_base_does`
  restates that delegation and can only fail if the delegation is removed. Its sibling
  `projecting_the_whole_equals_the_union_of_the_parts` looked tautological on the same reading and is
  not: an implementation that stopped unioning the extents turns it red. What neither asserts is the
  **disjointness** their shared doc claims, since union is idempotent — but the property is covered
  next door by `collapsing_adjacent_extents_preserves_every_row_id` and by
  `tests/row_entity.rs:79`'s both-directions round trip across the base/extent boundary.
- **`tessera-roaring`'s format path is pinned to succeed**, which is the half F3 does not cover:
  `Sink::flush` carries `debug_assert!(false, "the packed stream must be a valid portable bitmap")`
  before falling back, so any test that packs a stream croaring would reject panics in a debug test
  build. What is untested is only whether the format path is *taken at all*.

## L4 — the claims of `contracts.md` §2, checked one at a time

The absence lens was worked as an enumeration of the design's claims rather than as a reading of the
tests, because absence is the class a test-first pass cannot see. Each row is a normative claim this
crate owns; the third column is the test that would go red if it were violated.

| Claim (`contracts §2`) | Covered? | Where |
|---|---|---|
| §2.1 `seg_id` never reused, across flushes, merges, compactions and prefixes | yes | `tests/permutation_extents.rs:95` and `tests/incremental_bundle.rs:196` (a merge naming absent inputs publishes nothing); `tests/reclaim.rs:174` (linking onto an existing target refuses rather than overwriting) |
| §2.1 `CURRENT` is the only mutable file; a side-manifest is never replaced | yes | `tests/manifest_write.rs:355`, asserted at the filesystem operation, with the mutation named |
| §2.1 a `files`-map key may not escape the prefix | yes | `tests/bundle_read.rs:880` (a traversing `seg_id`) and `tests/reclaim.rs:128` (`..`, absolute, backslash) |
| §2.3 a candidate carrying deny-disposition state is never stepped past | yes, at four depths | `tests/bundle_read.rs:463`, `:493`, `:521`, `:561`, with the negative control at `:659` |
| §2.3 a file named in `files` but never verified fails the open | yes | `tests/bundle_read.rs:809`, matching `UnverifiedFile` |
| §2.6 `columns.arrow` is `(morton, tessera_id)` ascending, no further tiebreak | yes | `tests/segment_roundtrip.rs:197`; and the merge and fold each assert it over an interleaving fixture |
| §2.6 a column the manifest does not declare, or at a type it does not, is a typed error at open | yes | `tests/segment_roundtrip.rs:383` (the pre-residual three-column file), `:423`, and the declared-width round trip at `:662` including `boolean` at a row count that is not a multiple of eight |
| §2.6 no coordinate is stored; a position survives as `(morton, residual)` byte-exact | yes | `tests/merge_execution.rs:132` and `tests/fold_row_space.rs:171`, both as multisets read off the mapped files rather than through `unsplit32` |
| §2.6 `permutation.bin` header, `bound` semantics, `0xFFFF_FFFF` sentinel | yes | `tests/segment_roundtrip.rs:229`, `:247`, `:604`; the sentinel by raw bytes at `tests/fold_row_space.rs:262` |
| §2.6 a permutation slot past `row_count` is refused at open | yes | `tests/bundle_read.rs:840`, matching `InvalidPermutation` |
| §2.6 a scattered permutation is byte-identical to a sequential one | yes | `tests/segment_roundtrip.rs:561`, on the whole file |
| §2.6 `row-entity.u32` is the inverse of `permutation.bin` **as the fold writes it** | **no** | **F1** |
| §2.6 a merge writes neither `permutation.bin` nor `row-entity.u32` | yes, for the segment half | `tests/merge_execution.rs:390` pins the merged directory to exactly four files |
| §2.4 the external-id sidecar resolves both directions, and a locator is sized to the snapshot | yes | `tests/fold_external_ids.rs:281`, through the real reader |
| §2.4 keep-newest across runs, and a tombstoned newest binding does not fall back | yes | `tests/coalesce_runs.rs:118` and `tests/fold_external_ids.rs:243` |
| membership/containment/tile-index/row-column framing refusals | yes, except the width rules | the four `a_torn_or_foreign_*_refuses` tests; **F2** for the two width rules |
| `tessera-roaring`'s cost model | **no** | **F3** |

Two rows outside this crate's ownership were not chased: `contracts §2.4`'s dictionary extents are
positional in `tessera-filter` (track R5), and §2.6's `L₀` generation split is marked ⊘ unbuilt
under decision 0072, so there is nothing to test.

## Two things this track did not settle

Neither is a finding; both are recorded so the next reader does not spend the budget again.

- **F1's absence is established for `tessera-store` and for `tessera-engine --test fold`, not
  workspace-wide.** The full sweep was skipped on purpose while two other tracks were running. If the
  fix wave wants the stronger claim, the mutation is three lines and the run is one command.
- **`ContainmentPack`'s narrow-identifier rule has no test** —
  `membership.rs:570`'s *"`{expressions} expressions cannot be addressed by a 2-byte identifier"*.
  It is not reported as a finding because no fixture in the crate builds a level with more than
  65,536 expressions and the check is unreachable at any size a test would construct; it is named
  here so that "no test" is on the record rather than assumed absent by oversight.
</content>
</invoke>
