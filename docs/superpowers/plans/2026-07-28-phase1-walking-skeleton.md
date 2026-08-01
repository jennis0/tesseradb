# Tessera Phase 1 — Walking Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One end-to-end path — `tessera build` over the 10⁹ Phase 0 corpus → bundle → `tessera serve` → authorise → masked viewport query — proving p99 < 10 ms per viewport at 10⁹ with a real mask applied, with zero entity IDs observable on the wire, plus the WAL/allocator/overlay durability skeleton and the differential-oracle scaffolding.

**Architecture:** A single Rust binary (`tessera`) with serve and batch (build) modes, decomposed into workspace crates whose public surfaces make the invariants structural (system architecture §3). Geometry is Morton-ordered per slice (row space, `u32`); permissions are Roaring bitmaps over append-only entity IDs (entity space, `u64`); the two meet only in the `Permutation`. Phase 1 is one slice, one partition (`default`), no filters, no labels, no compartments, a deliberately-wrong placeholder sampler, and no streaming flush (ingest is durable via the WAL but items gain geometry only at the next build).

**Tech Stack:** Rust (stable, edition 2021). Crates: `croaring` ≥ 2.7 (Frozen views), `arrow` + `arrow-ipc` (arrow-rs), `parquet`, `memmap2`, `rayon`, `tokio` + `axum`, `serde`/`serde_json`, `postcard`, `crc32fast`, `sha2`, `rustc-hash`, `parking_lot`, `arc-swap`, `toml`, `tracing`. Dev: `proptest`, `criterion`, `tempfile`, `reqwest` (blocking, tests). Python (test-only): `uv` venv 3.12 with `pyroaring`, `pyarrow`, `polars`, `numpy`, `pytest`, `requests`.

## Global Constraints

Copied from the governing documents. Every task's requirements implicitly include this section.

1. **Document precedence.** The design docs live in `docs/design/` (file-search tooling skips it — pass paths explicitly). The architecture design (`docs/design/architecture.md`, r19) is the specification; the contracts spec (`docs/design/contracts.md`, r4) owns byte formats and its §0.3 deviations govern; the system architecture (`docs/design/system-architecture.md`, r4) owns crates/processes; the lifecycle design (`docs/design/concurrency-lifecycle.md`, r3) owns WAL/overlay/pin mechanisms. **If this plan and a spec document disagree, the spec is right — STOP and report the conflict; do not resolve it silently.**
2. **Invariants I1–I13** (design §4) are non-negotiable. The ones this phase touches: I1 (mask composed before use), I2 (no aggregate from unmasked data; the mask is the only entry to the geometry arrays), I4 (entity space ≠ row space; only `Permutation` crosses), I9 (entity IDs append-only, never reused, durable high-water), I10 (entity IDs never cross the trust boundary; per-session handles only), I11 (row-space artifacts pinned to *(prefix, segments-version, watermark)*; drained pins rejected `410`, never reinterpreted).
3. **Fail closed everywhere.** Any error in mask construction, composition, or verification returns an error (`500 fail-closed`) — never a partial result. Deny-disposition changes (`delete`/`suppress`) are never refused for load.
4. **Byte formats are fixed** by contracts spec §2 (`bundle_format = 1`): exact field tables in this plan's Reference Sheet. Do not invent or "improve" a format.
5. **Crate dependency rules** (SA §3), enforced by CI check: `tessera-authz` must not depend on `tessera-store` or `tessera-spatial`; `RowId` never appears in `tessera-authz`'s API; only `tessera-store` exports `Permutation`; `tessera-types` offers **no** conversions between ID newtypes (no `From`, no arithmetic); outside `tessera-wire`'s handle-table module, payload builders accept `Handle` only.
6. **British spelling** in all docs, comments, and API text: *authorisation*, *serialise*, *behaviour*, *colour*, *licence*.
7. **Test data:** the corpus at `data/scaled/` — `geometry.parquet` (10⁹ rows, sorted by `(morton, entity_id)`, columns include `entity_id`, `x`, `y`), `pairs.parquet` (1.72 B rows `(entity_id, term_id)`, 47,968 terms), `scales.json`. Smaller corpora are **prefixes**: filter `entity_id < limit` for limits 250_000 / 2_422_486 / 250_000_000 / 10⁹ (read `scales.json` for exact values). Iterate at 250k; validate at 2.4M; the exit benchmark runs at 10⁹. Disk is tight (~98 GB free): delete scratch bundles when done; the 10⁹ bundle is built once, late (Task 16).
8. **Performance floor:** bitmap ops cost O(containers touched). The permutation projection (seconds at 10⁹) must never sit on the per-viewport path — it is cached per (token, slice, pin). Comment this at the call site.
9. **Commit per task step** as instructed; conventional messages (`feat:`, `test:`, `chore:`). Run `cargo fmt --all`, `cargo clippy --workspace -- -D warnings`, and `cargo test --workspace` before every commit.
10. Python **drives, never implements**: Python appears only in `reference/` (the deliberately slow oracle) and `conformance/` test drivers. No Python in any request path or artifact production.
11. **Scope discipline:** no labels, no filters, no compartments/router/workers, no wasmtime, no candidate lists, no streaming flush, no merge policy in this phase. If a task seems to need one of these, STOP and report.

---

## Reference Sheet (normative values, copied from contracts spec r3)

Workers: consult this instead of re-deriving. Cited sections are the authority if a discrepancy is found.

### R1. Constants

| Name | Value | Source |
|---|---|---|
| `bundle_format`, `api_version`, `abi_version` | all `1` | contracts §1 |
| Entity ID | `u64`, must be < 2³² in `bundle_format = 1` | contracts §1 |
| Row ID | `u32`, segment-local | contracts §2.6 |
| Term ID | `u32` = ordinal across dictionary extents | contracts §2.4 |
| Row-absent sentinel | `0xFFFF_FFFF` | contracts §2.6 |
| `node_id` "none" sentinel | `0xFFFF_FFFF` | contracts §2.6 |
| `small_term_threshold` | 32 (manifest field; default pending calibration) | contracts §2.2 |
| Grid | 2¹⁶ × 2¹⁶; Morton code 32-bit, low-aligned in u64 on disk | contracts §2.5 |
| Frozen mask buffers | 32-byte aligned, exact length (engine-local cache, NOT bundle) | probes/pre-phase1-verifications §1 |
| Integers on disk | little-endian | contracts §1 |
| Digests | SHA-256, hex in JSON; manifest paths prefix-relative, forward slashes | contracts §1 |
| Viewport `k` default / cap | 30 / `max_k` config (200) | contracts §3.2, SA §7 |

### R2. Quantisation and Morton (contracts §2.5 — the oracle must reproduce byte-for-byte)

```
cell(v) = clamp( floor( (v − min) / (max − min) × 65536 ), 0, 65535 )     # compute in f64
```
Cells are half-open; `v = max` lands in cell 65535. Interleave: bit *i* of `cell(x)` → code bit 2*i*; bit *i* of `cell(y)` → code bit 2*i*+1. Worked example (must be a unit test): `x_cell=6, y_cell=3 → code 30`. A tile at depth *d* (0 ≤ d ≤ 16) is identified by prefix `code >> (32 − 2d)`; its code range is `[tile << (32−2d), (tile+1) << (32−2d))`; its row range comes from binary search over `morton.u64`.

### R3. Priority (normative since contracts r4 §2.6)

`priority(e) = (splitmix64(e) >> 48) as u16` where splitmix64 is:

```
z = e + 0x9E3779B97F4A7C15
z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
z = (z ^ (z >> 27)) * 0x94D049BB133111EB
z = z ^ (z >> 31)
```
(All wrapping u64 arithmetic.) Both the Rust engine and the Python oracle implement exactly this.

### R4. Bundle layout (Phase 1 subset — contracts §2.1, §0.4)

```
bundle/
  CURRENT                        # JSON {"prefix": "v00000", "manifest_digest": "<hex>"}
  v00000/
    MANIFEST.json                # fields per contracts §2.2 (see Task 8)
    dictionary/terms-0.dict      # records: u32 LE length ‖ descriptor bytes; term_id = ordinal
    partitions/default/
      SEGMENTS-0.json            # fields per contracts §2.3 (see Task 8)
      terms/postings.arrow       # one large_binary column; row ordinal = term_id;
                                 #   record = u8 tag ‖ payload; tag 0 = sorted u32 entity array
                                 #   (count ≤ small_term_threshold); tag 1 = portable Roaring
      terms/pairs.parquet        # (entity_id: uint64, term_id: uint32) sorted by (term, entity),
                                 #   DELTA_BINARY_PACKED — owner decision 2026-07-28 resolving OQ1;
                                 #   amends contracts §2.4 (file was "pairs.arrow" as Arrow IPC).
                                 #   Off both request paths: build-cadence + oracle reads only
      entities/external-ids-0.arrow  # (external_id: binary, entity_id: uint64) sorted by external_id
      slices/<slice_id>/
        permutation.bin          # "TSPM" ‖ u16 version=1 ‖ u16 reserved ‖ u64 bound ‖ u32×bound, sentinel 0xFFFFFFFF
        segments/<seg_id>/
          columns.arrow          # Arrow IPC file, ONE record batch, UNCOMPRESSED buffers, Morton order,
                                 #   priority tiebreak: (morton, priority, entity_id).
                                 #   Columns: entity_id u64, x f32, y f32, node_id u32, priority u16, [declared scalars]
          morton.u64             # raw sorted u64 LE codes, no header; length = row_count × 8
```
`CURRENT` is the only mutable file (write-then-rename). One segment per (partition, slice) at build. `seg_id` and `slice_id` are opaque; identity and order come from manifests, never filename order. Portable Roaring = the cross-implementation `RoaringFormatSpec` (croaring `Portable` serializer; `pyroaring` reads it directly).

