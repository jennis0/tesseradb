> **ARCHIVED 2026-08-01 — SUPERSEDED by the epic model. This was the live plan when planning moved to GitHub issues; its remaining work is now tracked there.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# Tessera Phase 2 — Roadmap, and the Stage 2.1 Implementation Plan

**Date:** 2026-07-31 · **Revision:** 2 (r1 reviewed adversarially on three lenses — security, performance, engineering; three CRITICALs and thirteen IMPORTANTs folded in) · **Source spec:** implementation plan §6 and §10 · **Status:** plan, for re-review before code

---

## Context

Phase 1's walking skeleton is complete and green. The serving-performance campaign that followed left Phase 2 in a different shape than plan §6 describes, and favourably: **§6's sampler half is largely already built.** What §6 calls "the hash-derived per-point priority" is r21's `tessera_id` prefix; the selection definition (floor ∪ threshold ∪ cap) landed at `263ba72` in [select.rs](crates/tessera-engine/src/select.rs). What remains of it is the refusal comment §6 demands, and the multi-segment merge that only becomes reachable once flush creates a second segment.

The mass of Phase 2 is elsewhere: **ingest becoming queryable**. Phase 1's buffered items are durable and carry authorisation state but have no row, so they contribute to no viewport, count or density — their ack is a durability receipt, not a visibility promise (design §11.2). Flush is what changes that, and behind it sit compaction and the epoch ledger, because without a fold deletions are terminal and the overlay grows monotonically. Alongside, plan §10.1's suite — today a three-test scaffold — has to become the deliverable it is described as.

Three facts about the current code shape the plan:

