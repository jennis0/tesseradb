# The duplication and consistency pass

**Status:** Working memo, a plan. Written 2026-09-11 against main at `70de8d70`. Every claim in §2
to §5 was read from code, and the twelve build-versus-ingest rows in §2 were each confirmed at
both sites. Nothing here decides an invariant; the rulings it needs are in §7. Appendix A is the inventory:
every item the surveys found, with its sites and its home, by track. The pass changes no
capability: a register row whose closing would make the service do something new is handed to
[`2026-09-11-capability-gaps.md`](2026-09-11-capability-gaps.md) and marked so below.

**Reads with:** [`2026-09-11-capability-map.md`](2026-09-11-capability-map.md), which records what
is built and where the documents and the code disagree. This memo takes that memo's disagreement
table into one track (§6, T6) and adds the two kinds of disagreement it did not look for: the two
entry points disagreeing with each other, and one copy of a rule disagreeing with another copy of
the same rule. [Decision 0091](../../decisions/0091-build-is-ingest-into-an-empty-database.md)
governs the first kind: a statement a build refuses and an ingest accepts is a defect unless it is
about acquisition.

**The standing rule** (owner, 2026-09-11;
[decision 0139](../../decisions/0139-one-implementation-between-build-and-ingest-and-across-a-type-family.md)).
Build and ingest share one implementation of each rule and each transformation, and a type family
is handled by one implementation over its members. A second copy is an exception, argued at the
item, and none is assumed. Where two copies exist the survivor is the one with the better memory,
CPU and disk behaviour, and every correction the other copy carries is ported to it with its
test. §4's last two paragraphs list what stays as two copies under that rule, and why.

## 1. The finding

The build and the ingest path hold one set of rules in two spellings. The engine's copies name the
build function they were transcribed from and state that the two must agree
(`crates/tessera-engine/src/attributes.rs:132`, `vocabularies.rs:338`, `filter.rs:1543`,
`compact.rs:2018`, `viewport.rs:4225`, `flush.rs:1579`). Twelve of those pairs have stopped
agreeing. Each divergence is a statement one entry point accepts and the other refuses, or a value
one entry point stores at a different width, disposition or panic from the other.

The tree already holds the construction that closes this class. `ScopedScalar::licence_of`
(`crates/tessera-store/src/manifest.rs:638`) is one six-line predicate called from the build and
from the engine; the comment beside its build caller records that its two earlier spellings
"used to agree by argument, and either could have been edited alone". `LayerDeclaration::validate`
(`crates/tessera-types/src/layer.rs:1204`) is the one declaration surface both entry points check
with the same function. The pass applies that construction to the rest: each rule moves down into
the crate both callers can see (`tessera-types`, `tessera-spatial`, `tessera-store`,
`tessera-filter`, `tessera-analyse`), and the transcriptions are deleted. A divergence is then a
compile error or a single edit, and the register in §2 closes as a consequence of the move rather
than row by row.

Three further groups are in scope because they are the same mechanism at a different scale: seven
copies of the scalar-family table, one per crate (§3); six per-kind copies of the same skeleton
inside the engine (§4); and roughly 9,500 lines of test scaffolding that spell out struct literals
and fixtures the crates could provide once (§5).

## 2. The register: statements the two entry points answer differently

Each row is a statement a caller can make at a build and at a running service. "Design" names the
section that settles the direction where one does. Rows marked *ruling* are decided in §7.

| # | Statement | Build | Ingest | Design | Direction |
|---|---|---|---|---|---|
| 1 | attribute named `record` | refused, `config.rs:5008` | accepted, `attributes.rs:317` has no arm | records §2: `attrs/record/` is the record blob's directory | ingest refuses |
| 2 | attribute named after a registered layer | accepted, no comparison in `config.rs` | refused, `attributes.rs:160` | contracts §3.4: a batch's columns are declared scalars or layer names | build refuses |
| 3 | `type = "u16", vocabulary = "v"` | refused, `config.rs:4758` | accepted and rewritten to a category, `attributes.rs:404` | configuration §6 names the category spelling; the manifest stores a category as a width and a vocabulary | *ruling A* |
| 4 | vocabulary name outside `[A-Za-z0-9_-]` | accepted, `config.rs:4223` checks emptiness only | refused, `vocabularies.rs:340` | the name is served on `/v1/meta`; contracts §3.2's argument for a column name | build refuses |
| 5 | vocabulary value with an empty key | accepted, `config.rs:4424` | refused, `vocabularies.rs:359` | per-point-attributes §5; records §7 | build refuses |
| 6 | `reserved` naming code 0, or a code past the width | accepted, `config.rs:4406` checks `u32` only | refused, `vocabularies.rs:150` | per-point-attributes §3: code 0 is the absent sentinel | build refuses |
| 7 | view metadata integer past the declared width | refused, `config.rs:4056` | accepted: the manifest carries `ViewMetadataType::Int` and `view.rs:109` can only check that | views §3.2: metadata is declared over the attribute types, so the width is part of the declaration | ingest range-checks; the manifest field carries the width |
| 8 | integer where float metadata is declared | accepted and widened, `config.rs:3998` | refused, `view.rs:114` | none | *ruling B* |
| 9 | category-typed view metadata | accepted | refused at declaration and at create, `view_declarations.rs:341`, `roster.rs:269` | views §3.2: the create route resolves no key | ruling C; a capability, built under the capability-gaps memo |
| 10 | `visibility = ["inherited"]` | refused, `config.rs:3218` | accepted; neither `session.rs:3838` nor `view_declarations.rs:291` mentions the word | configuration §4: `inherited` is reserved for the container's gate | ingest refuses |
| 11 | metadata name outside `[A-Za-z0-9_-]` | refused, `config.rs:3731` | accepted, `view_declarations.rs:316` | the name is a field on `/v1/meta`; contracts §3.2 | ingest refuses |
| 12 | empty string in a `keyword` cell | refused, `pipeline.rs:3737` | accepted: `control.rs:961` decodes `Keyword` through the `Utf8` arm with no check, and `flush.rs:1626` makes it a dictionary key | records §7: the empty string stays refused; contracts §2.4 makes it a value for `text` only | ingest refuses |
| 13 | a supplied content `type` word | four accepted, `config.rs:5909` | any string accepted; `LayerDeclaration::validate` checks none, and `authored_shape_kind` knows three shape words, `layer.rs:396` | polygon-membership §6.1 names the shape words; no document names the closed set | ruling D: the pass checks the six words at both doors; drawing a circle or ellipse is the capability-gaps memo's |

