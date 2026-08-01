> **ARCHIVED 2026-08-01 — EXECUTED as a probe campaign. Morton u64→u32 narrowing landed at 2a399b5; the harness lives in probes/markbudget/.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# Drawn-Mark Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Find where Tessera's drawn-mark budget actually binds — GPU, transport, handle table — set the server-side `k` cap from the measured minimum, decide the handle representation, and land the `morton` u64→u32 narrowing before the 10⁹ bundle is built.

**Architecture:** Four workstreams that share one number. Two documentation/format tasks that must land before Phase 1's 10⁹ build (§8 corrections, `morton.u32`); one self-driving browser probe harness (a Rust collector in `probes/markbudget/` that serves the probe pages, launches Windows Chrome from WSL2, samples the RTX 3080 through `nvidia-smi.exe`, and writes results JSON) carrying P1 and P2; one pure-Rust model carrying P3; then the calibration and the decision memo that consume all three.

**Tech Stack:** Rust 2021 (workspace crates + an excluded `probes/markbudget` Cargo project), axum 0.8, arrow 59 (with `ipc_compression`), Node 22 + esbuild for the browser bundles, deck.gl 9 (`ScatterplotLayer`, `TextLayer`, `CollisionFilterExtension`), `apache-arrow` JS, Python 3.10 for the reference oracle.

**Source spec:** [docs/archive/plans/2026-07-29-drawn-mark-budget-design.md](docs/archive/plans/2026-07-29-drawn-mark-budget-design.md)

---

## Global Constraints

Every task's requirements implicitly include this section.

- **Design corpus lives in `docs/design/`,** which default file-search tooling skips — always pass the path explicitly. Precedence: architecture design (**r19**) > contracts spec (**r4**) > system architecture (**r4**); lifecycle design r3 for WAL/overlay mechanisms. `§n` unprefixed means the architecture design.
- **If plan and spec disagree, STOP and report to the owner.** Do not resolve silently. Every design document carries an Appendix R review trail — read it before re-litigating a decision.
- **Never `git add -A` or `git commit -am`.** The working tree carries untracked owner files (`docs/archive/whitepaper/`, `docs/tessera-*.html`, `docs/archive/reference/`, `docs/archive/plans/`, `docs/evidence/memos/`) and in-flight Phase 1 Task 16 work (`crates/tessera-engine/benches/`, the `x-tessera-server-us` header in `crates/tessera-server/src/viewer.rs`, `Cargo.lock`, `crates/tessera-engine/Cargo.toml`, the `probes/optimisations.md` §4 expansion, the `docs/design/implementation-plan.md` §14 expansion, the plan §5 permutation note). **Every commit step in this plan names its paths explicitly.** Leave everything it does not name alone.
- **Owner decisions already taken for this plan, do not re-open:**
  1. `morton` u64→u32 and the §8 doc corrections land **before** Phase 1 Task 16's 10⁹ build, so the 60 GB bundle is built once against the final format.
  2. **Handle validity is required only for as long as the client holds the tile carrying it** — not across pan-away-and-return within a session. This settles the spec §5 open question and makes representation **B** admissible. All three representations are still measured.
  3. Probes are self-driving: a WSL2-hosted collector serves the page and receives a results POST; Windows Chrome is launched from WSL2 against it.
- **Rust is the implementation language** for engine, build and serving. Python is a first-class *consumer* (SDK, supervisor, test-only reference oracle) and never a component. The probe harness is measurement tooling, not a component — but it is written in Rust anyway so that P2 measures the *real* `tessera_wire::payload::viewport_ipc` bytes rather than a lookalike.
- **`probes/markbudget/` is excluded from the root Cargo workspace.** It is measurement tooling; it must not appear in `cargo build --workspace`, must not be caught by `scripts/check-layers.sh`, and must not change any shipped crate's dependency graph.
- **Quality gates at every task boundary that touches Rust:** `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `bash scripts/check-layers.sh` — all clean before committing.
- **Invariants that bear on this plan.** **I10** — entity IDs never cross the trust boundary; clients get per-session opaque handles. **I2** — every aggregate computable from inside `M_auth` alone. Any change to handle representation is I10-adjacent: option C converts `404 unknown` from "nothing is enumerable" to "everything decodes, then is mask-checked", and **must not be adopted on performance grounds alone**.
- **British spelling** (*authorisation*, *visualisation*, *licence*) throughout prose and comments.
- **Hardware for all probes:** the existing box — RTX 3080 10 GB, ~47 GB RAM, 12 cores, WSL2, browser on the Windows host. Verified available: `nvidia-smi.exe` at `/mnt/c/Windows/System32/nvidia-smi.exe` reports the RTX 3080; Chrome at `/mnt/c/Program Files/Google/Chrome/Application/chrome.exe`; WSL2 IP from `hostname -I`. **Disk is tight: 67 GB free on `/` against a ~60 GB 10⁹ bundle** — check `df -h` before anything that writes at scale and report to the owner rather than deleting.

---

## File Structure

**Task 1 — §8 corrections (docs only)**
- Modify: `docs/design/architecture.md` — Appendix A (two rows), §13.2, Appendix G (r20 entry)
- Modify: `probes/optimisations.md` — §4.4 trigger

**Task 2 — `morton` u64 → u32**
- Modify: `docs/design/contracts.md` — §0.3 deviation, §2.1 tree, §2.5, §2.6, revision block (r5)
- Modify: `crates/tessera-spatial/src/tiler.rs` — `sort_batch` returns `Vec<u32>`
- Modify: `crates/tessera-store/src/write.rs` — `write_morton_u32`, file renamed `morton.u32`
- Modify: `crates/tessera-store/src/read.rs` — `MortonSlice` over `u32`, `tile_ranges` widening fix
- Modify: `crates/tessera-store/src/lib.rs`, `crates/tessera-store/src/error.rs` — doc comments
- Modify: `crates/tessera-build/src/lib.rs` — two path literals
- Modify (tests): `crates/tessera-store/tests/segment_roundtrip.rs`, `crates/tessera-store/tests/bundle_read.rs`, `crates/tessera-build/tests/build_smoke.rs`
- Modify: `reference/oracle/bundle.py`, `reference/tests/test_differential.py`

**Task 3 — probe harness skeleton**
- Modify: `Cargo.toml` (root) — `[workspace] exclude`
- Create: `probes/markbudget/Cargo.toml`, `src/lib.rs`, `src/gpu.rs`, `src/bin/collector.rs`
- Create: `probes/markbudget/web/package.json`, `web/build.mjs`, `web/src/harness.js`, `web/src/smoke.js`, `web/smoke.html`
- Create: `probes/markbudget/run.sh`, `probes/markbudget/README.md`, `probes/markbudget/.gitignore`

**Task 4 — P1 GPU render ceiling**
- Create: `probes/markbudget/web/src/p1.js`, `web/p1.html`, `probes/markbudget/P1-render.md`
- Modify: `probes/markbudget/web/build.mjs` (entry point)

**Task 5 — P2 transport and decode ceiling**
- Create: `probes/markbudget/src/points.rs`, `web/src/p2.js`, `web/p2.html`, `probes/markbudget/P2-transport.md`
- Modify: `probes/markbudget/src/bin/collector.rs` (`/api/points` route), `Cargo.toml` (arrow), `web/build.mjs`

**Task 6 — P3 handle table ceiling**
- Create: `probes/markbudget/src/handles.rs`, `src/bin/p3_handles.rs`, `probes/markbudget/P3-handles.md`

**Task 7 — calibration: set the `k` cap**
- Modify: `crates/tessera-server/src/config.rs` (`DEFAULT_MAX_K`), `docs/design/contracts.md` (§3 `k` note), `docs/archive/plans/2026-07-28-phase1-walking-skeleton.md` (Task 16 k-sweep)

**Task 8 — the §5 decision and the results record**
- Create: `docs/evidence/memos/2026-07-29-handle-representation.md`, `probes/markbudget/RESULTS.md`
- Modify: `crates/tessera-wire/src/handles.rs` (exhaustion guard + recorded decision), `docs/design/implementation-plan.md` (P4 schedule), `probes/optimisations.md` (§3.5 k range)

---

### Task 1: The corrections owed to the corpus

Spec §8 lists four corrections owed to the corpus; spec §7 states two more ("Two corrections fall out"). All six are independent of any probe result. They land first because they are cheap, they are prerequisites for nothing, and two of them change numbers that Task 8 will quote back.

**Files:**
- Modify: `docs/design/architecture.md` (Appendix A ×2, §13.2 at line 559, §10.5 at line 439, Appendix G)
- Modify: `probes/optimisations.md` (§3.2 heading, §4.4 trigger)

**Interfaces:**
- Consumes: nothing.
- Produces: a corrected Appendix A residency table that Task 8's `RESULTS.md` cites; a §13.2 that no longer asserts as settled the property P1/P2 are testing; a §10.5 that no longer reads as a per-viewport claim.

- [ ] **Step 1: Correct Appendix A's permutation double-count (both tables).**

The 10⁷ table currently reads:

```
| Permutation arrays, both directions | 80 MB |
```

Replace that row with:

```
| Permutation `entity_to_row` | 40 MB |
```

The 10⁹ table currently reads:

```
| Permutation arrays, both directions | 8 GB | 125 MB |
```

Replace that row with:

```
| Permutation `entity_to_row` | 4 GB | 62.5 MB |
```

Then, immediately after the 10⁹ table (before the "Mask construction costs…" paragraph), add:

```markdown
**One direction only.** Earlier revisions listed "permutation arrays, both
directions". Contracts §2.6 stores only `entity_to_row: u32 × bound`; the
row→entity direction *is* the `entity_id` column of `columns.arrow`, already
counted in the hot column set above. Counting it twice inflated the residency
figure by 4 GB at 10⁹.
```

- [ ] **Step 2: Mark §13.2's render-path claim as under test.**

Line 559 currently reads:

> The render path is invariant — a viewport shows a few thousand points at 10<sup>9</sup> exactly as at 10<sup>7</sup>. The tile scheme is invariant; tiles simply go deeper. And label generation cost is the caller's, so it does not appear here at all.

Replace the whole paragraph with:

```markdown
The tile scheme is invariant; tiles simply go deeper. And label generation cost
is the caller's, so it does not appear here at all.

**The render path's invariance is a claim under test, not a settled property.**
It held only under the assumption that a viewport draws a few thousand marks.
The owner decision of 2026-07-29 inverts that assumption — the drawn-mark budget
should be the largest a given client can render, plausibly 10<sup>7</sup> on a
capable GPU — which changes which reads dominate and which structures must be
resident (Appendix A). Probes P1 (GPU render) and P2 (transport and decode)
decide it; until they report, this paragraph claims nothing about the render
path at large *k*. See the drawn-mark budget spec.
```

- [ ] **Step 3: Qualify Appendix A's wire-payload figure.**

The 10⁷ table row currently reads:

```
| Wire payload, 50 k points | 0.6 MB |
```

Replace with:

```
| Wire payload, 50 k points *(assumed k; P2 replaces)* | 0.6 MB |
```

and add to the same trailing note block added in Step 1:

```markdown
**The wire figure is an assumption, not a measurement.** 0.6 MB at 50 k points
is 12 B/point arithmetic against an assumed mark budget, not an observed Arrow
IPC payload. Probe P2 measures bytes on the wire, transfer time and JS decode
time across the sweep and replaces this row with a measured figure at the
calibrated *k*.
```

- [ ] **Step 4: Align `probes/optimisations.md` §4.4's trigger to plan §14.**

§4.4 currently ends:

```markdown
**Trigger:** a deployment whose real-label signature histogram shows the
knee, *and* a working conformance suite. *Evidence:* results §3, §6;
scaling analysis §5.3. **Open decision:** plan §14.
```

Replace that block with:

```markdown
**Trigger** *(aligned to plan §14, 2026-07-29)*. Design r18 retired the
real-label rerun permanently — no real access-labelled corpus is available to
this project — so the signature histogram is **deployment guidance**, not a
gate this project can pass. The two live gates are:

1. **A working conformance suite** (Phase 2, plan §10.1). The whole-group
   visibility shortcut is sound only while a group is untouched by the overlay
   and the live set — invariant-bearing, and exactly the class of change that
   passes every functional test while leaking.
