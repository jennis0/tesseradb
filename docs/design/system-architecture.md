# Tessera — System Architecture: Storage, Serving and Lifecycle

**Status:** Draft r5 — r4 plus ingest-audit corrections: flush as the visibility mechanism, batch-into-live recorded as open, fragmentation metric (see Appendix R)
**Companion to** `tessera-architecture-design.md` (r23), which owns the invariants and the *why*, and `tessera-implementation-plan.md`, which owns phasing. This document owns the *shape of the built system*: processes, crates, contracts, artifact formats, the operational lifecycle, configuration and packaging. `§n` refers to the design document. Where this document and the design disagree, the design is right.

**Scope note.** Backend only — the storage engine, the query surface and the lifecycle. The client appears only where its contract constrains the backend (the wire format). The visualisation architecture document remains the reference for everything drawn.

**Phase 0 caveat.** Everything here is conditional on the Phase 0 memo saying go. Nothing in this document weakens that gate; it exists so that when the memo lands, Phase 1 starts from a component design rather than a blank page.

---

## 1. The shape of the system

One Rust engine, one binary, two planes.

```
                        ┌─────────────────────────────────────────────┐
   app tier ───────────►│  tessera (single binary)                    │
   (session plane:      │  ┌──────────────┐   ┌──────────────────┐   │
    authorise, revoke)  │  │ session API  │   │  viewer API      │   │◄──── browser /
                        │  │ admin API    │   │  HTTP, token-    │      client SDK
   ops / data tier ────►│  │ scoped creds │   │  bearing         │   (data plane:
   (admin plane:        │  └──────┬───────┘   └────────┬─────────┘    meta, viewport,
    ingest, changes,    │         ▼                    ▼              labels, region,
    labels, lifecycle)  │  ┌─────────────────────────────────────┐    drill-down)
                        │  │            tessera-engine           │   │
                        │  │  authz · spatial · store · labels   │   │
                        │  │  filter · lifecycle · wire          │   │
                        │  └─────────────────────────────────────┘   │
                        │         │ mmap            │ fsync          │
                        └─────────┼─────────────────┼─────────────────┘
                                  ▼                 ▼
                          bundle/  (append-only     wal/  (durability for
                           versioned prefixes)       ingest, changes, allocator)
                                  ▲
                                  │ writes full builds
                          tessera build — the same engine in batch mode
```

**Three surfaces, not one API with roles**, scoped by who holds the credential and what losing it costs:

- The **viewer plane** accepts only a token plus a query — the sole surface an untrusted client ever reaches.
- The **session plane** carries exactly `authorise` and `revoke`, on its own listener with its own credential, held by the app tier. `authorise` stays off the viewer plane: under a bare-claims plugin the service trusts presented auth data by fiat (I5, §2.4), so an untrusted surface that mints capability from unverifiable claims is a bypass, not an endpoint; under a verifiable-credential plugin (§4.3) the argument becomes defence-in-depth and cost control rather than impossibility (D2). The integrating backend authenticates its user however it likes, calls `authorise`, and forwards only the token. And `authorise` does not sit with admin either: the app tier authorises on every session and should hold a credential that can mint sessions, not one that can ingest or delete (§4.2).
- The **admin plane** (unix socket by default) carries ingest, item changes, label submission and lifecycle operations under an *operator* credential, with the build-scope operations of §2.5 (unmasked node iteration) behind a further *build* credential — structurally unreachable from a user token or a session credential.

**Rust-native end to end; Python is a first-class consumer, never a component.** Every surface is plain HTTP with Arrow IPC bodies, so any language integrates directly — a JVM or Go shop uses the binary and the HTTP surfaces and Python never enters their stack. The "deployable via Python" requirement is met by packaging, not by architecture: `pip install tessera` ships the Rust binary inside the wheel (maturin; the `ruff` precedent), plus an SDK and a supervisor — `tessera.serve()` spawns and manages the process, `tessera.build()` drives the engine's batch mode, and requests never touch the Python interpreter. This keeps the trusted computing base one auditable binary, and it is what makes the compartment isolation ladder (§12.4) available: partitions are child processes, and a Python host process would put every compartment inside one address space.

**Consumption modes**, all the same engine:

| Mode | What runs | For whom |
|---|---|---|
| `tessera serve` | the binary, directly | production deployments |
| `tessera build` | the binary, batch mode | anyone producing bundles (§6.1) |
| `tessera.serve()` / `tessera.build()` (Python) | the same binary, supervised / driven | notebooks, small deployments, CI |
| `tessera-engine` (crate) | the engine linked into a Rust host | embedders with their own shell |

There is no in-process Python query mode. A notebook queries the supervised process over the session and viewer planes and gets `pyarrow` Tables back; one query path, one set of tests.

## 2. Process model

### 2.1 Default: one process

A deployment with no compartmented terms runs a single `tessera` process serving the single default partition. All of §12's machinery is dormant; nothing below this subsection is configured, spawned or paid for.

### 2.2 With compartments: a router and workers

When the bundle contains compartmented partitions, the parent process is a *router* and each partition is a *worker* process (`tessera --partition <hash>`) mmapping only its own partition's files. The division of state follows one rule, which is §12.3's isolation property restated: **entity IDs, bitmaps and columns for a compartment exist only inside its worker.** Concretely:

**The router holds:** the bundle-level term dictionary (descriptors are opaque policy-side identifiers, not corpus data — see §4.1), the plugin host, token state (auth data retained per §2.3, satisfied-term set, reachable-partition set), the label *presence registry* (below), and the per-session handle-routing key. It holds no postings, no masks, no columns, no entity IDs.

**Each worker holds:** its partition's term postings, mask *fragments* (built and cached locally, never shipped), columns, permutation, overlay, watermark, WAL, external-ID map, node membership, generating-set slices and label text.

**Authorise fans out.** The router runs `terms_of_auth`, interns to term IDs, computes the reachable set by testing required sets from the manifest, and sends the satisfied-term list to each reachable worker, which builds (or content-address-hits) its fragment and acks with no payload. The token records which workers acked. A worker restart loses its fragment cache only: the router retains the auth data, so the next query to that worker rebuilds the fragment transparently — §2.3's eviction-transparency rule doing double duty as crash recovery.

**Queries fan out and merge.** Counts sum; priority samples take the global k-lowest of per-worker k-lowests; containment composes per §2.4 below. Workers return payload rows keyed by *handles they mint themselves* (§4.5), never entity IDs, so merged responses assemble at the router without corpus identifiers crossing the compartment boundary. Control operations that address items (`/control/changes`) fan out to all workers; each consults its own external-ID map and the router aggregates acks.

**Worker lifecycle.** The router spawns workers at boot from the manifest's partition list, supervises them (restart with backoff, readiness gating), and enforces a per-request worker timeout. A worker that is down or times out fails the request — I13 permits *unreachable-by-authorisation* to contribute nothing satisfied; *unreachable-by-outage* is an error, never an empty contribution. A partition discovered at runtime (first item with a new required set, §12.4) triggers: alarm, directory + manifest side-file creation, worker spawn; it becomes queryable only for tokens authorised after it became ready — conservative and correct, since reachable sets are fixed at authorisation.

### 2.3 Cross-partition labels and the presence registry