Row 13 is the capability map's finding, restated here because it is the same class. One more
divergence is known and documented: `render` is refused on `PUT /control/attributes` under
decision 0136's amendment, and the engine keeps the build's two `render` refusals as dead arms so
the transcription reads the same. That one stays until the route can address a row.

The comment at `crates/tessera-engine/src/flush.rs:1584` states that the ingest plane refuses the
empty string. It does so for a category cell (`control.rs:1010`) and for nothing else. Row 12 is
the consequence.

## 3. Twins: one rule, several copies, and where each copy now differs

| Rule | Copies | Where the copies differ | Home after the pass |
|---|---|---|---|
| reserved column-name sets | `config.rs:4989`, `attributes.rs:134`, `control.rs:593` | the build refuses `record`; the server's set is a three-element subset because `x`/`y` are projection-dependent | `tessera-types` |
| vocabulary, metadata and gate validators | one each in `config.rs` and the engine | register rows 4 to 6, 10, 11 | `tessera-types::view`, `tessera-types::vocabulary`, beside `check_view_key`, which is already the model |
| placement predicates `blob_resident`, `owes_value_column`, `owes_postings` | `pipeline.rs:3255`, `filter.rs:1539` | the two predicate trees have different shapes; `postings_are_owed` answers `true` for an indexed `text` column and `owes_postings` `false`, and the build routes text away in a separate guard | `tessera-store::manifest`, beside `licence_of` |
| storage width of a column | `pipeline.rs:4145`, `compact.rs:2030`, `flush.rs:2322` | a `utf8` type panics in the build and returns `U32` in the fold | one function on `ScalarType` returning `Option`; a `None` refuses in the build and errors in the fold |
| code to value at the declared width | `input.rs:2178`, `write.rs:4934`, `control.rs:1051`, inlined at `session.rs:4815` | none seen | `ScalarType` |
| value to code | `pipeline.rs:5057`, `write.rs:4925`, inlined at `flush.rs:1650`, `values.rs:488`, `filter-write/lib.rs:163` | four dispositions of the fallthrough: `u32::MAX`, `None`, `Err`, and `Null` mapped to the absent code | one function with one documented fallthrough |
| analyser resolution from a recorded identity | `pipeline.rs:4508`, `filter.rs:1601`, `write.rs:4834`, `write.rs:4867` | one copy compared the first component of the identity only, since corrected; the comment at `pipeline.rs:4506` records that the bundle would then have refused to open for every reader | `tessera-analyse` |
| `ABSENT_CODE` | `vocabulary.rs:42`, `config.rs:183`, `filter-write/lib.rs:106` | none; `tessera-build` imports both of the first two | the store's; the filter-write copy stays, its comment giving the layering reason |
| the scalar-family list macro | `column.rs:118`, `store/write.rs:873`, `viewport.rs:293`, `payload.rs:95`, `viewer.rs:2112`, `gather_shape.rs:82`, and `wire_elem!` at `payload.rs:921` | each carries a different payload tuple and a different subset of the family; nothing checks they enumerate the same set; `wire_elem!` exists to recover the element type one of them dropped | one exported macro in `tessera-spatial` carrying `(variant, Rust type, Arrow array, Arrow type)` |
| numeric narrowing and the range and set predicates | `values.rs:516`, `viewport.rs:4422` | the entity route has the packed-container fast path and the row route does not; `HotSlice` has `Bool` and `TimestampUs` arms and `Codes` has neither; one integration test holds them together | the pure narrowing in `tessera-filter`'s public surface |
| `Overlay::touches` | one caller, `compose.rs:988`; the pair `is_deleted \|\| is_suppressed` spelled at eleven other sites across `compose`, `artifacts`, `session`, `browse`, `viewport` | each site composes the pair with different further terms in a different order | every site calls `touches`; one `layer_served_to` in `session.rs` for the three-term layer test |
| `SingleFlightCache` | `authz/single_flight.rs`, `engine/single_flight.rs` | documented: the module doc carries a rule-by-crate test table because a fix in one copy has been missed in the other | *ruling E* |
| the byte-bounded LRU | `derived_cache.rs:166`, `histogram.rs:178` | `derived_cache` evicts a batch to a low-water mark after a measured incident; `histogram` scans per victim, with a comment arguing its entry count is small | one generic cache; the argument that histogram does not need the batch pass is re-tested rather than kept |
| `adopt_all`, `adopt_indexes`, `adopt_columns` | `artifacts.rs:1970`, `:2048`, `:2119` | only the first increments the adoption counter that `session.rs:1738` logs | one generic |
| `held_at_current_version` | the generic at `write.rs:2561`; five hand-rolled copies of its closure at `write.rs:2516` to `:2545` | none | the generic, and a `LevelStamped` trait on the five extent types |
| `SegmentsManifest` literal | 24 sites of a 35-field struct, no `Default` | none | a constructor |
| `WalScalar` and `ScalarValue` | `lifecycle/wal.rs:117`, `spatial/tiler.rs:15` | none; `flush.rs:2553` transcribes one into the other variant for variant | one type, in `tessera-types` beside `ScalarType`, which moves there from `tessera-spatial` |
| the value and column enums over the family | fourteen, listed in T3's brief | `Codes` and `tessera-filter`'s `ColumnKind` have no `Bool` and no `TimestampUs` arm; two unrelated `ColumnKind`s are in scope together in `pipeline.rs` and `compact.rs` | one per storage representation; each survivor's doc names the representation that keeps it |

