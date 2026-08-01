> **ARCHIVED 2026-08-01 — NEVER EXECUTED, and superseded. Its SDD workspace is empty. The memory win was delivered instead by owner-directed streaming-build work under Phase 1 Task 16 (e5b5bea) and later by Phase 2 Gate G2 (spill.rs).**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# Build Memory Tier 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut `tessera build`'s peak RSS at 10⁹ items from ~150 GB to ~50–60 GB by replacing per-item heap containers with flat CSR and struct-of-arrays representations, without changing a single byte of any bundle it produces.

**Architecture:** The build's memory is not data, it is containers. The corpus is 10⁹ items carrying 1,718,472,823 term pairs — 6.9 GB of term IDs and 8 GB of geometry. The current pipeline spends ~150 GB on it, almost entirely in 10⁹ individual heap allocations (one `Vec` per item, twice over) plus a `HashMap` keyed by entity ID. Every replacement here is a representation change behind the same arithmetic: `HashMap<u64, Vec<u64>>` and `Vec<StagedItem>` become compressed sparse row (offsets + flat values); `Vec<TilerItem>` becomes struct-of-arrays with one typed column per declared scalar, which is what `columns.arrow` holds anyway; `digest_file` streams instead of slurping. No ordering, no hashing and no field value changes, so the bundle bytes are invariant — and Task 1 builds the harness that proves it before anything else moves.

**Tech Stack:** Rust 2021, `arrow-rs`, `parquet`, `sha2`, `memmap2`. Workspace at `crates/`.

## Global Constraints

- **Bundle bytes must not change.** Entity-ID assignment is permanent under I9 (design §11.1): the ordering chosen at first build is the ordering the corpus keeps forever. Every task ends by running the Task 1 golden test, and a hash mismatch is a task failure, not a fixture update.
- **I4 is compile-time.** `EntityId(u64)` and `RowId(u32)` are distinct newtypes with no conversion except through the versioned permutation (plan §2.1). Do **not** narrow entity IDs to raw `u32` to save bytes — the 4 GB is not worth demoting I4 to a discipline.
- **On-disk widths are not this plan's business.** The segment writer emits `morton.u32` and `write_segment` takes `codes: &[u32]`; `entity_id` stays `uint64` in `columns.arrow`. Whatever the tree does at Task 1's blessing time is the reference — this plan changes memory representation only, never a byte the writer emits. If a task's diff changes an on-disk width, that task is wrong.
- **Declared-scalar capability must survive.** `columns.arrow` already stores scalars as one Arrow column each (`crates/tessera-store/src/write.rs:86`). The refactor replaces a row-major staging form with a columnar one; it must not remove the ability to carry `U64`, `F32` or `Utf8` scalars.
- **British spelling** in comments and documentation.
- Test command for a single crate: `cargo test -p <crate>`. Whole workspace: `cargo test --workspace`.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `crates/tessera-build/src/columns.rs` | **New.** `TermCsr` (offsets + flat term IDs) and `StagedColumns` (SoA staging), with their invariants and constructors |
| `crates/tessera-build/src/input.rs` | Modify. `read_pairs` returns `TermCsr` keyed by dense point index instead of `HashMap<u64, Vec<u64>>` |
| `crates/tessera-build/src/lib.rs` | Modify. Pipeline consumes the new types; `digest_file` streams |
| `crates/tessera-spatial/src/tiler.rs` | Modify. `ItemColumns` + `ScalarColumn` replace `TilerItem`; `sort_batch` permutes in place |
| `crates/tessera-store/src/write.rs` | Modify. `write_segment` takes `&ItemColumns`; scalar columns are moved, not transposed |
| `crates/tessera-build/tests/golden_bundle.rs` | **New.** Byte-identity harness — the safety net for every task |
| `crates/tessera-build/tests/golden/bundle-250k.json` | **New.** Committed per-file SHA-256 map of the reference bundle |

---

### Task 1: Golden-bundle byte-identity harness

Nothing else in this plan is safe without this. It must be written and committed against the **unmodified** build.

**Files:**
- Create: `crates/tessera-build/tests/golden_bundle.rs`
- Create: `crates/tessera-build/tests/golden/bundle-250k.json` (generated, then committed)

**Interfaces:**
- Consumes: `tessera_build::{build, BuildArgs}`, `tessera_spatial::Extent`
- Produces: `golden_bundle::hash_bundle(root: &Path) -> BTreeMap<String, String>` — bundle-relative path → hex SHA-256, for every regular file under `root`. Later tasks re-run this test unchanged.

- [ ] **Step 1: Write the harness and the blessing path**

Create `crates/tessera-build/tests/golden_bundle.rs`:

```rust
//! Byte-identity harness: the build's output must not change while its memory
//! representation does. Entity-ID assignment is permanent under I9, so a hash
//! mismatch here means the corpus ordering moved — a task failure, never a
//! reason to re-bless the fixture.
//!
//! Bless once, against the unmodified build:
//!     TESSERA_BLESS_GOLDEN=1 cargo test -p tessera-build --test golden_bundle
//! Thereafter the test compares and fails on any difference.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tessera_build::{build, BuildArgs};
use tessera_spatial::Extent;

/// The Phase 0 scaled corpus. Absent on a machine without it — the test skips
/// rather than fails, so `cargo test --workspace` still passes on a fresh clone.
fn corpus() -> Option<(PathBuf, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let points = root.join("data/scaled/geometry.parquet");
    let pairs = root.join("data/scaled/pairs/categories-subclass.pairs.parquet");
    (points.exists() && pairs.exists()).then_some((points, pairs))
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/bundle-250k.json")
}

/// Every regular file under `root`, as bundle-relative forward-slash path -> hex SHA-256.
pub fn hash_bundle(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .expect("inside root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let bytes = fs::read(&path).expect("read file");
            out.insert(rel, format!("{:x}", Sha256::digest(&bytes)));
        }
    }
    out
}

#[test]
fn bundle_bytes_are_unchanged_at_250k() {
    let Some((points, pairs)) = corpus() else {
        eprintln!("skipping: data/scaled corpus not present");
        return;
    };
    let out = tempfile::tempdir().expect("tempdir");
    let root = out.path().join("bundle");

    build(&BuildArgs {
        points,
        pairs,
        out: root.clone(),
        extent: Extent { x_min: 0.0, x_max: 65536.0, y_min: 0.0, y_max: 65536.0 },
        slice_id: "s0".to_string(),
        limit: Some(250_000),
    })
    .expect("build succeeds");

    let mut actual = hash_bundle(&root);
    // MANIFEST.json embeds `created_at`, so it differs run to run by design.
    // CURRENT carries that manifest's digest and differs with it.
    actual.remove("v00000/MANIFEST.json");
    actual.remove("CURRENT");

    if std::env::var_os("TESSERA_BLESS_GOLDEN").is_some() {
        fs::create_dir_all(golden_path().parent().expect("parent")).expect("mkdir");
        fs::write(
            golden_path(),
            serde_json::to_vec_pretty(&actual).expect("serialise"),
        )
        .expect("write golden");
        eprintln!("blessed {} files", actual.len());
        return;
    }

    let expected: BTreeMap<String, String> = serde_json::from_slice(
        &fs::read(golden_path()).expect("golden file missing — bless it first"),
    )
    .expect("golden is valid JSON");

    assert_eq!(
        expected, actual,
        "bundle bytes changed — entity-ID assignment is permanent under I9; \
         investigate rather than re-blessing"
    );
}
```

