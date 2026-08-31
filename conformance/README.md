# `conformance/` — the invariant conformance suite

Pytest, same venv as `reference/` (`reference/.venv`, Python 3.12). Drives the release `tessera`
binary as a real subprocess — never calls into `tessera-engine` directly — using the same
server-spawning harness `reference/tests` uses (`reference/oracle/harness.py`, extracted from
`reference/tests/conftest.py` in this task so neither suite copy-pastes the other's spawn logic).

Run it:

```
reference/.venv/bin/pytest conformance/tests -v
reference/.venv/bin/pytest conformance/suite -v
```

`conformance/suite/` is the correctness suite's shared foundation (`docs/design/correctness-suite.md`
§3, §12.2, build-order row 1): the read battery's `Query`/`Recorded`/`Canonical` types, the one
response-canonicalisation implementation, and a recorder that issues a battery against a live
server. The canary comparator below is refactored onto it — there is deliberately no second copy.
`/v1/region` rides the battery as an explicitly marked absence (it is not in the router), pinned by
a test that fails the day the route lands. ⊘ The driver, stages and entitlements of §12.3 (rows 2–3)
are not built; what exists is the battery, the canonicalisation, and the tests that pin them —
including the property everything downstream rests on, that recording one battery twice against
unchanged state compares equal.

CI runs it on every pull request and every push to `main`, alongside the rest of CLAUDE.md's gate
(`.github/workflows/ci.yml`). That is conformance §6's per-PR tier; the nightly and release tiers
it also specifies do not exist.

**673 pass and 4 skip, over 677 cases (2026-08-31)** — `conformance/tests` and
`conformance/suite` together, the split CI's own case-count step compares against `conformance.md`
§0's marker line. The 39 that arrived on that date are the multi-view differential.

They had not passed at all, for the weeks between the configuration rework and 2026-08-20: `tessera serve` took `--deployment` in place of `-c` and `harness.spawn_server` still
passed the old spelling, so every server-backed module died at startup and nothing noticed. With
that fixed, 27 failed for **two causes, both the mask catalogue's own assumptions and neither a
defect** — `public` interned at term `0`, which makes a block's dictionary id one higher than the
corpus's own, and decision 0073's Morton tiebreak, which broke the designed
`entity_id == source_id` identity. Both are fixed; `oracle/catalogue.py`'s header carries the
argument and `conformance.md` §0 the account.

The one to carry: **`verify()`'s block check compares posting *sets***, and a within-block
permutation preserves a set — so the check whose comment said it re-derived the identity had never
tested it. `verify()` now compares the two spaces item by item (check 3b), and every planted column
is keyed by entity through `Bundle.source_of_entity` rather than by source id under an equality.

`conformance/suite` is the correctness suite's shared battery rather than a row of the invariant
matrix. It was excluded from the 2026-08-20 count on a `tomllib`/Python 3.11 caveat that no longer
holds — the venv is 3.12 — and both directories are counted together above.


(`reference/.venv/bin/pytest reference/tests -v` must also stay green. It is **not** run in CI:
two of its five modules build from the Phase 0 corpus, which is not in the repository, and
repointing them would cost the viewport differential its realistic term distribution. Run it
locally on a machine that has the corpus.)

Lint, if you have `ruff`, from the repository root — the configuration lives with the package it
governs and covers both suites:

```
uvx ruff check --config reference/pyproject.toml reference conformance
```

The selected rule set is small and passes at zero; `reference/pyproject.toml` says which rules are
deliberately *not* selected and why.

## What's here