`build_in_memory` (`crates/tessera-build/src/lib.rs:1089`) is a second implementation of the
whole build, kept as the byte-equality oracle for `tests/build_equivalence.rs`. It is not in this
table. It must move with every change to the rows above that touch the build, and the track briefs
say so.

## 4. Per-kind skeletons inside the engine

The same shape, written once per kind of thing stored. Each keeps its per-kind content and loses
its copy of the frame.

| Skeleton | Copies | Lines | Keep per kind |
|---|---|---|---|
| fold writer: pick eligible levels, compose bytes, filter by incarnation, file | five in `write.rs:14948` to `:15686` | 467, about 225 repeated | the eligibility predicate; `write_containment_partitions` writes no incarnation and `write_tile_indexes` has two extra exclusions |
| binary pack framing: length, magic, version, reserved, total-length | six in `store/membership.rs`, plus five `*_malformed` constructors | about 420 repeated of 1,740 | the content walk; the containment walk refuses in the permissive direction on I3 and the tile-index walk against a narrow read, and merging those is a regression |
| submit a command and destructure its `Ack` | twelve in `write.rs:3613` to `:3894`, and twelve one-line delegations in `session.rs:3660` | about 460 of 550 | the command and the pattern |
| `commit_*` WAL handler: prepare, append, fsync, apply, publish, ack | five in `write.rs:13218` to `:13930`; `commit_registry` at `:13143` is already parameterised | about 180 repeated | the resolve and the apply |
| `file_shape_rows` | re-implements `file_all` (`derived.rs:777`) inline because its item type differs by three fields | 49 of 44 | nothing |
| version-stamped `Mutex<Map>` cache | `cut.rs:550`, `artifact_content.rs:183`, `shapes.rs:430` | about 110 of 150 | `ArtifactProjections::get_or_build` stays as it is: it uses a floor where the others use equality |

Under the standing rule these are one implementation. `growth_records` and
`values_growth_records` become one fold with the skip-or-refuse policy as an argument; the policy
is the entry-point rule and stays explicit at each caller. The entity-scoped and group-scoped
predicate pairs become one function over a two-field trait, with their one rule difference (a
rendered scoped category is on both surfaces and a rendered entity-scoped one is not) as a branch
in that function. `ArtifactProjections::get_or_build` joins the generic cache with its floor
comparison as a parameter. The keyword dictionary and the text index take the build's writers at
both entry points: a bucket stays in RAM when it fits and spills otherwise (`pipeline.rs:22`), so a
flush batch touches no disk it did not touch before. That is read from the module doc and not
measured; T2 measures it.

Three items stay as two copies. The five exhaustive matches over `WalRecord`: a shared dispatch
removes the compile error a new variant produces at each consumer. `build_in_memory`: it is the
byte-equality oracle for the streaming build, a second reader in the sense `CLAUDE.md` keeps for
the Python oracle; after T2 it shares the column encodings with the pipeline and checks the
spill, the merge and the row order only, and its module doc says so. The engine-to-server scenario
mirrors in `tests`: the server test sees the serialisation layer and the engine test does not. The
layer gate (one label) against the view gate (a list) is a representation difference on the
disclosure surface and is *ruling I*.

## 5. The test surface

128,641 lines in `crates/*/tests` across 183 binaries. `Cargo.toml:56` says 161.

| What | Count | Lines | Consolidation |
|---|---|---|---|
| `BuildArgs` literal, 21 fields, no `Default` | 116 sites, 107 in tests | about 3,400 | `Default` or a `for_test` constructor |
| `ViewArgs`, `LayerDeclaration`, `EngineConfig` literals, no `Default` | 102, 98, 88 sites | about 3,800 | `Default` |
| `crates/tessera-build/tests`: no `common` module across 24 binaries | 13 copies of `fn args`, 16 of `write_points`, 11 of `extent`, 7 of `source_to_entity` | about 1,700 | a `common` module under `crates/tessera-build/tests`, on the engine's model |
| the server spawn prelude | 103 sites; 8 files have a private `default_server` | about 1,400 | one function in the existing `common` |
| engine barrier helpers `fold`, `flush`, `wait_until`, `rotate` | 18, 13, 20, 4 copies | about 800 | the existing `common`, beside `tick` |
| engine `struct Fixture`, `fn fixture`, `artifacts_of` | 33, 25, 12 copies | about 560 | the existing `common` |
| `write_points` and `write_pairs` | 60 and 31 declarations | about 2,500 | one parquet writer; the per-test schemas stay |
| store fixture builders | four of 112 to 122 lines, 62 to 85% shared | about 380 | the existing `fixture`, taking a segment count |

Byte-identical copies were confirmed by `diff`: `source_to_entity` in `keyword_column.rs` and
`record_blob.rs`; `build_fixture_bundle` in `attributes_write.rs`, `values_write.rs` and
`vocabularies_write.rs`; `fn args` in `keyword_column.rs` and `filter_postings.rs`. Two helpers
have drifted between copies: `wait_until` polls with a 10 s deadline and a sleep in `rebind.rs`
and with a constant and a yield in `write.rs`; the fold helper's deadline was raised to 120 s in
`vocabularies_write.rs:337` and left at 60 s in six other files.

Sole coverage that no consolidation may remove: `tessera-types/tests/compile_fail.rs` (I4),
`tessera-engine/tests/one_publisher.rs`, `send_sync.rs`, `tessera-analyse/tests/golden.rs`,
`tessera-build/tests/build_equivalence.rs`, `tessera-corpus/tests/generator_props.rs`, and the two
proptest suites with checked-in regression files.

The engine-to-server scenario mirrors (`artifact_fill.rs`, `membership_column.rs`, `suggest.rs` in
both crates) stay. The server test sees the serialisation layer and the engine test does not.

