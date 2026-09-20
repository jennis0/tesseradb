# Outstanding fixes

Things found during the cleanup that are not yet done. One line each; delete a line when it is fixed.

## Missing on one path

- `render` on a column declared at a running service is refused. A build accepts it. Needs a decision on where a rendered value lands for entities that already have rows.
- Category-typed view metadata works at a build and is refused at a running service: the create route resolves no vocabulary key.
- A closed vocabulary can now be declared empty, but the Python SDK still sends a closed set's keys inline to work round the old refusal (`clients/py/tesseradb/_commit.py`, `_database.py`).

- A generating set that holds an ingested member is answered by the exact masked-count route until a fold, because the containment partition is composed from the build's postings and does not read the flushed term tiers. Correct, and slower for those sets. Composing over the tiers means recomposing at every flush; the cost is unmeasured.
- No test publishes the same artifacts to a per-view layer through a build and through `PUT /control/layers/{name}/artifacts` and compares what is served. `crates/tessera-build/tests/scoped_layer_keys.rs` and `crates/tessera-server/tests/artifact_views.rs` each cover one path.

- View metadata: a build widens an integer where a float is declared; a running service refuses it.

- Unverified, from reading: a group-scoped family declared at a running service has no base on disc until its first flush, and `FilterColumns::open` may refuse the bundle rather than open the family empty.
- Unverified, from reading: a flush that publishes just after its view is dropped lists and composes its extents under the dropped view; they stay until a fold.
- An entity-scoped fill whose view is dropped waits for a flush of some surviving view. If the drop leaves no view at all, it holds the log until one is created.
- `DELETE /control/views/{group}/{key}` is in the HTTP API only: not in `docs/openapi/tessera.yaml`, the Python client, the TypeScript client or the CLI.
- A layer's `visibility` and `artifact_visibility.default` are checked for an empty word and for `inherited` at a build (`tessera_plugin::check_label`) and not when a layer is declared at a running service.
- The build always labels with `builtin:passthrough`; the engine takes whichever plugin it is given. Five sites compare a manifest's hash with `Passthrough::new().data_plugin_hash()` and three of them are in the engine, which holds its own plugin (`containment.rs`, `generation.rs`, `write/schema.rs`, `artifact_pass.rs`, build `config.rs`).

## Structure

- `tessera-build` keeps a second whole implementation of the build (`build_in_memory`) as a test oracle for the streaming one. Every change to the build is made twice.
- Things declared at a running service live in separate lists (`RuntimeAttributes`, `RuntimeVocabularies`, `RuntimeViewDeclarations`) until a fold writes them into `MANIFEST.json`, then are removed from those lists. Two manifests hold one schema.
- `PauseSiteArg::BeforeAck` (`write/executor/mod.rs`) is never constructed in a build without `fault-injection`, so `cargo clippy -p tessera-engine --lib -- -D warnings` fails. The workspace clippy enables the feature and does not see it.
- `Engine::request_flush`'s comment says `flush_max_items` was deleted. It is a live `EngineConfig` field and the tick reads it.

- `coalesce_delta_tiers` (`tessera-authz/src/tier.rs`) gathers every term's entities from every input tier into a `Vec<u32>` per term before it writes anything, so its memory is the whole of the tiers it merges. The fold's sweep (`term_sweep.rs`) does the same job a term at a time through bitmaps and a spool. The coalesce has no budget and nothing measures it.
- The Python oracle's `BUNDLE_FORMAT` (`reference/oracle/harness.py`) is 11 and the Rust constant is 16, so the oracle does not accept a current bundle.
- `"public"` is defined twice, as `tessera_plugin::PUBLIC` and as `tessera_authz::PUBLIC_LABEL`. Neither crate depends on the other.
- The word "gate" for a view's or layer's `visibility` is banned by `docs/writing.md` and is used about 1,160 times in `crates/` and 12 times in `docs/system`. It is gone from `tessera-plugin` only.

## Needs a decision

