# Tessera documentation

Four sets, each for a different reader. Pages marked *skeleton* have headings and a sentence of intent and no content yet.

| Set | For | Contents |
|---|---|---|
| [design/](design/) | Engineers working on the system; assessors checking the security argument | How Tessera works and why. The invariants and the leak register. [Decisions](decisions/) that bind the design |
| [reference/](reference/) | Anyone who needs an exact value | Formats on disk and on the wire, the HTTP API, measured facts, and material considered but not built |
| [guide/](guide/) | Operators and client developers | What Tessera does, a quickstart, the configuration reference, running a service, the client libraries |
| [developer/](developer/) | Contributors | Building from source, testing, CI, releasing, the roadmap, how work is done here |

## Target shape

The design set is written, in `system/`, in this reading order: [overview](system/overview.md), [security](system/security.md), [data model](system/data-model.md), [access control](system/access-control.md), [queries](system/queries.md), [annotations](system/annotations.md), [write path](system/write-path.md), [serving](system/serving.md), [clients](system/clients.md). The documents in `design/` remain the specification until they are deleted in one change and the code's citations are swept to the chapters. What each chapter replaced:

| Chapter | Replaces |
|---|---|
| overview | `design/README.md`, `architecture.md` §1 to §3, `system-architecture.md` |
| security: the threat model, the properties and how each is enforced, the residual-disclosure register, what is not claimed, the evidence | `architecture.md` §4 and Appendix C, `conformance.md` §4 |
| data model: entities, rows, views, projections, fields | `architecture.md` §5, `views.md`, `projections.md`, `records-and-search.md`, `per-point-attributes.md` |
| access control: masks, the overlay, sessions, plugins | `architecture.md` §6, `concurrency-lifecycle.md`, `core-access-expressions.md` |
| queries: viewport, filters, selection, suggest, search | `architecture.md` §7 and §8, `filter-index.md`, `filter-surface.md`, `selection-operand.md`, `value-suggestion.md`, `highlight-and-hierarchy.md` |
| annotations | `artifact-system.md` (the template), `annotations.md`, `annotation-representation.md`, `annotation-write-cycle.md`, `artifacts-from-points.md`, `artifact-shapes.md`, `polygon-membership.md`, `dag-hierarchies.md`, `artifact-serving-at-scale.md`, `artifact-fetch-protocol.md` |
| write path: ingest, flush, denies, merge, compaction | `write-path.md`, `compaction.md`, `geometry-pinning.md` |
| serving: streaming, delta, caches, freshness | `streamed-serving.md`, `delta-serving.md`, `caching.md`, `filter-result-cache.md`, `tile-addressed-integration.md`, `hot-row-geometry.md` |
| clients: the boundary, the store, the rules a client must keep | `client-interaction.md`, `client-architecture.md`, `client-components.md`, `client-obligations.md`, `view-switching.md` |

The reference set takes `contracts.md` §2 to §5 (formats and API), `openapi/`, the measured facts the design depends on (from `evidence/` and `probes/`), and the deferred sketches. The guide set takes `configuration.md` §1, §5, §7 and §8, `guides/views.md`, and the client READMEs. The developer set takes `conformance.md`, `correctness-suite.md`, `agents/`, `roadmap.md` and the CI workflow.

Each chapter is drafted by extraction from the documents it replaces, checked against a list of the rules that must survive, and lands on its own branch. The replaced documents are deleted in the same change.