2. **The gather probe** (§3.5, re-scoped to large *k* by the drawn-mark budget
   spec's P4). Phase 0 measured no column read at all, so the retrieval half of
   the case is modelled.

A deployment that *does* have real labels re-runs the Phase 0 measurements and
checks its own signature histogram for the knee before enabling the layout.
*Evidence:* results §3, §6; scaling analysis §5.3. **Open decision:** plan §14.
```

- [ ] **Step 5: Correct §10.5's residency claim (spec §7).**

Spec §7: "§10.5's 'serving nodes hold the term index resident' reads as a per-viewport claim and is not one — mask build is per *session* and reads only the ~10⁴ postings the principal satisfies."

§10.5's second paragraph (line ~439) currently reads:

> Serving nodes hold the term index, the auth plugin's auxiliary structures and the text index resident for their partition. Token-to-mask state is held with LRU plus maximum-lifetime eviction, with the auth data retained alongside so eviction is transparent (§2.3).

Replace with:

```markdown
Serving nodes hold the term index, the auth plugin's auxiliary structures and the text index resident for their partition. Token-to-mask state is held with LRU plus maximum-lifetime eviction, with the auth data retained alongside so eviction is transparent (§2.3).

**Resident for the partition, not read per viewport** *(r20)*. The sentence above sizes what a node holds; it is not a claim about per-request cost. Mask build is **per session**, and it reads only the ~10⁴ postings the principal actually satisfies — not the index. Ordering the structures by access *cadence* rather than by size gives a much smaller per-viewport working set than this section's residency figure implies: `priority` is scanned per viewport, `morton` is touched sparsely (~30 pages per tile), the gather columns are per viewport but page-sparse at small *k*, and the term index and `permutation.bin` are per *session*. **At a large mark budget the gather columns join the per-viewport set** — which is what makes the drawn-mark budget a residency question and not only a latency one.
```

- [ ] **Step 6: Correct `probes/optimisations.md` §3.2's subject (spec §7).**

Spec §7: "probes §3.2's 'the permutation must stay cached' refers to the **projected mask** per *(token, slice, pin)*, not to `permutation.bin`, which is read once per session."

The body already says this; the **heading** is what misleads a reader into thinking `permutation.bin` is the cached object. Change the heading at line ~204 from:

```markdown
### 3.2 The permutation must stay cached — **DECIDED** *(serving)*
```

to:

```markdown
### 3.2 The *projected mask* must stay cached — **DECIDED** *(serving)*
```

and add, after the existing *Evidence:* line:

```markdown
**What is cached is the projected mask**, per *(token, slice, pin)* — not
`permutation.bin`, which is read once per session and never per viewport. The
earlier heading said "the permutation", which reads as the file. Clarified
2026-07-29; no decision changes.
```

- [ ] **Step 7: Add the r20 entry to Appendix G.**

Immediately above the existing `- **r19** —` bullet, add:

```markdown
- **r20** — Four corrections owed to the corpus by the drawn-mark budget spec
  (2026-07-29). Appendix A's permutation row counted both directions; contracts
  §2.6 stores only `entity_to_row`, and the row→entity direction is the
  `entity_id` column already counted in hot columns — 8 GB → 4 GB at 10⁹.
  Appendix A's 50 k-point wire figure is marked as an assumption pending P2.
  §13.2's "the render path is invariant" is demoted from settled property to
  claim under test: it held only under a few-thousand-mark viewport, and the
  owner decision of 2026-07-29 makes the mark budget as large as the client can
  render. §10.5's residency sentence gains a note that it sizes what a node
  holds and is not a per-viewport claim — mask build is per session and reads
  only the postings the principal satisfies. No invariant changes; no format
  changes.
```

- [ ] **Step 8: Verify the edits landed and nothing else moved**

Run:
```bash
git -C /home/joe/code/tessera diff --stat docs/design/architecture.md probes/optimisations.md
grep -n "both directions" docs/design/architecture.md
grep -n "claim under test" docs/design/architecture.md
grep -n "r20" docs/design/architecture.md
grep -n "not a per-viewport claim\|Resident for the partition" docs/design/architecture.md
grep -n "projected mask\* must stay cached" probes/optimisations.md
```
Expected: exactly two files changed; `grep "both directions"` returns **no hits inside Appendix A's tables** (it may still appear in the new explanatory note — that is correct); every other grep returns at least one hit.

- [ ] **Step 9: Commit**

```bash
cd /home/joe/code/tessera
git add docs/design/architecture.md probes/optimisations.md
git commit -m "docs: four corrections owed by the drawn-mark budget spec (design r20)"
```

Note: `probes/optimisations.md` already carries uncommitted owner edits to §3.5 and §4.1–§4.3. Committing the file commits those too — that is intended and was confirmed by the sequencing decision; do not try to stage a partial file.

---

### Task 2: `morton.u64` → `morton.u32`

Spec §2: the code is 32 bits because §5.2 fixes the grid at 2¹⁶ × 2¹⁶ — a property of the *grid*, not the population, so it does not change at 10¹⁰ or 10¹¹. Narrowing saves 4 GB at 10⁹ at zero decode cost.

**The format decision this task takes, and why.** `bundle_format` stays at **1**. Format 1 has never been published: Phase 1 is its only writer, and the only bundles in existence are this repo's test fixtures and scratch builds. The *file rename* (`morton.u64` → `morton.u32`) is what makes any pre-existing bundle fail closed rather than mis-parse: `open_bundle` joins the segment directory with the literal filename and verifies it against the manifest's file map, so a stale bundle's missing `morton.u32` is a typed `StoreError`, never a silent half-width read. Recording the rename as a contracts §0.3 deviation rather than a format bump is therefore the fail-closed choice, not the lax one. **If the reviewer disagrees, STOP and raise it — do not bump the format unilaterally.**

**The trap in this task.** `Tile::code_range()` returns `(u64, u64)` and *must keep doing so*: at depth 0 the exclusive end is `(0 + 1) << 32` = 2³², which does not fit in `u32`. Narrowing `code_range` to `u32` overflows at depth 0 and silently returns an empty range in release builds. The binary search widens each stored code instead.

**Files:**
- Modify: `docs/design/contracts.md`
- Modify: `crates/tessera-spatial/src/tiler.rs:47-72`
- Modify: `crates/tessera-store/src/write.rs:1,23,29-54,188-194`
- Modify: `crates/tessera-store/src/read.rs:1,150-190,431-490,861-868`
- Modify: `crates/tessera-store/src/lib.rs:1`, `crates/tessera-store/src/error.rs:37`
- Modify: `crates/tessera-build/src/lib.rs:308,343`
- Test: `crates/tessera-store/tests/segment_roundtrip.rs:2,64-65`, `crates/tessera-store/tests/bundle_read.rs:108-109,206-207`, `crates/tessera-build/tests/build_smoke.rs:238,544`
- Modify: `reference/oracle/bundle.py:4,40,210-226`, `reference/tests/test_differential.py:5,76-81`

**Interfaces:**
- Consumes: `tessera_spatial::morton::{Tile, morton_of, MortonCode}` (unchanged), `MortonCode::raw() -> u32`.
- Produces:
  - `tessera_spatial::tiler::sort_batch(items: &mut [TilerItem], extent: &Extent) -> Vec<u32>`
  - `tessera_store::write::write_segment(dir: &Path, items: &[TilerItem], codes: &[u32], scalar_schema: &[(String, ScalarType)]) -> io::Result<()>`
  - `tessera_store::read::MortonSlice::u32(&self) -> &[u32]` (replaces `u64()`); `MortonSlice::len()` unchanged in meaning
  - `tessera_store::read::tile_ranges(seg: &SegmentData, tile: &Tile) -> Range<u32>` (signature unchanged)
  - On-disk: `.../segments/<seg>/morton.u32`, raw sorted little-endian `u32`, no header, length = `row_count × 4`

- [ ] **Step 1: Write the failing tests**

Add to `crates/tessera-spatial/src/tiler.rs`, inside the existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn sort_batch_returns_u32_codes_matching_morton_of() {
    let extent = Extent {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    };
    let mut items = vec![
        TilerItem {
            entity_id: EntityId::new(1),
            x: 0.75,
            y: 0.75,
            node_id: 0,
            priority: 10,
            scalars: Vec::new(),
        },
        TilerItem {
            entity_id: EntityId::new(2),
            x: 0.10,
            y: 0.10,
            node_id: 0,
            priority: 10,
            scalars: Vec::new(),
        },
    ];
    let codes: Vec<u32> = sort_batch(&mut items, &extent);
    assert_eq!(codes.len(), 2);
    assert!(codes[0] <= codes[1]);
    for (i, item) in items.iter().enumerate() {
        assert_eq!(
            codes[i],
            morton_of(item.x as f64, item.y as f64, &extent).raw(),
            "row {i}: returned code must equal morton_of() on the sorted item"
        );
    }
}
```

Add to `crates/tessera-store/tests/segment_roundtrip.rs`, as a new test function:

```rust
#[test]
fn morton_u32_file_is_four_bytes_per_row_and_sorted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let extent = tessera_spatial::Extent {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    };
    let mut items: Vec<TilerItem> = (0..1000u64)
        .map(|i| TilerItem {
            entity_id: EntityId::new(i),
            x: ((i * 7919) % 1000) as f32 / 1000.0,
            y: ((i * 104_729) % 1000) as f32 / 1000.0,
            node_id: 0,
            priority: (i % 65536) as u16,
            scalars: Vec::new(),
        })
        .collect();
    let codes = tessera_spatial::tiler::sort_batch(&mut items, &extent);
    tessera_store::write::write_segment(dir.path(), &items, &codes, &[]).expect("write_segment");

    assert!(
        !dir.path().join("morton.u64").exists(),
        "the u64 file must not be written any more"
    );
    let bytes = std::fs::read(dir.path().join("morton.u32")).expect("read morton.u32");
    assert_eq!(bytes.len(), items.len() * 4, "4 bytes per row, no header");

    let read_back: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(read_back, codes, "bytes must round-trip sort_batch's codes");
    assert!(
        read_back.windows(2).all(|w| w[0] <= w[1]),
        "codes must be non-decreasing"
    );
}
```

Add to `crates/tessera-store/tests/bundle_read.rs`, as a new test function (adapt the existing bundle-assembly helper this file already uses — do **not** duplicate it; call it):

```rust
#[test]
fn tile_ranges_covers_the_whole_segment_at_depth_zero() {
    // Depth 0's exclusive code-range end is 1 << 32, which does not fit in u32. This test
    // exists because narrowing `Tile::code_range` to u32 would overflow here and silently
    // return an empty range in release builds.
    let (dir, codes) = build_tiny_bundle();
    let bundle = tessera_store::read::open_bundle(dir.path()).expect("open_bundle");
    let seg = first_segment(&bundle);
    let whole = tessera_store::read::tile_ranges(
        seg,
        &tessera_spatial::Tile {
            prefix: 0,
            depth: 0,
        },
    );
    assert_eq!(
        whole,
        0u32..codes.len() as u32,
        "depth 0 must select every row in the segment"
    );
}
```

`build_tiny_bundle()` and `first_segment()` are the names to give the two helpers you extract from the existing test body in that file; if the file already names them differently, use its names.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd /home/joe/code/tessera
cargo test -p tessera-spatial sort_batch_returns_u32_codes_matching_morton_of 2>&1 | tail -20
cargo test -p tessera-store morton_u32_file_is_four_bytes_per_row_and_sorted 2>&1 | tail -20
cargo test -p tessera-store tile_ranges_covers_the_whole_segment_at_depth_zero 2>&1 | tail -20
```

Expected: the first two fail to **compile** (`sort_batch` returns `Vec<u64>`, `write_segment` expects `&[u64]`) — a compile failure is a valid red here. The third may pass already; that is fine, it is a regression guard for Step 4.

- [ ] **Step 3: Narrow `sort_batch`**

In `crates/tessera-spatial/src/tiler.rs`, change the doc comment on `sort_batch` from:

```rust
/// Returns the sorted items' Morton codes as low-aligned `u64`s (the 32-bit code widened,
/// matching `morton.u64`'s on-disk representation), in the same order as `items` post-sort.
```

to:

```rust
/// Returns the sorted items' Morton codes as `u32`s, matching `morton.u32`'s on-disk
/// representation, in the same order as `items` post-sort. The code is 32 bits because §5.2
/// fixes the grid at 2^16 x 2^16 — a property of the grid, not the population, so this width
/// does not change at 10^10 or 10^11 (contracts §2.5).
```

and the signature and body:

```rust
pub fn sort_batch(items: &mut [TilerItem], extent: &Extent) -> Vec<u32> {
    let mut codes: Vec<u32> = items
        .iter()
        .map(|item| morton_of(item.x as f64, item.y as f64, extent).raw())
        .collect();
```

Everything below (the `order` sort, the lockstep permutation apply) is unchanged; only `Vec<u64>` → `Vec<u32>` in the two local declarations (`codes`, `sorted_codes`) and the return type.

- [ ] **Step 4: Narrow the writer**

In `crates/tessera-store/src/write.rs`:

Module doc line 1–2: replace `morton.u64` with `morton.u32`.

`write_segment` doc and signature:

```rust
/// Write `columns.arrow` (Arrow IPC file format, one record batch, uncompressed buffers) and
/// `morton.u32` (raw little-endian `u32` codes, no header) into `dir`.
///
/// `items` and `codes` must already be in row order (i.e. the output of
/// [`tessera_spatial::tiler::sort_batch`]) and the same length; row *i*'s Morton code is
/// `codes[i]`. `scalar_schema` declares the name and Arrow type of each item's trailing
/// `scalars`, in the order they appear in `TilerItem::scalars`.
pub fn write_segment(
    dir: &Path,
    items: &[TilerItem],
    codes: &[u32],
    scalar_schema: &[(String, ScalarType)],
) -> io::Result<()> {
```

Line 52 becomes:

```rust
    write_morton_u32(&dir.join("morton.u32"), codes)?;
```

and the writer itself:

```rust
fn write_morton_u32(path: &Path, codes: &[u32]) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    for code in codes {
        writer.write_all(&code.to_le_bytes())?;
    }
    writer.flush()
}
```

Leave the `items.len() != codes.len()` check and the non-decreasing `debug_assert!` exactly as they are.

- [ ] **Step 5: Narrow the reader, and widen at the comparison**

In `crates/tessera-store/src/read.rs`:

Module doc line 1: `morton.u64` → `morton.u32`.

Lines ~150–186, the segment load block: replace the three `morton.u64` occurrences (the `seg_dir.join(...)`, the `format!("partitions/{}/slices/{}/segments/{}/morton.u64", …)` relative path, and the error message text) with `morton.u32`.

`MortonSlice` (from line ~431):

```rust
/// A memory-mapped, zero-copy view of `morton.u32`: raw sorted little-endian `u32` codes, no
/// header (contracts §2.5/§2.6).
pub struct MortonSlice {
    mmap: Mmap,
}

impl MortonSlice {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: read-only for this struct's lifetime; see `Permutation::load`'s note on the
        // shared operational hazard of a concurrently-truncated backing file.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if mmap.len() % 4 != 0 {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "{}: length {} is not a multiple of 4",
                    path.display(),
                    mmap.len()
                ),
            });
        }
        let slice = MortonSlice { mmap };
        // `tile_ranges`'s binary search is only sound over an ascending array (contracts
        // §2.5/§2.6: "Morton order"); a hand-corrupted or wrongly-built `morton.u32` that isn't
        // sorted would make `partition_point` silently return a wrong (not merely imprecise)
        // range instead of erroring — checked once here, fail-closed, rather than trusted.
        if !slice.u32().windows(2).all(|w| w[0] <= w[1]) {
            return Err(StoreError::MalformedBundle {
                detail: format!("{}: codes are not sorted ascending", path.display()),
            });
        }
        Ok(slice)
    }

    /// The number of codes (rows) in this segment.
    pub fn len(&self) -> usize {
        self.mmap.len() / 4
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// The codes, in row order (ascending, ties broken by priority then entity ID at write
    /// time — contracts §2.6).
    pub fn u32(&self) -> &[u32] {
        // SAFETY: length is a checked multiple of 4 (validated at `load`); the mmap base is
        // page-aligned (>= 4-byte aligned) by construction, so this cast is always valid — no
        // per-open re-check needed the way `permutation.bin`'s offset-16 slice needed one,
        // since here the slice starts at offset 0.
        unsafe { std::slice::from_raw_parts(self.mmap.as_ptr() as *const u32, self.len()) }
    }
}
```

`tile_ranges` (from line ~859) — **`code_range()` stays `(u64, u64)`; the stored code is widened per comparison**:

```rust
/// The row range `tile` occupies within `seg`'s Morton order, found by binary search over
/// `seg.morton.u32()` (contracts §2.5). Callers must treat a tile as resolving to a **set** of
/// ranges — one per segment sharing the tile's slice — even though Phase 1 has exactly one
/// segment per slice; the engine-level signature is `Vec<Range<u32>>` (task brief).
///
/// `Tile::code_range` returns `u64` bounds deliberately: at depth 0 the exclusive end is
/// `1 << 32`, which does not fit in `u32`. Each stored code is widened for the comparison
/// rather than the bounds being narrowed, which would overflow to an empty range there.
pub fn tile_ranges(seg: &SegmentData, tile: &Tile) -> Range<u32> {
    let codes = seg.morton.u32();
    let (lo, hi) = tile.code_range();
    let start = codes.partition_point(|&c| (c as u64) < lo);
    let end = codes.partition_point(|&c| (c as u64) < hi);
    start as u32..end as u32
}
```

Also update the doc comment at `crates/tessera-store/src/lib.rs:1` and the error variant doc at `crates/tessera-store/src/error.rs:37` — `morton.u64` → `morton.u32` in both.

- [ ] **Step 6: Update the build pipeline's two path literals**

In `crates/tessera-build/src/lib.rs`, change both occurrences:

```rust
    fsync_file(&segment_dir.join("morton.u32"))?;
```

and inside the `for path in [...]` manifest-digest loop:

```rust
        &segment_dir.join("morton.u32"),
