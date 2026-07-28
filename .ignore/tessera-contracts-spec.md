# Tessera — Contracts Specification

**Status:** Draft r4 — r3 plus two owner-decided amendments from Phase 1 planning review (Appendix R)
**Owns:** the byte- and schema-level definition of the boundaries named in the system architecture §4: the bundle format, the service API, the plugin ABI and the wire format. `§n` refers to the design (r15); `SA §n` to the system architecture (r4). Where this document and either of those disagree, they are right — except for the four recorded deviations in §0.3, which are proposed back to them.

---

## 0. Principles

### 0.1 A contract exists only where a second reader exists

The readers are: the Python reference oracle (which must parse bundles independently — it is the differential check on the engine), a future engine version (upgrades roll forward over old bundles), the conformance suite, and an auditor. Everything with exactly one reader-writer — the WAL, mask-fragment caches and frozen mirrors, derived tile tables and candidate lists, the allocator journal, the router/worker protocol, in-memory anything — is **out of contract** and may change without notice. We specify interchange, not implementation.

### 0.2 Adopt published formats; invent nothing with an existing spec

Columns are Arrow IPC files, unmodified. Postings, tombstones and membership bitmaps use the portable Roaring format (the cross-implementation `RoaringFormatSpec`), which `pyroaring` reads directly. Manifests are JSON. Exactly **one** bespoke binary layout exists — the permutation (2.6) — and it is a raw array with a 16-byte header.

### 0.3 Recorded deviations from the source documents

Each proposed back to its source; until applied there, this list is the record:

