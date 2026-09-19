# Outstanding fixes

Things found during the cleanup that are not yet done. One line each; delete a line when it is fixed.

## Missing on one path

- `render` on a column declared at a running service is refused. A build accepts it. Needs a decision on where a rendered value lands for entities that already have rows.
- Category-typed view metadata works at a build and is refused at a running service: the create route resolves no vocabulary key.
- A closed vocabulary can now be declared empty, but the Python SDK still sends a closed set's keys inline to work round the old refusal (`clients/py/tesseradb/_commit.py`, `_database.py`).

- View metadata: a build widens an integer where a float is declared; a running service refuses it.

## Structure

- `tessera-build` keeps a second whole implementation of the build (`build_in_memory`) as a test oracle for the streaming one. Every change to the build is made twice.
- Things declared at a running service live in separate lists (`RuntimeAttributes`, `RuntimeVocabularies`, `RuntimeViewDeclarations`) until a fold writes them into `MANIFEST.json`, then are removed from those lists. Two manifests hold one schema.

- The Python oracle's `BUNDLE_FORMAT` (`reference/oracle/harness.py`) is 11 and the Rust constant is 16, so the oracle does not accept a current bundle.

## Documentation

- `docs/design`, `docs/evidence` and `docs/decisions` are out of the tree (tag `docs-before-hide`). About 70 code comments and the Sources sections of `docs/system` still cite them. The link check is out of CI until they are swept.
- The reference set (on-disk formats, the control plane in OpenAPI, measured facts) is not written. `docs/reference`, `docs/guide` and `docs/developer` are skeletons.
- `scripts/check-test-reachability.sh` has not been examined for whether it guards an outcome.