```

Leave `crates/tessera-build/src/input.rs` alone — its `Geometry::Morton` reader consumes a `morton` *input column* from the caller's Parquet (already `u32::try_from`-checked at line 149), which is a different thing from the bundle's stored column.

- [ ] **Step 7: Update the existing tests to the new name and width**

- `crates/tessera-store/tests/segment_roundtrip.rs:2` (module doc), `:64-65` — `morton.u64` → `morton.u32`, and the byte read-back must decode 4-byte chunks:
  ```rust
  let morton_bytes = fs::read(dir.path().join("morton.u32")).expect("read morton.u32");
  let decoded: Vec<u32> = morton_bytes
      .chunks_exact(4)
      .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
      .collect();
  assert_eq!(decoded, codes);
  assert!(decoded.windows(2).all(|w| w[0] <= w[1]));
  ```
- `crates/tessera-store/tests/bundle_read.rs:108-109` — the manifest file-map key becomes `"partitions/default/slices/main/segments/seg0/morton.u32"`, and the `file_digest(&seg_dir.join("morton.u32"))` path with it. Line 206-207: `assert_eq!(seg.morton.u32(), codes.as_slice());`
- `crates/tessera-build/tests/build_smoke.rs:238` — expected manifest key becomes `"partitions/default/slices/s0/segments/seg-0/morton.u32"`. Line 544 — the `std::fs::read(out.join(".../morton.u32"))` path, and whatever assertion follows it must decode `<u4` not `<u8`.

- [ ] **Step 8: Run the whole Rust suite**

```bash
cd /home/joe/code/tessera
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace 2>&1 | tail -40
bash scripts/check-layers.sh
```
Expected: clippy clean, every test PASS, layer check clean. If any test outside the files listed in Step 7 fails, **stop and report** — it means a consumer of `MortonSlice::u64()` exists that this plan did not enumerate.

- [ ] **Step 9: Update the Python reference oracle**

In `reference/oracle/bundle.py`:
- Line 4 (module doc): `morton.u64` → `morton.u32`.
- Line 40 (dataclass field comment): `morton: np.ndarray  # uint32, row order (raw sorted codes from morton.u32)`
- Lines 210–226:
  ```python
      morton_path = seg_dir / "morton.u32"
      ...
      morton_bytes = morton_path.read_bytes()
      morton = np.frombuffer(morton_bytes, dtype="<u4")
      ...
      if len(morton) != row_count:
          raise ValueError(
              f"{seg_dir}: columns.arrow has {row_count} rows but morton.u32 has {len(morton)}"
          )
  ```

In `reference/tests/test_differential.py`: line 5 and the docstring at line 77 — `morton.u64` → `morton.u32`. The comparison at line 81 (`int(seg.morton[i])`) already works unchanged, because `int()` of a `uint32` and of a `uint64` are equal for a 32-bit code — that is the property this test protects.

- [ ] **Step 10: Run the differential suite**

```bash
cd /home/joe/code/tessera
bash scripts/setup-reference-venv.sh   # if the venv is not already present
# then, per reference/README or pyproject, e.g.:
.venv-reference/bin/python -m pytest reference/tests/test_differential.py -x -q 2>&1 | tail -30
```
Expected: PASS, including `test_morton_matches_byte_for_byte`. That test is the real gate here — it recomputes every code from `columns.arrow`'s x/y in Python and compares against the stored bytes, so it proves the narrowing did not change a single code value. If the venv setup or invocation differs, read `scripts/setup-reference-venv.sh` and `reference/pyproject.toml` and follow what they actually say rather than the sketch above.

- [ ] **Step 11: Amend the contracts spec**

In `docs/design/contracts.md`:

§2.1's file tree (around line 63): `morton.u64` → `morton.u32`.

§2.5's final sentence currently reads:

> The 32-bit code is stored low-aligned in a `u64`; high bits zero. A tile at depth *d* (0 ≤ d ≤ 16) is identified on the wire by its prefix value `code >> (32 − 2d)`; its row range in a segment is found by binary search over that segment's `morton.u64`.

Replace with:

```markdown
The 32-bit code is stored as a `u32` *(r5; was low-aligned in a `u64` with high bits zero — 4 GB of zeroes at 10⁹)*. The width is a property of the **grid**, not the population: §5.2 fixes the grid at 2¹⁶ × 2¹⁶, so it does not change at 10¹⁰ or 10¹¹, and it constrains only future grid depth beyond 16, which nothing currently wants. A tile at depth *d* (0 ≤ d ≤ 16) is identified on the wire by its prefix value `code >> (32 − 2d)`; its row range in a segment is found by binary search over that segment's `morton.u32`. **A tile's code range is computed in `u64`** — at depth 0 the exclusive end is 2³² — and each stored code is widened for the comparison; narrowing the bounds instead overflows to an empty range.
```

§2.6's line:

```markdown
`morton.u32` *(r5; was `morton.u64`)*: raw sorted `u32` codes, no header; length = `row_count × 4`.
```

§2.6's heading: `### 2.6 `columns.arrow`, `morton.u32`, `permutation.bin``.

Add to §0.3's deviation list a new numbered entry, following the existing style:

```markdown
N. **`morton.u32`, not `morton.u64`** *(r5)*. The stored Morton column is a raw `u32`, and the file is renamed accordingly. `bundle_format` stays at **1**: format 1 has never been published — Phase 1 is its only writer — and the *rename* is what makes any pre-existing bundle fail closed, since `open_bundle` verifies each named file against the manifest's file map and a missing `morton.u32` is a typed error, never a half-width read. Saves 4 GB at 10⁹ at zero decode cost. Source: the drawn-mark budget spec §2.
```

Replace `N` with the next unused number in that list.

Finally, in the spec's revision block (near line 262), add an **r5** paragraph above **r4** recording the narrowing and the deviation, in the same voice as r4's.

- [ ] **Step 12: Full verification**

```bash
cd /home/joe/code/tessera
grep -rn "morton\.u64" --include=*.rs --include=*.py crates/ reference/ conformance/
cargo test --workspace 2>&1 | tail -10
bash scripts/check-layers.sh
```
Expected: the grep returns **nothing** under `crates/`, `reference/` and `conformance/`. Hits inside `docs/archive/plans/2026-07-28-phase1-walking-skeleton.md` and `docs/design/architecture.md:411` are historical prose — leave the executed Phase 1 plan alone; the design's §10.3 mention is a passing reference the contracts spec now governs, so add `*(now `morton.u32` — contracts §2.5, r5)*` after it and nothing more.

- [ ] **Step 13: Commit**

```bash
cd /home/joe/code/tessera
git add docs/design/contracts.md docs/design/architecture.md \
        crates/tessera-spatial/src/tiler.rs \
        crates/tessera-store/src/write.rs crates/tessera-store/src/read.rs \
        crates/tessera-store/src/lib.rs crates/tessera-store/src/error.rs \
        crates/tessera-build/src/lib.rs \
        crates/tessera-store/tests/segment_roundtrip.rs \
        crates/tessera-store/tests/bundle_read.rs \
        crates/tessera-build/tests/build_smoke.rs \
        reference/oracle/bundle.py reference/tests/test_differential.py
git commit -m "feat(store,build,spatial): narrow the stored Morton column to u32 (contracts r5)"
```

---

### Task 3: The probe harness — collector, browser bundle, and the Chrome launcher

P1 and P2 both need: a page served over HTTP to Windows Chrome, a sweep driver that runs unattended, a way to get results back into the repo, and a VRAM reading from the real GPU. Build that once, prove it end to end with a trivial smoke probe, then Tasks 4 and 5 only add probe logic.

**Why a separate Cargo project.** `probes/markbudget/` is measurement tooling. Excluding it from the workspace keeps `cargo build --workspace`, `cargo clippy --workspace` and `scripts/check-layers.sh` describing only the shipped system, while `path` dependencies still let P2 call the *real* `tessera_wire::payload::viewport_ipc`.

**Files:**
- Modify: `Cargo.toml` (root)
- Create: `probes/markbudget/Cargo.toml`, `probes/markbudget/.gitignore`, `probes/markbudget/README.md`
- Create: `probes/markbudget/src/lib.rs`, `probes/markbudget/src/gpu.rs`, `probes/markbudget/src/bin/collector.rs`
- Create: `probes/markbudget/web/package.json`, `probes/markbudget/web/build.mjs`, `probes/markbudget/web/src/harness.js`, `probes/markbudget/web/src/smoke.js`, `probes/markbudget/web/smoke.html`
- Create: `probes/markbudget/run.sh`

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces, for Tasks 4–6:
  - Collector HTTP surface: `GET /<name>.html` and `GET /bundle/<name>.js` (static); `POST /api/gpu` → `{"used_mib": u64, "total_mib": u64}` sampled live; `POST /api/result` with body `{"probe": String, "run": Value}` → writes `probes/markbudget/results/<probe>-<utc-timestamp>.json` and returns `{"path": String}`; `POST /api/done` → shuts the collector down with exit code 0.
  - JS module `web/src/harness.js` exporting `sweep({probe, ns, arms, run, onProgress})` and `postResult(probe, run)`.
  - `probes/markbudget/run.sh <probe-name>` — builds the bundle, starts the collector, launches Windows Chrome at the page, waits for `/api/done`, prints the results path.
  - Rust `markbudget::gpu::sample() -> anyhow::Result<GpuSample>`.

- [ ] **Step 1: Exclude the probe project from the workspace**

In the root `Cargo.toml`, immediately after the `members = [...]` array, add:

```toml
# Measurement tooling, deliberately outside the shipped workspace: it must not appear in
# `cargo build --workspace`, must not be scanned by scripts/check-layers.sh, and must not
# change any shipped crate's dependency graph. It path-depends *into* the workspace so P2
# measures the real `tessera_wire::payload::viewport_ipc` bytes rather than a lookalike.
exclude = ["probes/markbudget"]
```

Verify: `cargo metadata --format-version 1 --no-deps | python3 -c "import json,sys; print([p['name'] for p in json.load(sys.stdin)['packages']])"` must not list `markbudget`.

- [ ] **Step 2: Create the probe project skeleton**

`probes/markbudget/Cargo.toml`:

```toml
[package]
name = "markbudget"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
tessera-wire = { path = "../../crates/tessera-wire" }
axum = "0.8"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
tower-http = { version = "0.6", features = ["fs"] }

[[bin]]
name = "collector"
path = "src/bin/collector.rs"
```

`probes/markbudget/.gitignore`:

```
web/node_modules/
web/dist/
```

Results JSON is **not** ignored — the measurements are the deliverable and belong in git.

- [ ] **Step 3: Write the GPU sampler with its failing test**

`probes/markbudget/src/gpu.rs`:

```rust
//! VRAM sampling through the Windows-host `nvidia-smi.exe`.
//!
//! WSL2 can execute Windows binaries directly, so the probe reads the real RTX 3080's memory
//! rather than the WSLg passthrough view — which is the number that matters, since the browser
//! under measurement runs on the Windows host.

use std::process::Command;

use serde::Serialize;

const NVIDIA_SMI: &str = "/mnt/c/Windows/System32/nvidia-smi.exe";

#[derive(Debug, Clone, Copy, Serialize)]
pub struct GpuSample {
    pub used_mib: u64,
    pub total_mib: u64,
}

/// Sample the GPU's memory right now.
///
/// Returns an error rather than a zeroed sample if `nvidia-smi.exe` is missing or unparseable —
/// a silently-zero VRAM column would be worse than a loud failure in a measurement artefact.
pub fn sample() -> anyhow::Result<GpuSample> {
    let out = Command::new(NVIDIA_SMI)
        .args([
            "--query-gpu=memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "nvidia-smi.exe exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the first GPU row of `--format=csv,noheader,nounits` output: `used, total` in MiB.
pub fn parse(stdout: &str) -> anyhow::Result<GpuSample> {
    let line = stdout
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("nvidia-smi produced no rows"))?;
    let mut parts = line.split(',').map(str::trim);
    let used_mib = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("no memory.used field in {line:?}"))?
        .parse()?;
    let total_mib = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("no memory.total field in {line:?}"))?
        .parse()?;
    Ok(GpuSample {
        used_mib,
        total_mib,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv_noheader_nounits() {
        let s = parse("1783, 10240\n").expect("parse");
        assert_eq!(s.used_mib, 1783);
        assert_eq!(s.total_mib, 10240);
    }

    #[test]
    fn empty_output_is_an_error_not_a_zero_sample() {
        assert!(parse("").is_err());
    }

    #[test]
    fn samples_the_real_gpu() {
        let s = sample().expect("nvidia-smi.exe must be reachable from WSL2");
        assert_eq!(s.total_mib, 10240, "expected the RTX 3080's 10 GiB");
        assert!(s.used_mib <= s.total_mib);
    }
}
```

- [ ] **Step 4: Run the GPU tests to verify they fail, then pass**

```bash
cd /home/joe/code/tessera/probes/markbudget
cargo test 2>&1 | tail -20
```
Expected on first run before `src/lib.rs` declares the module: compile error. After Step 5 declares it: 3 PASS. If `samples_the_real_gpu` fails, the hardware assumption in Global Constraints is wrong — **stop and report** rather than weakening the assertion.

- [ ] **Step 5: Write the collector**

`probes/markbudget/src/lib.rs`:

```rust
//! Shared plumbing for the drawn-mark budget probes (spec §4).
//!
//! Nothing here is part of the shipped system: this package is excluded from the root Cargo
//! workspace on purpose. It path-depends into the workspace only so that P2 serves the real
//! `tessera_wire::payload::viewport_ipc` bytes.

pub mod gpu;
```

`probes/markbudget/src/bin/collector.rs`:

```rust
//! Serves the probe pages to Windows Chrome, samples the GPU on request, and writes each run's
//! results into `probes/markbudget/results/`.
//!
//! The browser drives; this process only serves, samples and records. That split is what makes
//! the probes reproducible: the whole sweep is in the page, and rerunning is one command.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get_service, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;
use tower_http::services::ServeDir;

struct AppState {
    results_dir: PathBuf,
    shutdown: Notify,
}

#[derive(Deserialize)]
struct ResultBody {
    probe: String,
    run: Value,
}

#[derive(Serialize)]
struct ResultAck {
    path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let results_dir = root.join("results");
    std::fs::create_dir_all(&results_dir)?;

    let state = Arc::new(AppState {
        results_dir,
        shutdown: Notify::new(),
    });

    let web = root.join("web");
    let app = Router::new()
        .route("/api/gpu", post(gpu_sample))
        .route("/api/result", post(record_result))
        .route("/api/done", post(done))
        .fallback_service(get_service(ServeDir::new(web)))
        .with_state(state.clone());

    // 0.0.0.0 so Windows Chrome can reach it across the WSL2 vNIC; the port is fixed so
    // run.sh can construct the URL without parsing output.
    let addr: SocketAddr = "0.0.0.0:8731".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("collector listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { state.shutdown.notified().await })
        .await?;
    Ok(())
}

async fn gpu_sample() -> impl IntoResponse {
    match markbudget::gpu::sample() {
        Ok(s) => (StatusCode::OK, Json(serde_json::to_value(s).unwrap())),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn record_result(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ResultBody>,
) -> impl IntoResponse {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let name = format!("{}-{}.json", body.probe, stamp);
    let path = state.results_dir.join(&name);
    let pretty = serde_json::to_string_pretty(&body.run).unwrap_or_else(|_| "{}".into());
    match std::fs::write(&path, pretty) {
        Ok(()) => {
            eprintln!("wrote {}", path.display());
            (
                StatusCode::OK,
                Json(serde_json::to_value(ResultAck {
                    path: path.display().to_string(),
                })
                .unwrap()),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn done(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.shutdown.notify_one();
    StatusCode::OK
}
```

- [ ] **Step 6: Write the browser sweep driver**

`probes/markbudget/web/package.json`:

```json
{
  "name": "markbudget-web",
  "private": true,
  "type": "module",
  "scripts": {
    "build": "node build.mjs"
  },
  "dependencies": {
    "apache-arrow": "^17.0.0",
    "deck.gl": "^9.0.0"
  },
  "devDependencies": {
    "esbuild": "^0.23.0"
  }
}
```