- [ ] **Step 2: Add the dev-dependencies the harness needs**

In `crates/tessera-build/Cargo.toml`, under `[dev-dependencies]`, add `tempfile = "3"` and `serde_json = "1"` if either is absent (`sha2` is already a normal dependency of this crate).

- [ ] **Step 3: Bless the fixture against the unmodified build**

Run: `TESSERA_BLESS_GOLDEN=1 cargo test -p tessera-build --test golden_bundle -- --nocapture`
Expected: prints `blessed N files`, writes `crates/tessera-build/tests/golden/bundle-250k.json`.

- [ ] **Step 4: Verify it now compares and passes**

Run: `cargo test -p tessera-build --test golden_bundle`
Expected: PASS.

- [ ] **Step 5: Verify it actually catches a change**

Temporarily edit `crates/tessera-build/src/lib.rs:230` — change the staging sort's tiebreak from `a.source_id.cmp(&b.source_id)` to `b.source_id.cmp(&a.source_id)`. Run the test.
Expected: FAIL with the I9 message. **Revert the edit** and re-run to confirm PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/tessera-build/tests/golden_bundle.rs \
        crates/tessera-build/tests/golden/bundle-250k.json \
        crates/tessera-build/Cargo.toml
git commit -m "test(build): golden byte-identity harness for the 250k bundle"
```

---

### Task 2: Stream `digest_file` instead of reading whole files

`digest_file` calls `fs::read`, which allocates a buffer the size of the file. `columns.arrow` is ~22 GB at 10⁹.

**Files:**
- Modify: `crates/tessera-build/src/lib.rs:614-620`
- Test: `crates/tessera-build/tests/build_smoke.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `digest_file(path: &Path) -> Result<FileDigest>` — unchanged signature, unchanged output.

- [ ] **Step 1: Write the failing test**

Append to `crates/tessera-build/tests/build_smoke.rs`:

```rust
#[test]
fn digest_of_a_file_larger_than_one_chunk_matches_a_whole_file_hash() {
    use sha2::{Digest, Sha256};
    // Deliberately not a multiple of the streaming chunk size, so a boundary
    // bug shows up rather than cancelling out.
    let bytes: Vec<u8> = (0..(1 << 20) + 12345u32).map(|i| (i % 251) as u8).collect();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("blob.bin");
    std::fs::write(&path, &bytes).expect("write");

    let digest = tessera_build::digest_file_for_test(&path).expect("digest");
    assert_eq!(digest.size, bytes.len() as u64);
    assert_eq!(digest.sha256, format!("{:x}", Sha256::digest(&bytes)));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tessera-build --test build_smoke digest_of_a_file`
Expected: FAIL — `digest_file_for_test` does not exist.

- [ ] **Step 3: Implement streaming and the test hook**

In `crates/tessera-build/src/lib.rs`, replace `digest_file`:

```rust
/// SHA-256 and size of a file, hashed in fixed-size chunks.
///
/// Reading the whole file first would allocate a buffer the size of the file — at 10^9 items
/// `columns.arrow` is ~22 GB, so the digest step alone would dominate the build's peak RSS.
fn digest_file(path: &Path) -> Result<FileDigest> {
    use std::io::Read;

    const CHUNK: usize = 1 << 20;
    let mut file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| BuildError::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(64);
    {
        use std::fmt::Write;
        for byte in digest {
            let _ = write!(sha256, "{byte:02x}");
        }
    }
    Ok(FileDigest { size, sha256 })
}

/// Test-only re-export: the digest helper is private, and the streaming boundary is exactly
/// what wants a direct test.
#[doc(hidden)]
pub fn digest_file_for_test(path: &Path) -> Result<FileDigest> {
    digest_file(path)
}
```

Add `pub use tessera_store::manifest::FileDigest;` to the crate's re-exports if `FileDigest` is not already nameable from `tessera_build`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tessera-build`
Expected: PASS, including `bundle_bytes_are_unchanged_at_250k`.

- [ ] **Step 5: Commit**

```bash
git add crates/tessera-build/src/lib.rs crates/tessera-build/tests/build_smoke.rs
git commit -m "perf(build): stream file digests instead of reading whole files"
```

---

### Task 3: `TermCsr` — flat term storage replacing the per-entity `HashMap`

The single largest win. `HashMap<u64, Vec<u64>>` at 10⁹ entries costs ~71 GB of table (2³¹ buckets × 33 B) plus ~32 GB in 10⁹ separate small allocations. The same data as CSR is 4 GB of offsets plus 6.9 GB of values.

**Files:**
- Create: `crates/tessera-build/src/columns.rs`
- Modify: `crates/tessera-build/src/input.rs:168-200`
- Modify: `crates/tessera-build/src/lib.rs:22-23` (module declaration), `:177`, `:188-220`
- Test: `crates/tessera-build/tests/build_smoke.rs`

**Interfaces:**
- Consumes: `PointRow` from `input`.
- Produces:
  - `tessera_build::columns::TermCsr` with `fn len(&self) -> usize`, `fn terms(&self, i: usize) -> &[u32]`, `fn total(&self) -> usize`.
  - `input::read_pairs(path: &Path, source_ids: &[u64], limit: Option<u64>) -> Result<TermCsr>` — **signature changed**: it now takes the ascending source IDs of the selected points and returns one CSR row per point, in the same order.

- [ ] **Step 1: Write the failing test**

Create the test module inside the new file's own unit tests — append to `crates/tessera-build/tests/build_smoke.rs`:

```rust
#[test]
fn term_csr_groups_sorts_and_deduplicates_per_row() {
    use tessera_build::columns::TermCsrBuilder;
    // Rows 0,1,2. Terms arrive unsorted, with a duplicate in row 0.
    // Pass 1 counts, pass 2 places — the same order both times.
    let pairs = [(0usize, 7u32), (2, 4), (0, 3), (0, 7), (1, 9)];

    let mut builder = TermCsrBuilder::new(3);
    for (row, _) in pairs {
        builder.count(row);
    }
    let mut builder = builder.prepare().expect("within the u32 offset ceiling");
    for (row, term) in pairs {
        builder.put(row, term);
    }
    let csr = builder.finish();

    assert_eq!(csr.len(), 3);
    assert_eq!(csr.terms(0), &[3, 7]);
    assert_eq!(csr.terms(1), &[9]);
    assert_eq!(csr.terms(2), &[4]);
    assert_eq!(csr.total(), 4);
}

#[test]
fn term_csr_handles_rows_with_no_terms() {
    use tessera_build::columns::TermCsrBuilder;
    let mut builder = TermCsrBuilder::new(2);
    builder.count(1);
    let mut builder = builder.prepare().expect("prepare");
    builder.put(1, 5);
    let csr = builder.finish();
    assert_eq!(csr.terms(0), &[] as &[u32]);
    assert_eq!(csr.terms(1), &[5]);
}

#[test]
fn term_csr_rejects_more_pairs_than_a_u32_offset_can_address() {
    use tessera_build::columns::TermCsrBuilder;
    let mut builder = TermCsrBuilder::new(1);
    builder.count_many(0, u32::MAX as u64 + 1);
    assert!(builder.prepare().is_err());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tessera-build --test build_smoke term_csr`