The natural implementation of label containment — AND the verdicts of the workers you queried — is precisely the I13 violation §12.3 warns against: a generating set with a non-empty slice in an *unreachable* partition must fail, and a router that doesn't know the slice exists cannot fail it. So presence is first-class metadata:

- A label's generating set is stored *sliced*: each partition with a non-empty slice `G_p` holds that slice, plus the label text and tier (text is duplicated only into partitions whose data contributed to it, keeping it inside the ladder).
- Every slice record carries the label's full **partition-presence set** — the list of required-set hashes with non-empty slices — fixed at submission and build-enforced to be identical across copies.
- The router's label merge admits a label iff *presence ⊆ reachable* **and** every partition in presence returns `G_p ⊆ M_p`. Anything else — a missing verdict, a timeout, a presence entry outside the reachable set — withholds.

The presence registry (label id → presence set, no text, no entity IDs, no cardinalities) is the one piece of cross-partition label metadata the router holds. Isolation argument: it reveals *that* some label draws on a compartment, to a process that already routes queries into that compartment; it contains no corpus content. This mirrors the C13/C14 style of argument and should get its own line in Appendix C when the design is next revised (see Appendix R, action 3).

### 2.4 What this does not decide

Sharding by Morton range (§13.3) stays out of scope and unforeclosed: nothing here assumes a single global mask or single-process index, and the router/worker protocol is the same shape a shard fan-out will need. Slices are data, not processes: all temporal slices of a partition are served by its one worker, selected per request (§9).

## 3. Crate decomposition

A single workspace. The decomposition tracks the design's subsystems, but its real job is making the invariants structural: each crate's public surface is chosen so that violating an invariant is a compile error or an impossible import, not a code-review catch.

```
crates/
  tessera-types      EntityId(u64), RowId(u32), TermId(u32), Handle(u32), Priority,
                     MortonCode, PinId, contract-version constants. No conversions
                     between ID newtypes. No I/O. Everyone depends on it.
  tessera-plugin     wasmtime host; plugin ABI; module-hash versioning; the built-in
                     access-expression plugin (Appendix E) compiled natively.
  tessera-authz      term interning (single append-only namespace, §6.1); postings
                     readers (base + delta tiers; sorted-array small terms below the
                     manifest threshold); postings-union mask build — Phase 0
                     reassigned the §6.3 semi-join to build cadence and the oracle,
                     the union being 4–540× faster (probes, results §4.1) — with the
                     concatenate-radix-sort alternative for many-small-scattered
                     grants (the §10.4 primitive, second use; probes, optimisations
                     §1.2); fragment cache — content-addressed by (auth-hash,
                     plugin-version, partition, postings-epoch), frozen format, auth
                     data retained alongside for transparent rebuild (§2.3).
                     Deliberately *parametric*: build_fragment(terms, postings_snapshot,
                     watermark) takes lifecycle state as arguments and holds none,
                     which is what keeps authz ↔ lifecycle acyclic.
                     Entity space only: RowId does not appear in its API.
  tessera-store      segments, manifests, mmap and Arrow IPC columns; the Permutation
                     object — the only path between EntityId and RowId (I4), pinned to
                     a (prefix, segments-version, watermark) triple (I11); the
                     pin/snapshot manager; masked gather kernels; permutation cache.
                     Two readers, structurally separated: the *query reader*, whose
                     only column access takes a masked row-ID set (I2 as code
                     structure, §10.4); and a sealed *maintenance reader* — nameable
                     only by tessera-lifecycle — for the two sanctioned unmasked
                     paths: compaction's column rewrite and candidate-list
                     construction (itself sanctioned by §7.2's fast-path-with-exact-
                     fallback design). Its outputs are bundle artifacts, never
                     responses; the carve-out is documented at the trait, and the
                     canary-item test (plan §10.2) is what proves it never reaches
                     the response path.
  tessera-spatial    Morton codes, tile tables, viewport decomposition, per-tile
                     counting, priority sampling with the direct/candidate-list
                     crossover (§7.2). Also the *tiler* — quantisation, priority
                     hash, batch Morton sort, tile-table emit — one implementation,
                     used by streaming flushes and bulk builds alike (§6.1).
  tessera-labels     content-addressed immutable generating sets (I8); slice storage
                     with presence sets (§2.3); containment gating against M_auth
                     (I3); the fallback ladder and extractive tier with fixed
                     reference-corpus frequencies (§7.7); the servable-label cache,
                     keyed (auth-hash, plugin-version, overlay-version) so any overlay
                     change invalidates it — the cache that keeps I3 true under
                     deletion (§8.5); the invalidation queue, derived from WAL replay
                     (tombstones for deletions, change records for predicate
                     tightening) and rebuildable from it after restart.
  tessera-filter     the FilterOperand trait (§8.2); label and text-token operands
                     (valid-time reserved, prospective — design Appendix F is not
                     committed); the filter-result cache, keyed by (query, partition,
                     segments-version) and shared across principals (§8.3, §8.5).
                     Produces entity-space bitmaps; cannot see M_auth's construction.
  tessera-lifecycle  WAL (§6.2); ingest buffer, watermark, overlay (§11.2); external-ID
                     map; entity-ID allocator (single authority, durable high-water,
                     I9); flush; posting deltas; tiered merge with the re-rank
                     decorator (§11.3); tombstones; compaction; prefix publication;
                     object-store sync with digest verification.
  tessera-engine     composition root: the typed narrow API; mask composition per I1
                     (it, not authz, applies watermark patches — §6.4); the two-mask
                     model and the M_sel/frontier cache (§8.1, §8.5); router/worker
                     fan-out, merge, and the label-presence check (§2.3).
  tessera-wire       per-session handle tables and the handle encoding (§4.5); Arrow
                     IPC payload encoding. Payload builders accept Handle only —
                     EntityId is not importable here outside the handle-table module
                     itself (I10). The serialisation chokepoint of I1's second line:
                     nothing outside tessera-wire writes response bytes.
  tessera-server     axum HTTP for all three planes; config; process supervision
                     (router/worker); metrics and tracing. No query logic; depends on
                     tessera-engine's API types only — it can reach neither a column
                     nor a bitmap.
  tessera-build      the engine's batch mode (§6.1): full builds and rebuilds from
                     the declarative Parquet input contract, composing the same
                     tiler, plugin host, interner and allocator serving uses —
                     one implementation of each, which is the point.
  tessera-cli        the `tessera` binary: serve, build, status, flush, compact,
                     verify, migrate.
python/
  tessera/           the SDK, not the system: session/viewer/admin-plane clients
                     returning pyarrow Tables; tessera.build() and tessera.serve()
                     wrappers driving the binary; the supervisor.
reference/           deliberately slow, obviously correct Python oracle (plan §10.3) —
                     Python deliberately: test-only, independently derived
conformance/         the invariant suite (plan §10.2); CI-only oracles (accumulo-access,
                     DuckDB)
```

Dependency rules enforced in CI (a `cargo-deny`-style layer check): `tessera-authz` must not depend on `tessera-store` or `tessera-spatial` — permissions never learn geometry; only `tessera-store` exports `Permutation`, and `tessera-types` offers no ID conversions; the maintenance reader is sealed to `tessera-lifecycle`; oracles appear only under `conformance/`, and CI fails if the JVM or DuckDB enters the binary's dependency graph.

