# Test audit R7 — `tessera-build`, `tessera-corpus`, `tessera-analyse` and `tessera-spatial`

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests, not
the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect in
shipped code was found. Track R7 of Wave 1 of the test-quality campaign
([`test-audit-campaign.md`](../../test-audit-campaign.md)); Wave 0's
[reachability memo](2026-08-30-test-reachability.md) is the input for which tests run.
`conformance.md` §4.6 is untouched — a row moves only when a test moves with it, and no test moved.

## Results

**512 tests assessed across four crates, and the surface is healthy.** The per-crate listings at the
base commit are **367** (`tessera-build`), **90** (`tessera-spatial`), **44** (`tessera-corpus`) and
**11** (`tessera-analyse`); two of the 367 are `#[ignore]`d, so 510 execute.

**L1 took two commands and is clean.** None of the four crates declares a `[features]` section, and
none contains a single `#[cfg(feature = …)]` anywhere in `src/` or `tests/` — so the resolver-2
coupling Wave 0 found on the engine and Wave 1 found again in `tessera-types` cannot exist here, and
`-p` and `--workspace` select the same set. Both `#[ignore]`s
(`crates/tessera-build/src/residency.rs:808`, `crates/tessera-build/tests/build_equivalence.rs:1008`)
carry a reason and are measurement harnesses over a real corpus.

**Five findings, one S2 and two S3.** None of them is a test that cannot fail at all, and none is
unreachable. Four of the five are in the two smallest crates: **`tessera-build`'s 367 tests
yielded exactly one finding**, which is a striking result for the largest single body on this wave
and is not an artefact of a shallow read — the L4 claims table below was built from
`crates/tessera-build/src/`'s 389 refusal sites and every `⊘` marker in the crate, and the great
majority of them have a test that names the message.

**What makes this surface auditable, worth naming because a rewrite would lose it.** Three
conventions do real work here and go beyond the engine's:

- **The refusal is asserted by its message substring, essentially always.** The four crates hold
  **one** bare `assert!(x.is_err())` between them
  (`crates/tessera-build/tests/build_equivalence.rs:911`, the second arm of a test whose first arm
  checks the message). `tests/verify_deep.rs` routes every damage case through
  `expect_refusal(&root, "<substring>")`; `tests/build_layers.rs` asserts the offending key by name.
  R4 found 44 bare `is_err()` in `tessera-store` alone; this surface has essentially none.
- **The non-vacuity guard is written into the fixture, not left to the reader.**
  `tests/shape_ties.rs:167`'s `hold` asserts `inside > 0` *and* `inside < probes.len()` *and*
  `probes.len() > 16` *and* a non-empty interior before it compares anything;
  `tests/coordinate_width.rs:142` refuses its own fixture unless the pair collapses under `f32`;
  `tests/shape_wgs84.rs:11` states in its own header that *"these tests would pass under either
  semantics if they compared a densified shape with itself"* and then says what they compare
  instead.