1. **Portable Roaring in the bundle, not frozen** (was SA §4.1). Frozen is CRoaring-internal — a weak oracle and audit story; portable is publicly specified. The engine builds frozen mirrors under its local cache at sync time (out of contract); §10.4's frozen-view mask loading is untouched.
2. **The inverse permutation is not stored** (was §5.1 "plus its inverse", SA §4.1). Row→entity is the `entity_id` column of `columns.arrow`; storing it twice adds a consistency obligation with no reader.
3. **`tiles.bin` and `candidates.bin` are out of contract** (was SA §4.1). Both are derivable — tile ranges by binary search over `morton.u64`, candidate lists from the priority column — and I7 guarantees the exact fallback, so they are engine-local derived caches, exactly like the frozen mirrors.
4. **Side-manifests are per partition** (refines SA §4.1's prefix-level `SEGMENTS-<n>.json`). Workers flush independently and hold their own watermarks (SA §6.3–6.4); a prefix-global side-manifest would need a coordinating writer and would put entity-ID tombstones outside their compartment. The pin is a vector of per-partition *(n, watermark)*, which is what SA §4.2 already says it is.

### 0.4 Phase-marked, not speculative

Sections carry the phase that first consumes them. **Phase 1's conformance burden is: `CURRENT`, `MANIFEST.json`, one per-partition `SEGMENTS-0.json`, base postings, pairs, dictionary, columns, morton, permutation.** The delta/tombstone/deny machinery (2.4, 2.8) is exercised when streaming ingest lands; labels (2.9) at Phase 3; text and vectors (2.10) at Phase 4. All are specified now only because the side-manifest schema must anticipate them or change shape later.

## 1. Conventions

- All integers little-endian. IDs: entity `u64` (< 2³² in `bundle_format = 1` — §16's exhaustion answer will bump the format), row `u32`, term `u32`; node IDs and external IDs are caller-supplied (UTF-8 ≤ 256 bytes; byte strings ≤ 256 bytes respectively).
- JSON: UTF-8, unknown fields ignored by readers, absent optional fields take documented defaults.
- Three contract versions, each a single integer, all currently **1**: `bundle_format`, `api_version`, `abi_version`. They version independently because their reader populations do (bundles outlive engines; plugins are third-party; clients are external). Readers refuse anything newer; additive changes don't bump, removals and semantic changes do.
- Digests: SHA-256, hex in JSON. All manifest paths are prefix-relative, forward slashes.
- Every plane authenticates via `Authorization: Bearer <token-or-credential>`. Tokens and credentials never appear in URLs, query strings or logs.

## 2. The bundle format (`bundle_format = 1`)

### 2.1 Layout

```
bundle/
  CURRENT                       # JSON: {"prefix": "v00042", "manifest_digest": "<hex>"}
  v00042/
    MANIFEST.json
    dictionary/terms-<k>.dict   # immutable extents, logically concatenated — ONE
                                # namespace, bundle-level (SA D11), never per partition
    partitions/<phash>/
      SEGMENTS-<n>.json         # per-partition side-manifests; n zero-padded decimal
      terms/postings.arrow      # CSR: one tagged record per term (2.4)
      terms/deltas-<n>.arrow
      terms/pairs.parquet       # r4; was pairs.arrow — see 2.4
      entities/external-ids-<k>.arrow   # extent 0 at build; one per flush thereafter
      entities/nodes/…          # Phase 3 (2.9)
      entities/labels/…         # Phase 3 (2.9)
      entities/vocab/…          # Phase 3 (2.9)
      slices/<slice_id>/
        permutation.bin
        segments/<seg_id>/
          columns.arrow
          morton.u64
      text/…                    # Phase 4 (2.10)
      vectors/…                 # Phase 4 (2.10)
```

`<phash>` is `"default"` for the empty required set, else the hex SHA-256 over the partition's required descriptors, each encoded as `u32 length ‖ bytes`, sorted bytewise, concatenated. `CURRENT` is the only mutable file, replaced atomically (write-then-rename locally; conditional put on object stores). Every other file is immutable; the prefix grows only by whole new files named in a newer side-manifest. `<slice_id>` and `<seg_id>` are opaque; identity and order come from manifests, never filename lexicography. A `seg_id`, once used, is **never reused** — across flushes, compactions or prefixes (merge-abandonment identity checks and delta provenance both depend on it).

**One segment per (partition, slice) at build.** `tessera build` and every compaction emit exactly one segment per partition-slice — a build *is* a full compaction. Additional segments exist only between compactions, appended by streaming flushes. This is what makes the permutation's single-segment addressing (2.6) sufficient.

### 2.2 MANIFEST.json

| Field | Type | Meaning |
|---|---|---|
| `bundle_format` | int | this spec's version |
| `created_at` | RFC 3339 | provenance |
| `data_plugin_hash` | hex | hash of the **data module** (4.1); serving refuses on mismatch |
| `declared_bounds` | object | plugin bounds, verbatim (§6.1) |
| `declared_scalars` | array | `[{name, arrow_type}]` — the caller's per-item columns; ingest validates against this before any segment exists |
| `small_term_threshold` | int | cardinality at or below which a posting is stored as a sorted array rather than Roaring (2.4); default 32 pending Phase 1 calibration |
| `quantisation` | object | `{x_min, x_max, y_min, y_max}` (f64) — see 2.5 |
| `entity_id_high_water` | u64 | first unallocated entity ID at build; seeds the allocator |
| `slices` | array | `[{id, display_name}]` |
| `partitions` | array | `[{phash, required_terms: [hex…]}]`; exactly one `"default"` entry |
| `provenance` | object | free-form; includes the recorded §7.8 generating-set choice |
| `files` | object | path → `{size, sha256}` for every build-time file |

### 2.3 Per-partition SEGMENTS-\<n\>.json

Each is **complete** for its partition — full current state, not a diff — so a reader needs one per partition. Written by that partition's worker only, after every file it names is durable. `n` is per-partition, monotone, and **never resets across prefixes** (compaction carries the counter forward), so delta directory names never collide.

| Field | Type | Meaning |
|---|---|---|
| `segments_version` | int | = n |
| `watermark` | u64 | entity high-water this partition's postings cover (§11.2) |
| `entity_id_high_water` | u64 | allocator high-water as of this version |
| `segments` | array | `[{slice, seg_id, row_count, entity_lo, entity_hi}]` in serving order |
| `deltas` | array | delta directory versions present |
| `dict_extents` | array | `[{path, records}]` — the bundle-level dictionary extents this partition's term IDs require, ordinal order. Extents are immutable and shared: several partitions listing the same extent carry identical digests, so verification never conflicts |
| `external_id_extents` | array | extent paths in order |
| `tombstones` | array | deleted entity IDs, ascending (this partition's only — isolation holds; folded away at compaction) |
| `deny` | array | `[{entity_id, cause: "suppress"}]` — the current suppression set. **Publication rule:** any accepted deny-disposition change (delete, suppress) triggers immediate publication of a new side-manifest, not deferral to the next flush — a syncing replica must never reconstruct a state in which a suppressed item is visible (SA §6.2's fail-open, at the interchange layer). `unsuppress` removes the entry in the next manifest |
| `files` | object | path → `{size, sha256}` for files added since MANIFEST |

Entity IDs appear here only inside their own partition's directory; `entity_lo`/`entity_hi` and high-waters are counter values, not item data (SA §6.6's argument). The pin is the vector of per-partition *(prefix, n, watermark)*; single-partition deployments have a vector of one. The `watermark` component is **advisory** (status and debugging): I1 composition always uses the watermark of the mask fragment actually loaded, and pins fix row-space geometry only, never authorisation state (lifecycle design §2.3).

**Reader protocol.** Read `CURRENT` → fetch and digest-check `MANIFEST.json` → check `bundle_format` → per partition, take the highest `SEGMENTS-<n>.json` whose listed `files` **and** the MANIFEST `files` set all verify by size and digest; if the highest fails, step down until one verifies. Readiness requires a verifying manifest per partition **and** a freshness gate: `readyz` fails if the newest verifying `n` is older than the deployment's configured lag bound — unbounded step-down would let a badly synced replica serve long-deleted items as live.

### 2.4 Postings, deltas, pairs, dictionary, external IDs

- `terms/postings.arrow` — the base tier, one Arrow IPC file: a single `large_binary` column whose row ordinal is the term ID, so Arrow's offsets buffer *is* the CSR index and no bespoke index format exists. Each record is one tagged posting: `u8 tag` then payload — tag 0 = sorted `u32` entity array (terms at or below `small_term_threshold`), tag 1 = portable Roaring. The split is load-bearing, not tuning: at dictionary scale a third of terms are singletons, and per-term Roaring overhead alone would exceed the entire term-index budget (probes, results §4.3). Per-term *files* are ruled out for the same reason — a hundred million inodes is not a format.
- `terms/deltas-<n>.arrow` — sparse: `(term_id: uint32, posting: large_binary)` with the same tagged records, covering entities flushed at version *n*. Effective postings = base ∪ deltas − `tombstones`. *(Streaming phase.)*
- `terms/pairs.parquet` *(r4; was `pairs.arrow`)* — **Parquet**, `(entity_id: uint64, term_id: uint32)`, sorted by term then entity, `DELTA_BINARY_PACKED` (measured 3.8× smaller, 3× faster to read — figures that were always Parquet's: r3 specified Arrow IPC carrying a Parquet-only encoding, an internal inconsistency this revision resolves in Parquet's favour). The uncompressed-mmap rule (design §10.3) protects request paths, and this file sits on neither; DuckDB and pyarrow read Parquet natively, so both contracted readers are served. ~15 GB → ~6 GB at 10⁹. **Read at build cadence and by the DuckDB oracle only — never on the authorise path**: Phase 0 measurement reassigned the semi-join to build machinery, with the postings union as the authorise formulation (probes, results §4.1; supersedes §6.3's framing of the semi-join as the mask-build step — the pair relation remains a requirement, but its consumer is postings *construction* and the oracle).
- `dictionary/terms-<k>.dict` — bundle-level (one interning namespace — SA D11); immutable extents of `u32 length ‖ descriptor bytes` records; a descriptor's `term_id` is its ordinal across the concatenation in extent order. Extents, not appends: no file ever changes after a manifest names it, so verification needs no special case. New extents are written by the namespace owner (the router) at flush; workers reference them by path and digest.
- `entities/external-ids-<k>.arrow` — Arrow IPC `(external_id: binary, entity_id: uint64)`, sorted within each extent; extent 0 at build, one per flush after. This is what keeps `/control/changes` addressable after a restore beyond WAL retention.

### 2.5 Quantisation and Morton codes

The contract, because the oracle must reproduce `morton.u64` byte-for-byte and clients name tiles. Grid: 2¹⁶ × 2¹⁶ (§5.2). Quantisation of coordinate *v* over `[min, max]` from MANIFEST:

```
cell(v) = clamp( floor( (v − min) / (max − min) × 65536 ), 0, 65535 )
```

(so `v = max` lands in cell 65535; cells are half-open). Interleave: bit *i* of `cell(x)` occupies code bit 2*i*; bit *i* of `cell(y)` occupies code bit 2*i*+1 — matching the design's worked example (x=6, y=3 → 30). The 32-bit code is stored low-aligned in a `u64`; high bits zero. A tile at depth *d* (0 ≤ d ≤ 16) is identified on the wire by its prefix value `code >> (32 − 2d)`; its row range in a segment is found by binary search over that segment's `morton.u64`.

### 2.6 `columns.arrow`, `morton.u64`, `permutation.bin`

`columns.arrow`: standard Arrow IPC file, uncompressed buffers (mmap-and-slice — §10.3), one record batch, Morton order with priority tiebreak:

| Column | Type | Notes |
|---|---|---|
| `entity_id` | uint64 | the row→entity direction (deviation 2) |
| `x`, `y` | float32 | as supplied (quantisation is for codes, not storage) |
| `node_id` | uint32 | index into the partition's node table (2.9); `0xFFFFFFFF` = none |
| `priority` | uint16 | hash-derived constant (§7.2) |
| *declared scalars* | per MANIFEST | |

**The priority function is contract** *(r4)* — the oracle must reproduce the stored column, and §7.2's nesting argument depends on it being a fixed per-entity constant. `priority(e)` is the **high 16 bits of splitmix64 over the entity ID** (all arithmetic wrapping u64):

```
z = e + 0x9E3779B97F4A7C15
z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
z = (z ^ (z >> 27)) * 0x94D049BB133111EB
priority = (z ^ (z >> 31)) >> 48        # as u16
```

Previously the table said only "hash-derived", which no second reader could reproduce. Changing this function is a `bundle_format` bump.

`morton.u64`: raw sorted `u64` codes, no header; length = `row_count × 8`.

`permutation.bin`: header `TSPM`, `u16 version = 1`, `u16 reserved`, `u64 bound`; then `entity_to_row: u32 × bound`, sentinel `0xFFFFFFFF` (segments therefore hold fewer than 2³²−1 rows). **Row IDs are segment-local**; this file addresses the partition-slice's single build segment (2.1), and `bound` is that partition-slice's max build entity + 1 — not the global high-water, so sparse partitions don't ship oceans of sentinel. An entity in a *streamed* segment is located via its segment's `entity_lo`/`entity_hi` and that segment's `entity_id` column; compaction folds everything back into one segment and a fresh permutation.

### 2.9 Phase 3 reservations — nodes, labels, vocab

`entities/nodes/` will hold: `table.arrow` (`node_id: uint32` index → caller's node ID string — the wire and columns use the index; the admin plane uses the caller's string), per-node portable-Roaring membership, per-slice bounding boxes, per-segment row ranges, and per-node term distributions (consumed by `/control/nodes/{id}/term-distribution`). `entities/labels/`: per label — text, tier, generating-set slice (portable Roaring), partition-presence set (`phash` list, SA §2.3). `entities/vocab/`: CSR as three Arrow arrays. Field schemas land with Phase 3 as additive changes.

### 2.10 Phase 4 reservations — text, vectors

`text/` mirrors `terms/` exactly (dictionary extents + portable postings + deltas) — the same parser, no new format. Client-side token normalisation rules become contract when the operand lands. `vectors/` is decided in Phase 4; `bundle_format = 1` constrains it only to live under that directory and appear in `files`.

## 3. The service API (`api_version = 1`)

### 3.1 Conventions

JSON requests (`application/json`) unless marked **Arrow** (`application/vnd.apache.arrow.stream`); `api_version` in `/v1/meta` and an `x-tessera-api` header everywhere. All four viewer data endpoints return `x-tessera-pin` and accept an optional `pin` field (SA §4.2; `410` on a drained pin). Errors: `{"error": code, "detail": string, "retry_after_s"?: int}`, closed code list:

| HTTP | code | Meaning |
|---|---|---|
| 401 | `bad-credential` | missing/invalid token or credential |
| 403 | `expired-token` | re-authorise |
| 404 | `unknown` | unknown handle, node, external ID or slice (handles: not minted in this session — I10; nothing is enumerable) |
| 409 | `conflict` | duplicate external IDs (detail lists them) or batch-id replay with different bytes; a 409 batch had **no effect** |
| 410 | `pin-expired` | drained pin; rejected, never reinterpreted (I11) |
| 422 | `contract` | malformed request, bounds exceeded, unknown filter operand |
| 429 | `backpressure` | ingest only — `/control/changes` is **never** load-shed (it is small, WAL-appended, and refusing security operations for load is fail-open; overlay pressure schedules rebuilds and alarms instead, SA §6.5) |
| 500 | `fail-closed` | any mask/composition/containment failure; partial results do not exist |
| 503 | `not-ready` | unverified bundle, unready worker, unloaded plugin |

`/healthz` (liveness) and `/readyz` (2.3's verified + fresh + pinned + plugin loaded + workers ready) are on every plane's listener.

### 3.2 Viewer plane

`GET /v1/meta` → `api_version`, `bundle_format`, slices, quantisation extents, declared-scalar schema, filter operand names, and (Phase 3) the C11-gated label vocabulary — the one data-derived field.

`POST /v1/viewport` — `{slice, zoom, bbox: [x0,y0,x1,y1], k?, filters?, pin?}`; `k` defaults to 30, capped by `max_k`. Response **Arrow**, two batches:
1. *tiles*: `(tile: uint64, visible: uint64, matched: uint64)` — tile per 2.5's prefix encoding at depth `zoom`; exact masked counts; `matched = visible` with no filters (§8.1).
2. *points*: `(handle: uint32, x: float32, y: float32, …declared scalars)`.

`POST /v1/labels` *(Phase 3)* — request as viewport → JSON `[{node_handle, label, tier}]`, pre-gated (I3), nothing for unsatisfied candidates.

`POST /v1/region` — `{slice, polygon: [[x,y]…] | bbox, filters?, pin?}` (exactly one of polygon/bbox; vertices capped). Response **Arrow**, exactly three batches:
1. *summary*: one row `(visible: uint64, preview_rows: uint32)`;
2. *preview*: the points schema;
3. *breakdowns*: long-form `(scalar: utf8, value: utf8, count: uint64, exact: bool)`. The materialisation threshold governing `exact` is evaluated against the **visible (masked) count** — never the raw row count, whose value would be a per-request unmasked corpus quantity (I2).

`POST /v1/items/{handle}` — `{pin?}` → JSON scalars + drill-down fields.

`filters`: named operands composed by intersection (§8.2): `{"labels": [node_handle…]}` (Phase 3), `{"text": {"all": […], "any": […]}}` (Phase 4). Unknown names → `422`. Unmatched text tokens contribute an empty operand and no acknowledgement.

### 3.3 Session plane

`POST /session/authorise` — `{auth_data: "<base64>"}`. The decoded bytes are passed to `terms_of_auth` verbatim and are the fast-path hash input (§2.3 r15) — base64 always, because "verbatim JSON" is undefined under re-serialisation and the byte-identity the fast path needs must be unambiguous. Response: `{token, token_id, expires_at}` with `expires_at = min(backstop, plugin not_after)`. A valid credential yielding zero terms mints a token with zero visibility — deliberate: the service cannot distinguish "wrong credential" from "cleared for nothing", and refusing would disclose which (§7.7's refusal-carries-no-information property, applied to authorise).

`POST /session/revoke` — `{token_id}` → 204. Revocation is by the non-capability `token_id`, so the capability itself never transits a second time.

### 3.4 Admin plane

| Endpoint | Essentials |
|---|---|
| `POST /control/ingest` | **Arrow**: `(external_id: binary, x: float32, y: float32, access: utf8, node_id: utf8?, …declared scalars)` + headers `x-tessera-batch-id`, `x-tessera-slice` (optional when the bundle has one slice; `422` if ambiguous). 200 after WAL fsync: `{accepted, over_bound, over_bound_ids: [first 100…]}` — over-bound items are **indexed regardless** (bounds warn, never exclude — §6.2 r16); identity is what makes the warn a usable data-quality signal. Idempotency: the batch id maps to the SHA-256 of the raw request body; a retry must resend identical bytes (Arrow serialisation is not canonical, so re-serialising is the client's bug to avoid) |
| `POST /control/changes` | JSON `[{external_id, op: "predicate"\|"delete"\|"suppress"\|"unsuppress", access?}]`. 200 after WAL fsync; never 429; deny ops trigger immediate side-manifest publication (2.3) |
| `POST /control/labels` *(Ph 3)* | text, tier, node ID, generating set as external IDs |
| `GET /control/labels/invalidated?cursor=` *(Ph 3)* | `{items: [{label_id, cause}], next_cursor}` |
| `GET /control/nodes/{node_id}/term-distribution` *(Ph 3)* | caller's node ID string; build data |
| `GET /control/nodes/{node_id}/members?cursor=` *(Ph 3)* | **build credential**; external IDs, paged |
| `POST /control/allocate-ids` | `{count}` → `{lo, hi}` |
| `POST /control/flush` · `POST /control/compact` | 202 |
| `GET /control/status` | per-partition `{segments_version, watermark, readiness}`, overlay size, WAL depth, overflow count, pins held |

## 4. The plugin ABI (`abi_version = 1`)

### 4.1 One module or two

The auth and data functions may ship as **separate modules** (config: `plugin.data`, `plugin.auth`; one value serves both in the simple case). This keeps §6.1's two blast radii separate operationally: rotating trust anchors — a routine, auth-side event under SA §4.3, since anchors live *in the auth module* — changes only the auth hash (mask-cache invalidation), never `data_plugin_hash` (a full-reindex event). MANIFEST's `data_plugin_hash` is the data module's hash; the auth module's hash keys the mask cache.

### 4.2 Conventions

wasm32, no WASI imports (instantiation refuses any). Exports: `alloc(len: u32) -> u32` and `dealloc(ptr: u32, len: u32)`. Variable-length values cross as packed `u64` = `ptr << 32 | len` — no multi-value feature dependency, one convention. Lifecycle: the host allocates inputs via `alloc`, calls the export, copies the output, then calls `dealloc` on both input and output buffers; a module's output must stay valid until that `dealloc`; no buffer outlives its call.

### 4.3 Exports

| Export | Signature | Output (JSON bytes — the module needs a JSON emitter once, so every output uses it) |
|---|---|---|
| `abi_version` | `() -> u32` | must return 1 |
| `declared_bounds` | `() -> u64` | `{max_distinct_terms, max_terms_per_item, max_terms_per_token}` |
| `terms_of_label` | `(u64) -> u64` | `{"terms": ["<hex descriptor>", …]}` |
| `terms_of_auth` | `(u64) -> u64` | `{"terms": [...], "not_after"?: unix_seconds}` |

An empty `terms` list is valid: default deny (label side), zero-visibility token (auth side). A trap or structurally malformed JSON fails the operation closed (`500 fail-closed`); exceeding a declared bound **warns and proceeds** — bounds are sizing declarations, not enforcement (§6.2 r16). Determinism is environmental (no clock, no WASI — plan §2.3); `not_after` is enforced by the *host* clock (SA §4.3). Batching is an additive `abi_version = 2` candidate with no current customer.

## 5. The wire format

Determined by §3's Arrow schemas plus three rules: every identity on the viewer plane is a per-session `u32` handle (I10; byte-scan-tested in payloads and logs); handles are per-session-keyed and structureless, and the decoded worker-local reference is a session-table index, never an entity ID (SA §4.5); buffers are uncompressed for zero-copy slicing. The API section *is* the wire contract; there is no second document to drift.

## 6. Out of contract, deliberately

The WAL; frozen mirrors, derived tile tables and candidate lists (deviation 3); the allocator journal; the router/worker protocol; `tessera.toml` (stable UX, not a byte contract); metrics names. The graduation trigger is acquiring a second independent reader — that event, not foresight.

## 7. Open items

- **Verify** `croaring`-written portable Roaring round-trips through `pyroaring` byte-for-byte (both implement `RoaringFormatSpec`; one-hour check the oracle depends on).
- **Verify** conditional-put semantics for `CURRENT` on the target object store; fallback is a publisher-side lock only publishers pay.
- Region breakdown materialisation threshold *value* (its evaluation basis is fixed in 3.2) — set at deployment review alongside `min_visible_members`.
- `readyz` freshness lag default.

## Appendix R — Review record

r1 was reviewed by two independent reviewers: implementability-and-simplicity (verdict: sound-with-fixes) and conformance against the design and system architecture (verdict: needs-rework, centred on the side-manifest schema). r2 resolves all findings. Structural: side-manifests moved **per partition** with per-partition *(n, watermark)* pins, partition-local tombstones and an in-contract `deny` set with an immediate-publication rule (closing a replica fail-open the WAL's out-of-contract status would otherwise create); dictionary append-only file replaced by immutable extents; `tiles.bin` and `candidates.bin` de-contracted as derived caches (bespoke binary formats: three → one); the Morton/quantisation function specified (2.5); the ABI given a packed-u64 convention, buffer lifecycle, uniform JSON outputs and a two-module split so trust-anchor rotation cannot masquerade as a reindex event; row IDs defined segment-local with one-build-segment-per-partition-slice; external-ID extents added per flush; `/v1/region`'s exact three-batch layout fixed with its threshold evaluated against masked counts (I2); auth_data made base64-precise for the r15 fast-path hash; token revocation moved off the URL; `declared_scalars`, slice/node ingest fields, overflow identity, batch-id raw-byte idempotency, phash construction, sentinel reservations and header scheme all pinned. Four deviations from the sources are recorded in §0.3 and proposed back (SA §4.1's layout tree and §5.1's inverse permutation being the substantive two).

**r4** applies two owner decisions (2026-07-28) raised during Phase 1 implementation-planning review. The pair relation becomes **`pairs.parquet`** (§2.1, §2.4): r3 specified Arrow IPC with `DELTA_BINARY_PACKED` — a Parquet encoding Arrow IPC does not support — so the spec was internally inconsistent and the measured 3.8×/3× figures were Parquet's all along; r3 had already re-scoped the file off both request paths, so the uncompressed-mmap argument never applied to it, and Parquet stands. And the **priority function is fixed** (§2.6): high 16 bits of splitmix64 over the entity ID — previously "hash-derived" with no definition, which no oracle could reproduce. Both were surfaced by the Phase 1 plan's independent review as silent-deviation risks and resolved by the owner rather than by the plan.

**r3** folds in two Phase 0 measurement results (`probes/results.md`, `probes/optimisations.md`): the postings layout replaced per-term files with a single CSR Arrow file carrying tagged records and a `small_term_threshold` — dictionary-scale measurement (117M terms, 34% singletons) made per-term Roaring untenable in both file count and overhead; and `pairs.arrow` re-scoped to build cadence and the oracle, since measurement reassigned the authorise path from the semi-join to the postings union. A related engine-internal note with no contract impact: frozen mirrors want 32-byte alignment (probes, optimisations §6.5) — internal because r2 had already moved frozen out of the bundle.
