# `conformance/` — the invariant conformance suite

Pytest, same venv as `reference/` (`reference/.venv`, Python 3.12). Drives the release `tessera`
binary as a real subprocess — never calls into `tessera-engine` directly — using the same
server-spawning harness `reference/tests` uses (`reference/oracle/harness.py`, extracted from
`reference/tests/conftest.py` in this task so neither suite copy-pastes the other's spawn logic).

Run it:

```
reference/.venv/bin/pytest conformance/tests -v
```

CI runs it on every pull request and every push to `main`, alongside the rest of CLAUDE.md's gate
(`.github/workflows/ci.yml`). That is conformance §6's per-PR tier; the nightly and release tiers
it also specifies do not exist.

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
| `tests/test_canary.py` | I2 | Three synthetic states (`reference/oracle/canary_fixture.py`) built under one identity key, differing in one item at the extreme corner of the extent: the base corpus; the base plus an item carrying a term no tested grant set holds; the base plus an item carrying a term they do. One comparator runs over all of them, comparing **canonicalised bytes** on three separately-addressable surfaces — the tile batch sorted by tile id, the points batch as served (contracts §3.2 orders it by `tessera_id` within each tile), and the §3.3 underlay's masked per-cell counts. The canary state must agree on all three; the visible state must disagree on all three, which is §4.4's positive control and what stops a canonicalisation that silently dropped a surface from reading green. Also checks the **five allocation rules** for both extra-item states, and that nothing session-dependent remains in the response body. |
| `tests/test_mask_catalogue.py` | fixture integrity | The adversarial mask catalogue (`reference/oracle/catalogue.py`) is the shape it claims: eight cases, each named for the property it attacks, each reaching exactly its declared entity set by two independent routes (postings union, pairs semi-join). Includes the container-boundary and ~5%-crossover claims, which are the two that stop being true silently. Carries a **strict xfail** for `fx_key` in the points batch — see "Known limitations". |
| `tests/test_i7_selection.py` | I7 | §7.2's served set, engine against the literal definition in `reference/oracle/viewport.py`, over the catalogue × depths `{0,2,4,6}` × `k` `{2,30,500}`, against both a θ-saturated and a θ-live server, compared as **ordered lists** (contracts §2.6 makes the order contract). Plus cross-zoom nesting into *the child that contains the point*; the **cap**, against a server with `k_max_marks = 128` so `cap = min(k, K_max)` actually binds; and **the negative control**: a first-`k` stub that serves §7.2's count in storage order instead of `tessera_id` order, with the assertion that the differential disagrees with it on most tiles. A differential that passes against a deliberately wrong implementation is testing nothing. |
| `tests/test_filter_differential.py` | I12 (mask half), C11 | The attribute-filter differential: engine against `reference/oracle/filters.py` — a per-entity walk over the **fixture's planted values**, never the `attrs/` artefact — across the mask catalogue's principals. Per tile, a filter moves `matched` and never `visible`; the filtered served set equals the oracle's brute-force `M_sel` exactly (θ saturated, so a filter that *dropped* visible matching items is caught, not just one that widened); a hidden, a hollow and a nonexistent category value are byte-identical in outcome, with a single-member `solo` value as the positive control; `all_of`/`any_of` nest and obey their empty identities; an unknown column is a `422` and an unknown value is not. The fixture's decorrelation of attribute from grant is itself asserted, because a correlated fixture passes every cross-principal check vacuously. Carries a **strict xfail**: a cross-family operator (`prefix` on a category) answers as an empty operand where the contract reading says `422` — see the test's docstring. The frontier half of I12, I3 and Rule S over filter counts stay uncovered; the module doc says why each. |
| `tests/test_overlay_journal.py` | I1, I7, I2 | The overlay-heavy catalogue state: acked control operations — deletes and suppressions, which since decision 0047 withdrew the `predicate` op are the whole of what a Phase 1 overlay can hold — composed in entity space by `oracle.journal.AckedJournal` and in row space by the engine. Counts **and served points**, the latter against a **θ-live** server — the combination that catches an engine sampling from the pre-overlay mask (which serves denied items as marks while every count stays right) and one anchoring θ on the pre-overlay projection (§7.2's own I2 leak). Its negative control builds both of those engines out of the oracle and shows the comparison rejects them. Plus the journal's rules: a refused operation enters no composition and moves nothing, and an acked ingest is not an applied one. And the withdrawn `predicate` op: refused with a typed 422 in both directions, composing nothing — the pin on the novel-descriptor silent hide the withdrawal dissolved. |

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
| `catalogue_bundle_root` | `/tmp/tessera-catalogue` | 150,000 synthetic items designed **backwards from the adversarial mask catalogue** (`oracle.catalogue`), so each mask shape is reachable as a grant set. Three Roaring containers, a block placed astride 65,536, a pair straddling §7.2's ~5% crossover, a block confined to one depth-6 tile, and a `high_tail` block placed entirely above the byte-scan's floor. |
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
- **`x-tessera-slice`**: unimplemented per the ledger note (Phase 1 ships exactly one slice); not
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
  2026-08-01. `.github/workflows/ci.yml` records it alongside the Rust gate's figure, since runner
  time is what decides whether a gate stays enabled.