Expected: FAIL — `tessera_build::columns` does not exist.

- [ ] **Step 3: Implement `TermCsr`**

Create `crates/tessera-build/src/columns.rs`:

```rust
//! Flat column representations for the batch build.
//!
//! The build used to hold one `Vec` per item and a `HashMap` keyed by entity ID. At 10^9 items
//! that is ~175 GB of container for ~7 GB of term IDs — 10^9 individual heap allocations of
//! roughly seven bytes each, twice over. Compressed sparse row replaces both: one offsets array,
//! one values array, no per-item allocation.

/// Term IDs grouped by row, as compressed sparse row. Read-only; build one with
/// [`TermCsrBuilder`].
#[derive(Debug, Clone, Default)]
pub struct TermCsr {
    offsets: Vec<u32>,
    values: Vec<u32>,
}

impl TermCsr {
    /// Number of rows.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when there are no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `i`'s sorted, deduplicated term IDs.
    pub fn terms(&self, i: usize) -> &[u32] {
        let lo = self.offsets[i] as usize;
        let hi = self.offsets[i + 1] as usize;
        &self.values[lo..hi]
    }

    /// Total term IDs across all rows.
    pub fn total(&self) -> usize {
        self.values.len()
    }
}

/// CSR construction for producers that visit destination rows **in order**, appending each row's
/// terms once. That is the natural shape for the staging loop and for permuting an existing CSR,
/// and it needs neither a counting pass nor a staging buffer: offsets fall out of the running
/// length. Rows are stored exactly as given, so the caller owns sorting and deduplication —
/// [`crate::signature_sort_key`] already does both.
#[derive(Debug, Clone, Default)]
pub struct TermCsrAppender {
    offsets: Vec<u32>,
    values: Vec<u32>,
}

impl TermCsrAppender {
    /// An empty appender, sized for `rows_hint` rows.
    pub fn new(rows_hint: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows_hint + 1);
        offsets.push(0u32);
        Self { offsets, values: Vec::new() }
    }

    /// Append the next row's terms.
    pub fn push_row(&mut self, terms: &[u32]) {
        self.values.extend_from_slice(terms);
        self.offsets.push(self.values.len() as u32);
    }

    /// Finish. Errors when the total exceeds what a `u32` offset can address.
    pub fn finish(self) -> std::result::Result<TermCsr, String> {
        if self.values.len() as u64 > u32::MAX as u64 {
            return Err(format!(
                "TermCsr: {} pairs exceeds the u32 offset ceiling ({})",
                self.values.len(),
                u32::MAX
            ));
        }
        Ok(TermCsr { offsets: self.offsets, values: self.values })
    }
}

/// Two-pass CSR construction for producers that visit destination rows in **arbitrary** order —
/// `read_pairs`, whose input is grouped by term rather than by entity. **Count** every
/// `(row, term)`, [`prepare`](TermCsrBuilder::prepare), then **put** the same pairs in the same
/// order.
///
/// Two passes rather than one because staging the pairs to count them costs 8 bytes each — at
/// 1.72e9 pairs that is 13.8 GB held only to be thrown away. The caller re-reads its source
/// instead; for a 1.6 GB compressed Parquet file that is far cheaper than the staging buffer.
///
/// The fill uses the offsets array as its own cursor and repairs it afterwards, so there is no
/// separate cursor allocation: peak is one offsets array plus one values array.
#[derive(Debug, Clone)]
pub struct TermCsrBuilder {
    /// Counts during the counting pass; offsets-then-cursor during the fill.
    offsets: Vec<u32>,
    values: Vec<u32>,
    filling: bool,
}

impl TermCsrBuilder {
    /// A builder for `rows` rows, in the counting phase.
    pub fn new(rows: usize) -> Self {
        Self {
            offsets: vec![0u32; rows + 1],
            values: Vec::new(),
            filling: false,
        }
    }

    /// Counting pass: record that `row` carries one more term.
    pub fn count(&mut self, row: usize) {
        self.offsets[row] += 1;
    }

    /// Counting pass, `n` terms at once. Saturates rather than wrapping so an over-count is
    /// caught by [`prepare`] instead of silently truncating a row.
    pub fn count_many(&mut self, row: usize, n: u64) {
        let slot = &mut self.offsets[row];
        *slot = slot.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
    }

    /// End the counting pass: exclusive prefix-sum the counts into start offsets and allocate the
    /// values array. Errors when the total exceeds what a `u32` offset can address.
    pub fn prepare(mut self) -> std::result::Result<Self, String> {
        let rows = self.offsets.len() - 1;
        let total: u64 = self.offsets[..rows].iter().map(|c| *c as u64).sum();
        if total > u32::MAX as u64 {
            return Err(format!(
                "TermCsr: {total} pairs exceeds the u32 offset ceiling ({})",
                u32::MAX
            ));
        }
        let mut running = 0u32;
        for slot in self.offsets[..rows].iter_mut() {
            let count = *slot;
            *slot = running;
            running += count;
        }
        self.offsets[rows] = running;
        self.values = vec![0u32; total as usize];
        self.filling = true;
        Ok(self)
    }

    /// Fill pass: place `term` in `row`. The caller must present exactly the pairs it counted;
    /// presenting more overflows the row and is a caller bug.
    pub fn put(&mut self, row: usize, term: u32) {
        debug_assert!(self.filling, "put before prepare");
        let slot = self.offsets[row] as usize;
        self.values[slot] = term;
        self.offsets[row] += 1;
    }

    /// Finish: repair the offsets the fill advanced, then sort, deduplicate and compact each row.
    ///
    /// The label set is a *set* — the signature key and the postings writer both depend on that,
    /// so deduplication is a correctness step, not a tidy-up.
    pub fn finish(mut self) -> TermCsr {
        let rows = self.offsets.len() - 1;
        if rows == 0 {
            return TermCsr { offsets: vec![0], values: Vec::new() };
        }
        // After the fill, `offsets[row]` is that row's *end*. Shifting right by one and zeroing
        // the head restores start offsets — the classic counting-sort repair, and it is why no
        // separate cursor array was needed.
        let end_of_last = self.offsets[rows - 1];
        for row in (1..rows).rev() {
            self.offsets[row] = self.offsets[row - 1];
        }
        self.offsets[0] = 0;
        self.offsets[rows] = end_of_last;

        let mut compact = Vec::with_capacity(rows + 1);
        compact.push(0u32);
        let mut write = 0usize;
        for row in 0..rows {
            let lo = self.offsets[row] as usize;
            let hi = self.offsets[row + 1] as usize;
            let slice = &mut self.values[lo..hi];
            slice.sort_unstable();
            let mut kept = 0usize;
            for i in 0..slice.len() {
                if kept == 0 || slice[i] != slice[kept - 1] {
                    slice[kept] = slice[i];
                    kept += 1;
                }
            }
            self.values.copy_within(lo..lo + kept, write);
            write += kept;
            compact.push(write as u32);
        }
        self.values.truncate(write);
        self.values.shrink_to_fit();

        TermCsr {
            offsets: compact,
            values: self.values,
        }
    }
}
```