### R5. Service API (Phase 1 subset — contracts §3)

Errors: JSON `{"error": code, "detail": string}`; closed code list: `bad-credential` 401, `expired-token` 403, `unknown` 404, `conflict` 409, `pin-expired` 410, `contract` 422, `backpressure` 429 (ingest, and — *amendment, concurrency workstream Task 4* — the viewer/session compute-admission gate in front of `/v1/viewport`, `/v1/items`, `/session/authorise`; still never `/control/changes`), `fail-closed` 500, `not-ready` 503. Bearer auth on every plane; tokens never in URLs or logs. `/healthz` + `/readyz` on every listener.

- **Viewer plane** (`serve.viewer`, default `127.0.0.1:7407`): `GET /v1/meta`; `POST /v1/viewport` `{slice, zoom, bbox:[x0,y0,x1,y1], k?, pin?}` → Arrow stream, batch 1 *tiles* `(tile: uint64, visible: uint64, matched: uint64)` (matched = visible; no filters in Phase 1), batch 2 *points* `(handle: uint32, x: float32, y: float32, …declared scalars)`; `POST /v1/items/{handle}` `{pin?}` → JSON scalars. All data responses set `x-tessera-pin`.
- **Session plane** (`serve.session`, default `127.0.0.1:7408`): `POST /session/authorise` `{auth_data: "<base64>"}` → `{token, token_id, expires_at}` (zero-term credential mints a zero-visibility token — deliberate); `POST /session/revoke` `{token_id}` → 204.
- **Admin plane** (`serve.control`, unix socket; tests may configure loopback TCP — the documented Windows shape, SA §4.2 — since `reqwest` does not speak unix sockets; shell checks use `curl --unix-socket`): `POST /control/ingest` (Arrow batch `(external_id: binary, x: float32, y: float32, access: utf8, node_id: utf8?, …scalars)`, headers `x-tessera-batch-id`, optional `x-tessera-slice` (422 if ambiguous); 200 only after WAL fsync, body `{accepted, over_bound, over_bound_ids}`; replay of acked batch id idempotent; same id + different body hash → 409); `POST /control/changes` (JSON `[{external_id, op: "predicate"|"delete"|"suppress"|"unsuppress", access?}]`; 200 after fsync; **never 429**); `GET /control/status`.
- `GET /v1/meta` response fields: `api_version`, `bundle_format`, `slices` (`[{id, display_name}]`), `quantisation` extents, declared-scalar schema, filter operand names (`[]` in Phase 1), contract versions.
- `readyz` note: contracts §2.3's freshness-lag gate is trivially satisfied (single local writer, no replicas) and its default is an open item (contracts §7) — acknowledged, not implemented.

### R6. Phase 1 plugin

The wasmtime host is out of scope (Task list, constraint 11). `tessera-plugin` defines the trait now and ships one native implementation, `builtin:passthrough`:
- `terms_of_label(access: &[u8]) -> Vec<Descriptor>`: `access` is a UTF-8 comma-separated list of descriptor strings; split on `,`, trim, drop empties, return bytes.
- `terms_of_auth(auth_data: &[u8]) -> AuthTerms`: `auth_data` is JSON `{"terms": ["<descriptor>", …]}`; return descriptors, `not_after: None`.
- `declared_bounds()`: `{max_distinct_terms: 200_000_000, max_terms_per_item: 4096, max_terms_per_token: 100_000}` (sizing declarations; exceeding warns, never excludes).
- `data_plugin_hash()` / `auth_plugin_hash()`: SHA-256 of the string `"builtin:passthrough:1"`.

The Phase 0 corpus's `pairs.parquet` carries integer `term_id`s; the build (Task 8) synthesises each item's `access` string as the comma-joined decimal source term ids, so descriptors are decimal strings like `"1207"`. Determinism obligation applies: same input bytes, same descriptors.

---

## Task 0: Repository, workspace, CI gates

**Files:**
- Create: `.gitignore`, `Cargo.toml` (workspace), `rust-toolchain.toml`, `crates/*/Cargo.toml` + `src/lib.rs` stubs for: `tessera-types`, `tessera-plugin`, `tessera-authz`, `tessera-store`, `tessera-spatial`, `tessera-lifecycle`, `tessera-engine`, `tessera-wire`, `tessera-server`, `tessera-build`, `tessera-cli`
- Create: `scripts/check-layers.sh`

**Interfaces:**
- Produces: a building workspace; `scripts/check-layers.sh` exits non-zero on a forbidden dependency edge.

- [ ] **Step 1: Initialise git and the workspace skeleton.** `git init`; `.gitignore` containing `target/`, `data/`, `*.venv`, `.venv/`, `__pycache__/`, `/bundles/`. Workspace `Cargo.toml` listing all eleven crates under `crates/`, with `[workspace.dependencies]` pinning shared deps (`croaring = "2"` — verify the resolved version is ≥ 2.7.0 in `Cargo.lock`, it must expose `croaring::bitmap::Frozen`). Each crate: minimal `Cargo.toml` (edition 2021) and empty `lib.rs` (`tessera-cli` gets `src/main.rs` printing usage). `tessera-cli` produces the binary named `tessera` (`[[bin]] name = "tessera"`).
- [ ] **Step 2: Write the layer check.**

```bash
#!/usr/bin/env bash
# scripts/check-layers.sh — dependency edges the design forbids (SA §3).
# DIRECT dependencies only: server -> engine -> store is legitimate transitively,
# so a transitive check would be permanently red. --depth 1 is load-bearing.
set -euo pipefail
fail=0
deny() { # deny <crate> <forbidden-DIRECT-dep>
  if cargo tree -p "$1" --prefix none -e normal --depth 1 | tail -n +2 | grep -q "^$2 "; then
    echo "FORBIDDEN: $1 directly depends on $2"; fail=1
  fi
}
deny tessera-authz tessera-store
deny tessera-authz tessera-spatial
deny tessera-server tessera-store     # server sees engine API types only
deny tessera-server tessera-authz
deny tessera-wire tessera-store
deny tessera-wire tessera-authz
# I4: no ID conversions in types
if grep -rn "impl From" crates/tessera-types/src/ | grep -E "EntityId|RowId|TermId|Handle"; then
  echo "FORBIDDEN: ID conversion in tessera-types"; fail=1
fi
# I10: the payload module never sees EntityId (handles + plain columns only)
if grep -n "EntityId" crates/tessera-wire/src/payload.rs 2>/dev/null; then
  echo "FORBIDDEN: EntityId in tessera-wire payload module"; fail=1
fi
exit $fail
```

**Owner-action note (recorded, not a worker step):** at kickoff, reserve the PyPI distribution `tessera-index` with a placeholder release and check the crates.io `tessera*` namespace (probes/pre-phase1-verifications §2 — the availability check was a snapshot, not a hold). Needs the owner's publishing credentials.

- [ ] **Step 3: Verify.** Run `cargo build --workspace && bash scripts/check-layers.sh`. Expected: success, no forbidden edges.
- [ ] **Step 4: Commit.** `git add -A && git commit -m "chore: scaffold cargo workspace and layer checks"`

---

### Task 1: `tessera-types` — the ID newtypes

**Files:**
- Create: `crates/tessera-types/src/lib.rs`
- Test: same file (`#[cfg(test)]`)

