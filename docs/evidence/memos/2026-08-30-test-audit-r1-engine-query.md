# Test audit R1 — the engine's query and serving path

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests**,
not the code under them: nothing below is a claim that the engine is wrong, and no live defect in
shipped code was found. `conformance.md` §4.6 is untouched — a row moves only when a test moves
with it, and no test moves in this wave.

Track R1 of the test-quality audit. The question is not "does this test assert?" but *can this test
fail, and does it fail for the reason its name claims* — plus *what claim in this subsystem has no
test at all*. The reconnaissance baseline (no `assert!(true)`, no self-comparison, no commented-out
tests) was taken as given and not re-derived; so was
[test reachability](2026-08-30-test-reachability.md), whose one finding on this surface is now
closed (below).

## Results

**This surface is healthy, and unusually so.** Roughly 12,400 lines of integration test across
twelve files, plus the `#[test]` blocks of eleven `src/` modules — about 250 test functions in
scope. Every test read against the design section it names checked what that section says, with
one mis-named exception (F3) and no vacuous ones.

Three properties are pervasive here and are what makes the surface strong, so they are worth
recording as the local convention rather than as praise:

- **Non-vacuity guards.** A large fraction of the tests assert their own premise before asserting
  their claim — `"a real subset"`, `"or this test proves nothing"`, `"the eviction must actually
  have evicted"`, `"the versions must advance, or this is vacuous"`. `cache.rs:119`'s
  `tighten_to_one_entry` documents the silent mistake it was written with and explains that the
  guard is what stops both its callers being vacuous. Several tests also record the *mutation* that
  turns them red (`deny_mask.rs:157`, `viewport.rs:2514`).
- **Independent oracles rather than branch-against-branch.** `selection.rs:650`'s `reference_served`
  transcribes §7.2's definition in a deliberately different shape; `cut.rs`'s `reference_cut`
  does the same for the artifact cut; `region_leaf.rs:62`'s `inside` reads the fixture generator
  rather than the bundle. Where two mechanisms *are* compared
  (`tiered_decode_matches_the_per_value_path_…`, `the_two_contains_routes_agree`) the doc says so
  and names the definition test that pins the shared behaviour.
- **Two principals wherever a masking claim is made.** The I2-bearing tests
  (`underlay_sub_cells_sum_to_the_tile_s_masked_visible_count`, `underlay_totals_are_per_viewer`,
  `the_zoom_zero_count_equals_the_anchor_a_client_could_solve_for`,
  `a_region_counts_exactly_for_each_principal_…`, `eviction_never_widens_a_mask`) all run a wide and
  a narrow credential and assert the narrow one exactly, not merely that it is smaller.

**The 95-instance `assert!(x.is_err())` pattern the campaign flagged repo-wide is not a problem
here.** Six bare `is_err()`s occur on this surface (`filtering.rs:1117`, `:1200`, `:1879`, `:2428`,
`:2434`, `:3668`). Every one is a *secondary* arm of a test whose primary assertion checks the
error's kind or text, and four are bracketed by an `is_ok()` control on the restored input — so a
regression to "always refuses" is caught by the same test.

Three findings, ranked. Two are absences on the `region` leaf, which is the newest machinery on this
surface and the only place a stated design guarantee has no test at all. The third is cosmetic.

---

## F1 — No test exercises a `region` leaf across a geometry publication, so the region cache's generation term is unasserted

**Class:** missing. **Severity:** S1 — an invariant clause with no test.

**The claim.** `selection-operand.md` §7 states, under **I11**, that *"the region's row set is a
row-space artefact and carries the generation it was built against, as §5's cache key states"*, and
`crates/tessera-engine/src/region.rs:90` states the mechanism at the field:

> `segments_version` is in the key so a stale entry is never usable (I11): a row-space artefact
> keyed on anything else survives a merge that renumbered the rows under it.