In `crates/tessera-build/src/lib.rs`, add `pub mod columns;` beside `pub mod error;` and `pub mod input;`.

- [ ] **Step 4: Run the CSR tests**

Run: `cargo test -p tessera-build --test build_smoke term_csr`
Expected: PASS (both).

- [ ] **Step 5: Change `read_pairs` to return `TermCsr`**

In `crates/tessera-build/src/input.rs`, replace `read_pairs` and drop the now-unused `use std::collections::HashMap;`:

```rust
/// Read `pairs` (`entity_id`, `term_id`), keeping rows with `entity_id < limit`, grouped by the
/// **position** of each source entity in `source_ids` — which must be the ascending source IDs of
/// the selected points. Returns one CSR row per point, in that same order.
///
/// Keying on position rather than on the entity ID itself is what removes the `HashMap`: at 10^9
/// items its table alone was ~71 GB before a single term was stored. `source_ids` is already
/// sorted by the caller, so the lookup is a binary search — or a direct index when the IDs are
/// dense, which the Phase 0 corpus is.
pub fn read_pairs(path: &Path, source_ids: &[u64], limit: Option<u64>) -> Result<TermCsr> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let id_idx = column_index(path, &schema, "entity_id")?;
    let term_idx = column_index(path, &schema, "term_id")?;

    // Dense fast path: source IDs are exactly 0..n, so position == id and the search is a
    // bounds check. The Phase 0 corpus assigns dense 0-based IDs (dataset §4.1).
    let dense = source_ids
        .first()
        .is_some_and(|first| *first == 0 && source_ids.last() == Some(&(source_ids.len() as u64 - 1)));

    let keep = prunable_row_groups(builder.metadata(), id_idx, limit);
    let reader = builder
        .with_row_groups(keep)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;

    // Two passes over the file: count, then place. Staging the pairs to count them in one pass
    // would cost 8 bytes each — 13.8 GB at the 10^9 corpus's 1.72e9 pairs, held only to be
    // discarded. Re-decoding a 1.6 GB Parquet file is much cheaper than that buffer.
    let mut builder = TermCsrBuilder::new(source_ids.len());
    for_each_pair(path, id_idx, term_idx, limit, source_ids, dense, |row, _term| {
        builder.count(row);
        Ok(())
    })?;
    let mut builder = builder.prepare().map_err(BuildError::Invalid)?;
    for_each_pair(path, id_idx, term_idx, limit, source_ids, dense, |row, term| {
        builder.put(row, term);
        Ok(())
    })?;
    Ok(builder.finish())
}

/// Stream `(row, term)` from the pairs file, resolving each source entity ID to its position in
/// `source_ids`. Both of [`read_pairs`]'s passes go through here, so they cannot drift: a pair
/// counted in pass one is placed in pass two, in the same order.
#[allow(clippy::too_many_arguments)]
fn for_each_pair(
    path: &Path,
    id_idx: usize,
    term_idx: usize,
    limit: Option<u64>,
    source_ids: &[u64],
    dense: bool,
    mut visit: impl FnMut(usize, u32) -> Result<()>,
) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let keep = prunable_row_groups(builder.metadata(), id_idx, limit);
    let reader = builder
        .with_row_groups(keep)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;

    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, "entity_id")?;
        let terms = read_u64_column(path, &batch, term_idx, "term_id")?;
        for i in 0..batch.num_rows() {
            if limit.is_some_and(|l| ids[i] >= l) {
                continue;
            }
            let row = if dense {
                usize::try_from(ids[i]).ok().filter(|r| *r < source_ids.len())
            } else {
                source_ids.binary_search(&ids[i]).ok()
            };
            let Some(row) = row else {
                return Err(BuildError::Invalid(format!(
                    "pairs file references entity id {} absent from the points file",
                    ids[i]
                )));
            };
            let term = u32::try_from(terms[i]).map_err(|_| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!("term id {} does not fit in u32", terms[i]),
            })?;
            visit(row, term)?;
        }
    }
    Ok(())
}
```

Add `use crate::columns::{TermCsr, TermCsrBuilder};` to the file's imports. Delete the now-unused `schema`/`reader` setup left above the two passes in `read_pairs` — it keeps only the `File::open`, `id_idx`, `term_idx` and `dense` derivation, since `for_each_pair` opens the file itself.

- [ ] **Step 6: Update the pipeline to consume it**

In `crates/tessera-build/src/lib.rs`, replace lines 177 and the staging loop at 188-220:

```rust
    let source_ids: Vec<u64> = points.iter().map(|p| p.source_id).collect();
    let term_csr = input::read_pairs(&args.pairs, &source_ids, args.limit)?;
    drop(source_ids);
```

and the loop body, replacing `pairs_by_source.remove(...)` with the CSR row and deleting the trailing `if !pairs_by_source.is_empty()` block (the absent-entity check now lives in `read_pairs`):

```rust
    let mut staged: Vec<StagedItem> = Vec::with_capacity(points.len());
    let mut over_bound_items = 0u64;
    for (row, point) in points.iter().enumerate() {
        let source_terms = term_csr.terms(row);
        // The Phase 0 corpus carries integer term IDs; the item's `access` label is the
        // comma-joined decimal source term IDs, so `builtin:passthrough` yields decimal-string
        // descriptors (R6).
        let mut access = String::new();
        for (i, t) in source_terms.iter().enumerate() {
            if i > 0 {
                access.push(',');
            }
            access.push_str(&t.to_string());
        }
        let descriptors = plugin.terms_of_label(access.as_bytes())?;
        if descriptors.len() > bounds.max_terms_per_item as usize {
            // A declared bound is a *declaration*: record it and carry on. Dropping terms here
            // would silently widen the item's visibility (I2/I3).
            over_bound_items += 1;
        }
        let terms: Vec<TermId> = descriptors.iter().map(|d| dict.intern(d)).collect();
        staged.push(StagedItem {
            source_id: point.source_id,
            x: point.x,
            y: point.y,
            signature: signature_sort_key(&terms),
        });
    }
    drop(term_csr);
```

> **Ordering note, load-bearing:** `read_pairs` previously produced `Vec<u64>` sorted ascending as *numbers*; the `access` string is built from that order and interning assigns term IDs in first-appearance order. `TermCsrBuilder::finish` sorts ascending as `u32`, which is the same order for the same values. The golden test in Step 8 is what proves it.

- [ ] **Step 7: Run the unit and smoke tests**

Run: `cargo test -p tessera-build`
Expected: PASS.

- [ ] **Step 8: Run the golden test — this is the gate**

Run: `cargo test -p tessera-build --test golden_bundle`
Expected: PASS. A failure means the term ordering moved; fix the ordering, do not re-bless.

- [ ] **Step 9: Commit**

```bash
git add crates/tessera-build/src/columns.rs crates/tessera-build/src/input.rs \
        crates/tessera-build/src/lib.rs crates/tessera-build/tests/build_smoke.rs
git commit -m "perf(build): flat CSR term storage, replacing the per-entity HashMap"
```

---

### Task 4: `StagedColumns` — struct-of-arrays staging