## 6. The pass

Six tracks after one seam commit. Tracks are worktrees off the seam under
[`../../agents/parallel-work.md`](../../agents/parallel-work.md); each brief names its files, and
each implementer runs the crate's own tests and one review before reporting. The full check list
in `CLAUDE.md` runs once, on the merged result.

**Seam commit, done directly.** `Default` for `BuildArgs`, `ViewArgs`, `LayerDeclaration`,
`EngineConfig`; a constructor for `SegmentsManifest`; every open-coded overlay pair calls
`Overlay::touches`; `crates/tessera-build/src/config.rs:183` deleted; `Cargo.toml:56` corrected.
Small, and every track builds on it.

**Merging a pair.** For each pair of copies a track diffs them; lists every correction one
carries and the other lacks, with the commit that made it; keeps the copy with the better memory,
CPU and disk behaviour, stating the difference where it is a complexity class; ports each
correction from the deleted copy with its test; and moves, never drops, a test that covers a rule
the survivor's tests do not. Survivors already known: `derived_cache`'s batch eviction over
`histogram`'s scan per victim; the entity route's packed-container narrowing over the row route's
buffered one; the build's spill-capable keyword and text index writers over the flush's in-memory
ones; `stream!` over `gather!`; the analyser resolution that compares the full identity.

| Track | Does | Owns | Closes |
|---|---|---|---|
| T1 one rule at both doors | moves the reserved sets and the vocabulary, metadata, gate and content-type validators into `tessera-types`; both entry points call them; row 7's manifest field carries the width | `tessera-types/src/{view,vocabulary,layer}.rs`; `config.rs` validators; `attributes.rs`, `vocabularies.rs`, `view_declarations.rs`, `session.rs` gate check; `control.rs` reserved set | register rows 1, 2, 4 to 7, 10 to 13, and the rulings' rows as ruled |
| T2 one placement rule | moves the placement predicates, the code conversions, `record_value_of`, the value gather and analyser resolution into `tessera-store`, `tessera-filter`, `tessera-filter-write`, `tessera-analyse`; the keyword dictionary and text index writers into `tessera-filter-write`, called by the flush; row 12's keyword check into the shared admission | `pipeline.rs`, `flush.rs`, `compact.rs`, `filter.rs` predicates, `write.rs:4641` to `:4950`, `input.rs`, `lib.rs` build oracle, `filter-write/src` | §3 rows 3 to 8 |
| T3 one family table | `ScalarType` and `ScalarValue` move to `tessera-types` and `WalScalar` is deleted; the exported family macro beside them; the width function on `ScalarType`; the six downstream tables; the narrowing shared; the fourteen enums reduced to one per storage representation | `tessera-types/src/scalar.rs` (new), `tiler.rs`, `lifecycle/wal.rs`, `column.rs`, `store/write.rs`, `store/read.rs`, `viewport.rs`, `payload.rs`, `viewer.rs`, `values.rs`, `gather_shape.rs` | §3 rows 4, 9, 10, 17, 18 |
| T4 one skeleton per shape | the six §4 collapses, the generic adopt, the generic cache with `ArtifactProjections` in it, `held_at_current_version` used, the growth fold and the scoped-entity predicate trait | `write.rs` outside T2's range, `artifacts.rs`, `store/membership.rs`, `derived.rs`, `derived_cache.rs`, `histogram.rs`, `cut.rs`, `artifact_content.rs`, `shapes.rs`, `filter.rs` predicate pairs | §3 rows 13 to 15, §4 |
| T5 the test surface | §5 in the order given | `crates/*/tests` only | §5 |
| T6 documents and comments | the capability map's disagreement table; `flush.rs:1584`; `lib.rs:1812`'s "one derivation, shared by both build implementations"; every "transcribed" comment the other tracks make false; `system-architecture.md` §3's crate table for the types T3 moves | `docs/design`, the named comments | the capability map's table |

The order the tracks run in, and their status, are in
[`../../consistency-pass.md`](../../consistency-pass.md), the pass's tracker. T3 follows T2
because both would otherwise edit `flush.rs` and `pipeline.rs`; T4 follows both because
`write.rs`, `filter.rs` and `viewport.rs` cannot carry two tracks at once.

**The guard.** Not built yet: a test that takes each statement in §2 and drives it at a build and
at the control route, asserting one answer. Until it exists the register is checked by reading.
T1 writes it, one case per row, in `crates/tessera-server/tests`, which already spawns a server
over a built bundle. Decision 0091 asks for the same shape at corpus scale in the conformance
suite; that is a later track and is not this one.

What the pass removes, estimated from the counts above and not yet measured: 4,000 to 5,000 lines
of `src`, 9,000 to 10,000 lines of `tests`, and between forty and sixty test binaries once the
engine's flush cluster and the build's declaration-rule binaries share a fixture.

## 7. Rulings

- **A.** Register row 3, `type = "u16", vocabulary = "v"`. (a) Accept at both entry points: the
  manifest already spells a category as a width and a vocabulary. (b) Refuse at both: `category`
  is the one spelling, and nothing in the tree uses the other on the control route, so (b) removes
  the engine's rewrite path and a spelling with no user. Recommended: (b).
- **B.** Register row 8, an integer where a float is declared. (a) Accept and widen at both: TOML
  and JSON both spell `0` as an integer, and the stored value is a float either way. (b) Refuse at
  both. Recommended: (a).
- **C.** Register row 9, category-typed view metadata. Ruled (b) in decision 0140: the create
  route takes the key as the build takes it (`config.rs:3984`), the handler resolving
  (`control.rs:990`) and the write executor minting. It is a capability and is built under the
  capability-gaps memo; the pass leaves the row open and marked.
- **D.** Register row 13, the supplied content type set. Ruled (a) in decision 0140: the six
  words `text`, `polygon`, `extent`, `point`, `circle`, `ellipse`, checked in
  `LayerDeclaration::validate` and nowhere else. The pass does the check; whether an authored
  circle or ellipse is drawn is the capability-gaps memo's.
