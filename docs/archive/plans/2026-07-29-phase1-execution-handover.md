> **ARCHIVED 2026-08-01 — SPENT. A handover note for an agent resuming Phase 1 at Task 10. Phase 1 is complete; the note has no remaining instruction value.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# Phase 1 execution handover — 2026-07-29

**For a fresh agent resuming the Phase 1 walking-skeleton build.** The previous
controller session executed Tasks 0–9 of the plan via
superpowers:subagent-driven-development and stopped after Task 9's completion.
Nothing is mid-flight: every completed task is committed, reviewed, and
recorded. Your job is Tasks 10–16 plus the final whole-branch review.

## How to resume (exact procedure)

1. Invoke `superpowers:subagent-driven-development` with the plan
   `docs/archive/plans/2026-07-28-phase1-walking-skeleton.md`.
2. The plan's SDD workspace already exists:
   `.superpowers/sdd/2026-07-28-phase1-walking-skeleton/`. Its `progress.md`
   ledger names this plan on line 1 — trust it over any recollection. Tasks
   with a `Task <N>: complete` line (0–9) are DONE; do not re-dispatch them.
   Resume at **Task 10**.
3. Task briefs are pre-sliced: `task-<N>-brief.md` (10–16 ready) plus
   `shared-context.md` (Global Constraints + the normative Reference Sheet
   R1–R6). Implementer reports live beside them as `task-<N>-report.md`.
4. Per task: record BASE (`git rev-parse HEAD`) → dispatch implementer
   (general-purpose subagent; give it the brief path, shared-context path,
   the Context block of interfaces below, and a report-file path) → generate
   the diff via the skill's `scripts/review-package PLAN BASE HEAD` → dispatch
   a task reviewer with brief+report+diff paths → fix loop (resume the same
   implementer via SendMessage for rounds 1–3) → scoped re-review per round →
   ledger + next task.
5. Model selection that worked: **sonnet** implementers (haiku for pure
   transcription tasks), **opus** reviewers for invariant-bearing tasks
   (6, 7, 9, 10, 11, 13, 15), sonnet for mechanical ones, sonnet/haiku for
   scoped re-reviews. Always set the model explicitly.