Cache ownership, mapping §8.5's table onto crates: mask fragments → authz; row-space permutations of fragments → store; servable-label set → labels; filter results → filter; `M_sel` and frontier → engine. Every entry carries the invalidation key from §8.5's table verbatim; the overlay version's presence in the servable-label key is the invariant-bearing one.

## 4. The five contracts

Every boundary is one of five versioned contracts. Everything else is internal and free to change.

### 4.1 The bundle format (build ↔ serve)

The unit of storage, deployment, backup and rollback. A bundle is a directory tree — local filesystem in the simple case, object-store prefix at scale — of versioned prefixes and a single mutable pointer. The mutability rule, stated precisely because r1 got it wrong: **files are immutable; the live prefix is append-only; retired prefixes are frozen.** Streaming ingest may add files to the live prefix (new segment directories, posting deltas, side-manifests); nothing ever rewrites or deletes a file except retirement of a whole prefix. The design's "the prefix name is the segment-set version" (§10.2) is refined to: the pin is the triple *(prefix, segments-version, watermark)*, and I11's discipline applies to the triple.

```
bundle/
  CURRENT                          → "v00042"    (atomic pointer; flip to publish/rollback)
  v00042/
    MANIFEST.json                  bundle-format version; data-plugin hash; partition list
                                   with required sets; slice list; declared cardinality
                                   bounds; quantisation extent (recorded so every artifact
                                   producer shares one grid); entity-ID high-water at
                                   build; per-file digests
                                   and sizes; build provenance — including the recorded
                                   generating-set choice (§7.8: prompt-sample vs full
                                   membership), which is provenance, not config
    dictionary/                    the single, bundle-level, append-only term-interning
                                   namespace (§6.1: "owned in one place"). Descriptors are
                                   opaque policy-side identifiers (hashes under the
                                   reference plugin) and carry no corpus data — the
                                   isolation boundary of §12.3 covers entity IDs and
                                   bitmaps, which never leave their partition. Recorded
                                   as a decision (D11) because the alternative
                                   (per-partition namespaces) silently breaks byte-
                                   equality-of-descriptors as the I5 mechanism.
    partitions/<required-set-hash>/
      terms/
        postings/                  per-term Roaring over entity space, frozen (base tier)
        deltas/<segments-version>/ per-flush posting deltas (§6.3); folded into base at
                                   the next prefix publication
        pairs.arrow                exploded (entity_id, term_id) relation (§6.3)
      entities/
        nodes/                     membership bitmaps, per-slice bboxes, per-segment row
                                   ranges, term distributions
        labels/                    label text + tier + generating-set slice + presence set
        vocab/                     CSR per-item vocabulary vectors (extractive tier)
        external-ids/              caller external-ID → entity mapping (admin plane only)
      slices/<slice-id>/
        permutation.bin            entity→row and row→entity, sentinel for absent (§5.1)
        segments/<seg-id>/
          columns/*.arrow          hot columns, Morton order, priority tiebreak (§10.3)
          morton.u64 · tiles.bin · candidates.bin
      text/                        token → Roaring postings (entity space), base + deltas
      vectors/                     sidecar, chunked (§8.3); optional
    SEGMENTS-<n>.json              side-manifests: each is *complete* (every live segment
                                   and delta with digests), numbered monotonically;
                                   written only after the files it names are durable
```

**Read protocol.** A reader (a booting node, a syncing replica) resolves `CURRENT`, takes the highest `SEGMENTS-<n>.json`, verifies every referenced file exists with matching digest and size, and only then declares ready. Local publication is write-then-rename; object stores get no rename, which is exactly why side-manifests are numbered, complete and digest-bearing — a half-synced prefix fails verification instead of serving wrong geometry. `readyz` gates on this verification.

**Refusals.** The engine refuses a bundle whose format version is newer than it knows, and refuses to serve if the manifest's data-plugin hash differs from the configured plugin's — that mismatch is a full-reindex event (§6.1), not tolerable drift.

**Rollback.** Flipping `CURRENT` backwards abandons segments streamed into the newer prefix; those batches survive in the WAL (§6.2) up to its retention window and are replayed by `tessera migrate --replay` into the restored prefix. Rollback is therefore an operator action with a stated, bounded data-loss-and-recovery story rather than a silent one.

The manifest and side-manifests are JSON, deliberately: they are what an auditor reads first, and the cost at this frequency is nil. Hot structures are binary.

**The byte-level definition lives in the contracts specification**, whose §0.3 records four refinements adopted over this sketch after its own review cycle: portable Roaring in the bundle with frozen mirrors as engine-local cache; a single-direction permutation, storing `entity_to_row` only *(annotated 2026-07-30: this read "row→entity is the `entity_id` column", the column contracts r6 removes. Row→entity is now the inverse of the keyed `tessera_id` bijection — a pure function, no second array either way, so the refinement is unchanged; see contracts §0.3 deviations 2 and 6, design r22 §5.1)*; `tiles.bin` and `candidates.bin` de-contracted as derived caches; and per-partition side-manifests carrying per-partition *(n, watermark)* pins, with tombstones and the deny set inside their own partition directory. Where this tree and that document differ, it is right. Contracts §0.3 has since grown to nine deviations; the five later ones (5–9) are r5/r6 changes this sketch does not anticipate.

### 4.2 The service API (the query surface)

HTTP on every surface, Arrow IPC bodies for anything columnar, JSON for control messages. No gRPC: one framework (axum) serves all three planes, browsers reach the viewer plane without a proxy, and the payloads that matter are Arrow either way.

**Viewer plane** — token-bearing, the only untrusted surface. Five verbs, and the intent is that the list is complete (Appendix H: the surface *is* what makes the leak register enumerable):

| Verb | Request | Response |
|---|---|---|
| `GET /v1/meta` | — | slices, coordinate extent, max tile depth, declared-scalar schema, contract versions, available filter-operand *names*, and the C11 containment-filtered label vocabulary — the response's one data-derived field, and it is gated on `M_auth` |
| `POST /v1/viewport` | slice, zoom, tile range or bbox, filter set, k (server-capped), optional session pin | Arrow: per-tile exact masked counts, matched-and-visible alongside total-visible (§8.1); sampled points (handle, x, y, declared scalars); session pin |
| `POST /v1/labels` | slice, viewport, filter set, optional session pin | frontier nodes (as handles) with at most one gated label each + tier (§7.6–7.8) |
| `POST /v1/region` | slice, polygon/box, filter set, optional session pin | exact masked count; sampled preview; masked breakdowns (§7.4) |
| `POST /v1/items/{handle}` | drill-down; optional session pin | item detail; further selections over the same range |

**Pins are real request parameters.** A response carries a *session pin* — an opaque, per-session-scrambled reference to router-side pin state. Under fan-out that state is a **vector**: one *(prefix, segments-version, watermark)* triple per reachable partition, captured from each worker at serve time, because segments-versions and watermarks are per-worker facts and a single triple cannot name them. The client sees one opaque value either way. A client may present it on subsequent requests for cross-request consistency (drill-down after a flush); a drained pin is **rejected** with an explicit `410 pin-expired` (I11: rejected, not silently applied), and the client re-queries unpinned. Because even a scrambled pin's *rate of change* signals corpus activity the viewer may not be authorised to infer, this is a residual channel: proposed as a new Appendix C entry (Appendix R, action 3), with the note that it is C14-like — low severity, and partially maskable by rotating the scrambling per session.