- **E.** `SingleFlightCache`. (a) A leaf crate `tessera-cache` under `tessera-authz` and
  `tessera-engine`; `check-layers.sh` denies edges, and this adds none it names. (b) Keep the
  twins and their test table. The standing rule settles this as (a) unless ruled otherwise.
- **F.** `crates/tessera-bench` (19,044 lines, 21 binaries) and `crates/tessera-engine/examples`
  (2,381 lines, 8 files) are compiled by `clippy --all-targets` and duplicate each other's sweeps.
  (a) Leave both where they are. (b) Fold the examples into `tessera-bench` as bins, one
  measurement crate with one fixture ledger. Moving them to `probes/` is not offered: four were
  used on 2026-09-04 and 2026-09-09, and a probe crate path-depends on the workspace and stops
  compiling when an API moves. Recommended: (b).
- **G.** Test support. (a) A `tests/common` module per crate, as the engine and server already
  have; the parquet writer, the `BuildArgs` constructor and the bundle assembler are then copied
  into four modules. (b) A `tessera-testkit` crate as a dev-dependency. `check-layers.sh` runs
  `cargo tree -e normal`, which does not see a dev edge, so the `tessera-corpus` check gains
  `-e normal,dev` to keep the generator off it (correctness-suite §13). Recommended: (b), the
  standing rule applied to the tests.
- **H.** Tracking. Local, by owner direction (2026-09-11). (a) A committed file under `docs/`,
  the sole authority for the pass's status, on the model the artifact work used, and named as an
  owner-directed exception in
  [`../../agents/epic-lifecycle.md`](../../agents/epic-lifecycle.md). (b) The gitignored track
  ledgers only, with this memo as the plan. Recommended: (a).
- **I.** The layer gate is one label (`LayerDeclaration::visibility`, an `Option<String>`) and the
  view gate a list (`Option<Vec<String>>`, any one satisfying), and the two resolve a label
  differently: the layer path looks the label up in the dictionary as a literal term
  (`session.rs:4211`, `registry.rs:2695`) and the view path puts it through the plugin's
  `terms_of_labels` (`gate.rs:182`), which architecture §3 names as the one derivation, run at
  both entry points. Under `builtin:passthrough` the two agree; under a plugin that derives
  descriptors they do not, and the direction depends on the plugin. (a) One representation, a
  list, a layer's label being a one-element list, and one resolver, the plugin's, called by
  `visible_layers`, the artifact gate and the view gate alike: a format change under decision
  0048, and the layer gate then honours the plugin as the view gate does. (b) Keep both and state
  the asymmetry. Recommended: (a); the design already says which resolver is right.

## Appendix A. The inventory

Every duplication the five surveys found, grouped by the track that takes it. An item marked †
was confirmed at both sites for this memo; the rest are as the surveys reported them, with their
citations, and the track confirms an item before it merges it. "Home" is where the one
implementation lives afterwards; "survivor" names the copy that is kept where the copies differ.

### T1a. Validators, reserved sets and admission

| Item | Sites | Home or survivor | Note |
|---|---|---|---|
| † reserved column-name sets | `config.rs:4989`, `attributes.rs:134`, `control.rs:593` | `tessera-types` | register row 1; the server subtracts `x`/`y` explicitly |
| attribute compiler: type, vocabulary, analyser and flag rules | `config.rs:4568` `compile_attributes` (~320 lines), `attributes.rs:355` `compile` (~172) | `tessera-types`, over `ScalarType`, an analyser lookup and a vocabulary width lookup; the build keeps its TOML-only keys (`field`, `multi`, `render_in`, `fields`, `title`) at the parse | register rows 1 to 3; the `render` refusals stay engine-side under decision 0136 |
| † vocabulary name, value key and `reserved` rules | `config.rs:4217` `compile_vocabularies`, `:4406` `compile_reserved`, `:4424` `parse_inline_values`; `vocabularies.rs:121` `resolve`, `:340` `check_name`, `:359` `check_value_key` | `tessera-types::vocabulary` | register rows 4 to 6; code assignment stays split: the build pins codes, the executor mints |
| `is_category_width` and `usable_max` restated | `vocabularies.rs:323`, `:329` against `tiler.rs:236`, `vocabulary.rs:765`; `ScalarType::max_code` at `tiler.rs:242` is a third spelling returning `Option` | the `ScalarType` method and one `usable_max` | make the originals `pub` |
| † view gate check | `config.rs:3273` `compile_view_gate` and `:3211` `check_label`; `session.rs:3838` `check_gate_labels`; `view_declarations.rs:291` `check_gate`; `gate_of` at `view_declarations.rs:285` and `roster.rs:248` | one `check_gate_labels(labels, plugin)` in `tessera-types::view` beside `check_view_key`; the build passes `Passthrough::new()` as it does at `config.rs:3306` | register row 10; `const INHERITED` at `config.rs:189` against the literal at `session.rs:3795` |
| † metadata name check | `config.rs:3731` `check_metadata_name`; `view_declarations.rs:316` `check_metadata`; `ROSTER_KEYS` at `config.rs:3675` and `ROSTER_NAMES` at `view_declarations.rs:314` | `tessera-types::view`, over `&[GroupMetadataField]` | register row 11 |
| projection parse | `config.rs:3057` `compile_projection`, `view_declarations.rs:239` `parse_projection` | one function; the build's message lists the closed set, keep that message | ten lines |
| view and group name uniqueness | `config.rs:3364`, `view_declarations.rs:220` `check_name_free` | one function over the two name sets | consistent answers today |
| frame bounds | `view_declarations.rs:254` `compile_frame` re-implements `Bounds::validate` (`morton.rs:22`) | call `Bounds::validate` and wrap its message | the build's extent fitting (`config.rs:1726` to `:1977`) is acquisition and stays |
| † keyword admission: the empty string | `pipeline.rs:3737` refuses; `control.rs:961` `scalar_at` decodes `Keyword` with no check; `flush.rs:1626` makes it a key | one admission function, records §7, called by the build's `for_each_keyword` and the handler | register row 12; the comment at `flush.rs:1584` is corrected |
| † supplied content type set | `config.rs:5909` accepts four; `layer.rs:396` `authored_shape_kind` knows three shape words; `LayerDeclaration::validate` checks none | `LayerDeclaration::validate`, ruling D | register row 13 |
| the both-doors test | none | `crates/tessera-server/tests`, one case per register row | the guard |