- A group-scoped family declared with neither `index` nor `render` (not text) has a value column: a build writes it and `/v1/items` serves it. The ingest join rule's reader (`write/joined.rs`, `flushed_scoped_of`) asks `scoped_is_filterable` (index or render) and answers "no value held" for it, so a joining row naming a different value for that cell is accepted unchecked, and the flush writes no extent for such a family, so the supplied value is stored nowhere. From reading; no test builds the case. Decide whether the join rule should ask `scoped_has_value_column` (a join accepted today would then refuse), and what a flush owes such a family.
- The engine takes part of its configuration at `Engine::open` (`EngineConfig`) and the rest through nine setters afterwards (`set_cache_bounds` and its siblings). The default values and most validity rules live in `tessera-server`'s loader, so a caller that opens the engine directly (`tessera-bench`, tests, any embedder) gets unbounded caches unless it calls the setters, and re-states every default. `Engine::open` refuses `k_min = 0` and selection widths below 2; the rest is unchecked below the server. Decide whether the engine owns the defaults and the bounds, with the server's loader reading from it.
- A flush fsyncs some of its files and not others. The segment's files and the membership extents are synced before the side-manifest commits; the delta tier, the dictionary extent, the filter-column extents and the entity-terms extent are not. All of them are digested in the manifest, so after a crash a torn one is refused when the bundle opens, and its rows are still in the write-ahead log. Decide what a restart owes that case: refusing the bundle, or dropping the torn flush and replaying the log. If it is the first, syncing the remaining files before the commit closes it.
- A request that waits for another request's fragment build (authorise, and the per-request fragment lookup) cannot be cancelled: the engine passes no cancel token there, so a client that disconnects holds its request slot until the build it waits on finishes or the wait budget runs out. `authorise` takes no token today; the viewport path has one and does not pass it to `fragment_for`.
- A label's row on the artifacts frame still carries its cluster's two filter bits (`matched`, `highlighted`). The row now names its cluster in `target`, so a client can read them from the cluster's own row. Keep the copy or remove it.
- How the `tessera` binary reaches a Python user at release. Measured 2026-09-18: 45 MiB, 12 MiB stripped and compressed. The options were a wheel per platform that the `[local]` extra depends on, or a download on first use.
- A `text` column declared with no `analyser` is given `unicode` by `tessera_store::declaration`. Decide whether the server may choose that or must refuse.

## Clients and tests

- The Python oracle (`reference/oracle/wire.py`) does not decode the artifacts frame's `target` column, so no conformance case asserts it. `clients/py/tests/test_sdk_pages.py` checks it against a live server.
- `clients/ts/components/src/artifact-list.ts` hides label rows only when some label attached to a cluster. That condition is left over from the join by count; labels are hidden whenever the layer depends on another.
- The recorded frames under `clients/ts/core/test` have a null `target` added by `liftArtifactTarget` instead of being recaptured, so no recorded frame carries a label naming its cluster.
- The conformance suite has no corpus with a per-view annotation layer, so nothing there checks that a view serves only its own artifacts. The Rust server tests do.
- `distinct_key_first_viewports_overlap_instead_of_serialising` (`crates/tessera-engine/tests/viewport.rs`) asserts a timing ratio and fails when the box is loaded. It failed in four runs on 2026-09-18 and passed alone each time.
- The coalesce unit tests (`crates/tessera-engine/src/coalesce.rs`) test the planner and the rebase separately, each on a hand-built manifest, and repeat one "replaces its window in both halves" test per axis. A plan, execute, rebase round trip per axis over one shared fixture would test the join between the halves and roughly halve the module.
- `Wal::retained_from` and `batch_identity` (`crates/tessera-lifecycle/src/wal.rs`) are tested only through the engine, and that test checks that a batch id is forgotten at rotation and not that a retained one is kept.
- If `Wal::rotate` fails after deleting some members, the engine skips trimming its batch-id memory until the next rotation that succeeds, so a few ids are remembered longer than the log holds them.

## Documentation

- `docs/design`, `docs/evidence` and `docs/decisions` are out of the tree (tag `docs-before-hide`). About 70 code comments and the Sources sections of `docs/system` still cite them. The link check is out of CI until they are swept.
- The reference set (on-disk formats, the control plane in OpenAPI, measured facts) is not written. `docs/reference`, `docs/guide` and `docs/developer` are skeletons.
- `scripts/check-test-reachability.sh` has not been examined for whether it guards an outcome.