r1's `filters/validate` verb is **removed**: operand discovery moved into `/v1/meta`, and a text-token vocabulary echo would have been an unmasked corpus-wide aggregate — an I2 bug by the design's own rule. Text operands acknowledge nothing about the vocabulary; an unmatched token simply yields an empty operand.

**Session plane** — a separately bindable listener (loopback by default; the app tier typically reaches it over the internal network), its own credential, exactly two verbs:

| Verb | Purpose |
|---|---|
| `POST /session/authorise` | auth data → `{token, expires_at}`; content-addressed mask reuse (§2.3) |
| `DELETE /session/tokens/{id}` | revocation backstop |

The scoping is least-privilege, not theatre, and what a session credential is *worth* depends on the plugin's auth-data shape. With a **bare-claims** plugin (the reference plugin's category list), the §6.1 contract trusts whatever is presented, so the credential is read-everything — mitigated by scope, network placement and audit logging of authorise calls, not pretence. With a **verifiable-credential** plugin (§4.3), auth data is a signed, principal-bound assertion the plugin cryptographically verifies, and the credential degrades to "may submit credentials": authority derives from possessing a user's assertion, and compromising the app tier yields the assertions it sees in flight, not instant authority over every principal. Production deployments should prefer the second shape.

**Admin plane** — unix socket (loopback + credential on Windows) with two credentials: *operator*, and *build* for the unmasked iteration endpoint. Identifiers on this plane are the **caller's own**: external item IDs and the caller's stable node IDs (§2.4). Viewer-session handles never appear here (they are session artifacts and this plane has no session); entity IDs never appear anywhere.

| Verb | Purpose |
|---|---|
| `POST /control/ingest` | Arrow batch: **external_id**, x, y, predicate, cluster node id, scalars. Carries a caller batch id; acked only after WAL fsync; replay of an acked batch id is idempotent; duplicate external_ids are rejected with the conflict list |
| `POST /control/changes` | predicate change / deletion / suppression, addressed by external_id → overlay entries (§11.2); same WAL-then-ack contract |
| `POST /control/labels` | label + declared generating set (as external IDs, resolved on submission) |
| `GET /control/labels/invalidated?cursor=` | pull queue of labels needing regeneration (§7.6); rebuildable in full from WAL replay — deletion-driven entries from tombstones, *predicate-tightening* entries from replayed change records — so a restart never silently loses either kind of notification |
| `GET /control/nodes/{node-id}/term-distribution` | for the caller's labeller (§2.5) |
| `GET /control/nodes/{node-id}/members?cursor=` | **build credential only**; unmasked by design; returns external IDs |
| `POST /control/allocate-ids` | leases an entity-ID range; used by `tessera build` when rebuilding against a live deployment (§6.6), available to any external artifact producer |
| `POST /control/flush` · `POST /control/compact` | force lifecycle actions |
| `GET /control/status` | watermark, overlay size, segment counts, over-bound warn count, WAL depth, pins held |

**Backpressure and limits.** `/control/ingest` returns `429 + Retry-After` when the WAL or buffer is at bound. `/control/changes` with *deny* disposition (deletion, suppression) is **never refused for capacity** — refusing a security operation for load is fail-open; deny entries are tiny and always accepted, and a saturated overlay instead schedules mask rebuilds and raises an alarm. Viewport `k` is server-capped; region vertex counts and cursor page sizes are bounded.

Plus `/healthz`, `/readyz` (ready = digest-verified sync + pinned + plugin loaded + workers ready), and Prometheus metrics on an admin-trusted port (§9). Failure semantics everywhere: **fail closed** — any error in mask construction, composition or containment returns an error, never a partial result (§10.6).

### 4.3 The plugin ABI (policy ↔ core)

A WASM module under wasmtime with no WASI capabilities (plan §2.3), exporting `terms_of_label`, `terms_of_auth` and `declared_bounds` (§6.1). Descriptors are opaque byte strings; the engine owns interning in the single bundle-level namespace. The module hash keys both blast radii: auth-function hash into the fragment cache key, data-function hash into the manifest. The plugin host runs in the router (it sees auth data and descriptors, never corpus data). The built-in access-expression plugin ships compiled in (`plugin = "builtin:access-expressions"`); `builtin:open` exists for demos and tests; out-of-process-over-a-pipe is the documented fallback for policy engines that cannot target WASM.

**Verifiable auth data.** Auth data need not be bare claims: a plugin may accept a signed, principal-bound assertion (JWS, SAML, an attribute certificate) and verify it against trust anchors **embedded in the module** — signature verification is pure computation and needs no capability, and embedded anchors mean rotation is a module-hash change, which correctly invalidates every cached mask. Three consequences the contract absorbs explicitly:

- *Expiry is the host's job.* The sandbox has no clock, deliberately (determinism). `terms_of_auth` returns terms plus an optional `not_after` extracted from the credential; the host — which has a real clock, outside the sandbox — refuses expired assertions and clamps token lifetime to min(backstop, `not_after`). The plugin stays a pure function.
- *No online revocation.* I6 and the capability-free sandbox forbid OCSP/CRL fetching; revocation is bounded by assertion lifetime, so the pattern pushes toward short-lived assertions.
- *The mask cache gets a second key.* Signed assertions carry volatile bytes (nonces, timestamps, signatures), so §2.3's auth-data-hash key would never hit and every login would pay a full mask build. The canonical dedup key becomes the hash of the **satisfied term set** — all the mask actually depends on — with the auth-data hash retained as a fast path that also skips the plugin run when auth data is byte-stable. Proposed back to the design as a §2.3 refinement (Appendix R, action 5).

### 4.4 The filter contract (extension ↔ core)