**Evidence.** `crates/tessera-engine/tests/region_leaf.rs` is the only test of the region leaf
anywhere in the workspace (`FilterExpr::Region` appears in no other test file). Its four cases open
the engine at `:122`, `:161`, `:221` and `:294`, and **none of them renumbers the row space**: there
is no flush, no merge and no fold in the file, and no ingest that would extend it.

A reader grepping the file will find state changes, and they are the wrong ones. `:303` publishes an
artifact and `:354`/`:367` are a suppress/unsuppress pair — an artifact publication advances a
*level* version and a deny op moves the overlay, and neither touches `segments_version`, which
tracks the geometry the rows are numbered in. `:227` and `:240` set `set_max_region_cells`, which is
the fourth key term and the only one any case does vary. So `RegionKey`'s `view`, `prefix` and
`segments_version` terms are never varied, and no region answer is ever taken at two generations.

The retention rule makes this live rather than theoretical:
`crates/tessera-engine/src/write.rs:4681`'s `prune_region_cache` keeps entries at
`segments_version >= live - KEEP_SUPERSEDED_GENERATIONS`, so **the superseded generation's
decompositions are deliberately still resident**. What prevents a request at the new generation
reaching them is the key field alone.

**What a defect would look like.** `RegionKey` derives `Hash`/`Eq` over its fields
(`region.rs:94`), so the plausible defect is a simplification — dropping `prefix` as "redundant with
`segments_version`", or lifting the region cache to a coarser key when someone shares it more
widely. The interior bitmap is built from `tile_ranges_all` over the *old* segments
(`region.rs:161`), so a stale hit returns row ids from a row space that has been renumbered. The
consumer still intersects with the live composed mask, so the observable is **not** a cross-principal
disclosure: the viewer is served their own points, but the wrong ones — marks outside the lasso they
drew, and a `matched` count that does not correspond to any shape. That is I11's own description
("not stale-restrictive but simply wrong"), and it fails silently, with no error and a plausible
number.

**Why this is worth an S1.** `conformance.md` §4.6 already records I11 as uncovered, and `CLAUDE.md`
records that its cover was deleted with the pin that carried it — i.e. this is a *testing* gap, not
a machinery gap. The region cache is a new row-space artefact that states an I11 obligation at its
own key and does not discharge it. The test is cheap: build, region-count, publish a second geometry
the way `cache.rs:75`'s `publish_second_geometry` does, region-count again against the oracle, and
assert the second answer is the new row space's.

**Confidence:** high. Established by reading and by grep over the whole test tree; no mutation was
needed, because no test in the surface constructs two generations at all.

**Disposition:**

---

## F2 — The region cache's digest-collision guard has no test, and `src/region.rs` carries none at all

**Class:** missing. **Severity:** S3 — a real defect could pass unnoticed.

**The claim.** The region cache is keyed on a **truncated** digest — the first 128 bits of SHA-256
over the shape's canonical bytes (`crates/tessera-engine/src/region.rs:105`) — and the design closes
the collision by comparing the bytes themselves on every hit. `region.rs:116` puts it at the field:
*"The canonical bytes, compared on every hit so a digest collision is detected rather than argued
away."* The check and its branch are at `crates/tessera-engine/src/viewport.rs:3870`:

```rust
Ok(entry) if entry.is_of(&canonical) => entry,
Ok(_) => Arc::new(build()),
```

**Evidence.** `is_of` appears at exactly two sites in the workspace — its definition
(`region.rs:199`) and that match arm — and at none in `tests/`. `src/region.rs` has **no `#[cfg(test)]`
module at all**: it is the only module in this surface's `src/` list with zero tests. The `Ok(_)`
arm is unreachable in every test that exists.

**What a defect would look like.** A collision cannot be forced in a test, which is precisely why
the guard is the kind of code that gets removed as redundant — *"the digest is the key, so the
entry is the shape"*. With it gone, one viewer's lasso is answered with a different shape's rows,
and because the region cache is deliberately principal-free (`region.rs:7`) the wrong shape may
have been decomposed for someone else. The count is still masked, so this is a wrong answer rather
than a cross-principal disclosure — but it is the guard that lets the key be truncated at all, and
nothing would go red if it went.