### T1b. Declared shapes

| Item | Sites | Home or survivor | Note |
|---|---|---|---|
| † metadata value typing | `config.rs:3963` `compile_metadata_value`, `:4056` `integer_range`; `view.rs:109` `admits`; `control.rs:3866` `metadata_value` | `GroupMetadataField.ty` carries `ScalarType`; `admits` range-checks; one function | register rows 7 and 8; ruling B; manifest version bump. Row 9 is the capability-gaps memo's |
| † the layer gate: one label, a literal lookup | `layer.rs:663` `visibility: Option<String>`; `registry.rs:2695` resolves through `resolve_label`, wired to `generation.dict.lookup` at `session.rs:4211`; the view path at `gate.rs:170` through the plugin | a list, resolved by `gate.rs::passes`, for `visible_layers`, `ArtifactGate` and views | ruling I; manifest version bump |
| `resolve_group` and `resolve_plain` | `view_declarations.rs:93`, `:166`: a six-line prologue and a fourteen-line held-same-differing epilogue each | one prologue and epilogue; the `members` rules stay group-only | ~45 lines |
| `with_vocabularies`, `with_groups`, `with_plain_views` | `manifest.rs:1138`, `:1161`, `:1177`: clone, skip if held, push | one `push_new_by` | ~25 lines |
| `declared_of_scoped` and `scoped_as_declared` | `session.rs:4726`, `control.rs:1935` | one adapter | |

### T2. Placement, encoding and the writers

| Item | Sites | Home or survivor | Note |
|---|---|---|---|
| placement predicates | `pipeline.rs:3255` `blob_resident`, `:3273` `value_column_is_owed`, `:5038` `postings_are_owed`; `filter.rs:1539` `owes_value_column`, `:1570` `blob_resident`, `:1586` `owes_postings` | `tessera-store::manifest`, beside `licence_of` | the two trees differ in shape; `text` is routed by a separate guard in the build; the duplicated four-paragraph doc at `pipeline.rs:3236` and `filter.rs:1556` becomes one |
| `scalar_schema_of` | `write.rs:4641`, `build/lib.rs:1815` | one derivation over the render columns | a width disagreement here puts every row's values under the wrong heading |
| † storage width of a column | `pipeline.rs:4145` `column_kind`, `compact.rs:2030` `column_kind_of`, `flush.rs:2322` `empty_codes` | a method on `ScalarType` returning `Option`; the build refuses on `None`, the fold errors | the build panics on `Utf8` today and the fold returns `U32`; the `category` argument is derived three ways (`vocabulary.is_some()`, `job.postings`, `spec.category`) |
| code to value at the declared width | `input.rs:2178` `code_as`, `write.rs:4934` `code_at_declared_width`, `control.rs:1051` `code_at`, `session.rs:4815` inline, `attributes.rs:576` `absent_scalar` | `ScalarType::code_at` | |
| value to code | `pipeline.rs:5057` `category_code`, `write.rs:4925` `scalar_code`, `flush.rs:1650` inline, `values.rs:488` `Codes::at`, `filter-write/lib.rs:163` `code_at` | one function, one fallthrough | four fallthroughs today: `u32::MAX`, `None`, `Err`, and `Null` to the absent code |
| analyser resolution | `pipeline.rs:4508`, `filter.rs:1601` `resolve_analyser`, `write.rs:4834` `text_schema_of`, `write.rs:4867` `analyser_of`, `attributes.rs:436`, `config.rs:4755` | `tessera-analyse::analyser_for_identity` | `text_schema_of` and `analyser_of` are one body over two record types |
| `Family::of` and `Family::of_scoped` | `filter.rs:224`, `:241`: identical ten-line bodies over `.vocabulary` and `.arrow_type` | one, over a two-field trait | |
| `record_value_of` | `pipeline.rs:3446`, `flush.rs:2493` | beside `RecordValue` in `tessera-filter-write` | the flush copy has no category arm and drops no absent code |
| per-column value packing | `pipeline.rs:3563` `write_column_values`, `:4191` `push_numeric_chunks` with `stream!`, `:3660` `Presence`; `flush.rs:1593` `extent_values` with `gather!` | `tessera-filter`, beside `Codes` and `ValueColumnWriter`; `stream!` survives | the build writes a presence file only when an absence proves one is owed, the flush always; state the base-versus-extent reason once |
| keyword dictionary writer | `pipeline.rs:3718` to `:4128` (chunk, spill, merge, scatter); `flush.rs:1610` (in-memory sort) | `tessera-filter-write`; the build's survives, a bucket in RAM when it fits | measure that a flush batch touches no disk |
| text index writer | `pipeline.rs:4487` to `:4960`; `flush.rs:2006` `write_text_layer` | `tessera-filter-write`; the build's survives | the emit is already shared |
| minter seeding | `config.rs:2380` `open_minters`; `vocabulary.rs:283` `seed_manifest`, `:562` `Vocabularies::seed`; `write.rs:2671` the replay loop | `Vocabularies::seed` | the width bounding a draw comes from the vocabulary in the build and from the column in the store, with a `U32` fallback the build lacks; the build skips non-open vocabularies |
| vocabulary value emission | `vocabulary.rs:787` `values_of` (no titles), `engine/vocabularies.rs:95` `values_with_titles`, `build/lib.rs:1998` inline from `Schema::titles`, `suggest.rs:877` | the minter holds `(key, code, title)` once; the build seeds titles into it | |
| render-presence bitmap loop | `store/flush.rs:185`, `build/lib.rs:1587`, `pipeline.rs:5199` `render_presence_of`, `merge.rs:390` | `tessera-store::render_presence`, beside `from_present_rows`; `write_render_presence` takes the predicate | the `any_absent` short-circuit repeats what `from_present_rows` already decides |
| `parse_ingest_batch` and `parse_values_batch` | `control.rs:1123`, `:1665`: the type probe twice per function, the five-line decode match four times | one column decoder in `control.rs` | same crate, same types |
| group-scoped family ownership | `write.rs:4729` `scoped_owner_view_of`, `viewport.rs:1376` `owning_key_of`, `pipeline.rs:2758` `scoped_render_targets` | `tessera-store::manifest`, as `licence_of` was | `pipeline.rs:2751` states the twin and the reason it was not shared |
| coalesced text presence written without `run_optimize` | `filter-write/text.rs:123`, against `values_writer.rs:124` `presence_bytes` | `presence_bytes`, or a `portable_bytes` in `tessera-roaring` | the one gap; every render-presence producer already goes through `write_render_presence` |

