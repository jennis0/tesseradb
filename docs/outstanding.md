# Outstanding fixes

Things found during the cleanup that are not yet done. One line each; delete a line when it is fixed.

## Missing on one path

- `render` on a column declared at a running service is refused. A build accepts it. Needs a decision on where a rendered value lands for entities that already have rows.
- Category-typed view metadata works at a build and is refused at a running service: the create route resolves no vocabulary key.
- A closed vocabulary can now be declared empty, but the Python SDK still sends a closed set's keys inline to work round the old refusal (`clients/py/tesseradb/_commit.py`, `_database.py`).

- A generating set that holds an ingested member is answered by the exact masked-count route until a fold, because the containment partition is composed from the build's postings and does not read the flushed term tiers. Correct, and slower for those sets. Composing over the tiers means recomposing at every flush; the cost is unmeasured.
- No test publishes the same artifacts to a per-view layer through a build and through `PUT /control/layers/{name}/artifacts` and compares what is served. `crates/tessera-build/tests/scoped_layer_keys.rs` and `crates/tessera-server/tests/artifact_views.rs` each cover one path.

- View metadata: a build widens an integer where a float is declared; a running service refuses it.

## Structure

- `tessera-build` keeps a second whole implementation of the build (`build_in_memory`) as a test oracle for the streaming one. Every change to the build is made twice.
- Things declared at a running service live in separate lists (`RuntimeAttributes`, `RuntimeVocabularies`, `RuntimeViewDeclarations`) until a fold writes them into `MANIFEST.json`, then are removed from those lists. Two manifests hold one schema.
- `crates/tessera-engine/src/write.rs` is 18,000 lines.

- The Python oracle's `BUNDLE_FORMAT` (`reference/oracle/harness.py`) is 11 and the Rust constant is 15, so the oracle does not accept a current bundle.

## Needs a decision

- A label's row on the artifacts frame still carries its cluster's two filter bits (`matched`, `highlighted`). The row now names its cluster in `target`, so a client can read them from the cluster's own row. Keep the copy or remove it.
- How the `tessera` binary reaches a Python user at release. Measured 2026-09-18: 45 MiB, 12 MiB stripped and compressed. The options were a wheel per platform that the `[local]` extra depends on, or a download on first use.

## Clients and tests

- The Python oracle (`reference/oracle/wire.py`) does not decode the artifacts frame's `target` column, so no conformance case asserts it. `clients/py/tests/test_sdk_pages.py` checks it against a live server.
- `clients/ts/components/src/artifact-list.ts` hides label rows only when some label attached to a cluster. That condition is left over from the join by count; labels are hidden whenever the layer depends on another.
- The recorded frames under `clients/ts/core/test` have a null `target` added by `liftArtifactTarget` instead of being recaptured, so no recorded frame carries a label naming its cluster.
- The conformance suite has no corpus with a per-view annotation layer, so nothing there checks that a view serves only its own artifacts. The Rust server tests do.
- `distinct_key_first_viewports_overlap_instead_of_serialising` (`crates/tessera-engine/tests/viewport.rs`) asserts a timing ratio and fails when the box is loaded. It failed in four runs on 2026-09-18 and passed alone each time.
- `artifact_interleavings` hung once for 70 minutes at no CPU while three other cargo runs shared the box (2026-09-18). It has not reproduced.
- `Wal::retained_from` and `batch_identity` (`crates/tessera-lifecycle/src/wal.rs`) are tested only through the engine, and that test checks that a batch id is forgotten at rotation and not that a retained one is kept.
- If `Wal::rotate` fails after deleting some members, the engine skips trimming its batch-id memory until the next rotation that succeeds, so a few ids are remembered longer than the log holds them.

## Documentation

- `docs/design`, `docs/evidence` and `docs/decisions` are out of the tree (tag `docs-before-hide`). About 70 code comments and the Sources sections of `docs/system` still cite them. The link check is out of CI until they are swept.
- The reference set (on-disk formats, the control plane in OpenAPI, measured facts) is not written. `docs/reference`, `docs/guide` and `docs/developer` are skeletons.
- `scripts/check-test-reachability.sh` has not been examined for whether it guards an outcome.