**Interfaces:**
- Produces (everyone consumes): `EntityId`, `RowId`, `TermId`, `Handle`, `Priority`, `MortonCode`, `SliceId(String)`, `SegId(String)`, `PinId { prefix: String, segments_version: u64 }` (I11's pin — geometry identity only, see Task 11), constants `BUNDLE_FORMAT: u32 = 1`, `API_VERSION: u32 = 1`, `ABI_VERSION: u32 = 1`, `ROW_ABSENT: u32 = 0xFFFF_FFFF`, `NODE_NONE: u32 = 0xFFFF_FFFF`, `SMALL_TERM_THRESHOLD_DEFAULT: u32 = 32`. Each ID type: `pub fn new(raw) -> Self`, `pub fn raw(self) -> <int>`, derives `Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug` — **no `From`/`Into` between ID types, no arithmetic impls** (I4 as a type rule).

- [ ] **Step 1: Write the failing test.**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_distinct_types_with_raw_access() {
        let e = EntityId::new(7);
        let r = RowId::new(7);
        assert_eq!(e.raw(), 7u64);
        assert_eq!(r.raw(), 7u32);
        // The next line MUST NOT compile if uncommented — I4:
        // let _: RowId = e.into();
    }
    #[test]
    fn constants() {
        assert_eq!(BUNDLE_FORMAT, 1);
        assert_eq!(ROW_ABSENT, 0xFFFF_FFFF);
    }
}
```

- [ ] **Step 2: Run to verify failure.** `cargo test -p tessera-types` — FAIL: types not defined.
- [ ] **Step 3: Implement** the newtypes exactly as in Interfaces (a small macro for the repetition is fine). `EntityId` is `u64`; `RowId`, `TermId`, `Handle` are `u32`; `Priority` is `u16`; `MortonCode` is `u32`.
- [ ] **Step 4: Run tests.** `cargo test -p tessera-types` — PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(types): ID newtypes with no cross-space conversions (I4)"`

---

### Task 2: `tessera-spatial` — quantisation, Morton codes, tiles

**Files:**
- Create: `crates/tessera-spatial/src/morton.rs`, `crates/tessera-spatial/src/lib.rs`
- Test: `crates/tessera-spatial/src/morton.rs` (`#[cfg(test)]`) + `crates/tessera-spatial/tests/morton_props.rs`

**Interfaces:**
- Consumes: `tessera-types` (`MortonCode`).
- Produces:
  - `pub struct Extent { pub x_min: f64, pub x_max: f64, pub y_min: f64, pub y_max: f64 }`
  - `pub fn cell(v: f64, min: f64, max: f64) -> u16` — R2's formula exactly.
  - `pub fn interleave(x_cell: u16, y_cell: u16) -> MortonCode`
  - `pub fn morton_of(x: f64, y: f64, e: &Extent) -> MortonCode`
  - `pub struct Tile { pub prefix: u64, pub depth: u8 }` with `pub fn code_range(&self) -> (u64, u64)` (half-open, over the low-aligned-u64 code space: `(prefix << (32-2d), (prefix+1) << (32-2d))`).
  - `pub fn tiles_for_bbox(bbox: [f64; 4], depth: u8, e: &Extent) -> Vec<Tile>` — quantise corners to cells, shift to depth-d tile coords (`cell >> (16 - d)`), enumerate the `(tx, ty)` grid inclusive of both corners, `prefix = interleave(tx << (16-d) ... )` — careful: compute the prefix by interleaving the *d-bit* tile coordinates directly (spread over 2d bits), i.e. `interleave_bits(tx, ty, d)`.

- [ ] **Step 1: Failing unit tests.**

```rust
#[test]
fn worked_example_from_contracts_2_5() {
    assert_eq!(interleave(6, 3).raw(), 30); // contracts §2.5
}
#[test]
fn cell_boundaries() {
    assert_eq!(cell(0.0, 0.0, 1.0), 0);
    assert_eq!(cell(1.0, 0.0, 1.0), 65535);        // v = max lands in top cell
    assert_eq!(cell(0.5, 0.0, 1.0), 32768);
    assert_eq!(cell(-4.0, 0.0, 1.0), 0);           // clamped
}
#[test]
fn tile_range_nests() {
    let t = Tile { prefix: 0b11, depth: 1 };       // quadrant (1,1) at depth 1
    let (lo, hi) = t.code_range();
    assert_eq!(lo, 3u64 << 30);
    assert_eq!(hi, 4u64 << 30);
}
```

- [ ] **Step 2: Run to verify failure**, implement `spread`/`interleave` (bit-twiddling spread over 32 bits), `cell`, `Tile`, `tiles_for_bbox`.
- [ ] **Step 3: Property tests** (`proptest`): (a) `interleave` is injective on random pairs; (b) deinterleaving (write the inverse in the test) round-trips; (c) for random points and any depth, the point's code falls inside exactly one depth-d tile of `tiles_for_bbox` over the full extent; (d) parent tile range contains all four children's ranges.
- [ ] **Step 4: Run.** `cargo test -p tessera-spatial` — PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(spatial): quantisation, Morton interleave, tile ranges per contracts 2.5"`

---

### Task 3: `tessera-spatial` — the tiler; `tessera-store` — segment file writers

The tiler is one implementation used by `tessera build` now and streaming flush later (plan §5). It sorts a batch and emits the three per-segment artifacts.

**Files:**
- Create: `crates/tessera-spatial/src/tiler.rs`; `crates/tessera-store/src/write.rs`, `crates/tessera-store/src/lib.rs`
- Test: `crates/tessera-store/tests/segment_roundtrip.rs`

**Interfaces:**
- Consumes: Task 2 (`morton_of`, `Extent`), `tessera-types`.
- Produces:
  - `tessera_spatial::tiler::TilerItem { entity_id: EntityId, x: f32, y: f32, node_id: u32, priority: u16, scalars: Vec<ScalarValue> }` (Phase 1 `ScalarValue`: `U64(u64) | F32(f32) | Utf8(String)`).
  - `tessera_spatial::tiler::sort_batch(items: &mut [TilerItem], extent: &Extent) -> Vec<u64>` — computes each item's Morton code (f64 math on the f32 values), sorts items by `(morton, priority, entity_id)`, returns the sorted codes (low-aligned u64). Row ID = index after sort.
  - `tessera_store::write::write_segment(dir: &Path, items: &[TilerItem], codes: &[u64], scalar_schema: &[(String, ScalarType)]) -> io::Result<()>` — writes `columns.arrow` (Arrow IPC **file** format, one record batch, uncompressed) and `morton.u64` (raw LE bytes).
  - `tessera_store::write::write_permutation(path: &Path, items_in_row_order: &[EntityId], bound: u64) -> io::Result<()>` — R4's `TSPM` layout; every entity's slot gets its row index; all other slots `0xFFFF_FFFF`.