### T3. The type family

| Item | Sites | Home or survivor | Note |
|---|---|---|---|
| `ScalarType` and `ScalarValue` | `tiler.rs:118`, `:15` | `tessera-types` | `tessera-lifecycle` then reaches them without a new edge |
| † `WalScalar` | `wal.rs:117`; `flush.rs:2553` `to_scalar_value` is variant for variant | deleted; `ScalarValue` | WAL format change |
| † the family list macro | `column.rs:118`, `store/write.rs:873`, `viewport.rs:293`, `payload.rs:95`, `viewer.rs:2112`, `gather_shape.rs:82`, `wire_elem!` at `payload.rs:921` | one exported macro carrying `(variant, Rust type, Arrow array, Arrow type)` | `wire_elem!` deleted; four docstrings re-derive one justification |
| the value and column enums | `Codes` `values.rs:455`, `ColumnKind` `values_writer.rs:56`, `ColumnKind` `store/write.rs:367`, `ScalarColumnValues` `:849`, `ScalarSlice` `read.rs:1381`, `ScalarOut` `viewport.rs:81`, `ColumnBuf` `:274`, `HotSlice` `:4062`, `ScalarColumn` `payload.rs:67`, `ColumnData` `column.rs:189`, `RecordValue` `record.rs:154` | one per storage representation, named in its doc; `BatchValues` (`input.rs:1873`) and `ViewMetadataValue` are the prior art for collapsing | `Codes` and `tessera-filter`'s `ColumnKind` lack `Bool` and `TimestampUs`; two `ColumnKind`s share a name |
| Arrow downcast and type tables | `control.rs:960` `scalar_at`, `ingest_json.rs:422` `scalar_column`, `read.rs:1569` `flat!`, `values.rs:1329` `borrow!`, `store/write.rs:900`, `payload.rs:115`; `arrow_type_of` `store/write.rs:706`, `ColumnKind::of` `:389`, `ColumnKind::arrow_type` `values_writer.rs:87`, the accept list at `read.rs:1684` | generated from the family macro | `control.rs:944` records six types that were declarable and un-ingestable through this scatter; `input.rs:2243` `read_integer` and `:2014` `decode_values` stay, being a widening |
| † numeric narrowing and the range and set predicates | `values.rs:516` to `:602` and `:872` to `:991`; `viewport.rs:4422` to `:4509` and `:4233` to `:4376` | the narrowing public in `tessera-filter`; the entity route's packed-container walk survives | `HotSlice` has `Bool` and `TimestampUs` arms and `Codes` has neither, so `bool_matching` (`viewport.rs:4403`) has no entity twin; `float_range!` carries an unused parameter in one copy |
| `stored_as_wal` and `stored_field_out` | `session.rs:4800`, `viewport.rs:2342`: the same sixteen arms | one adapter over a constructor | same crate |
| `encode_value` and `decode_value` | `record.rs:247` hand-written, `:326` generated; `KIND_*` at `:184` | one `record_kinds!` table driving both and the constants | a wire format kept in two lists |
| `RunIter` and `for_each_slot_run` | `values.rs:384`, `:766`; `filter.rs:3852`, `:3815` | `tessera-roaring`, beside `Sink` | the engine copy is not counted by `take_scan_work` |
| byte width of a type | `tiler.rs:220` `row_bits`, `pipeline.rs:333` `staged_width`, `residency.rs:361` `fixed_width`, `values_writer.rs:105` `ColumnKind::width` | one `fixed_width_bytes` on `ScalarType`; each caller keeps its string and bool policy | `staged_width` prices strings at a `String` header the design says they no longer cost |
| category-width mapping outside T2's files | `viewport.rs:2215`, `:2354`; `session.rs:4807` | `ScalarType::code_at` | |

### T4. Per-kind skeletons in the engine and the store