Not testable as a collision, but testable as a contract: `is_of` is `pub` on a `pub struct`, so a
unit test that a decomposition built from one shape's canonical bytes reports `false` for another's
pins that the comparison is on content — and keeps the branch alive in coverage.

**Confidence:** high (grep is exhaustive for a two-site symbol). **Mutation:** not used; reading
leaves no doubt that no test reaches the branch.

**Disposition:**

---

## F3 — `item_lookup_goes_through_the_permutation_not_a_column_scan` does not discriminate a column scan

**Class:** mis-named. **Severity:** S4 — weak, and not misleading to anyone who reads the doc.

`crates/tessera-engine/tests/viewport.rs:692`. The body resolves the *last* source item's
`tessera_id` and asserts it comes back with the right external id — which catches "a truncated or
first-rows-only lookup", exactly as the test's own doc comment says at `:688`. It does not catch a
linear scan: an implementation that walked the entity-id column top to bottom would find the same
row and pass. The name is the half a reviewer greps for, and it claims the mechanism rather than the
property.

The doc is honest about the gap, which is why this is S4 and not higher; the name is what would
benefit from matching it.

**Disposition:**

---

## Checked and found sound (recorded so the attacks are not re-run)

**Reachability (L1).** Wave 0's one finding on this surface is closed: `bench-timing` is now named
explicitly on the engine's self dev-dependency (`crates/tessera-engine/Cargo.toml:71`), and
`cargo tree -p tessera-engine -e normal,dev` resolves the package with
`bench-timing,default,fault-injection`. So `-p` and `--workspace` select the same tests, the two
`viewport.rs` cases that were gated entirely now always run, and the four that lose an assertion arm
no longer do. `timing.rs`'s `laps_accumulate_across_visits` has a one-armed `if cfg!(…)` and would
assert nothing in a build without the feature — it was checked for that reason and is **not** a
finding, because no selection the repository offers produces such a build. Its three siblings all
carry an `else` arm.

**The three-threads tests.** `viewport_output_is_byte_identical_at_compute_threads_1_and_8` and its
two siblings are the campaign's named example of a fidelity failure, and the engine's copies are
clean: `viewport.rs:2514`'s doc records that review caught exactly this, names the fix
(`set_serial_fallback_max_rows_for_test(0)` on **both** engines, `:2577`), and states plainly what
the test degrades to without the feature. With F-above closed, both engines take the genuine
`pool.install` branch.

**I7, the selection definition.** `selection.rs:575`'s
`selection_matches_the_definition_over_both_internal_branches` runs a half-populated mask against an
independent transcription of §7.2 that computes `C_θ` over the *visible* rows only
(`selection.rs:650`), across four depths, four caps, three floors and two thresholds, and asserts a
per-branch coverage count because an earlier single threshold would have passed with one branch
dead. `every_served_row_is_visible` (`:735`) closes the membership half over a sparse mask. The
floor clause has three tests including one that demonstrates the consequence the config refusal
exists for (`a_zero_floor_would_blank_a_tile_…`, `:299`).

**I2 and the θ anchor.** `the_theta_anchor_falls_when_an_item_is_suppressed`
(`viewport.rs:1697`) pins the composed-total anchor against the exact differencing attack the design
names, and its doc says why nothing else in the tree would catch a regression to
`base.cardinality()`. `the_zoom_zero_count_equals_the_anchor_a_client_could_solve_for` (`:1763`)
pins the three-crate coincidence `GET /v1/meta`'s disclosure argument rests on. The underlay's two
cases assert per-tile sums against the *masked* visible count for both principals.