`StagedItem` is 40 bytes inline plus one heap allocation per item for `signature`. At 10⁹ that is ~72 GB. As SoA with a shared CSR it is ~24 GB.

**Files:**
- Modify: `crates/tessera-build/src/columns.rs`
- Modify: `crates/tessera-build/src/lib.rs:119-129` (delete `StagedItem`), `:186-260`, `:553-582`
- Test: `crates/tessera-build/tests/build_smoke.rs`

**Interfaces:**
- Consumes: `TermCsr` from Task 3.
- Produces: `columns::StagedColumns` with public fields `source_id: Vec<u64>`, `x: Vec<f32>`, `y: Vec<f32>`, `signatures: TermCsr`, and `fn len(&self) -> usize`, `fn sort_by_signature(&mut self)`.

- [ ] **Step 1: Write the failing test**

Append to `crates/tessera-build/tests/build_smoke.rs`:

```rust
#[test]
fn staged_columns_sort_by_signature_then_source_id() {
    use tessera_build::columns::{StagedColumns, TermCsrAppender};
    // Three items: two share signature [1], one has [0]. Within the shared signature the
    // tiebreak is the source id, ascending (§11.1).
    let mut sig = TermCsrAppender::new(3);
    sig.push_row(&[1]);
    sig.push_row(&[0]);
    sig.push_row(&[1]);
    let mut staged = StagedColumns {
        source_id: vec![50, 60, 40],
        x: vec![1.0, 2.0, 3.0],
        y: vec![4.0, 5.0, 6.0],
        signatures: sig.finish().expect("within ceiling"),
    };
    staged.sort_by_signature();

    // [0] sorts before [1]; then source ids 40 < 50 within signature [1].
    assert_eq!(staged.source_id, vec![60, 40, 50]);
    assert_eq!(staged.x, vec![2.0, 3.0, 1.0]);
    assert_eq!(staged.y, vec![5.0, 6.0, 4.0]);
    assert_eq!(staged.signatures.terms(0), &[0]);
    assert_eq!(staged.signatures.terms(1), &[1]);
    assert_eq!(staged.signatures.terms(2), &[1]);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tessera-build --test build_smoke staged_columns`
Expected: FAIL — `StagedColumns` does not exist.

- [ ] **Step 3: Implement `StagedColumns`**

Append to `crates/tessera-build/src/columns.rs`:

```rust
/// Staged items before entity-ID assignment, as struct-of-arrays.
///
/// Replaces a `Vec<StagedItem>` whose every element owned a `Vec<u32>` signature — 40 bytes
/// inline plus one heap allocation per item, ~72 GB at 10^9. The signature lives in a shared
/// [`TermCsr`] instead.
#[derive(Debug, Clone, Default)]
pub struct StagedColumns {
    pub source_id: Vec<u64>,
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub signatures: TermCsr,
}

impl StagedColumns {
    /// Number of staged items.
    pub fn len(&self) -> usize {
        self.source_id.len()
    }

    /// True when nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.source_id.is_empty()
    }

    /// Sort into signature-sorted assignment order (§11.1): by the item's sorted term-ID list
    /// lexicographically, ties broken by source ID. **Permanent under I9** — the ordering chosen
    /// here is the entity-ID ordering the corpus keeps forever.
    ///
    /// Sorts a permutation of indices and applies it, rather than sorting the rows: the rows are
    /// four parallel arrays plus a CSR, and moving an index is 8 bytes against ~24.
    pub fn sort_by_signature(&mut self) {
        let n = self.len();
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_by(|&a, &b| {
            self.signatures
                .terms(a as usize)
                .cmp(self.signatures.terms(b as usize))
                .then(self.source_id[a as usize].cmp(&self.source_id[b as usize]))
        });

        // `mem::take` first: reading `self.source_id` inside a closure that assigns to
        // `self.source_id` is a borrow conflict, and cloning to dodge it would double the peak.
        let old_source_id = std::mem::take(&mut self.source_id);
        self.source_id = order.iter().map(|&i| old_source_id[i as usize]).collect();
        drop(old_source_id);
        let old_x = std::mem::take(&mut self.x);
        self.x = order.iter().map(|&i| old_x[i as usize]).collect();
        drop(old_x);
        let old_y = std::mem::take(&mut self.y);
        self.y = order.iter().map(|&i| old_y[i as usize]).collect();
        drop(old_y);

        // Destination rows are visited in order, so an appender suffices — no counting pass, and
        // each row's terms are already sorted and deduplicated by construction.
        let mut permuted = TermCsrAppender::new(n);
        for &old in &order {
            permuted.push_row(self.signatures.terms(old as usize));
        }
        self.signatures = permuted
            .finish()
            .expect("permuting cannot grow the pair count past a ceiling the source cleared");
    }
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p tessera-build --test build_smoke staged_columns`
Expected: PASS.

- [ ] **Step 5: Replace `StagedItem` in the pipeline**

In `crates/tessera-build/src/lib.rs`: delete the `StagedItem` struct (lines 119-129). Replace the staging loop's accumulator and the sort:

```rust
    let mut source_id = Vec::with_capacity(points.len());
    let mut xs = Vec::with_capacity(points.len());
    let mut ys = Vec::with_capacity(points.len());
    // The loop visits rows 0..n in order, so appending each signature *is* the CSR fill — no
    // counting pass, and no second call into the plugin (which at 1-10 us per item would cost
    // hours at 10^9).
    let mut signatures = columns::TermCsrAppender::new(points.len());
    let mut over_bound_items = 0u64;
    for (row, point) in points.iter().enumerate() {
        // ...access string and descriptors exactly as in Task 3...
        let terms: Vec<TermId> = descriptors.iter().map(|d| dict.intern(d)).collect();
        source_id.push(point.source_id);
        xs.push(point.x);
        ys.push(point.y);
        signatures.push_row(&signature_sort_key(&terms));
        let _ = row;
    }
    drop(term_csr);
    let mut staged = columns::StagedColumns {
        source_id,
        x: xs,
        y: ys,
        signatures: signatures.finish().map_err(BuildError::Invalid)?,
    };
```

Replace the sort at lines 230-234 with `staged.sort_by_signature();` and `let n = staged.len() as u64;`.

Replace the postings loop at 253-260:

```rust
    for position in 0..staged.len() {
        let new_id = position as u32;
        for &term in staged.signatures.terms(position) {
            per_term[term as usize].push(new_id);
            pair_count += 1;
        }
    }
```

Change `write_external_ids(path: &Path, staged: &[StagedItem])` to take `&StagedColumns` and iterate `staged.source_id`:

```rust
fn write_external_ids(path: &Path, staged: &columns::StagedColumns) -> Result<()> {
    let mut rows: Vec<([u8; 8], u64)> = staged
        .source_id
        .iter()
        .enumerate()
        .map(|(position, source_id)| (source_id.to_le_bytes(), position as u64))
        .collect();
    rows.sort_unstable_by_key(|(key, _)| *key);
    // ...remainder unchanged...
```

- [ ] **Step 6: Run the full crate tests**

Run: `cargo test -p tessera-build`
Expected: PASS.

- [ ] **Step 7: Run the golden test — the gate**