- **Independent oracles rather than a second call.** `tests/morton_props.rs:8` writes its own
  `deinterleave` by hand; `tests/shape_ties.rs:188` compares each boundary cell's *carried* parity
  against a full ray cast from the cell corner; `tests/build_equivalence.rs` compares two whole
  bundles byte for byte with only `created_at` normalised, and guards the comparison itself against
  vacuity (`:445`, *"a bundle that only contained CURRENT and MANIFEST would pass the loop
  vacuously"*).

## Findings

### F1 — the golden vectors cannot detect the edit their own failure message names

**Claim:** `crates/tessera-analyse/tests/golden.rs:65`'s identity assertion says it guards two
cases, one of which — the vectors edited to match changed behaviour, without moving
`ANALYSER_VERSION` — it structurally cannot detect.

**Evidence.** The assertion compares the identity string recorded *in the vector file* against the
one the binary computes (`crates/tessera-analyse/tests/golden.rs:65`–`:73`):

> `assert_eq!(set["identity"]…, analyser.identity(), "the vectors were recorded under a different
> identity than this binary carries. Either the version moved without re-recording the vectors, or
> the vectors were edited without moving the version — and the second is the one that silently
> invalidates every index already built");`

`Analyser::identity` is `format!("{UNICODE}/{UNICODE_VERSION}")` over two hand-maintained
constants (`crates/tessera-analyse/src/lib.rs:165`, `:76`, `:110`). Neither side of the comparison
is a function of the vectors. So in the case the message singles out — someone changes the
tokeniser, watches `the_golden_vectors_hold` go red on a token list, and regenerates
`vectors/golden.json` from the new behaviour — the JSON's `identity` field is unchanged, the
binary's is unchanged, and the assertion passes. Every token assertion passes too, by construction.
The **first** case is genuinely caught; the second, which the message calls out as the dangerous
one, is not caught here or anywhere else in the repository.

The consequence is durable. `tests/text_index.rs`'s
`the_manifest_records_the_analyser_identity_per_column` pins the identity into the manifest, and the
reader checks a column against the analyser it was written with — but that machinery compares one
recorded string against another, so an unmoved version over changed behaviour makes both agree while
the terms disagree. `golden.rs:11` states the stakes in its own words: *"two analysers disagree by
producing different but individually valid terms — a mismatch nothing downstream can detect."*

**Class:** under-discriminating. **Severity: S2** — analyser identity is irreversible in the sense
this campaign's ladder means: an index built under a stale identity is not detectably wrong, and the
remedy is a rebuild nobody knows to run.

**What a defect would let through:** a behaviour change absorbed into the recorded answers instead of
into the version. What would notice is not a bigger assertion but a different one — a digest of
`vectors/golden.json` checked in beside `UNICODE_VERSION`, so that editing the answers without
moving the version fails on the constant rather than on the tokens.

**Confidence:** high, by reading; the two constants are literals and the comparison is between two
copies of the same string. Mutation not used, and would prove nothing reading does not.

**Disposition:**

### F2 — the permutation-coverage half of `tessera verify` has no deliberate-damage test

**Claim:** `correctness-suite.md` §11 states that `tessera verify` *"re-confirms the permutation
covers exactly the rows the segments claim"*, and no test anywhere damages a permutation to make
that check fire.

**Evidence.** The check is `crates/tessera-build/src/lib.rs:1654`:

> `if claimed != total_rows { … "view '{view_id}': the row space claims {claimed} rows but the
> segments hold {total_rows} — not a bijection" }`

with its per-row companion at `:1682` (*"no entity claims this row"*). The accept direction is
tested twice — `tests/build_smoke.rs:734` `verify_accepts_a_freshly_built_bundle` and
`tests/verify_deep.rs:308` `a_flushed_and_reingested_bundle_verifies_shallow_and_deep`. The refuse
direction is not: `tests/verify_deep.rs`'s ten damage cases cover a short column, an unsorted Morton
column, two posting faults, a locator fault, a sidecar digest, two dictionary-extent faults, the
pairs union and the source-binding refusal, and **none touches `permutation.bin`**. Nothing in
`crates/tessera-cli/tests/` or `conformance/tests/` damages one either.

This is the one §11 property whose damage test is missing, in a file whose own module doc adopts the
rule (`tests/verify_deep.rs:11`): *"§18 obligation 9: a checker nobody has seen fail is a checker
nobody knows works."*

**Mutation run.** In a throwaway worktree the `claimed != total_rows` refusal was replaced by
`let _ = claimed;`. `cargo test -p tessera-build --test verify_deep --test build_smoke --test
build_equivalence` stayed **green — verify_deep 12/12, build_smoke 12/12**. Reverted; the worktree
was removed; nothing committed. The whole workspace was deliberately not run, another audit track
being active.

**Class:** missing. **Severity: S3** — a real defect could pass. Not S1: the verifier is a checker
over a bundle at rest, and `open_bundle`'s `validate_rows` and `is_well_formed` already refuse an
out-of-range or aliasing permutation on the read path (`lib.rs:1624`'s own comment says so), so what
this check adds is the *surjective* half — a row no entity claims — which is the arm nothing else
holds.

**What a defect would let through:** a bundle carrying a row in `columns.arrow` that no entity can
address, passing `tessera verify` silently. The verb exists precisely to be the check that runs
where the data came from somewhere real (`correctness-suite.md` §11), so a verifier that stopped
checking is a check that reports success over corpora no fixture covers.

**Confidence:** high, mutation-proven for the three `tessera-build` binaries that verify, and
established by grep for the rest of the tree.

**Disposition:**

### F3 — `small_polygons.rs` asserts that two routes agree, not that either says *inside*

**Claim:** the file's headline is *"A polygon smaller than one depth-16 cell holds the point inside
it"*, and the body asserts only that the direct test and the boundary-cell route return the same
answer — which they do when both return `false`.

**Evidence.** `crates/tessera-spatial/tests/small_polygons.rs:1` states the claim and `:6`–`:8`
gives its provenance (*"The build reported these as holding a shape and no row (2026-08-29), and
this test is half of why that is right: both routes agree the source coordinates are inside"*). The
only assertion is `:66`:

> `assert_eq!(direct, via_cell, "{wkt}");`

The canonicalisation report is bound and printed at `:41` and asserted about nowhere. So a
canonicalisation that dropped a sub-cell ring — the exact hazard the file exists to record — gives
an empty decomposition, `direct == false`, `via_cell == false`, and a green test whose module doc is
then false.

The sibling test written against the same hazard shows the author already knows the guard is needed:
`crates/tessera-build/tests/coordinate_width.rs:193` asserts `report.rings_dropped == 0` with the
reason at the line (*"the square must survive quantisation, or nothing below is being tested"*), then
asserts `shape.contains(stored)` **true** and `shape.contains(narrowed)` **false**. `small_polygons.rs`
does neither.

Run with `--nocapture` at the base commit, both cases report `rings_dropped: 0`, `interior 0`,
`boundary cells 1`, `direct true via_cell true` — so the property holds today and the test simply
does not say so. Two further details belong in the record: `interior 0` means the interior-tile arm
of `via_cell` is never taken by either case, and the module doc says *"for these three"* while
`CASES` (`:23`) holds two.

**Class:** under-discriminating. **Severity: S3** — the failure direction is narrowing (a shape
holding nothing selects no rows), so it is not a disclosure; what it costs is the one regression
test standing behind `polygon-membership.md` §4.1's sub-cell case.

**What a defect would let through:** a canonicalisation or decomposition that loses a polygon smaller
than a cell. The build campaign that produced these fixtures found three real Overture divisions of
this shape, so the case is a measured one rather than a constructed one.

**Confidence:** high, by reading, with the current values confirmed by one `--nocapture` run.
Mutation not used — the assertion is a single equality between two booleans and reading settles what
it admits.

**Disposition:**

### F4 — `the_golden_vectors_hold` checks the first analyser's vectors and no other's

**Claim:** the file guarantees that every analyser this binary carries owes a vector set, and then
checks the vectors of exactly one of them.

**Evidence.** `crates/tessera-analyse/tests/golden.rs:61` opens with
`let set = &doc["analysers"][0];` and never indexes past it, while
`every_analyser_has_vectors_and_every_vector_set_an_analyser` (`:21`) compares **names** only. Add a
second analyser with a correct name and a wrong vector set and both tests pass. `ANALYSER_NAMES`
holds one entry today (`crates/tessera-analyse/src/lib.rs:115`), so nothing is untested now; the gap
opens on the day decision 0070's second pipeline arrives, which is the day the file is least likely
to be re-read.

**Class:** missing (latent). **Severity: S4** — weak but not misleading today, and the fix is a loop.

**What a defect would let through:** a second analyser shipped with unchecked golden vectors, under a
name the first test says is covered.

**Confidence:** high, by reading. Mutation not used.

**Disposition:**

### F5 — `the_served_rings_are_a_subsequence_at_every_resolution` does not check a subsequence

**Claim:** the name states the property `polygon-membership.md` §7.2 rests on — a served ring is the
stored ring *filtered*, so the drawn shape is a selection of the shape's own vertices — and the body
checks only that the vertex count does not increase as the resolution coarsens.

**Evidence.** `crates/tessera-spatial/tests/shape.rs:280`–`:283`. The whole of the resolution ladder
is:

> `let n: usize = rings.iter().flatten().map(Vec::len).sum();`
> `prop_assert!(n <= last.max(2048).min(last), "w={w} n={n} last={last}");`

(the guard reduces to `n <= last` for every value of `last`). No assertion compares a served vertex
against a stored one, at any resolution, in this file or elsewhere: `rings`' other tests
(`crates/tessera-spatial/src/shape/polygon.rs:558`, `:593`) assert ring counts and lengths, and the
`vertices_out` assertions in `tests/shape.rs:357` and `tests/shape_ties.rs:326` are about
canonicalisation rather than about serving. An implementation that resampled a ring — returning
interpolated points at a decreasing count — passes every one of them.

**Class:** mis-named. **Severity: S4** — the parts of §7.2 that decide what a viewer sees are covered
elsewhere and covered well (the ring-role rules at `polygon.rs:558` and `:593`, weight monotonicity
at `simplify.rs:113`, the guard's firing condition at `tests/shape.rs:291` and `:309`). What is
missing is the property the name promises, and the name is what a reviewer greps for.

**What a defect would let through:** a served ring whose vertices are not the shape's own. Since
§7.1 is explicit that *"a served ring is a drawing, never the predicate"*, this is a fidelity
question about the drawing and not a membership one.

**Confidence:** high, by reading. Mutation not used.

**Disposition:**

## The four handed items

### 1. Build and ingest held to the same behaviour (decision 0091) — **sound, and tested in the right place**

`tessera-build`'s own equivalence tests hold the two *build implementations* to each other, not the
build to the ingest: `tests/build_equivalence.rs` compares `build` against `build_in_memory` byte for
byte over seven fixtures, and `tests/build_layers.rs`'s `both_build_paths_place_the_same_layers_on_the_same_entities`
means the same pair. That is the correct division of labour, and 0091's own test exists: it is
`crates/tessera-server/tests/membership_column.rs:782` and `:805`
(`a_scalar_membership_column_ingests_the_database_a_member_table_builds` and its lineage sibling),
which build one corpus whole on one side, build a seed and ingest the tail on the other, and compare
tiles, points and artifacts across three credentials — with **two explicit non-vacuity guards** at
`:754` and `:771` saying in as many words that two identical empty answers must not pass. A projected
coordinate gets the same treatment at `crates/tessera-server/tests/projected_ingest.rs:286`. Those
are track R8's to assess; what R7 can say is that the claim is not orphaned.

**Where the two entry points could diverge without a test noticing, and why none of it is reported
as a finding.** The crate's build-only refusals were enumerated and each is about acquisition, which
0091 exempts: `--out` already holds a bundle (`crates/tessera-build/src/lib.rs:689`), zero points
selected (`:756`), `--view` selection (`crates/tessera-build/src/config.rs:1736`–`:1756`), the
extent being stated or fitted now (`config.rs:1216`, `:1271`), the memory-budget and batch-feasibility
refusals (`crates/tessera-build/src/pipeline.rs:603`–`:775`), and the input-immutability checks
(`pipeline.rs:248`). The one place the crate *duplicates* a control-plane rule rather than routing
through it is `crates/tessera-build/src/layers.rs:1798` `verify_dependencies`, whose own doc names the
hazard — *"a build that admitted what an ingest refuses is the fail-open half of one rule stated
twice"*. Its first arm is tested, from the engine side
(`crates/tessera-engine/tests/artifact_build_time.rs:519`, whose doc is *"The build refuses what the
ingest refuses"*); its second arm — an artifact attached into a layer the declaration does not name
in `depends_on` — has no test on the build side. That is not reported as a finding because the build
publishes through `LayerRegistry::prepare_publish` (`layers.rs:1327`, `:1350`, `:1453`), whose own
`UndeclaredAttachment` arm is tested at `crates/tessera-lifecycle/src/registry.rs:1844`, so deleting
the build's copy loses the address in the message and not the refusal.

### 2. Who tests the generator — **sound, and the strongest single answer on this surface**

The question Wave 2 will ask of `reference/oracle` has a good answer here, and it is not "the
generator's tests agree with the generator". Four things carry it:

- **Each of the four generator properties has a test whose fixture is the defect it rules out.**
  `crates/tessera-corpus/tests/generator_props.rs:57` `positions_spread_at_any_size` restates the
  superseded fixture's `((e·37) mod 1000, (e·53) mod 1000)` in its own doc and then requires 99,900
  distinct positions in 100,000 items **at two windows** — one of them a billion items in — and
  additionally caps the worst per-position pile-up at three. `:83` and `:105` attack the
  decorrelation property from two independent angles (an `x`/`y` cell diagonal, and a single term's
  carriers across the four quadrants at three seeds). `:130` refuses an affine `fx_key`. `:34`
  proptests that *n* appears in no derivation.
- **The census — the one O(*n*) method total verification leans on — is checked three ways**:
  against a direct visibility loop (`:169`), against itself across zooms by roll-up (`:191`), and
  against a **second, independent bucketing** through `morton_of` and `Tile::code_range` rather than
  through the shifted-cell path the census itself takes (`:210`, whose doc says why).
- **Every derived structure repeats the same three-part pattern** — both directions agree, the
  answer does not depend on *n*, and the census agrees with a brute-force enumeration — once each in
  `crates/tessera-corpus/src/partition.rs`, `artifacts.rs`, `boundary.rs` and `hierarchy.rs`, plus
  structural guards that would catch a degenerate generator (`artifacts.rs:438`
  `artifacts_within_a_level_overlap`, `hierarchy.rs:311` `the_tree_is_deep_and_unbalanced`).
- **The written forms are compared against the closed forms, not against each other.**
  `tests/materialise_roundtrip.rs` reads back `points.parquet`, `pairs.parquet`, the ingest
  `RecordBatch` and all four artifact fixture families and compares each row against `Corpus::item`,
  `Corpus::terms`, `partition_artifact_of`, `artifact_members`, `boundary_artifacts`,
  `artifact_parent` and `treed_members`. `:101` pins the prefix property at the *file* level.
  `ingest_batch_is_the_wire_shape_of_the_same_items` even pins per-column nullability, with the
  reason at the test.

**Would they catch a self-consistent but wrong corpus?** For the failure classes the design names —
a collapsed position space, a correlated grant structure, a giveaway join key, an *n*-dependent
derivation, a materialiser disagreeing with the lookups — yes, each by a test that would go red. The
one thing they cannot catch is a corpus that is *uninteresting* in a way `correctness-suite.md` §8
does not enumerate, and that is the honest residual of a generator that defines its own ground truth.
`Corpus::config_toml` is the remaining seam — the declaration must agree with the columns the
materialisers write — and it is exercised end to end from four call sites outside this crate
(`crates/tessera-cli/tests/corpus_cli.rs:180`, `crates/tessera-engine/tests/common/mod.rs:219` and
`:270`, `conformance/suite/verification.py:154`), which is the right place for it given
`check-layers.sh` forbids this crate depending on the build.

### 3. The golden vectors — **the repo's only known-answer set, and it has one real gap**

Verdict: **F1 and F4**. What is sound is worth recording alongside them, because the file does more
than most: it requires ten named script families by name (`golden.rs:99`–`:108`, with the reason at
`:96` — *"asserting the coverage here stops a future edit from quietly deleting the awkward cases
rather than fixing them"*), it asserts `vectors.len() >= 10` so a truncated file fails loudly, it
refuses five near-miss analyser names individually (`:44`), and it pins purity and
instance-independence (`:114`), which is the property the fold's merge argument rests on. The vector
file itself carries per-family `why` notes recording *why* an expected answer is right — the
diacritics case is the model — which is what makes "read back against the design's rules" a claim a
reviewer can check rather than a claim about a past reviewer.

### 4. The proptest generators — **sound; the strategies reach the interesting region, and the tie fixtures exist because the strategies do not**

- `crates/tessera-spatial/tests/shape.rs:131`'s `rings_strategy` draws star polygons with 3–40
  vertices, radii from 1,000 to 300,000 grid units on a 10⁶ extent, and independently sampled hole
  and second-part flags — so holes, multi-part shapes and shapes spanning a large fraction of the
  extent are all reached. `probes` (`:73`) is not uniform: it adds the eight-neighbourhood of every
  vertex and seven points along every edge, plus each of those shifted by one unit, which is where
  the tie rules live.
- **`shape_ties.rs` exists precisely because the strategies cannot reach the grid lines**, and its
  header says so (`:4`): *"The property tests in `shape.rs` draw star polygons at random `f64`
  positions and essentially never put a vertex on a tile boundary."* Its `hold` helper (`:167`, with its four guards at `:174`, `:185`, `:186` and `:196`) is
  the strongest single assertion block on this surface — four non-vacuity guards, the descent against
  the direct test at every probe, **carried parity against a full ray cast at every boundary cell**,
  and every interior tile's four corners tested directly. Ten fixtures drive it across depths 4, 10
  and 16: tile corners, far edges, a horizontal edge on a cell bottom, diamonds, collinear runs
  through canonicalisation, a hole touching its outer at a vertex, two parts sharing an edge
  (vertical and horizontal), rings through `(0, 0)` and through `u32::MAX`, and mixed-depth corners.
  The delegation to `hold` is not an empty body — it is where the assertions are.
- `small_polygons.rs` is **F3**, and is the one place on this surface where the delegation-versus-
  emptiness question resolves the wrong way: there is no helper, and the single assertion is weaker
  than the file's claim.
- `morton_props.rs` is sound: (b) round-trips through a hand-written inverse rather than through the
  library, and (c) documents at the test why depth is capped at 8 and why the capped range is
  representative — a modelled argument, stated as one.

## What was checked and found sound

Kept so the attacks are not re-run.

- **The two build implementations are compared as whole bundles, with the comparison itself guarded.**
  `tests/build_equivalence.rs:411` walks both trees, requires the same file *set*, normalises only
  `created_at` and skips only `CURRENT` (which is that manifest's digest), and refuses a bundle of
  fewer than seven files so the loop cannot pass vacuously. Seven fixtures drive it, each chosen for
  a branch the two could disagree on — including an attributed one whose header says exactly why a
  *freshly minting* vocabulary is out of scope and what is compared instead.
- **I9 is asserted directly and not only through byte equality.**
  `tests/build_equivalence.rs:941` recovers each entity's signature from `pairs.parquet`, requires the
  sorted signatures to be non-decreasing in entity id, and then requires identical signatures to form
  runs (`distinct < n / 4`) — so an ordering that happened to be sorted but scattered fails.
  `tests/build_smoke.rs`'s `signature_sort_key_is_the_sorted_term_id_list` and
  `entity_ids_break_signature_ties_on_the_morton_code` pin the key and the tiebreak separately, and
  `crates/tessera-build/src/lib.rs:2172` pins order-independence of the key itself.
- **The `verify --deep` damage tests repair the manifest digest before asserting**, so what fails is
  the structural check and never the digest sweep in front of it — the file says so at `:14` and
  cites §18 obligation 9 as the reason the accept tests exist at all. Every one asserts a message
  substring through `expect_refusal`, so a reader that refused everything for the wrong reason would
  not pass.
- **The filter-postings and keyword tests read back through the reader that will serve them** —
  `tessera_filter::ColumnPostings`, `SortedDict`, `ValueColumn`, `RecordBlob` — and both files state
  the reason in their headers: a test that re-implemented the decode would agree with itself.
  `tests/keyword_column.rs` asserts the *pair* (an entity's ordinal resolved against the dictionary
  written beside it equals the value that entity carried), which is the thing no unit test can reach.
- **`tests/text_index.rs`'s two-homes assertion** — an indexed `text` column writes both a token index
  and a blob row — is the failure mode it names: a build writing only the index answers `match`
  correctly and returns an empty field. Both halves are asserted, and the unindexed case is asserted
  as its complement.
- **`tests/render_presence.rs` declines to duplicate a comparison and says where it lives instead**
  (`:11`): the two builds' agreement on the presence bitmap is `build_equivalence.rs`'s, over a
  fixture whose rendered numbers already carry nulls. That is the correct disposition, not a gap.
- **`tests/render_tail.rs` gives `bool` its own case for a stated reason** — Arrow packs it least
  significant bit first, and a reversed bit order is a column of plausible booleans, every one of them
  another row's. Every renderable type is given values a wrong width would mangle.
- **`tests/attribute_pass.rs` asserts the coverage arithmetic against numbers the fixture computes for
  itself**, on the explicit ground that a tally that lost or double-counted a lane changes the report
  without changing one byte of the bundle — the one defect a bundle comparison cannot see. The corpus
  is deliberately not a multiple of 64 so a lane rounding to a word boundary writes into a
  neighbour's bits.
- **`tests/projected_build.rs` pins published addresses, not recorded ones.** The figures come from
  `test_corpora/common/projection-vectors.json`, shared with the Python module that placed the built
  geographic corpora, and the header names the two defects a self-comparison would miss — a frame
  mirrored north-south, and one with the axes exchanged. `clipped_points_are_counted_and_clamped_points_are_not`
  is the pair that stops one counter standing in for the other.
- **`tests/coordinate_width.rs` asserts positions and cells rather than types**, and says so
  (`:15`): *"Type signatures prove nothing here."* Its `f32` arm is asserted rather than described, so
  the test discriminates the width rather than passing against a reader that never narrowed anything.
- **`spill.rs`'s 45 unit tests attack every intermediate file three ways** — a byte flip against the
  content anchor, a truncation against the length, and trailing garbage — plus three proptest
  round-trips and the ascending-order refusals of each writer. The content anchor's own doc
  (`:1613`-ish, at `fn mix64`'s siblings) records why a plain sum was rejected: it can be
  compensated.
- **`config/tests.rs`'s 118 cases are an enumeration of `configuration.md`'s table, not a sample.**
  `:179` asserts the accepted key set *is* the table; `:367` requires every retired key to be refused
  rather than aliased; `:403` requires an unknown key to be refused rather than ignored. The
  specified-but-unbuilt refusals are pinned as refusals (`:892`, `:1629`), which is decision 0013's
  form applied to a parser.
- **`residency.rs:756`** pins the refusal that matters operationally — a memory budget under the
  mapped tail refuses **before the build starts** — with its complement at `:797`.
- **`BuildError::ExternalIdTooLong` is declared and never constructed**, which is correct and
  documented: the build's external-id representation is fixed at exactly eight bytes
  (`crates/tessera-build/src/lib.rs:1884`, `rows.iter().map(|row| row.source_id().to_le_bytes())`),
  and `crates/tessera-server/src/control.rs:438`–`:444` records that the contracts §1 cap is
  therefore enforced and tested at `/control/ingest`, which is the only caller-supplied-bytes path.
  Checked because a declared-and-unreachable refusal is exactly the shape this audit hunts; it is not
  one.
- **`require_decomposable_labelling` (`crates/tessera-build/src/pipeline.rs:3624`) cannot fail on its
  hash arm today** — `pipeline.rs:799` constructs `Passthrough::new()` and the function compares it
  against `Passthrough::new()` — and its own doc says why it is written that way: the hash check is
  what will hold when `build` grows a plugin parameter. Its probe arm *is* live and would fire if
  `Passthrough`'s rule changed. Recorded rather than reported, for the same reason as the line above.

## L4 — the claims of the build, checked one at a time

Built from `crates/tessera-build/src/`'s refusal sites, `⊘` markers and design citations rather than
from the tests, because absence is the class a test-first pass cannot see. The third column is the
test that would go red if the claim were violated.

| Claim | Covered? | Where |
|---|---|---|
| **I9** — entity ids are position in signature order, ties on the Morton code, permanent (§11.1) | yes, three ways | `tests/build_equivalence.rs:941` (order *and* run contiguity); `tests/build_smoke.rs` `signature_sort_key_is_the_sorted_term_id_list`, `entity_ids_break_signature_ties_on_the_morton_code`; `crates/tessera-build/src/lib.rs:2172` (the key is sorted, deduplicated, order-independent) |
| §11.1 — batch-scoped assignment is identity-bearing and must be replayed, not re-derived | yes | `tests/build_equivalence.rs:816` `batched_build_is_byte_identical_to_the_batched_reference`, and `needlessly_small_batches_are_refused` at `:890` for the fragmentation refusal, asserted on the message |
| The streaming build is byte-identical to the reference, over every branch the two could disagree on | yes, seven fixtures | `tests/build_equivalence.rs` (plain, field-sourced, attributed, tie-group, no-mint/no-pairs, batched, `--limit`), plus `streaming_build_is_deterministic` |
| **I4** — the permutation is a bijection onto its segment's rows | accept only | `tests/build_smoke.rs:734`, `tests/verify_deep.rs:308`; **F2** for the refuse direction |
| `correctness-suite.md` §11 — every `columns.arrow` column is the segment's row count; Morton codes non-decreasing | yes, by deliberate damage | `tests/verify_deep.rs:385`, `:410`, each asserting the loader's own message substring |
| §11 — postings sorted, duplicate-free, bounded by `entity_id_high_water` | yes | `tests/verify_deep.rs:434` and its duplicate-entity sibling |
| §11 — the external-id locator and sidecar agree in both directions, newest binding first | yes | `tests/verify_deep.rs` `a_locator_slot_addressing_another_entitys_binding_is_refused`, with the accept side at `the_fixture_carries_a_key_bound_in_two_runs` |
| §11 — dictionary extents positional, never repeating a descriptor (decision 0042) | yes, both arms | `tests/verify_deep.rs` `a_dict_extent_repeating_a_descriptor_is_refused`, `…_with_a_miscounted_declaration_is_refused` |
| §11 — `pairs.parquet` is the union of the base postings it was written with | yes | `tests/verify_deep.rs` `a_pairs_file_missing_a_base_pair_is_refused` |
| §11.1 — verifying against a source file is refused until `source.digest` exists | yes, as a refusal | `tests/verify_deep.rs` `a_source_binding_request_is_refused_until_the_contract_carries_the_field`; the ⊘ is at `crates/tessera-build/src/deep.rs:77` |
| `contracts §2.6` — `columns.arrow` is `(morton, tessera_id)` ascending; the tail order is the order `/control/ingest` builds each row's scalar vector in | yes | `crates/tessera-build/src/pipeline.rs:4463` (the comparator against a full `tessera_id` sort over engineered ties) and `tests/render_tail.rs` per type |
| `contracts §2.6`/§3.4 — a declared column may not shadow a fixed or reserved name | yes | `config/tests.rs:790`, `:798` |
| Contracts §1 — an over-long external id is refused, never truncated | not reachable at build | documented; enforced and tested at `/control/ingest` — see the sound list |
| `configuration.md` — every accepted key, every retired key, every unknown key | yes, as an enumeration | `config/tests.rs:179`, `:367`, `:403` |
| Specified-but-unbuilt declarations are refused rather than ignored (`multi`, `render_in`, view visibility, `withdraw_on_member_deletion`, a vocabulary `gate` column) | yes | `config/tests.rs:679`, `:689`, `:886`, `:1623`; `crates/tessera-build/src/input.rs:1388` |
| Vocabulary code rules — code 0 reserved, width bound, no reassignment of a retired code, no two values at one code | yes, one test each | `config/tests.rs:463`, `:469`, `:477`, `:485`; and at build, `tests/discovered_vocabulary.rs` `a_retired_code_is_never_minted_to_a_new_key`, `exhaustion_at_build_is_a_typed_error_naming_column_and_width` |
| `projections.md` §6 — both float widths accepted on input, the narrower widened; a narrowing decides a cell at a deep frame | yes | `tests/coordinate_width.rs`, all three cases |
| `projections.md` §3/§4/§7 — the transform, the frame, and the clip counter that the clamp counter cannot stand in for | yes | `tests/projected_build.rs`, seventeen cases including the report's wording |
| `polygon-membership.md` §4.3 — a WGS84 shape holds the rows of its *curved* image | yes | `tests/projected_build.rs` `a_wgs84_shape_layer_holds_the_rows_of_its_curved_image`; `crates/tessera-spatial/tests/shape_wgs84.rs` for the geometry |
| `polygon-membership.md` §7.2 — ring-role rules, weight monotonicity, the budget guard's firing condition | yes | `crates/tessera-spatial/src/shape/polygon.rs:558`, `:593`; `simplify.rs:113`; `tests/shape.rs:291`, `:309` |
| `polygon-membership.md` §7.2 — the served ring is a *filtering* of the stored ring | **no** | **F5** |
| `polygon-membership.md` §4.1 — a polygon smaller than a cell holds the point inside it | asserted only as an agreement | **F3** |
| `records-and-search.md` §4.4 — the analyser is pinned by known answers, and a change moves the version | half | **F1** — the behaviour-change direction is caught; the recorded-answer-edit direction is not |
| Decision 0070 — every analyser owes a vector set, and a declared name this binary lacks is refused | names yes, vectors partly | `golden.rs:21`, `:44`; **F4** for the vectors of any analyser but the first |
| Decision 0091 — the same corpus built or ingested is the same database to every client | yes, outside this crate | `crates/tessera-server/tests/membership_column.rs:782`, `:805`; `projected_ingest.rs:286` |
| Decision 0089 — every artifact of a layer declaring a dependency must attach into a layer it named | one arm | `crates/tessera-engine/tests/artifact_build_time.rs:519` for the *no attachment* arm; the *undeclared target* arm is held by the registry's own test — see handed item 1 |
| The layer refusals — an unassigned member, an undeclared key, a null entity, a cycle, a double parent, a level hole, a tiered edge within one level | yes, one test each, asserted on the offending name | `tests/build_layers.rs`, throughout |
| Ignore-and-report rather than refuse, for a source that meets nothing or partly | yes | `tests/attribute_sources.rs` `rows_naming_entities_this_build_did_not_load_are_ignored`, `a_source_that_meets_nothing_still_builds` |
| The disclosure report is derived from the declaration alone and is byte-identical between builds | yes, outside this crate | `crates/tessera-cli/tests/check_and_reports.rs:436`, `:457` |
| The generator's four properties, its census, and every materialised form | yes | see handed item 2 |

Two rows were deliberately not chased. `crates/tessera-build/src/config.rs:62` and `:3015` mark
`configuration.md` §1's **carry rule** as ⊘ unbuilt — codes are assigned from an empty slate every
build — so there is nothing to test. And `crates/tessera-build/src/artifact_pass.rs`'s four
report-and-degrade fallbacks (`:169`, `:510`, `:553`, `:591`) have no test; they are not reported as a
finding because `:389` records what a degraded layout costs — *"served artifact-major. Every answer
is unchanged; the layout is not"* — which puts them outside this campaign's severity ladder entirely.

## Two things this track did not settle

Neither is a finding; both are recorded so the next reader does not spend the budget again.

- **`correctness-suite.md` §11's ⊘ marker appears to be stale, in the direction `CLAUDE.md` warns
  about.** It says *"Every bullet above is new"* and that the built verifier's identity loop *"restarts
  its row index at zero for each segment while indexing an array spanning the whole view — correct for
  the single-segment shape a build produces, and wrong for any bundle that has flushed"*. Every bullet
  is in fact implemented in `crates/tessera-build/src/deep.rs` and damage-tested in
  `tests/verify_deep.rs`; and `crates/tessera-build/src/lib.rs:1665`–`:1673` looks the row base up from
  the row space per segment, with a comment saying it is done that way so the check cannot depend on
  the segment list's ordering — and `a_flushed_and_reingested_bundle_verifies_shallow_and_deep` passes
  over a multi-segment bundle. This is a **documentation** question, not a defect, and it is exactly
  the register error `CLAUDE.md` names: recording built machinery as absent understates the corpus's
  own gap. Not edited here; this track is read-only and the marker is not `conformance.md`'s.
- **The multi-pass content anchors are unit-tested and not end-to-end.**
  `crates/tessera-build/src/pipeline.rs:247` `input_changed` and
  `crates/tessera-build/src/input.rs:605` both report an input file mutated mid-build.
  `spill.rs`'s bucket, band, text-run and member-run tests fire the anchor at the *file* level, and
  `join_chunk_propagates_the_callbacks_error` fires the propagation — but no test mutates a points or
  pairs file between two passes of a real build. That is a *have not tested* with a real reason
  (arranging it needs a hook between passes), it discloses nothing, and it is named here so that
  "no test" is on the record rather than assumed absent by oversight.