6. After Task 16: final whole-branch review (most capable model,
   superpowers:requesting-code-review's code-reviewer.md, diff from the
   repo's first scaffold commit `b159b35` to HEAD), pointing it at the
   ledger's `minor (deferred)` and note lines for triage. One fix wave max,
   one scoped re-review, then superpowers:finishing-a-development-branch.

## State of the world

- **Repo:** `/home/joe/code/tessera`, branch `master` (repo was created by
  Task 0; there is no remote). HEAD at handover: `b9c9ce2`. All workspace
  tests green; `cargo fmt --all`, `cargo clippy --workspace --all-targets --
  -D warnings`, `bash scripts/check-layers.sh` all clean at every completed
  task boundary.
- **Documents:** design corpus in `docs/design/` (search tools skip it — pass
  paths explicitly). Authority order: architecture design (r19) > contracts
  spec (r4) > system architecture (r4); lifecycle design r3 for WAL/overlay
  mechanisms. The plan cites all of these; if plan and spec disagree, STOP
  and report to Joe — do not resolve silently.
- **Untracked user files** (`docs/archive/whitepaper/`, `docs/tessera-*.html`,
  `docs/archive/reference/`, `docs/archive/plans/`, the white-paper plan): Joe's
  own work. Leave them alone; never `git add -A`.
- **Owner decisions already made** (do not re-open): pairs relation is
  `pairs.parquet` (contracts r4); priority = splitmix64-high-16 over the
  entity ID (contracts r4 §2.6); fragment-cache canonical key includes
  bundle identity + plugin hash (design r19 §2.3); wasmtime host deferred to
  Phase 2; ingest visibility deferred (no streaming flush in Phase 1); WAL
  stores descriptors not TermIds; serve-time deny publication deferred to
  Phase 2 as one package.

## What Tasks 0–9 delivered (interfaces Tasks 10–16 consume)

Commit trail is in the ledger; per-task detail in the task reports. The
load-bearing surfaces:

- `tessera-types`: `EntityId(u64)/RowId(u32)/TermId(u32)/Handle(u32)/
  Priority(u16)/MortonCode(u32)` with `new()`/`raw()` only (no conversions —
  I4); `SliceId(String)`, `SegId(String)`, `PinId { prefix: String,
  segments_version: u64 }`; constants `BUNDLE_FORMAT/API_VERSION/ABI_VERSION
  = 1`, `ROW_ABSENT`, `NODE_NONE`, `SMALL_TERM_THRESHOLD_DEFAULT = 32`.
  Serde derives exist behind an opt-in `serde` feature (only
  tessera-lifecycle enables it).
- `tessera-spatial`: `Extent` (+`validate()`), `cell`, `interleave`,
  `morton_of`, `Tile { prefix: u64, depth: u8 }` (`code_range()`),
  `tiles_for_bbox(bbox, depth, &Extent)`; `tiler::{TilerItem, ScalarValue,
  ScalarType, sort_batch}`. Depth/extent guards are **debug_assert only** —
  Task 13's server MUST validate request zoom (0..=16) and bbox before
  calling spatial (422 `contract`).
- `tessera-store`: `write::{write_segment, write_permutation}`;
  `manifest::{Manifest, SegmentsManifest, CurrentPointer}`;
  `read::open_bundle(root) -> Bundle` (full fail-closed read protocol:
  digest verification, loaded-file-must-be-verified coverage, path
  sanitisation, permutation slot validation); `SegmentData`/`ColumnsRef`
  (zero-copy typed slices over mmap); `Permutation` (`row_of`, `project` —
  projection costs seconds at 10⁹, cache per (token, slice, pin), NEVER
  per-viewport); `tile_ranges(seg, tile) -> Range<u32>` (engine level treats
  tile → Vec of ranges).
- `tessera-authz`: `dict::{DictWriter (+len()), Dict::{load, lookup, len}}`;
  `postings::{write_postings, PostingsReader::open(path, mmap),
  PostingRef::{Array(&[u8] raw LE u32s), Roaring(BitmapView)}}` (all records
  validated at open — fail closed); `fragment::{build_fragment(terms,
  &PostingsReader) -> Bitmap (union via Bitmap::fast_or — croaring 2.7 has
  no or_many), FragmentCache::new(dir, bundle_identity, auth_plugin_hash),
  get_or_build(satisfied, auth_data_hash, postings, watermark) ->
  Arc<FrozenFragment>, FrozenFragment { view() -> BitmapView, watermark }}`.
  Frozen files are digest-verified (sha256 in a 48-byte sidecar) before the
  unsafe view; cache dir is 0700/0600, engine-local.
- `tessera-plugin`: `builtin:passthrough` per Reference Sheet R6
  (terms_of_label = comma-split, terms_of_auth = JSON `{"terms": [...]}`,
  declared_bounds, data/auth hash = sha256("builtin:passthrough:1")). Trait
  mirrors the ABI; wasmtime host is Phase 2.
- `tessera-build` + CLI: `tessera build --points … --pairs … --out …
  --extent … --slice … [--limit N]` and `tessera verify <bundle>`.
  Signature-sorted allocation implemented (`signature_sort_key` = sorted
  term-id list; ordered (key, external_id)). ALL build-written files are
  digested in `MANIFEST.files`; `SEGMENTS-0.files` is `{}` (contracts §2.2).
- `tessera-lifecycle`: `wal::{WalRecord::{IngestBatch, Change, Lease},
  WalRow (records descriptors as Vec<Vec<u8>>, NEVER TermIds; records
  allocated entity ids), ChangeOp, Wal::{open (replay, TWAL header check,
  positional CRC rule — corruption below the sync point fails closed;
  missing sidecar on a non-empty WAL = everything acked), append, fsync
  (tmp+rename sidecar, dir fsyncs), poisoning on write errors}}`;
  `alloc::{Allocator, assign_sorted, high_water_from(&[WalRecord])}` (seed =
  max(manifest high-water, high_water_from(replayed))).

## Corpus facts (controller rulings — Tasks 14 and 16 depend on these)

- `data/scaled/pairs.parquet` does NOT exist; pairs live in
  `data/scaled/pairs/` as 7 policy variants. The chosen baseline is
  `data/scaled/pairs/categories-subclass.pairs.parquet` — Tasks 14/16 must
  use the same variant the bundle was built from.
- `data/scaled/geometry.parquet` has NO x/y columns — schema is
  `(entity_id, morton, row_id, priority)`. The build de-interleaves the
  stored Morton code under the **mandatory identity extent
  `--extent 0,65536,0,65536`** (hard-enforced; byte-exact, tested). Bundles
  are cell-faithful, not coordinate-faithful.
- A verified 250k test bundle builds in ~6 s / <100 MB RSS to
  `/tmp/tessera-250k` (rebuild at will:
  `cargo run --release -p tessera-cli -- build --points
  data/scaled/geometry.parquet --pairs
  data/scaled/pairs/categories-subclass.pairs.parquet --limit 250000
  --extent 0,65536,0,65536 --slice s0 --out /tmp/tessera-250k`).
- Disk is tight (~98 GB free): delete scratch bundles; the 10⁹ bundle
  (~60 GB) is built ONCE, in Task 16 only. Python env for Tasks 14/15:
  `uv venv --python 3.12` + pyroaring pyarrow polars numpy pytest requests
  (system Python is 3.10).

## Process lessons from Tasks 0–9 (keep doing these)

- Every task so far needed exactly one fix round; reviews found 7 Criticals
  total (UB on untrusted bytes ×2, verification-coverage hole, three WAL
  fail-open windows, frozen-view trust gap) — the loop pays for itself; do
  not skip reviews or accept "Approved with Importants" without a fix round
  (any Critical/Important triggers the loop).
- Give reviewers the invariant context (which invariant the task bears,
  what fail-open looks like there) and named scrutiny targets; opus reviews
  of security-bearing diffs were the highest-value dispatches of the session.
- Implementers disclose deviations honestly — read their concerns; two
  corpus-shape discoveries (above) came from an implementer, not a review.
- The ledger (`progress.md`) carries deferred minors and notes-for-later
  tasks (e.g. "note for Task 13", "deferred → Task 15", "deferred → Task
  16"). Carry the relevant pointers into each task's dispatch, and point the
  final review at all of them.

## Remaining work

Tasks 10–16 per their briefs, then the final review. Task 10 (overlay +
I1 composition) is next and is the most invariant-dense remaining task —
its brief encodes the three-independent-facts overlay model and the
minus⊆base / plus∩base=∅ diff-composition rules that came out of the plan's
own pre-execution review; implement them exactly as written. Watch Task 10's
consumption of Task 9's descriptor-based WAL records (resolve descriptors →
in-memory TermIds via the bundle dict + deterministic replay-order interning;
in-memory-only terms are unsatisfiable until the next build — fail closed).