Run: `cargo test -p tessera-build --test golden_bundle`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/tessera-build/src/columns.rs crates/tessera-build/src/lib.rs \
        crates/tessera-build/tests/build_smoke.rs
git commit -m "perf(build): struct-of-arrays staging with shared signature CSR"
```

---

### Task 5: `ItemColumns` and `ScalarColumn` — columnar tiler input

`TilerItem` is 48 bytes per item, of which 24 is a `scalars: Vec<ScalarValue>` that Phase 1 never fills. `ScalarValue` is 32 bytes (the `String` variant sets the size), so a single declared `u64` scalar would cost ~88 bytes per item row-major against 8 bytes columnar. `columns.arrow` already stores scalars as one Arrow column each, so this deletes a transpose rather than adding one.

**Files:**
- Modify: `crates/tessera-spatial/src/tiler.rs`
- Modify: `crates/tessera-store/src/write.rs:29-180`
- Modify: `crates/tessera-build/src/lib.rs:289-311`
- Test: `crates/tessera-store/tests/segment_roundtrip.rs`

**Interfaces:**
- Consumes: `StagedColumns` from Task 4.
- Produces:
  - `tessera_spatial::tiler::ScalarColumn` — `U64(Vec<u64>)`, `F32(Vec<f32>)`, `Utf8 { offsets: Vec<u32>, bytes: Vec<u8> }`, with `fn len(&self) -> usize` and `fn permute(&self, order: &[u32]) -> ScalarColumn`.
  - `tessera_spatial::tiler::ItemColumns` with public fields `entity_id: Vec<EntityId>`, `x: Vec<f32>`, `y: Vec<f32>`, `node_id: Vec<u32>`, `priority: Vec<u16>`, `scalars: Vec<ScalarColumn>`, and `fn len(&self) -> usize`.
  - `tessera_store::write::write_segment(dir: &Path, items: &ItemColumns, codes: &[u32], scalar_schema: &[(String, ScalarType)]) -> io::Result<()>` — **signature changed** in its second parameter.

> `entity_id` stays `Vec<EntityId>` (8 bytes each). Narrowing to raw `u32` would save 4 GB and cost I4 its compile-time guarantee — the newtype distinction between `EntityId` and `RowId` is the whole point of plan §2.1. Not worth it.

- [ ] **Step 1: Write the failing test**

Append to `crates/tessera-store/tests/segment_roundtrip.rs`:

```rust
#[test]
fn segment_carries_declared_scalars_of_every_kind() {
    use tessera_spatial::tiler::{ItemColumns, ScalarColumn, ScalarType};
    use tessera_types::EntityId;

    let dir = tempfile::tempdir().expect("tempdir");
    let items = ItemColumns {
        entity_id: vec![EntityId::new(0), EntityId::new(1)],
        x: vec![1.0, 2.0],
        y: vec![3.0, 4.0],
        node_id: vec![7, 8],
        priority: vec![11, 22],
        scalars: vec![
            ScalarColumn::U64(vec![100, 200]),
            ScalarColumn::F32(vec![0.5, 1.5]),
            ScalarColumn::Utf8 {
                offsets: vec![0, 2, 5],
                bytes: b"ab".iter().chain(b"cde").copied().collect(),
            },
        ],
    };
    let schema = vec![
        ("count".to_string(), ScalarType::U64),
        ("score".to_string(), ScalarType::F32),
        ("tag".to_string(), ScalarType::Utf8),
    ];

    tessera_store::write::write_segment(dir.path(), &items, &[0, 1], &schema)
        .expect("write_segment");

    let bytes = std::fs::read(dir.path().join("columns.arrow")).expect("read");
    let reader = arrow::ipc::reader::FileReader::try_new(std::io::Cursor::new(bytes), None)
        .expect("arrow reader");
    let batch = reader.into_iter().next().expect("one batch").expect("batch ok");

    assert_eq!(batch.num_rows(), 2);
    assert_eq!(batch.schema().field(5).name(), "count");
    assert_eq!(batch.schema().field(6).name(), "score");
    assert_eq!(batch.schema().field(7).name(), "tag");

    let tags = batch
        .column(7)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .expect("utf8 column");
    assert_eq!(tags.value(0), "ab");
    assert_eq!(tags.value(1), "cde");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tessera-store --test segment_roundtrip segment_carries_declared_scalars`
Expected: FAIL — `ItemColumns` does not exist.

- [ ] **Step 3: Implement `ScalarColumn` and `ItemColumns`**

In `crates/tessera-spatial/src/tiler.rs`, replace `TilerItem` and `ScalarValue` with:

```rust
/// One declared-scalar column: every row's value for a single declared scalar.
///
/// Columnar because `columns.arrow` is columnar — a `Vec<ScalarValue>` per item cost 24 bytes of
/// header plus a heap allocation plus 32 bytes per value, and had to be transposed at write time
/// anyway. `Utf8` follows Arrow's own layout: `offsets` has `len + 1` entries and row `i` is
/// `bytes[offsets[i]..offsets[i + 1]]`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarColumn {
    U64(Vec<u64>),
    F32(Vec<f32>),
    Utf8 { offsets: Vec<u32>, bytes: Vec<u8> },
}

impl ScalarColumn {
    /// Number of rows this column carries.
    pub fn len(&self) -> usize {
        match self {
            ScalarColumn::U64(v) => v.len(),
            ScalarColumn::F32(v) => v.len(),
            ScalarColumn::Utf8 { offsets, .. } => offsets.len().saturating_sub(1),
        }
    }

    /// True when the column carries no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// This column with rows reordered so that new row `i` holds old row `order[i]`.
    pub fn permute(&self, order: &[u32]) -> ScalarColumn {
        match self {
            ScalarColumn::U64(v) => ScalarColumn::U64(order.iter().map(|&i| v[i as usize]).collect()),
            ScalarColumn::F32(v) => ScalarColumn::F32(order.iter().map(|&i| v[i as usize]).collect()),
            ScalarColumn::Utf8 { offsets, bytes } => {
                let mut new_offsets = Vec::with_capacity(order.len() + 1);
                let mut new_bytes = Vec::with_capacity(bytes.len());
                new_offsets.push(0u32);
                for &i in order {
                    let lo = offsets[i as usize] as usize;
                    let hi = offsets[i as usize + 1] as usize;
                    new_bytes.extend_from_slice(&bytes[lo..hi]);
                    new_offsets.push(new_bytes.len() as u32);
                }
                ScalarColumn::Utf8 { offsets: new_offsets, bytes: new_bytes }
            }
        }
    }
}

/// A batch of items to be placed into a segment, as struct-of-arrays: identity, geometry, node
/// attachment, priority (R3, computed by the caller), and one column per declared scalar.
///
/// `entity_id` stays `EntityId` rather than a raw `u32`: I4 is a compile-time property (plan
/// §2.1) and the four bytes saved per row are not worth demoting it to a discipline.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemColumns {
    pub entity_id: Vec<EntityId>,
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub node_id: Vec<u32>,
    pub priority: Vec<u16>,
    pub scalars: Vec<ScalarColumn>,
}

impl ItemColumns {
    /// Number of items.
    pub fn len(&self) -> usize {
        self.entity_id.len()
    }