- **`SegmentsManifest` parses `deltas`, `tombstones` and `deny` ([manifest.rs:216-233](crates/tessera-store/src/manifest.rs#L216-L233)) and the read path consults none of them.** Inert while nothing writes them; fail-*open* the moment something does.
- **The write path is serialised by holding the WAL mutex across append→fsync→apply→swap** ([session.rs:844-921](crates/tessera-engine/src/session.rs#L844-L921)), with 14 lines of comment explaining that the discipline is load-bearing. A single writer thread makes that ordering structural.
- **`concurrency/viewpath` rewrote most of the files this stage touches** and is now merged — `session.rs`, `viewport.rs`, `control.rs`, `error.rs`, `state.rs`, `server/session.rs`, `config.rs`, `check-layers.sh`, `fragment.rs` and both shared test files. It replaced `row_projection_cache` with a `SingleFlightCache` whose miss path is non-blocking and returns `EngineError::ProjectionBuilding`. **Every line reference below was re-verified against `9f2424a` after the merges; they moved substantially, and stale ones strand a worker.**

### Owner rulings

1. **Candidate lists are declined.** Phase 0 measured run ratio 1.03–1.15; the hot-path memo refuted B1 by cell-occupancy measurement. Direct evaluation is the main route by measurement, not a fallback. §6 requires this in the source. The single-cell-tile fast path stays on the perf ledger.
2. **No external oracles** — neither `accumulo-access` nor DuckDB. Native differential only; the release-gate dependency assertion is retained, being what keeps them out. **Consequence: I5 ships unenforced — O5.**
3. **Perf is not the objective but is not excluded.** Fresh components get built performantly the first time; wins falling out of already-rewritten code are in scope. Standalone ledger items (B10, B3/B4) stay a separate workstream.
4. **Preconditions, not tasks** — see the gate below.

---

## Phase 2 roadmap — four stages plus a standing track

| Stage | Delivers | Exit criterion |
|---|---|---|
| **2.1 — Foundations and the write path** | One writer thread owning the WAL; pins that survive a generation swap, bounded per lifecycle §2.2; caches that evict without thrashing; group-commit allocation with window-scoped signature sort; the fail-closed side-manifest guard | Group commit's compression effect measured; no acked deny lost or re-exposed; denies never load-shed; **serving not regressed** |
| **2.2 — Read path and the publication package** | Deltas and tombstones honoured; streamed segments with their own permutation; multi-segment read and the global-bottom-*m* merge; then flush + side-manifest publication + immediate deny publication + `readyz` freshness **as one unit** | **An ingested item becomes visible.** A suppression reaches a replica before any flush |
| **2.3 — Compaction and retirement** | The fragment epoch ledger and retirement floor; the three retirement rules kept distinct; compaction with full carry-forward and the evaluate fold; the tiered merge policy | The overlay stops growing monotonically; a post-snapshot tombstone survives the fold |
| **2.4 — The conformance harness** | The `conformance` feature (eight pause points, three commands); the eight interleaving scripts; three-tier CI gating; fuzz targets; the wasmtime plugin host for I6 | The full invariant suite runs in CI |
| **T — Conformance track** *(standing, from day 1)* | Adversarial mask catalogue and fixtures; the §7.2 definition in the Python oracle; the I7 differential and cross-zoom nesting; the I1 differential; the I2 canary extended to every aggregate; the byte scan per surface | Grows with each stage; folds into 2.4's gating |

**Why the standing track.** The oracle and fixture work needs no engine feature — it tests what exists and extends as each stage lands. It is what makes 2.4 small enough to plan in one sitting, and it means I7's differential exists *before* the merge it will prove.

Two items are placed by dependency, not theme: the **`/control/status` fragmentation figure** lands in 2.1 (it measures group commit, and deferring the measurement past the change is pointless); the **wasmtime plugin host** lands in 2.4 (its only Phase 2 consumer is I6's row). **Admin-plane hygiene** rides along in 2.1 because that stage rewrites the files anyway.

---

## The gate — preconditions

**Three of four are satisfied.** `main` is at **`9f2424a`** and carries all the code preconditions; verified present rather than assumed:

- **G1 — `concurrency/viewpath` — MERGED** at `53fa504`. `engine/src/{single_flight,cancel}.rs` and `authz/src/single_flight.rs` present; `EngineError::{ProjectionBuilding, FragmentBuilding}` present.
- **G2 — the batched-build rework — MERGED** at `d3581cb` + `49c5097`. `build/src/spill.rs` present; `BuildArgs` gained `batch_items`, `memory_budget`, `band_rows`.
- **G3 — `perf/b9-run-decode` — MERGED** at `544cd1a`. `decode_tier` and `RUN_DECODE_MIN_DENSITY_PCT` present in `select.rs`.
- **G4 — the criterion baseline — CAPTURED** at `9f2424a` against the surviving 2.4 M fixture:
  `fragment_build/100` 109.98 µs · `fragment_build/176` 271.69 µs · `compose` 68.163 ns · `viewport/tile_sweep_k0` 1.2210 ms · `viewport/gather_k30` 1.6637 ms · `viewport/gather_k500` 2.1652 ms.

**The 1e9 half of the gate is deferred, by owner decision (2026-07-31).** The 47 GB bundle did not survive the `/tmp` wipe and the box has 21 GB free, so a rebuild needs deliberate reclamation plus ~2.5 h. **The per-track gate is therefore the 2.4 M baseline used as a no-regression check**; the 1e9 A/B and O7's absolute-budget restatement happen at **stage close**. The accepted risk, recorded so it is chosen rather than discovered: *a regression that only manifests at 10⁵× scale stays hidden until the end of the stage.* Every track's review gate must treat the 2.4 M numbers as relative, never as evidence the latency budget is met.

## Execution — worktrees off `9f2424a`

All work happens in worktrees; `main` is never committed to directly.

```
.claude/worktrees/phase2-seam       (branch phase2/seam)      — Task 0, serial
.claude/worktrees/phase2-track-a    (branch phase2/track-a)   — Tasks 1, 2
.claude/worktrees/phase2-track-b    (branch phase2/track-b)   — Tasks 3a…10
.claude/worktrees/phase2-track-c    (branch phase2/track-c)   — Tasks 4, 5
.claude/worktrees/phase2-track-t    (branch phase2/track-t)   — the standing track
```

**Creating a track worktree is three steps, not one** *(added at the Task 0 review gate, F2 — rule 3's teeth were still discipline: the allowlist and its check landed with no hook and no CI running them)*:

```bash
git worktree add .claude/worktrees/phase2-track-c phase2/track-c
echo c > .claude/worktrees/phase2-track-c/.claude/track   # the per-worktree track marker
bash scripts/install-hooks.sh                             # once per clone; hooks are shared
```

The marker is a file rather than a git config value because `core.hooksPath` and the hooks
directory are **shared across every worktree** (`git rev-parse --git-common-dir` is one path for
all of them), so the one installed `pre-commit` cannot know which track it is running for. It is
gitignored, and a worktree without one — `main`, a scratch checkout — is not checked at all, which
is deliberate: the controller commits across track boundaries by definition, and a hook that
refused there would teach everyone `--no-verify`.

`phase2/seam` branches from `9f2424a` and merges to `main` first — every other branch is cut **from the merged seam commit, not from `9f2424a`**, since the seam is what makes the tracks disjoint. Each track then merges after its review gate, in the order set out below. Track T can branch from `9f2424a` immediately; it touches no crate the seam moves.

Stale worktrees from the merged branches (`concurrency-viewpath`, `agent-*`, `density-sampling`) should be pruned before starting, to keep `git worktree list` legible.

---

## How the work parallelises

The constraint is file ownership, not task independence — left as-is, most of stage 2.1's tasks want [session.rs](crates/tessera-engine/src/session.rs) and [control.rs](crates/tessera-server/src/control.rs). The discipline: **a seam commit first, then disjoint file ownership, enforced mechanically.**

Four rules:

1. **The seam commit changes no behaviour.** It moves code and names interfaces. The review artefact is a `cargo test -- --list` diff showing the same test set, plus a green run — not an assertion that nothing changed.
2. **Config keys are landed by the seam, not the tracks.** [config.rs](crates/tessera-server/src/config.rs) is otherwise the file every track wants.
3. **Ownership is an allowlist, not an intention.** Task 0 commits `.claude/track-allowlist.toml` mapping each track to its files, and a pre-commit check runs `git diff --name-only` against it. *Added at review: r1's "a track that discovers it needs another track's file stops and reports" was a wish; this gives it teeth.*
4. **Seam signatures are frozen at Task 0 review.** `WritePath`, `PinManager`, `RowProjectionCache`, `LifecycleHandle`, `Command`, `Receipt`. A signature change is a stop-the-line event, because C is independent of B only while they hold.

**Stage 2.1 tracks** (redrawn at review — r1's map was wrong in six places):

| Track | Owns | Tasks |
|---|---|---|
| **A — store and reader** | `tessera-store/src/{read,manifest,error}.rs`, `tessera-store/tests/bundle_read.rs`, the `select.rs` doc block + its `check-layers.sh` rule | 1, 2 |
| **B — the writer** | all of `tessera-lifecycle`, `tessera-engine/src/write.rs`, `tessera-server/src/{control,error,health}.rs`, `tessera-server/tests/http_write.rs` (new), the counters' `check-layers.sh` rule | 3a, 3b, 6, 7a, 7b, 8, 9, 10 |
| **C — engine state** | `tessera-engine/src/{pins,cache}.rs`, `tessera-authz/src/{fragment,single_flight}.rs`, `tessera-server/src/{state,session}.rs`, `tessera-engine/tests/{pins,cache}.rs` | 4, 5 |
| **T — conformance** | `reference/`, `conformance/` | standing |

Files r1 mis-assigned and this revision fixes: `check_bearer` is in [state.rs:241](crates/tessera-server/src/state.rs#L241), not `control.rs` — its fix moves to **C**. `error.rs` and `health.rs` were unowned and are needed by Task 3b — assigned to **B**. `server/src/session.rs` holds revoke ([:92-98](crates/tessera-server/src/session.rs#L92-L98)), Task 5's only `prune_token` call site — assigned to **C**. `check-layers.sh` is touched by A and B — split by rule, each track appending its own, reviewed together at integration. The shared test files `engine/tests/viewport.rs` and `server/tests/http.rs` are touched by both B and C — **Task 0 splits them**, extracting write-path cases into `server/tests/http_write.rs` and pin cases into `engine/tests/pins.rs`, so neither track edits a shared file.

**Merge protocol.** Every track passes the three-lens review gate (see Process) before it merges. A merges first — it genuinely collides with nobody. Then **B and C integrate sequentially with a rebase, not an end-state octopus**: C rebases onto B's merged head and re-runs its tests, because both moved code out of `session.rs` and their residual edits to `Engine::open` will conflict textually even though they are semantically disjoint. C's gate runs *after* that rebase, so the reviewers see the code as it will land.

**Across stages,** 2.2 and 2.3 are sequential; each subdivides the same way; T runs throughout.

---

## Global constraints

1. **The spec wins.** If this plan and a design document disagree, the document is right — STOP and report. Precedence: architecture design > contracts spec (§0.3 deviations govern) > everything else.
2. **Design for audit before performance.** Prefer the construction that is obviously correct.
3. Each task states the tests that must exist **and the plumbing they require**. A named test with no hook to hang on is not a test — see Task 3a's fault-injection deliverables.
4. British spelling; the established security vocabulary.
5. `bash scripts/check-layers.sh` and the track allowlist check green before every commit.

---

# Stage 2.1 — Foundations and the write path

**Delivers:** the single-writer thread every later stage hangs off; bounded pins that survive a generation swap; caches that evict without thrashing; group-commit allocation; the reader guard that makes 2.2's writers structurally unable to outrun the reader.

**Does not deliver** any new visible capability beyond `/control/status`'s fragmentation figure and ingest's admission bound. That is expected — this stage is the floor 2.2 and 2.3 stand on.

---

## Task 0 — the seam commit *(serial; blocks all tracks)*

**Nature:** pure refactor plus two architecture decisions that must be made here rather than discovered by a worker.

### Decision 1 — where the executor lives *(review C1: as r1 wrote it, Task 3 was a dependency cycle)*

The thread's loop runs append → fsync → **apply → swap** → ack. Apply and swap clone the `IngestBuffer`/`Overlay` out of a `Generation` and `store` a new one — and `Generation` lives in [engine/src/lib.rs](crates/tessera-engine/src/lib.rs) and holds a `tessera_store::Bundle`. But **engine depends on lifecycle**, so a thread in `tessera-lifecycle` importing `Generation` is a cycle cargo refuses, and lifecycle deliberately has no `tessera-store` dependency.

**Resolution: the executor lives in `tessera-engine/src/write.rs`; `tessera-lifecycle` owns the vocabulary.** Lifecycle keeps `Command`, `Receipt`, `CommitWindow`, the queues, `Wal` and `assign_sorted` — all of which are entity-space and store-free. Engine owns the thread that consumes them, because only engine can see both a `Wal` and a `Generation`. No callback indirection, no new dependency edge, `check-layers.sh` unchanged.

### Decision 2 — the `Command` shape *(review C2: r1's shape made its own headline test unpassable)*

`WalRow.entity_id` is mandatory ([wal.rs:82-84](crates/tessera-lifecycle/src/wal.rs#L82-L84)), so `Command::Ingest { rows: Vec<WalRow> }` forces the *handler* to allocate before submitting — which Task 7 then makes impossible, since window-scoped allocation happens at close. r1 asserted "no variant here changes"; both could not be true.

**Resolution: `Command::Ingest` carries unallocated rows from day one.**

```rust
/// Everything a WalRow needs except the entity_id, plus the resolved term set that
/// is its sort signature. Allocation happens on the executor — at Task 3a per
/// command, at Task 7a per window — and the type does not change between them.
pub struct UnallocatedRow { pub external_id: Option<Vec<u8>>, pub x: f32, pub y: f32,
                            pub scalars: Vec<WalScalar>, pub terms: Vec<TermId> }
```

The `allocator: Mutex<Allocator>` field ([session.rs:325](crates/tessera-engine/src/session.rs#L325)) and `allocator_high_water()` ([session.rs:621](crates/tessera-engine/src/session.rs#L621)) move behind the seam with it — r1's `WritePath` field list omitted the allocator entirely.

### The carves

1. **`tessera-engine/src/write.rs`** — move `accept_ingest` ([session.rs:844](crates/tessera-engine/src/session.rs#L844)), `accept_change` ([:923](crates/tessera-engine/src/session.rs#L923)), `apply_change_locked` ([:963](crates/tessera-engine/src/session.rs#L963)), `accepted_batches` ([:358](crates/tessera-engine/src/session.rs#L358)) and the allocator out of `session.rs` into `WritePath`. It needs `dict` and `resolver_state` too: `accept_change` calls `self.resolve_terms` *inside* the WAL critical section ([session.rs:941](crates/tessera-engine/src/session.rs#L941)) and cannot move without them.
2. **`tessera-engine/src/cache.rs`** — wrap the merged `SingleFlightCache<(u64, String, u64), RowProjection>` ([session.rs:309](crates/tessera-engine/src/session.rs#L309)) as `RowProjectionCache`. **Its miss path is fallible and non-blocking** (`EngineError::ProjectionBuilding`), so the wrapper's signature is `get_or_build(...) -> Result<Arc<RowProjection>, CacheBusy>`, not r1's infallible `get_or_insert`.
3. **`tessera-engine/src/pins.rs`** — move the pin check ([viewport.rs:429-441](crates/tessera-engine/src/viewport.rs#L429-L441)) behind `PinManager::resolve`, returning:

   ```rust
   /// Geometry only. Overlay, buffer and overlay_version are deliberately absent: a pin
   /// fixes row-space geometry and NEVER authorisation state (I11, lifecycle §2.3) — a
   /// suppression applies to a pinned request the moment it is accepted.
   pub struct PinnedGeometry {
       pub prefix: String,
       pub segments_version: u64,
       /// ADVISORY — status and debugging only. NEVER an input to I1 composition: the
       /// effective watermark is always the mask fragment's own (lifecycle §2.3 and its
       /// Appendix R action 2, which amended five separate phrasings implying otherwise).
       pub watermark: u64,
       pub bundle: Arc<Bundle>,
   }
   ```

   *Added at review (security I-3): the `watermark` annotation. A bare field of that name on the type every pinned request receives is the most natural thing in the world to feed into `L = overlay ∪ {entities ≥ W}`, and composing with a pinned `W` below the fragment's produces wrong counts.*
4. **Test-file split** — extract write-path cases from `server/tests/http.rs` into `http_write.rs`, and pin cases from `engine/tests/viewport.rs` into `engine/tests/pins.rs`, so B and C never edit a shared file.
5. **Config keys**, all of them, so no track touches `config.rs` again: `commit_window_max_items`, `commit_window_max_age_ms`, `ingest_queue_bound`, `ingest_max_batch_rows`, `ingest_max_batch_bytes`, `wal_hard_limit_bytes`, `row_projection_cache_bytes`, `fragment_cache_bytes`, `expected_concurrent_sessions`, `pin_ttl_secs`, `pins_per_session_max`, `overlay_soft_limit`, `flush_max_items`, `flush_max_age_secs`. **Only `flush_max_items` and `flush_max_age_secs` are inert this stage** — `overlay_soft_limit` alarms from Task 6 (r1 contradicted itself on this; review I5). Every unit is `_secs` or `_ms` explicitly and consistently.
6. **`.claude/track-allowlist.toml`** and its pre-commit check.

**Done when:** `cargo test -- --list` is identical before and after, the suite is green, `check-layers.sh` passes, and the allowlist check runs.

---

## Track A — store and reader

### Task 1: `tessera-store` — refuse a side-manifest carrying state the reader cannot honour

**Invariants:** I1. **Spec:** contracts §2.3 reader protocol; plan §6 precondition 1; SA §6.2, §9.
**Files:** [read.rs](crates/tessera-store/src/read.rs) (`load_verifying_segments_manifest` at [:279](crates/tessera-store/src/read.rs#L279); candidates sorted newest-first at [:285](crates/tessera-store/src/read.rs#L285) and tried in the loop at [:294](crates/tessera-store/src/read.rs#L294)), [manifest.rs:223-231](crates/tessera-store/src/manifest.rs#L223-L231), `error.rs`.

**Design.** No format change:

```rust
pub const HONOURED_STATE: &[&str] = &[];   // 2.2 adds each as it is implemented
impl SegmentsManifest {
    /// Never a bool: "carries tombstones" and "carries deltas" want different
    /// operator responses, and — see below — different reader behaviour.
    pub fn unhonourable_state(&self) -> Vec<&'static str>;
}
```

**The split that matters — corrected at review (security C-1, the one CRITICAL).** r1 put the check in the candidate loop and let *every* unhonourable manifest step down to an older one. That is fail-open, and it is exactly the fail-open the guard exists to close: a manifest carries `deny` or `tombstones` **because a deny was accepted** (contracts §2.3's publication rule). Stepping down past it is precisely the state contracts §2.3 forbids a replica to reconstruct — every suppression and deletion since the last honourable manifest silently undone, indefinitely, with `readyz` green, because the freshness gate that §2.3 pairs with step-down does not land until 2.2.

So the two cases separate:

- **`deltas` only** → **step down.** Staleness in the fail-safe direction: items missing, never items re-exposed. The availability argument (a mid-sync replica should serve the older consistent state rather than go hard down) holds here and only here.
- **`tombstones` or `deny` non-empty and unhonourable** → **the partition is unready.** Typed error, operator response "run a build that honours this manifest". Between hard-down and serving suppressed items forever, the corpus chooses hard-down every time it is asked (SA §9: "a worker that cannot verify its partition marks itself unready rather than serving partial data").

`StoreError::UnhonourableManifest { partition, n, fields }`, surfaced as the per-candidate reason. Log the step-down at `warn` with partition, `n` and field names — never entity IDs (SA §9).

**Tests.** A fixture manifest with `"tombstones": [17]` → the partition does **not** open, and `readyz` is not-ready; *today it opens and serves entity 17, which is the fail-open demonstrated*. Same for a `deny` entry. `"deltas": [1]` → steps down to `SEGMENTS-0.json` and serves. All-empty opens normally. Every candidate unhonourable → not-ready.

**Done when:** no reachable path serves a row named by a tombstone or covered by a deny that the build does not understand.

### Task 2: `tessera-engine` — record the candidate-list refusal in the source

**Invariants:** I7. **Spec:** plan §6 — "This warrants a comment in the source, not just a line in a document."

**Design.** A `//! ## Why there is no candidate-list route` block in [select.rs](crates/tessera-engine/src/select.rs), four claims each carrying its evidence, because a claim without evidence gets re-litigated: (a) tippecanoe's multiplier clusters degrade to empty tiles below pass rate ~1/N, and a width-`c·k` list yields `k` survivors only above coverage `1/c` — 25% at `c`=4, describing almost no realistic principal; (b) Phase 0 measured run ratio 1.03–1.15 and §7.2 r18 puts most occupied depth-6 tiles below the ~5% crossover; (c) direct evaluation is bounded by the tile's priority block and gets *cheaper* as coverage falls, so below ~5% it is also faster — **the main route, not the fallback**; (d) deleting it to "simplify" reintroduces tippecanoe's failure mode silently, for the users least able to report it.

Separately, one sentence in the hot-path memo recording the single-cell-tile fast path as a perf-ledger item explicitly not in this plan.

`check-layers.sh` fails if `select.rs` lacks the marker `NO CANDIDATE-LIST ROUTE`. A comment CI cannot notice being deleted is a comment that will be deleted.

---

## Track B — the writer

### Task 3a: `tessera-engine`, `tessera-lifecycle` — the executor thread, the two queues, and the fault hooks

**Invariants:** I1, I9; the ack ordering. **Spec:** lifecycle §1.3, §4, §7. *(Split from r1's Task 3 at review — it bundled the thread, the handlers, readiness and four tests needing plumbing that did not exist.)*

**Design.** One `std::thread` per partition, in `engine/src/write.rs`, owning the `Wal` **by value**. That is the substance: today's ordering is a discipline held by a mutex and a comment; after this it is a consequence of single ownership, and `Mutex<Wal>` is deleted along with the Critical-1 lost-update race it was defending against.

```rust
pub enum Command {
    Ingest { rows: Vec<UnallocatedRow>, batch_id: String, body_hash: [u8; 32] },
    Change { external_id: Vec<u8>, entity: EntityId, op: ChangeOp, descriptors: Option<Vec<Vec<u8>>> },
}
// Flush and Compact are added additively in 2.2 and 2.3.

pub struct LifecycleHandle {
    work: SyncSender<Job>,   // bounded by ingest_queue_bound
    deny: Sender<Job>,       // unbounded — a deny is never refused for load
}
impl LifecycleHandle {
    /// Both return a Result: the executor can be dead, and a handle that swallows that
    /// while still returning 202s is the worst outcome (review I-5).
    pub fn submit(&self, cmd: Command) -> Result<Receipt, SubmitError>;      // may 429
    pub fn submit_deny(&self, cmd: Command) -> Result<Receipt, SubmitError>; // never 429; may report a dead executor
}
```

The asymmetry is the design: the work queue is **bounded** (full → 429), the deny queue **unbounded** (never refused for load). The loop drains deny to empty before touching work, so a deny's wait is bounded by the work item currently executing, not by queue depth. *Note both consequences so they are chosen, not discovered: a sustained deny flood starves ingest completely, and the deny queue is unbounded in memory.*

Execution here is one command at a time with today's semantics — allocate (per command, via `assign_sorted`) → append → fsync → apply → swap → ack. Task 7a widens it to a window; separating them means the ordering can be reviewed before batching complicates it.

**Receipt awaiting must not block the reactor** — a tokio handler blocking on `recv()` re-creates the defect the concurrency branch fixed. Use an async-aware oneshot, or `spawn_blocking` at the handler.

**Fault hooks are deliverables of this task, not of stage 2.4** *(review I2; the Phase 1 ledger already records "deny-op WAL-failure path inspection-only — fault-injectable Wal would test it")*: a fault-injectable `Wal` (append/fsync failure on command), an fsync counter, and a stallable executor with a kill point between fsync and swap. Every named test below needs one of them, and 2.4's pause points are three stages away.

**Failure modes.** WAL append fails on `Delete`/`Suppress`: apply anyway, swap, then return the error — the item is hidden immediately and the caller gets 500 + alarm (lifecycle §4). Never a refusal that leaves a deny unapplied. A failed append **poisons the WAL** ([wal.rs:146](crates/tessera-lifecycle/src/wal.rs#L146) — `self.len` can no longer name a record boundary, so every subsequent call refuses), which must therefore trip the not-ready posture rather than being retried. Executor panic: sends fail, planes report not-ready.

**Tests.** `a_deny_is_never_queued_behind_work`. `ack_follows_fsync_then_swap` — an ordering log records exactly `append, fsync, swap, ack`; *the fail-open is an ack preceding the swap, letting a client observe a 200 for a suppression not yet in force*. `deny_append_failure_still_applies` — injected disk-full on a suppress returns 500 **and** the item is invisible next query. `a_poisoned_wal_trips_not_ready`.

### Task 3b: `tessera-server` — handler migration and the readiness posture

**Files:** [control.rs](crates/tessera-server/src/control.rs), [error.rs](crates/tessera-server/src/error.rs), [health.rs](crates/tessera-server/src/health.rs).

**Design.** `/control/ingest` and `/control/changes` submit and await instead of touching the WAL, retiring the deferred minor "blocking WAL fsync inside async handlers without `spawn_blocking`". `error.rs` gains the `SubmitError` mapping: queue-full → 429 with `retry_after_s`, dead executor → 503.

**`readyz` is wired here, and this is new work, not a tweak** *(review I-5/C3: three tasks in r1 asserted readiness and no task owned it)*. Today it is a stateless `StatusCode::OK` ([health.rs:16-17](crates/tessera-server/src/health.rs#L16-L17)) whose module doc records the freshness gate as "trivially satisfied in Phase 1". It no longer is: readiness now means executor alive and WAL unpoisoned.

*Two corrections, both from the Task 3b worker and both confirmed at its gate (2026-08-01).* **"Every partition opened" is not a readiness conjunct** — an unhonourable manifest makes `Engine::open` fail, so `prepare` returns `Err` before `run` binds a listener, and the conjunct would be compile-time `true`. Track A's Task 1 discharges it at startup, not at `readyz`. Task 3b ships a table of contracts §3.1's five conditions against where each is discharged instead, which is the honest shape. And **`readyz` needed only `health.rs`** — `viewer.rs` and `session.rs` already routed it with state, so "touches all three routers" overstated it.

~~*Also fold in the branch's own follow-up, which r1 missed:* `EngineError::{ProjectionBuilding, FragmentBuilding}` currently take the fail-closed **500** arm; the branch's doc says they should be **429**.~~ **Already done, and this line was stale when written** *(Task 3b worker, confirmed independently at its gate, 2026-08-01)*. `fa497fe` mapped both to 429 with two tests on 2026-07-30 — a strict ancestor of this plan's own gate commit. The worker wrote no third test and demonstrated the existing pair by deleting the arms. Recorded rather than deleted, because a plan that quietly loses a claim teaches nothing; the lesson is that "the branch's doc says X should happen" is not evidence that X has not happened.

**Tests.** `a_dead_executor_is_not_ready`. `queue_full_is_429_with_retry_after`. ~~`projection_building_is_429_not_500`~~ (exists already — see above).

### Task 6: `tessera-server` — the admission bound and the never-shed asymmetry

**Spec:** design §11.2; SA §6.5, §7; contracts §3.1's 429 row; lifecycle §4's headroom rule.

**Design.** `/control/ingest` submits to the bounded queue; full → `429` with `retry_after_s` from window age plus observed drain rate. `/control/changes` submits to the unbounded deny queue and **cannot** 429.

**The headroom arithmetic now has operands** *(review I-2/I3: r1 asserted a check over three quantities that did not exist — no per-batch caps, no WAL bound, and a queue bounded in entries so one ten-million-row batch walked straight past it)*. Task 0 landed `wal_hard_limit_bytes`, `ingest_max_batch_rows`, `ingest_max_batch_bytes`; this task enforces the batch caps in the handler (422 per contracts §3.1) and asserts at startup that `ingest_queue_bound × ingest_max_batch_bytes` plus reserved deny headroom sits strictly below `wal_hard_limit_bytes` — refusing to start and naming both values otherwise. **Queued `Command`s also hold their rows in heap**, so RAM enters the same arithmetic (~1 GB at bound 100 × 10 k rows × 1 KB).

**Ordering rule:** the 429 signal is evaluated only *after* `check_bearer`, so an unauthenticated caller cannot observe ingest pressure. `control.rs` does this everywhere today; state it so the rewrite preserves it.

`overlay_soft_limit` alarms and increments a metric here. It cannot yet *schedule a fold* — there is no fold until 2.3. Say so at the config site: an operator setting it today gets an alarm, not relief.

**Tests.** `ingest_429s_when_the_queue_is_full`. **`changes_never_429s` in the same state** — the asymmetry, and the test that matters: batching a security operation for latency is acceptable, refusing one for load is not. `an_oversized_batch_is_422_not_a_queue_slot`. `headroom_arithmetic_is_checked_at_startup`. `backpressure_is_invisible_before_auth`.

### Task 7a: `tessera-lifecycle` — the commit window and window-scoped allocation

**Invariants:** I9. **Spec:** lifecycle §5.1; design §11.1 r23.

**Design.**

```rust
pub struct CommitWindow {
    entries: Vec<WindowEntry>,
    by_batch: FxHashMap<String, usize>,   // Task 8's join index
    opened_at: Instant,
    seq: u64,
}
pub enum WindowEntry {
    Ingest { rows: Vec<UnallocatedRow>, batch_id: String, body_hash: [u8; 32], waiters: Vec<Responder> },
    Change { .. , waiters: Vec<Responder> },
}
```

**Allocation is the point.** On close, every ingest entry's rows are gathered into one `Vec<PendingItem>` and passed to the existing [`assign_sorted`](crates/tessera-lifecycle/src/alloc.rs#L173) — unchanged; only its input set widens. IDs scatter back by `(entry_index, row_index)`. This makes design §11.1's sort scope **the window, at the server**, not whatever chunk size a client picked.

**I9 is untouched** — IDs are still issued monotonically from the high-water; the window changes only *how many* are assigned in one sorted run. Cite lifecycle §5.1 at the site, since "the window allocates" reads like an allocator change and is not one.

**Calibrate the claim honestly** *(review M1)*: the probes' 8.9–36.7× compression was measured under a **full-corpus** signature sort. A window realises run lengths ≈ `window_items × term_density` — a 10 k window at 2% density gives runs ~200 against a ~1.0 scattered baseline: still a large win, but a *fraction of the ceiling*, and it scales with `commit_window_max_items`. Size that knob from this arithmetic and record the achieved ratio against the ceiling.

Then: one `WalRecord::IngestBatch` per entry (batch identity preserved for Task 8), **one** `fsync`, one generation swap applying every entry, then all waiters acked. WAL rows carry allocated IDs so replay reuses them.

**A free win in code being rewritten anyway** (owner ruling 3): today the record is framed from `rows.clone()` inside `WritePath::accept_ingest` (`crates/tessera-engine/src/write.rs` — **the seam moved this out of `session.rs`; find it by symbol, not by line**). Frame the bytes first, then move the rows into the buffer apply.

**Tests.** `the_sort_scope_is_the_window_not_the_request` — four 25-row submissions with interleaving signatures produce an assignment **identical** to one 100-row submission; *if this does not hold, group commit is decoration*. `one_fsync_per_window`. `each_waiter_gets_its_own_ids` in its own row order. A proptest: IDs strictly monotone across windows, none twice, **none issued that is not in the WAL**.

### Task 7b: `tessera-lifecycle` — close policy and window failure semantics

**Design — close policy, three triggers.** `entries.len() >= commit_window_max_items`; `opened_at.elapsed() >= commit_window_max_age_ms`; **or both queues are empty.** Without the third, a single ingest on an idle server waits the full window age for company that is not coming, converting a latency budget into a latency floor.

**Failure semantics — new at review (security I-1; r1 specified per-command rules that do not compose into a window).** Three rules:

1. **Append order = entries order = apply order.** Replay applies records in append order; the live path applies entries in vec order. If an implementation appends all ingest then all changes while applying in arrival order, a live `suppress → unsuppress` can replay as `unsuppress → suppress` — and the mirror interleaving replays fail-open. Tested by replay-equivalence of a mixed window.
2. **Partial failure splits by disposition, and the split is not expressible as one uniform swap.** Ingest entries in a failed window have **no effect** and 500 — applying un-fsynced ingest makes items appear and vanish on the next crash, and lifecycle §4's apply-anyway rule is for `Delete`/`Suppress` **only**. Deny entries in the same window apply, swap, and 500. The executor then transitions to not-ready (the WAL is poisoned).
3. **The starvation bound is two windows, not one.** A deny racing the close decision lands in the next window, so the worst case is ≈ 2 × `commit_window_max_age_ms`. Lifecycle §1.3 r4's "never starved beyond one window" holds for a deny that reaches an open window; state the racing case honestly rather than rounding it down.

**Buffer clone cost — record it** *(review perf I1)*. The window reduces clone *count*, not clone *cost*: `IngestBuffer` is an `FxHashMap` with two heap `Vec`s per item, so a clone is O(total buffered items) — estimated 150–300 MB and 100–300 ms at 1 M items, 1.5–3 GB and 1–3 s at 10 M, with a transient 2× spike. Because a deny's wait is bounded by the item currently executing, **this is the deny-ack latency floor and it grows linearly with buffer depth**, while `flush_max_items` is inert this stage. Emit a clone-duration counter, tie `overlay_soft_limit` to the deny budget in the startup arithmetic, and **record now that stage 2.2 makes the buffer chunked/persistent when it rewrites buffer handling for flush** — so 2.1's O(B) clone is a known temporary rather than an inherited posture.

**Tests.** `an_idle_submission_does_not_wait_for_the_age_bound`. `a_mixed_window_replays_in_append_order`. `a_failed_window_applies_denies_and_drops_ingest`. `a_poisoned_wal_trips_not_ready` (with 3a).

### Task 8: batch-id idempotency across a held window

**Spec:** contracts §3.4; lifecycle §5.1's "the one genuinely new piece of design".

**Design.**

```rust
pub enum BatchState {
    Accepted { body_hash: [u8; 32], entity_ids: Vec<EntityId> },
    Held     { window_seq: u64, body_hash: [u8; 32] },
    Unknown,
}
```

Lookup order: durable index → open window → unknown, **evaluated on the executor, not in the handler** — between a handler check and the enqueue the window can close, and a retry that saw `Unknown` then enqueued into a fresh window has double-allocated.

Per state: *unknown* → new entry. *Accepted, same bytes* → replay recorded IDs (re-deriving would not work at all for a row with no external ID). *Accepted, different bytes* → 409, no effect. *Held, same bytes* → **join**: append the caller's responder to the existing entry's `waiters`, so both receive the same IDs off one allocation. *Held, different bytes* → 409, **and the held original still applies** (O1).

*Record the retention bound on `Unknown`:* `accepted_batches` is rebuilt from WAL replay in `WritePath::reconstruct` (`crates/tessera-engine/src/write.rs` — **the seam moved this out of `Engine::open`; find it by symbol**), so past `wal_retention` an old `batch_id` regresses to `Unknown`. Note there is no `wal_retention` config key yet — the seam's Task 0b report records its absence. Rows with external IDs are caught by the duplicate check; **rows without one re-ingest silently as new entities.** Pre-existing, but this is the task that formalises the state machine and should carry the caveat.

**Tests.** `a_retry_joins_rather_than_reallocating` — hold the window, submit `B` twice byte-identically, release: high-water advanced by `rows` not `2·rows`, both responses carry the **same** `tessera_id` list, exactly one WAL record. `held_plus_different_bytes_409s_without_disturbing_the_original`. `crash_between_fsync_and_swap_replays_rather_than_reallocates`.

### Task 9: denies inside the window — coupled and bounded

**Spec:** lifecycle §5.1's two non-negotiable rules, §4.

**Design.** Denies enter the same window as ingest. Batching a deny for latency is fine; acknowledging one before it is in force is not.

- **Coupled ack.** A deny's receipt resolves only after the generation carrying it has been swapped in — the ack step runs strictly after `store()`. **Never a 200 for an entry not yet in force.**
- **Bounded starvation.** A deny arriving while a window is open joins *that* window (worst case two, per 7b rule 3). The drain-deny-first rule is what makes this true when a window is closing as the deny arrives.

The disk-full path now lives in one place: append fails, `Delete`/`Suppress` applies to the in-memory overlay and swaps anyway, receipt carries the error → 500 + alarm. **Nothing durable is published in 2.1**, so there is nothing to gate — the gate lifecycle §4 puts on side-manifest publication is a 2.2 obligation, recorded as a comment at the failure site so 2.2 need not rediscover it.

**Tests.** `a_denys_ack_is_coupled_to_its_application` — the request is outstanding while the executor is stalled before the swap; after release the 200 arrives *and* a viewport issued immediately after no longer shows the item. `a_deny_is_applied_at_the_window_it_joined`.

### Task 10: the fragmentation figure, and admin-plane hygiene

**Spec:** contracts §3.4 r8; SA §9; §3.1's 422 row.

**Design — the figure, and where it is actually computed** *(reviews I3/I4: r1 said "counters emitted where postings are written", and **nothing in the serving process writes postings in 2.1** — flush is 2.2, so an implementer would have gone to `tessera-build`, whose global sort shows no window effect at all)*.

**The emission site is window-close allocation.** With IDs just assigned, accumulate per-term `(postings, runs, Σ(1−p_t)·postings_t)` by comparing each `(row, term)` pair against that term's last-assigned ID. `run_ratio = expected_runs / actual_runs`; `postings_per_container` from the same counters. Scan-free, as required — nothing on the viewport path reads them, asserted by a `check-layers.sh` rule.

Two costs to state rather than discover: the incremental state needs a per-term last-assigned ID, O(distinct terms) — free at 48 k terms, **~2.8 GB at the surnames set's 116.9 M**, the same term-cardinality scaling that OOM-killed that build; and ~4–5 hashmap probes per row on the executor, ~40–50 ms per 100 k-row window, which belongs in the window's execution budget.

**It is the entity-space quantity** — posting run length, probes results §2 — *not* the row-space mask run ratio of their §5. They normalise the same way over different sets and are not comparable; the doc comment must say so.

**Design — hygiene** (three here; `check_bearer` moves to Track C, since it lives in `state.rs`): `parse_ingest_batch` validates the batch schema against `MANIFEST.declared_scalars` and returns `422` naming the column instead of silently dropping non-`U64`/`F32`/`Utf8` ones — *a silent drop is positional misalignment wearing a success's clothes*; `x-tessera-slice` is read and validated (unknown → 422, absent with two slices → 422, absent with one → accepted); `over_bound_ids` is base64 like every other external-ID surface rather than `from_utf8_lossy`, which destroys an 8-byte-LE ID.

**Tests.** Hand-computed counters over a constructed layout. **The behavioural test that is the figure's purpose:** the same corpus ingested as one large window versus a hundred small ones with the window disabled — the former's `run_ratio` materially higher, and the achieved ratio recorded against the full-sort ceiling. One test per hygiene fix.

---

## Track C — engine state

### Task 4: `tessera-engine` — the pin manager, the drain list, and its bounds

**Invariants:** I11. **Spec:** lifecycle §2.1 (remove → verify → reclaim), **§2.2 (TTL and per-session cap)**, §2.3.

**Design.** Today's pin is an equality check against the live generation, correct only because there is never more than one. 2.2's flush and 2.3's compaction both supersede generations while requests are in flight.

```rust
/// Slimmed, not Arc<Generation> (review C2): holding a whole generation also pins the
/// superseded overlay and buffer — state a pin must never use — and makes the I11
/// "never returns authorisation state" property policed at resolve rather than structural.
struct DrainEntry { prefix: String, segments_version: u64, watermark: u64,
                    bundle: Arc<Bundle>, retired_at: Instant }

pub struct PinManager { drain: Mutex<Vec<DrainEntry>>, per_session: Mutex<FxHashMap<u64, usize>> }
```

Four rules carry it:

- **`resolve` returns `PinnedGeometry`, never a generation.** Overlay, buffer and `overlay_version` come from the *live* generation regardless of the pin. Returning the superseded generation is the natural implementation and it is fail-open — the request would compose against a pre-suppression overlay.
- **Reclaim is remove → verify strong count is one → drop.** The inverse is a use-after-free: a `resolve` racing between verify and remove clones an `Arc` about to be dropped. A racing `resolve` that misses gets `None` → 410, which is correct — a drained pin is expired.
- **TTL and per-session cap, from `pin_ttl_secs` and `pins_per_session_max`** *(added at review — perf C2: r1 implemented lifecycle §2.1's drain list and silently dropped §2.2's bounds, which is not a liberty this plan has under its own Global Constraint 1)*. At a **measured 47.02 GB** bundle with a 22.5 GB viewport-hot set, one slow client holding a pin across two compactions contests essentially all of a 47 GB box's page cache and holds up to ~94 GB of invisible disk (`df` ≠ `du`) through deleted-but-mapped files. The failure is a warm-to-cold cliff: **measured** 4.0–4.5 ns/visible-row warm against a **modelled** 50–100 µs/page-miss. Expose a drain-depth gauge on `/control/status` and alarm above depth 1.
- **`resolve` must not take the drain lock on the common path** *(review perf I4)*. Every request calls it. Lock only when a presented pin fails the live equality check — otherwise stage 2.1 reintroduces a per-request global mutex on the viewport path at the branch's 48-way admission concurrency, the exact class the branch spent its F4 work removing.

**Restart:** the drain list starts empty, so every presented pin 410s. The two forbidden alternatives — reconstructing `(n_old, W)` from old side-manifests, or reinterpreting against current geometry — are commented at the resolve site; both look like helpfulness and the second is I11's exact failure ("not stale-restrictive but simply wrong").

**Tests.** `a_pin_survives_a_generation_swap`. **`a_suppression_applies_to_a_pinned_request_immediately`** — the test that catches the fail-open. **`a_pinned_request_composes_with_the_fragment_watermark_not_the_pinned_one`** — the negative control for `PinnedGeometry.watermark`. `a_drained_pin_is_410`. `reclaim_is_remove_then_verify`. `a_pin_past_its_ttl_is_410`; `a_session_cannot_exceed_its_pin_cap`. `resolve_takes_no_lock_when_no_pin_is_presented`.

### Task 5: `tessera-engine`, `tessera-authz` — cache eviction that does not thrash

**Invariants:** I3's cache half; the memory bound. **Spec:** design §8.5; lifecycle §7. *Absorbs three deferred minors: the projection cache never evicts, revoke does not prune it, bundle swap does not prune it.*

**Design.** Keep the merged `SingleFlightCache` shape and key `(token_id, slice, segments_version)`; add an LRU list, a byte bound, and two pruners — `prune_token(token_id)` on revoke (call site: [server/src/session.rs:98](crates/tessera-server/src/session.rs#L98)), `prune_generation(segments_version)` on reclaim.

**Prune on drain-list reclaim, not on generation swap.** A pinned request still needs its generation's projection, and its `segments_version` is exactly the key a swap-triggered prune would delete. Tying cache lifetime to the rule governing the geometry the entries describe is why this task depends on Task 4 rather than merely on the swap.

**Thrash protection — the part r1 was missing entirely** *(review perf C1)*. The miss/hit cost ratio here is 10⁵–10⁷: a projection miss is `RowProjection::new`, **measured** at seconds (the 10⁹ warm-up viewport is 10.7 s), and every ≥25%-coverage mask at 10⁹ serialises to a **measured** 125.12 MB dense bound — so at the w=10⁴ operating point each entry is ~125 MB. A `row_projection_cache_bytes` of 512 MB holds **four sessions**; five active sessions round-robining is a 100% miss rate, and because the single-flight miss path returns `ProjectionBuilding` to every racer, the failure presents as a permanent 429 storm with a core set pegged on rebuilds. Plain LRU does not degrade in this regime, it collapses. So:

- **Startup validation**: `row_projection_cache_bytes` **and `fragment_cache_bytes`** must each admit at least `expected_concurrent_sessions` entries at the measured per-entry size, or the server refuses to start — the same pattern as Task 6's headroom assertion. Document ~125 MB/session at 10⁹ in the config doc. *(Both caches, added at the Task 0 review gate, F11: they hold the same-shaped Roaring object at the same measured size, and the fragment bound deliberately carries no margin — a validation covering only one leaves the other free to be set to a value that collapses. The two bounds' entry counts are governed by different quantities, sessions against distinct grant sets, and both constants now say so.)*
- **Evicted `Arc`s are collected under the lock and dropped after release.** Dropping a ~125 MB bitmap frees ~15 k containers; doing it inside a lock the branch's module doc makes load-bearingly O(1) convoys 48 admitted requests. This must be stated or the natural implementation does the wrong thing.
- **Eviction and miss counters on `/control/status`**, with an alarm on sustained eviction of young entries.
- Sizing via `get_serialized_size_in_bytes` — O(containers), microseconds, fine; note at the accounting site that it underestimates in-memory footprint for array containers with capacity slack (up to ~2×), so the bound is approximate.

`FragmentCache` gains `evict(key)` (2.4's conformance command calls it) and a byte-bounded in-memory tier over the same shape. The `.frag` sidecar tier is untouched — digest-verified, so an in-memory eviction costs a re-open plus SHA-256 of ~125 MB (~60–80 ms estimated), never correctness.

`check_bearer` becomes a constant-time comparison here, and its doc comment stops claiming "constant-time-ish", which is false.

**Tests.** `revoke_prunes_the_token`. `reclaim_prunes_the_generation`. **`a_pinned_generations_projection_is_not_pruned_by_a_swap`** — catches the wrong coupling. **`an_undersized_bound_does_not_livelock`** — bound below working set, N sessions round-robin, assert a bounded `ProjectionBuilding` rate and forward progress; *the bound holding is not the risk, the bound biting is*. `an_evicted_then_rebuilt_projection_is_byte_identical` and `eviction_never_widens_a_mask` — a cache whose miss path is more permissive than its hit path is a disclosure with a performance explanation.

---

## Track T — the standing conformance track

Scoped far enough to start; it gets its own plan when 2.2 begins.

**Now:** the adversarial mask catalogue — empty, single item, ~0.01% coverage, 100%, straddling the ~5% crossover, **container-boundary masks via deliberately sparse ID allocation** (at 10⁴ dense IDs no mask straddles a 2¹⁶ boundary, so it must be constructed), all-in-one-tile, overlay-heavy. Plus `fx_key` — a unique planted declared scalar per fixture item, served in the points batch, giving a legitimate handle→item join with no reverse map and no I10 tension. Plus the four canary allocation rules **with a fifth added at review: canaries are allocated in their own commit window**, since window-scoped signature sort otherwise sorts "IDs after all real IDs" into the middle. Plus the `AckedJournal` recording only 200'd operations.

**Then:** the §7.2 definition in [viewport.py](reference/oracle/viewport.py) as a *definition* — a literal sort by `tessera_id`, a literal count below `P_d`, a literal take of `m`; no bitmaps, no heaps, no fast paths — and the I7 differential over the catalogue × depths × `k`, with the negative control proving it disagrees with a first-k stub.

**As 2.1 lands:** extend the I1 differential to randomised acked-journal states including window-held batches, and add group commit to the I9 allocator fuzz.

---

## Process

**Three ledgers, one fold.** Each track keeps `.superpowers/sdd/2026-07-31-phase2-stage1/progress-<track>.md` in its worktree, append-only in the established format. At each track's merge the controller folds its entries into `progress.md` on `main`. Deferred minors carry forward under the existing convention; the ~40 Phase 1 deferrals stay where they are except the eight this stage absorbs, which are marked closed with their commit.

**Task briefs.** Each task gets a `task-<n>-brief.md` with a signature-level Interfaces block before dispatch — the Phase 1 method, which averaged one fix round per task *with* that block. The unstated decisions this review caught (the executor's crate, the `Command` shape) are exactly what a worker invents when the block is missing.

### The review gate — three lenses per track, before merge

**This plan was itself reviewed this way, and the split earned its cost:** three reviewers in independent contexts, one lens each, found one disclosure path, two designs that would not hold at 10⁹, and three defects that would have stranded a worker — with almost no overlap between them. A single reviewer would have found a fraction. The same gate applies to the code.

**When.** Task 0 before the tracks are cut (it blocks everything, and it is reviewed *as a refactor* — the question is whether behaviour is genuinely unchanged, not whether the design is good). Then each track when its work is complete, before its merge. The same protocol carries into stages 2.2–2.4.

**Three reviewers, dispatched in parallel, each with its own context:**

| Lens | Model | Reviews for |
|---|---|---|
| **Security** | Fable | Fail-open paths the diff creates or leaves; the invariants that track bears, walked concretely against the code rather than the plan's claims; new disclosure channels and whether any widens Appendix C; whether the plan's "structural, not disciplinary" claims are actually structural |
| **Performance** | Fable | Whether it holds at 10⁹ against the *measured* evidence in [probes/](probes/) and the hot-path memo — not against intuition; contention introduced on the request path; memory behaviour under the drain list, the caches and the buffer clone; whether the serving gates would catch a regression this diff could cause |
| **Quality and maintainability** | Opus | Whether a future reader can follow the invariant reasoning without the plan in hand; module boundaries and whether the seams held; error types and failure-path completeness; doc comments that *argue* rather than restate the signature; test quality — do the named tests actually distinguish the failure they claim to, and can any of them pass vacuously; dead abstractions, naming, and CLAUDE.md's "keep modules readable in isolation" |

**What each gets:** the branch diff, the plan's task text, the SDD brief and the worker's report, and explicit paths into `docs/design/` (default tooling skips it). Each is told it has no stake in the work being right, must cite `file:line` rather than assert from memory, must rank CRITICAL / IMPORTANT / MINOR with a concrete failure scenario per finding, and **must say where the work is right when a reviewer would expect otherwise** — a review that only lists problems is not calibrated and cannot be triaged.

**Handling.** The controller triages, not the worker — invariant-bearing decisions stay with the reviewer (CLAUDE.md). Criticals block the merge. Importants are fixed or explicitly deferred to the ledger *with the reason*. Minors go to the ledger. Fix rounds are recorded in the established format (`Track B: fix round 1/5 (2 addressed, 0 open — …; commits A..B)`).

**And verify the reviewers.** The Phase 1 ledger carries several `controller correction:` entries against reviewer findings, and this plan's own three reviews disagreed with each other on Task 1 — the engineering lens called the step-down "the correct reading of the actual protocol" while the security lens correctly identified it as a disclosure path, because only one of them was looking at the deny-specific case. A finding is an argument to check, not a verdict to apply.

---

## Verification

Per task, the named tests. For the stage:

```bash
cargo test --workspace
bash scripts/check-layers.sh                             # layering, the Task 2 marker, no viewport-path counter read
bash scripts/check-track-allowlist.sh                    # file ownership
reference/.venv/bin/pytest reference/tests -v
reference/.venv/bin/pytest conformance/ -v
```

**Serving gates — new at review (perf I2: r1's verification could have passed while serving regressed 2×).** The stage touches the hot path in three places (the seam's pin and cache indirection, `PinManager::resolve` on every request, LRU touch and eviction inside the single-flight lock):

- Fixed-viewport 1e9 A/B at the k=500 operating point against the 2026-07-31 campaign figures (123.3 ms p50 post-B9). **Gate: p50 within 5%.**
- One `scripts/bench_concurrency.py` arm — LRU-under-lock is precisely the contention class the branch's F4 measurement exists to police.
- `cargo bench -p tessera-engine` against rebuilt fixtures (G4), **through `scripts/bench-slot.sh`**.

**Every benchmark runs under the slot** *(controller, 2026-07-31, on Track C's fix-round process finding)*. Parallel tracks in parallel worktrees mean a criterion run competes with other tracks' builds: Track C's first pairing measured `tile_sweep_k0` at 3.55 ms and `compose` at 99 ns — **+50% and +43%** — at load average 12.1 with three tracks compiling. It discarded that run, but only because it thought to look at the load; the instruction "benches must run alone" is not a mechanism, and the viewport-bench regression memo records how expensive a misattributed measurement is to unpick after the fact. `scripts/bench-slot.sh` takes an exclusive `flock` across every worktree and then waits for two consecutive quiet load readings, so the common failure — measuring through someone else's build — has to be worked around rather than merely not noticed. A run that gives up waiting says so on stderr and demands the fact be recorded with the numbers.

**The criterion gate cannot police lock contention, and the Task 4 gate proved it** *(controller, 2026-07-31, on Track C's performance lens)*. The gated benches are **single-threaded, closed-loop, one session, and never present a pin**. An unconditional uncontended mutex added to `PinManager::resolve` would cost ~15–25 ns against a 2.3–4.0 ms request — ~0.001%, invisible under this box's documented ±1–3% criterion CIs. So the rule the gate exists to enforce (no drain lock on the common path) is enforced by criterion *not at all*; what actually caught it on Task 4 was the worker's own `drain_locks` counter test, whose coverage is exactly one lock type.

Two consequences, binding from Task 5 onward:

1. **Any task that adds or moves a lock on the request path must run a paired `scripts/bench_concurrency.py` arm at admission width, not criterion alone**, and report both. Task 5 is the immediate case: it puts LRU touch and eviction inside the single-flight lock, which is the same class one layer down, and its `drain_locks`-equivalent does not exist.
2. **A counted choke point is worth more than a bench here.** Track C converted an unmeasurable contention property into a unit-testable one by routing every acquisition through one counted helper. Prefer that construction wherever a task claims a path is lock-free — it is the only form of the claim that survives a refactor, and CLAUDE.md's "structural, not disciplinary" test is exactly this distinction.

End to end, against a built bundle:

1. `tessera serve`; authorise; viewport — baseline.
2. Four small ingest batches in quick succession → confirm from `/control/status` they were **one** window (one fsync, one `overlay_version` bump) and the IDs are signature-sorted across all four.
3. `suppress` while ingest is in flight → the 200 arrives only after the item is invisible to a viewport issued immediately after.
4. Fill the queue: ingest → 429 with `retry_after_s`; changes → 200 in the same state.
5. Pin, suppress, re-issue the pinned request → same geometry, item gone.
6. `/control/status` `fragmentation` with the window enabled and disabled, **recorded against the full-sort ceiling**. Stage 2.1's headline number.
7. `kill -9` after an observed 200 → every acked deny survived; repeat with the WAL truncated to the last fsync offset → no *acked* operation missing.
8. Hand-edit a side-manifest to carry a `deny` entry → the partition is **not ready**; change it to `deltas` only → it steps down and serves.

---

## Sizing

| Track | Tasks | Size |
|---|---|---|
| Task 0 (serial) | 0 | M–L (larger than r1's estimate: two architecture decisions, the test-file split, the allowlist) |
| A | 1, 2 | S + S |
| B | 3a, 3b, 6, 7a, 7b, 8, 9, 10 | M, M, M, M, M, M, M, M |
| C | 4, 5 | L, M |
| T | standing | M ongoing |

Critical path is Task 0 → B: **32–45 working days, ≈ 6.5–9 weeks** — r1 said 5–6 and was wrong; A and C fit inside it. Splitting r1's two L tasks into four M ones is the review's granularity fix: at L, Task 3 bundled the thread, two queues, liveness, readiness wiring, handler migration and four tests needing plumbing that did not exist — more than the house method reviews in one sitting.

---

## Open questions raised to the owner (do NOT resolve silently)

**O1 — "a 409 batch had no effect" against a held window (Task 8).** If `B` is *held* and a retry arrives with different bytes, the retry 409s — but does "no effect" reach the held original (discarding an already-accepted in-flight batch) or only the retry? The plan implements "the held original still applies". Confirm.

**O2 — the streamed segment's `permutation.bin` indexing base (blocks 2.2; raised now for lead time).** Contracts §2.6 says a streamed segment's permutation is "bounded by its own `entity_hi − entity_lo`", implying relative indexing. But the `TSPM` header has **no base field**, and §2.6's other sentence defines `bound` absolutely. Two readings, one file format. Either `reserved` becomes a base, `bound` is reinterpreted per segment kind, or a `version = 2` header lands. **A contracts amendment, not an implementation choice.**

**O3 — multi-segment and the §8.5 cache keys (2.2).** With N segments there are N row spaces and therefore N projections, so the key must gain `seg_id`. Neither §8.5 nor lifecycle §1.2 says so. Task 5 leaves the key as-is; 2.2 widens it.

**O4 — is the `readyz` freshness lag bound a disclosure control? (2.2).** SA §7: performance knobs default, disclosure controls do not. Unbounded step-down lets a replica serve long-deleted items as live. Contracts §7 still carries the default as open. *Task 1's deny/tombstone split (security C-1) reduces the urgency but does not remove it — the deltas-only step-down is still unbounded in time without the gate.*

**O5 — I5 ships unenforced.** Declining `accumulo-access` removes what §10.2 calls "the only mechanism in the project that attacks I5's label half". A Python re-implementation by the same team from the same reading is materially weaker. Accept and record I5's row as *unenforced*, or fund an independent expression evaluator (not scheduled)? Stated so the consequence is chosen rather than inherited.

**O6 — two items §6 does not name but requires.** The executor thread (Phase 1 does WAL work inline) and the pin manager with a drain list (compaction publishes a prefix nothing re-opens; today's equality check cannot outlive a swap). Flagged because they widen §6's stated scope, not because they are optional.

**O7 — Phase 1's exit record.** `phase1-results.md` was never written and the p99 < 10 ms criterion measured a different engine on a since-replaced format. **Deferred to stage close by owner decision (2026-07-31)**, together with the 1e9 baseline it depends on — the 47 GB bundle no longer exists and the disk to rebuild it does not either. Until then stage 2.1 gates on *relative* 2.4 M numbers only, and no track may report that a latency budget is met.

**O8 — the fragmentation counters across a restart.** 2.2 will need persisted per-partition counters to combine base and delta tiers. `SEGMENTS-<n>.json` has no such field and adding one is a format change; the alternative is an out-of-contract local file that does not survive restore-from-bundle. 2.1 derives from live counters only, so not yet blocking — but 2.2 must not discover it late.

**O9 — does cache eviction need an Appendix C row?** *(New — security I-4.)* A byte-bounded shared cache couples principals: viewer A's latency now varies with viewer B's activity and B's projection sizes, a function of B's masked cardinality. Almost certainly inside C14/C15's "activity, not content" reasoning — but Appendix C's own rule is that anything unlisted is a bug, not a trade-off, and this plan is the change that creates the channel. One row, or an owner-signed argument that it falls under an existing entry.

**O10 — the surnames term-cardinality ceiling reaches the serving path.** *(New — perf I3.)* The fragmentation counters need a per-term last-assigned ID, ~2.8 GB at the surnames set's 116.9 M terms — the same scaling that OOM-killed that build. Bound the counter state (sample terms, or cap and report coverage), or accept that the figure is unavailable above some term cardinality and say which.
