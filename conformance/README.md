# `conformance/` — Phase 1 invariant conformance suite (Task 15)

Pytest, same venv as `reference/` (`reference/.venv`, Python 3.12). Drives the release `tessera`
binary as a real subprocess — never calls into `tessera-engine` directly — using the same
server-spawning harness `reference/tests` uses (`reference/oracle/harness.py`, extracted from
`reference/tests/conftest.py` in this task so neither suite copy-pastes the other's spawn logic).

Run it:

```
reference/.venv/bin/pytest conformance/tests -v
```

(`reference/.venv/bin/pytest reference/tests -v` must also stay green — this suite's refactor
touched `reference/tests/conftest.py` and `reference/tests/wire.py`, both now thin wrappers over
shared `oracle` modules, but their fixtures/behaviour are unchanged.)

## What's here

| File | Invariant | What it proves |
|---|---|---|
| `tests/test_byte_scan.py` | I10 | No 8-byte LE entity-id encoding appears in any viewport response's points batch, or in the server's `RUST_LOG=info` log, across a zoom range — for both the mask's admitted entities and its denied ones. See the file's module doc for why the scan is restricted to a "safe" high entity-id range (the fixture's dense, small entity-id space is otherwise structurally indistinguishable, by raw integer value, from `tessera-wire`'s deliberately sequential per-session handles and the tile stream's I2-legitimate aggregate counts — scanning the full universe would produce constant, meaningless matches, not real ones). |
| `tests/test_restart_replay.py` | WAL/deny-survival (plan §10.3) | An ingest, two suppressions and a delete all survive a `SIGKILL` (not a graceful stop) and a restart on the *same* WAL/cache/bundle with nothing re-submitted: the denies stay applied, the entity-id allocator's high-water mark is unchanged (evidence the ingested batch replayed with its original allocated ids, not fresh ones — see the file's doc on why this stands in for the brief's "status shows buffered rows", which Phase 1's actual `/control/status` shape doesn't expose directly), and replaying the *same* ingest batch id/body again is still idempotent. |
| `tests/test_canary.py` | I2 (scaffold) | A synthetic bundle pair (`reference/oracle/canary_fixture.py`) — identical except one extra item, at an extreme corner of the extent, carrying a term no tested grant set holds — produces byte-for-byte-equivalent *decoded* responses (tile counts, tile list, point multisets) between the two bundles, across zooms and grant sets, and the canary's own tile never appears in either. Also checks the **five canary allocation rules** as one property: the canary bundle's stored rows are the canary-free bundle's rows plus exactly one at the end. This is the scaffold Phase 2 extends to centroids/hulls; Phase 1 has no other derived aggregate to check. |
| `tests/test_mask_catalogue.py` | fixture integrity | The adversarial mask catalogue (`reference/oracle/catalogue.py`) is the shape it claims: eight cases, each named for the property it attacks, each reaching exactly its declared entity set by two independent routes (postings union, pairs semi-join). Includes the container-boundary and ~5%-crossover claims, which are the two that stop being true silently. Carries a **strict xfail** for `fx_key` in the points batch — see "Known limitations". |
| `tests/test_i7_selection.py` | I7 | §7.2's served set, engine against the literal definition in `reference/oracle/viewport.py`, over the catalogue × depths `{0,2,4,6}` × `k` `{2,30,500}`, against both a θ-saturated and a θ-live server. Plus cross-zoom nesting, and **the negative control**: a first-`k` stub that serves §7.2's count in storage order instead of `tessera_id` order, with the assertion that the differential disagrees with it on most tiles. A differential that passes against a deliberately wrong implementation is testing nothing. |
| `tests/test_overlay_journal.py` | I1 | The overlay-heavy catalogue state: 500 acked control operations (deletes, suppressions, predicate-narrows, and predicate-widens onto entities *outside* the token's mask) composed in entity space by `oracle.journal.AckedJournal` and in row space by the engine, then compared. Plus the journal's one rule, exercised directly: a refused operation is recorded as refused, enters no composition, and moves nothing. |

## Fixtures

Two corpora, both cached at a fixed `/tmp` path and rebuilt only when they are not the shape the
builder now produces:

| Fixture | Path | What it is for |
|---|---|---|
| `bundle_root` | `/tmp/tessera-250k` | A 250k prefix of the Phase 0 corpus, built by `oracle.harness.ensure_fixture_bundle`. Realistic term distribution; random grant sets. |
| `catalogue_bundle_root` | `/tmp/tessera-catalogue` | 150,000 synthetic items designed **backwards from the adversarial mask catalogue** (`oracle.catalogue`), so each mask shape is reachable as a grant set. Three Roaring containers, a block placed astride 65,536, a pair straddling §7.2's ~5% crossover, and a block confined to one depth-6 tile. |

## Known limitations / explicitly out of scope

- **`fx_key` is planted but cannot be served** — conformance design §2/decision 4 makes a per-item
  declared scalar the handle→item join, and `tessera-build` writes `declared_scalars: Vec::new()`
  into MANIFEST and `scalars: Vec::new()` onto every tiler item, so no built bundle can carry one.
  Every other layer already supports it (`tessera-store::write_segment` takes a scalar schema,
  `tessera-wire::viewport_ipc` emits scalar columns, `/control/ingest` parses them); only the build
  does not connect them. Pinned by a **strict** xfail in `tests/test_mask_catalogue.py`, so the day
  the build gains support the test fails and someone reads this paragraph. Until then the
  differentials join by `(x, y)` multiset, which is weaker: two entities sharing rounded coordinates
  in one tile are indistinguishable to it.
- **I10 byte-scan is necessary, not sufficient** (documented in the test file itself): absence of
  a matching byte pattern cannot prove no code path could ever leak an entity id under a different
  encoding. The complementary, structural half of I10's assurance is a code review of
  `tessera-wire/src/handles.rs` (`HandleTable` mints an independent per-session counter, never a
  transform of `EntityId` — read that module's own doc, which already discusses the sequential-
  mint disclosure it deliberately accepts).
- **Deny-op WAL-append-failure fault injection** (`test_restart_replay.py`'s docstring): Task 13
  left this out of scope for Phase 1 (no fault-injection plumbing exists yet to force a real fsync
  failure); not tested here either.
- **`x-tessera-slice`**: unimplemented per the ledger note (Phase 1 ships exactly one slice); not
  exercised.
- **Task 6's bit-flipped-`.frag`-is-a-cache-miss check**: added as a Rust unit test,
  `crates/tessera-authz/tests/fragment.rs::bit_flipped_frag_file_is_treated_as_a_cache_miss_and_rebuilds`,
  rather than here. It needs no running server or HTTP surface — `FragmentCache` is exercised
  directly, in-process, which is both cheaper and a more precise reproduction of the exact failure
  mode (a corrupted, right-length `.frag` sidecar) than driving it through the whole stack would
  be. Kept alongside Task 6's existing fixture-cache tests rather than invented as a new
  conformance file.
- **Wall-clock**: see `.superpowers/sdd/2026-07-28-phase1-walking-skeleton/task-15-report.md` for
  the measured run time of `pytest conformance/tests -v`.