    /// True when the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.entity_id.is_empty()
    }
}
```

Keep `ScalarType` exactly as it is.

- [ ] **Step 4: Rewrite `write_segment` against `ItemColumns`**

In `crates/tessera-store/src/write.rs`, change `write_segment` and `write_columns_arrow` to take `items: &ItemColumns`, build the five fixed columns from the parallel vectors, and replace `build_scalar_column` with a direct conversion:

```rust
/// Build one declared-scalar Arrow column from the staged column at `idx`, checking the staged
/// kind matches the declared `ty` (a mismatch is a caller bug — fail closed).
fn scalar_column_to_arrow(
    column: &ScalarColumn,
    ty: ScalarType,
    name: &str,
    rows: usize,
) -> io::Result<ArrayRef> {
    if column.len() != rows {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "write_segment: scalar '{name}' has {} rows, segment has {rows}",
                column.len()
            ),
        ));
    }
    match (ty, column) {
        (ScalarType::U64, ScalarColumn::U64(v)) => Ok(Arc::new(UInt64Array::from(v.clone()))),
        (ScalarType::F32, ScalarColumn::F32(v)) => Ok(Arc::new(Float32Array::from(v.clone()))),
        (ScalarType::Utf8, ScalarColumn::Utf8 { offsets, bytes }) => {
            let mut values: Vec<&str> = Vec::with_capacity(rows);
            for i in 0..rows {
                let lo = offsets[i] as usize;
                let hi = offsets[i + 1] as usize;
                values.push(std::str::from_utf8(&bytes[lo..hi]).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("write_segment: scalar '{name}' row {i} is not UTF-8: {e}"),
                    )
                })?);
            }
            Ok(Arc::new(StringArray::from(values)))
        }
        (declared, got) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write_segment: scalar '{name}' declared {declared:?}, staged {got:?}"),
        )),
    }
}
```

Delete `scalar_type_mismatch`, which no longer has a caller. Update the `use` list to import `ItemColumns` and `ScalarColumn` instead of `TilerItem` and `ScalarValue`.

- [ ] **Step 5: Update the build's construction site**

In `crates/tessera-build/src/lib.rs`, replace the `tiler_items` construction (lines 289-303):

```rust
    let mut item_columns = ItemColumns {
        entity_id: (0..staged.len()).map(|p| EntityId::new(p as u64)).collect(),
        x: std::mem::take(&mut staged.x),
        y: std::mem::take(&mut staged.y),
        node_id: vec![NODE_NONE; staged.len()],
        priority: (0..staged.len())
            .map(|p| priority_of(EntityId::new(p as u64)))
            .collect(),
        // Phase 1 declares no scalars; the columns are what a later phase fills.
        scalars: Vec::new(),
    };
```

and update the `write_segment` and `row_order` call sites to read from `item_columns`:

```rust
    let codes = sort_batch(&mut item_columns, &args.extent);
    write_segment(&segment_dir, &item_columns, &codes, &[])
        .map_err(|e| BuildError::io(&segment_dir, e))?;
    // ...
    let row_order: Vec<EntityId> = item_columns.entity_id.clone();
```

Import `ItemColumns` from `tessera_spatial::tiler`. `sort_batch`'s new signature lands in Task 6; until then this task's build will not compile against the old `sort_batch` — implement Task 6's Step 3 signature change here if you are running the two tasks together, or stub `sort_batch` to take `&mut ItemColumns` and keep its existing body operating on the parallel arrays.

- [ ] **Step 6: Run store and build tests**

Run: `cargo test -p tessera-store -p tessera-spatial -p tessera-build`
Expected: PASS, including the new scalar round-trip.

- [ ] **Step 7: Run the golden test — the gate**

Run: `cargo test -p tessera-build --test golden_bundle`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/tessera-spatial/src/tiler.rs crates/tessera-store/src/write.rs \
        crates/tessera-build/src/lib.rs crates/tessera-store/tests/segment_roundtrip.rs
git commit -m "perf(store,spatial): columnar ItemColumns and per-scalar columns"
```

---

### Task 6: In-place `sort_batch`

`sort_batch` currently allocates `order: Vec<usize>` (8 GB at 10⁹), then a full `sorted_items` clone (48 GB) and `sorted_codes` (8 GB) before `clone_from_slice`. Peak inside this one function was ~120 GB.

**Files:**
- Modify: `crates/tessera-spatial/src/tiler.rs:50-79`
- Test: `crates/tessera-spatial/src/tiler.rs` unit tests

**Interfaces:**
- Consumes: `ItemColumns` from Task 5.
- Produces: `sort_batch(items: &mut ItemColumns, extent: &Extent) -> Vec<u32>` — same contract (row order is `(morton, priority, entity_id)` ascending; returned codes align with the sorted items), new parameter type. The element type matches what `write_segment` already takes.

- [ ] **Step 1: Update the existing unit tests to the new type**

In `crates/tessera-spatial/src/tiler.rs`'s test module, replace the `item` helper and both tests:

```rust
    fn columns(rows: &[(u64, f32, f32, u16)]) -> ItemColumns {
        ItemColumns {
            entity_id: rows.iter().map(|r| EntityId::new(r.0)).collect(),
            x: rows.iter().map(|r| r.1).collect(),
            y: rows.iter().map(|r| r.2).collect(),
            node_id: vec![0; rows.len()],
            priority: rows.iter().map(|r| r.3).collect(),
            scalars: Vec::new(),
        }
    }

    #[test]
    fn sorts_by_morton_then_priority_then_entity_id() {
        let e = unit_extent();
        // Three items at the identical coordinate (same Morton code): must order by
        // (priority, entity_id) — the tiebreak is contract (contracts §2.6).
        let mut items = columns(&[(9, 0.5, 0.5, 5), (2, 0.5, 0.5, 5), (1, 0.5, 0.5, 1)]);
        let codes = sort_batch(&mut items, &e);
        assert_eq!(
            items.entity_id.iter().map(|e| e.raw()).collect::<Vec<_>>(),
            vec![1, 2, 9]
        );
        assert_eq!(codes.len(), 3);
        assert_eq!(codes[0], codes[1]);
        assert_eq!(codes[1], codes[2]);
    }

    #[test]
    fn returned_codes_are_non_decreasing() {
        let e = unit_extent();
        let mut items = columns(&[(1, 0.9, 0.9, 0), (2, 0.1, 0.1, 0), (3, 0.5, 0.5, 0)]);
        let codes = sort_batch(&mut items, &e);
        assert!(codes.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn scalar_columns_follow_the_row_permutation() {
        let e = unit_extent();
        let mut items = columns(&[(1, 0.9, 0.9, 0), (2, 0.1, 0.1, 0)]);
        items.scalars = vec![ScalarColumn::U64(vec![90, 10])];
        sort_batch(&mut items, &e);
        // Row (2, 0.1, 0.1) sorts first, so its scalar must come first too.
        assert_eq!(items.scalars[0], ScalarColumn::U64(vec![10, 90]));
    }
```

- [ ] **Step 2: Run to verify the new test fails**

Run: `cargo test -p tessera-spatial scalar_columns_follow`
Expected: FAIL — `sort_batch` does not take `ItemColumns`, or scalars are not permuted.