- Priority is **not computed here** — callers pass it (the allocator owns entity IDs; priority = R3 over the final entity ID; computed in Task 8's build and passed in).

- [ ] **Step 1: Failing round-trip test** (`segment_roundtrip.rs`): construct 1,000 `TilerItem`s with random coords in a unit extent, `priority` per R3, `sort_batch`, `write_segment` + `write_permutation` to a tempdir, then: (a) read `morton.u64` bytes back and assert non-decreasing and equal to returned codes; (b) open `columns.arrow` with `arrow_ipc::reader::FileReader` and assert row 0's `entity_id` equals `items[0].entity_id.raw()` post-sort and schema field names/types match R4; (c) parse `permutation.bin` manually (header magic/version/bound) and assert for every row *i*: `perm[entity_id[i]] == i`, and that a never-used entity slot reads `0xFFFF_FFFF`.
- [ ] **Step 2: Run to verify failure**, then implement. Writing Arrow: build `UInt64Array`/`Float32Array`/… columns, one `RecordBatch`, `arrow_ipc::writer::FileWriter` with default (uncompressed) options. `write_permutation`: `BufWriter`, write `b"TSPM"`, `1u16`, `0u16`, `bound: u64`, then a `vec![0xFFFF_FFFFu32; bound]` patched with row indices, as LE bytes. Return an error (do not panic) if any `entity_id.raw() >= bound` or `>= 2^32`.
- [ ] **Step 3: Tiebreak test:** two items with identical coords must order by `(priority, entity_id)`. Add and run.
- [ ] **Step 4: Run all.** `cargo test -p tessera-store -p tessera-spatial` — PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(store,spatial): tiler sort and segment writers (columns.arrow, morton.u64, permutation.bin)"`

---

### Task 4: `tessera-authz` — dictionary extents and the interner

**Files:**
- Create: `crates/tessera-authz/src/dict.rs`, `crates/tessera-authz/src/lib.rs`
- Test: in-file `#[cfg(test)]`

**Interfaces:**
- Consumes: `tessera-types` (`TermId`).
- Produces:
  - `pub struct DictWriter` — `new(dir: &Path)`, `intern(&mut self, descriptor: &[u8]) -> TermId` (dedups; ordinal assignment), `finish(self) -> io::Result<Vec<PathBuf>>` writes `terms-0.dict` (Phase 1 writes exactly one extent): each record `u32 LE length ‖ bytes`.
  - `pub struct Dict` — `load(paths: &[PathBuf]) -> io::Result<Dict>` (ordinal across extents in order), `lookup(&self, descriptor: &[u8]) -> Option<TermId>`, `len(&self) -> u32`. Backed by `rustc_hash::FxHashMap<Box<[u8]>, TermId>`.

- [ ] **Step 1: Failing test:** intern `[b"1207", b"9", b"1207"]` → ids `[0, 1, 0]`; `finish`; `Dict::load` → `lookup(b"9") == Some(TermId::new(1))`, `lookup(b"nope") == None`, `len() == 2`; and the file's raw bytes are exactly `04 00 00 00 31 32 30 37 01 00 00 00 39`.
- [ ] **Step 2: Run (fail), implement, run (pass).** `cargo test -p tessera-authz`
- [ ] **Step 3: Commit.** `git commit -am "feat(authz): dictionary extents and term interner (contracts 2.4)"`

---

### Task 5: `tessera-authz` — postings writer and reader (CSR tagged records)

**Files:**
- Create: `crates/tessera-authz/src/postings.rs`
- Test: `crates/tessera-authz/tests/postings_roundtrip.rs`

**Interfaces:**
- Consumes: Task 4; `croaring`.
- Produces:
  - `pub fn write_postings(path: &Path, per_term: &[Vec<u32>], small_term_threshold: u32) -> io::Result<()>` — `per_term[t]` is term *t*'s **sorted** entity list (entity ids known < 2³²). One Arrow IPC file, single `LargeBinaryArray` column named `posting`; record *t* = `[0u8] ‖ u32 LE array` if `len ≤ threshold` else `[1u8] ‖ portable Roaring bytes` (`Bitmap::from_sorted` → `run_optimize` → `serialize::<Portable>`).
  - `pub struct PostingsReader` — `open(path: &Path, mmap: bool) -> Result<Self>`; `pub enum PostingRef<'a> { Array(&'a [u8] /* raw LE u32s */), Roaring(croaring::BitmapView<'a>) }`; `fn posting(&self, t: TermId) -> Result<PostingRef<'_>>`; `fn term_count(&self) -> u32`. (Holds the mmap; returns views into it. `BitmapView::deserialize` — the portable/no-alignment path — is correct here; `Frozen` is only for the engine-local fragment cache.)

- [ ] **Step 1: Failing round-trip test:** three terms — `[5]` (singleton → tag 0), `1..=1000` (→ tag 1), `[]` (empty → tag 0, zero entries) — write, reopen, assert variants, contents, and that record 1's first byte is `1`. Cross-check with `pyroaring` deferred to Task 14 (oracle).
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Property test:** random per-term sets round-trip exactly (compare as sorted vecs), threshold boundary at exactly 32 and 33 entries.
- [ ] **Step 4: Commit.** `git commit -am "feat(authz): CSR postings with tagged records and small-term arrays"`

---

### Task 6: `tessera-authz` — mask fragment build and the frozen cache

**Files:**
- Create: `crates/tessera-authz/src/fragment.rs`
- Test: `crates/tessera-authz/tests/fragment.rs`

**Interfaces:**
- Consumes: Task 5.
- Produces:
  - `pub fn build_fragment(terms: &[TermId], postings: &PostingsReader) -> Result<croaring::Bitmap>` — partition postings into `Roaring` views and small arrays; `Bitmap::or_many` (bulk union — measured as the authorise path) over the views; `add_many` the concatenated-then-sorted small arrays; `run_optimize`. **Parametric: takes postings as an argument, holds no lifecycle state; `RowId` must not appear anywhere in this crate.**
  - `pub struct FragmentCache` — directory-backed frozen store. `new(dir: &Path, bundle_identity: [u8; 32] /* the MANIFEST digest */, auth_plugin_hash: [u8; 32])`; `get_or_build(&self, satisfied: &[TermId], auth_data_hash: [u8; 32], postings: &PostingsReader, watermark: u64) -> Result<Arc<FrozenFragment>>`. Canonical key = SHA-256 over `bundle_identity ‖ auth_plugin_hash ‖ sorted term_id u32 LEs` (design §2.3 requires the plugin version in the key; SA §3 adds partition + postings-epoch — term IDs are bundle-relative ordinals, so a persistent cache dir reused across rebuilds would otherwise serve a frozen fragment naming a *different* entity set: a disclosure bug, not a perf bug). In-memory `FxHashMap<auth_data_hash, canonical_key>` fast path. `FrozenFragment { view(&self) -> croaring::BitmapView<'_>, watermark: u64 }` — `watermark` is the generation's SEGMENTS watermark at build time, passed in by the caller; Task 10's composition uses the fragment's own watermark (lifecycle §2.3).
  - Frozen file discipline (probes/pre-phase1-verifications §1): serialise with `Frozen` (`REQUIRED_ALIGNMENT = 32`); the file contains exactly the frozen bytes from offset 0 (mmap base is page-aligned ⇒ 32-aligned); `deserialize_view` over the full mmap. Cache dir sits under the engine's local cache path, never in the bundle.

- [ ] **Step 1: Failing correctness test:** write postings for 50 random terms over a 100k-entity universe; for random grant subsets assert `build_fragment` equals the brute-force union computed with plain `HashSet<u32>` in the test.
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Frozen round-trip test:** build a fragment, `FragmentCache::get_or_build`, drop, reopen cache dir, `get_or_build` again with same terms — second call must **not** rebuild (instrument with a counter) and the view must equal the original bitmap. Assert file length equals `Frozen::get_serialized_size_in_bytes` exactly.
- [ ] **Step 3b: Stale-bundle miss test:** reopen the cache with a *different* `bundle_identity`, same terms — must rebuild, not hit (the disclosure case in the key rationale above).
- [ ] **Step 4: Commit.** `git commit -am "feat(authz): postings-union fragment build and frozen fragment cache"`

---

### Task 7: `tessera-store` — manifests, digest verification, the loader, `Permutation`

**Files:**
- Create: `crates/tessera-store/src/manifest.rs`, `crates/tessera-store/src/read.rs`, `crates/tessera-store/src/permutation.rs`
- Test: `crates/tessera-store/tests/bundle_read.rs`

**Interfaces:**
- Consumes: Tasks 3, `tessera-types`.
- Produces:
  - Serde structs `Manifest`, `SegmentsManifest`, `CurrentPointer` matching contracts §2.2/§2.3 field-for-field (unknown JSON fields ignored: `#[serde(default)]` style, no `deny_unknown_fields`).
  - `pub fn open_bundle(root: &Path) -> Result<Bundle>` — the **read protocol** (contracts §2.3): read `CURRENT` → fetch `MANIFEST.json`, check digest matches `CURRENT.manifest_digest` and `bundle_format ≤ 1` (refuse newer) → per partition take the highest `SEGMENTS-<n>.json` whose listed files (and MANIFest's) all verify by size + SHA-256; step down if not. Any failure → typed error (serving must stay unready).
  - `pub struct Bundle { manifest, partitions: … }`; `pub struct SegmentData { row_count: u32, morton: MortonSlice, columns: ColumnsRef }` — `ColumnsRef` exposes `entity_id(&self) -> &[u64]`, `x(&self) -> &[f32]`, `y(&self) -> &[f32]`, `node_id(&self) -> &[u32]`, `priority(&self) -> &[u16]`, `scalar(&self, name) -> ScalarSlice`. Implementation: mmap `columns.arrow`, parse the IPC footer/metadata with `arrow_ipc` to locate each column's data-buffer offset, hold typed slices **into the mmap** (zero-copy; validate: single batch, no compression, expected types). If wrestling arrow-ipc buffer offsets exceeds a day, fallback: also write each column as a raw sidecar in `write_segment` — **do not**; that changes the bundle contract. STOP and report instead.
  - `pub struct Permutation` (the **only** EntityId→RowId path in the codebase): `load(path) -> Result<Self>` (validates magic/version; mmap), `row_of(&self, e: EntityId) -> Option<RowId>` (`None` on sentinel/out-of-bound), `project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap` — iterate mask in order, gather `entity_to_row[e]` into a `Vec<u32>` skipping sentinels, radix/`sort_unstable`, `Bitmap::from_sorted` (design §10.4). Comment at the definition: *projection costs seconds at 10⁹ — cache per (token, slice, pin); never on the per-viewport path.* **Keep the backing representation private** — no method returning the raw `&[u32]`, no caller indexing it. It is a flat array today only because §11.1 deliberately keeps entity order and row order unrelated (leak C6); under a signature-major row layout it becomes near-monotone and wants Elias-Fano-class encoding (plan §14, probes/optimisations §4). The interface is what keeps that a build flag instead of a bundle-format break.
  - `pub fn tile_ranges(seg: &SegmentData, tile: &Tile) -> Range<u32>` — binary search `morton.u64` for the tile's code range (interface note: callers must treat a tile as resolving to a **set** of ranges — one per segment — even though Phase 1 has one segment; the engine-level signature is `Vec<Range<u32>>`).

- [ ] **Step 1: Failing test** (`bundle_read.rs`): hand-assemble a tiny bundle in a tempdir using Task 3/4/5 writers plus hand-written manifests (compute real digests with `sha2`), `open_bundle`, assert segments load, columns read back, `Permutation::project` of a small bitmap matches a per-entity `row_of` loop, and `tile_ranges` agrees with a linear scan of the morton array. Then corrupt one byte of `columns.arrow` and assert `open_bundle` fails with a digest error.
- [ ] **Step 2: Run (fail), implement, run (pass).** (The manifest structs get exercised properly in Task 8; here they may be hand-built in the test.)
- [ ] **Step 3: Commit.** `git commit -am "feat(store): bundle read protocol, zero-copy segment loader, Permutation (I4/I11)"`

---

### Task 8: `tessera-build` + `tessera-cli` — the batch build

**Files:**
- Create: `crates/tessera-build/src/lib.rs`, `crates/tessera-build/src/input.rs`; `crates/tessera-cli/src/main.rs` (subcommands `build`, `verify`, `serve` stub)
- Test: `crates/tessera-build/tests/build_smoke.rs`

**Interfaces:**
- Consumes: everything above + `tessera-plugin`'s `builtin:passthrough` + the allocator's *assignment rule* (signature sorting is implemented here for the bootstrap build; the serving allocator in Task 9 reuses the same free function).
- Produces:
  - `pub struct BuildArgs { points: PathBuf, pairs: PathBuf, out: PathBuf, extent: Extent, slice_id: String, limit: Option<u64> /* entity_id < limit prefix filter */ }`
  - `pub fn build(args: &BuildArgs) -> Result<BuildReport>` and CLI `tessera build --points … --pairs … --out … --extent x0,x1,y0,y1 --slice 2026-07 [--limit N]`.
  - `pub fn signature_sort_key(terms: &[TermId]) -> Vec<u32>` — the item's **sorted term-id list**; batches are ordered by this key lexicographically, ties broken by external id. (This is §11.1's signature-sorted assignment — permanent under I9, the reason it ships now. Comment says so.)
  - CLI `tessera verify <bundle>` — runs the Task 7 read protocol + permutation bijectivity check, prints OK/failures.

- [ ] **Step 1: Failing smoke test** (250-item scale): synthesise tiny `points.parquet` + `pairs.parquet` in the test (via the `parquet` crate), run `build`, then `open_bundle` and assert: (a) manifest fields present and digests verify; (b) entity IDs are dense `0..n` and **grouped by signature** — items sharing a term set occupy contiguous ID ranges; (c) postings row count = dictionary len; (d) for a random term, its posting equals the set of new entity ids of items carrying it; (e) `pairs.parquet` is sorted by `(term_id, entity_id)`; (f) external-ids file maps source `entity_id` (as 8-byte LE `binary`) → new `EntityId`.
- [ ] **Step 2: Run (fail). Implement the pipeline:**
  1. Read `points` (record-batch streaming; apply `--limit`). Read `pairs` grouped by source entity id → per-item source-term list.
  2. Per item, synthesise `access` = comma-joined decimal source term ids → `terms_of_label` (passthrough) → descriptors → `DictWriter::intern` → `TermId`s.
  3. Sort items by `(signature_sort_key, source_id)`; assign `EntityId` = position (bootstrap allocates from zero; record `entity_id_high_water = n` in MANIFEST — seeds the serving allocator).
  4. `priority = R3(entity_id)`.
  5. Emit `pairs.parquet` (new ids, `(term, entity)`-sorted, DELTA_BINARY_PACKED via the `parquet` crate), postings (Task 5), dictionary, external-ids extent 0 (sorted by external id bytes).
  6. Tiler (Task 3) → one segment; `write_permutation` (bound = max entity + 1).
  7. Manifests: per-file sizes/digests, `SEGMENTS-0.json` with **every** contracts-§2.3 field so the oracle's parser and this writer agree: `segments_version: 0`, `watermark: n`, `entity_id_high_water: n`, `segments: [{slice, seg_id, row_count, entity_lo: 0, entity_hi: n-1}]`, `deltas: []`, `dict_extents: [{path, records}]`, `external_id_extents: [<path>]`, `tombstones: []`, `deny: []`, `files: {…}`, `MANIFEST.json` (all contracts §2.2 fields; `data_plugin_hash` from the passthrough; `small_term_threshold: 32`; `partitions: [{"phash": "default", "required_terms": []}]`; `provenance: {"generating_set_choice": "prompt-sample"}`), then `CURRENT` last, write-then-rename.
  Memory note: implement in-memory first (fine ≤ 2.4M); at 250M/10⁹ if RSS explodes, switch the signature sort to an external merge over disk chunks under `args.out/tmp/` — report before doing so.
- [ ] **Step 3: Run smoke test (pass). Then real-data check at 250k:** `cargo run --release -p tessera-cli -- build --points data/scaled/geometry.parquet --pairs data/scaled/pairs.parquet --limit 250000 --extent <from scales.json/dataset.md> --slice s0 --out /tmp/tessera-250k && cargo run --release -p tessera-cli -- verify /tmp/tessera-250k`. Expected: verify OK. Record wall time in the task log.
- [ ] **Step 4: Commit.** `git commit -am "feat(build): tessera build batch mode with signature-sorted entity allocation (I9/§11.1)"`

---

### Task 9: `tessera-lifecycle` — WAL, entity-ID allocator, replay

**Files:**
- Create: `crates/tessera-lifecycle/src/wal.rs`, `crates/tessera-lifecycle/src/alloc.rs`, `crates/tessera-lifecycle/src/lib.rs`
- Test: `crates/tessera-lifecycle/tests/wal.rs`, `crates/tessera-lifecycle/tests/alloc_props.rs`

**Interfaces:**
- Consumes: `tessera-types`, `postcard`, `crc32fast`.
- Produces:
  - `pub enum WalRecord { IngestBatch { batch_id: String, body_hash: [u8;32], rows: Vec<WalRow> }, Change { external_id: Vec<u8>, op: ChangeOp, descriptors: Option<Vec<Vec<u8>>> }, Lease { lo: u64, hi: u64 } }` with `WalRow { external_id: Vec<u8>, entity_id: EntityId, descriptors: Vec<Vec<u8>>, x: f32, y: f32, scalars: … }` and `ChangeOp { Predicate, Delete, Suppress, Unsuppress }`. **WAL rows record their allocated entity IDs; replay reuses them and never re-allocates** (SA §6.2). **The WAL records term *descriptors* (raw bytes), never `TermId`s:** term IDs are bundle-relative ordinals and the bundle's dictionary extents are immutable (contracts §2.4 — new extents are written at flush, which Phase 1 defers), so a novel descriptor has no durable ID. On replay (and on live accept), descriptors resolve through the bundle dictionary plus a deterministic in-memory extension interned in replay order; a term that entered only in memory is unsatisfiable by any session until the next `tessera build` folds it in — fail-closed and stated (see Open Question 5).
  - `pub struct Wal` — `open(path) -> Result<(Wal, Vec<WalRecord>)>` (replay on open), `append(&mut self, rec: &WalRecord) -> Result<()>` (buffered), `fsync(&mut self) -> Result<u64>` (returns durable offset). Record framing: `u32 LE len ‖ postcard bytes ‖ u32 LE crc32(postcard bytes)`.
  - **Positional CRC rule** (lifecycle §4, verbatim behaviour): `open` tracks the last-fsync offset persisted in a tiny sidecar `wal.sync` (8-byte LE offset, written+fsynced after each WAL fsync). During replay: a CRC/framing failure **past** that offset → truncate there (never acked); a failure **at or below** it → return `Err(WalCorruption)` and the caller must stay unready (fail closed — acked state, possibly denies, is damaged).
  - `pub struct Allocator` — `new(high_water: u64)` (seeded from MANIFEST/SEGMENTS at boot, then advanced past replayed WAL rows/leases), `allocate(&mut self, n: u64) -> Range<u64>` (monotone, never reuses), `high_water()`. Batch assignment helper `pub fn assign_sorted(items: &mut [PendingItem], alloc: &mut Allocator)` — sorts by `(signature_sort_key, external_id)` (same free function as Task 8) then assigns sequential IDs.
- Ack contract (wired in Task 13): **WAL append → fsync → in-memory apply/swap → 200.** Never ack before fsync.

- [ ] **Step 1: Failing WAL tests:** (a) append 3 records, fsync, reopen → 3 records identical; (b) append 2, fsync, append 1 **without** fsync, corrupt the last record's CRC byte on disk, reopen → 2 records, no error (tail truncation); (c) corrupt record 1 of 3 (below the sync point), reopen → `Err(WalCorruption)`.
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Allocator property tests** (`proptest`, the I9 fuzz from plan §10.2): interleave `allocate` calls, simulated crashes (drop allocator, rebuild from `max(manifest_hw, replayed rows/leases)`), and assert (a) strict monotonicity, (b) no ID ever handed out twice across crashes, (c) `assign_sorted` groups identical signatures contiguously.
- [ ] **Step 4: Run (pass). Commit.** `git commit -am "feat(lifecycle): WAL with positional CRC fail-closed rule, I9 allocator with signature-sorted assignment"`

---

### Task 10: `tessera-lifecycle` + `tessera-engine` — buffer, overlay, watermark, I1 composition

**Files:**
- Create: `crates/tessera-lifecycle/src/overlay.rs`, `crates/tessera-lifecycle/src/buffer.rs`; `crates/tessera-engine/src/compose.rs`, `crates/tessera-engine/src/lib.rs`
- Test: `crates/tessera-engine/tests/compose.rs`

**Interfaces:**
- Consumes: Tasks 6, 7, 9.
- Produces:
  - `Overlay` — `FxHashMap<EntityId, OverlayEntry>` where `OverlayEntry { deleted: bool, suppressed: bool, evaluate_terms: Option<Vec<TermId>> }` — **three independent facts, never one overwritable disposition** (lifecycle §3.1's three retirement rules; conflating them was caught in review twice — CLAUDE.md). Replay semantics: `Delete` sets `deleted = true` — **terminal in Phase 1**: nothing clears it (deletion denies retire only via the epoch ledger, which does not exist until compaction lands); `Suppress` sets `suppressed = true`; `Unsuppress` clears `suppressed` **only** (it must never clear `deleted` — the sequence `delete → suppress → unsuppress` re-exposing a deleted item is the fail-open this structure exists to prevent); `Predicate` sets `evaluate_terms` (and must never clear either deny flag). Resolution order when composing: `deleted` > `suppressed` > `evaluate_terms`. Addressed by external id via the external-ID map (bundle extent 0 + WAL rows); a change for an unknown external id → error `404 unknown`.
  - `IngestBuffer` — replayed `WalRow`s not yet in any segment: `entity_id → (terms: Vec<TermId>, x, y, scalars)` — term IDs resolved from the row's stored descriptors via the bundle dictionary plus the deterministic in-memory extension (Task 9). (Phase 1 has no flush; buffered items are durable and participate in authorisation state but have no row geometry until the next `tessera build` — stated Phase 1 limitation, plan §5's WAL rationale is durability semantics, not visibility latency.)
  - `tessera_engine::compose::EffectiveMask` — the I1 composition `M_auth = (fragment \ L) ∪ direct_eval(L)`, `L` = overlay ∪ {entities ≥ fragment watermark}, evaluated **as diffs over the cached row-space projection** (equivalence with entity-space composition is exactly what the differential oracle checks):
    ```rust
    pub struct EffectiveMask {
        base: Arc<RowProjection>,        // cached projection of the frozen fragment (per token+slice+pin)
        minus: croaring::Bitmap,          // rows REMOVED from base:  rows(fail(L)) ∩ base
        plus: croaring::Bitmap,           // rows ADDED beyond base:  rows(pass(L)) ∖ base
    }
    impl EffectiveMask {
        pub fn count_range(&self, r: Range<u32>) -> u64;   // base.range_cardinality − |minus∩r| + |plus∩r|
        pub fn iter_range(&self, r: Range<u32>) -> impl Iterator<Item = u32> + '_;  // merged, ordered
        pub fn contains_row(&self, row: u32) -> bool;
    }
    pub fn compose(fragment: &FrozenFragment, satisfied: &FxHashSet<TermId>,
                   overlay: &Overlay, buffer: &IngestBuffer,
                   base: Arc<RowProjection>, perm: &Permutation) -> EffectiveMask
    ```
    **Rules — these define the diff exactly; the differential oracle (Task 14) checks their equivalence with I1's entity-space form.** For each entity *e* in `L` = keys(overlay) ∪ buffered entities ≥ the **fragment's own** watermark (pins never fix authorisation — I11/lifecycle §2.3), resolve **exactly once**, first matching rule wins:
    1. `deleted` → fails.
    2. `suppressed` → fails.
    3. `evaluate_terms = Some(t)` → passes iff `t ∩ satisfied ≠ ∅` (this replaces the fragment's verdict in **both directions**: an entity *not* in the fragment whose predicate change grants a satisfied term now **passes** — the evaluate-widening case; one in the fragment whose change revoked its terms now **fails**).
    4. buffered (≥ watermark, no overlay entry) → passes iff its terms ∩ `satisfied ≠ ∅`.

    Then, with `row(e)` = the pinned permutation's row (skip entities with no row): `minus = { row(e) : e fails } ∩ base` and `plus = { row(e) : e passes } ∖ base`. **Two structural invariants, asserted in debug builds and tested: `minus ⊆ base` and `plus ∩ base = ∅`.** Without the `∩ base` clamp, denying an entity the session's fragment never contained corrupts every count over its tile (−1 per range, possibly negative); without `∖ base`, an evaluate-pass already in the fragment double-counts. In Phase 1 buffered entities have no row, so rule 4 contributes nothing to `plus` — keep the code path and test it with a synthetic permutation anyway.
  - Generation model (lifecycle §1.1, slimmed): `tessera_engine::Generation { prefix: String, segments_version: u64, watermark: u64, bundle: Arc<Bundle>, overlay_version: u64, overlay: Arc<Overlay>, buffer: Arc<IngestBuffer> }` held in an `arc_swap::ArcSwap`. **Request ordering invariant, commented at the load site: a request loads the generation pointer exactly once, at request start, before acquiring any fragment or cache entry.**
- [ ] **Step 1: Failing composition tests:** build a 10k-entity in-memory fixture (postings, fragment, permutation identity-ish). Cases: (a) no overlay/buffer → counts equal the raw projection; (b) suppress one visible entity → its tile count drops by 1 immediately and `contains_row` is false; (c) unsuppress restores it; (d) an evaluate entry whose new terms no longer intersect → excluded; whose terms still intersect → included; (d2) **evaluate-widening**: an entity *outside* the fragment whose predicate change grants a satisfied term → included (appears in `plus`); (e) a buffered entity with a row in a synthetic permutation and intersecting terms → included; without intersecting terms → excluded; (f) deny beats evaluate: `delete` + later `predicate` granting satisfied terms → still excluded; (f2) **out-of-fragment deny**: suppress an entity the session's fragment does not contain → every count identical to (a), byte-for-byte (the `minus ⊆ base` clamp); (f3) **cross-cause sequences**: `delete → suppress → unsuppress` → still excluded; `suppress → delete → unsuppress` → still excluded; (g) count_range equals a brute-force loop over `iter_range` on 200 random ranges; (h) the two structural invariants (`minus ⊆ base`, `plus ∩ base = ∅`) hold across all of the above.
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Restart-replay unit test** (the conformance test's engine-level half): apply suppress + delete changes through the WAL, rebuild `Overlay` from a fresh replay, assert every deny survives and composition still excludes them — **including the cross-cause sequences replayed in order**: `delete X → suppress X → unsuppress X` (X stays excluded) and `delete Y → predicate Y` with terms the session satisfies (Y stays excluded).
- [ ] **Step 4: Commit.** `git commit -am "feat(engine,lifecycle): overlay, buffer, I1 composition with fragment-owned watermark"`

---

### Task 11: `tessera-engine` — authorise, tokens, the viewport query

**Files:**
- Create: `crates/tessera-engine/src/session.rs`, `crates/tessera-engine/src/viewport.rs`
- Test: `crates/tessera-engine/tests/viewport.rs`

**Interfaces:**
- Consumes: Tasks 2, 6, 7, 10; `tessera-plugin` (passthrough).
- Produces:
  - `Engine::open(bundle_root, cache_dir, wal_path, plugin, config) -> Result<Engine>` — runs the read protocol, replays WAL, seeds allocator (`max(manifest high-water, WAL)`), builds the `Generation`.
  - `Engine::authorise(&self, auth_data: &[u8]) -> Result<Session>` — plugin `terms_of_auth` → `Dict::lookup` each descriptor (unknown descriptors are simply unsatisfied — not an error) → `FragmentCache::get_or_build` → `Session { token_id, satisfied: FxHashSet<TermId>, fragment: Arc<FrozenFragment>, handles: Mutex<HandleState-owned-by-wire>, expires_at }`. Token string: 32 random bytes hex (from `getrandom`); `expires_at = now + config.token_max_lifetime`. Zero terms → valid zero-visibility session.
  - `Engine::viewport(&self, session, slice, zoom: u8, bbox: [f64;4], k: usize, pin: Option<PinId>) -> Result<ViewportOut>` where `ViewportOut { pin: PinId, tiles: Vec<TileCount { tile: u64, visible: u64, matched: u64 }>, points: Vec<PointOut { entity_id: EntityId, x: f32, y: f32, scalars: … }> }` (entity IDs leave the engine only into `tessera-wire`, which translates — the engine's output type is not serialisable by design; no `serde` derive).
  - Query steps (design §2.6, retrieve 1–9): load generation once → resolve/validate pin. **A pin is `(prefix, segments_version)` — geometry identity only, never `overlay_version`** (I11 + lifecycle §2.3: pins fix geometry, never authorisation). A presented pin whose `(prefix, segments_version)` doesn't match a live generation → `PinExpired` → HTTP 410; an overlay swap (any accepted change) must **not** invalidate outstanding pins — a suppression applies to a pinned request the moment it is accepted, and the pinned request still succeeds → get-or-build the **row projection** of the session's fragment (cache key: (token_id, slice, segments_version); built via `Permutation::project`, `rayon` where useful) → `compose` (Task 10) → `tiles_for_bbox(bbox, zoom)` → per tile: ranges from `tile_ranges` (a `Vec<Range<u32>>` — iterate all), `visible = Σ count_range`, skip empty → **placeholder sampler: the first k visible row IDs in range order** (`iter_range(...).take(k)`) — *deliberately naive and obviously wrong (plan §5): first-k is not the priority-sample definition; Phase 2's differential test must disagree with it. Comment this loudly.* → collect selected rows across tiles (Morton order keeps them sorted) → gather x/y/scalars through `ColumnsRef` → map rows → entity IDs via the `entity_id` column.
- [ ] **Step 1: Failing integration test** (fixture bundle built by Task 8's smoke path at ~10k synthetic items): (a) full-coverage session sees every point of a bbox (small k caps it; tile `visible` counts equal brute force from the test's own pairs data); (b) a session satisfying one term sees exactly that term's items; (c) zero-term session: all counts 0, no points; (d) suppress an item via overlay → viewport count drops; (e) `matched == visible` everywhere; (f) sampler returns the **first** k in row order (assert exact row ids — proves the placeholder is the placeholder).
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Latency sanity at 2.4M** (ignored test, `--ignored`): build `/tmp/tessera-2m4` if absent, random 300-tile viewports, warm token, assert p99 < 50 ms (generous local gate; the real 10 ms gate is Task 16 at 10⁹).
- [ ] **Step 4: Commit.** `git commit -am "feat(engine): authorise and masked viewport query with placeholder first-k sampler (I7 placeholder)"`

---

### Task 12: `tessera-wire` — handles and Arrow payloads

**Files:**
- Create: `crates/tessera-wire/src/handles.rs`, `crates/tessera-wire/src/payload.rs`, `crates/tessera-wire/src/lib.rs`
- Test: `crates/tessera-wire/tests/wire.rs`

**Interfaces:**
- Consumes: `tessera-types`, `arrow`; engine output types.
- Produces:
  - `handles::HandleTable` (per session, behind `parking_lot::Mutex`): `handle_for(&mut self, e: EntityId) -> Handle` (stable within session; sequential mint), `entity_of(&self, h: Handle) -> Option<EntityId>`. **`EntityId` is importable in this module only within the crate; `payload` accepts `Handle` and plain columns exclusively** (I10; the Task 0 layer check greps for this). Module comment required: sequential per-session mint leaks only within-session visit order, which the viewer already observes; the per-session **keyed permutation** encoding of SA §4.5 arrives with router/worker fan-out, not Phase 1.
  - `payload::viewport_ipc(tile: &[u64], visible: &[u64], matched: &[u64], points_handles: &[u32], xs: &[f32], ys: &[f32], scalars: …) -> Vec<u8>` — plain slices, deliberately: no engine types cross into `tessera-wire`, keeping the crate free of direct `tessera-engine`/`tessera-store` edges (`tessera-server` destructures the engine's `ViewportOut` and mints handles via the table). Arrow IPC **stream** with the two R5 batches (schema names exactly `tile`, `visible`, `matched`; `handle`, `x`, `y`, …).
- [ ] **Step 1: Failing tests:** (a) handle stability + per-session isolation (two tables, same entity → independent handles); (b) `entity_of` of an unminted handle → `None`; (c) encode a viewport payload, decode with `arrow_ipc::reader::StreamReader`, assert schemas and values; (d) **byte-scan unit test**: for entities with ids `{0xDEADBEEF, 7, 1_000_000}`, assert the 8-byte LE encoding of each id does not appear in the payload bytes.
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Commit.** `git commit -am "feat(wire): per-session handle tables and Arrow IPC payloads (I10)"`

---

### Task 13: `tessera-server` — the three planes, config, `tessera serve`

**Files:**
- Create: `crates/tessera-server/src/{lib,config,viewer,session,control}.rs`; extend `crates/tessera-cli/src/main.rs` (`serve -c tessera.toml`)
- Test: `crates/tessera-server/tests/http.rs`

**Interfaces:**
- Consumes: Tasks 11, 12.
- Produces:
  - `Config` from `tessera.toml` per SA §7: `[bundle] path, cache, wal`; `[plugin] module = "builtin:passthrough"`; `[disclosure] min_visible_members, token_max_lifetime` — **no defaults; absence of either is a startup error naming design §7.5 / §2.3** (min_visible_members is parsed and stored though nothing consumes it until Phase 3 — the startup rule is the point); `[serve] viewer, session, control, max_k = 200`. Session credential and operator credential from files/env (`session_credential_file` etc.), never inline.
  - Three listeners (axum): viewer (R5 endpoints; bearer = session token), session (`authorise`/`revoke`; bearer = session credential), control (unix socket; `ingest`/`changes`/`status`; bearer = operator credential). Error mapping per R5's closed code list. `/healthz`, `/readyz` (ready = bundle verified + WAL replayed + plugin loaded).
  - Control handlers own the **ack contract**: parse → allocate IDs (`assign_sorted`) → WAL append → fsync → apply to buffer/overlay + generation swap (bump `overlay_version`) → 200. `changes` is never 429. Ingest batch-id idempotency: replay of an acked id with equal body hash → 200 (no effect); different hash → 409. Ingest 200 body: `{accepted, over_bound, over_bound_ids: [first 100]}` — over-bound items are indexed regardless (bounds warn, never exclude — design §6.2 r16). **Deny-op append failure** (lifecycle §4): if the WAL append/fsync for a `delete`/`suppress` genuinely fails, still apply it to the in-memory overlay and swap (visible immediately), return **500 with an alarm log** — durability is owed and the caller must retry; never a 200 without fsync, never a refusal that leaves the item visible. Deferral note: contracts §2.3's immediate side-manifest publication on accepted deny ops is not implemented — Phase 1 writes no side-manifests at serve time and has no syncing replicas; contracts §0.4 phases that machinery with streaming ingest.
  - `tracing` with a strict rule tested later: **no tokens, no auth data, no entity IDs in log output.**
- [ ] **Step 1: Failing HTTP test** (`http.rs`, spawn the server in-process on port 0 against the 10k fixture bundle): (a) authorise → token; viewport → 200, Arrow decodes, counts match Task 11's expectations; (b) missing/garbage token → 401; (c) revoke → subsequent viewport 403/401; (d) `viewport` with unknown slice → 404 `unknown`; malformed bbox → 422 `contract`; (e) `/control/changes` suppress by external id → next viewport count drops (no re-authorise needed); (f) ingest a small Arrow batch → 200; repeat same batch-id+body → 200 idempotent; same id different body → 409; (g) stale pin (fabricated `(prefix, segments_version)`) → 410 `pin-expired`; (g2) **pins survive overlay swaps**: take a pin from a viewport response, suppress an item via `/control/changes`, re-query **with the pin** → 200 (not 410) and the count reflects the suppression (pins fix geometry, never authorisation); (h) config missing `[disclosure]` → process refuses to start (assert on a spawn helper).
- [ ] **Step 2: Run (fail), implement, run (pass).**
- [ ] **Step 3: Commit.** `git commit -am "feat(server): viewer/session/control planes, fail-closed config, WAL-before-ack"`

---

### Task 14: `reference/` — the Python differential oracle

Deliberately slow, obviously correct, independently derived (plan §10.3). It re-implements the *definitions* (quantisation R2, priority R3, mask = set union from `pairs.arrow`, count = brute-force loop) — it must NOT call the Rust code or share logic with it.

**Files:**
- Create: `reference/pyproject.toml` (or plain scripts + `requirements.txt`), `reference/oracle/__init__.py`, `reference/oracle/{morton.py,bundle.py,mask.py,viewport.py}`, `reference/tests/test_differential.py`, `scripts/setup-reference-venv.sh` (`uv venv --python 3.12 .venv && uv pip install pyroaring pyarrow polars numpy pytest requests`)

**Interfaces:**
- Produces:
  - `morton.py`: `cell`, `interleave`, `priority` (R2/R3, from scratch, numpy allowed).
  - `bundle.py`: parse `CURRENT`/manifests (verify digests), `permutation.bin`, `morton.u64`, dictionary extents, `postings.arrow` tagged records (tag 1 via `pyroaring.BitMap.deserialize` — this doubles as the portable-format cross-check), `columns.arrow`.
  - `mask.py`: `mask_of(terms, pairs_path) -> set[int]` — from **pairs**, not postings (independent derivation): scan `pairs.parquet` (pyarrow), collect entity ids whose term is granted.
  - `viewport.py`: `counts(bundle, mask, slice, zoom, bbox) -> dict[tile, int]` by brute-force loop over every row's morton code; `first_k(bundle, mask, tile, k) -> list[rows]`.
- [ ] **Step 1: Failing differential test** (`test_differential.py`, against `/tmp/tessera-250k` from Task 8 — build it first if absent; server spawned via a fixture): for 20 random grant sets (mixed sizes, incl. one empty and one huge) × 10 random viewports at zooms 3–8: (a) `morton.u64` bytes == oracle's recomputation from `columns.arrow` x/y (byte-for-byte — the contracts §2.5 obligation); (b) postings-derived server counts == oracle's pairs-derived brute-force counts for every tile (this is the union-vs-semi-join differential, and the entity-space vs row-space-diff composition equivalence check); (c) point sets: server handles are opaque — compare `(x, y)` multisets of returned points against oracle `first_k`; (d) suppress an item over the control plane → oracle told to drop it → counts re-agree; (e) **composition equivalence stress**: apply a batch of mixed changes (delete, suppress, predicate-widen onto an entity the grant set previously missed, predicate-narrow) and re-compare all counts — the oracle composes in entity space per I1's formula and projects, the server composes as row-space diffs; agreement here is the equivalence proof for Task 10's rules.
- [ ] **Step 2: Run (fail while wiring, then pass).** `cd reference && .venv/bin/pytest tests/ -v`
- [ ] **Step 3: Commit.** `git commit -am "test(reference): independent Python oracle and differential harness"`

---

### Task 15: `conformance/` — Phase 1 invariant tests

**Files:**
- Create: `conformance/README.md`, `conformance/tests/{test_byte_scan.py,test_restart_replay.py,test_canary.py}` (pytest, same venv as `reference/`; drives the release binary as a subprocess)

**Interfaces:**
- Consumes: the `tessera` binary, the fixture + 250k bundles, `reference/oracle`.
- Produces: the Phase 1 slice of plan §10.2's matrix — I10 byte-scan, restart-replay (deny survival), I2 canary scaffold, plus the I9 fuzz already living in Task 9 and the I4 compile-rule in Task 1.
- [ ] **Step 1: `test_byte_scan.py` (I10, plan §10.2):** authorise; fetch viewports covering every tile of the fixture; using the oracle, compute the entity ids of every item the mask admits; assert no 8-byte LE encoding of **any** of them (nor of any denied item's id) appears in any response body **or in the server's log output** (run the server with logs captured). Known-limitation note in the file: absence-of-encoding is necessary, not sufficient; the handle-table code review is the other half.
- [ ] **Step 2: `test_restart_replay.py` (plan §10.3's restart-replay test):** start server → ingest a batch → suppress two built items + delete one → assert all three invisible → `SIGKILL` the process → restart on the same WAL → assert without any re-submission: all three still invisible, ingested batch replays (status shows buffered rows), allocator high-water unchanged, and a repeat of the acked batch-id is still idempotent.
- [ ] **Step 3: `test_canary.py` (I2 scaffold):** rebuild the fixture bundle with one extra item at extreme coordinates carrying a term **no test principal is granted**; assert across every zoom: all tile counts, the tile *list*, and every response byte are identical to a session on the canary-free bundle except where the oracle says visible content differs (practical form: canary's tiles report 0 and never appear; counts elsewhere unchanged). This is the scaffold Phase 2 extends to centroids/hulls.
- [ ] **Step 4: Run all (pass): `reference/.venv/bin/pytest conformance/tests -v`. Commit.** `git commit -am "test(conformance): I10 byte-scan, restart-replay deny survival, I2 canary scaffold"`

---

### Task 16: Benchmarks and the 10⁹ exit criteria

**Files:**
- Create: `crates/tessera-engine/benches/viewport.rs` (criterion), `scripts/bench_p99.py`, `scripts/build_full.sh`

- [ ] **Step 1: Criterion micro-benches** at 2.4M (fragment build for w ∈ {10², 10⁴}; compose; 300-tile count sweep; gather of 300×30 rows). Regression gate: commit the baseline JSON.
- [ ] **Step 2: Build the 10⁹ bundle.** `scripts/build_full.sh` → `tessera build` with no `--limit` onto the largest free disk. **Check free space first** (`df`); the bundle is ~60 GB (columns ~22 GB, morton 8 GB, permutation 4 GB, external-ids ~20 GB, pairs-as-Parquet ~6 GB, postings 1–4 GB) alongside 18 GB of source parquet — if short, delete scratch bundles and consult the owner before deleting anything in `data/`. Record build wall time and peak RSS in `docs/superpowers/plans/phase1-results.md`. If memory forces the external-sort fallback in Task 8, implement it now (report first).
- [ ] **Step 3: `scripts/bench_p99.py` — the exit measurement** (plan §5): serve the 10⁹ bundle; authorise a realistic principal (w = 10⁴ random grants — reuse the grant-construction recipe in `probes/mask_probe.py`); one warm-up pass (fragment build + row projection are per-token one-offs — report their times separately; they are NOT in the viewport budget); then ≥ 2,000 random-pan viewports (~300 tiles each, mixed zooms, k = 30) over HTTP; report p50/p99/max server-side and end-to-end. **Exit gate: server-side p99 < 10 ms.** Also record: fragment cardinality, frozen file size (the handoff's unmeasured number — record it in phase1-results.md), row-projection time, RSS.
- [ ] **Step 4: Run the full conformance + differential suites once against the 10⁹ server** (differential at reduced sample count — 5 grants × 5 viewports; the oracle is slow by design).
- [ ] **Step 5: Write `docs/superpowers/plans/phase1-results.md`** — every number from steps 2–4 against each plan-§5 exit criterion, pass/fail, plus any deviations taken. Commit everything. `git commit -am "test(bench): 10^9 exit-criteria measurement"`

---

## Phase 1 exit checklist (plan §5)

- [ ] p99 viewport latency < 10 ms server-side against the 10⁹ corpus with a real (w=10⁴) mask applied — measured, recorded.
- [ ] Zero entity IDs observable in any wire payload or log — byte-scan test green, not asserted.
- [ ] Signature-sorted entity allocation in effect (Task 8 assertion (b) + Task 9 property (c)).
- [ ] WAL ack contract + positional CRC rule + restart-replay deny survival green.
- [ ] Differential oracle agrees on counts, geometry bytes, and first-k across random grants/viewports.
- [ ] Placeholder sampler is first-k and commented as deliberately wrong.
- [ ] `tessera verify` passes on every built bundle; corrupted-byte test red-paths green.
- [ ] Layer checks green: authz never sees RowId/store; only store exports Permutation; payloads accept Handle only.

## Open questions raised to the owner (do NOT resolve silently)

1. **RESOLVED (owner, 2026-07-28): the pair relation is `pairs.parquet`** — Parquet, sorted `(term_id, entity_id)`, `DELTA_BINARY_PACKED`. Contracts §2.4 specified Arrow IPC with a Parquet-only encoding (internally inconsistent; the measured 3.8×/3× figures were Parquet's); the file is off both request paths (build-cadence + oracle reads only), so the uncompressed-mmap rule doesn't apply, and DuckDB/pyarrow read Parquet natively. Amend contracts §2.4 accordingly (its own §0.3 deviation style). Saves ~15 GB at 10⁹.
2. **RESOLVED (owner, 2026-07-28): priority = splitmix64-high-16 over the entity ID**, now normative in contracts r4 §2.6 (R3 above mirrors it). Changing it is a `bundle_format` bump.
3. **CONFIRMED (owner, 2026-07-28): the wasmtime host lands in Phase 2** with the conformance suite (the I6 sandbox test needs it there anyway); Phase 1 ships the trait + native `builtin:passthrough` only.
4. **CONFIRMED (owner, 2026-07-28): ingest visibility deferred as read.** Phase 1's WAL is about deny durability, not visibility latency; ingested items gain geometry at the next `tessera build`. Streaming flush is explicitly assigned to Phase 2 (implementation plan §6, amended).
5. **CONFIRMED (owner, 2026-07-28): the fail-closed novel-descriptor choice stands** (Task 9): WAL stores descriptors as raw bytes; deterministic re-intern above the bundle dictionary on replay; in-memory-only terms unsatisfiable until the next build.
6. **RESOLVED (owner, 2026-07-28): cache key widened** — design r19 §2.3 now names the postings identity (manifest digest; partition + postings-epoch under fan-out) as the third canonical-key component. Task 6 implements it.
7. **CONFIRMED (owner, 2026-07-28): serve-time deny publication deferred as part of the Phase 2 publication package** — streaming flush + side-manifest publication + immediate-deny rule + `readyz` freshness ship as one unit (implementation plan §6, amended); none without the others.