Internally a trait; externally the operand set of §4.2. An operand receives query parameters and an optional candidate bitmap, returns an entity-space bitmap, and must be order-independent: threshold semantics, never top-k (§8.2); ranked forms apply k after intersection. Text is token postings over the same Roaring machinery as the term index — boolean AND/OR, no scoring, no second index technology in the trusted computing base. Deliberately less than a search engine: C9 is closed *by scope*, and a full-text library would reopen it as a standing review obligation for a capability (ranking) the design forbids. Phrase or fuzzy matching, if ever wanted, enters as build-time tokenisation, not a query-time engine. The valid-time operand is reserved but not committed (design Appendix F's own status).

### 4.5 The wire format (serve ↔ client)

Arrow IPC record batches; schema versioned in the payload header; shared by the browser client and the Python SDK (`pyarrow` reads it natively). All identities are per-session `u32` handles (I10, byte-scan tested per plan §10.2). Under fan-out, *item* handles are minted by the worker that owns the item and encoded through a per-session keyed permutation whose key the router issues at token creation: the router can decode a handle to (partition, worker-local reference) for routing `/v1/items/{handle}`, while the client sees values with no stable structure — in particular no learnable partition grouping across sessions. The load-bearing constraint, stated because every other sentence here survives its violation: **the decoded worker-local reference is an index into the worker's per-session handle table, never an entity ID** — putting the entity ID in the permutation's plaintext would ship corpus identifiers to the router (and, keyed weakly, to the wire) while matching the rest of this section to the letter. Workers therefore never emit entity IDs even to the router, and the router never holds a table of them.

*Node* handles are the router's to mint, from the caller's stable node IDs (§2.4): a frontier node may span partitions, workers report per-node masked counts against those caller IDs (caller node IDs are not entity IDs and appear only in authorised responses), and the router merges before minting one per-session handle per frontier node. Same keyed-permutation treatment, same byte-scan obligation.

## 5. The read path, briefly

The read path is §2.6 and is not restated. What this document adds is *where each step lives*: authorise steps 1–5 in `tessera-authz` + `tessera-plugin`, fanned per §2.2; retrieve step 1 (pin) in `tessera-store`, step 2 (I1 composition) in `tessera-engine` — composition, not authz, because the overlay and watermark are lifecycle state and authz is parametric (§3); step 3 in `tessera-filter`; step 4 in `tessera-store` (the permutation — the only meeting point of the ID spaces); steps 5–8 in `tessera-spatial`; step 9 in `tessera-store` (masked gather); step 10 in `tessera-wire`; step 11 in `tessera-labels`. The pin is a value threaded by ownership through every call — no ambient "current version" static exists (I11).

## 6. Lifecycle: durability, ingest, change, compaction

### 6.1 Two write paths, one engine

**Bulk builds are the engine in batch mode.** `tessera build` (the `tessera-build` crate) consumes the declarative input contract — Parquet with external_id, x, y, cluster node id, access expression and declared scalars, plus the hierarchy and label files — runs the data plugin, derives partitions and required sets, Morton-sorts, writes a complete new prefix, and publishes by `CURRENT` flip (§14). r1–r3 placed this in Python, following the implementation plan's stack line; r4 amends it (D4), and the plan's own rationale dissolves on inspection — it justified Python by interop with UMAP, HDBSCAN and Toponymy, but those belong to the *caller's* model pipeline, out of scope per §2.1; the build step only reads their outputs, which is Parquet, which arrow-rs reads natively. What the move buys: **one tiler** instead of two carrying a bit-for-bit agreement obligation; **one plugin host** instead of two (a Python-side wasmtime running `terms_of_label` would have been a second implementation of the I5-critical path — a divergence hazard in its own right); one interner, one allocator; and a pure-Rust consumption path for shops where Python is unwelcome. `tessera.build()` in the wheel is a wrapper that drives this binary.

**Streaming ingest belongs to the serving engine.** The buffer, watermark and overlay (§11.2) are serving state — the watermark participates in every I1 composition — so their owner owns ingest. This resolves a genuine ambiguity between the plan's "Python produces segments" and the design's §11.2.

Both paths call the same tiler (`tessera-spatial`), interner (`tessera-authz`) and allocator (`tessera-lifecycle`); there is nothing left to diverge. The manifest still records the quantisation extent so every artifact producer, present or future, shares one grid. The differential oracle (`reference/`, plan §10.3) checks both paths against the deliberately slow Python reference — which stays Python precisely because it is test-only and must be independently derived.

### 6.2 Durability: the WAL and the ack contract

r1 had no durability story; this section is the fix, and one line of it is security-relevant rather than operational: **an unpersisted overlay fails open** — a *deny* entry (deletion, administrative suppression) that evaporates on restart makes suppressed items visible again.

Each partition worker (or the single process) keeps a write-ahead log. `POST /control/ingest` and `POST /control/changes` ack **only after fsync** of: the batch rows (with caller batch id), the change entries, and the entity-ID allocator's advanced high-water. **WAL rows record their allocated entity IDs.** Replay — crash recovery and the §4.1 rollback replay alike — reuses the recorded IDs and never re-allocates, or entity-ID stability across rebuilds (§5.1) and every entity-space structure referencing those IDs would silently break. Restart replays the WAL: the in-memory buffer is rebuilt, the overlay is reconstructed in full, and the allocator resumes past its durable high-water (I9 under crash-replay). Acked-batch-id replay is idempotent; the external-ID map (also WAL-covered) is what makes duplicate detection possible at all.

**Two retirement rules, deliberately different.** A *WAL entry* retires once its data is segment-durable, subject to the rollback-replay retention window (§4.1). An *overlay entry* is governed by §6.5's rule, which is strictly later for deny dispositions — conflating the two is the fail-open r2's reviewers warned against, so the distinction is stated here at the point of temptation.

### 6.3 Ingest, flush and posting deltas

`/control/ingest` (routed per partition after required-set derivation; alarm on new-partition discovery): terms resolved via the plugin host and interned; entity IDs allocated (batch-ordered by term signature for posting compression, §11.1, never Morton order, C6); coordinates quantised against the manifest extent; priorities derived; batch buffered. Flush — size or age, whichever first — writes an immutable segment directory, writes a **posting delta** (term → Roaring over the batch's ID range) rather than touching the frozen base postings, appends the same for text tokens, and publishes a new complete `SEGMENTS-<n>.json`. Items exceeding the declared per-item bound are **indexed anyway and warned** — the r16 decision replacing exclusion (§6.2): a monotone predicate with more terms intends broader visibility, and a resource guard must not produce an authorisation-shaped outcome. Warn count and identities surface in `/control/status` and metrics as a data-quality signal.

### 6.4 The watermark patch, with a mechanism

r1 said "OR the flushed contribution into live masks", which is impossible as stated: fragments are content-addressed, shared across tokens, and served as immutable frozen views. The actual mechanism:

- Fragment identity includes a **postings-epoch** (the segments-version its postings reflect). A flush creates a new epoch; it does not touch any existing fragment.
- Until a token's fragment is refreshed, flushed entities are simply *live*: they sit at-or-above the fragment's watermark, so I1's composition (`M_auth = (fragment \ L) ∪ direct_eval(L)`) already covers them exactly. Correctness never depends on patching.
- Refresh is **lazy and per-fragment**: on next use past a staleness threshold (or when `L`'s cost bound trips), the engine builds epoch *n+1* as `fragment@n ∪ delta-postings(satisfied terms, epochs n+1..)` — the monotone patch of §11.2, now costed: one OR per delta per live fragment, amortised by laziness so a flush never stampedes the fragment cache. The old epoch stays frozen for draining pins.
- **The effective watermark is always the fragment's own.** A request's I1 composition uses the watermark of the fragment epoch it actually loaded — an entity between that watermark and the store's latest is then in `L` and directly evaluated, never in neither set. This holds for pinned requests too: a pin fixes row-space geometry only, never authorisation state (lifecycle design §2.3), so the `W` recorded in a session-pin vector is advisory — composition never reads it.

**"Correctness never depends on patching" is a claim about *flushed* entities** *(r5; scope qualifier, not a retraction — see design §11.2)*. Every bullet above concerns an entity that already sits in a segment: it has a row, so `L`-membership and direct evaluation put it into the answer exactly. An entity still in the **buffer** has no row in any segment, and every viewer verb asks a row-space question — count the rows in this tile range, the rows in this density cell, the rows this selection picks. The composition resolves such an entity's verdict and then has nowhere to put it, so it contributes to nothing regardless of what `L` says. **Flush is therefore the ingest-visibility mechanism, not merely the thing that bounds segment count**, and a phase that ships the buffer without it has built durability and authorisation state rather than queryable ingest — which is exactly Phase 1's position, and is why the acknowledgement contract of §6.2 is a durability receipt rather than a visibility promise. Nothing above is wrong; it simply starts one step later than it reads.

### 6.5 The overlay

Predicate changes, deletions and suppressions become overlay entries with *evaluate* or *deny* dispositions, term sets inline (§11.2), owned per partition by its worker — the router never holds them (they carry entity IDs). Overlay size is a first-class metric: I1's composition cost is linear in it, and its configured bound triggers fragment-refresh scheduling rather than refusal (§4.2's backpressure asymmetry). Deletion additionally removes the entity from postings (via a delta-tier tombstone honoured by the postings reader), enqueues affected labels for invalidation (§7.6), and leaves a row tombstone for compaction. IDs are never reused (I9; the allocator is fuzzed for exactly this).

**The deny-retirement rule.** Because the §6.4 epoch advance is a pure union, it can never *remove* an entity from a fragment: a deleted or suppressed item's invisibility rests entirely on its deny entry until every fragment that predates the deletion is gone. So a deny entry may leave the overlay only when **no servable fragment epoch predates the deletion** — that is, when every cached fragment across every live token has been rebuilt from post-deletion postings (compaction may force full fragment rebuilds, rather than monotone advance, precisely to bound how long that takes). Retiring a deny entry on any earlier trigger — segment durability, compaction of the row tombstone, WAL retirement — reopens visibility for tokens still on old epochs, which is the fail-open this document exists to forbid. The conformance suite carries a test for exactly this window (Appendix R, action 2).

### 6.6 Merging, compaction, and the two-writer problem

The merge scheduler follows the Lucene-derived shape (§11.3, plan §2.4): tiered natural merges (2 MB floor, ~10 per tier, 5 GB max merged), deletes-triggered merges at 20% tombstones, forced compaction; Morton re-ranking as a decorator above 2^18 rows and always on forced merges. A compaction rewrites permutation, tile table, candidate lists and columns, **folds posting deltas into the base tier**, drops compaction-tombstoned entries, and publishes a new prefix. Masks, node memberships, generating sets and the term index in entity space are untouched (§11.3).

Two writers touch the bundle — the engine's serving mode (streamed segments, deltas) and its batch mode, `tessera build` (full prefixes) — so the races are named and owned. *(r5: "the Python builder" corrected to name the crate; D4 moved the builder into the engine in r4 and this sentence was not carried over. One implementation, two modes — §6.1.)*

- **Compaction vs concurrent flushes.** Compaction snapshots a segments-version, rewrites that set, and at publication *carries forward verbatim* everything accepted after its snapshot: segments, deltas, **post-snapshot tombstones, the active suppression set, and unfolded overlay entries** — folding away only snapshot-covered state (a post-snapshot tombstone folded away while its entity survives in the rebuilt base would be fail-open; lifecycle design §5.3). Compaction's fold obligation also extends to *evaluate* overlay entries: it rewrites affected entities' postings from the term sets those entries carry, which is what makes predicate changes eventually retirable (lifecycle design §3.4). Flushes never block.
- **Bulk rebuild vs live ingest.** The builder (`tessera build`, its own process, possibly its own host) records the WAL positions at snapshot; after `CURRENT` flips to its prefix, the serving engine replays WAL entries past those positions into the new prefix as ordinary ingest. The catch-up window is the build duration; visibility latency during it degrades gracefully rather than data being lost.
- **Entity-ID allocation has one authority, and under fan-out it has an address: the router.** The ID space is global across partitions (an item keeps its entity ID through a §12.5 partition move, or generating sets and label references to it would break). The router owns the counter, durable in a small allocator journal fsynced ahead of any lease (I9); workers lease contiguous ranges from it and record per-row allocations in their own WALs (§6.2). A counter high-water is a number, not entity-space data — holding it at the router does not breach §2.2's isolation rule, which covers entity IDs *of items*, bitmaps and columns. A `tessera build` running against a live deployment leases ranges via `POST /control/allocate-ids`; the bootstrap build (no serving process yet) allocates from zero and records the high-water in the manifest, which seeds the router's journal on first boot.
- In-flight requests drain against their pinned triple; the pin manager retains retired prefixes until drained (I11).

**Open: how a large batch lands into a *live* bundle** *(r5; recorded as an open architectural question at the owner's direction, deliberately not settled here)*. The two bullets above cover a full rebuild racing live ingest, and §6.3 covers a steady arrival stream. Neither is quite the shape an operator reaches for when loading ten million new items into a bundle that is already serving. Three candidate shapes, with what each costs:

- **One (or few) very large `/control/ingest` batches.** No new mechanism: it is the designed streaming path used at a size it was never explicitly sized for. It also happens to be the shape that preserves design §11.1's signature runs, since the sort scope is the batch. Needs flush to exist, and needs a documented batch-size floor to stop being accidentally defeated by a client that chunks its upload.
- **A first-class builder append.** Teach batch mode to read a live bundle's high-water and external-ID map, emit a new segment plus a posting delta under a fresh `SEGMENTS-<n>`, and publish. Fastest for bulk, and reuses machinery that already exists — but it adds a second writer to the segment-and-manifest surface this section deliberately gave to the lifecycle thread, which is the race the rest of §6.6 exists to avoid re-opening.
- **The rebuild path above, built properly.** Snapshot WAL positions, lease IDs via `POST /control/allocate-ids`, carry entity IDs forward, write the next prefix, flip, replay the catch-up window. Most faithful to what is already written; also the most work, and it is the only one of the three with no implementation surface at all today.

The choice is deferred rather than absent; whoever settles it should note that the first shape needs nothing this document does not already promise, and the other two each add a writer or a mode.

### 6.7 Reindex and repartition

A data-plugin change or compartment-map change is a full rebuild through `tessera build` into a fresh prefix *(r5; was "the Python pipeline" — same D4 carry-over as §6.6)* — expensive, not risky, reversible (§12.5); the manifest hash check (§4.1) prevents serving old artifacts under a new plugin. A single item's partition move is the first-class two-step of §12.5: *deny* overlay entry in the source, ingest in the destination, both WAL-covered.

### 6.8 Backup, restore, upgrade

The bundle plus the WAL is the recovery story: immutable-once-retired prefixes make object-store versioning or `rsync` sufficient; restore is pointing a fresh node at a prefix (digest-verified). Token and fragment state are deliberately *node-local and disposable* — tokens re-derive from auth data (§2.3), so node replacement costs re-authorisation and nothing else; this is distinct from in-process eviction, which is transparent by the retained-auth-data rule. Engine upgrades roll forward on the same bundle (format version permitting); format bumps ship a `tessera migrate` that writes a new prefix rather than editing one.

## 7. Configuration

One file, `tessera.toml`. The philosophy has a security edge, borrowed from Appendix E's "no code path supplies a default": **performance knobs default; disclosure controls do not.** A config missing a disclosure control fails to start, naming the design section that explains the knob — the config file doubles as the deployment's disclosure-review checklist.

```toml
[bundle]
path = "s3://acme-maps/tessera"        # or "./bundle" — a local directory is the simple case
cache = "/var/lib/tessera"             # NVMe sync target
wal  = "/var/lib/tessera/wal"

[plugin]
module = "builtin:access-expressions"  # or "file://policy.wasm"
# declared bounds come from the plugin; overrides here are refused, not merged

[disclosure]                            # no defaults; absence is a startup error
min_visible_members = 25               # §7.5 — reviewed as a security control
token_max_lifetime = "1h"              # §2.3 backstop, r17 default; the caller's refresh policy governs

[serve]
viewer  = "127.0.0.1:7407"             # loopback by default; binding wider is an explicit act
session = "127.0.0.1:7408"             # authorise/revoke only; the app tier's surface (§4.2)
control = "unix:/run/tessera/control.sock"   # admin plane; loopback TCP + credential on Windows
max_k = 200
# session/operator/build credentials via files or env, never inline

[ingest]                                # defaults shown; all optional
flush_max_items = 100_000
flush_max_age  = "60s"
wal_retention  = "72h"                  # bounds the rollback-replay window (§4.1)
overlay_soft_limit = 500_000            # beyond: fragment refresh scheduled, alarm raised

[merge]                                 # defaults from §11.3; rarely touched
```

Two things r1 put here that do not belong: `label_gating` — the prompt-sample vs full-membership choice (§7.8) is made upstream and *recorded in the manifest as build provenance*, because a config knob the engine doesn't act on is a compliance fiction; and any compartment configuration — the compartment map is schema (§12.5), discovered from data, and config can neither create nor destroy a partition.

## 8. Python packaging and the developer path

### 8.1 The wheel

`pip install tessera` delivers the `tessera` binary (per-platform wheels via maturin), the SDK (session/viewer/admin clients, `pyarrow`-native) and the supervisor. The wheel is packaging, not architecture: everything it does rides the same binary and HTTP surfaces any other language uses directly. No Docker, no JVM, no external services; the object store is optional (a bundle is a directory). Platform matrix: Linux and macOS first-class; Windows supported with the loopback admin plane. Supervisor hygiene, because notebooks are the hard case: the child binds **port 0** by default and reports its ports back over the supervision pipe; the same pipe is a watchdog — the engine exits when its supervisor fd closes (covers macOS/Windows where `PDEATHSIG` doesn't), so a killed kernel never orphans a server; a pidfile beside the WAL detects and reaps stale processes on the next `tessera.serve()`.

### 8.2 Day one, end to end

```python
import tessera

# 1. Build a bundle from the model pipeline's outputs (Parquet in, artifacts out).
#    A thin wrapper driving `tessera build` — the engine's own batch mode (§6.1).
bundle = tessera.build(
    points="points.parquet",            # external_id, x, y, cluster_id, access, <scalars…>
    hierarchy="hierarchy.parquet",      # caller's cluster tree, stable node ids
    labels="labels.parquet",            # label text + declared generating sets
    plugin=tessera.plugins.access_expressions(dimensions=DIMS),  # or a .wasm path
    out="./bundle",
)

# 2. Serve it. Spawns the Rust process; Python supervises.
srv = tessera.serve(bundle, disclosure=dict(min_visible_members=25,
                                            token_max_lifetime="1h"))

# 3. The integrating backend authorises its authenticated user…
token = srv.authorise({"categories": user_categories})

# 4. …and either hands the token to a client pointed at srv.viewer_url,
#    or queries directly: results arrive as pyarrow Tables.
tiles = srv.viewport(token, slice="2026-07", bbox=(0, 0, 1, 1), zoom=6)
```

In production the same verbs appear as `tessera serve -c tessera.toml` under systemd or a container, `POST /session/authorise` from the backend's session middleware (session credential only — the app tier never holds admin), and the viewer plane behind the organisation's TLS termination. The notebook and production run the same binary against the same bundle; there is no "dev engine".

### 8.3 What the integrator owns

The §2.4 contract as a checklist, because it is the part the service cannot check: consistent plugin functions (I5 — run the shipped property harness against your policy oracle in CI), a stable projection across slices, stable cluster node identity, honest generating sets (C12), unique external IDs, and a token-refresh policy (I6). `tessera verify` runs what *is* mechanisable: digest verification, manifest/plugin-hash agreement, permutation bijectivity, generating-set slices resolving to live entities, presence-set consistency across copies, declared-bounds conformance.

## 9. Observability and failure

Metrics map to named risks so dashboards read in the design's vocabulary: mask fragment build latency (§6.3 budget), fragment cardinality and frozen size (residency ceiling), overlay size (I1 cost), watermark lag (visibility latency), WAL depth and fsync latency, over-bound warn count (§6.2 r16), segment and delta counts per slice (fan-out), **posting fragmentation per partition** (§11.1's signature runs erode with every small ingest batch and nothing repairs them, so the erosion is only visible if measured — postings-per-container over base plus deltas, and run ratio against the 1/(1−p) baseline; it is also the trigger condition for the deferred index-ordinal split in plan §14), invalidation queue depth (§7.6), partition-creation events (**alarm**, §12.4), per-tile crossover ratio (validates the 5% figure in production), and C4's timing spread — measured from day one so "quantify before treating as acceptable" actually happens. The metrics port is **admin-trusted and never viewer- or session-routable**: overlay size, overflow counts and fragment cardinalities are unmasked corpus quantities, and any per-auth-hash label on them stays off shared dashboards.

Tracing spans follow §2.6's step names verbatim. Logs never contain tokens, auth data, entity IDs or descriptors; the conformance suite's byte-scan runs against log output as well as wire output.

Failure is closed everywhere: a worker that cannot verify its partition marks itself unready rather than serving partial data; a router that cannot reach a *reachable* partition fails the request; a drained pin is rejected, not reinterpreted.

## 10. Decisions

Recorded in the design's own style — each will be re-proposed otherwise. D1–D10 stand from r1; D6 was amended in r2; D11–D16 are new in r2; D1, D2 and D4 are amended in r4 at the owner's direction.

1. **Rust-native end to end; Python is a first-class consumer, never a component** *(amended r4)*. Every surface is language-agnostic HTTP+Arrow; the wheel ships the binary, an SDK and a supervisor. Rejected: PyO3 in-process serving — forfeits process-level compartment isolation, entangles the TCB with a host interpreter, saves one process. Rejected (r4): a Python-implemented build pipeline (see D4).
2. **`authorise` on its own session plane** *(amended r4)*. Off the admin plane because the app tier should hold a credential that mints sessions, not one that ingests or deletes. Off the viewer plane: with a bare-claims plugin this is structural — an untrusted surface minting capability from unverifiable claims is a bypass — and with a verifiable-credential plugin (§4.3) it softens to defence-in-depth plus cost control (mask construction is the system's most expensive operation; an openly reachable minting endpoint is a resource-exhaustion surface). The session plane is the default in both cases; a verified-client authorise profile is possible later without redesign, and the door is recorded as open rather than closed on principle. What the credential is worth depends on the plugin shape (§4.2): read-everything under bare claims, submit-credentials under verification — prefer the latter in production.
3. **HTTP + Arrow IPC on every plane; no gRPC.**
4. **Both write paths live in the engine; bulk build is `tessera build`** *(amended r4)*: one tiler, one plugin host, one interner, one allocator — Python drives, never implements. Supersedes r2's bit-for-bit dual-tiler differential-test obligation: there is nothing left to diverge. The plan's Python-build rationale (UMAP/HDBSCAN/Toponymy interop) belongs to the caller's out-of-scope model pipeline (§2.1), not to artifact production (§6.1).
5. **Text filtering is token postings on the existing Roaring machinery.** Rejected: any full-text engine in the TCB.
6. **The bundle is versioned prefixes + `CURRENT`** *(amended)*: files immutable, live prefix append-only, retired prefixes frozen; the pin is *(prefix, segments-version, watermark)*; digest-bearing complete side-manifests; stated rollback-replay story.
7. **Router/worker processes for partitions; single process when none exist.**
8. **Tokens are opaque references to server-side state.**
9. **Disclosure controls have no defaults.** (`label_gating` removed from config — the §7.8 choice is recorded as manifest build provenance, not a knob the engine pretends to act on.)
10. **One binary for daemon and CLI.**
11. **One term-interning namespace, bundle-level, held at the router** *(new)*. Descriptors are opaque policy-side identifiers; the §12.3 isolation property covers entity IDs and bitmaps, which never leave workers. Rejected: per-partition namespaces — they dissolve byte-equality-of-descriptors as the I5 mechanism and break required-set gating at the router.
12. **Label presence sets are first-class** *(new)*: generating sets stored as per-partition slices, each carrying the label's full partition-presence set; the router withholds unless presence ⊆ reachable and every presence partition affirms containment. This is the I13 merge done right, and the presence registry needs an Appendix C entry.
13. **WAL-before-ack durability; deny-disposition changes are never load-shed** *(new)*: an unpersisted overlay fails open, so deletion/suppression must be both durable and always accepted.
14. **Caller-supplied external IDs are the admin-plane identity** *(new)*: they give `/control/changes` an addressee, ingest an idempotency story, and the admin plane an identifier that is neither an entity ID (I10) nor a session handle.
15. **Watermark patching is lazy epoch advance, never in-place mutation** *(new)*: correctness comes from I1's live-set composition; patching is an amortised cost optimisation (§6.4).
16. **Handles are worker-minted, per-session-keyed** *(new)*: routable by the router, structureless to the client, and entity IDs cross no process boundary.

**To verify before Phase 1 commits:** `croaring` frozen-view support; maturin binary-shipping ergonomics for the supervisor pattern; PyPI name availability (decide the fallback early).

**Deliberately not decided here**, deferred with their owners: sharded index placement (§13.3 — measurement), retroactive revocation across slices (§9 — policy), prompt-sample vs full-membership gating (§7.8 — Phase 3, recorded in the manifest either way), the mask-build tier alternative (plan §14), and everything in §16.

## Appendix R — Review record

r1 was reviewed by two independent reviewers with no stake in the draft: one against the design's invariants and source documents (verdict: sound-with-fixes), one for engineering and operational soundness (verdict: needs-rework in the process-model and lifecycle sections). r2 resolved their findings and was then checked by a third, independent verification pass, finding by finding (verdict: ready-with-minor-fixes — 23 of 24 resolved, one partial, plus three specification gaps the rewrite itself introduced). r3 closes those: the **deny-retirement rule** (§6.5 — a deny overlay entry outlives every fragment epoch predating its deletion; the monotone epoch advance can never remove an entity, so retiring the entry early is fail-open), the pinned watermark bound to the fragment epoch and the pin as a per-partition **vector** under fan-out (§6.4, §4.2), the allocator's address under fan-out (router-owned journal, worker range leases, WAL rows recording allocated IDs so replay never re-allocates — §6.6, §6.2), predicate-tightening invalidations made WAL-replayable alongside tombstones (§4.2), router-minted node handles and the worker-local-reference-is-not-an-entity-ID constraint (§4.5). **r4** applies three owner-directed amendments, argued in place: the bulk builder moved from Python into the engine (`tessera build` — D4; kills the dual-tiler and dual-plugin-host divergence hazards outright), the Python package reframed as SDK + supervisor over language-agnostic surfaces (D1), and `authorise` split onto a dedicated session plane so the app tier's credential cannot ingest or delete (D2). All blocker and significant findings from the two r1 reviews are resolved in r2: the I13 label merge (→ §2.3, D12), bundle mutability and side-manifest protocol (→ §4.1, D6), the watermark patch mechanism and posting deltas (→ §6.3–6.4, D15), WAL durability and the fail-open overlay (→ §6.2, D13), external IDs and ingest idempotency (→ §4.2, D14), dictionary ownership (→ D11), overlay/watermark ownership under fan-out (→ §2.2, §6.5), the entity-ID authority (→ §6.6), cache-tier crate homes (→ §3), control-plane identifiers (→ §4.2), worker lifecycle and handle namespaces (→ §2.2, §4.5), the missing metadata endpoint (→ `/v1/meta`), maintenance-reader carve-out (→ §3), checksums and the read protocol (→ §4.1), supervisor hygiene (→ §8.1), backpressure asymmetry (→ §4.2), and the pin contract (→ §4.2). The `filters/validate` verb was removed as an I2 leak.

**r5** applies the corrections from an audit of the Phase 1 ingest implementation against this corpus, run before Phase 2's streaming path is designed on top of it. Four changes, all scope qualifiers or omissions rather than reversals. **§6.4 gains the qualifier that "correctness never depends on patching" is a claim about *flushed* entities**: an item still in the buffer has no row, every viewer verb asks a row-space question, so the composition resolves its verdict and has nowhere to put it — flush is the ingest-visibility mechanism, not merely the bound on segment count, and §6.2's acknowledgement is a durability receipt rather than a visibility promise (companion: design §11.2, also r23). **§6.6 records "how a large batch lands into a live bundle" as an open architectural question** with three candidate shapes and their costs, deliberately unsettled at the owner's direction; the two existing bullets cover a rebuild racing ingest and a steady stream, and neither is that shape. **§9's metric list gains posting fragmentation per partition** — design §11.1's signature runs erode with every small ingest batch and nothing repairs them, so the erosion is invisible unless measured, and this metric is also the trigger condition for plan §14's deferred index-ordinal split. **Two D4 carry-overs corrected**: §6.6's "the Python builder" and §6.7's "the Python pipeline" both predate r4's move of the builder into the engine and are now named as `tessera build`. No decision (D1–D16) changes.

**Actions this document raises against its companions** — all five applied on 2026-07-27 (design r15, plan amended); the list is retained as the record of what changed and why:

1. *Implementation plan*: the stack line "Python build pipeline" is amended — the build pipeline is the engine's batch mode (`tessera build`), written once in Rust; Python becomes SDK + supervisor (§2.2 of the plan should carry the r4 argument, since its stated rationale applies to the caller's out-of-scope model pipeline). Phase 1 builds one tiler, in Rust; the WAL joins the walking skeleton's storage work. The `reference/` oracle stays Python, deliberately.
2. *Implementation plan*: the conformance matrix gains a restart-replay test asserting deny-disposition overlay entries survive a crash (the fail-open case), and a deny-retirement window test — delete an item, retain a pre-deletion fragment epoch on a live token, run compaction, and assert the item stays invisible until every predating epoch is gone (§6.5). (The r2 tiler differential test is superseded by D4: one tiler.)
3. *Design, Appendix C*: two candidate entries — the session pin's rate of change as a corpus-activity signal (C14-like, low), and the router's label presence registry (metadata crossing the compartment boundary without entity IDs or counts; C13-adjacent).
4. *Design §6.1*: record that the interning namespace lives at the router under fan-out (D11), so the "owned in one place" sentence acquires a process address.
5. *Design §2.3 and §6.1*: the verifiable-auth-data pattern (§4.3 here) — in-module trust anchors, host-side `not_after` enforcement outside the sandbox, revocation bounded by assertion lifetime — and the mask-cache key refinement: canonical dedup by satisfied-term-set hash, auth-data hash retained as a fast path. The second changes a sentence of §2.3 and strictly widens cache sharing.