- [ ] **Step 3: Implement**

Replace `sort_batch`:

```rust
/// Sort `items` into segment (row) order: `(morton, priority, entity_id)` ascending
/// (contracts §2.6 — the priority tiebreak within equal Morton codes is contract, not
/// incidental).
///
/// Returns the sorted items' 32-bit Morton codes, in the same order as `items` post-sort —
/// matching `morton.u32`'s on-disk representation and `write_segment`'s parameter type.
///
/// Sorts a `u32` index permutation and applies it column by column. The previous form cloned the
/// whole item vector before overwriting it, which at 10^9 rows meant holding two copies of every
/// column at once.
pub fn sort_batch(items: &mut ItemColumns, extent: &Extent) -> Vec<u32> {
    let n = items.len();
    let raw: Vec<u32> = (0..n)
        .map(|i| morton_of(items.x[i] as f64, items.y[i] as f64, extent).raw())
        .collect();

    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (a as usize, b as usize);
        raw[a]
            .cmp(&raw[b])
            .then(items.priority[a].cmp(&items.priority[b]))
            .then(items.entity_id[a].raw().cmp(&items.entity_id[b].raw()))
    });

    // Each column is taken out before being rebuilt: reading a field inside a closure that
    // assigns to that same field is a borrow conflict, and one column at a time is also what
    // keeps the transient copy to one column's worth rather than the whole batch.
    macro_rules! permute_column {
        ($field:expr) => {{
            let old = std::mem::take(&mut $field);
            $field = order.iter().map(|&i| old[i as usize]).collect();
        }};
    }
    permute_column!(items.entity_id);
    permute_column!(items.x);
    permute_column!(items.y);
    permute_column!(items.node_id);
    permute_column!(items.priority);

    let old_scalars = std::mem::take(&mut items.scalars);
    items.scalars = old_scalars.iter().map(|c| c.permute(&order)).collect();

    order.iter().map(|&i| raw[i as usize]).collect()
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tessera-spatial`
Expected: PASS, all three.

- [ ] **Step 5: Run the golden test — the gate**

Run: `cargo test -p tessera-build --test golden_bundle`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/tessera-spatial/src/tiler.rs
git commit -m "perf(spatial): permute columns in place in sort_batch, u32 codes in memory"
```

---

### Task 7: Tighten lifetimes — free `points` and `staged` at last use

`points` (16 GB at 10⁹) currently lives to the end of `build()` although its last read is the staging loop. `staged` (~24 GB after Task 4) coexists with `item_columns` although `write_external_ids` is its last reader.

**Files:**
- Modify: `crates/tessera-build/src/lib.rs:161-320`

**Interfaces:**
- Consumes: everything from Tasks 3–6.
- Produces: no signature changes.

- [ ] **Step 1: Reorder so each large structure has a last use, then drop it**

In `build()`, make these three changes:

1. After the staging loop and `drop(term_csr);`, add `drop(points);`. The loop is `points`'s last reader — `staged.source_id`, `staged.x` and `staged.y` now carry everything downstream needs.
2. Move the `write_external_ids(&external_ids_path, &staged)?;` call to sit immediately **before** the `ItemColumns` construction, so `staged` is fully consumed before `item_columns` allocates.
3. Immediately after constructing `item_columns` (which already `mem::take`s `staged.x` and `staged.y`), add `drop(staged);`.

- [ ] **Step 2: Verify no borrow outlives its drop**

Run: `cargo build -p tessera-build`
Expected: compiles. If the borrow checker objects, the reorder is wrong — move the reader, do not clone to satisfy it.

- [ ] **Step 3: Run the crate tests**

Run: `cargo test -p tessera-build`
Expected: PASS.

- [ ] **Step 4: Run the golden test — the gate**

Run: `cargo test -p tessera-build --test golden_bundle`
Expected: PASS. The write order of `external-ids-0.arrow` moved relative to other files, but each file's own bytes are unchanged and `MANIFEST.files` is a sorted `BTreeMap`, so the manifest is unchanged too.

- [ ] **Step 5: Commit**

```bash
git add crates/tessera-build/src/lib.rs
git commit -m "perf(build): free points and staged columns at their last use"
```

---

### Task 8: Measure and record peak RSS

The plan's claim is a number. Verify it rather than asserting it.

**Files:**
- Create: `docs/archive/plans/bench-baselines/2026-07-29-build-memory.md`

**Interfaces:**
- Consumes: the completed Tasks 1–7.
- Produces: a committed record of peak RSS at three scales, before and after.

- [ ] **Step 1: Measure the pre-change baseline**

```bash
git stash list  # ensure clean
git checkout $(git merge-base HEAD master) -- crates/  # or note the pre-Task-1 SHA
cargo build --release -p tessera-cli
for limit in 250000 2422486 250000000; do
  /usr/bin/time -v ./target/release/tessera build \
    --points data/scaled/geometry.parquet \
    --pairs data/scaled/pairs/categories-subclass.pairs.parquet \
    --out /tmp/tessera-baseline-$limit --extent 0,65536,0,65536 --slice s0 \
    --limit $limit 2>&1 | grep -E "Maximum resident|Elapsed"
  rm -rf /tmp/tessera-baseline-$limit
done
```

Record each `Maximum resident set size`. **Expect the 250M run to fail or thrash** — that is the finding, not an error; record it as such and skip to Step 2.

- [ ] **Step 2: Restore the branch and measure after**

```bash
git checkout HEAD -- crates/
cargo build --release -p tessera-cli
# same loop, --out /tmp/tessera-after-$limit
```

- [ ] **Step 3: Write the record**

Create `docs/archive/plans/bench-baselines/2026-07-29-build-memory.md` with a table of scale, peak RSS before, peak RSS after, ratio, and elapsed time before/after — plus one line stating whether 250M now completes on a 39 GB box, and the extrapolated 10⁹ figure with the arithmetic shown.

- [ ] **Step 4: Commit**

```bash
git add docs/archive/plans/bench-baselines/2026-07-29-build-memory.md
git commit -m "docs: peak-RSS baselines for the Tier 1 build-memory work"
```

---

## What this plan does not do

Recorded so the next reader does not look for it.

**It does not make 10⁹ build on a 39 GB box.** Tier 1 targets ~50–60 GB. The remaining excess is that the pipeline's stages still coexist in memory: `item_columns` (~22 GB), the `order` permutation, `per_term`, and the Arrow arrays `write_columns_arrow` builds are all live at once. Getting under 39 GB needs Tier 2 — spilling intermediates between stages and external-sorting the Morton step, which is plan §12's named breakage (*"the Morton sort stops fitting in memory (external radix sort over disk-backed chunks)"*) arriving earlier than that document expects. Tier 2 is also what makes 10¹⁰ reachable at all.

**It does not change any on-disk width.** The segment writer already emits `morton.u32`; `columns.arrow`'s `uint64` entity IDs are untouched. Every task here is a memory-representation change, gated by Task 1 on producing identical bytes.

**It does not change `per_term: Vec<Vec<u32>>`.** At t≈1.72 and 47,968 terms it is ~7 GB of values in 47,968 vectors — large but not per-item, and its doubling-growth overshoot is bounded. It is the right next target if Tier 1 lands short.