`probes/markbudget/web/build.mjs`:

```js
// Bundles each probe page's entry point into web/dist/<name>.js as a self-contained IIFE.
// No CDN at run time: the page must be reproducible from a checkout plus `npm ci`.
import { build } from 'esbuild';

const ENTRIES = ['smoke'];   // Task 4 adds 'p1'; Task 5 adds 'p2'

await build({
  entryPoints: ENTRIES.map((n) => `src/${n}.js`),
  outdir: 'dist',
  bundle: true,
  format: 'iife',
  minify: true,
  sourcemap: false,
  target: ['chrome120'],
  logLevel: 'info',
});
```

`probes/markbudget/web/src/harness.js`:

```js
// The sweep driver every probe page shares.
//
// A probe supplies `run(n, arm)` returning a plain-object measurement; the harness handles the
// N sweep, the arm loop, GPU sampling around each point, progress reporting and the results
// POST. Nothing here knows what is being measured.

const API = '';   // same origin as the page

export async function gpu() {
  try {
    const r = await fetch(`${API}/api/gpu`, { method: 'POST' });
    if (!r.ok) return null;
    return await r.json();
  } catch {
    return null;
  }
}

export async function postResult(probe, run) {
  const r = await fetch(`${API}/api/result`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ probe, run }),
  });
  if (!r.ok) throw new Error(`POST /api/result failed: ${r.status}`);
  return await r.json();
}

export async function done() {
  await fetch(`${API}/api/done`, { method: 'POST' }).catch(() => {});
}

// Yield to the browser so a long sweep does not starve compositing or the fetch queue.
export const idle = (ms = 250) => new Promise((res) => setTimeout(res, ms));

/**
 * Run `probe.run(n, arm)` for every (arm, n) pair and collect the results.
 *
 * @param {{probe: string, ns: number[], arms: string[],
 *          run: (n: number, arm: string) => Promise<object>,
 *          onProgress?: (msg: string) => void}} cfg
 * @returns {Promise<object>} the run record, already POSTed
 */
export async function sweep(cfg) {
  const { probe, ns, arms, run, onProgress = () => {} } = cfg;
  const points = [];
  const started = new Date().toISOString();

  for (const arm of arms) {
    for (const n of ns) {
      onProgress(`${arm} @ N=${n} …`);
      const gpuBefore = await gpu();
      let measurement;
      let error = null;
      try {
        measurement = await run(n, arm);
      } catch (e) {
        error = String(e && e.stack ? e.stack : e);
        measurement = null;
      }
      const gpuAfter = await gpu();
      points.push({ arm, n, measurement, error, gpuBefore, gpuAfter });
      onProgress(`${arm} @ N=${n} ${error ? 'ERROR' : 'ok'}`);
      await idle();
    }
  }

  const record = {
    probe,
    started,
    finished: new Date().toISOString(),
    userAgent: navigator.userAgent,
    hardwareConcurrency: navigator.hardwareConcurrency,
    devicePixelRatio: window.devicePixelRatio,
    viewport: { w: window.innerWidth, h: window.innerHeight },
    arms,
    ns,
    points,
  };
  const ack = await postResult(probe, record);
  onProgress(`wrote ${ack.path}`);
  return record;
}
```

`probes/markbudget/web/src/smoke.js`:

```js
// Proves the whole loop end to end — page served, sweep driven, GPU sampled, results written,
// collector shut down — with a probe that measures nothing interesting on purpose.
import { sweep, done } from './harness.js';

const log = (msg) => {
  const el = document.getElementById('log');
  el.textContent += `${msg}\n`;
  el.scrollTop = el.scrollHeight;
};

(async () => {
  await sweep({
    probe: 'smoke',
    ns: [1000, 10000],
    arms: ['baseline'],
    onProgress: log,
    async run(n) {
      const t0 = performance.now();
      const a = new Float32Array(n);
      for (let i = 0; i < n; i++) a[i] = Math.sqrt(i);
      return { n, sum: a[n - 1], ms: performance.now() - t0 };
    },
  });
  log('done');
  await done();
})();
```

`probes/markbudget/web/smoke.html`:

```html
<!doctype html>
<meta charset="utf-8">
<title>markbudget — smoke</title>
<style>
  body { font: 13px/1.5 ui-monospace, monospace; margin: 0; padding: 1rem; }
  #log { white-space: pre; height: 80vh; overflow: auto; }
</style>
<h1>markbudget — smoke</h1>
<div id="log"></div>
<script src="/dist/smoke.js"></script>
```

- [ ] **Step 7: Write the runner**

`probes/markbudget/run.sh`:

```bash
#!/usr/bin/env bash
# Run one drawn-mark-budget probe end to end.
#
#   ./run.sh smoke      # or p1, p2
#
# Builds the browser bundle, starts the collector, launches Chrome on the Windows host against
# the page, and waits for the page to POST /api/done. Results land in results/.
set -euo pipefail

PROBE="${1:?usage: run.sh <smoke|p1|p2>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=8731
CHROME="/mnt/c/Program Files/Google/Chrome/Application/chrome.exe"

cd "$HERE/web"
[ -d node_modules ] || npm ci
npm run build

cd "$HERE"
cargo build --release --bin collector
./target/release/collector &
COLLECTOR_PID=$!
trap 'kill "$COLLECTOR_PID" 2>/dev/null || true' EXIT

# Wait for the listener rather than sleeping blind.
for _ in $(seq 1 50); do
  if curl -sS -o /dev/null "http://127.0.0.1:$PORT/$PROBE.html"; then break; fi
  sleep 0.2
done

# Windows Chrome reaches WSL2 over localhost forwarding; fall back to the vNIC address.
URL="http://localhost:$PORT/$PROBE.html"
PROFILE="$(mktemp -d)"
"$CHROME" \
  --user-data-dir="$(wslpath -w "$PROFILE")" \
  --no-first-run --no-default-browser-check \
  --new-window "$URL" &

echo "waiting for $PROBE to finish (collector exits on /api/done)…"
wait "$COLLECTOR_PID" || true
trap - EXIT

echo
echo "results:"
ls -t "$HERE/results" | head -5
```

Make it executable: `chmod +x probes/markbudget/run.sh`.

- [ ] **Step 8: Run the smoke probe end to end**

```bash
cd /home/joe/code/tessera/probes/markbudget
./run.sh smoke
```
Expected: npm installs, esbuild reports one output, the collector prints `collector listening`, a Chrome window opens and shows two log lines then `done`, the collector prints `wrote …/results/smoke-<stamp>.json` and exits, and the script lists the file. Then:

```bash
cat results/smoke-*.json | head -40
```
Expected: a JSON record with `probe: "smoke"`, two points, and **non-null `gpuBefore`/`gpuAfter` containing `used_mib`/`total_mib`**. A null GPU sample means `nvidia-smi.exe` is not reachable from the collector's process — fix that before Task 4, because P1's VRAM column depends on it.

If Chrome cannot reach `localhost`, replace `URL` with `http://$(hostname -I | awk '{print $1}'):$PORT/$PROBE.html` and note the change in the README; Windows Firewall may also need to allow inbound on 8731.

- [ ] **Step 9: Write the README**

`probes/markbudget/README.md` — cover: what this measures and why (one paragraph, citing the spec), the four probes and which of them live here (P1, P2, P3; P4 is post-Phase-1 and needs real 10⁹ columns), how to run (`./run.sh <probe>` for browser probes, `cargo run --release --bin p3_handles` for P3), where results land and that they are committed deliberately, the hardware the numbers are against (RTX 3080 10 GB, 47 GB RAM, 12 cores, WSL2 + Windows Chrome), and the standing caveat that this package is **excluded from the root workspace** and is not part of the shipped system.

- [ ] **Step 10: Commit**

```bash
cd /home/joe/code/tessera
git add Cargo.toml probes/markbudget/Cargo.toml probes/markbudget/.gitignore \
        probes/markbudget/README.md probes/markbudget/run.sh \
        probes/markbudget/src probes/markbudget/web/package.json \
        probes/markbudget/web/package-lock.json probes/markbudget/web/build.mjs \
        probes/markbudget/web/src probes/markbudget/web/smoke.html \
        probes/markbudget/results
git commit -m "test(probes): drawn-mark-budget harness — collector, sweep driver, Chrome runner"
```

---

### Task 4: P1 — the GPU render ceiling

Spec §4/P1: deck.gl `ScatterplotLayer` over synthetic points, sweeping N from 10⁵ to 3×10⁷, four arms — with and without GPU picking, with and without collision-filtered labels. Binary attributes throughout; the JS-object path is not the configuration this system would ship.

**Report:** frame time at each N per arm; the N at which frame time crosses 16 ms and 33 ms; VRAM at each N; whether picking or labels dominates the degradation.
**Decides:** the upper bound on any budget, and whether picking must become optional above some N.

**Files:**
- Create: `probes/markbudget/web/src/p1.js`, `probes/markbudget/web/p1.html`, `probes/markbudget/P1-render.md`
- Modify: `probes/markbudget/web/build.mjs`

**Interfaces:**
- Consumes: `harness.js`'s `sweep`, `done`; the collector's `/api/gpu` and `/api/result`.
- Produces: `probes/markbudget/results/p1-<stamp>.json` whose per-point `measurement` is `{n, arm, frames, p50Ms, p95Ms, meanMs, attributeBytes, drawCalls}`; and `P1-render.md`, the written report.

- [ ] **Step 1: Add `p1` to the bundle entry points**

In `probes/markbudget/web/build.mjs`:

```js
const ENTRIES = ['smoke', 'p1'];   // Task 5 adds 'p2'
```

- [ ] **Step 2: Write the probe**

`probes/markbudget/web/src/p1.js`:

```js
// P1 — the GPU render ceiling (drawn-mark budget spec §4).
//
// Sweeps N from 1e5 to 3e7 over four arms. Binary attributes throughout: the JS-object data
// path is not the configuration this system would ship, and measuring it would understate the
// ceiling by measuring JS, not the GPU.
//
// Frame time is measured while *panning*, not on a static scene: a still deck.gl view stops
// redrawing, and a probe that measured a stationary camera would report the compositor's idle
// rate rather than the render cost.

import { Deck, OrthographicView } from '@deck.gl/core';
import { ScatterplotLayer, TextLayer } from '@deck.gl/layers';
import { CollisionFilterExtension } from '@deck.gl/extensions';
import { sweep, done, idle } from './harness.js';

const NS = [1e5, 3e5, 1e6, 3e6, 1e7, 2e7, 3e7].map(Number);

const ARMS = [
  'plain',            // scatterplot only
  'picking',          // + GPU picking invoked once per frame
  'labels',           // + collision-filtered TextLayer
  'picking+labels',   // both
];

const FRAMES_MEASURED = 120;
const FRAMES_WARMUP = 30;
const LABEL_COUNT = 2000;   // labels are the caller's cost (§13.2); a realistic on-screen count

const log = (msg) => {
  const el = document.getElementById('log');
  el.textContent += `${msg}\n`;
  el.scrollTop = el.scrollHeight;
};

// Deterministic positions so every arm at a given N draws exactly the same scene.
function makePositions(n) {
  const xs = new Float32Array(n * 2);
  let s = 0x9e3779b9;
  for (let i = 0; i < n; i++) {
    s = (Math.imul(s ^ (s >>> 16), 0x21f0aaad) >>> 0);
    s = (Math.imul(s ^ (s >>> 15), 0x735a2d97) >>> 0);
    xs[i * 2] = ((s >>> 0) / 4294967296) * 1000 - 500;
    s = (Math.imul(s ^ (s >>> 16), 0x21f0aaad) >>> 0);
    xs[i * 2 + 1] = ((s >>> 0) / 4294967296) * 1000 - 500;
  }
  return xs;
}

function makeColors(n) {
  const c = new Uint8Array(n * 4);
  for (let i = 0; i < n; i++) {
    c[i * 4] = 80;
    c[i * 4 + 1] = 140;
    c[i * 4 + 2] = 220;
    c[i * 4 + 3] = 200;
  }
  return c;
}

function makeLabels(positions, count) {
  const out = [];
  const stride = Math.max(1, Math.floor(positions.length / 2 / count));
  for (let i = 0; i < count; i++) {
    const j = i * stride;
    out.push({ position: [positions[j * 2], positions[j * 2 + 1]], text: `n${i}` });
  }
  return out;
}

function layersFor(arm, n, positions, colors, labels) {
  const wantsPicking = arm.includes('picking');
  const layers = [
    new ScatterplotLayer({
      id: 'points',
      data: {
        length: n,
        attributes: {
          getPosition: { value: positions, size: 2 },
          getFillColor: { value: colors, size: 4, normalized: true },
        },
      },
      radiusUnits: 'pixels',
      getRadius: 2,
      pickable: wantsPicking,
    }),
  ];
  if (arm.includes('labels')) {
    layers.push(
      new TextLayer({
        id: 'labels',
        data: labels,
        getPosition: (d) => d.position,
        getText: (d) => d.text,
        getSize: 12,
        sizeUnits: 'pixels',
        collisionEnabled: true,
        extensions: [new CollisionFilterExtension()],
      }),
    );
  }
  return layers;
}

async function runArm(n, arm) {
  const positions = makePositions(n);
  const colors = makeColors(n);
  const labels = arm.includes('labels') ? makeLabels(positions, LABEL_COUNT) : [];
  const wantsPicking = arm.includes('picking');

  const canvas = document.getElementById('gl');
  const deck = new Deck({
    canvas,
    views: new OrthographicView({}),
    initialViewState: { target: [0, 0, 0], zoom: 0 },
    controller: false,
    layers: layersFor(arm, n, positions, colors, labels),
  });

  // Warm up: first frames include shader compilation and buffer upload, which are real costs
  // but not per-frame ones — reporting them inside the frame-time p50 would be wrong.
  const times = [];
  let frame = 0;
  await new Promise((resolve) => {
    let last = performance.now();
    const tick = () => {
      const now = performance.now();
      if (frame >= FRAMES_WARMUP) times.push(now - last);
      last = now;
      frame += 1;

      // Pan a little every frame so deck.gl actually redraws.
      const t = frame / 60;
      deck.setProps({
        viewState: { target: [Math.sin(t) * 50, Math.cos(t) * 50, 0], zoom: 0 },
      });
      if (wantsPicking) {
        // Invoking picking is what costs; a `pickable: true` layer nobody picks is free.
        deck.pickObject({ x: canvas.width / 2, y: canvas.height / 2, radius: 1 });
      }

      if (times.length >= FRAMES_MEASURED) {
        resolve();
        return;
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });

  deck.finalize();
  await idle(400);   // let the GPU driver release before the post-point VRAM sample

  const sorted = [...times].sort((a, b) => a - b);
  const q = (p) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))];
  return {
    n,
    arm,
    frames: times.length,
    p50Ms: q(0.5),
    p95Ms: q(0.95),
    meanMs: times.reduce((a, b) => a + b, 0) / times.length,
    attributeBytes: positions.byteLength + colors.byteLength,
    labelCount: labels.length,
  };
}

(async () => {
  await sweep({
    probe: 'p1',
    ns: NS,
    arms: ARMS,
    onProgress: log,
    run: runArm,
  });
  log('done');
  await done();
})();
```

`probes/markbudget/web/p1.html`:

```html
<!doctype html>
<meta charset="utf-8">
<title>markbudget — P1 GPU render ceiling</title>
<style>
  body { font: 13px/1.5 ui-monospace, monospace; margin: 0; display: grid; grid-template-columns: 1fr 420px; height: 100vh; }
  #gl { width: 100%; height: 100%; display: block; }
  #log { white-space: pre; overflow: auto; padding: 1rem; border-left: 1px solid #ccc; }
</style>
<canvas id="gl" width="1600" height="1200"></canvas>
<div id="log"></div>
<script src="/dist/p1.js"></script>
```

- [ ] **Step 3: Verify the bundle builds and the smallest N renders**

```bash
cd /home/joe/code/tessera/probes/markbudget/web
npm install --save @deck.gl/core @deck.gl/layers @deck.gl/extensions
npm run build
```
Expected: esbuild reports `dist/p1.js`. If `deck.gl`'s scoped sub-packages resolve differently in the installed version, import from the `deck.gl` umbrella instead (`import {Deck, OrthographicView, ScatterplotLayer, TextLayer, CollisionFilterExtension} from 'deck.gl'`) — check `node_modules/deck.gl/package.json`'s exports before choosing.

Then a smoke run at reduced scale: temporarily set `const NS = [1e5]` and `const ARMS = ['plain']`, run `./run.sh p1`, confirm points appear on screen and a `p1-*.json` lands with a plausible `p50Ms` (single-digit ms at 10⁵). **Restore the full `NS` and `ARMS` before the real run** and delete the reduced-scale result file.

- [ ] **Step 4: Run the full sweep**

```bash
cd /home/joe/code/tessera/probes/markbudget
./run.sh p1
```
Expected: 28 points (4 arms × 7 N). Wall clock is roughly `28 × (120 frames + overhead)` — budget 10–20 minutes; at 3×10⁷ a frame may take hundreds of ms, so the tail arms are the slow ones. If Chrome's tab crashes at the top N (out-of-memory on the GPU process), that **is** a result: record the N at which it died in the report and re-run the sweep with that N removed rather than silently dropping the point.

- [ ] **Step 5: Write the report**

`probes/markbudget/P1-render.md`. Required contents, per the spec's "Report" clause:

1. Method and hardware in one short section (browser version from the record's `userAgent`, canvas size, `devicePixelRatio`, frames measured, the panning-camera rationale, the warm-up exclusion).
2. A table: arm × N → p50 ms, p95 ms, VRAM used (MiB, from `gpuAfter.used_mib` minus the run's idle baseline), attribute bytes.
3. **The two crossings**: the N at which p50 frame time crosses 16 ms and 33 ms, per arm, interpolated between the bracketing measurements and stated as an interval, not a false-precision point.
4. **Which of picking or labels dominates the degradation** — compare `picking` and `labels` against `plain` at each N and say plainly which cost grows faster, and whether picking must become optional above some N.
5. VRAM against the spec's prediction that it is not close (10⁷ × 12 B = 120 MB on a 10 GB card) — confirm or refute with the measured numbers.
6. A **Limitations** section: what this does not measure (a real Tessera payload's layout, tile-caching `TileLayer` behaviour, multiple simultaneous layers, a non-idle desktop), and any N that crashed or was dropped.
7. One-line headline: **the P1 ceiling is N ≈ …**, which Task 7 consumes.

- [ ] **Step 6: Commit**

```bash
cd /home/joe/code/tessera
git add probes/markbudget/web/build.mjs probes/markbudget/web/src/p1.js \
        probes/markbudget/web/p1.html probes/markbudget/web/package.json \
        probes/markbudget/web/package-lock.json \
        probes/markbudget/results probes/markbudget/P1-render.md
git commit -m "test(probes): P1 — GPU render ceiling swept to 3e7 marks across four arms"
```

---

### Task 5: P2 — the transport and decode ceiling

Spec §4/P2: serve N points as Arrow IPC `(handle: uint32, x: float32, y: float32, …scalars)`, sweeping N over the same range. Measure separately bytes on the wire, transfer time, and `apache-arrow` decode time to GPU-ready typed arrays.

**Decides:** whether the budget is transport-bound; whether the "buffers are uncompressed for zero-copy slicing" rule (contracts §6) should hold on the *wire* as well as on disk — a separate question from §10.3; and whether prefix-then-extend delivery is necessary rather than merely available.

**The arms, and the risk this task must resolve up front.** `apache-arrow` for JavaScript has historically **not** decoded IPC buffer compression (LZ4/ZSTD) even though the Rust and C++ implementations write it. If that holds for the installed version, the LZ4/ZSTD *buffer-compression* arms are undeliverable and the deployable alternative is HTTP transport compression over uncompressed IPC — which is arguably the more realistic arm anyway, since it preserves zero-copy slicing after decompression. Step 1 settles this by experiment before any sweep is run; whichever way it lands, the report records it as a finding.

Arms:
- `raw` — uncompressed IPC, no HTTP content encoding (the contracts §6 baseline)
- `http-gzip` — uncompressed IPC, `Content-Encoding: gzip`
- `http-zstd` — uncompressed IPC, `Content-Encoding: zstd`
- `ipc-lz4` — LZ4-frame IPC buffer compression, no HTTP encoding *(gated on Step 1)*
- `ipc-zstd` — ZSTD IPC buffer compression, no HTTP encoding *(gated on Step 1)*

Network arms: the spec asks for localhost and a LAN hop. **Windows Chrome → the WSL2 collector is already a virtual NIC hop, not loopback** — that is the primary arm and the honest one, since it is the same path P1 runs over. A true LAN hop needs a second machine; if none is available, the report records the gap rather than relabelling the vNIC as a LAN.

**Files:**
- Create: `probes/markbudget/src/points.rs`, `probes/markbudget/web/src/p2.js`, `probes/markbudget/web/p2.html`, `probes/markbudget/P2-transport.md`
- Modify: `probes/markbudget/src/lib.rs`, `probes/markbudget/src/bin/collector.rs`, `probes/markbudget/Cargo.toml`, `probes/markbudget/web/build.mjs`

**Interfaces:**
- Consumes: `tessera_wire::payload::{viewport_ipc, ScalarColumn}`; `harness.js`.
- Produces:
  - `markbudget::points::synth_columns(n: usize) -> (Vec<u32>, Vec<f32>, Vec<f32>, Vec<f32>)` — handles, xs, ys, one `f32` scalar
  - `markbudget::points::raw_payload(n: usize) -> Vec<u8>` — the real `viewport_ipc` bytes
  - `markbudget::points::compressed_payload(n: usize, codec: Codec) -> anyhow::Result<Vec<u8>>` where `pub enum Codec { Raw, Lz4, Zstd }`; `Codec::Raw` delegates to `raw_payload`
  - Collector route `GET /api/points?n=<usize>&codec=<raw|lz4|zstd>` returning `application/octet-stream`, honouring `Accept-Encoding: gzip|zstd` for the HTTP arms
  - `probes/markbudget/results/p2-<stamp>.json` whose per-point `measurement` is `{n, arm, wireBytes, encodedBytes, transferMs, decodeMs, totalMs, rows}`
  - `P2-transport.md`, the written report

- [ ] **Step 1: Settle the compressed-IPC question before building anything on it**

Write a throwaway check (delete it before committing):

```bash
cd /home/joe/code/tessera/probes/markbudget/web
npm install
node -e "
  const arrow = require('apache-arrow');
  console.log('apache-arrow', require('apache-arrow/package.json').version);
  console.log('has compression support export:', Object.keys(arrow).filter(k => /compress/i.test(k)));
"
```

Then the decisive test: generate one LZ4-compressed IPC stream with Rust (a five-line `cargo run` scratch binary or a unit test that writes to `/tmp/claude-1000/-home-joe-code-tessera/.../lz4.arrow`) and attempt `tableFromIPC` on it in Node.

Record the outcome in the task report and in `P2-transport.md`:
- **If it decodes:** all five arms run.
- **If it throws:** `ipc-lz4` and `ipc-zstd` are dropped, `points.rs` still implements `Codec` (the Rust-side bytes are a real finding — they answer "how much would buffer compression save on the wire, if a client could read it"), and the sweep measures those two arms **server-side only** (bytes, no decode), clearly marked as such.

Do not skip this step or defer it into the sweep; a mid-sweep failure produces 40 minutes of unusable data.

- [ ] **Step 2: Add the arrow dependency with IPC compression**

In `probes/markbudget/Cargo.toml`, add:

```toml
arrow = { version = "59", default-features = false, features = ["ipc", "ipc_compression"] }
async-compression = { version = "0.4", features = ["tokio", "gzip", "zstd"] }
tower-http = { version = "0.6", features = ["fs", "compression-gzip", "compression-zstd"] }
```

(replacing the earlier `tower-http` line). The workspace's `arrow` is `ipc`-only; enabling `ipc_compression` here affects only this excluded package's dependency graph, which is exactly why the package is excluded.

- [ ] **Step 3: Write the failing test for the payload generator**

`probes/markbudget/src/points.rs`:

```rust
//! Synthetic `/v1/viewport` payloads for P2.
//!
//! The uncompressed arm goes through the **real** `tessera_wire::payload::viewport_ipc`, so what
//! P2 measures is the shipped schema and the shipped framing — not a lookalike that might be
//! narrower or differently aligned. The compressed arms re-encode the same logical batch with
//! Arrow IPC buffer compression, and a test pins them to the real writer's row count and schema.

use serde::Deserialize;
use tessera_wire::payload::{viewport_ipc, ScalarColumn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Raw,
    Lz4,
    Zstd,
}

/// Deterministic synthetic points: `n` marks with one `f32` scalar, plus a small tile batch.
///
/// Positions come from a fixed splitmix-style sequence so a rerun at the same `n` produces
/// byte-identical output — a probe whose payload size wobbles between runs cannot report a
/// wire-bytes column.
pub fn synth_columns(n: usize) -> (Vec<u32>, Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut handles = Vec::with_capacity(n);
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    let mut score = Vec::with_capacity(n);
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..n {
        handles.push(i as u32);
        s = s.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1);
        xs.push(((s >> 33) as f32 / 2_147_483_648.0) * 1000.0 - 500.0);
        s = s.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1);
        ys.push(((s >> 33) as f32 / 2_147_483_648.0) * 1000.0 - 500.0);
        score.push((i % 1000) as f32 / 1000.0);
    }
    (handles, xs, ys, score)
}

/// The real wire payload for `n` points, uncompressed (contracts §6).
pub fn raw_payload(n: usize) -> Vec<u8> {
    let (handles, xs, ys, score) = synth_columns(n);
    // A realistic tile batch alongside the points: ~300 tiles per viewport (design §10.4).
    let tiles: Vec<u64> = (0..300u64).collect();
    let visible: Vec<u64> = tiles.iter().map(|t| t * 37 % 10_000).collect();
    let matched: Vec<u64> = visible.iter().map(|v| v / 2).collect();
    viewport_ipc(
        &tiles,
        &visible,
        &matched,
        &handles,
        &xs,
        &ys,
        &[("score", ScalarColumn::F32(&score))],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_payload_is_deterministic_and_grows_linearly() {
        let a = raw_payload(10_000);
        let b = raw_payload(10_000);
        assert_eq!(a, b, "the same n must produce byte-identical payloads");

        let small = raw_payload(10_000).len() as f64;
        let big = raw_payload(100_000).len() as f64;
        let ratio = big / small;
        assert!(
            (8.0..12.0).contains(&ratio),
            "10x the points should be ~10x the bytes, got {ratio:.2}x"
        );
    }

    #[test]
    fn raw_payload_bytes_per_point_is_in_the_expected_range() {
        // handle u32 + x f32 + y f32 + score f32 = 16 B/point, plus fixed framing and the tile
        // batch. Appendix A's 12 B/point assumption is what P2 exists to replace; this test
        // only guards against a wildly wrong schema (e.g. f64 columns).
        let n = 1_000_000;
        let bytes_per_point = raw_payload(n).len() as f64 / n as f64;
        assert!(
            (15.0..18.0).contains(&bytes_per_point),
            "expected ~16 B/point, got {bytes_per_point:.2}"
        );
    }
}
```

Add `pub mod points;` to `probes/markbudget/src/lib.rs`.

- [ ] **Step 4: Run the tests**

```bash
cd /home/joe/code/tessera/probes/markbudget
cargo test points 2>&1 | tail -20
```
Expected: both PASS. If `raw_payload_bytes_per_point_is_in_the_expected_range` fails, read the actual figure before adjusting the bound — it is telling you something about the real schema, and **that number is a P2 finding**: record it and reconcile with Appendix A's 12 B/point (Task 1 Step 3 already marked that row as an assumption).

- [ ] **Step 5: Add the compressed codecs**

Extend `points.rs` with:

```rust
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow::ipc::{CompressionType, MetadataVersion};
use arrow::record_batch::RecordBatch;

/// Re-encode the points batch with Arrow IPC buffer compression.
///
/// Framing matches `viewport_ipc`'s (u32 LE tile-stream length, then the two streams), so a
/// client reads both arms with one code path — the arms differ only in buffer encoding.
/// `Codec::Raw` delegates to the real writer rather than reproducing it, so the baseline arm
/// cannot drift away from the shipped bytes.
pub fn compressed_payload(n: usize, codec: Codec) -> anyhow::Result<Vec<u8>> {
    if codec == Codec::Raw {
        return Ok(raw_payload(n));
    }
    let compression = match codec {
        Codec::Lz4 => CompressionType::LZ4_FRAME,
        Codec::Zstd => CompressionType::ZSTD,
        Codec::Raw => unreachable!("handled above"),
    };
    let options = IpcWriteOptions::try_new(8, false, MetadataVersion::V5)?
        .try_with_compression(Some(compression))?;

    let (handles, xs, ys, score) = synth_columns(n);
    let tiles: Vec<u64> = (0..300u64).collect();
    let visible: Vec<u64> = tiles.iter().map(|t| t * 37 % 10_000).collect();
    let matched: Vec<u64> = visible.iter().map(|v| v / 2).collect();

    let tile_schema = Arc::new(Schema::new(vec![
        Field::new("tile", DataType::UInt64, false),
        Field::new("visible", DataType::UInt64, false),
        Field::new("matched", DataType::UInt64, false),
    ]));
    let tile_batch = RecordBatch::try_new(
        tile_schema.clone(),
        vec![
            Arc::new(UInt64Array::from(tiles)) as ArrayRef,
            Arc::new(UInt64Array::from(visible)),
            Arc::new(UInt64Array::from(matched)),
        ],
    )?;

    let point_schema = Arc::new(Schema::new(vec![
        Field::new("handle", DataType::UInt32, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("score", DataType::Float32, false),
    ]));
    let point_batch = RecordBatch::try_new(
        point_schema.clone(),
        vec![
            Arc::new(UInt32Array::from(handles)) as ArrayRef,
            Arc::new(Float32Array::from(xs)),
            Arc::new(Float32Array::from(ys)),
            Arc::new(Float32Array::from(score)),
        ],
    )?;

    let tile_bytes = write_stream(&tile_schema, &tile_batch, &options)?;
    let point_bytes = write_stream(&point_schema, &point_batch, &options)?;

    let mut out = Vec::with_capacity(4 + tile_bytes.len() + point_bytes.len());
    out.extend_from_slice(&(tile_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&tile_bytes);
    out.extend_from_slice(&point_bytes);
    Ok(out)
}

fn write_stream(
    schema: &Arc<Schema>,
    batch: &RecordBatch,
    options: &IpcWriteOptions,
) -> anyhow::Result<Vec<u8>> {
    let mut writer =
        StreamWriter::try_new_with_options(Vec::new(), schema.as_ref(), options.clone())?;
    writer.write(batch)?;
    Ok(writer.into_inner()?)
}
```

The `arrow::ipc::{CompressionType, MetadataVersion}` re-export paths are the ones arrow 59 uses; if the compiler disagrees, find them with `cargo doc -p arrow --open` rather than guessing — do not switch to a hand-rolled encoder.

and the test that pins the arms together:

```rust
#[test]
fn raw_codec_is_byte_identical_to_the_real_writer() {
    assert_eq!(
        compressed_payload(1000, Codec::Raw).expect("raw"),
        raw_payload(1000),
        "Codec::Raw must be the real viewport_ipc bytes, or the baseline arm measures a lookalike"
    );
}

#[test]
fn compression_actually_shrinks_the_payload() {
    let raw = compressed_payload(1_000_000, Codec::Raw).expect("raw").len();
    for codec in [Codec::Lz4, Codec::Zstd] {
        let c = compressed_payload(1_000_000, codec).expect("compressed").len();
        assert!(c < raw, "{codec:?} produced {c} bytes against raw's {raw}");
    }
}
```

Implement `compressed_payload` until both pass. Run: `cargo test points 2>&1 | tail -20`.

- [ ] **Step 6: Add the collector route**

In `probes/markbudget/src/bin/collector.rs`, add:

```rust
#[derive(Deserialize)]
struct PointsQuery {
    n: usize,
    #[serde(default = "default_codec")]
    codec: markbudget::points::Codec,
}

fn default_codec() -> markbudget::points::Codec {
    markbudget::points::Codec::Raw
}

async fn points(axum::extract::Query(q): axum::extract::Query<PointsQuery>) -> impl IntoResponse {
    // Generated per request rather than cached: caching would measure a warm allocator, and
    // generation is off the clock the browser reads (it times `fetch` start to response end,
    // which includes it — so the report must state generation time separately; see below).
    match markbudget::points::compressed_payload(q.n, q.codec) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
```

register it with `.route("/api/points", get(points))`, and add the HTTP-compression arms by layering `tower_http::compression::CompressionLayer::new().gzip(true).zstd(true)` **on that route only** — not on the whole router, or the `raw` arm would be compressed too and the baseline would be wrong.

**Generation time must not pollute the transfer measurement.** Add a `?warm=1` handshake: the probe fetches each (n, codec) once and discards it before the measured fetch, so allocation and codec warm-up are outside the timed request. State this in the report.

- [ ] **Step 7: Write the browser probe**

`probes/markbudget/web/src/p2.js` — structure mirroring `p1.js`:

```js
// P2 — the transport and decode ceiling (drawn-mark budget spec §4).
//
// Three numbers per point, never conflated: bytes on the wire, transfer time, and the JS decode
// to GPU-ready typed arrays. "GPU-ready" is the load-bearing part — a decode that stops at an
// Arrow Table has not produced anything deck.gl can upload, so the probe drains each column to
// a TypedArray and times that too.

import { tableFromIPC } from 'apache-arrow';
import { sweep, done } from './harness.js';

const NS = [1e5, 3e5, 1e6, 3e6, 1e7, 2e7, 3e7].map(Number);

// Set from Step 1's finding. If apache-arrow cannot decode compressed IPC buffers, drop
// 'ipc-lz4' and 'ipc-zstd' here and say so in the report — do not leave them failing.
const ARMS = ['raw', 'http-gzip', 'http-zstd', 'ipc-lz4', 'ipc-zstd'];

const ARM_CONFIG = {
  raw:         { codec: 'raw',  encodings: 'identity' },
  'http-gzip': { codec: 'raw',  encodings: 'gzip' },
  'http-zstd': { codec: 'raw',  encodings: 'zstd' },
  'ipc-lz4':   { codec: 'lz4',  encodings: 'identity' },
  'ipc-zstd':  { codec: 'zstd', encodings: 'identity' },
};

const log = (msg) => {
  const el = document.getElementById('log');
  el.textContent += `${msg}\n`;
  el.scrollTop = el.scrollHeight;
};

// The payload is two concatenated Arrow IPC streams behind a u32 LE length prefix
// (tessera-wire's framing). Decoding it here is deliberate: it is part of what a real client
// must do, and it is where a naive reader would go wrong.
function splitFramed(buf) {
  const view = new DataView(buf);
  const tileLen = view.getUint32(0, true);
  return {
    tiles: new Uint8Array(buf, 4, tileLen),
    points: new Uint8Array(buf, 4 + tileLen),
  };
}

async function runArm(n, arm) {
  const cfg = ARM_CONFIG[arm];
  const url = `/api/points?n=${n}&codec=${cfg.codec}`;

  // Warm the server's generation path so it is not inside the timed request.
  await fetch(`${url}&warm=1`, { cache: 'no-store' }).then((r) => r.arrayBuffer());

  performance.clearResourceTimings();
  const t0 = performance.now();
  const res = await fetch(url, { cache: 'no-store' });
  const buf = await res.arrayBuffer();
  const tTransfer = performance.now();

  const { tiles, points } = splitFramed(buf);
  const tileTable = tableFromIPC(tiles);
  const pointTable = tableFromIPC(points);
  // Drain to typed arrays — this is what deck.gl needs, and it is not free.
  const xs = pointTable.getChild('x').toArray();
  const ys = pointTable.getChild('y').toArray();
  const handles = pointTable.getChild('handle').toArray();
  const tDecode = performance.now();

  // Resource timing gives encoded vs decoded size, which is how the HTTP-compression arms are
  // distinguished from the raw one on the wire.
  const entry = performance.getEntriesByName(new URL(url, location.href).href).pop();

  return {
    n,
    arm,
    rows: pointTable.numRows,
    tiles: tileTable.numRows,
    wireBytes: buf.byteLength,
    encodedBytes: entry ? entry.encodedBodySize : null,
    transferBytes: entry ? entry.transferSize : null,
    transferMs: tTransfer - t0,
    decodeMs: tDecode - tTransfer,
    totalMs: tDecode - t0,
    typedArrayBytes: xs.byteLength + ys.byteLength + handles.byteLength,
  };
}

(async () => {
  await sweep({ probe: 'p2', ns: NS, arms: ARMS, onProgress: log, run: runArm });
  log('done');
  await done();
})();
```

`probes/markbudget/web/p2.html` — same shell as `smoke.html`, titled *markbudget — P2 transport and decode*, loading `/dist/p2.js`. Add `'p2'` to `build.mjs`'s `ENTRIES`.

- [ ] **Step 8: Run the sweep**

```bash
cd /home/joe/code/tessera/probes/markbudget
./run.sh p2
```
Expected: 5 arms × 7 N (or 3 × 7 if Step 1 dropped the IPC arms). At 3×10⁷ the raw payload is ~500 MB — **check that Chrome does not OOM before trusting the top two N**, and if it does, record the ceiling as a finding rather than shrinking the sweep silently.

- [ ] **Step 9: Write the report**

`probes/markbudget/P2-transport.md`. Required contents:

1. Method, hardware, and the network path actually measured (Windows Chrome → WSL2 vNIC), with the **true-LAN-hop gap stated explicitly** if no second machine was available.
2. Step 1's finding on `apache-arrow` compressed-IPC support, stated as a finding with the version tested.
3. A table: arm × N → wire bytes, encoded bytes, transfer ms, decode ms, end-to-end ms, B/point.
4. **The split between transfer and decode** at each N — the spec asks for this specifically, because they imply different fixes.
5. **The N at which end-to-end crosses 100 ms and 1 s**, per arm, as intervals.
6. **The three decisions the spec assigns to P2**, answered directly:
   - is the budget transport-bound? (compare against P1's ceiling)
   - should "buffers are uncompressed for zero-copy slicing" (contracts §6) hold on the **wire** as well as on disk? Answer from the measured decode penalty against the measured byte saving — and note that HTTP transport compression preserves zero-copy slicing after decompression while IPC buffer compression does not.
   - is prefix-then-extend delivery **necessary** rather than merely available? Answer from the N at which end-to-end crosses 100 ms.
7. The measured B/point, reconciled against Appendix A's 12 B/point assumption (Task 1 marked that row pending this).
8. One-line headline: **the P2 ceiling is N ≈ …**, which Task 7 consumes.

- [ ] **Step 10: Commit**

```bash
cd /home/joe/code/tessera
git add probes/markbudget/Cargo.toml probes/markbudget/src \
        probes/markbudget/web/build.mjs probes/markbudget/web/src/p2.js \
        probes/markbudget/web/p2.html probes/markbudget/web/package.json \
        probes/markbudget/web/package-lock.json \
        probes/markbudget/results probes/markbudget/P2-transport.md
git commit -m "test(probes): P2 — transport and decode ceiling over the real wire payload"
```

---

### Task 6: P3 — the handle table ceiling

Spec §4/P3: per-session handle table footprint and mint rate at N from 10⁵ to 3×10⁷, for each candidate representation in spec §5, plus growth under a simulated pan trace at 1%, 10% and 50% coverage.

This is the ceiling with **no prior art** — no comparable system mints an opaque per-session identity for every drawn mark — so the measurement is the only evidence there will be.

**What the owner has already decided (Global Constraints):** handle validity is required only for as long as the client holds the tile carrying it. That makes **B** admissible; it does **not** decide the representation, which is Task 8's job.

**Files:**
- Create: `probes/markbudget/src/handles.rs`, `probes/markbudget/src/bin/p3_handles.rs`, `probes/markbudget/P3-handles.md`
- Modify: `probes/markbudget/src/lib.rs`, `probes/markbudget/Cargo.toml` (add `[[bin]] p3_handles`)

**Interfaces:**
- Consumes: `tessera_wire::handles::HandleTable` (representation A as shipped), `tessera_types::{EntityId, Handle}`.
- Produces:
  - `pub trait HandleRep { fn handle_for(&mut self, e: EntityId) -> Handle; fn entity_of(&self, h: Handle) -> Option<EntityId>; fn footprint_bytes(&self) -> usize; fn on_pin_drain(&mut self); }`
  - `pub struct RepA` (wraps the shipped `HandleTable`), `pub struct RepB` (pin-scoped, freed on drain), `pub struct RepC` (keyed format-preserving permutation, no table)
  - Binary `p3_handles` writing `probes/markbudget/results/p3-<stamp>.json`
  - `P3-handles.md`, the written report

- [ ] **Step 1: Write the failing tests for the three representations**

`probes/markbudget/src/handles.rs`, tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn reps() -> Vec<(&'static str, Box<dyn HandleRep>)> {
        vec![
            ("A", Box::new(RepA::new())),
            ("B", Box::new(RepB::new())),
            ("C", Box::new(RepC::new(0xD1B5_4A32_D192_ED03))),
        ]
    }

    #[test]
    fn every_rep_round_trips_within_a_session() {
        for (name, mut rep) in reps() {
            for raw in [1u64, 42, 999_999, 1_000_000_007] {
                let e = EntityId::new(raw);
                let h = rep.handle_for(e);
                assert_eq!(rep.entity_of(h), Some(e), "rep {name}: handle must decode back");
                assert_eq!(rep.handle_for(e), h, "rep {name}: handle must be stable");
            }
        }
    }

    #[test]
    fn every_rep_is_injective_over_a_large_sample() {
        for (name, mut rep) in reps() {
            let mut seen = std::collections::HashSet::new();
            for raw in 0..200_000u64 {
                let h = rep.handle_for(EntityId::new(raw));
                assert!(seen.insert(h.raw()), "rep {name}: handle collision at entity {raw}");
            }
        }
    }

    #[test]
    fn rep_b_frees_on_pin_drain_and_rep_a_does_not() {
        let mut a = RepA::new();
        let mut b = RepB::new();
        for raw in 0..100_000u64 {
            a.handle_for(EntityId::new(raw));
            b.handle_for(EntityId::new(raw));
        }
        let a_before = a.footprint_bytes();
        let b_before = b.footprint_bytes();
        a.on_pin_drain();
        b.on_pin_drain();
        assert_eq!(a.footprint_bytes(), a_before, "A never frees — that is its defining cost");
        assert!(
            b.footprint_bytes() < b_before / 10,
            "B must release the drained pin's table: {} -> {}",
            b_before,
            b.footprint_bytes()
        );
    }

    #[test]
    fn rep_c_is_a_bijection_and_that_is_the_i10_concern() {
        // Every 32-bit value decodes to *some* row under C. This test does not endorse C; it
        // pins the property that Task 8's review must weigh, so a later change cannot quietly
        // alter it. Contracts §3's `404 unknown` ("nothing is enumerable") weakens under C to
        // "everything decodes, then is mask-checked".
        let rep = RepC::new(0xD1B5_4A32_D192_ED03);
        for probe in [0u32, 1, 7, 65_535, u32::MAX / 2, u32::MAX] {
            assert!(
                rep.entity_of(Handle::new(probe)).is_some(),
                "C decodes every 32-bit value by construction — including {probe}"
            );
        }
    }

    #[test]
    fn rep_c_footprint_is_constant_in_n() {
        let mut c = RepC::new(0xD1B5_4A32_D192_ED03);
        let empty = c.footprint_bytes();
        for raw in 0..1_000_000u64 {
            c.handle_for(EntityId::new(raw));
        }
        assert_eq!(c.footprint_bytes(), empty, "C stores nothing — that is its whole claim");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd /home/joe/code/tessera/probes/markbudget
cargo test handles 2>&1 | tail -20
```
Expected: compile failure — `HandleRep`, `RepA`, `RepB`, `RepC` do not exist.

- [ ] **Step 3: Implement the three representations**

Above the test module in `probes/markbudget/src/handles.rs`:

```rust
//! The three candidate handle representations from the drawn-mark budget spec §5, modelled so
//! their per-session cost can be measured before one of them is committed to the wire.
//!
//! **This is a model, not the implementation.** The shipped table is
//! `tessera_wire::handles::HandleTable` (representation A); `RepA` wraps it verbatim so the
//! measurement is of the real thing. B and C are modelled here only to be measured; adopting
//! either is Task 8's decision, and C additionally requires independent invariant review
//! because it changes an I10-adjacent property.

use std::collections::HashMap;

use tessera_types::{EntityId, Handle};
use tessera_wire::handles::HandleTable;

pub trait HandleRep {
    fn handle_for(&mut self, e: EntityId) -> Handle;
    fn entity_of(&self, h: Handle) -> Option<EntityId>;
    /// Bytes this representation holds for the session right now. Counts allocated capacity,
    /// not live length: a `HashMap` at 0.875 load factor with 2x growth costs what it reserved,
    /// and the residency question in §13.1 is about resident bytes, not logical entries.
    fn footprint_bytes(&self) -> usize;
    /// The pin this table was scoped to has drained. A no-op for representations that do not
    /// scope to a pin.
    fn on_pin_drain(&mut self);
}

/// **A** — stable for the session, never freed. The shipped `HandleTable`, wrapped.
pub struct RepA {
    inner: HandleTable,
    minted: usize,
}

/// **B** — table scoped to the pin/generation, freed when the pin drains.
///
/// Handles must stay valid while the client holds the tile carrying them (owner decision,
/// 2026-07-29), which a pin's lifetime covers; a cached tile outliving its pin gets `410` and
/// the client re-requests. Modelled as two generations: the live table and the previous one,
/// still answerable until the next drain — because dropping the previous generation the instant
/// a pin rolls would 410 every in-flight request, which is a correctness bug, not a saving.
pub struct RepB {
    live: HashMap<EntityId, Handle>,
    live_rev: Vec<EntityId>,
    prev: HashMap<EntityId, Handle>,
    prev_rev: Vec<EntityId>,
    base: u32,
}

/// **C** — derived, not stored: a keyed format-preserving permutation over the row ID.
///
/// Zero per-session bytes, O(1) both directions. **Changes an I10-adjacent property** — a
/// bijection means every 32-bit value decodes to a real row, so contracts §3's `404 unknown`
/// ("nothing is enumerable") weakens to "everything decodes, then is mask-checked". Measured
/// here; adoption needs independent review, not just a footprint number (spec §5).
///
/// Modelled as a 4-round Feistel network over 32 bits keyed per session — the standard
/// construction for a format-preserving permutation on a small domain.
pub struct RepC {
    key: u64,
    minted: usize,
}
```

```rust
impl RepA {
    pub fn new() -> Self {
        Self {
            inner: HandleTable::new(),
            minted: 0,
        }
    }
}

impl HandleRep for RepA {
    fn handle_for(&mut self, e: EntityId) -> Handle {
        let before = self.minted;
        let h = self.inner.handle_for(e);
        // `HandleTable` exposes no length accessor; a handle equal to the current count is a
        // fresh mint, anything lower is a hit.
        if h.raw() as usize >= before {
            self.minted = h.raw() as usize + 1;
        }
        h
    }

    fn entity_of(&self, h: Handle) -> Option<EntityId> {
        self.inner.entity_of(h)
    }

    /// **Modelled, not measured.** The shipped `HandleTable` exposes no capacity accessor, so
    /// this estimates `HashMap<EntityId, Handle>` at its allocated (not live) size — std's
    /// hashbrown keeps load factor at 7/8 and doubles, so ~2x the logical entry bytes is the
    /// honest steady-state estimate — plus the reverse `Vec<EntityId>`. The report must state
    /// this is a model and reconcile it against the binary's measured RSS delta.
    fn footprint_bytes(&self) -> usize {
        let entry = std::mem::size_of::<EntityId>() + std::mem::size_of::<Handle>();
        self.minted * entry * 2 + self.minted * std::mem::size_of::<EntityId>()
    }

    /// A never frees. That is its defining cost, not an oversight.
    fn on_pin_drain(&mut self) {}
}

impl RepB {
    pub fn new() -> Self {
        Self {
            live: HashMap::new(),
            live_rev: Vec::new(),
            prev: HashMap::new(),
            prev_rev: Vec::new(),
            base: 0,
        }
    }
}

impl HandleRep for RepB {
    fn handle_for(&mut self, e: EntityId) -> Handle {
        if let Some(h) = self.live.get(&e) {
            return *h;
        }
        // A handle minted under the previous generation stays valid until the next drain, and
        // re-minting it would hand the same entity two handles inside one session.
        if let Some(h) = self.prev.get(&e) {
            return *h;
        }
        let h = Handle::new(self.base + self.live_rev.len() as u32);
        self.live_rev.push(e);
        self.live.insert(e, h);
        h
    }

    fn entity_of(&self, h: Handle) -> Option<EntityId> {
        let raw = h.raw();
        if raw >= self.base {
            return self.live_rev.get((raw - self.base) as usize).copied();
        }
        let prev_base = self.base - self.prev_rev.len() as u32;
        if raw >= prev_base {
            return self.prev_rev.get((raw - prev_base) as usize).copied();
        }
        None
    }

    fn footprint_bytes(&self) -> usize {
        let entry = std::mem::size_of::<EntityId>() + std::mem::size_of::<Handle>();
        let e = std::mem::size_of::<EntityId>();
        (self.live.len() + self.prev.len()) * entry * 2
            + (self.live_rev.len() + self.prev_rev.len()) * e
    }

    /// The pin drained: the live generation becomes the previous one and the old previous
    /// generation is released. Keeping one generation back is not slack — dropping it the
    /// instant a pin rolls would `410` every in-flight request.
    fn on_pin_drain(&mut self) {
        self.base += self.live_rev.len() as u32;
        self.prev = std::mem::take(&mut self.live);
        self.prev_rev = std::mem::take(&mut self.live_rev);
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl RepC {
    pub fn new(key: u64) -> Self {
        Self { key, minted: 0 }
    }

    /// Diagnostic only — C stores nothing, so this counts calls, not memory. It exists so the
    /// mint-throughput arm can assert it actually minted what it thinks it did.
    pub fn minted(&self) -> usize {
        self.minted
    }

    const ROUNDS: u32 = 4;

    fn round_fn(&self, round: u32, half: u16) -> u16 {
        (splitmix64(self.key ^ ((round as u64) << 48) ^ (half as u64)) >> 48) as u16
    }

    fn encrypt(&self, v: u32) -> u32 {
        let (mut l, mut r) = ((v >> 16) as u16, (v & 0xFFFF) as u16);
        for round in 0..Self::ROUNDS {
            let next = l ^ self.round_fn(round, r);
            l = r;
            r = next;
        }
        ((l as u32) << 16) | r as u32
    }

    fn decrypt(&self, v: u32) -> u32 {
        let (mut l, mut r) = ((v >> 16) as u16, (v & 0xFFFF) as u16);
        for round in (0..Self::ROUNDS).rev() {
            let prev = r ^ self.round_fn(round, l);
            r = l;
            l = prev;
        }
        ((l as u32) << 16) | r as u32
    }
}

impl HandleRep for RepC {
    fn handle_for(&mut self, e: EntityId) -> Handle {
        self.minted += 1;
        // The domain is the row ID, which is u32 by construction (contracts §2.6).
        Handle::new(self.encrypt(e.raw() as u32))
    }

    /// Returns `Some` for **every** 32-bit input, by construction. That is not a bug to fix —
    /// it is the I10-adjacent property the spec §5 flags: contracts §3's `404 unknown`
    /// ("nothing is enumerable") weakens to "everything decodes, then is mask-checked".
    fn entity_of(&self, h: Handle) -> Option<EntityId> {
        Some(EntityId::new(self.decrypt(h.raw()) as u64))
    }

    fn footprint_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    fn on_pin_drain(&mut self) {}
}
```

Add `pub mod handles;` to `src/lib.rs` and a `[[bin]] name = "p3_handles"` / `path = "src/bin/p3_handles.rs"` entry to `Cargo.toml`.

**One thing to watch.** `every_rep_is_injective_over_a_large_sample` walks entity IDs `0..200_000`; C is injective over the *full* 32-bit domain by construction, so it passes trivially. B's injectivity is the one that could actually break — check the `base` arithmetic if it fails.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd /home/joe/code/tessera/probes/markbudget
cargo test handles 2>&1 | tail -20
```
Expected: 5 PASS.

- [ ] **Step 5: Write the measurement binary**

`probes/markbudget/src/bin/p3_handles.rs`:

```rust
//! P3 — the handle-table ceiling (drawn-mark budget spec §4).
//!
//! Three measurements per representation: footprint at N, mint throughput, and growth under a
//! simulated pan trace. Results land in `results/p3-<stamp>.json`, the same directory and
//! naming the browser probes use, so `RESULTS.md` reads all four uniformly.

use std::time::Instant;

use markbudget::handles::{HandleRep, RepA, RepB, RepC};
use serde_json::json;
use tessera_types::EntityId;

const NS: &[usize] = &[100_000, 300_000, 1_000_000, 3_000_000, 10_000_000, 30_000_000];
const CORPUS: u64 = 1_000_000_000;
const PAN_STEPS: usize = 2_000;
const PIN_TILES: usize = 32;
const TILE_ROWS: usize = 266_000; // a depth-6 tile at 10^9 (probes/optimisations §4.2)
const COVERAGES: &[f64] = &[0.01, 0.10, 0.50];
const KEY: u64 = 0xD1B5_4A32_D192_ED03;

fn rep(name: &str) -> Box<dyn HandleRep> {
    match name {
        "A" => Box::new(RepA::new()),
        "B" => Box::new(RepB::new()),
        "C" => Box::new(RepC::new(KEY)),
        other => panic!("unknown representation {other}"),
    }
}

/// Resident bytes for this process, from `/proc/self/statm` field 2 (resident pages).
///
/// This is the number that lands on the §13.1 residency ceiling; `footprint_bytes()` is a model
/// and the report must reconcile the two rather than quoting whichever is convenient.
fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").expect("read /proc/self/statm");
    let pages: u64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() -> anyhow::Result<()> {
    let mut footprints = Vec::new();
    for name in ["A", "B", "C"] {
        for &n in NS {
            // Pre-generate so the RNG is outside the timed mint loop.
            let mut s = KEY;
            let ids: Vec<EntityId> = (0..n)
                .map(|_| EntityId::new(splitmix(&mut s) % CORPUS))
                .collect();

            let rss_before = rss_bytes();
            let mut r = rep(name);
            let t0 = Instant::now();
            for &e in &ids {
                r.handle_for(e);
            }
            let elapsed = t0.elapsed();
            let rss_after = rss_bytes();

            let row = json!({
                "rep": name,
                "n": n,
                "modelled_bytes": r.footprint_bytes(),
                "rss_delta_bytes": rss_after.saturating_sub(rss_before),
                "mint_seconds": elapsed.as_secs_f64(),
                "mints_per_second": n as f64 / elapsed.as_secs_f64(),
            });
            eprintln!("{row}");
            footprints.push(row);
            drop(r);
        }
    }

    let mut traces = Vec::new();
    for name in ["A", "B", "C"] {
        for &coverage in COVERAGES {
            let visible_per_tile = (TILE_ROWS as f64 * coverage) as usize;
            let mut r = rep(name);
            let mut s = KEY ^ coverage.to_bits();
            let mut trajectory = Vec::with_capacity(PAN_STEPS);
            for step in 0..PAN_STEPS {
                // A random walk over Morton tile space: each step surfaces one tile's worth of
                // visible rows, drawn from a tile-local entity range so the walk has locality
                // rather than re-drawing the whole corpus every step.
                let tile_base = splitmix(&mut s) % (CORPUS - TILE_ROWS as u64);
                for _ in 0..visible_per_tile {
                    let off = splitmix(&mut s) % TILE_ROWS as u64;
                    r.handle_for(EntityId::new(tile_base + off));
                }
                if step % PIN_TILES == PIN_TILES - 1 {
                    r.on_pin_drain();
                }
                trajectory.push(r.footprint_bytes());
            }
            let row = json!({
                "rep": name,
                "coverage": coverage,
                "visible_per_tile": visible_per_tile,
                "steps": PAN_STEPS,
                "pin_tiles": PIN_TILES,
                "final_bytes": trajectory[trajectory.len() - 1],
                "peak_bytes": trajectory.iter().max(),
                // Every 20th sample: the shape is the finding, and 2,000 raw points per arm
                // would bury it.
                "trajectory_sampled": trajectory.iter().step_by(20).collect::<Vec<_>>(),
            });
            eprintln!(
                "rep {name} coverage {coverage}: final {} bytes",
                trajectory[trajectory.len() - 1]
            );
            traces.push(row);
        }
    }

    let record = json!({
        "probe": "p3",
        "corpus": CORPUS,
        "tile_rows": TILE_ROWS,
        "footprints": footprints,
        "pan_traces": traces,
    });

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = dir.join(format!("p3-{stamp}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}
```

Add `tessera-types = { path = "../../crates/tessera-types" }` to the probe project's dependencies for `EntityId`.

- [ ] **Step 6: Run the measurement**

```bash
cd /home/joe/code/tessera/probes/markbudget
cargo run --release --bin p3_handles 2>&1 | tail -40
```
Expected: a `p3-*.json` in `results/`. **Watch RSS**: representation A at N = 3×10⁷ is plausibly 1.5–2.5 GB in this model and the box has ~12 GB available — if the process is killed, that is a result. Record the N at which it died and re-run without it.

- [ ] **Step 7: Write the report**

`probes/markbudget/P3-handles.md`. Required contents:

1. Method, and an explicit statement of which numbers are **modelled** (A's `footprint_bytes`) versus **measured** (RSS, throughput, trajectory).
2. Table: representation × N → modelled bytes, measured RSS delta, bytes/handle, mint throughput.
3. The pan-trace trajectories at 1%, 10% and 50% coverage, with the shape described in words, not only plotted numbers: does A grow without bound across the trace, and how fast; where does B plateau; C is flat by construction.
4. **The per-session cost multiplied by concurrent sessions**, against §13.1's residency ceiling — the spec assigns this decision to P3 specifically. State how many concurrent sessions each representation permits at a 10⁷ mark budget on a 10 GB / 47 GB box.
5. The mint-rate ceiling: at what marks/second does minting itself become the bottleneck, and how does that compare to P1's and P2's ceilings.
6. **No recommendation.** The representation decision is Task 8's, and it is not a performance-only decision. State the numbers; leave the choice.
7. One-line headline: **the P3 ceiling is N ≈ … per session for representation …**, which Task 7 consumes.

- [ ] **Step 8: Commit**

```bash
cd /home/joe/code/tessera
git add probes/markbudget/Cargo.toml probes/markbudget/src/lib.rs \
        probes/markbudget/src/handles.rs probes/markbudget/src/bin/p3_handles.rs \
        probes/markbudget/results probes/markbudget/P3-handles.md
git commit -m "test(probes): P3 — handle-table footprint, mint rate and pan-trace growth for A/B/C"
```

---

### Task 7: Calibrate the `k` cap from the measured minimum

Spec §9: the spec succeeds when "a server-side k cap is set from their minimum rather than assumed". Today `DEFAULT_MAX_K = 200` and `k` defaults to 30 — both assumptions, neither measured.

Also fixes the coupling the spec's §6 argues about: Phase 1 Task 16's exit criterion is written at `k = 30`, so as written it would pass against a mark budget the product does not want.

**Files:**
- Modify: `crates/tessera-server/src/config.rs` (`DEFAULT_MAX_K` and its doc)
- Modify: `docs/design/contracts.md` (§3's `k` line)
- Modify: `docs/archive/plans/2026-07-28-phase1-walking-skeleton.md` (Task 16 Step 3)

**Interfaces:**
- Consumes: the three headline numbers from `P1-render.md`, `P2-transport.md`, `P3-handles.md`.
- Produces: a `DEFAULT_MAX_K` justified in a comment by the three measurements; a Task 16 exit measurement that sweeps `k`.

- [ ] **Step 1: Write the failing test**

Add to `crates/tessera-server/tests/http.rs`:

```rust
#[test]
fn default_max_k_is_the_calibrated_cap_not_the_placeholder() {
    // The cap is set from the minimum of P1 (GPU render), P2 (transport and decode) and P3
    // (handle table) — see probes/markbudget/RESULTS.md. 200 was the pre-measurement
    // placeholder; this test exists so a later edit cannot quietly revert to a guess.
    assert_eq!(
        tessera_server::config::DEFAULT_MAX_K,
        CALIBRATED_MAX_K,
        "max_k must match the calibrated figure recorded in probes/markbudget/RESULTS.md"
    );
}
```

where `CALIBRATED_MAX_K` is the literal you derive in Step 3. If `DEFAULT_MAX_K` is currently private, make it `pub` — it is a documented default (`config.rs:6` already says so) and pinning it in a test is worth the visibility.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd /home/joe/code/tessera
cargo test -p tessera-server default_max_k_is_the_calibrated_cap 2>&1 | tail -20
```
Expected: FAIL (`200 != <calibrated>`), or a compile error if `DEFAULT_MAX_K` was private.

- [ ] **Step 3: Derive and set the cap**

`k` in the contracts is **per tile**; the probe ceilings are **per viewport**. Do not conflate them. The derivation, which must be written out in the comment:

```
viewport_ceiling = min(P1_ceiling, P2_ceiling, P3_ceiling)      # marks per viewport
tiles_per_viewport ≈ 300                                        # design §10.4
max_k = floor(viewport_ceiling / tiles_per_viewport)
```

Then round **down** to a round number, and cap it at whatever the smallest ceiling actually supports rather than the arithmetic optimum. Update `crates/tessera-server/src/config.rs`:

```rust
/// The server-side per-tile mark cap.
///
/// **Calibrated, not assumed** (2026-07-29). Set from the minimum of the three measured
/// ceilings — P1 GPU render, P2 transport and decode, P3 handle table — divided by the ~300
/// tiles a viewport resolves to (design §10.4). The binding ceiling was <P?>; see
/// `probes/markbudget/RESULTS.md` for the numbers and `probes/markbudget/P<?>-*.md` for the
/// method. The previous value of 200 predated any measurement.
///
/// A client may request less; `k` above this is silently clamped (contracts §3: "k
/// (server-capped)"). Raising it requires re-running P1–P3, not an argument.
pub const DEFAULT_MAX_K: usize = <calibrated>;
```

Fill `<calibrated>` and `<P?>` from the reports. Run the test: `cargo test -p tessera-server default_max_k_is_the_calibrated_cap 2>&1 | tail -10` → PASS.

- [ ] **Step 4: Record the cap in the contracts spec**

§3's viewport line currently reads:

> `POST /v1/viewport` — `{slice, zoom, bbox: [x0,y0,x1,y1], k?, filters?, pin?}`; `k` defaults to 30, capped by `max_k`. Response **Arrow**, two batches:

Amend to:

```markdown
`POST /v1/viewport` — `{slice, zoom, bbox: [x0,y0,x1,y1], k?, filters?, pin?}`; `k` defaults to 30, capped by `max_k`. **`max_k` is calibrated, not assumed** *(r5)*: it is set from the minimum of the measured GPU-render, transport-and-decode and handle-table ceilings divided by the ~300 tiles a viewport resolves to. Raising it is a measurement, not a configuration preference. Because priority is a fixed per-entity constant (§2.6), selection is prefix-stable in *k* — the *k* winners at a small *k* are a strict subset of those at a large one — so a large budget need not be one blocking response: a server may send a prefix and extend it, and both responses are the same definition evaluated at different *k*. Response **Arrow**, two batches:
```

- [ ] **Step 5: Re-point Phase 1 Task 16's exit measurement at the calibrated k**

In `docs/archive/plans/2026-07-28-phase1-walking-skeleton.md`, Task 16 Step 3 currently ends `…mixed zooms, k = 30) over HTTP; report p50/p99/max server-side and end-to-end. **Exit gate: server-side p99 < 10 ms.**`

Replace the `k = 30` clause and the gate sentence with:

```markdown
mixed zooms, **swept over k ∈ {30, 300, 3 000, max_k}**) over HTTP; report p50/p99/max
server-side and end-to-end **at each k**. **Exit gate: server-side p99 < 10 ms at k = 30**
(the walking skeleton's own criterion) **and the p99-vs-k curve recorded** for the calibrated
`max_k`, so Phase 2 inherits a measured curve rather than a single point. A p99 above 10 ms at
the calibrated `max_k` is a *finding to record*, not a Phase 1 failure — Phase 1's sampler is
the deliberately-wrong first-k placeholder, so its cost at large k is not the shipped cost.
```

This is the coupling the spec's §6 warns about: without it, Phase 1 passes against a mark budget the product does not want.

- [ ] **Step 6: Verify**

```bash
cd /home/joe/code/tessera
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace 2>&1 | tail -10
bash scripts/check-layers.sh
grep -n "k = 30" docs/archive/plans/2026-07-28-phase1-walking-skeleton.md
```
Expected: all green; the grep shows `k = 30` only inside the new exit-gate sentence.

- [ ] **Step 7: Commit**

```bash
cd /home/joe/code/tessera
git add crates/tessera-server/src/config.rs crates/tessera-server/tests/http.rs \
        docs/design/contracts.md \
        docs/archive/plans/2026-07-28-phase1-walking-skeleton.md
git commit -m "feat(server): calibrate max_k from the measured P1/P2/P3 minimum"
```

---

### Task 8: The §5 handle decision, and the results record

Spec §9's remaining criteria: the §5 handle representation is decided and recorded with its reasoning, and P4 is scheduled with the k range P1–P3 established.

**Scope discipline.** The spec asks for the representation to be **decided and recorded**, not implemented. Switching the shipped `HandleTable` from A to B is real work in `tessera-wire` and `tessera-server` (pin-drain plumbing, a `410` path) and belongs to whichever phase takes it on; this task records the decision, its reasoning, and the implementation obligation. The one code change it does make is the u32 exhaustion guard, a Phase 1 deferred minor that the decision makes concrete.

**Files:**
- Create: `docs/evidence/memos/2026-07-29-handle-representation.md`, `probes/markbudget/RESULTS.md`
- Modify: `crates/tessera-wire/src/handles.rs`, `docs/design/implementation-plan.md`, `probes/optimisations.md`

**Interfaces:**
- Consumes: all three probe reports; Task 7's calibrated `max_k`.
- Produces: the recorded decision; `RESULTS.md` as the single citable artefact; a scheduled P4.

- [ ] **Step 1: Get option C reviewed against I10 before writing the memo**

Spec §5: option C "must not be adopted on performance grounds alone" and "needs independent review before adoption, not just measurement". Dispatch a reviewer with **no stake in the outcome** and give it exactly: `docs/design/architecture.md` §4 (I10) and Appendix C (the leak register), `docs/design/system-architecture.md` §4.5, `docs/design/contracts.md` §3 and §6, `probes/markbudget/src/handles.rs`, `probes/markbudget/P3-handles.md`, and spec §5.

The question to put to it, verbatim:

> Under representation C (handle = keyed format-preserving permutation over the row ID), every 32-bit value decodes to a real row, so `404 unknown` becomes "everything decodes, then is mask-checked". Does that weaken I10, and does it open a leak that is not already in Appendix C (C1–C16)? Consider specifically: (a) whether a viewer can distinguish "handle outside my mask" from "handle of a row that does not exist", and what that discloses about corpus size or density; (b) whether per-session keying is sufficient to prevent cross-session correlation; (c) whether the mask check on every decoded handle is a new oracle for probing the visible set, and at what rate; (d) whether anything in Appendix C already covers this shape. Answer the question asked; do not evaluate A or B.

Record the reviewer's verdict verbatim in the memo. If the reviewer finds a leak not in Appendix C, **C is out** regardless of its footprint, and the memo says so.

- [ ] **Step 2: Write the decision memo**

`docs/evidence/memos/2026-07-29-handle-representation.md`, following the house style of the existing memo in that directory. Required contents:

1. **The question**, and the constraint that rules out the easy answer: tile-caching clients (`TileLayer`) hold a tile's payload across pans, so a handle must stay valid as long as the client holds the tile carrying it — minting fresh per request breaks `POST /v1/items/{handle}` for any cached tile.
2. **What SA §4.5 fixes and no option may violate**: a handle decodes to an index into the worker's per-session handle table, never an entity ID; handles are per-session-keyed and structureless to the client; entity IDs cross no process boundary.
3. **The owner decision of 2026-07-29** on the spec's open question: handle validity is required only for as long as the client holds the tile, not across pan-away-and-return. Record that this is a client-contract decision and that it makes B admissible.
4. **The measurements**, cited from `P3-handles.md` — footprint, mint rate, pan-trace trajectory for all three.
5. **The I10 review of option C**, verbatim from Step 1.
6. **The decision**, with reasoning, in this order: the invariant argument first, the cost second. If C survived review and B is admissible, say plainly why the chosen one wins on grounds that are not only performance.
7. **The implementation obligation**: what has to change, in which crate, and when — including the `410`-on-drained-pin path if B is chosen, and the client-side contract note that goes to the visualisation architecture.
8. **What would reopen this**: the specific measurement or contract change that would overturn the decision.

- [ ] **Step 3: Add the handle-exhaustion guard**

Phase 1 ledger, Task 12: *"minor (deferred): no u32 handle-exhaustion guard in `handle_for` (aliasing past `u32::MAX` mints)"*. At a 10⁷ mark budget across a long session this stops being theoretical. Write the failing test first, in `crates/tessera-wire/src/handles.rs`'s test module:

```rust
#[test]
fn minting_past_u32_max_fails_closed_rather_than_aliasing() {
    // Aliasing at the u32 boundary would hand two entities the same handle within one session,
    // so `entity_of` would resolve one viewer's mark to another entity's row. That is an I10
    // failure, not a capacity inconvenience — it must fail closed.
    let mut table = HandleTable::new();
    table.force_next_handle_for_test(u32::MAX);
    let a = table.try_handle_for(EntityId::new(1));
    assert!(a.is_ok(), "the last handle must still be mintable");
    let b = table.try_handle_for(EntityId::new(2));
    assert!(b.is_err(), "minting past u32::MAX must fail closed, not alias");
}
```

Implement: add `pub fn try_handle_for(&mut self, e: EntityId) -> Result<Handle, HandleExhausted>` returning an error when `handle_to_entity.len() == u32::MAX as usize`; keep `handle_for` as a thin wrapper that panics with a clear message (callers in `tessera-server` migrate to `try_handle_for` and map it to a `503`, since it is a resource condition, not a client error). `force_next_handle_for_test` is `#[cfg(test)]` only.

Run: `cargo test -p tessera-wire 2>&1 | tail -20` → all PASS.

Then record the decision at the definition site — extend the existing `HandleTable` doc comment with a short paragraph naming the chosen representation, pointing at the memo, and stating what is implemented today versus what the decision obliges.

- [ ] **Step 4: Write `RESULTS.md`**

`probes/markbudget/RESULTS.md` — the single artefact anything else cites. Required contents:

1. **The headline**, first line: the four ceilings, which one binds, and the calibrated `max_k`.
2. A table: probe → what it measures → measured ceiling → confidence → report file.
3. **Was the spec's prediction right?** Spec §3 records the belief that it binds at *transport or the handle table*, not GPU or storage, and explicitly asks the probes to falsify it. Say whether it did.
4. The four §8 corrections, marked applied with their commit.
5. **What is still not known**: P4 (gather and selection), the true-LAN hop if it was not run, any N that crashed, and every limitation the three reports flagged.
6. Hardware and dates, so a rerun is comparable.

- [ ] **Step 5: Schedule P4**

Spec §6 puts P4 post-Phase-1, pre-Phase-2, and §9 requires it scheduled *with the k range P1–P3 established*.

In `docs/design/implementation-plan.md` §14, in the "What decides it" list, replace input 2's text with a version naming the now-known k range:

```markdown
2. **The gather probe** (probes, optimisations §3.5, re-scoped to large *k*). Phase 0 measured
   no column read at all, so the retrieval half of the case is modelled. The read that matters
   is the *priority* column under direct evaluation, which touches every visible row in a tile
   range rather than *k*. **Scheduled post-Phase-1, pre-Phase-2, over k ∈ [<lo>, <hi>]** — the
   range P1–P3 established (probes/markbudget/RESULTS.md) — because the gather inverts from a
   sparse point read to a near-full scan of the geometry columns somewhere inside it, and that
   inversion is what decides whether §10.3's uncompressed rule should be qualified per column
   group.
```

In `probes/optimisations.md` §3.5, append to the *Probe:* paragraph:

```markdown
**Re-scoped to large *k* (2026-07-29).** The output gather is no longer a control: at a mark
budget of 10⁶–10⁷ it touches ~1% of rows over 10⁹, which at 1024 f32s per 4 KB page means
essentially every page — the gather stops being a sparse point read and becomes a scan of the
geometry columns (~24 GB at 10⁹) to extract a sparse subset. That is the regime where block
codecs win, and x and y are Morton-sorted and therefore strongly structured. Sweep k from 10³
to the calibrated ceiling (probes/markbudget/RESULTS.md), measure column-major against
`priority` split into its own file, and uncompressed against delta/frame-of-reference-encoded
x and y. Report the k at which the gather ceases to be page-sparse.
```

Fill `<lo>`/`<hi>` from `RESULTS.md`.

- [ ] **Step 6: Verify against the spec's success criteria**

Spec §9 lists four. Check each explicitly and record the check in the task report:

```bash
cd /home/joe/code/tessera
ls probes/markbudget/P1-render.md probes/markbudget/P2-transport.md \
   probes/markbudget/P3-handles.md probes/markbudget/RESULTS.md \
   docs/evidence/memos/2026-07-29-handle-representation.md
grep -n "DEFAULT_MAX_K" crates/tessera-server/src/config.rs
grep -n "r20" docs/design/architecture.md
grep -n "Scheduled post-Phase-1" docs/design/implementation-plan.md
cargo test --workspace 2>&1 | tail -10
bash scripts/check-layers.sh
```

1. P1–P3 reported and `max_k` set from their minimum — ✅ / ❌
2. §5 representation decided and recorded with reasoning — ✅ / ❌
3. The four §8 corrections applied — ✅ / ❌
4. P4 scheduled with the k range P1–P3 established — ✅ / ❌

Any ❌ is not done. Report it rather than closing the task.

- [ ] **Step 7: Commit**

```bash
cd /home/joe/code/tessera
git add probes/markbudget/RESULTS.md docs/evidence/memos/2026-07-29-handle-representation.md \
        crates/tessera-wire/src/handles.rs \
        docs/design/implementation-plan.md probes/optimisations.md
git commit -m "docs(probes): the drawn-mark budget result, the handle decision, and P4 scheduled"
```

---

## Out of scope for this plan

Stated so nobody widens it silently:

- **P4 (gather and selection)** — spec §6 places it post-Phase-1, pre-Phase-2; it needs real 10⁹ columns, which are Phase 1 Task 16's output. Task 8 schedules it; it does not run it.
- **Storage layout decisions** — spec §9 says explicitly that this spec does not attempt to decide them. Column-group split, per-group codec policy and plan §14's signature-major decision are Phase 2, gated on P4.
- **Implementing the chosen handle representation** — Task 8 decides and records; the code change (pin-drain plumbing, the `410` path) belongs to the phase that takes it on.
- **`entity_id` narrowing** — spec §2 considered and rejected it. Do not revisit.
- **Phase 1 Task 16 itself** — this plan re-points its exit measurement at a swept `k` (Task 7 Step 5) and lands the format change before its build, but running the 10⁹ build and writing `phase1-results.md` stays with Phase 1's own plan and its SDD ledger.

## Risks the executor should watch

- **Disk.** 67 GB free against a ~60 GB 10⁹ bundle. Nothing in this plan writes at that scale, but Task 2 changes the format that bundle will be built in, so a mistake here is expensive downstream. Check `df -h` before Phase 1 resumes.
- **Chrome OOM at the top of both browser sweeps.** 3×10⁷ points is ~360 MB of attributes (P1) or ~500 MB of payload (P2). A crash is a result — record the N, do not silently shrink the sweep.
- **`apache-arrow` and compressed IPC.** Task 5 Step 1 exists to settle this before 40 minutes of sweeping. Do not defer it.
- **The `code_range` overflow trap** in Task 2. Narrowing `Tile::code_range` to `u32` is the obvious-looking simplification and it is wrong at depth 0; the test in Step 1 is what catches it.
- **Uncommitted Task 16 work in the tree.** Every commit step names its paths. Never `git add -A`.