| File | Invariant | What it proves |
|---|---|---|
| `tests/test_byte_scan.py` | I10 | No encoding of any entity id — admitted or denied — appears in a viewport response's points batch, its sub-cell stream, a `/v1/items` body, or the server's `RUST_LOG=info` log, across a zoom range; nor does the deployment identity key, nor a caller external id outside the one designed exception. See the module doc for the scan widths, the `SAFE_ID_FLOOR` reasoning, and why the corpus moved. Carries **two controls**: a real transmitted `tessera_id` the byte-window mechanism must recover, and a **planted** entity id every sweep mechanism must flag — the second catches a scanner that works on real traffic but ignores the value class I10 is about. |
| `tests/test_restart_replay.py` | WAL/deny survival; durability ordering (conformance §5) | Two tests. An ingest, two suppressions and a delete survive a `SIGKILL` and a restart on the same WAL/cache/bundle with nothing re-submitted. And, separately, they survive a **truncation of the WAL to its last-synced offset** — what a power loss would have done — which is the variant that falsifies an engine acking before it fsyncs; a SIGKILL alone loses nothing, because the page cache outlives the process. The offset comes from the WAL's own `.sync` sidecar, so no introspection command was needed. |
| `tests/test_canary.py` | I2 | Three synthetic states (`reference/oracle/canary_fixture.py`) built under one identity key, differing in one item at the extreme corner of the extent: the base corpus; the base plus an item carrying a term no tested grant set holds; the base plus an item carrying a term they do. One comparator runs over all of them, comparing **canonicalised bytes** via the shared canonicalisation (`conformance/suite/`, correctness-suite §12.2) on four separately-addressable surfaces — the tile batch sorted by tile id, the points batch as served (contracts §3.2 orders it by `tessera_id` within each tile), the §3.3 underlay's masked per-cell counts, and the trailer minus its elapsed-time fields. The canary state must agree on all four; the visible state must disagree on all four, which is §4.4's positive control and what stops a canonicalisation that silently dropped a surface from reading green. Also checks the **five allocation rules** for both extra-item states, and that nothing session-dependent remains in the response body. |
| `tests/test_mask_catalogue.py` | fixture integrity | The adversarial mask catalogue (`reference/oracle/catalogue.py`) is the shape it claims: eight cases, each named for the property it attacks, each reaching exactly its declared entity set by two independent routes (postings union, pairs semi-join). Includes the container-boundary and ~5%-crossover claims, which are the two that stop being true silently. Carries a **strict xfail** for `fx_key` in the points batch — see "Known limitations". |
| `tests/test_i7_selection.py` | I7 | §7.2's served set, engine against the literal definition in `reference/oracle/viewport.py`, over the catalogue × depths `{0,2,4,6}` × `k` `{2,30,500}`, against both a θ-saturated and a θ-live server, compared as **ordered lists** (contracts §2.6 makes the order contract). Plus cross-zoom nesting into *the child that contains the point*; the **cap**, against a server with `k_max_marks = 128` so `cap = min(k, K_max)` actually binds; and **the negative control**: a first-`k` stub that serves §7.2's count in storage order instead of `tessera_id` order, with the assertion that the differential disagrees with it on most tiles. A differential that passes against a deliberately wrong implementation is testing nothing. |
| `tests/test_filter_differential.py` | I12 (mask half), C11 | The attribute-filter differential: engine against `reference/oracle/filters.py` — a per-entity walk over the **fixture's planted values**, never the `attrs/` artefact — across the mask catalogue's principals. Per tile, a filter moves `matched` and never `visible`; the filtered served set equals the oracle's brute-force `M_sel` exactly (θ saturated, so a filter that *dropped* visible matching items is caught, not just one that widened); a hidden, a hollow and a nonexistent category value are byte-identical in outcome, with a single-member `solo` value as the positive control; `all_of`/`any_of` nest and obey their empty identities; an unknown column is a `422` and an unknown value is not. The fixture's decorrelation of attribute from grant is itself asserted, because a correlated fixture passes every cross-principal check vacuously. Carries a **strict xfail**: a cross-family operator (`prefix` on a category) answers as an empty operand where the contract reading says `422` — see the test's docstring. The frontier half of I12, I3 and Rule S over filter counts stay uncovered; the module doc says why each. |
| `tests/test_keyword_differential.py` | records §4.3, §10 (the keyword family, one layer) | The keyword differential over the base build: `eq`, `in`, `prefix` and `contains` on `submitter`, engine against `oracle.filters.KeywordColumn` — which holds the strings the fixture planted and **no dictionary and no ordinals**, so agreement is agreement between two constructions rather than a transcription. Run as a matrix over principals chosen so that both `contains` routes are exercised (the engine picks the narrow probe below ~15% of the layer's key count and the broad dictionary walk at or above it). Owns two of the family's catalogue entries: the **ordinal boundaries** — the dictionary's first and last value, each carried by exactly one entity — and the **sentinel**, a needle no dictionary holds, which must answer and answer empty across all four operators with a positive control beside it. Also pins `/v1/meta`: `submitter` publishes family `keyword` with exactly the four string operators, **`range` is not among them**, and a `range` leaf on the column is a `422`. |
| `tests/test_keyword_layers.py` | records §4.3, §7, §10 (the keyword family, layered) | What more than one layer adds. The corpus is driven over the control plane to three states on a private bundle copy — base + one flush extent, base + two flush extents, and folded via `POST /control/compact` — and all seventeen probes are checked against the oracle at each. The batches are chosen so each layer's dictionary disagrees with the others in the way the entries name: the base's ordinal 0 is a value neither extent holds, each extent's own ordinal 0 is a value the base has never held, and one value sits in the base and an extent at two different ordinals — so a single resolve reused across layers returns *the wrong entities*, not none. Owns the remaining reachable entries: **a value present in one layer's dictionary and absent from another's**, and **a prefix range empty in one layer and non-empty in the next**. Plus folded-against-layered: the fold rebuilds one dictionary from the survivors and renumbers every ordinal, and no answer may move. |
| `tests/test_text_differential.py` | records §4.4, §4.5, §10 (the text family) | The text differential over the base build: `match`, its m-of-n form and `phrase` on `abstract`, engine against `oracle.text.TextColumn` — which holds the **prose the fixture planted** and no dictionary, no posting and no ordinal. **Both sides tokenise through `tessera tokenise`** (decision 0070's own reason for that verb): reimplementing UAX #29 in Python would compare PyICU's ICU4C against the engine's icu4x and make every marginal disagreement a research question, so what is differential here is the set-and-sequence arithmetic over one token stream. Twenty expressions × five principals, chosen for the shapes the family's catalogue entries name — a word one entity carries, a word none does, both non-Latin scripts under the real segmenter, m-of-n at every m including the unsatisfiable one, and the adjacency pair planted **both ways** so a `phrase` that returned its own conjunction fails. Also: the phrase is a strict subset of its conjunction (engine-to-engine, so it survives an oracle bug); hidden and absent are identical in **every frame but the timing trailer**, which is exactly what C25 accepts as observable; `/v1/meta` publishes `text` with `match` and `phrase` and none of the four string predicates, and a negation over a text column is a `422`; and the **suppression row on both text routes** — `match` reads the postings, `phrase` reads the postings *and* decompresses the blob, which is the only filter route in this system that reads a stored value at query time. The analyser's **golden vectors run here too**, through the CLI, so they are a conformance obligation rather than one crate's unit test. ⊘ Field-scoped semantics over multi values is not reachable: `multi = true` is refused at the schema (#87). |
| `tests/test_schema_refusals.py` | records §2 (the refusal list) | Each schema records §2 refuses fails a **real `tessera build`** naming its reason per decision 0013 — `multi = true` names records §5; `render`+`multi` names decision 0039's permanent fence (and wins over the bare-`multi` refusal); `index` on a rendered number names decision 0064; `record` is a reserved name (review N10); a stale `used_for` key refuses loudly. The positive control: a neither-key declaration builds green and writes `attrs/record/*` — blob-resident, not tolerated. |
| `tests/test_record_blob.py` | records §3/§10 (the blob's addressing) | The one artefact-level check the design licenses (review B7): the record blob's addressing self-consistency — blocks tile `blocks.bin`, ranks tile the rank space, rows tile their blocks, discriminants agree with has-row's rank order, fields frame exactly, tags are blob-resident columns only — walked by `oracle.record_blob` (structure only, never values; its module doc holds the licence). Plus has-row against the generation functions' presence, and the oversized-row rule on the planted > 256 KiB note. Value equality is **deliberately elsewhere**: at build level in Rust (`crates/tessera-build/tests/record_blob.rs`), and at the served surface when drill-down lands (`oracle.catalogue.record_of` is the waiting expectation). |
| `tests/test_label_containment.py` | I3 | **Containment, both halves of conformance §4.4's row.** Over `oracle/label_fixture.py`, whose two principals are **one entity apart**: that entity is planted inside the widest generating set and nowhere else, so "one member short" is a fact about the corpus rather than a hope. Three labels of one layer over one membership, differing only in which generating sets their ranked contents were drawn from — so the same response carries an absence *and* its control. The one whose only content spans the split entity is **absent whole** for the narrower principal (no identity, no count, no stripped description — decision 0076); the ranked one falls back to the content they do contain; the third is served to both. The layer is `public` with an `inherited` artifact gate and no existence criterion, so containment is the only conjunct that can fail. Checked at four zoom tiers. The **cache half behaviourally**: warm every tier on one token, suppress that single generating-set member, re-ask on the **same token** — the label is withheld at the ack and the answer is byte-for-byte the narrower principal's, because containment is a function of the mask and not of how an entity left it. The pin is not re-presented: decision 0041 made it advisory, so it could not hold a suppression out either way. |
| `tests/test_overlay_journal.py` | I1, I7, I2 | The overlay-heavy catalogue state: acked control operations — deletes and suppressions, which since decision 0047 withdrew the `predicate` op are the whole of what a Phase 1 overlay can hold — composed in entity space by `oracle.journal.AckedJournal` and in row space by the engine. Counts **and served points**, the latter against a **θ-live** server — the combination that catches an engine sampling from the pre-overlay mask (which serves denied items as marks while every count stays right) and one anchoring θ on the pre-overlay projection (§7.2's own I2 leak). Its negative control builds both of those engines out of the oracle and shows the comparison rejects them. Plus the journal's rules: a refused operation enters no composition and moves nothing, and an acked ingest is not an applied one. And the withdrawn `predicate` op: refused with a typed 422 in both directions, composing nothing — the pin on the novel-descriptor silent hide the withdrawal dissolved. |
| `tests/test_multiview_differential.py` | I1, I2, I7 (order), I10/C17, I12 (scoped operand); `views.md` §1, §4, §5, §9 | **The multi-view differential**, over `oracle/multiview.py`'s corpus: four views on one entity space of 6,144 items in six compartments, the plain view holding all of them and a group of three holding different subsets, each with its own frame and its own independently drawn layout. Masked tile counts per view against the oracle answering through that view's permutation, Morton order and frame; `served(view) == mask ∩ members(view)` as an **equality** on one token across every view, with the union over all of them equal to the mask; the same entity's `tessera_id` identical in every view and its position different in each; contracts §2.6's within-tile order in every view; and `views.md` §5's pinned leaf — bare under its own view, pinned by key and across views of one group, with per-view presence and the two refusals. A negative control fails if any two views serve the same per-tile counts. ⊘ No gate: every view is `public`, the gate being unbuilt |

## Fixtures

The catalogue is cached at a fixed `/tmp` path and rebuilt whenever the **stamped recipe** beside
the bundle is not the input set the builder wants now (`<bundle>.FIXTURE.json`; see
`oracle/harness.py`'s "Fixture reuse" section). Reuse used to be decided by a predicate over the
bundle — an allowlist that had to be extended in step with every new build input, and twice was
not, which is how the suite came to be green as a function of `(checkout, /tmp state)` rather than
of the checkout. Delete the stamp to force a rebuild; no manual cache wipe is needed for a changed
seed, layout, extent or identity key.

**Every fixture is synthesised from a seed. Nothing here reads the Phase 0 corpus**, which is what
lets the suite run from a clean checkout and therefore in CI. The byte-scan and restart-replay
modules used to build from a 250k prefix of it; each module's own doc records what moving off it
cost. `reference/tests` still uses that corpus and is a separate question.

| Fixture | Path | What it is for |
|---|---|---|
| `catalogue_bundle_root` | `/tmp/tessera-catalogue` | 150,000 synthetic items designed **backwards from the adversarial mask catalogue** (`oracle.catalogue`), so each mask shape is reachable as a grant set. Three Roaring containers, a block placed astride 65,536, a pair straddling §7.2's ~5% crossover, a block confined to one depth-6 tile, and a `high_tail` block placed entirely above the byte-scan's floor. Since 2026-08-12 the schema also plants a render-only category (`shelf`) and two blob-resident columns (`note`, `pages`) whose generation functions carry the blob's adversarial shapes: an empty-string value, a present zero, an oversize row, and entities absent from has-row entirely. Since 2026-08-13 it plants the keyword column `submitter`: two anchor values carried by one entity each and sorting first and last, ~7,800 near-unique `node-<region>-<id>` keys sharing a stem so a prefix range spans hundreds of front-coded blocks, and 36 `hub-<region>-<letter>` values carried by thousands of entities each. |
| canary states | per-test tmp dir | Three tiny corpora from `oracle.canary_fixture`, built under one identity key: the base corpus, the same plus an item carrying a term nobody holds, and the same plus an item carrying a term principals do hold. The third is the comparator's positive control. |

## Known limitations / explicitly out of scope

- **`fx_key` is planted but cannot be served** — conformance design §2/decision 4 makes a per-item
  declared scalar the handle→item join, and `tessera-build` writes `declared_scalars: Vec::new()`
  into MANIFEST and `scalars: Vec::new()` onto every tiler item, so no built bundle can carry one.
  Every other layer already supports it (`tessera-store::write_segment` takes a scalar schema,
  `tessera-wire::viewport_ipc` emits scalar columns, `/control/ingest` parses them); only the build
  does not connect them. Pinned by a **strict** xfail in `tests/test_mask_catalogue.py`, so the day
  the build gains support the test fails and someone reads this paragraph. That test's body
  requests a viewport and checks the served column against the planted keys — not just MANIFEST —
  so when it flips it exercises the wire path rather than reading green on a declaration. Until
  then the differentials join by the ordered `(x, y)` list, which is weaker: two entities sharing
  rounded coordinates in one tile are indistinguishable to it. Where an exact answer is needed
  (which items did a defective engine serve?) the tests use `tessera_id` and the fixture's own
  key instead.
- **I10 byte-scan is necessary, not sufficient** (documented in the test file itself): absence of
  a matching byte pattern cannot prove no code path could ever leak an entity id under a different
  encoding. The complementary, structural half of I10's assurance is a code review of
  `tessera-wire/src/handles.rs` (`HandleTable` mints an independent per-session counter, never a
  transform of `EntityId` — read that module's own doc, which already discusses the sequential-
  mint disclosure it deliberately accepts).
- **Deny-op WAL-append-failure fault injection**: forcing a real fsync failure needs the write
  path's fault switchboard (lifecycle §7.3), which is reachable from Rust tests and not from here.
  Not tested in this suite. Note this is a *different* property from the durability ordering
  `test_restart_replay.py` now covers: that one asks what survives when unsynced bytes are lost,
  this one asks what the engine does when the sync itself fails.
- **A coalesced keyword extent is not reachable from here**, so records §10's "an entity whose only
  value arrived in a coalesced extent" has no test in this suite. The entity-space coalesce
  deliberately declines a column whose extents carry a dictionary: a coalesced extent would sit in
  the live composition beside the dictionaries of the extents it replaced, its ordinals renumbered
  against a dictionary no reader holds. Such a column waits for the fold. The rule and its reason
  are in `tessera_engine::coalesce::plan_coalesce`, pinned by
  `a_column_with_per_layer_dictionaries_waits_for_the_fold`; the merge itself
  (`tessera_filter_write::coalesce_keyword_extents`) has its own differential in that module,
  `a_coalesced_keyword_extent_reads_back_every_entitys_own_key`. There is
  therefore no HTTP-reachable state to test, and `test_keyword_layers.py` covers the **fold** — the
  route such a column actually takes — rather than hand-writing an artefact to fake the other.
- ~~**A session does not see a flush that promoted a descriptor it had already named.**~~ **Fixed
  2026-08-14** (#112). Two callers paired a session's `satisfied` — frozen at authorise — with the
  *live* generation's dictionary length, which is the one pairing `FragmentCache::get_or_build`'s
  caller obligation forbids: the memo from `auth_data_hash` to the canonical fragment key then held
  a pre-promotion term set under a post-promotion length, and the next authorise of the same bytes
  hit it and was handed the fragment for the grant set it had just stopped having. Both now pass the
  **generation stamp** the term set was resolved against, which the engine refuses to publish
  without strictly increasing — where a dictionary length is faithful only while nothing renumbers,
  and compaction's term sweep can renumber.
  Pinned by two tests in `crates/tessera-engine/tests/dict_generation.rs`: one drives the
  request path and one the **background refresh**, which is the caller that made the defect look
  triggerless — it rebuilds every resident session's fragment after every publication, unprompted,
  so in a running deployment nobody has to make the request that writes the bad entry. Each test
  fails on its own call site under the pre-fix behaviour. The suite's own habit of authorising after
  the flush it means to see is kept: it is good practice independently, and each layering module
  still asserts the principal's unfiltered corpus size at every stage, so a generation disagreement
  fails as its own precondition rather than as an unexplained filter result.
- **`x-tessera-view`**: unimplemented per the ledger note (Phase 1 ships exactly one view); not
  exercised.
- **Task 6's bit-flipped-`.frag`-is-a-cache-miss check**: added as a Rust unit test,
  `crates/tessera-authz/tests/fragment.rs::bit_flipped_frag_file_is_treated_as_a_cache_miss_and_rebuilds`,
  rather than here. It needs no running server or HTTP surface — `FragmentCache` is exercised
  directly, in-process, which is both cheaper and a more precise reproduction of the exact failure
  mode (a corrupted, right-length `.frag` sidecar) than driving it through the whole stack would
  be. Kept alongside Task 6's existing fixture-cache tests rather than invented as a new
  conformance file.
- **The I4 compile-fail rows are in Rust, not here**: `crates/tessera-types/tests/ui/`, driven by
  `trybuild`. They assert that code does *not* compile, which no pytest module can do. `cargo test
  --workspace` runs them.
- **Five of conformance §5's eight interleaving scripts cannot be written yet** (scripts 2–6) —
  they test a stamp ledger, a retirement floor and a compaction fold that do not exist. Script 7,
  the positional CRC rule, is covered in substance by `crates/tessera-lifecycle/tests/wal.rs` in
  both directions. Script 8 needs a `before_fragment_acquire` pause point, and §5 is emphatic that
  it must be built by extending the write path's existing fault switchboard rather than beside it.
- **Durability ordering is not established here** — see `test_restart_replay.py`'s module doc and
  issue #71. The truncating test proves replay under discard of the unsynced tail; an engine that
  published its sync offset without ever fsyncing passes it. The property is held in Rust by the
  write path's `Published` token type.
- **Wall-clock**: 24 s from deleted fixtures, including the 150,000-item catalogue build, measured
  2026-08-01 over 97 tests. 22 s over 239 tests against a warm fixture, measured 2026-08-13 — the
  keyword family's two modules added 142 tests and no measurable time, the layered one included,
  because its ingests, flushes and compaction fold are all sub-second on a corpus this size. The
  two figures are not comparable (one carries the build, the other does not) and are both kept
  rather than averaged. `.github/workflows/ci.yml` records the cold one alongside the Rust gate's
  figure, since runner time is what decides whether a gate stays enabled.