**I12, both halves.** `filtering.rs:1581` asserts `visible` does not move under a filter while
`matched` does, and `filtering.rs:1569` asserts the same at the tile-sum level for a narrow
principal. `region_leaf.rs:137` asserts it for the region leaf.

**The region leaf's own disclosure surface, apart from F1 and F2.** The one place
`selection-operand.md` §6 says a disclosure could enter — *"the verdict is a function of the shape
alone, never of the data"* — is asserted for both principals on both the exact path
(`region_leaf.rs:132`) and the cover path (`:236`). The cover test's principal assertion is a lower
bound only (`matched(&narrow) >= exact_narrow`, `:238`), which was examined as a candidate finding
and rejected: the exact test asserts the narrow principal's count *exactly*, and the mask
intersection it thereby pins is the same consumer code the cover path runs, so a cover-branch-only
masking defect is not constructible. The budget is demonstrated to be in the key by the
cover-then-exact sequence at `:240`. Cross-principal cache sharing is exercised in the right
direction — the wide principal misses, the narrow one hits, and the narrow count is asserted exactly
(`:141`).

**The caches.** `tests/cache.rs` is the strongest file on the surface: every case that depends on an
eviction having happened asserts that it happened, every case that depends on two principals seeing
different sets asserts that too, and the module doc explains why the assertions go through the real
request path rather than comparing two in-process `RowProjection`s. `eviction_never_widens_a_mask`
records both mistakes made writing it and the guard that caught them.

**I13a.** `crates/tessera-engine/src/single_flight.rs` carries 25 unit tests covering the panicking build, the oversized
build, the pruned build, the timed-out waiter, the cancelled waiter, the unwinding build that must
not delete a later builder's slot, and the prune that must not be undone by a publish. The region
cache is a `SingleFlightCache` (`crates/tessera-engine/src/session.rs:1303`), so `selection-operand.md` §7's I13a
paragraph is discharged by inheritance rather than by absence.

**The masked-count histogram.** `src/histogram.rs`'s module doc enumerates nine key terms with a
reason each; its unit tests exercise three of them as key-discriminating (`token_id`, `layer`,
`overlay_version`). This was pursued as an absence and dropped: `level_version` and the publication
terms are exercised end to end by `tests/artifact_cache_cadence.rs` (another track's file), which
asserts that a publication is in the next request's cut and that a write re-derives what it moved.

**The drill-down gate.** `an_unknown_id_and_an_invisible_one_are_indistinguishable`
(`viewport.rs:837`) uses a zero-coverage session, which is the weakest form of "invisible" — a gate
that tested "does this principal see anything at all" would pass it. The *partial*-principal case
exists, in `tests/fold.rs:1955` (`engine.item(&subset, restricted_id, None).unwrap().is_none()`),
which is another track's file. Noted here only because the claim is a query-path one and its
strongest test does not live on the query path.

**Everything else read and found to check what it names:** `access_terms.rs` (five cases, each
asserting exact per-credential counts including the null-fill and the comma-in-a-term case),
`deny_mask.rs` (four, each with its mutation or its non-vacuity guard recorded),
`compose.rs` (ten, including the `visible_to`-versus-`compose` differential over every precedence
branch and the count-versus-materialised-rows agreement), `staleness_hint.rs` (three, each
guarded by an assertion that the dictionary actually moved), `stepped_down.rs`, `send_sync.rs`
(a compile-only assertion, correctly), `filtering.rs`'s vocabulary section (the gate applied to both
request forms, and the page cut after the gate rather than before), `src/cut.rs` (a reference-oracle
property test over random trees), `src/select.rs`, `src/filter.rs`'s traversal-equivalence tests for
§2.1's "an unresolvable value is an empty operand", and `src/compose.rs`'s run-walk decode tests.

## What was not done

No file was edited, nothing was committed, and no mutation worktree was created — reading against
the design left no case in genuine doubt about whether a test could fail. `conformance.md` §4.6 is
untouched: F1 names a test that would move I11 partway, but the test does not exist yet, so the row
does not move.