| Item | Sites | Home or survivor | Note |
|---|---|---|---|
| fold writers | `write.rs:14948` `write_containment_partitions`, `:15308` `write_row_columns`, `:15449` `write_shape_rows`, `:15528` `write_shape_held`, `:15592` `write_tile_indexes` | one `file_derived_kind` taking the eligibility and compose closures | containment writes no incarnation; tile indexes carry two extra exclusions |
| pack framing | `membership.rs:248`, `:504`, `:852`, `:1221`, `:1379`, `:1681`; `malformed` at `:81`, `:411`, `:795`, `:1037`, `:1604`; `map_read_only` at `:1812` used by three of five `open`s | one `frame_header` and one `malformed` | the content walks stay per format |
| submit and destructure the `Ack` | `write.rs:3613` to `:3894`, twelve methods; `session.rs:3660` to `:3930`, twelve delegations | one `submit_expecting` | |
| `commit_*` handlers | `write.rs:13218`, `:13503`, `:13656`, `:13715`, `:13815`; `commit_registry` at `:13143` is parameterised | `commit_registry`'s shape for all five; `Resolution<T>` (`view_declarations.rs:86`) for the three `Resolution` enums | |
| `file_shape_rows` | `derived.rs:929` re-implements `file_all` (`:777`) | `file_all`, generic over an item trait | hard-codes `"tssr"` where `file_all` takes a closure |
| version-stamped caches | `cut.rs:550`, `artifact_content.rs:183`, `shapes.rs:430`; `artifacts.rs:2868` `ArtifactProjections::get_or_build` with a floor comparison | one `VersionedCache` with the comparison as a parameter | |
| † the byte-bounded LRU | `derived_cache.rs:166`, `histogram.rs:178` | one; the batch eviction survives | the argument that histogram's entry count is small is re-tested |
| `adopt_all`, `adopt_indexes`, `adopt_columns` | `artifacts.rs:1970`, `:2048`, `:2119` | one generic | only the first counts adoptions; `session.rs:1738` logs the count for one kind |
| † `held_at_current_version` | the generic at `write.rs:2561`; five hand-rolled copies at `:2516` to `:2545`; five identical closures at `:8477` | the generic over a `LevelStamped` trait | |
| the derived-extent types and their seeding | `manifest.rs:1572` to `:1734`, five types with a verbatim `incarnation` paragraph; `write.rs:3278` and `session.rs:1361`, seven seeding blocks each | `LevelStamped`; one `union_across_partitions` | |
| `growth_records` and `values_growth_records` | `write.rs:5759`, `:10985` | one fold; the skip-or-refuse policy an argument | |
| entity-scoped and group-scoped predicates | `filter.rs:708` and `:792`, `:1519` and `:954`, `:1539` and `:785`, `:1586` and `:811` | one function per question over a two-field trait; the one rule difference a branch | after T2 has moved the entity-scoped side down |

### T5. The test surface

| Item | Sites | Consolidation |
|---|---|---|
| † `BuildArgs`, `ViewArgs`, `LayerDeclaration`, `EngineConfig`, `SegmentsManifest` literals | 116, 102, 98, 88, 24 sites | `Default` from the seam; `..Default::default()` at each site |
| † `crates/tessera-build/tests` helpers | 13 `fn args`, 16 `write_points`, 11 `extent`, 11 `write_empty_pairs`, 7 `source_to_entity`, 9 `current_prefix`, 5 `test_key`, 3 `assert_bundles_identical`, 4 `collect` | `tessera-testkit` |
| † the server spawn prelude | 103 sites; private `default_server`s in `layers.rs:19`, `viewport_membership.rs:27`, `categories.rs:227`, `suggest.rs:176`, `artifact_fill.rs:39`, `artifact_exclusion.rs:42`, `grow_memberships.rs:45`, `artifact_views.rs:155` | one in `common` |
| the write-route scaffold | `attributes_write.rs`, `vocabularies_write.rs`, `values_write.rs`, `view_declarations_write.rs`: `build_fixture_bundle` ×3 identical, `struct Served` ×10, `flush` ×7, `fold` ×5, `restart` ×9 | `common`; the four binaries stay |
| † engine barrier helpers | `fold` ×18, `flush` ×13, `wait_until` ×20, `remove_the_whole_log` ×6, `rotate` ×4 | `common`, beside `tick`; the `wait_until` and fold-deadline drift resolved to one |
| † `struct Fixture`, `fn fixture`, `artifacts_of` | 33, 25, 12 | `common` |
| `engine_at` and `engine_over` | 10 and 7 sites | `open_engine_with` in `common` |
| `fn declaration() -> LayerDeclaration` | 17 sites | `LayerDeclaration::flat(name)` in the testkit; the per-file comments on why a criterion is set move with the override |
| `write_points` and `write_pairs` | 60 and 31 declarations; the engine's and server's `common` copies differ by two lines | one parquet writer in the testkit; the per-test schemas stay |
| store fixture builders | `fixture/mod.rs:69`, `bundle_read.rs:66`, `cell_histogram.rs:67`, `manifest_write.rs:66`; `synthetic_tessera_id` ×5, `hex_sha256` ×5, `file_digest` ×4 | the leaf helpers in the testkit; one builder taking a segment count and returning its items |
| `SCHEMA_TOML` constants | 18, and ~45 named raw-string configs | the small ones shared; the large ones stay inline as the test's subject |
| binaries that share one fixture | the engine's flush cluster (ten binaries, 3,204 lines); the build's declaration-rule binaries (six under 300 lines) | three or four binaries each |
| duplicate test names across binaries | `the_route_declares_…` in two server files; `a_public_column_is_suggested_as_authored`, `revoke_prunes_the_token`, `the_two_layouts_answer_identically` in two crates each | renamed; `check-test-reachability.sh` reports fewer false positives |

### E and F

| Item | Sites | Home or survivor |
|---|---|---|
| † `SingleFlightCache` | `authz/single_flight.rs` (1,146 lines), `engine/single_flight.rs` (2,181) | `tessera-cache`; the fallible form survives, `get_or_derive_waiting`, `retain_keys`, `peek`, `ready_entries` move in, one test suite from the module doc's table |
| the engine examples | `calibration_sweep`, `min_len_sweep`, `tile_axis_sweep`, `stage_attribution`, `decode_tiers`, `route_saving`, `open_rss`, `refresh_probe` (2,381 lines); `underlay_route.rs` and `viewport_sweep.rs` in bench share 41 ten-line windows | `tessera-bench` binaries over one sweep scaffold |
| `gather_shape.rs` copies of `ScalarOut`, `ColumnBuf`, `row_to_point`, `build_scalar_columns` | `gather_shape.rs:44` to `:260` | stays: an A/B benchmark freezes its arm; it takes the family macro from T3 |
