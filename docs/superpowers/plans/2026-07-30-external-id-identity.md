# The Boundary Identity — `tessera_id` as a Keyed Bijection

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a keyed, opaque, fixed-width `tessera_id` the identity at the service boundary in both directions; take entity↔external resolution off the viewport path entirely; drop two columns' worth of dead weight from the hot bundle; **redefine `priority` as a 16-bit prefix of that identity, so the sample stops being ordered by permission signature above V ≈ 2×10⁶** (owner decision, 2026-07-30, folded in); then rebuild the 10⁹ bundle once against the final format and re-run Phase 1's exit measurement.

**Architecture.** The wire identity is

```
tessera_id = FPE_k(shard_id: u32 ‖ entity_id: u32) → u64
```

a **keyed 64-bit format-preserving permutation** (a balanced Feistel network under a per-deployment key). It is collision-free by construction, stable across restart with nothing persisted, and a pure function of `(key, shard, entity)`. Three separable moves share one rebuild:

1. `columns.arrow`'s `entity_id: uint64` column becomes `tessera_id: uint64` — width-neutral, and the row already in hand *is* the identity to show, so internal→external needs no lookup at all.
2. The `node_id: uint32` column is deleted: the build writes a billion identical `NODE_NONE` sentinels that nothing reads (owner decision D7).
3. The caller's `external_id` is **retrieved by sidecar lookup on drill-down only** (D3, D4) — not carried inline, not a co-equal identity. Per-session handles are **retired from the viewer plane** (D5).

Because decryption yields `(shard, entity)` directly, the `tessera_id → entity` sidecar direction **does not exist**. Only two directions remain, both off the viewport path: `entity → external_id` (drill-down, per interaction) and `external_id → entity` (control plane).

### The framing: one identifier, sometimes supplied and sometimes derived *(owner Ruling A, 2026-07-29)*

The plan was previously written as *"Tessera always mints a `tessera_id`, and the caller may additionally supply an external ID."* That is a mechanism description masquerading as a model, and it invites two mistakes: treating the external ID as an optional extra rather than the primary key, and treating the derived identity as a second namespace to be reconciled with the first. **The owner's framing, which this plan now states everywhere it states anything about identity:**

> **All data has a single, stable, global identifier. It is the caller's external ID when the caller supplies one, and it is derived by Tessera only when the caller does not. When derived, it simply *is* the `tessera_id` — a pure function of the entity, at zero storage.**

Three consequences follow, and each is a correction to how something downstream was written:

1. **There is one identity, not two.** For an item whose caller supplied `"order-8817-Q3"`, that string is the identity and `tessera_id` is the fixed-width transport encoding the wire is obliged to carry (a variable-length key cannot sit in a hot column — see the arithmetic). For an item whose caller supplied nothing, `tessera_id` *is* the identity, full stop: nothing is stored, nothing is looked up, and the sidecar has no row for it. The `0xFFFFFFFF` locator sentinel is therefore not "missing data" — it is the ordinary case of an item whose identifier is its `tessera_id`.
2. **The sidecar is a *translation* table, not an identity store.** It exists only for the items where two representations of one identity must be mapped between. An item with no caller key needs no entry, and the design must not acquire a code path that manufactures one.
3. **Nothing about the mechanism changes.** The wire still always carries the fixed-width `tessera_id`; drill-down still returns the caller's key where one exists; the control plane still addresses items by the caller's key. This is a restatement that fixes what the plan *says*, and — see the next paragraph — deletes one thing it was going to *do*.

**What the restatement made unnecessary, checked rather than assumed.** The one place the plan stored a *derived* external ID is the build: `crates/tessera-build/src/lib.rs:659` and `crates/tessera-build/src/pipeline.rs:335` synthesise an 8-byte little-endian `external_id` from each item's `source_id`, for a synthetic corpus that has no caller namespace at all. Under this framing **those bytes are not identity — they are corpus scaffolding.** The synthetic corpus's items have no caller-supplied key, so their identity is their `tessera_id`, and a strictly faithful build would write **no extents and no locator**, taking the new bundle from ~47.0 GiB to ~28.4 GiB.

**They are nonetheless kept for Phase 1, deliberately and with the reason recorded**, because they are the only thing at 10⁹ that exercises the two sidecar directions the design is committing to: Task 15 Step 3's pathological all-extents residency measurement (43.3 GiB against 47 GiB) and Task 11's `/control/ingest` dedup have no test article without them. **They are scaffolding, and the plan must say so wherever it sizes them** — the 14.9 GiB of extents and 3.7 GiB of locator at 10⁹ measure *a deployment whose callers all supply 8-byte keys*, not a floor. Two things follow that Task 15's memo must state: a deployment whose callers supply nothing pays **zero** sidecar disk, and the sidecar sizing table below is therefore a function of the deployment, not a property of Tessera. The synthesis stays in the build; it does not become a *requirement* of the format, and no reader should infer from it that Tessera derives external IDs. (It also does not rescue the disk gate — 51.1 + 28.4 still does not fit in 51.1 + 15 — so it changes no decision in Task 14.)

**`tessera_id` is a TRANSPORT identifier, not a durable key** (owner ruling, 2026-07-29). It is stable across *rebuilds* — that is what the per-deployment key buys, and it is why a bookmark or a shared link survives a nightly rebuild. It is **not** stable across **repartitioning or resharding**, because the bijection's input encodes placement and repartitioning moves points. A consumer that persists an identity in its own database persists the **`external_id`** — Goal 3's primary key — and resolves it through the control plane. To make staleness detectable rather than silent, MANIFEST's `identity` carries an **epoch** which is advertised on `/meta` and may be presented on drill-down; see "The identity epoch" below. The churn at a repartitioning is *partial* — only moved points change identity — which is exactly why a signal is needed: without one, a stale identifier does not 404, it silently names a different item.

**Tech Stack:** Rust 2021 (workspace crates), `arrow` 59 (IPC), `memmap2`, `axum` 0.8, `sha2`; Python 3.12 for the test-only reference oracle (`pyarrow`, `pyroaring`, `numpy`, `pytest`).

**Source:** Owner decisions 2026-07-29 (`.superpowers/sdd/2026-07-28-phase1-walking-skeleton/identity-revision-brief.md`, Parts 1–2 and Part 7) and the independent review of the 2026-07-30 draft (six Criticals, Part 5). This revision supersedes that draft's random-minting identity model in full.

---

## Global Constraints

Every task's requirements implicitly include this section.

- **Design corpus lives in `.ignore/`,** which default file-search tooling skips — always pass the path explicitly. **Precedence: architecture design (r20, `.ignore/tessera-architecture-design.md`) > contracts spec (r5, `.ignore/tessera-contracts-spec.md`) > system architecture (r4).** The contracts spec's §0.3 deviations govern **only where the contracts spec and the *system architecture* differ** — CLAUDE.md scopes them that narrowly, and they never override the design. Where this plan needs the design to change, it changes the design explicitly (Task 4), in the design, with an Appendix G entry. Lifecycle design r3 owns WAL/overlay/pin mechanisms. `§n` unprefixed means the architecture design.
- **Every document carries an Appendix R review trail** (system architecture, contracts spec, lifecycle) or an Appendix G revision history (design). Read it before re-litigating a decision. **If this plan and a spec document disagree, STOP and report to the owner.** Do not resolve silently.
- **Never `git add -A` or `git commit -am`.** The working tree carries untracked owner files (`docs/whitepaper/`, `docs/tessera-*.html`, `docs/reference/`, `docs/design-memos/`, `docs/superpowers/specs/`, `.superpowers/`) and in-flight work. **Every commit step in this plan names its paths explicitly.** Leave everything it does not name alone. **Do not commit this plan file.**
- **Disk is at 97% — 15 GiB free on `/`.** Any step that writes at scale runs `df -h /` first. If space is short, **STOP and report to the owner**; never delete anything to make room. **One narrow exception, at one step only:** the owner has ruled (Q5, 2026-07-29) that **Task 14 Step 2 may delete `/tmp/tessera-1e9`** — and only Task 14 Step 2, and only after the Task-2-baseline precondition below is verified to hold. No other task, and no other path, deletes anything.
- **Quality gates at every task boundary that touches Rust**, all clean before committing:
  ```bash
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  bash scripts/check-layers.sh
  ```
  **One named exception, and only one.** The format change crosses a crate boundary that cannot be crossed atomically, so **Tasks 6, 7, 8 and 9 leave the workspace deliberately red** and commit with a *scoped* gate (`cargo clippy -p <crate> …`, `cargo test -p <crate> …`) plus `cargo fmt --all` and `bash scripts/check-layers.sh`, stating the red workspace in the commit message. **Task 10 Step 6 is where `cargo test --workspace` must be green again**, and no task after Task 10 may commit red. If any task *other* than 6–9 finds the workspace red, that is a defect, not this exception.
- **British spelling** (*authorisation*, *serialise*, *behaviour*, *licence*) throughout prose, comments and API text.
- **Rust is the implementation language.** Python is a first-class *consumer* (SDK, supervisor, test-only reference oracle) and never a component: no Python in any request path, in artifact production, or in the trusted computing base.
- **The differential oracle is an independent re-derivation and its independence is the point.** Update `reference/oracle/` **only** where the spec change mandates it. The oracle must implement the Feistel **from the spec text**, never by porting the Rust. Never "fix" the oracle to agree with the engine.

### Invariants this plan touches

- **I10 — entity IDs never cross the trust boundary.** Its *substance* is preserved and strengthened: after Task 6 the gather cannot see an entity ID, because `columns.arrow` no longer stores one. Its *mechanism clause* changes — "clients receive per-session opaque handles instead" becomes "clients receive an opaque `tessera_id`" — and the design text saying so must be amended, not left contradicting the code (Critical C-3, Task 4).
- **I9 — entity IDs append-only, never reused.** Untouched. Signature-sorted assignment (§11.1) is untouched and must stay untouched. **What does change (2026-07-30) is that signature order stops leaking into row order:** the storage sort's final tiebreak was the entity ID, which is permanently signature-sorted, and it becomes the `tessera_id`, which is not. §11.1's assignment is unchanged; what it is *visible through* is.
- **I2 — derived quantities are functions of visible data only.** Untouched, and must stay untouched. No task in this plan changes what is counted, sampled or aggregated.
- **I4 — permissions in entity space, geometry in row space, related only by an explicit permutation.** This plan makes the permutation the *only* entity↔row bridge on the request path, which is what §5.1 always said it was.
- **I7 — sampling after masking.** Its *letter* is untouched: no task changes when the mask is applied. Its **purpose** is what the priority redefinition serves — above V ≈ 2×10⁶ the old sample was ordered by permission signature through the tiebreak, which keeps I7's letter and breaks what it is for. The plan does not change the placeholder first-k sampler; it changes the **storage order that sampler inherits**, which is where the defect lived. See "`priority` becomes a prefix of `tessera_id`" below.
- **C4 (response timing) is `Open` in the leak register.** Critical C-5 raises a *new, strong* per-click channel on `/v1/items` that C4's existing text (viewport time correlating weakly) does not cover. Task 9 removes it structurally and Task 4 records the ruling; C4 itself stays `Open`.

### Owner rulings already taken — do NOT re-open

1. **The wire identity is `tessera_id = FPE_k(shard_id ‖ entity_id)`** — a keyed 64-bit format-preserving permutation (brief Part 7). Not a random mint, not a UUID, not a caller string.
2. **The caller's `external_id` is retrieved by lookup on drill-down, not carried inline** (D3, D4). Goal 3 is relaxed: it is no longer required to be the only external-facing identifier.
3. **Per-session handles are retired from the viewer plane** (D5). Retained for Phase 3 node handles.
4. **`node_id` is dropped from `columns.arrow`** (D7). `NODE_NONE` stays in `tessera-types`; Phase 3 re-adds a node column additively when it acquires a reader.
5. **Entity IDs are `u32`** (D8) — the 4B shard cap makes this exact, and contracts §1 already asserts `< 2³²` for `bundle_format = 1`.
6. **C6 is accepted as the caller's problem** (D1), moving from `Closed` to `Accepted — caller's control`. Register hygiene, not a security escalation; keep the tone proportionate. **I2 is unaffected** — densities, counts and clusters remain masked.
7. **`/v1/items/{tessera_id}` returns an identical `404`** for "no such ID" and "exists but not visible": same status, same error code, same detail string, no branch-dependent logging or metrics.
8. **This change is in scope for Phase 1**, which closes on the *new* format. Phase 1 Task 16 Steps 4–5 are deferred until it lands.
9. **The bundle is built once against the final format.** No intermediate 10⁹ build.
10. **`tessera_id` is a transport identifier, not a durable key** (2026-07-29). Stable across rebuilds; **not** stable across repartitioning or resharding. Consumers persist `external_id`. An **identity epoch** makes staleness detectable.
11. **A build that has made no explicit identity-key decision REFUSES** (review round 2, Critical N-1). Minting is never the default: `--mint-id-key` is a flag an operator must type. A printed warning on a 90-minute build's stdout is not a gate.
12. **`Engine::item` returns `Result<Option<ItemOut>, StoreError>`** (Critical N-3). No `.ok().flatten()` anywhere on that path; a sidecar error is a `500`, not a `404` that reads as "no such item".
13. **The item endpoint's visibility test is in entity space** (review round 2's best find, Q4 option (d)), not through a `RowProjection`. This closes the C-5 timing channel completely rather than narrowing it, and it retires open question 4.
14. **No compressed external-ID store** (brief Part 9). Simplicity wins; recorded as a conditional future option with the `pairs.parquet` precedent.
15. **The identity framing is "one identifier, supplied or derived"** (Ruling A, 2026-07-29). All data has a single stable global identifier: the caller's external ID when supplied, derived by Tessera when not — and when derived it **is** the `tessera_id`, at zero storage. The wire always carries the fixed-width `tessera_id`. See "The framing" above.
16. **The external-ID sidecar is a placeholder for a future adopted metadata store** (Ruling B, 2026-07-29). Minimal, narrow-boundaried, marked transitional; the same slot as §8.3's vector sidecar. Appendix D does not forbid adoption here. See "The external-ID sidecar is a PLACEHOLDER" above.
17. **The drill-down structure is the locator, not a duplicate column** (Q3, 2026-07-29). The owner's reason is to *"build around the more likely real-world scenario of an externally provided ID"*, under which the locator's 4 B/row is **independent of key length** while a duplicate column scales with it. This settles open question 3.
18. **Task 14 MAY delete `/tmp/tessera-1e9`** (Q5, 2026-07-29), **conditional on Task 2 having captured the current-format baseline first** — an enforced precondition, not advice. See Task 14 Step 2.
19. **The identity key lives in a per-deployment config file, `--id-key-file`** (Q6, 2026-07-29) — **not** an environment variable. The wider config file is not designed here.
20. **Contracts §1's external-ID cap tightens to 64 bytes** (Q7, 2026-07-29), and **the identity epoch is advertised on `/meta` and OPTIONAL on `/v1/items`** (Q8, 2026-07-29), as this plan already assumed. Reasoning recorded at the resolved questions below.
21. **`priority` is `high16(tessera_id)`, and the storage sort order is `(morton, tessera_id)`** (owner decision, 2026-07-30, `docs/design-memos/2026-07-30-priority-as-identity-prefix.md`). The standalone `splitmix64`-over-entity priority function is **deleted**; the column, its `u16` width and its 2 B/row are unchanged. **Important I-4 is retired by argument** — `priority` is no longer an unkeyed function of the entity ID, so the viewer-plane prohibition, its layer check and its byte-scanner sweep all lapse. See "`priority` becomes a prefix of `tessera_id`" below.

### What this revision deleted from the previous draft

Recorded so it is not reintroduced by an executor working from a stale memory of the plan:

- **All random-minting machinery is deleted, not adapted**: `mint_public_ids`, the seeded CSPRNG, the `ids: Vec<u64>` and `order: Vec<u32>` allocations, collision detection, the re-mint repair loop, the live-collision map, and MANIFEST `provenance.public_id_seed`. A bijection cannot collide, so there is nothing to detect and nothing to repair. **Critical C-2 dissolves** — the IDs are not persisted because they need not be, and they are stable across restart because the function is pure. **Important I-1's RSS regression disappears with the allocations.**
- **The `tessera_id → entity` sidecar direction is deleted.** Decryption yields `(shard, entity)`. Every size, residency and task consequence in the "Arithmetic" section below is re-derived from scratch; none of the previous draft's sidecar arithmetic is carried forward.
- **Seed threading through `BuildArgs` is deleted.** `build_equivalence.rs` byte-equality gets *simpler*: a pure function of `(key, shard, entity)` produces identical bytes on both build paths with nothing threaded but the key already in MANIFEST.
- **The name `public_id` is deleted** in favour of `tessera_id` (the brief's name), which also settles the previous draft's open question 2. `external_id` keeps meaning what contracts §1 and SA D14 already say it means: the caller's byte string.

### What review round 2 changed (2026-07-30) — read this if you hold an earlier copy

Round 2's verdict was **sound with fixes**: of round 1's six Criticals, C-1, C-3, C-4 and C-6 were confirmed fixed, **C-2 was confirmed dissolved** (no residual random-mint machinery survives anywhere), and **C-5 was fixed in substance but overclaimed in spec** — which this revision resolves by adopting the reviewer's own better construction. The Feistel was independently **verified invertible** and the ruling is to keep it unchanged. The arithmetic now closes against the measured bundle in both directions. What moved:

| | change |
|---|---|
| **N-1** | A build with **no** explicit identity-key decision **REFUSES**. Minting was the default and the safe path was a flag; that is now inverted, with a test asserting non-zero exit *and* an empty output directory. |
| **N-2** | New leak-register entry **C17** — stable wire identity across sessions and principals — cross-referenced from §10.6. Accepted (it is the point of D5), but it must be *in the table*. |
| **N-3** | `Engine::item` returns `Result<Option<ItemOut>, StoreError>`. The specified `.ok().flatten()` is deleted: `Err` → `500`, `Ok(None)` → the identical `404`. |
| **Q4 → design** | The item endpoint's visibility test moves to **entity space**, per the reviewer's option (d). O(1), no `RowProjection`, identical work for an unknown ID and an invisible one. **C-5 closed rather than narrowed**; the "404s everything until a viewport has been drawn" behaviour is gone; open question 4 is retired. |
| **I-1** | `Allocator::allocate` is capped at `u32::MAX` and `forward` takes a **checked conversion**. "Collision-free by construction" previously rested on nothing enforced. |
| **I-3** | The control-plane principal obtains chosen-plaintext pairs **by construction** and is outside the defended set. Stated, because the round-function ruling depends on it. |
| **I-4** | `priority` — an *unkeyed* `splitmix64` of the entity ID — is **forbidden on the viewer plane**, in the layer check and the byte-scanner. **⚠ RETIRED 2026-07-30** by the owner's priority-as-prefix decision, which removes the premise (`priority` is no longer unkeyed). Round 2's finding was correct on round 2's definition; see "What the priority-as-prefix decision changed" below. |
| **I-7** | The locator is **singular**: `ext-locator.u32`, one file. The two readings differed by 33 GiB. |
| **I-8 / I-9** | Ingest dedup consults `Engine::established` (post-build duplicates); drill-down for post-build entities is specified rather than returning a `None` that reads as "no external ID". |
| **owner, post-review** | `tessera_id` is a **transport** identifier with an **identity epoch**; the partition-vs-shard ambiguity is **resolved** against the code and §12.4. |
| **Part 9** | **No compression.** Recorded as a conditional future option with the `pairs.parquet` precedent; contracts §1's 256-byte cap addressed. |
| **arithmetic** | `terms/`'s `du` moves to **Task 2** (the disk gate depends on it); the pathological all-extents residency case is stated at 43.3 GiB. |

### What the owner's answers changed (2026-07-29) — read this if you hold the round-2 copy

All nine open questions are now closed and two rulings landed. **No mechanism changed**; what changed is framing, one CLI flag, one contract number, and one enforced ordering.

| | change |
|---|---|
| **Ruling A** | **One identity, supplied or derived.** The plan no longer reads "always mint a `tessera_id`, plus optionally a caller external ID". All data has a single stable global identifier: the caller's external ID when supplied, the `tessera_id` when not — and when derived it *is* the `tessera_id`, at **zero storage**. Restated in the header, the three-identifiers table, "What remains in sidecars", contracts §2.4 and Tasks 3, 8 and 15. **What it revealed:** the build's synthesis of `external_id` from `source_id` stores no identity — it is **corpus scaffolding**, kept only as a 10⁹ test article, and every sidecar figure is now labelled as a property of the deployment rather than a floor. |
| **Ruling B** | **The sidecar is a PLACEHOLDER** for a future adopted metadata store, applied as a *design constraint*: nothing clever, a two-operation boundary the storage cannot leak past, marked transitional at the type and in the spec. Recorded as the same slot as §8.3's vector sidecar and the routing principle's per-interaction row, and that **Appendix D does not forbid adoption here** — it rejects adoption for the *access-control layer*, and a cold store that never participates in masking is a different question. Any replacement inherits fail-closed typed errors, off-the-request-path, and integrity-before-answer. |
| **Q3** | Locator **confirmed**, on a stronger reason than the plan's own: build for the real-world externally-provided ID, under which the locator is 4 B/row *independent of key length* while a duplicate column scales with it. |
| **Q5** | Task 14 **may** delete `/tmp/tessera-1e9`, conditional on Task 2's baseline — now an **enforced precondition check** at Task 14 Step 2a, with Task 2's results JSON schema as its contract, not an advisory ordering. A REFUTED verdict does not carry the authorisation. |
| **Q6** | `--id-key-file` — a **per-deployment config file**, not an environment variable, with **no default search path** so N-1 survives. The wider config file is direction, not design. |
| **Q7** | Contracts §1's external-ID cap **tightens from 256 to 64 bytes**; over-length is a typed error, never a truncation. |
| **Q8** | Epoch **advertised on `/meta`, optional on `/v1/items`** — as assumed, with the owner's reasoning now recorded in §2.2 so "optional" does not read as an oversight. |
| **Q9** | Confirmed as written. |
| **sizing** | The sidecar sizing table across key lengths and **the ~16-byte threshold** at which the store dominates the bundle now sit in the arithmetic section and in Task 15's memo, so the replacement is scheduled by evidence. Phase 1 builds neither compression nor replacement. |

### What the priority-as-prefix decision changed (2026-07-30) — read this if you hold a pre-fold copy

Source: `docs/design-memos/2026-07-30-priority-as-identity-prefix.md`, an owner decision folded into this plan after Tasks 1–3 had been executed and committed. **No identity mechanism changes** — the Feistel, the key, the epoch, the sidecars and every arithmetic figure in this plan are untouched, and the bundle changes zero bytes. What changes is one column's *definition*, the storage sort order, and one prohibition that loses its premise.

| | change |
|---|---|
| **`priority`** | Redefined from `high16(splitmix64(entity_id))` to **`high16(tessera_id)`**. Same column, same `u16`, same 2 B/row, same physical position. The standalone priority function is **deleted**, not kept alongside. |
| **sort order** | `(morton, priority, entity_id)` → **`(morton, tessera_id)`**. Stated in full below; this is the single most load-bearing edit in the fold, because the oracle re-derives row order from build inputs. |
| **Important I-4** | **Retired.** The prohibition rested entirely on `priority` being an *unkeyed* residue of the entity ID. It is now 16 bits of an identity the wire already carries in full. The layer-check grep and the byte-scanner sweep go with it. |
| **row order is now key-dependent** | It was not before. A re-key changes which row lands where inside a Morton cell, and therefore reshuffles the sample. Recorded as an accepted residual below. |
| **tasks touched** | Task 4 (spec text, plus amending Task 3's committed memo), Task 5 (`TesseraId::priority()` — the single definition), Task 6 (`sort_batch`, `TilerItem`, the writers), Task 7 (delete `priority_of`, the `RowRec` comparator, the chi-squared check), Task 9 (one consequence note), Task 10 (drop the layer-check grep), Task 12 (oracle), Task 13 (drop the sweep). **No new task, and the whole fold must land before Task 14's rebuild** — it changes the storage sort order, so landing it afterwards means rebuilding twice. |

---

## The routing principle — where per-point capability lives

**This is cross-cutting owner guidance (brief Part 3), not a fact about this change.** It binds every future per-point metadata, filter, text and vector feature, and it is recorded here so the next such feature does not re-derive it badly. Task 4 lands it in the design.

| role | home | mechanism | examples |
|---|---|---|---|
| per **rendered mark** | hot fixed-width column | `columns.arrow` declared scalars | colour, importance, declared scalars — and `priority`, a 16-bit prefix of the row's own `tessera_id` (below) |
| per **query** | entity-space bitmap | the filter contract, §8.2 | labels, text match, vector threshold |
| per **interaction** | cold sidecar keyed by wire ID | drill-down fetch | external ID, full record, provenance |

**Route by access ratio, not data type.** A viewport draws ~143,000 marks; a user clicks a handful. Anything read once per interaction belongs four orders of magnitude away from anything read once per mark. Design §10.3 already states that fixed-width hot columns carry "entity ID, x, y, cluster node ID, priority and per-item scalars" and that "neither text nor high-dimensional vectors appear here; both live outside the hot path (§8.3)". This principle is that statement generalised.

Constraints binding any future metadata work, from §8.2 — quoted because each has a failure mode that looks like a feature:

- Every filter returns an entity-ID bitmap; composition is intersection only.
- **Threshold, never top-k.** A top-k filter evaluated before intersection is post-filtering, and results would vary with what the principal cannot see.
- **The mask goes in first, not last.**
- **Pre-intersection cardinality must be structurally unreachable** (C8).
- The filterable vocabulary must itself be containment-filtered (C11).

**`priority` is a 16-bit prefix of the row's `tessera_id`** *(owner decision, 2026-07-30)*. It is in `columns.arrow` because the storage sort needs it at build, because a future sampler reads it server-side, and because a cheap prefix must be **physically contiguous** — a strided read of the high 2 bytes of a `uint64` array touches every page holding any value (512 `u64` per page against 2,048 `u16`) and pulls the whole cache line regardless, which is §10.4's column-major argument one level down.

**The general rule stands and `priority` now satisfies it: a hot column may be shown only if it is independent of the entity ID, or keyed under the deployment key.** Any future per-mark column derived from the entity ID by an *unkeyed* function inherits the prohibition that `priority` has just been released from.

> **Historical note — Important I-4, retired 2026-07-30, and why a future reader must not reinstate it by reflex.** Review round 2 forbade `priority` on the viewer plane, in the contracts spec, in `scripts/check-layers.sh` and in the conformance byte-scanner. That finding was **correct on the definition it was given**: `priority` was then `high16(splitmix64(entity_id))`, an *unkeyed* 16-bit residue of the entity ID, so publishing it handed a viewer a 65,536× narrowing of entity space per mark, computable offline and combinable across the ~143,000 marks of one viewport — I10 defeated by a sort key. The owner's redefinition removes the premise rather than weakening the rule: `priority` is now 16 bits of a **keyed** identity that the same payload already carries in full, so a viewer learns nothing from it that the `tessera_id` column does not already tell them. A viewer holding *k* marks learns only that their priorities fall below some cut *P*, and *P* is fully determined by *k* and the exact masked count *V* that §7.1 already gives them; nothing about unseen items is recoverable, and no leak-register entry is required. **I-4 was retired by argument, not weakened by convenience.** Reinstating the prohibition without first re-establishing that `priority` is unkeyed would be reinstating a guard against a threat that no longer exists.

**Owner's forward note.** Expanding the core columnar store is an explicitly available trade — more per-point data in the hot path, paid for in resident memory. It is a **deliberate option with a stated cost**, not something forbidden. The cost is exact and computable: one byte per row per 10⁹ items is 0.93 GiB of resident set, read on every viewport. Any proposal to add a hot column should state that number and argue it against the residency budget in Appendix A.

---

## `priority` becomes a prefix of `tessera_id` *(owner decision, 2026-07-30)*

**Source:** `docs/design-memos/2026-07-30-priority-as-identity-prefix.md`. **Deadline:** this fold must land before Task 14 rebuilds the 10⁹ bundle, because it changes the storage sort order; landing it afterwards means rebuilding twice. **Cost:** zero bytes in the bundle, zero bytes on the wire, no change to any figure in the Arithmetic section.

### The two defects being fixed

Reproduced rather than summarised, because both are the kind of finding a later reader will try to re-derive.

**1. Above V ≈ 2×10⁶ the sampler orders by permission signature.** `priority` was `high16(splitmix64(entity_id))` — a `u16`, so 65,536 distinct values. For a tile with **V** visible items the *k*-th lowest priority sits at ≈ `k·2¹⁶/V`, which is a resolvable value only when **V ≤ 2¹⁶·k**. At *k*=30 that threshold is **V ≈ 2×10⁶**. Above it every candidate carries the same priority and the **tiebreak becomes the sampler** — and the tiebreak was `entity_id`: storage order was `(morton, priority, entity_id)`, direct evaluation keeps the *k* lowest out of a partial sort, so equal priorities resolved by row order, and entity IDs are **signature-sorted, permanently, under I9** (§11.1).

So a principal whose visible set spans groups A and B, where A was allocated lower entity IDs, sees mostly A at coarse zoom even if B is ten times larger. **This keeps the letter of I7 and breaks its purpose.** §7.2 rejects global LOD sampling because *"a principal authorised for a small or clustered slice would see a nearly empty screen while thousands of authorised items sat invisible beneath a sample that selected around them"* — this is that failure moved *inside* the visible set, and it correlates with permissions precisely **because the entity allocator was made permission-aware**.

It bites **head principals, not tail**: at 0.01% coverage V at depth 0 is 10⁵ and the cut resolves; at 25% coverage V is 2.5×10⁸ and the sample is tie-dominated from roughly depth 3 upward. Candidate lists inherit it — they are built as "top c·*k* by priority", tie-broken the same way (§7.2). Large *k* *helps* (V ≤ 2¹⁶·k), so **the binding case is the default overview at k≈30 — the first screen a user sees.**

**2. Shard-local `u32` entity IDs break §12.3's composition argument.** D8 makes `entity_id` a shard-local `u32`, so `splitmix64(entity_id)` is no longer the *"global per-item property"* on which §12.3's *"priority sampling composes exactly"* depends. Item 12,345 carries an identical priority in **every** shard, and `(priority, shard_id, entity_id)` as a global order makes shard 0 win every tie. `high16(tessera_id)` restores the property: the shard is part of the bijection's input, so the value is global by construction.

### The decision

**`priority` is defined as `high16(tessera_id)`** — the top 16 bits of the `u64`, i.e. `(tessera_id >> 48) as u16`. Same column, same `u16` width, same 2 B/row hot read, same physical position in `columns.arrow`. The standalone `splitmix64`-over-entity construction is **deleted**, not kept alongside.

### The sort and tiebreak statement — normative

This paragraph replaces every "the sort tiebreak does not move" statement in earlier copies of this plan, in Task 3's committed memo, and in `crates/tessera-spatial/src/tiler.rs`'s doc comments. It is normative and an executor must not paraphrase it:

> **The storage sort order is `(morton, tessera_id)` ascending, and it needs no further tiebreak.**
>
> `tessera_id` is a keyed bijection over 2⁶⁴ and there is exactly one row per entity, so within a build no two rows share a `tessera_id` and the order is **total**. Because `priority` is a *prefix* of `tessera_id`, "order by `(morton, priority, tessera_id)`" is **identically** "order by `(morton, tessera_id)`" — there is no composite comparator to get subtly wrong, and an implementation may compare the 16-bit prefix first purely as an optimisation, provided a test asserts the two agree. `entity_id` is **not** a sort key at any position. `priority` is **not** an independent sort key: it is a cached prefix, and it must be derived at exactly one place from the `tessera_id` it is a prefix of.
>
> **What the oracle must re-derive:** row order as `(morton_of(x, y, extent), forward(identity.key, identity.shard_id, entity_id))` ascending. **Row order is therefore key-dependent, where it previously was not** — the oracle reads `identity` from MANIFEST for `identity.py` already, so this adds a dependency and no new artifact. A different key gives a different order within a Morton cell; that is intended (see the residuals) and is not a defect for `build_equivalence.rs`, which passes one key to both build paths.
>
> **The one condition under which an explicit tiebreak returns:** if a future format ever stores more than one row per entity, `tessera_id` stops being unique per row and the order stops being total. Nothing in Phase 1 does this — `permutation.bin`'s `entity_to_row` is a bijection — but a Phase 3 or later change that breaks it must supply a tiebreak explicitly rather than inherit an unspecified one.

### The build sequence inverts: `tessera_id` is computed BEFORE the sort, not written at the row after it

**This is the consequence most likely to produce a wrong implementation if left vague, and it is not in the source memo's change list** (it was raised by the independent gate on Task 3's memo, 2026-07-30). Until now `tessera_id` was *"a column value written at the row, not a sort key"* — the pipeline could allocate entity IDs, sort by `(morton, priority, entity_id)`, and derive the identity afterwards in row order. Under `priority = high16(tessera_id)` **a function of `tessera_id` is the sort key**, so the identity must exist before the tiler runs. The sequence, for **both** build paths:

1. Allocate entity IDs — signature-sorted assignment, §11.1, **unchanged**.
2. **`tessera_id = forward(identity.key, identity.shard_id, entity_id)` for every item — before the tiler.** Fallible (Important I-1): a checked conversion, collected into a `Result`.
3. Morton codes from `x`, `y` against the extent — unchanged.
4. **Sort by `(morton, tessera_id)`**, permuting the companion `entity_ids` vector identically. That vector is still needed — for `permutation.bin`'s `entity_to_row` and for the external-ID extents and locator — but it is a **companion, not a sort key**, which is a different reason from the one Task 6 previously gave for passing it.
5. Derive the `priority` column as `(tessera_id >> 48) as u16` **over the already-sorted `tessera_id` vector**. It is a projection of a column that is already in hand, computed at exactly one place, never an independent hash.
6. Write `columns.arrow` as `(tessera_id, x, y, priority, …declared scalars)`.

**The streaming pipeline's compact sort record needs a decision, and this plan makes it.** `pipeline.rs`'s `RowRec` is a deliberately 12-byte `#[repr(C)]` record (`morton: u32, entity: u32, priority: u16, _pad: u16`) because it is sorted at 10⁹ scale. A full `tessera_id` in it would make it 16 bytes — **+3.7 GiB at 10⁹, at the build's tightest moment, immediately re-spending what dropping the `NODE_NONE` column just freed.** Instead: **`RowRec` keeps its 12 bytes and its `priority` prefix, and the comparator recomputes `forward(shard_id, entity)` only when `(morton, priority)` ties.** `forward` is a pure function of the stored `entity`, so this is exact, and it costs eight `splitmix64` rounds on the fall-through path only. Task 6 must carry a test asserting this comparator agrees with a naive full-`tessera_id` sort over a small batch containing engineered prefix ties. **If the tie path shows up in the build profile at 10⁹, the escalation is the 16-byte record and its 3.7 GiB — report the measurement to the owner rather than choosing silently.** The in-memory reference path has no such trade: `TilerItem` carries the `tessera_id` itself, so its comparator reads it directly.

**Why prefix width is a performance knob and nothing more.** Fall-through volume — the rows whose 16-bit prefixes tie and which a comparator must resolve on the full `u64` — is ≈ V/2^w, which at 10⁹ is a few thousand scattered 8 B reads against a prefix scan of hundreds of megabytes. The sample is *correct* at any prefix width, because the prefix is a prefix. **In Phase 1 this arithmetic is a build-time comparator concern only:** the placeholder first-k sampler takes rows in storage order and never reads the priority column at query time, so no executor should build a runtime prefix-scan-then-fall-through path in this plan.

### What must be verified, not assumed *(carried from the memo, unresolved)*

The high 32 bits of `tessera_id` are the left Feistel half `L`, and this plan is explicit that the construction is **a blinding permutation, not a cipher**. Sampling needs **uniformity, not unpredictability**, and 8 balanced rounds of `splitmix64` deliver it — **but the inputs are highly structured** (`shard_id = 0`, `entity_id` dense from 0), and residual structure in `L` would be inherited by the sample, which is the exact defect being fixed. **Task 7 adds a chi-squared check over `high16(tessera_id)` alongside the existing known-answer vectors in `build_equivalence`.** This is an open verification item, not a stated fact: if the check fails, the fold does not land and the owner is told.

### The confirming measurement — PENDING, do not run it here

The memo's own confirmation is *"over the existing 10⁹ k-sweep, count the (tile, principal) pairs with V > 2×10⁶. No new corpus needed."* **That measurement is being run concurrently by another agent and is not this plan's work.** Two consequences an executor must respect:

1. It reads the **existing** `/tmp/tessera-1e9` bundle, so it must be complete before **Task 14 Step 2** deletes it. Add it to the things Task 14's precondition check confirms are already recorded.
2. The redefinition is justified by the argument above independently of the count — the count sizes the defect, it does not establish it. Do not treat a pending measurement as a gate on the fold, and do not restate its result before it exists.

### Residuals, recorded not hidden *(carried from the memo)*

- **The sample reshuffles on a re-key as well as on a reshard.** Before this change it reshuffled on reshard only, since `entity_id` changed. Re-key is rare and operator-initiated, and a reshard already bumps the identity epoch. **Accepted.**
- **Key rotation's blast radius widens from identifiers to row order** *(raised by the gate on Task 3's memo)*. With a key-dependent sort key, a rotation **reorders tied rows**, so `columns.arrow` row IDs are no longer invariant under rotation: a rebuild under a new key produces a different `permutation.bin` as well as different identifiers. `build_equivalence.rs`'s determinism claim is untouched — the build is still a pure function of `(key, shard_id, entity_id)` — but "rotation invalidates every outstanding identifier" becomes "rotation invalidates every outstanding identifier **and changes row order**". Rotation was already a breaking change and remains one; this widens what breaks, not how often. **Accepted, and it must be stated wherever rotation is described** (the `--rotate-id-key` flag row and the threat model's rotation bullet).
- **Prefix width may need revisiting at 10¹²⁺.** If the coarse-zoom path remains a live scan rather than a session-established summary, fall-through at zoom 0 grows to ~1.5×10⁸ rows. The trigger is `w ≈ log₂(V_max/k)` — about 24 bits for a 10⁹ shard at head coverage. **Record the trigger; do not pay for it now.**
- **Not addressed:** sample stability across resharding. Hashing the durable `external_id` would give it, but that key is absent for items whose identity *is* their `tessera_id` (Ruling A), so stability would be mixed. More machinery than the rare event warrants.

### What this does NOT change

- The Feistel construction, its key, its schedule, its round count, the epoch, the CLI flags, the N-1 refusal, the allocator cap, `forward`'s checked conversion.
- Any figure in the Arithmetic section. `columns.arrow` stays 18 B/row; `priority` stays `u16`. **The bundle changes zero bytes.**
- The placeholder first-k sampler, the mask-build path, or anything in `tessera-authz` (still out of scope) — only the **storage order it reads in** changes.
- §11.1's signature-sorted entity-ID assignment, which is permanent under I9.

---

## The external-ID sidecar is a PLACEHOLDER — a design constraint, not a note *(owner Ruling B, 2026-07-29)*

> Owner: *"once we're handling metadata more consistently, we'll need to consider how we handle additional fields in any case (I'm hoping by adopting an existing technology rather than rolling our own), so consider external ID lookup a placeholder for a future more capable data store sidecar."*

This is not a caveat to be recorded and forgotten. It **binds the design of Tasks 8, 9 and 11** and it changes what "good" looks like for them: the target is not the best external-ID store that can be built, it is the smallest one that satisfies its two callers and can be **taken out** without touching anything else.

**Four constraints, each checkable at review:**

1. **Do not over-invest.** The sidecar satisfies exactly two callers — the `/v1/items` drill-down (`entity → external_id`) and the control plane (`external_id → entity`). **No compression, no block format, no clever index, no caching layer, no statistics.** Brief Part 9 already declined compression on its own arithmetic; Ruling B declines it a second time on a stronger ground — it would be machinery invested in a component scheduled for replacement. Sorted Arrow extents plus a `u32` locator is the whole design and it should stay the whole design.
2. **The boundary is narrow and swappable.** Everything the rest of the system knows about external IDs is **two operations**: `resolve(external_id) -> Result<Option<EntityId>>` and `external_id_of(entity) -> Result<Option<Vec<u8>>>`. Task 8 puts both behind one module surface (`crates/tessera-store/src/sidecar.rs`) with the storage format entirely behind it — no extent descriptor, no locator index, no Arrow type, and no file path leaking into `tessera-engine` or `tessera-server`. The test of whether this held: **replacing the storage must be a change to one file plus its constructor call.** If a reviewer finds a second module that knows the sidecar is sorted, or is Arrow, or is per-extent, that is a defect against this ruling even though it is not a defect against any invariant.
3. **It is marked transitional in the plan and in the spec.** Task 4's §2.4 text carries the marker, so a later contributor extends the replacement rather than this. A future contributor who reads the spec and finds a well-specified store with no successor note will build on it, and Ruling B will have been recorded only where nobody looks.
4. **It is the same slot as §8.3's vector sidecar and the routing principle's per-interaction row.** §8.3 already says a vector index is *"a sidecar in a different format… this is where chunked object-store-native storage earns its place"*; the routing principle's third row is *"per interaction → cold sidecar keyed by wire ID"*. The external ID is the **first** occupant of that slot, not a special case, and the future store serves all of it: per-point metadata, the full record, provenance, and eventually text and vectors. Task 3's memo and Task 4's §10.3 text say so, so the replacement is scoped once rather than three times.

**Appendix D does not forbid adopting an external store here, and the distinction must be recorded before someone cites it wrongly.** Appendix D rejects adopting a search engine, a vector database or a relationship-based authorisation service **for the access-control layer**, on correctness-before-performance grounds: those systems would sit inside masking, where a wrong or stale answer is a disclosure. A **cold per-point metadata store that never participates in masking** is a different thing — it is read after the visibility test has already returned "visible" (Task 9's ordering is explicit about this and is load-bearing for C-5), so it cannot influence what is visible, only what is displayed about something already visible. Task 4 records the distinction in contracts §2.4 so a future reader reaches for Appendix D and finds it already answered.

**What any adopted replacement must still satisfy** — these do *not* relax, and they are the acceptance criteria for the eventual swap:

- **Fail-closed with typed errors.** Every failure mode — missing, corrupt, digest mismatch, unreachable, timed out — is a typed error. **Never a `None` that reads as "no external ID"**, because a corrupt mapping suppresses the **wrong item** on `/control/changes`. This is the same rule as Critical N-3 and the Task 8 error contract, and an adopted store makes it *harder*, not easier: a network store adds "unavailable" as a failure mode that a local mmap does not have, and "unavailable" must not degrade to "unknown".
- **Off the request path (§10.3).** Per-interaction, never per-mark. An adopted store that acquires a viewport-path caller has left this slot and is back inside Appendix D's argument.
- **Integrity is verified before an answer comes out of it**, whatever form that takes for the adopted technology — the local equivalent today is the per-extent digest *and* sortedness check (Critical C-1).

**Out of scope here:** choosing the replacement, or designing it. Phase 1 builds the placeholder and the boundary; the sizing threshold below is what schedules the replacement by evidence.

---

## Baseline — the measurement that motivates this

Measured 2026-07-29 on the built 10⁹ bundle at `/tmp/tessera-1e9`. Bundle 51.1 GB total. Box: 47 GiB RAM, 12 cores, WSL2.

`k` is **per tile**; a viewport is ~300 tiles. All figures server-side.

| k | marks/viewport | p50 | p99 | max | payload |
|---|---|---|---|---|---|
| 30 | — | 4.33 ms | 48.85 ms | — | — |
| 50 | 11,850 | 5.30 ms | 52.71 ms | 57.8 ms | 0.15 MB |
| 500 | 81,260 | 23.86 ms | 66.15 ms | 638 ms | 1.09 MB |
| 1000 | 143,857 | 41.38 ms | 82.44 ms | 919 ms | 2.00 MB |
| 2500 | 298,759 | 67.65 ms | 226.76 ms | 2906 ms | 4.33 MB |
| 5000 | 520,312 | 100.36 ms | 375.14 ms | 421 ms | 7.69 MB |

Boot 176.8 s. Warm-up row projection 9.5–19.3 s, one-off per (token, slice, pin). **The box began swapping during the high-k rows, so k=2500 and k=5000 are contaminated** — do not quote them as measurements of anything but a swapping box.

**Reading.** `p99 − p50` is near-**constant** at ~42–47 ms across k = 50…1000, while p50 scales linearly with marks at ~0.19 µs marginal per point. A fixed tail penalty that does not scale with the work done is the signature of cold major page faults, not of compute. The viewport working set (`columns` 21.1 + `morton` 3.7 + `permutation` 3.7 = 28.5 GiB) plus 18.9 GiB of external-ID extents that the viewport path never touches but which `ExternalIdIndex::load` page-touches in full at `Engine::open` (`crates/tessera-store/src/external_ids.rs:211`, `validate_sorted`) exceeds 47 GiB of RAM.

**This is a HYPOTHESIS, not an established fact.** The warm-versus-cold discrimination run has never been done. **Task 2 does it, before any format work**, and its result is a gate: if the tail survives with the working set fully resident and with the external-ID index unloaded, the residency story is wrong and the owner must be told before a 90-minute rebuild is spent on it.

---

## The premise, and why it is safe

**Compiler-verified, 2026-07-30 (method: rename the accessor, `cargo check --workspace --all-targets`, enumerate every error — not grep).** `ColumnsRef::entity_id()` (`crates/tessera-store/src/read.rs:591`) has exactly three consumers in the entire workspace:

| Consumer | What it does |
|---|---|
| `crates/tessera-engine/src/viewport.rs:113` | `Engine::item` — linear scan of the column to find the row for a given `EntityId` |
| `crates/tessera-engine/src/viewport.rs:314` | `row_to_point` — the gather; reads the row's identity for output |
| `crates/tessera-store/tests/bundle_read.rs:192` | test assertion |

**Nothing else.** Not mask composition, not the overlay/deny path, not the build pipeline, not `tessera-wire`, not `tessera-server`. Mask composition uses the *forward* permutation (`entity_to_row`, `permutation.bin`) projected into row space; contracts §2.6 deviation 2 already calls the column "the row→entity direction". Both production readers are pure row→identity **for output**, so replacing the column's contents with `tessera_id` is structurally clean.

`ColumnsRef::node_id()` (`read.rs:603`) has **zero** production consumers; its only caller anywhere is `crates/tessera-store/tests/bundle_read.rs:195`.

The codebase will have moved by the time the format work starts, so **Task 1 re-runs this check as a guard** and fails the task if the consumer set has grown.

### The three identifiers

**Read this table under Ruling A**: `external_id` and `tessera_id` are not two identities but two representations of **one** identity, and only one of them exists for any given item unless the caller supplied a key. Where the caller supplied nothing, the item's identifier *is* its `tessera_id` and the `external_id` column of this table simply does not apply to it. Where the caller supplied a key, that key is the durable identity and `tessera_id` is the fixed-width transport encoding of it. `entity_id` is neither: it is an internal allocation that never leaves the process.

| | `tessera_id` | `external_id` | `entity_id` |
|---|---|---|---|
| **Who assigns it** | Tessera derives it | the caller supplies it | Tessera allocates it |
| **How** | `FPE_k(shard_id ‖ entity_id)` | caller's business | signature-sorted, §11.1 |
| **Width** | `u64`, fixed | **≤ 64 bytes**, variable *(r6 tightens it from 256 — Q7)* | `u32` (D8; `< 2³²` in `bundle_format = 1`) |
| **Where stored** | `columns.arrow`, at the row — but *derivable*, not authoritative | sidecars only | `permutation.bin`'s index; entity-space structures |
| **Where it appears** | viewer plane (points, `/v1/items/{tessera_id}`), admin plane | admin plane (`/control/ingest`, `/control/changes`) and drill-down responses | **never leaves the process** (I10) |
| **Lifetime** | **transport**: stable across rebuilds while the key and the epoch hold; invalidated wholesale by key rotation, and *partially* by repartitioning/resharding (epoch bump) | caller's business — **this is the durable key** | permanent, never reused (I9) |
| **Safe to persist in a consumer's database?** | **No.** Cache it for the life of a view; re-resolve after an epoch change | **Yes** — Goal 3's primary key | never seen |

`tessera_id` is stored at the row rather than recomputed per gathered mark because the gather is a tight zero-copy loop over mmap'd columns and eight splitmix64 rounds per mark at 143,857 marks per viewport is work the format can pay for once at build. It is nonetheless **derivable**, which is what makes `tessera verify` able to check the whole column against the key.

### I10 under this change — verified

§11.1's signature-sorted entity-ID assignment (permanent under I9; measured 8.9–36.7× posting compression, up to 130× on union cost) is **safe only because entity IDs never leave** — the design says so in terms: *"It costs nothing, does not weaken I9, and is safe only because of I10."*

Under this change entity IDs still never leave, and the guarantee gets *stronger* in substance while its *mechanism* changes:

1. Before: `columns.arrow` stored the entity ID at every row, the gather read it, `PointOut` carried it, and `tessera-wire`'s handle table was the last line of defence stopping it reaching the wire. The invariant rested on a runtime discipline (`handle_for`) at the serialisation chokepoint.
2. After: `columns.arrow` stores no entity ID at all. The gather **cannot** produce one. `PointOut` carries a `TesseraId`. The entity ID exists only in entity-space structures (postings, masks, the overlay) and as the *index* of `permutation.bin` — never as a value on any path that reaches `tessera-wire`.
3. `scripts/check-layers.sh`'s I10 grep (`grep -n "EntityId" crates/tessera-wire/src/payload.rs` must be empty) still holds and is extended in Task 10.

**The design text must be amended, not left to contradict the code (Critical C-3).** Three places say the mechanism is a per-session handle:

- `.ignore/tessera-architecture-design.md:143` — I10: *"Clients receive per-session opaque handles instead."*
- `:100` — §2.6 step 10: *"Row IDs to entity IDs to per-session opaque handles."*
- `:445` — §10.6: *"Points carry per-session opaque handles rather than entity IDs."*

All three are amended in Task 4. The *substance* of I10 is preserved verbatim; only the mechanism clause changes.

**C6 revision (D1).** Appendix C's C6 currently reads:

```
| C6 | Entity ID gaps on the wire | Count, and under Morton assignment also location, of unauthorised items | Medium | **I10**; opaque per-session handles | Closed |
```

It becomes (Task 4):

```
| C6 | External ID gaps on the wire | Where the caller's external IDs carry structure (sequential keys, ingest-ordered surrogates), the gap between two visible IDs is a count of unauthorised items | Medium | `tessera_id` is a keyed permutation of entity space and carries no order, so it discloses nothing; a caller who supplies structured external IDs and exports them is choosing that disclosure. Entity IDs still never cross the boundary (**I10**) | Accepted — caller's control |
```

**C17 — a NEW register entry, required by CLAUDE.md's own rule** *(Critical N-2)*. Retiring the per-session handle (D5) opens two channels that the handle mechanism previously closed as a side effect. Both are **accepted trades** — they are, precisely, the point of D5, since a stable identifier is what lets a client bookmark, share and reconcile a point. But CLAUDE.md says *"anything not in that table is a bug"*, and neither channel is in the table today, so accepting them silently would leave the register wrong about the system:

1. **Existence-over-time probing.** A client holds a `tessera_id`, polls `/v1/items` on a schedule, and watches it turn from `200` to `404`. The transition is a **timestamped signal that the item was deleted, suppressed, or that the principal's grants changed** — for one item the client already knew about and could already see. The per-session handle closed this by expiring: last session's handle named nothing this session.
2. **Cross-principal correlation.** Two principals who can both see an item now observe the **same** identifier for it, so they can join their views out-of-band and learn "we are looking at the same point" — and, by difference, "you can see something I cannot". Per-session handles made the identifier principal-relative, so the join was impossible without both parties already sharing the underlying item.

Task 4 adds the row:

```
| C17 | Stable wire identity across sessions and principals | Existence-over-time probing on a held `tessera_id` (visible → 404 is a timestamped delete/suppress/grant-change signal); and cross-principal correlation, since two principals see the same identifier for the same item and can join views out of band | Medium | **This is the intended trade of D5**, not a residual: a stable identifier is what lets a client bookmark, share and reconcile a point across sessions, and per-session handles bought their unlinkability by making all three impossible. Both channels are bounded to items the probing principal **already sees** — `tessera_id` is order-free, so neither yields entity space, a count of what is hidden, or anything about an item never visible to that principal (**I2** unaffected). The identity **epoch** bounds it further in time. | Accepted — the point of D5 |
```

and **cross-references it from §10.6**, where the mechanism change is stated, so a reader arriving at "points carry an opaque `tessera_id`" is pointed at what that costs rather than having to find it in an appendix. C17 joins C1, C4, C6, C12, C14, C15 and C16 in the list needing an owner and a review date before launch.

---

## The identity construction — specified, not assumed

This section is the normative statement of the seven points the brief's Part 7 requires the revision to settle. **Task 3 turns it into a reviewed memo and Task 4 lands it in the contracts spec; Task 5 implements it; the Python oracle reimplements it from this text alone.**

### 1. The construction

`tessera_id = FPE_k(shard_id: u32 ‖ entity_id: u32) → u64`, a **balanced Feistel network**, 8 rounds, 32-bit halves, keyed by a 128-bit deployment key.

**Input encoding.** `L₀ = shard_id`, `R₀ = entity_id`. (Equivalently: the 64-bit input is `(shard_id as u64) << 32 | entity_id as u64`, split at bit 32.)

**Independently verified invertible** (review round 2). Forward `(L,R) ← (R, L ^ F(i,R))`, inverse `(L,R) ← (R ^ F(i,L), L)` over the rounds reversed. The balanced 32/32 split makes this a permutation of 2⁶⁴ for **any** round function; round-count parity is irrelevant; and the output packing `(L<<32)|R` is itself a bijection. There is no unbalanced-split or odd-round hazard here. **Do not change the construction** — the ruling is to keep `splitmix64` at 8 rounds, conditional only on the three fixes below (the allocator cap, the control-plane threat-model statement, and the third, which was *"forbid `priority` on the viewer plane"* and is now **satisfied differently**: `priority` is a prefix of the keyed identity, so there is no unkeyed residue of the entity ID to forbid. The condition is **met, not dropped** — see "`priority` becomes a prefix of `tessera_id`" above).

**Key schedule.** The key is 16 bytes, read little-endian as two `u64` halves `k0` (bytes 0–7) and `k1` (bytes 8–15).

```
round_key(i) = splitmix64( k0 ^ k1.wrapping_mul(i + 1) )      for i = 0, 1, …, 7
```

**Notation, so a second implementer cannot read it two ways** *(review round 2 reproducibility edit)*. Every range in this section is written out: `i` takes the eight values `0, 1, 2, 3, 4, 5, 6, 7` in that order on the forward path and in the reverse order `7, 6, 5, 4, 3, 2, 1, 0` on the inverse path. Rust's `0..8` and Python's `range(8)` both denote exactly this; the pseudocode's `0..8` is **exclusive of 8** and is spelled out here because a `0..=8` reading gives nine rounds and a silently different, still-invertible, still-collision-free permutation that disagrees with every stored column.

**Hex case rule** *(review round 2 reproducibility edit)*. `identity.key` is written as **exactly 32 lowercase hexadecimal characters** (`0-9a-f`), most-significant byte first — i.e. `key_bytes[0]` is the first two characters. Readers **accept lowercase only** and reject any other spelling with a typed error rather than case-folding, so that MANIFEST has one canonical form and a digest over MANIFEST is stable. The little-endian read into `k0`/`k1` is over the *decoded bytes*, not over the text.

**Degenerate keys are rejected** *(review round 2)*. A key with `k1 == 0` collapses the schedule to a single constant round key for all eight rounds; the all-zero key does the same and is additionally the value an uninitialised buffer supplies. Both are still permutations, so nothing fails loudly. **`IdentityKey::from_hex` and `--id-key` therefore refuse `k1 == 0` and refuse the all-zero key**, with a typed error naming the reason. The CSPRNG mint retries rather than emitting one (probability ~2⁻⁶⁴; the retry loop exists so the property is enforced, not assumed).

**Round function.** `splitmix64` — the function contracts §2.6 fixed for `priority` up to r5, and which after the 2026-07-30 redefinition survives in the spec as **this** round function, with `priority` becoming a prefix of its output rather than a second application of it (all arithmetic wrapping `u64`):

```
splitmix64(x):
    z = x + 0x9E3779B97F4A7C15
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
    z = (z ^ (z >> 27)) * 0x94D049BB133111EB
    return z ^ (z >> 31)

F(i, r: u32) -> u32 = ( splitmix64( (r as u64) ^ round_key(i) ) >> 32 ) as u32
```

**Rounds.**

```
for i in 0, 1, 2, 3, 4, 5, 6, 7:          # eight rounds, ascending
    (L, R) = (R, L ^ F(i, R))
tessera_id = (L as u64) << 32 | R as u64
```

**Inverse** (the engine's only use of it, on `/v1/items`):

```
L = (id >> 32) as u32;  R = id as u32
for i in 7, 6, 5, 4, 3, 2, 1, 0:          # the same eight rounds, descending
    (L, R) = (R ^ F(i, L), L)
shard_id = L;  entity_id = R
```

**`forward`'s input is a checked conversion, not a cast** *(Important I-1)*. `entity_id` is `u32` by D8 and `EntityId` is a `u64` newtype in the code today, so `forward` takes `EntityId` and **returns an error (or panics in a `debug_assert`-backed constructor path) if `entity.raw() > u32::MAX`** rather than truncating. A truncating `as u32` is what would make "collision-free by construction" false: two entities differing only above bit 32 would share a `tessera_id`, and `invert` would return the *wrong* entity — a `/control/changes` suppression against the wrong item. See "The allocator cap" below; the two must land together, because the cap is what makes the checked conversion unreachable in practice and the checked conversion is what makes the cap's absence loud rather than silent.

**The allocator cap** *(Important I-1)*. `Allocator::allocate` is today `lo + n` on a `u64` with **no cap at all** (`crates/tessera-lifecycle/src/alloc.rs:31–36`), and `Allocator::new` seeds from a `u64` high-water. "Collision-free by construction" therefore currently rests on nothing but the corpus being small. **`allocate` must refuse to hand out any ID `≥ u32::MAX`** — a typed error (`AllocError::Exhausted { high_water }`), not a wrap and not a panic in a serving path — and `Allocator::new` must refuse a seed above the same bound. This is §16's entity-ID-exhaustion question arriving early, and refusing is the fail-closed answer: an ingest that would exhaust the space is rejected, the WAL is untouched, and the operator is told. Task 7 implements it; Task 5's `forward` is what makes a bypass loud.

**Why a bijection at all, and why this one.** A Feistel network is a permutation for *any* round function `F` — invertibility does not depend on `F`'s quality — which is what makes collision-freedom structural rather than probabilistic. `splitmix64` is chosen over SHA-256 for three reasons: it was already contract in this spec (§2.6's priority up to r5), so the oracle already reproduces it and a second reader has one construction to learn instead of two — and after the 2026-07-30 redefinition there is genuinely only **one** use of it left in the format, since `priority` becomes a prefix of this function's output rather than an independent hash; it is ~2 ns rather than ~150 ns, which at 8 rounds × 10⁹ items is ~16 s of build rather than ~20 min; and the property being defended (below) does not need a cipher.

**Honest limitation, for the reviewer to judge explicitly.** This is a *blinding permutation*, not a cipher. `splitmix64` is not a cryptographic PRF, and 8 rounds of it should not be assumed to resist an adversary who obtains known `(entity_id, tessera_id)` pairs. The threat model below is what makes that acceptable, and the reviewer is asked to rule on the trade rather than have it assumed: **it buys collision-freedom by construction and a pure function, at the cost of an obviously-correct sorted array, against "design for audit before performance".** The mitigating fact is that the whole construction is ~30 lines with a round-trip test and fixed known-answer vectors on both sides.

### 2. Key lifetime and location

**The key MUST be stable across rebuilds.** If it changes, every `tessera_id` changes, and every client-held identifier — a bookmark, a shared link, a row in a consumer's database — silently breaks. A per-*bundle* key is therefore **wrong**. The key is **per-deployment (per-lineage)**.

It lives in MANIFEST, digest-covered like everything else, as a top-level object:

```json
"identity": {
  "construction": "feistel-splitmix64-v1",
  "rounds": 8,
  "key": "<32 lowercase hex characters>",
  "shard_id": 0,
  "epoch": 1
}
```

**CRITICAL N-1 — a build that has made no explicit key decision REFUSES.** The previous draft made minting the *default* and put the safe path behind a flag. That is backwards at the one step whose mistake cannot be undone: an operator who rebuilds and forgets `--carry-id-key-from` produces a bundle in which every bookmark, every shared link and every consumer-database row is wrong, and the only signal is a line of stdout on a ninety-minute build that nobody is watching. **Minting must be a thing an operator types.**

`tessera build` gains:

| flag | behaviour |
|---|---|
| *(none of the four below)* | **REFUSE.** Exit non-zero before any work, with: *"no identity key decision: pass `--carry-id-key-from <bundle>` to keep this deployment's lineage (the normal rebuild), `--id-key-file <path>` to read this deployment's key from its config file, `--id-key <32 hex>` to restore a recorded key, or `--mint-id-key` to start a new lineage — which invalidates every `tessera_id` any client holds."* No bundle is written and no disk is consumed. |
| `--carry-id-key-from <bundle-root>` | Read `identity.key` and `identity.epoch` from that bundle's MANIFEST and carry both forward verbatim. **This is the normal rebuild path.** |
| `--id-key-file <path>` | **Owner ruling Q6.** Read the key from a per-deployment config file at the given path. **This is where the key lives outside the bundle**, and it is the answer to "the bundle was lost and must be rebuilt from source". See "The deployment config file" below. |
| `--id-key <32 hex>` | Use the given key (lowercase hex, non-degenerate). For restoring a lineage from a recorded key; `--epoch <n>` may accompany it and defaults to 1. **Discouraged in practice** — a key on a command line reaches shell history, process listings and CI logs; `--id-key-file` exists so this does not have to be the ordinary route. |
| `--mint-id-key` | **Explicitly** mint a fresh 16-byte key from the OS CSPRNG at `epoch = 1`, record it, and print it prominently. Help text: *"starts a NEW identity lineage; every `tessera_id` any client holds becomes wrong."* |
| `--rotate-id-key` | Required to proceed when a key was carried or supplied *and* the operator intends a different one. Refuses without it. Help text must state **both** consequences: every `tessera_id` any client holds becomes wrong, **and** row order changes, because the storage sort key is `(morton, tessera_id)` *(2026-07-30)*. |
| `--bump-id-epoch` | Advance `identity.epoch` while keeping the key — the repartitioning/resharding signal (see "The identity epoch"). |

**Refusal rules.** If `--carry-id-key-from` names a bundle whose `identity.construction` or `identity.rounds` differ from this build's, **refuse** — a silently different construction under the same key is the worst outcome available. **If any two key sources are given and disagree** — `--id-key-file` against `--carry-id-key-from`, `--id-key` against either — refuse unless `--rotate-id-key`, whose help text states that all outstanding identifiers are invalidated. Agreement between two sources is not an error and is in fact the useful case: it is how an operator checks that the config file and the previous bundle are the same lineage. Refuse a degenerate key (`k1 == 0`, all-zero) from any source. Refuse a key that is not exactly 32 lowercase hex characters.

**Absent `identity` in a MANIFEST being read** is a typed reader error, not a default. A pre-r6 bundle must fail closed.

#### The deployment config file — `--id-key-file` *(owner ruling Q6, 2026-07-29)*

MANIFEST holds the key and `--carry-id-key-from` carries it forward, which covers the normal rebuild. It does **not** cover the case that matters most: the bundle is lost, or the deployment is rebuilt from source, and the key has to come from somewhere or every client identifier silently breaks. The owner has ruled that it comes from a **per-deployment configuration file**, named on the command line — **not** from an environment variable.

> Owner: *"we'll ultimately need something similar to elastic index configuration."*

So the key is the **first tenant of a deployment config file that will eventually carry more than the key** — index settings, retention, partition policy, the eventual metadata store's connection details. **This plan does not design that file.** It adds one flag and one minimal shape, and records the direction so the file is extended rather than replaced.

**What Phase 1 specifies, and no more:**

- `--id-key-file <path>` reads a small TOML file whose only Phase-1-meaningful content is the identity key:
  ```toml
  # Tessera deployment configuration.
  # The identity key is per DEPLOYMENT, not per bundle: every `tessera_id` any client
  # holds is derived under it, and changing it invalidates all of them. Back this file
  # up wherever the deployment's secrets live and treat losing it as losing the
  # deployment's identity lineage.
  [identity]
  key = "<32 lowercase hex characters>"
  ```
- **Unknown keys and unknown sections are ignored with a note, not an error** — that is what makes the file extensible by a later phase without breaking a Phase 1 binary. An unknown key *inside* `[identity]` is an error, because a misspelt `kye =` must not silently fall through to a refusal that reads as "no key given".
- **Every validation `--id-key` gets, `--id-key-file` gets**: exactly 32 lowercase hex characters, non-degenerate, typed error naming the file and the reason.
- **No default search path, and no environment variable.** The path is always given explicitly. This is what keeps **Critical N-1 intact**: the refusal exists so that a *human* decides, and a file the binary finds on its own — in `$CWD`, in `/etc`, in `$TESSERA_CONFIG` — is not a human deciding. `--id-key-file` counts as an explicit decision **only because the operator typed the path.** An implementation that adds a fallback location has defeated N-1 without touching N-1's code.
- **`--mint-id-key` does not write the file.** It prints the key and tells the operator to record it; writing a config file as a side effect of a build would create the file the previous bullet forbids the binary from finding on its own.

**Out of scope, recorded as direction only:** the wider config file's schema, precedence between file and flags beyond the refusal rule above, per-index or per-partition settings, and secret management. Task 3's memo records the direction; Task 4 mentions the flag in contracts §2.2 and does not specify a config format in the spec.

### 2a. The identity epoch — making staleness detectable

`tessera_id` is a **transport** identifier (owner ruling 10). The key makes it stable across rebuilds. Nothing makes it stable across a **repartitioning or a reshard**, because the bijection's input encodes placement: §12.5 says a policy change is a reindex, and §13.3's shards are row ranges. A repartitioning moves *some* points to a different container, and every moved point's `(shard_id, entity_id)` — and hence its `tessera_id` — changes.

The danger is not a `404`. It is that the churn is **partial**: an identifier that named a moved point now inverts, perfectly validly, to whatever entity now occupies that slot. A client presenting a stale ID gets a `200` describing a **different item**, silently. A signal is therefore mandatory, not a nicety.

**Where it lives.** §12.5's mitigation is already the right shape: a repartitioning is built under a new §10.2 immutable versioned prefix and cut over by **flipping a pointer**. That flip is the natural home for the epoch, because it is the exact moment at which outstanding identifiers stop meaning what they meant.

**The contract:**

- `identity.epoch: u32` in MANIFEST, carried forward verbatim by `--carry-id-key-from`, advanced by `--bump-id-epoch`, and **required to be advanced by any build whose partitioning or sharding differs from the bundle it carried the key from** — refuse otherwise, on the same fail-closed principle as the construction check.
- `GET /meta` reports `identity_epoch` (a deployment-level integer; it is not the key, encodes no entity data, and leaks nothing — it is exactly as sensitive as `bundle_format`).
- `POST /v1/items/{tessera_id}` accepts an **optional** `epoch` in the request body. If present and unequal to the current epoch, the response is `409 conflict`, `detail: "stale identity epoch; re-resolve by external_id"`. **This branch is safe against C-5 and C4** precisely because it is entity-independent: the check happens before inversion, costs a scalar comparison, and returns the same answer for every ID in existence. A client that omits `epoch` gets today's behaviour and accepts the risk.
- Rotation (`--rotate-id-key`) resets `epoch` to 1 — a new lineage, not a continuation of the old one.

**Advertised, not required — OWNER RULING (Q8, 2026-07-29), confirming what this plan already assumed.** `/meta` reports `identity_epoch`; `/v1/items` accepts `epoch` **optionally** and answers `409` on mismatch. The stricter alternative — making `epoch` mandatory on every drill-down, so a stale identifier could *never* silently name a different item — was considered and declined, and the owner's reasoning is recorded because it is what a future reader will otherwise re-derive badly:

1. **The durable identifier is the `external_id`, so anything a consumer persists is keyed on that** (Ruling A, and owner ruling 10). A consumer following the contract does not have a stale `tessera_id` in its database to present; it has an `external_id`, which it re-resolves through the control plane. The mandatory epoch would be guarding a class of caller that the contract already tells not to exist.
2. **Key rotation and repartitioning are deliberate breaking changes, not scheduled hygiene**, so the epoch fires approximately never. Requiring it would put friction on **every** drill-down, forever, to guard a once-in-a-deployment event.
3. A required field is also a required *round trip*: a client that must present an epoch must first fetch one, which makes `/meta` a precondition of the first drill-down for no benefit in the 99.99% case.

The cost of "advertised" is that a client which ignores the epoch entirely can, after a repartitioning, present a stale ID and receive a `200` describing a different item. **That is the caller's choice, made once, and it is exactly C6's and C12's shape: the service offers the mechanism and states the consequence.** It is cheap to take — one integer from `/meta`, echoed on drill-down — and clients that hold identifiers across a deployment change should take it. Task 4 states this in contracts §2.2 alongside the mechanism rather than leaving the "optional" to read as an oversight.

**What consumers are told, in one sentence, and it belongs in the contracts spec:** *persist `external_id`; treat `tessera_id` as valid only for the epoch it was issued under.*

### 3. Threat model for the key

**The key is not a secret against anyone holding the bundle.** It is in MANIFEST; a bundle-holder can invert every `tessera_id` to `(shard, entity)`. That is acceptable and intended: a bundle-holder already has the postings, the masks and the geometry, so entity IDs tell them nothing new.

**The property being defended is narrower and precise:** a **viewer-plane** *client* — holding `tessera_id`s and no bundle — cannot derive entity IDs, cannot order them, and cannot count the gaps between them. That is what C6 was about and what D1 relaxes to the caller's own external IDs.

**The control-plane principal is NOT in the defended set, and obtains chosen-plaintext pairs by construction** *(Important I-3; this is the premise the `splitmix64` ruling depends on, so it must be written down rather than left implied)*. Two mechanisms hand it exact `(entity_id, tessera_id)` pairs, repeatably, with no attack involved:

1. `/status` returns `entity_id_high_water` (`crates/tessera-server/src/control.rs:384`) — the allocator's next ID, in the clear.
2. Task 11 makes `/control/ingest` return the `tessera_id`s it just allocated, and the allocator is monotone and dense (I9), so the caller knows precisely which entity IDs those were.

Ingest a batch of *n* items and you hold *n* known plaintext/ciphertext pairs for the deployment key, chosen in the sense that you decide *n*. **That is fine, and it is why the round function does not need to be a PRF**: a control-plane principal already holds `/control/changes`, the ingest path and the corpus size; entity IDs tell it nothing it cannot ask for directly. But it means the correct statement of the defended property is *"a viewer-plane principal cannot recover entity space"*, **not** *"nobody can"*. An implementation that ever hands a `tessera_id` and its entity ID to the same *viewer* would move the construction inside the attacked set, where 8 rounds of a non-cryptographic mixer is not a claim this plan makes. Both the byte-scanner (Task 13) and the layer check (Task 10) exist to keep that from happening by accident.

**Corollary as it now stands — `priority` is inside the keyed set, and Important I-4 is retired** *(owner decision, 2026-07-30)*. Round 2's corollary read: *`priority` is an unkeyed `splitmix64` of the entity ID, so publishing it hands a viewer a 16-bit residue of the entity — a 65,536× narrowing per mark, computable offline against a candidate entity range and combinable across marks; therefore it is forbidden on the viewer plane, in the contracts spec and in the byte-scanner's sweep.* The redefinition removes its premise: `priority` is `high16(tessera_id)`, a keyed function of a value the same payload already carries in full, so it narrows nothing a viewer does not already hold. **A viewer receiving *k* marks learns only that their priorities fall below some cut *P*, and *P* is fully determined by *k* and by the exact masked count *V* that §7.1 already gives them; nothing about unseen items is recoverable.** The prohibition, its layer check and its byte-scanner sweep are retired — see the historical note under the routing principle for why a future reader must not reinstate them by reflex.

**What the threat model still requires, unchanged:** an implementation must never hand a `tessera_id` **and its entity ID** to the same *viewer*. That is what would move the construction inside the attacked set, and it is what the byte-scanner (Task 13) and the layer check (Task 10) exist to prevent. The general rule survives verbatim: **a hot column may be shown only if it is independent of the entity ID, or keyed under the deployment key.** `priority` now satisfies its second limb; a future unkeyed derivative of the entity ID would not.

Consequences to write down so a later reader does not mistake the key for something it is not:

- **No special handling is required** beyond the bundle's existing protection. It is not a KMS key, not rotated on a schedule, not split.
- **But it must never leave the server.** It appears in no API response (including `/meta`, `/status` and error bodies), in no log line, and in no metric label. Task 13 adds a conformance assertion sweeping for the key bytes on the viewer plane, exactly as the byte-scanner already sweeps for entity IDs.
- **Rotation is a breaking change for clients,** not an operational hygiene measure. Do not schedule it. **And since 2026-07-30 it changes more than identifiers:** the storage sort key is `(morton, tessera_id)`, so a rotation reorders tied rows and a rebuild under a new key yields a different `permutation.bin` as well as different `tessera_id`s. Determinism is unaffected (the build remains a pure function of `(key, shard_id, entity_id)`); the blast radius is wider.

### 4. Width — `u64`, and why not 32 bits

Stay at `u64` even though a single-shard Phase 1 deployment could encode the whole entity space in 32 bits. **Narrowing later is a breaking change for every client**; the 4 extra bytes buy forward compatibility with multi-shard, and the identity column is width-neutral against the `entity_id: uint64` it replaces, so `u64` costs nothing against today. On the wire it is 8 B/point against the retired handle's 4 B: **+0.58 MB on a 2.00 MB payload at k=1000**, which is an input to the drawn-mark plan's `DEFAULT_MAX_K` calibration and is recorded as such.

### 5. Which prefix — `shard_id`, and what is unsettled

`shard_id` here means the **§13.3 row-range shard** (sharding for scale), **not the §12 partition** (compartmented isolation). §13.3 is explicit that row IDs are a spatial ordering and shards are contiguous row ranges; §13.4 rules sharding premature below ~10⁸, so **Phase 1 is single-shard and `shard_id = 0`**, recorded in MANIFEST.

**The partition-vs-shard ambiguity, resolved** *(review round 2; this was open question 1 in its load-bearing form)*. The previous draft left "which discriminator does the bijection encode?" open. It can be closed against the code and the design, and the answer is **the row-range shard, and only ever that**, for three independent reasons:

1. **Entity IDs are already bundle-global across partitions, in the code today.** `Manifest::entity_id_high_water` is a **single** field on the bundle manifest (`crates/tessera-store/src/manifest.rs:68`), not one per `PartitionDescriptor`, and `Engine` carries **one** `Allocator` seeded from it (`crates/tessera-lifecycle/src/alloc.rs:24–27`). Two items in different partitions cannot receive the same entity ID. The bundle layout's `partitions/default/` is a *container* for postings, geometry and sidecars; it is not an ID namespace. So a partition component in the bijection's input would encode **nothing** — it would be a constant.
2. **A §12 partition could not be a 32-bit prefix even if one were wanted.** §12.4 fixes partition identity as *"a canonical hash of the sorted required set"*, because partitions are **discovered, not declared** — created on first sight of a new required set, converging between racing workers precisely because the identity is a content hash. A hash is not a dense small integer, and a dense small integer could not be assigned without the global coordination §12.3's isolation property exists to avoid. This is a structural fact about §12, not a Phase 1 simplification.
3. **§12 partitions exist in the format today; §13.3 row-range shards do not.** `partitions/<phash>/` is in the §2.1 layout tree and `partitions: Vec<PartitionDescriptor>` is in the manifest; there is no shard concept in the bundle at all, and §13.4 rules sharding premature below ~10⁸. So the prefix is a **reserved field**, correctly valued 0, whose justification is forward compatibility with the axis that does not exist yet — which is also why Task 5's `the_shard_prefix_separates_identity_spaces` test matters: it is the only thing keeping a reserved-and-unused field from being silently dropped from the input encoding.

**What genuinely remains open, narrowed** (recorded at resolved question 1 below, and in design §16 — it needs no decision and blocks nothing): §16's exhaustion entry proposes *"shard-local u32 with a (partition, shard, offset) global ID"*. If a future multi-shard deployment allocates entity IDs **per shard** rather than globally, the prefix stops being reserved and becomes load-bearing, and the 32/32 split is exactly right. If it keeps allocating globally, the prefix stays 0 forever and the 4 bytes buy nothing but the option. **Either way the encoding is unchanged and nothing in Phase 1 turns on it** — which is what makes this recordable rather than blocking. Task 3 records it; Task 4 states the resolution above in §16 and does **not** decide the exhaustion question.

### 6. Determinism against `build_equivalence.rs`

The bijection is a **pure function of `(key, shard, entity)`**. Both build paths — the streaming pipeline (`pipeline.rs`) and the in-memory reference path (`lib.rs:180`) — read the same key from the same `BuildArgs`, apply the same function to the same entity IDs, and produce byte-identical `columns.arrow`. **No seed is threaded, no RNG is constructed, no ordering dependence exists.** This is strictly simpler than the random-mint design it replaces, and it is why the previous draft's seeded-minting machinery is deleted rather than adapted.

**The sort order moves onto the identity** *(owner decision, 2026-07-30; this supersedes the earlier "the sort tiebreak does not move")*. `sort_batch` orders by **`(morton, tessera_id)`** ascending, with no further tiebreak, because `priority` is a prefix of `tessera_id` and `tessera_id` is unique per row. Contracts §2.6 calls that order contract, and the oracle re-derives it from `(morton_of(x, y, extent), forward(identity.key, identity.shard_id, entity_id))`. The normative statement, including the one condition under which an explicit tiebreak returns, is in "The sort and tiebreak statement" above; do not restate it from memory.

**Row order is now key-dependent, and `build_equivalence.rs` is unaffected by that.** Both build paths read the same key from the same `BuildArgs`, so they sort identically and produce byte-identical `columns.arrow`. What key-dependence does mean is that a *re-key* changes row order within a Morton cell — recorded as an accepted residual — and that a future test comparing two bundles built under different keys must compare sets, not row indices. Task 6 still passes entity IDs alongside `TilerItem`, but now as a **companion vector permuted with the items** for `permutation.bin` and the sidecars, **not as a sort key**.

### 7. What remains in sidecars

Two directions, both off the viewport path, both per-*interaction* or per-*admin-call* under the routing principle:

| direction | caller | cadence | structure |
|---|---|---|---|
| `entity → external_id` | `/v1/items` drill-down (D4, D6) | per click | **one** positional `u32` locator into the sorted external-ID extents |
| `external_id → entity` | `/control/ingest` dedup, `/control/changes` past WAL retention | per admin call | the existing sorted `external-ids-<k>.arrow` family, entity narrowed to `uint32` |

**There is no `tessera_id → entity` sidecar.** Decryption is a pure function; it needs no file, no map and no I/O. This is the single largest simplification the bijection buys and it is what dissolves Critical C-2.

**The sidecar holds rows only for items whose caller supplied a key** *(Ruling A)*. It is a translation table between two representations of one identity, not a store of identities: an item with no caller key has nothing to translate, its identity is its `tessera_id`, and it occupies no extent row — only a `0xFFFFFFFF` locator slot, which is the *ordinary* case rather than a missing value. A deployment whose callers supply no keys at all pays **zero** sidecar disk and still has a complete, stable, global identifier for every item. No code path may manufacture an external ID for an item that has none.

**And the whole structure is transitional** *(Ruling B)*. It is the placeholder for a future adopted metadata store; keep it minimal, keep both directions behind one module surface, and mark it as such in the spec (Task 4) and at the type (Task 8).

**The locator is SINGULAR — one file, not one per extent** *(Important I-7; the previous draft specified it both ways, and the two readings differ by 33 GiB)*. It is `entities/ext-locator.u32`, **no `<k>` suffix**: one raw `u32` array of length `entity_id_high_water`, indexed by entity ID, holding that entity's **ordinal in the concatenated sorted external-ID extents**. At 10⁹ that is 4 B/row = **3.7 GiB, once**. A per-extent family (`ext-locator-<k>.u32`) would need each file indexed by the *global* entity ID — because an entity's ID says nothing about which extent its key sorts into — so every extent would carry a full-length array and ten extents would cost 37 GiB, blowing the disk gate on its own. **Any occurrence of `ext-locator-<k>` in this plan or in the spec is a defect; the name is `ext-locator.u32`.** The extents remain plural and per-flush; only the locator is singular.

Sentinel `0xFFFFFFFF` for an entity with no caller external ID.

**Post-build entities: the locator does not cover them, and `None` must not be the answer** *(Important I-9)*. The locator is written at build over `entity_id_high_water` as it stood then. An entity ingested afterwards has **no locator slot** (the array is short) and no extent entry (its key is in the WAL, not in a flushed extent). Reading past the array's end and returning `None` would report *"this item has no external ID"* for an item that has one — a wrong answer dressed as a legitimate state. **The drill-down resolution order is therefore, explicitly:**

1. **Live first.** `Engine`'s in-memory `established: FxHashMap<Vec<u8>, EntityId>` (`crates/tessera-engine/src/session.rs:156`) is the authority for everything ingested since the build. It is keyed by external ID, so the drill-down direction needs its **inverse** — an `FxHashMap<EntityId, Vec<u8>>` maintained alongside it, populated by the same two writers (`session.rs:237` at replay and `session.rs:482–487` at ingest). It is bounded by post-build ingest volume, which is bounded by WAL retention; it is not a second copy of the corpus.
2. **Then the bundle.** If `entity.raw() < locator_len`, index the locator; `0xFFFFFFFF` → `Ok(None)` (genuinely no external ID).
3. **Neither, and `entity.raw() >= locator_len`** → `Ok(None)` is *correct* only when the entity does not exist at all. If the entity is past the locator but at or below the allocator high-water and absent from the live map, that is an **inconsistency**, not a "no external ID": return `Err(StoreError::InvalidSidecar { .. })`. A drill-down that cannot account for an entity it just resolved a row for must fail closed rather than quietly under-report.

This is the same shape as `resolve_external_id`'s existing live-map-first ordering (`session.rs:407–412`), running in the other direction, and it is why Task 9 owns both.

**Why a locator rather than a second copy — CONFIRMED by the owner (Q3), and the reason is stronger than the arithmetic that first suggested it.** The naive drill-down structure is an entity-ordered binary column of external IDs — 12 B/row ≈ 11.2 GiB at 10⁹ on the synthetic corpus's 8-byte keys, duplicating bytes the sorted family already holds. The locator is 4 B/row ≈ 3.7 GiB: one indexed read, then one positional read from a lazily-opened extent.

The owner's ruling rests on a different and better argument than "7.5 GiB cheaper today": **build around the more likely real-world case, which is an externally provided ID.** Under that case the two structures scale differently, and the difference is not a constant:

| | duplicate entity-ordered column | locator |
|---|---|---|
| cost model | `mean_key_len + 4` bytes/row — **scales with the caller's key length** | **4 bytes/row, flat** |
| 8-byte synthetic keys | 11.2 GiB | 3.7 GiB |
| 16-byte binary UUIDs | 18.6 GiB | 3.7 GiB |
| 36-char UUID strings | 37.3 GiB | 3.7 GiB |
| 64-byte cap (Q7) | 63.3 GiB | 3.7 GiB |

The duplicate column's cost is *the whole external-ID store a second time*. The locator's is a `u32` per entity, whatever the caller's keys look like — so on the real-world deployment the ruling is built for, the margin is not 7.5 GiB, it is tens of GiB and it widens with key length. **The indirection is the cheap half of the trade, not the expensive one.** Open question 3 is closed on this basis; the alternative is recorded above only so the shape of the rejected design survives.

**Compression is considered and REJECTED for now** *(brief Part 9; owner steer: simplicity)*. Do **not** build a compressed or block-indexed external-ID store. The arithmetic does not support it, and the alternatives are worse rather than simpler:

| shape | disk at 10⁹ |
|---|---|
| **planned:** sorted-by-external-id extents (4 B offset + 8 B value + 4 B entity = 16 B/row = 14.9 GiB) + one `u32` locator (3.7 GiB) | **18.6 GiB** |
| entity-ordered only (11.2 GiB) + a hash index for the control plane | 22.4 GiB |
| entity-ordered only, storing the ids twice | 26.3 GiB |
| entity-ordered only, control plane resolves by scan | 11.2 GiB, but an 11.2 GiB sequential read **per admin op**, which also evicts the page cache this whole change exists to protect |

And **the 18.6 GiB is disk, not memory** — per-extent lazy, never resident on the render path (24.7 GiB steady / 43.3 GiB pathological against 47 GiB **on this corpus's 8-byte keys**; the pathological case is bounded by the sidecar's own size, and what a deployment with longer keys costs *resident* is a question about Ruling B's replacement store, not about this one). Compression would buy disk on a bundle that already fits once the old one is deleted, in exchange for a block format, per-block digests and a decoder on an authorisation-adjacent serve path, for zero latency gain.

**Recorded as a conditional future option, with its precedent.** Contracts §2.4 already licenses compression off the request path: `terms/pairs.parquet` is Parquet with `DELTA_BINARY_PACKED`, *"measured 3.8× smaller, 3× faster to read"*, justified because *"the uncompressed-mmap rule (design §10.3) protects request paths, and this file sits on neither"*. The external-ID store sits on neither either, under D4/D6, so the same licence would apply — **when the data earns it**. The trigger is data-dependent and must not be assumed: it becomes worth doing for a deployment using long human-readable string keys, where dictionary or prefix encoding pays for itself; **random UUIDs compress essentially not at all**, so a deployment measuring first is the precondition. Task 4 records this as a note in contracts §2.4, not as a deviation.

**The external-ID cap tightens from 256 bytes to 64 — OWNER RULING (Q7, 2026-07-29).** Contracts §1 permits an external ID of up to 256 bytes and **nothing in the format sizes the store for that**. The arithmetic above assumes the synthetic corpus's 8-byte keys; a deployment using the full 256-byte cap would pay ~32× on the value bytes — 67 GiB at the 64-byte cap and well past 250 GiB at 256 — with no warning anywhere in the format. The owner has ruled to **tighten the cap to 64 bytes**, which covers a 36-character UUID string, a ULID, an ObjectId, and every business key a sane caller uses, while bounding the sidecar at a number the sizing table below actually states.

Task 4 Step 10 therefore does **both** of the things the earlier draft offered as alternatives, because they are complementary rather than exclusive: it **tightens §1's cap to 64 bytes**, *and* it states in §1 and §2.4 that **sidecar disk scales linearly with the caller's key length**, so a deployment near the cap knows what it is buying. Tightening now is one line; tightening after a caller depends on 100-byte keys is a breaking change, and the contract is open exactly once. Readers reject an over-length external ID with a **typed error at ingest and at build**, not by truncating — a truncated key is a *different* key, and two callers' keys that share a 64-byte prefix would collide into one entity.

---

## Arithmetic — at 10⁹, redone (Critical C-4)

Bytes/row × 10⁹, in GiB. **None of the previous draft's sidecar arithmetic is carried forward.** `columns.arrow` today is 22 B/row = 21.1 GiB measured (`entity_id` u64 8, `x` f32 4, `y` f32 4, `node_id` u32 4, `priority` u16 2). **The 2026-07-30 priority redefinition changes none of this arithmetic**: same column, same `u16`, same 2 B/row — only the function that fills it, and the order the rows sit in.

**Identity width, decided (brief Part 4, independently checked):**

| identity column at the row | B/row | `columns.arrow` | vs today |
|---|---|---|---|
| **`tessera_id` u64, `node_id` dropped** | **18** | **17.3 GiB** | **−3.8** |
| 16-byte UUID, `node_id` dropped | 26 | ~25.0 GiB | **+3.9** |
| variable bytes, ~16 B mean + offsets | 30 | ~29.0 GiB | **+7.9** |

17.3 GiB, not 16.8: the previous draft's "after" figure dropped the ~3% Arrow IPC overhead it had accepted in its "before" column.

Wire cost at k=1000 (143,857 points) against today's 2.0 MB total payload: `u64` 1.15 MB of identity, 16-byte UUID 2.3 MB, 36-char UUID string 5.2 MB.

**Viewport working set** — the number the hypothesis is about:

| | before | after (sidecars unopened) |
|---|---|---|
| `columns.arrow` | 21.1 (22 B/row) | **17.3** (18 B/row) |
| `morton.u32` | 3.7 | 3.7 |
| `permutation.bin` | 3.7 | 3.7 |
| external-ID extents, page-touched at open by `validate_sorted` | **18.9** | **0** — per-extent lazy (C-6) |
| `ext-locator.u32` | — (did not exist) | **0** unopened; touched pages only, up to 3.7 |
| **total** | **47.4 GiB** vs 47 GiB RAM | **24.7 GiB** |

**Steady state after the first drill-down is 24.7 GiB + one extent, not 24.7 GiB unconditionally** (Critical C-6). An extent is 10⁸ rows; at 16 B/row that is **1.49 GiB**, plus the touched pages of the 3.7 GiB locator. Record it that way in Task 15's memo. One `OnceLock` over *all* extents — the previous draft's design — would map and digest the whole family on the first click and take the working set to ~35.9 GiB permanently. **Lazy opening is per extent.** The extent is already selected by an O(extents) first-key/last-key scan before anything is mapped, so this costs nothing to arrange.

**The pathological case, stated so nobody has to derive it under pressure** *(review round 2)*. A workload that drills down into every extent ends at

```
24.7  (columns 17.3 + morton 3.7 + permutation 3.7)
+ 14.9  (all ten extents, 1.49 GiB each)
+  3.7  (the whole locator resident)
= 43.3 GiB   against 47 GiB RAM
```

**Still under — and the qualifier is not decoration.** 43.3 GiB is a property of **this corpus's 8-byte synthetic keys under the placeholder sidecar design**, because the `14.9` term above is just the extents' size and the extents' size is the caller's key length. It is **not** a general property of Tessera, and an earlier draft of this paragraph asserted, unqualified, that the residency goal "survives the worst drill-down pattern rather than only the expected one" — a sentence that generalises past what is being measured. What holds generally is narrower and is the claim to make: **the pathological all-extents case is bounded by the size of the external-ID sidecar, which is bounded by the deployment's keys — not by anything intrinsic to the format.** On the corpus Phase 1 measures, that bound lands at 43.3 GiB against 47 GiB of RAM.

**Residency under real caller-supplied keys is a property of the replacement store, and is out of scope here** *(Ruling B)*. This sidecar is an explicitly transitional placeholder for a future adopted per-point metadata store — the routing principle's per-interaction row, the same slot as §8.3's vector sidecar — so projecting *its* all-extents residency onto 16-byte UUIDs or 36-character strings would be forecasting a design already scheduled for replacement, and the numbers would read as system properties while being wrong for the store that actually ships. **No such projection appears in this plan or in Task 15's memo.** The *disk* sizing table below is a different matter and stays: disk cost is already labelled deployment-dependent, it is what schedules the replacement by evidence, and it makes no claim about what is resident.

**This is the number Task 15 Step 3 must actually try to reach** — 1,000 drill-downs spread across the key space, not clustered — because a residency claim that holds only for a well-behaved click pattern is not a residency claim even on the corpus it is measured on. **And Task 15 Step 4's memo must carry the qualifier rather than the headline.**

**Whole bundle on disk — CORRECTED (Task 2 Step 3a, measured 2026-07-29 on the existing `/tmp/tessera-1e9`):**

| | before (measured) | after (projected) |
|---|---|---|
| `columns.arrow` | 21.07 | **17.3** |
| `morton.u32` + `permutation.bin` | 7.45 (3.725 + 3.725, both measured) | 7.45 |
| `terms/` (postings.arrow 0.135 + `pairs.parquet` 0.113) | **0.25** (measured — see note) | 0.25 |
| `external-ids-<k>.arrow` (`external_id` binary + `entity_id`) | 18.86 (u64 entity, measured) | **14.9** (u32 entity, 16 B/row, projected) |
| `ext-locator.u32` (drill-down) | — | **3.7** |
| **total** | **47.63 GiB (measured; = 51.1 GB decimal)** | **~40.6 GiB** |

**The `terms/` residual theory in the previous version of this section was WRONG, and Task 2 Step 3a's `du` settles it: `terms/` is 0.25 GiB, not 3.7 GiB and not contracts §2.4's ~5.6 GiB.** Root cause found: the "51.1 GiB" this section's table was built to sum to was never a GiB figure — it is `frozen_bundle_bytes / 1e9` (decimal **GB**) from `scripts/bench_p99.py`'s reporting, `51,142,099,202 / 10⁹ = 51.14`. The same byte count in binary GiB is `51,142,099,202 / 2³⁰ = 47.63`. The ~3.5 GiB gap between those two units-of-the-same-number is what got misattributed to an unmeasured `terms/` line, because a table built in GiB rows was being forced to sum to a GB total. Once every row is actually measured in GiB (`du -sb`, converted by `2^30`), the table sums correctly on its own: `21.07 + 7.45 + 0.25 + 18.86 = 47.63 GiB`, which matches the measured `du -sb /tmp/tessera-1e9` total (`51,142,099,202` bytes = `47.630` GiB) to three decimal places (the ~36 KiB gap is `MANIFEST.json`/`CURRENT`/the dictionary). **The previous draft's 0.25 GiB figure for `terms/` was correct all along**; this plan's "corrected" 3.7 GiB residual was itself the error, arrived at by solving for a residual against a GB total mislabelled as GiB.

**Contracts §2.4's ~5.6 GiB `pairs.parquet` estimate is also refuted by direct measurement**: `pairs.parquet` is 121,442,840 bytes = **0.113 GiB**, ~48× under the estimate. This is not a units error like the one above — it is a genuine finding about the synthetic corpus's term distribution (far fewer term pairs per item than §2.4's estimate assumes) and belongs in the record as such; §2.4 should be annotated the next time it is touched.

**Consequence for the disk gate: the after-total is smaller than either branch this section used to plan against, not larger.** Using the measured `terms/` = 0.25 GiB in the "after" column: `17.3 + 7.45 + 0.25 + 14.9 + 3.7 = 43.6 GiB` (not 47.0, and nowhere near the 48.9 GiB the ~5.6 GiB branch would have produced). Free space is ~15 GiB; the old bundle is 47.63 GiB (measured); **they still cannot coexist** — that conclusion is unchanged — but the margin after a rebuild is more comfortable than this section previously projected, which is relevant to how tightly Task 14 needs to watch disk during the build.

**The build derives `external_id` from `source_id` for every item** (`crates/tessera-build/src/lib.rs:659`, `crates/tessera-build/src/pipeline.rs:335`) — 8 bytes little-endian. The previous draft's claim that the alias extents go to zero because "the synthetic corpus supplies no caller namespace" is **false** and is deleted. Under D3 that family *is* the external-ID sidecar; it is sized above, not assumed away.

**But those 8-byte keys are scaffolding, not identity** *(Ruling A)*. The synthetic corpus's callers supply nothing; under the owner's framing those items' identity is their `tessera_id`, and a strictly faithful build writes no extents and no locator at all (bundle ~28.4 GiB). They are kept because they are the only 10⁹ test article for the two sidecar directions and for Task 15 Step 3's pathological residency case. **The consequence for every figure below: 14.9 + 3.7 GiB is what *this corpus* costs, not what Tessera costs.** A deployment whose callers supply no keys pays zero; a deployment whose callers supply UUID strings pays far more.

### Sidecar sizing across key lengths, and the threshold that schedules the replacement

Sorted extents at 10⁹, costed as `(key bytes + 4 offset + 4 entity)` per row, against the locator's flat `4` *(owner, 2026-07-29)*:

| external ID | extents | locator | sidecar total | share of a ~32.4 GiB core bundle + sidecar |
|---|---|---|---|---|
| **none supplied** (identity *is* `tessera_id`) | **0** | **0** | **0** | 0% |
| 8-byte synthetic (today's corpus) | 14.9 GiB | 3.7 | **18.6** | ~36% |
| 16-byte binary UUID | 22.4 GiB | 3.7 | **26.1** | ~45% |
| 36-char UUID string | 41 GiB | 3.7 | **44.7** | ~58% |
| 64-byte cap (Q7) | 67 GiB | 3.7 | **70.7** | ~69% |

**The locator stays 3.7 GiB throughout** — that is Q3's ruling in one row, and it is why the indirection is the cheap half of the trade.

**The threshold, stated so the replacement is scheduled by evidence rather than by surprise: once mean key length exceeds ~16 bytes, the external-ID store dominates the bundle** — it passes the combined size of `columns.arrow`, `morton.u32`, `permutation.bin` and `terms/` put together — **and at that point compression, or the Ruling B replacement store, pays for itself.** Below ~16 bytes neither does: brief Part 9's arithmetic holds, the store is disk rather than resident memory, and a block format would buy nothing for a decoder on an authorisation-adjacent path.

**Phase 1 builds neither.** The number is recorded here, and in Task 15's memo, so that the first deployment with long human-readable keys is met with a measurement and a decision that already exists rather than with a surprise at build time. Two operational corollaries worth stating with it: the trigger is **mean** key length in a given deployment, so it must be measured per deployment and not assumed from the key *type*; and **random UUIDs compress essentially not at all**, so a deployment that crosses the threshold with binary UUIDs is a candidate for the replacement store, not for compression.

**Free-space problem, stated plainly.** 15 GiB free; the new bundle is ~43.6 GiB (corrected — see the "Whole bundle on disk" table above); the old is 47.63 GiB measured (51.1 GB decimal). **The old and new bundles cannot coexist.** Task 14 is gated on an explicit owner decision, with a 2.5 × 10⁸ fallback that fits. Task 2 exists partly so the current bundle's baseline is captured in full *before* that decision is reached.

**Attribution — do not let these be conflated.** The residency win comes **almost entirely from dropping `node_id` (−3.8 GiB) and de-residenting the external-ID extents (−18.9 GiB)**. The identity swap is **width-neutral and saves zero bytes**, and **so is the 2026-07-30 priority redefinition** — same column, same `u16`, same 2 B/row; it is justified by I7's purpose and §12.3's composition argument, and must never be credited with a byte or a millisecond. The identity swap is justified by the architecture argument — the boundary identity is a pure function of the internal one, so internal→external needs no structure and external→internal needs no lookup — not by the tail. They are planned and built together only because the format must be final before the single rebuild. Task 15's memo must separate them explicitly.

---

## Coordination with the drawn-mark budget plan

`docs/superpowers/plans/2026-07-29-drawn-mark-budget.md` is in flight and overlaps this plan at two points.

- **Its Task 2 (`morton.u64` → `morton.u32`) has landed** (commit `2a399b5`, contracts §0.3 deviation 5, spec r5). No conflict; this plan builds on it.
- **Its Task 7 sets `DEFAULT_MAX_K`** in `crates/tessera-server/src/config.rs` from the measured minimum of three ceilings — GPU render (P1), transport/decode (P2), and **handle table (P3)**.
- **The conflict is P3.** D5 retires the per-session handle from the viewer plane, so the handle-table ceiling **stops being an input** to `DEFAULT_MAX_K`, and the transport ceiling changes because a point's identity goes from 4 bytes to 8. Two of the three inputs move. This is now settled by owner decision, not contingent on an open question.

**Ordering, explicitly:**

1. Drawn-mark Tasks 3–6 (the probe harness and the P1/P2/P3 measurements) may proceed in parallel with this plan's Tasks 1–13 — they touch `probes/markbudget/` (excluded from the workspace) and nothing this plan touches.
2. **Drawn-mark Task 7's `DEFAULT_MAX_K` *value* must not be committed until this plan's Task 15 has run.** A `k` calibrated on the current bundle is calibrated against a swapping box and a 4-byte handle; both change here. The calibration *method* is unaffected — only the numbers it consumes.
3. Neither plan edits the other's files. Drawn-mark Task 7 lists `crates/tessera-server/src/config.rs`, `.ignore/tessera-contracts-spec.md` (§3 `k` note) and `docs/superpowers/plans/2026-07-28-phase1-walking-skeleton.md` (Task 16 k-sweep); this plan touches the contracts spec in §0.3/§2/§3/§5 and the Phase 1 plan's Task 16 only in Task 16, after drawn-mark Task 7. **If both are in flight on the contracts spec at once, STOP and report** rather than merging revision blocks by hand.

**A second in-flight plan overlaps the priority fold, and this plan does NOT resolve the overlap.** `docs/superpowers/plans/2026-07-30-selection-route-chooser.md` declares itself a companion to the priority memo and states that it *"shares Task 1's call site"* — i.e. the selection path this fold's spec text describes but does not implement. **Nothing from that plan is folded in here, and nothing here decides anything for it.** If both are in flight against the selection path or against design §7.2 at the same time, **STOP and report to the owner** rather than reconciling them; the sequencing between the two is an owner decision, not an executor's.

---

## File Structure

**Task 1 — premise guard (no shipped change)**
- Create: `docs/design-memos/2026-07-30-columns-identity-premise.md`

**Task 2 — warm/cold discrimination (measurement only)**
- Create: `scripts/discriminate_tail.py`, `docs/design-memos/2026-07-30-tail-discrimination.md`
- Modify: `crates/tessera-engine/src/session.rs`, `crates/tessera-engine/Cargo.toml`, `crates/tessera-store/src/external_ids.rs` (temporary, feature-gated)

**Task 3 — the identity construction memo** *(EXECUTED, committed `acfe2ce`)*
- Create: `docs/design-memos/2026-07-30-tessera-id-construction.md`, `reference/vectors/tessera_id.json`

**Task 4 — the documents**
- Modify: `docs/design-memos/2026-07-30-tessera-id-construction.md` — Step 0, the four priority amendments to Task 3's committed memo (the vectors file is **not** touched)
- Modify: `.ignore/tessera-architecture-design.md` — I10 (`:143`), §2.6 step 10 (`:100`), §10.6 (`:445`), Appendix C (C6), Appendix A, §10.3 (routing principle), §11.1, §16 (open question), **§7.2 (`:240`, `:246`, `:248`), §2.6 step 7 (`:97`), §5.2 (`:163`), `:411`, §12.3 (`:525`), §14 (`:590`) — the priority redefinition**, Appendix G (r21)
- Modify: `.ignore/tessera-contracts-spec.md` — §0.3 (deviations 6–9), §1, §2.1, §2.2 (MANIFEST `identity`), §2.4, §2.6, §3.1, §3.2, §3.4, §5, Appendix R (r6)

**Task 5 — the bijection**
- Create: `crates/tessera-types/src/identity.rs`
- Modify: `crates/tessera-types/src/lib.rs` (new `identity` module, `TesseraId`; keep `NODE_NONE`), `scripts/check-layers.sh` (I4 grep gains `TesseraId`)

**Task 6 — `tessera-store`: the columns schema**
- Modify: `crates/tessera-store/src/read.rs`, `crates/tessera-store/src/write.rs`, `crates/tessera-store/src/manifest.rs`
- Modify: `crates/tessera-spatial/src/tiler.rs`
- Modify (tests): `crates/tessera-store/tests/segment_roundtrip.rs`, `crates/tessera-store/tests/bundle_read.rs`

**Task 7 — `tessera-build`: the key, the column, the sidecars, the allocator cap**
- Modify: `crates/tessera-build/src/lib.rs`, `crates/tessera-build/src/pipeline.rs`, **`crates/tessera-cli/src/main.rs`** (the seven identity-key CLI flags — including `--id-key-file` (Q6) — and the N-1 refusal; the CLI is in `tessera-cli`, **not** in a `tessera-build/src/bin/` that does not exist), `scripts/build_full.sh` (forward the key flag; hard-code none)
- Modify: `crates/tessera-lifecycle/src/alloc.rs` (cap `allocate` and the seed at `u32::MAX` — Important I-1)
- Modify (tests): `crates/tessera-build/tests/build_equivalence.rs`, `crates/tessera-build/tests/build_smoke.rs`, `crates/tessera-lifecycle/tests/`

**Task 8 — the sidecars: per-extent lazy, digest- *and* sortedness-verified**
- Create: `crates/tessera-store/src/sidecar.rs`
- Modify: `crates/tessera-store/src/external_ids.rs` (folded in and deleted), `crates/tessera-store/src/lib.rs`, `crates/tessera-store/src/error.rs`

**Task 9 — `tessera-engine`: gather, entity-space visibility, drill-down, open nothing**
- Modify: `crates/tessera-engine/src/compose.rs` (factor out `verdict`; add `visible_to`), `crates/tessera-engine/src/viewport.rs`, `crates/tessera-engine/src/session.rs`

**Task 10 — the boundary: wire and server**
- Modify: `crates/tessera-wire/src/payload.rs`, `crates/tessera-wire/src/handles.rs`, `crates/tessera-wire/src/lib.rs`
- Modify: `crates/tessera-server/src/viewer.rs`, `crates/tessera-server/src/error.rs`
- Modify: `scripts/check-layers.sh`

**Task 11 — `/control/ingest` duplicate detection and the drill-down field**
- Modify: `crates/tessera-server/src/control.rs`, `crates/tessera-server/src/viewer.rs`, `crates/tessera-engine/src/session.rs`

**Task 12 — the oracle**
- Modify: `reference/oracle/bundle.py`, `reference/oracle/identity.py` (new), `reference/oracle/viewport.py`, `reference/oracle/harness.py`, `reference/tests/test_differential.py`, `reference/tests/test_identity.py` (new)

**Task 13 — conformance**
- Modify: `conformance/tests/test_byte_scan.py`, `conformance/tests/test_restart_replay.py`

**Task 14 — the 10⁹ rebuild (owner-gated)**
- Modify: `scripts/build_full.sh`

**Task 15 — re-measurement**
- Modify: `scripts/bench_p99.py`
- Create: `docs/design-memos/2026-07-30-identity-results.md`

**Task 16 — Phase 1 exit record**
- Create: `docs/superpowers/plans/phase1-results.md`
- Modify: `docs/superpowers/plans/2026-07-28-phase1-walking-skeleton.md`, `CLAUDE.md`

---

### Task 1: Re-verify the premise, compiler-enforced

The owner has already run this check (2026-07-30) and the result is recorded above. This task re-runs it because the codebase moves, and because every later task assumes the answer. **It ships no production change** — the rename is reverted before the commit; only the memo lands.

**Files:**
- Create: `docs/design-memos/2026-07-30-columns-identity-premise.md`

**Interfaces:**
- Consumes: nothing.
- Produces: a confirmed consumer set for `ColumnsRef::entity_id()` and `ColumnsRef::node_id()`, cited by Tasks 6, 7 and 9.

- [ ] **Step 1: Rename both accessors so every consumer becomes a compile error**

In `crates/tessera-store/src/read.rs`, rename `entity_id` → `entity_id_PREMISE_CHECK` (line 591) and `node_id` → `node_id_PREMISE_CHECK` (line 603). Change nothing else.

- [ ] **Step 2: Enumerate every consumer**

```bash
cargo check --workspace --all-targets 2>&1 | grep -E "^error|-->" | tee /tmp/claude-1000/premise.txt
```

Expected: errors pointing at exactly

```
crates/tessera-engine/src/viewport.rs:113
crates/tessera-engine/src/viewport.rs:314
crates/tessera-store/tests/bundle_read.rs:192   (entity_id)
crates/tessera-store/tests/bundle_read.rs:195   (node_id)
```

**If any other file appears, STOP and report to the owner.** The premise has changed and Tasks 6–10 need revising before anything is written. Specifically: a consumer in `tessera-authz`, `tessera-lifecycle`, or anywhere in mask composition would mean the column is load-bearing for authorisation, not just for output, and the identity swap would be unsafe.

- [ ] **Step 3: Revert the rename**

```bash
git checkout -- crates/tessera-store/src/read.rs
cargo check --workspace --all-targets
```
Expected: clean.

- [ ] **Step 4: Write the memo**

Create `docs/design-memos/2026-07-30-columns-identity-premise.md` recording: the date, the method (*rename the accessor and let the compiler enumerate the consumers — not grep*), the verbatim consumer list from Step 2, a one-line description of what each consumer does with the value, and the conclusion: **both production readers are pure row→identity for output; mask composition uses the forward permutation (`permutation.bin`) and never the column; therefore replacing the column's contents is structurally clean.** Note that `node_id()` has zero production consumers.

**Quality gate:** `cargo check --workspace --all-targets` clean after the revert, and `git status --porcelain crates/` empty.

- [ ] **Step 5: Commit**

```bash
git add docs/design-memos/2026-07-30-columns-identity-premise.md
git commit -m "docs: compiler-enforced premise check for the columns identity swap"
```

---

### Task 2: Warm/cold discrimination — test the hypothesis before spending the rebuild

The ~42–47 ms constant tail is *hypothesised* to be cold major page faults. Nobody has measured it. This task discriminates, on the **current** bundle, before any format work — because if the hypothesis is wrong, the format change is still architecturally justified but its performance claim is not, and the owner must know that before a 90-minute rebuild and an irreversible bundle deletion.

**Files:**
- Create: `scripts/discriminate_tail.py`, `docs/design-memos/2026-07-30-tail-discrimination.md`
- Modify: `crates/tessera-engine/src/session.rs`, `crates/tessera-engine/Cargo.toml`, `crates/tessera-store/src/external_ids.rs` (temporary, feature-gated)

**Interfaces:**
- Consumes: the existing 10⁹ bundle at `/tmp/tessera-1e9`; the k-sweep harness in `scripts/bench_p99.py`.
- Produces: a confirm/refute verdict on the page-fault hypothesis, and a per-arm table of `majflt` deltas that Task 15 compares against.

- [ ] **Step 1: Add a temporary, feature-gated skip of the external-ID index load — fail-closed (Important I-2)**

In `crates/tessera-engine/src/session.rs`, at the `ExternalIdIndex::load(&external_id_paths)` call inside `Engine::open` (~line 221–228), gate the load behind a Cargo feature so the 18.9 GB of extents are neither mapped nor page-touched.

**The previous draft's `ExternalIdIndex::empty()` is fail-open by construction and must not be used:** it returns `None` from `resolve`, which reads as "unknown external ID", so a WAL-resident suppression against a bundle-resident item would **silently not apply**. Under a measurement feature that is a suppression that does not suppress. Instead:

```rust
// TEMPORARY (Task 2, tail discrimination — removed in Task 8, which makes the
// sidecars lazy for real). Under `skip-id-index` the bundle's external-ID extents
// are neither mapped nor scanned, and every resolution is a typed ERROR rather
// than a `None`: a `None` here would read as "unknown external ID" and a
// WAL-resident suppression would silently fail to apply. Measurement builds only.
#[cfg(feature = "skip-id-index")]
let external_index = ExternalIdIndex::disabled();
#[cfg(not(feature = "skip-id-index"))]
let external_index = ExternalIdIndex::load(&external_id_paths).map_err(EngineError::Store)?;
```

In `crates/tessera-store/src/external_ids.rs`:

```rust
/// An index that refuses to resolve. Used only by the `skip-id-index` measurement
/// feature (Task 2). Every `resolve` returns `StoreError::IdIndexDisabled` — never
/// `None`, which callers read as "no such external ID" and which would turn a
/// suppression into a no-op.
pub fn disabled() -> Self { Self { state: State::Disabled } }
```

Declare the feature in `crates/tessera-engine/Cargo.toml`:

```toml
[features]
# Measurement only: skip loading the bundle's external-ID extents at open, so a
# tail-latency run can attribute page faults. Every resolution errors. Never
# enable in a serving build.
skip-id-index = []
```

Add a test asserting `disabled()` errors rather than returning `None`, and assert in `Engine::open` that the feature is not enabled when a WAL with deny records is present — or, if that is awkward, log a `WARN` at open naming the feature. The measurement workload is viewport-only and never resolves, so the feature is inert in practice; the point is that it cannot become a quiet fail-open if reused.

- [ ] **Step 2: Write the discrimination harness**

Create `scripts/discriminate_tail.py`. It runs the same random-pan viewport workload as `scripts/bench_p99.py` (reuse its request generator by import — do not fork it) across **four arms**, and records per-arm p50/p99/max **and** the server process's major-fault delta.

```python
#!/usr/bin/env python3
"""Discriminate the constant ~42-47 ms viewport tail: cold page faults, or not?

Four arms over the same workload, same bundle, same principal, same viewports:

  A  cold        server just booted, no pre-fault, external-ID index loaded
  B  prefaulted  as A, but every hot mapping read end-to-end before measuring
  C  no-index    server built with `--features skip-id-index` (18.9 GB of
                 external-ID extents never mapped), no pre-fault
  D  no-index+prefaulted   C plus the pre-fault pass

The hypothesis predicts: the constant tail is large in A, small in B, small in C,
smallest in D -- i.e. it tracks residency, not work.  If the tail survives in D,
the hypothesis is REFUTED and the residency story is not what is costing the tail.

Major faults are the direct evidence: read field 12 (majflt) of
/proc/<pid>/stat before and after each arm.  A tail caused by page faults must
show a majflt delta that falls with the arms; a tail that persists at ~zero
major faults is something else (allocator, GC of the frozen cache, tokio
scheduling, NUMA, or the swap the box entered at high k).
"""
```

Requirements the implementer must meet, spelled out:

1. **Arms A and B** use a server built normally; **arms C and D** use a server built with the feature. The feature is declared on `tessera-engine`, so the build command must enable it **through the binary's crate** (Important I-2's second half — a bare `--features skip-id-index` on a virtual manifest fails):
   ```bash
   cargo build --release --features tessera-engine/skip-id-index
   ```
   If the workspace binary crate does not re-export the feature, add a pass-through feature on the binary crate rather than building `-p tessera-engine` alone (which produces no server).
2. **Cold** means: freshly booted server process, and the page cache dropped between arms. Under WSL2, `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches` may not be permitted — if it fails, **do not silently continue**: print a warning, record `cache_dropped: false` in the results JSON, and note in the memo that arms A and C are "warm-ish" and the discrimination is weaker. Report to the owner.
3. **Pre-fault** means: after boot and after the one-off warm-up (fragment build + row projection), read every byte of `columns.arrow`, `morton.u32` and `permutation.bin` once (`cat <file> > /dev/null` per file is sufficient and is what §10.5's "pre-faulting at boot" means). Record the pre-fault wall time separately; it is not in the viewport budget.
4. **Same viewports, same order, same seed** across all four arms. The workload must be identical or the comparison is worthless.
5. **k values: 50, 500, 1000 only.** k=30 is below the interesting range; k=2500 and 5000 swapped the box and would contaminate every arm.
6. Record per arm: p50, p99, max, `majflt` delta, `minflt` delta, peak RSS (`VmHWM` from `/proc/<pid>/status`), and `SwapTotal`/`SwapFree` deltas from `/proc/meminfo`.
7. Write `docs/design-memos/2026-07-30-tail-discrimination.md` and a machine-readable `probes/tail-discrimination.json`. **The JSON's shape is a contract with Task 14 Step 2a, not a convenience**: `{"cache_dropped": bool, "arms": {"A": {"p50":…, "p99":…, "max":…, "majflt_delta":…, "minflt_delta":…, "peak_rss":…}, "B": {…}, "C": {…}, "D": {…}}}`, with per-k breakdowns nested underneath. Task 14's deletion gate parses exactly these keys for all four arms and refuses to delete the old bundle if any is absent.

**This is the ONLY capture of the old-format baseline that will ever exist.** The owner has authorised Task 14 to delete `/tmp/tessera-1e9` (Q5) **conditional on this task having captured it**, and that authorisation is what makes the rebuild possible at all on a 97%-full disk. Anything not measured here is not measurable later: capture generously, commit the JSON and the memo in this task's own commit, and do not leave either untracked.

- [ ] **Step 3: Check disk and run**

```bash
df -h /
```
The bundle is already built; this task writes only results JSON and a memo (kilobytes). If `df` shows under 1 GiB free, STOP and report.

- [ ] **Step 3a: Settle the `terms/` line — one `du`, on the EXISTING bundle, now**

Moved here from Task 14 on purpose *(review round 2)*: the disk gate that Task 14 Step 2 asks the owner to rule on is **sensitive to this number**, and measuring it after the ruling, on the new bundle, informs nothing. It is seconds of work on a bundle this task is already using.

```bash
du -sh  /tmp/tessera-1e9
du -sh  /tmp/tessera-1e9/*/partitions/*/terms/
du -sh  /tmp/tessera-1e9/*/partitions/*/terms/pairs.parquet
du -sh  /tmp/tessera-1e9/*/partitions/*/terms/postings*
du -sh  /tmp/tessera-1e9/*/partitions/*/entities/
du -shc /tmp/tessera-1e9/*/partitions/*/slices/*/segments/*/columns.arrow
```

The plan's arithmetic carries `terms/` at **3.7 GiB** as a *residual* — the number that makes the before-table sum to the measured 51.1 GiB — while contracts §2.4 sizes `pairs.parquet` alone at ~5.6 GiB. **They do not reconcile, and this `du` decides which is true.** Record the result in the memo (Step 4) as its own short section, and:

- if `terms/` is ≈ 3.7 GiB, the arithmetic stands and the after-total is ~47.0 GiB;
- if it is ≈ 5.6 GiB, **the after-total is ~48.9 GiB**, the free-space margin narrows, and Task 14 Step 2's report to the owner must carry the corrected number;
- either way, note what `pairs.parquet` actually is against contracts §2.4's estimate. A material miss is a finding about the synthetic corpus's term distribution and belongs in the record, not in a silent correction to the table.

**Correct the "Arithmetic" section of this plan in the same commit** if the measurement moves it. A plan carrying a figure its own task has refuted is worse than one that never measured.

```bash
cargo build --release
cargo build --release --features tessera-engine/skip-id-index
python3 scripts/discriminate_tail.py --bundle /tmp/tessera-1e9 --out probes/tail-discrimination.json
```

Each arm boots the server (~177 s) and runs ≥ 2,000 viewports per k. Budget ~90 minutes wall.

- [ ] **Step 4: Write the verdict**

`docs/design-memos/2026-07-30-tail-discrimination.md` states, in this order: the four arms' tables; the `majflt` deltas; **Step 3a's component `du` and its consequence for the disk gate**; and one of exactly two verdicts:

- **CONFIRMED** — the `p99 − p50` gap falls materially between arms A→D and tracks the major-fault delta. Proceed; Task 15 predicts arm-D-like numbers on the new format *without* the feature flag.
- **REFUTED** — the gap survives at arm D with near-zero major faults. **STOP and report to the owner.** The format change remains architecturally justified (the identity argument stands on its own, and D5/D7 are owner decisions independent of the tail) but the plan's performance claim does not, and the owner should decide whether the rebuild is still worth its disk and its irreversibility. Record what the tail *did* correlate with.

Do not soften a refutation. A plan that reports its own premise dead is worth more than one that quietly reframes it.

- [ ] **Step 5: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash scripts/check-layers.sh
git add scripts/discriminate_tail.py docs/design-memos/2026-07-30-tail-discrimination.md probes/tail-discrimination.json crates/tessera-engine/src/session.rs crates/tessera-engine/Cargo.toml crates/tessera-store/src/external_ids.rs
git commit -m "test(probes): discriminate the constant viewport tail against page residency"
```

---

### Task 3: The identity construction — specify it before anything is written

> ## ✅ Task 3 is EXECUTED and COMMITTED (`acfe2ce`) — and partially superseded by the 2026-07-30 priority decision
>
> Both outputs are in the tree: `docs/design-memos/2026-07-30-tessera-id-construction.md` and `reference/vectors/tessera_id.json`. **Do not re-run this task.** Step 4's independent gate has also completed and it passed: the reviewer reimplemented the construction in Python **from the memo's text alone** and reproduced **all 57 vectors plus all 10 key-schedule values — 114/114, first run**. Nothing in the memo's §1.1–§1.9 references `priority`, so **the gate survives the priority redefinition untouched and `reference/vectors/tessera_id.json` needs no amendment at all.** Tasks 5 and 12 may proceed against both artifacts as committed.
>
> **What must be amended in the committed memo, and who owns it: Task 4 Step 0.** The memo is the reviewed record of a decision that has since moved, and a memo that quietly drops a condition misrepresents the review it records. Four amendments, no more:
>
> | memo location | amendment |
> |---|---|
> | §3.2, the `priority` bullet (memo:443–456) | **Superseded.** Replace the prohibition with the retirement argument: `priority` is `high16(tessera_id)`, a keyed function of a value the payload already carries in full, so there is no unkeyed residue to forbid. **Keep the general rule at memo:454–455 verbatim** — *a hot column may be shown only if it is independent of the entity ID, or keyed under the deployment key* — because it is what **licenses** the change rather than being collateral to it. Delete the "Open interaction, flagged not resolved" block (memo:457–465), whose condition has now been met. |
> | §5a, precondition 4 (memo:582–584) | **Restate as satisfied differently, never silently drop.** §5a says in terms that *"a memo that omits them misrepresents the review"*. The 8-round `splitmix64` ruling was made conditional on three fixes, the third being the `priority` prohibition. That channel is now closed **by keying `priority`, not by forbidding it** — the condition is met, and the round-function ruling still rests on something real. **This is the amendment most likely to be missed.** |
> | §6, Determinism (memo:601–607) | **Superseded.** `sort_batch` orders by `(morton, tessera_id)`; `tessera_id` **is** the sort key, so the memo's *"it is a column value written at the row, not a sort key"* inverts — the identity is computed **before** the tiler. Transcribe "The sort and tiebreak statement" and "The build sequence inverts" from this plan; do not paraphrase. Add that row order is now key-dependent and that rotation therefore reorders tied rows. |
> | §6, the storage argument (memo:609–614) | **Unaffected in substance** — `tessera_id` is still stored rather than recomputed per gathered mark, for the same reason. But it sits immediately after the sentence being replaced and **will read oddly if §6 is edited in isolation**; check the join. |
>
> Everything else in the memo stands: §1's construction, the key lifetime and flags, the N-1 refusal, the config file, the epoch, the threat model's other limbs, the width and prefix arguments, the sidecar sections, and the honest limitation.

The bijection is the load-bearing novelty of this change and the one thing the Python oracle must reproduce **from spec text alone**. This task produces the reviewable artifact and the shared test vectors, before either implementation exists, so that neither can be written by reading the other.

**Files:**
- Create: `docs/design-memos/2026-07-30-tessera-id-construction.md`
- Create: `reference/vectors/tessera_id.json`

**Interfaces:**
- Consumes: the "The identity construction — specified, not assumed" section above; `.ignore/tessera-architecture-design.md` §13.3 (row-range shards), §12 (partitions), §16 (entity-ID exhaustion); contracts §1, §2.6.
- Produces: the memo Task 4 lands in the spec, and the known-answer vectors Tasks 5 and 12 both test against.

- [ ] **Step 1: Write the memo**

`docs/design-memos/2026-07-30-tessera-id-construction.md` must cover, each as its own section, each argued rather than asserted:

1. **The construction** — input encoding, key schedule, round function, round count, forward and inverse pseudocode, exactly as given above, **including the explicit `0, 1, …, 7` round enumeration, the lowercase-hex canonical form, and the degenerate-key rejection** (`k1 == 0`, all-zero). Written so that a competent Python programmer with no access to the Rust can implement it correctly on the first try. Record that the construction was **independently verified invertible** in review round 2 and is not to be changed.
2. **Key lifetime and location** — per-deployment/per-lineage, in MANIFEST, the **seven** `tessera build` flags, **the N-1 refusal (a build with no explicit key decision exits non-zero before any work)**, the other refusal rules, and the fail-closed handling of an absent `identity` object. **State plainly the consequence of getting this wrong:** every client-held identifier silently breaks on rebuild, and a stdout warning on a ninety-minute build is not a gate.
2b. **The deployment config file** *(owner ruling Q6)* — `--id-key-file <path>` is where the key lives outside the bundle, so a deployment rebuilt from source keeps its lineage. Record: the minimal TOML shape; that unknown sections are ignored but an unknown key inside `[identity]` is an error; that there is **no default search path and no environment variable**, because a file the binary finds on its own is not a human deciding and would defeat N-1; that `--mint-id-key` prints and does not write; and the owner's stated direction that this file will grow into *"something similar to elastic index configuration"*. **Do not design that file in the memo** — one flag, one shape, one sentence of direction.
2a. **`tessera_id` is a transport identifier, and the identity epoch** — stable across rebuilds, **not** across repartitioning or resharding; the churn is *partial*, so a stale ID names a **different item** rather than 404ing; consumers persist `external_id`; the epoch rides the §12.5/§10.2 prefix flip, is advertised on `/meta`, and may be presented on `/v1/items` for a `409`.
3. **Threat model** — what the key defends (**a viewer-plane** client cannot derive or order entity IDs) and what it does not (a bundle-holder can invert everything, and that is fine). **State explicitly that the control-plane principal is outside the defended set and obtains chosen-plaintext pairs by construction** — `/status`'s `entity_id_high_water` plus `/control/ingest`'s returned `tessera_id`s over a dense monotone allocator — because the round-function ruling depends on that premise and a later reader must not have to reconstruct it. The consequences: no special key handling needed; the key must never leave the server on any plane; rotation is a breaking change and is not hygiene; ~~**`priority` must never be emitted on the viewer plane** (unkeyed `splitmix64` of the entity ID)~~ — **superseded 2026-07-30: `priority` is `high16(tessera_id)` and the prohibition is retired; see the box at the head of this task.**
4. **Width** — `u64`, and why narrowing to 32 bits for a single-shard deployment is refused.
5. **Which prefix** — §13.3 row-range shard, not §12 partition; `shard_id = 0` in Phase 1. **Reproduce the three-part resolution** from "Which prefix" above (one global `Manifest::entity_id_high_water` and one `Allocator`; §12.4's partition identity is a content hash, not a dense integer; shards do not exist in the format while partitions do), and record only the *narrowed* residual — whether a future multi-shard deployment allocates entity IDs per shard — as an open question.
5a. **The preconditions the ruling is conditional on** — the allocator cap at `u32::MAX`, `forward`'s checked conversion, the control-plane threat-model statement, and the `priority` prohibition **— which since 2026-07-30 is *satisfied differently* rather than dropped: the channel is closed by keying `priority`, not by forbidding it.** The Feistel ruling is *conditional on these three fixes*; a memo that omits them misrepresents the review.
6. **Determinism** — pure function, no seed threading, byte-equality between build paths, and the explicit statement of the storage sort order — **which since 2026-07-30 is `(morton, tessera_id)` with no further tiebreak, computed before the tiler runs, not `(morton, priority, entity_id)`.**
7. **What remains in sidecars** — the two directions, their cadences, and the explicit statement that no `tessera_id → entity` sidecar exists. **Plus, and these are the two owner rulings the memo must not omit:** *(a)* under **Ruling A** the sidecar is a translation table holding rows only for items whose caller supplied a key — an item with no caller key has its `tessera_id` as its identity, occupies no extent row, and must never have one manufactured for it; *(b)* under **Ruling B** the whole structure is a **placeholder** for a future adopted metadata store, so it stays minimal, both directions sit behind one module surface, and Appendix D — which rejects adoption for the *access-control layer* — does not forbid adoption for a cold store that never participates in masking. State the fail-closed and off-the-request-path conditions any replacement inherits.
8. **The honest limitation, and the ruling already taken** — `splitmix64` is not a cryptographic PRF; this is a blinding permutation whose security rests on the *viewer* never obtaining known plaintext pairs. **Review round 2 ruled: keep `splitmix64` at 8 rounds**, conditional on the three fixes in §5a; second choice was keyed SipHash-1-3 at ~80–120 s added to a ninety-minute build (~2%). Record the ruling and its conditions, state the trade against "design for audit before performance", and record the fallback that was *not* taken: a 128-bit random ID, collision-free in practice and needing no crypto in the oracle, at 26 B/row ≈ 25.0 GiB — *worse than today* — which reopens the residency problem. **This point is now a record, not an open question**; Step 4's review must not re-litigate it unless it finds an error in the round-2 verification.

- [ ] **Step 2: Generate the known-answer vectors**

`reference/vectors/tessera_id.json`: a fixed key and at least 32 `(shard_id, entity_id, tessera_id)` triples covering `entity_id ∈ {0, 1, 2, 0xFFFFFFFF, 0x7FFFFFFF}`, `shard_id ∈ {0, 1, 0xFFFFFFFF}`, and a spread of random values. Compute them **by hand or by a throwaway script written from the memo** — not by running Task 5's implementation, which does not exist yet.

```json
{
  "construction": "feistel-splitmix64-v1",
  "rounds": 8,
  "key": "000102030405060708090a0b0c0d0e0f",
  "vectors": [{"shard_id": 0, "entity_id": 0, "tessera_id": "0x…"}, …]
}
```

Both Task 5 (Rust) and Task 12 (Python) test against this file. If the two implementations disagree with each other but agree with the file, the file is wrong; if they disagree with the file, the implementations are. That is the point of generating it first.

- [ ] **Step 3: Sanity-check the bijection property empirically**

A throwaway script: for one fixed key, encrypt every `entity_id` in `0..2²⁴` at `shard_id = 0` and assert the outputs are distinct, and that decrypt∘encrypt is the identity. This proves nothing a Feistel does not prove structurally, but it catches a transcription error in the round loop, which is the realistic failure. Record the result in the memo; do not commit the script.

- [ ] **Step 4: Dispatch for independent review — this is the gate**

Per CLAUDE.md's working method, hand the memo to a subagent to review against the design corpus and the invariants, with no stake in it being right. The review must answer, in these words:

- Does the construction as written reproduce identically in a second language from this text alone? *(The round enumeration and the hex-case rule are the two places round 2 found ambiguity; check them specifically.)*
- **Is the N-1 refusal sufficient, now that `--id-key-file` is a fourth accepted source?** A build with none of `--carry-id-key-from` / `--id-key-file` / `--id-key` / `--mint-id-key` must exit non-zero before any work. `--id-key-file` counts as an explicit decision **only** because the operator typed the path. Is there any path — a default search location, `$TESSERA_CONFIG`, a default in a Makefile or CI job, `scripts/build_full.sh` — that could supply any of the four without an operator having decided? The config file makes this question *harder* than it was, which is why it is asked again here.
- **Is the identity epoch sufficient to make a stale `tessera_id` detectable rather than silently wrong?** The failure being defended is a *partial* churn at repartitioning, where a stale ID inverts validly to a different item.
- Does anything in the construction violate I2, I4, I9, I10 or I13?

**Not in scope for this review:** whether `splitmix64` is the right round function. Review round 2 verified the construction invertible and ruled to keep it at 8 rounds, conditional on the allocator cap, the control-plane threat-model statement and the `priority` prohibition. Re-open it **only** if you find an error in that verification, and say what the error is.

**Act on the review before Task 5 writes a line of code.**

- [ ] **Step 5: Commit**

```bash
git add docs/design-memos/2026-07-30-tessera-id-construction.md reference/vectors/tessera_id.json
git commit -m "docs: specify the tessera_id keyed bijection, its key lifetime and its threat model"
```

---

### Task 4: The documents — design r21, contracts r6

The format is a contract before it is code. This task lands the spec so Tasks 5–13 have something to conform to, and so the single rebuild in Task 14 is built against a final, written format.

**Precedence note for the executor:** the design is the specification. Contracts §0.3 deviations govern only where the contracts spec and the *system architecture* differ; they do not override the design. Where the design's I10 mechanism clause, §2.6 step 10 and §10.6 conflict with this change, **the design is amended in the design** — that is Critical C-3 and it is why this task edits the design first.

**Files:**
- Modify: `.ignore/tessera-architecture-design.md`
- Modify: `.ignore/tessera-contracts-spec.md`
- Modify: `docs/design-memos/2026-07-30-tessera-id-construction.md` *(Step 0 — Task 3's committed memo, four amendments only)*

**Interfaces:**
- Consumes: Task 1's memo (the premise), Task 2's verdict (the motivation), Task 3's memo (the construction), the priority-as-prefix memo (`docs/design-memos/2026-07-30-priority-as-identity-prefix.md`).
- Produces: the normative schemas Tasks 5–13 implement.

- [ ] **Step 0: Amend Task 3's committed construction memo for the priority redefinition**

Task 3 shipped at `acfe2ce` stating the plan's *then*-current position on `priority` and flagging the conflict inline. This task owns bringing it into line, because it is the task that lands the construction in the spec and the memo is the spec's source. **Exactly the four amendments enumerated in the box at the head of Task 3** — §3.2's `priority` bullet (keeping the general rule at memo:454–455, which licenses the change), §5a's precondition 4 restated as *satisfied differently*, §6's determinism statement and build-sequence inversion, and the join into memo:609–614's storage argument. **`reference/vectors/tessera_id.json` needs no amendment** and must not be touched: the memo's §1 does not reference `priority`, and the independent gate reproduced all 57 vectors and all 10 key-schedule values from the memo's text alone (114/114). Commit it with this task's documents commit, adding the path explicitly.

- [ ] **Step 1: Amend the design's I10 mechanism clause (`:143`) — Critical C-3**

The *substance* of I10 is preserved verbatim; only the mechanism clause changes:

```markdown
**I10 — Entity IDs never cross the trust boundary.** They are dense and assigned in
signature order, so the gap between two visible IDs is a count of unauthorised items.
Clients receive an opaque `tessera_id` instead — a keyed permutation of
`(shard_id, entity_id)` under a per-deployment key (contracts §2.6), which is
order-free, collision-free by construction, and invertible only inside the trust
boundary. *(r21; previously "per-session opaque handles", retired from the viewer
plane by owner decision — see Appendix G.)* This is also what makes the
ingest-order optimisation in §11.1 safe (C6).
```

- [ ] **Step 2: Amend §2.6 step 10 (`:100`) and §10.6 (`:445`) — Critical C-3**

§2.6 step 10:

```markdown
10. **Translate out.** Row IDs to `tessera_id`s — read directly from the gathered row,
    since the identity is stored where it is shown from. Entity IDs never cross the
    boundary (§10.6, **I10**).
```

§10.6, first paragraph — **and it must cross-reference C17** (Critical N-2), so the cost of the mechanism change is one hop from the sentence that makes it:

```markdown
Responses are Arrow IPC; typed arrays go straight into GPU buffers with no parsing.
Points carry an opaque `tessera_id` rather than an entity ID (**I10**), which the
server inverts on drill-down. *(r21; previously per-session opaque handles. The
handle mechanism is retained for Phase 3's node handles, where the identity is
genuinely per-session; a point's identity is not, and a stable identifier is what
lets a client bookmark, share or reconcile a point across sessions.)* The point
identity and the authorisation token are different objects. **A stable identity is
linkable across sessions and across principals by construction — see Appendix C's
C17, which records what that costs and why it is the intended trade.** The identity
is a *transport* identifier: it survives rebuilds, but not a repartitioning (§12.5),
which advances the identity **epoch** at the §10.2 prefix flip. A consumer that
persists an identity persists the caller's `external_id`, not this one.
```

- [ ] **Step 3: Revise Appendix C's C6, and add C17 (Critical N-2)**

Replace the C6 row with the row given in "C6 revision" above, changing the channel name to `External ID gaps on the wire`.

**Add the C17 row** exactly as given in the "C17" block above — *Stable wire identity across sessions and principals*, **Medium**, **Accepted — the point of D5**. CLAUDE.md's rule is that anything not in the table is a bug, and retiring the per-session handle opens two channels the handle closed as a side effect (existence-over-time probing on a held ID; cross-principal correlation). They are accepted, not residual, but they must be **in the table** to be accepted at all.

Add both to the closing line's list of entries needing an owner and review date before launch: `Owner and review date for C1, C4, C6, C12, C14, C15, C16 and C17 to be assigned before launch.`

**Add a C4 annotation** for the new per-click channel Critical C-5 identified — this is a *new, strong* channel that C4's existing "viewport time correlates weakly" text does not cover:

```markdown
*(r21)* `/v1/items/{tessera_id}` returns an identical `404` for "no such ID" and
"exists but not visible" — same status, code and detail, with no branch-dependent
logging or metrics. The **timing** channel on that endpoint is closed structurally
rather than narrowed: inversion of a `tessera_id` is a pure function taking no I/O,
and the visibility test that follows it is an **entity-space** question —
`fragment.contains(entity)`, adjusted by the overlay's `deleted > suppressed >
evaluate_terms` precedence and the ingest buffer, exactly as §5.2's composition
resolves it per entity. That is O(1), touches no row-space projection, and performs
**identical work for an identifier that names nothing and one that names an
invisible item**: both take the same three constant-time lookups and return the same
`404`. The endpoint therefore has no per-ID cost to correlate against. The earlier
row-space formulation — project the fragment, then test the row — would have paid a
9.5–19.3 s projection build for a known-but-invisible ID and nothing at all for an
unknown one: a per-click existence oracle four orders of magnitude wide. C4 itself
remains `Open` for the viewport path.
```

- [ ] **Step 4: Record the routing principle in §10.3**

Append to §10.3, after the sentence naming what fixed-width hot columns carry:

```markdown
**Route by access ratio, not data type** *(r21)*. The rule the sentence above is an
instance of: data read once per **rendered mark** belongs in a fixed-width hot column;
data read once per **query** belongs in an entity-space bitmap behind the filter
contract (§8.2); data read once per **interaction** belongs in a cold sidecar keyed by
the wire identity and opened on first use. A viewport draws ~10⁵ marks and a user
clicks a handful, so the three cadences are four orders of magnitude apart and the
placement decision follows from the ratio rather than from the type of the data. The
caller's external ID is the first instance decided this way: it is per-interaction, so
it is a sidecar (contracts §2.4), not a column. **The per-interaction row and §8.3's
vector sidecar name one slot, not two**: per-point metadata, the full record, provenance,
text and vectors are all per-interaction, and the intention is that a single adopted
store eventually serves them rather than each growing its own format. The external-ID
sidecar is that slot's first and deliberately transitional occupant. **Appendix D does
not bar such an adoption:** it rejects adopting a search engine, vector database or
relationship-based authorisation service **for the access-control layer**, where a wrong
or stale answer is a disclosure. A cold store read only after the mask has already
decided visibility never participates in masking; it inherits instead the ordinary
conditions — fail-closed with typed errors, integrity verified before an answer leaves
it, and off the request path. **Expanding the hot columnar store
remains an available trade** — more per-point data on the render path, paid for in
resident memory at 0.93 GiB per byte per row per 10⁹ items — and a proposal to take it
should state that number against Appendix A's budget rather than treat the store as
closed.
```

- [ ] **Step 5: Update Appendix A's residency figures**

The 10⁹ table's `Hot columns` row changes from `24 GB` to `18 GB` (and its per-shard figure from `375 MB` to `281 MB`); the 10⁷ table's `hot columns (24 B/row) 240 MB` becomes `hot columns (18 B/row) 180 MB`. Add immediately after the 10⁹ table:

```markdown
**Four columns, not five** *(r21)*. Contracts §2.6 r6 removes `node_id` (no reader
before Phase 3 — the build wrote a billion identical sentinels into a per-viewport
file) and replaces `entity_id` with the width-neutral `tessera_id`: 22 B/row →
18 B/row. The external-ID extents, which earlier revisions did not count because they
were assumed cold, were in fact mapped and linearly scanned at open; r6 makes them a
per-extent lazily-opened sidecar and they leave the residency table, at the cost of one
extent joining it after the first drill-down.
```

- [ ] **Step 6: Annotate §11.1**

Append to the paragraph that ends *"…and is safe only because of **I10**"*:

```markdown
*(r21)* Contracts r6 makes this stronger in substance while changing its mechanism.
With `columns.arrow` carrying a `tessera_id` instead of the entity ID, no request-path
artifact stores an entity ID at all — the gather cannot produce one — so the ordering
freedom this section spends on posting compression is protected by construction and not
only by a discipline at the serialisation chokepoint. What the viewer sees instead is a
keyed permutation of `(shard, entity)`, which is order-free: signature order does not
survive it, and gaps in it count nothing. The residual channel is a caller's own
external IDs where the caller chooses to carry structure in them, which is C6 as
revised.
```

- [ ] **Step 7: Record the unsettled question in §16**

Add to §16's open questions, adjacent to the entity-ID-exhaustion entry:

```markdown
**Entity IDs are globally unique across §12 partitions; the identity's prefix is the
§13.3 shard** *(r21)*. The `tessera_id` construction (contracts §2.6) encodes
`(shard_id: u32, entity_id: u32)`. Three facts settle which discriminator that is, and
they are recorded here because the question keeps being asked: the entity-ID high-water
is a **single** bundle-level value with a **single** allocator, so two items in
different partitions cannot share an ID; §12.4 fixes partition identity as *a canonical
hash of the sorted required set*, because partitions are discovered rather than
declared, and a content hash is not a dense small integer; and §12 partitions exist in
the bundle format today while §13.3's row-range shards do not. A partition component in
the identity input would therefore encode a constant, and could not be a `u32` in any
case. `shard_id` is a **reserved field**, valued 0 for as long as §13.4 rules sharding
premature.

What remains open is narrower, and is a *consequence* of the exhaustion entry above
rather than of this construction: if a future multi-shard deployment allocates entity
IDs **per shard**, the reserved prefix becomes load-bearing and the 32/32 split is
exactly right; if it keeps allocating globally, the prefix stays 0 and the four bytes
buy only the option. The encoding is the same either way, so nothing is blocked.
```

- [ ] **Step 7a: Land the priority redefinition in the design — §7.2, §12.3, and the four places that describe the sort order**

*(Added by the 2026-07-30 fold. The design is the specification, so this is where the redefinition becomes normative; contracts §2.6 in Step 10 implements it.)* Read each line before editing — the wording differs at each and a search-and-replace will produce nonsense.

| design line | what it says now | amendment |
|---|---|---|
| `:240` (**§7.2**, the definition) | *"Every item carries a fixed pseudo-random **priority**, derived by hashing its entity ID."* | Derived as the **high 16 bits of the item's `tessera_id`** (contracts §2.6), which is a keyed permutation of `(shard_id, entity_id)`. Keep the nesting argument **verbatim** — it depends only on priority being a fixed per-item constant and mask-independent, which it still is. **Add the reason for the change**, in §7.2's own terms: at 65,536 distinct values the *k*-th lowest priority is resolvable only when V ≤ 2¹⁶·*k* (V ≈ 2×10⁶ at *k*=30), and above that the tiebreak was the entity ID — signature-sorted under I9 — so the sample was ordered by permission signature. That is the failure this section rejects for global LOD sampling, moved inside the visible set; it is the argument for the redefinition and it belongs in the section that makes it. |
| `:97` (§2.6 step 7) | *"Priority is a hash-derived per-point constant and mask-independent"* | *"a keyed per-point constant — the high 16 bits of the item's `tessera_id` — and mask-independent"*. The rest of the step is unchanged. |
| `:248` (§7.2, direct evaluation) | *"read their priorities, keep the k lowest"* | Add that the comparator **falls through to the full `tessera_id` on prefix ties**, and that this is identically "the *k* lowest by `tessera_id`" because the prefix is a prefix — so there is no composite comparator to get wrong and the sample is correct at any prefix width. Note the width is a **performance** knob: fall-through volume ≈ V/2^w, and the revisit trigger is `w ≈ log₂(V_max/k)` (~24 bits for a 10⁹ shard at head coverage). Phase 1 implements no such comparator — the sampler is the placeholder first-k — so this is spec text ahead of code, and say so. |
| `:246` (§7.2, candidate lists) | *"the top c·k items by priority, unmasked"* | Unchanged in form; annotate that "by priority" now means "by `tessera_id` prefix", so the lists inherit the fix rather than needing their own. |
| `:163` (§5.2), `:411`, `:590` (§14) | *"priority as the intra-leaf tiebreak"* / *"derive per-item priorities; sort and assign row ranks"* | State the order as **`(morton, tessera_id)`** and, at `:590`, invert the build sequence: the identity is derived **before** the sort, and the priority column is a projection of the sorted identity column. Transcribe from "The build sequence inverts" above. |
| `:525` (**§12.3**) | *"Because priority is a global per-item property, the k lowest-priority visible items in a tile equal the k lowest of the union of each partition's k lowest."* | The claim is **restored, not amended**: under D8's shard-local `u32` entity IDs, `splitmix64(entity_id)` had stopped being a global per-item property — item 12,345 carried an identical priority in every shard and `(priority, shard_id, entity_id)` made shard 0 win every tie. `high16(tessera_id)` is global by construction because the shard is part of the bijection's input. Record the defect and its repair here; the composition argument itself stands verbatim. |

**Do not touch `:845`** (Appendix G's r19 paragraph, which records the priority function as fixed at splitmix64-high-16). It is a historical record of what r4 decided and must stay true to that; r21's paragraph is where the change is recorded.

- [ ] **Step 8: Bump the design to r21**

Bump the status line to `revision 21` and prepend to Appendix G:

```markdown
- **r21** — The boundary identity (owner decision, 2026-07-29), companion to the
  contracts spec's r6. **I10's mechanism clause changes and its substance does not**:
  clients receive an opaque `tessera_id` — a keyed permutation of
  `(shard_id, entity_id)` under a per-deployment key — instead of a per-session handle.
  §2.6 step 10 and §10.6 amended to match; the handle mechanism is retained for Phase
  3's node handles. Entity IDs still never cross the boundary, and after r6 no
  request-path artifact stores one at all, so §11.1's signature-sorted assignment is
  protected structurally rather than by a serialisation-time discipline. Appendix C's
  **C6 moves from `Closed` to `Accepted — caller's control`**: the entry claimed
  closure by handles, and the residual disclosure is now a caller's choice to use
  structured external IDs and export them — C12's shape, register hygiene rather than a
  new exposure. **A new C17** records what retiring the handle costs and accepts it:
  a stable identity is linkable across sessions (existence-over-time probing on a held
  ID) and across principals (out-of-band correlation), both bounded to items the
  principal already sees, and both the *point* of D5 rather than residuals; §10.6
  cross-references it. **C4 annotated** with a structural closure of the `/v1/items`
  timing channel — the endpoint's visibility test is an entity-space question answered
  in O(1) with identical work for an unknown identifier and an invisible one, so the
  channel is closed rather than narrowed. `tessera_id` is a **transport** identifier:
  stable across rebuilds, not across §12.5's repartitioning, which advances an identity
  **epoch** at §10.2's prefix flip; consumers persist `external_id`. Appendix A's
  hot-column row corrected to 18 B/row; the external-ID extents
  leave the residency table. §10.3 records the **routing principle** (per-mark column,
  per-query bitmap, per-interaction sidecar), the deliberate hot-column trade, and that
  the per-interaction row and §8.3's vector sidecar are **one slot** whose first occupant
  — the external-ID store — is explicitly transitional, with the note that **Appendix D
  bars adoption for the access-control layer and not for a cold store off the request
  path**. §16
  records that entity-ID uniqueness across §12 partitions is unsettled. No invariant
  changes in substance.

  **`priority` is redefined as the high 16 bits of the item's `tessera_id`** (owner
  decision, 2026-07-30), and the storage sort order becomes **`(morton, tessera_id)`**
  with no further tiebreak. Same column, same `u16`, **zero bytes changed**. Two defects
  are repaired. §7.2's sample was resolvable only while V ≤ 2¹⁶·*k* — V ≈ 2×10⁶ at
  *k*=30 — and above that threshold the tiebreak *was* the sampler; the tiebreak was the
  entity ID, which §11.1 assigns in signature order, so **the sample was ordered by
  permission signature**, keeping I7's letter and breaking its purpose, at the default
  overview, for head principals, with candidate lists inheriting it. And §12.3's
  composition argument had quietly lapsed: under D8's shard-local `u32` entity IDs
  `splitmix64(entity_id)` was no longer the global per-item property the argument names.
  A keyed bijection over 2⁶⁴ is global, uniform and uncorrelated with signature, and
  because the `u16` is a *prefix* of it, "*k* lowest by priority then by `tessera_id`" is
  identically "*k* lowest by `tessera_id`" — so prefix width becomes a performance knob
  only. Consequences recorded rather than hidden: row order is now **key-dependent**, so
  a key rotation reorders tied rows as well as invalidating identifiers; the sample
  reshuffles on a re-key as well as on a reshard; and the uniformity of the Feistel's
  high bits under structured inputs is **verified by a chi-squared check at build**, not
  assumed. The identity swap's viewer-plane prohibition on `priority` (contracts r6) is
  **retired by argument**: 16 bits of a keyed identity the payload already carries in
  full discloses nothing, since the cut *P* is determined by *k* and the masked count
  §7.1 already gives. No leak-register entry is required.
```

- [ ] **Step 9: Add four deviations to contracts §0.3**

Append after deviation 5:

```markdown
6. **`columns.arrow` carries `tessera_id`, not `entity_id`** *(r6)*. The row→entity
   direction (deviation 2) becomes the row→**wire identity** direction: the identity
   the service shows is stored at the row it is shown from, so internal→external needs
   no lookup, and external→internal needs none either because `tessera_id` is an
   invertible keyed permutation of `(shard_id, entity_id)` (§2.6). It is width-neutral
   against the `entity_id: uint64` it replaces. Entity IDs are thereby absent from
   every request-path artifact except `permutation.bin`'s *index*, which strengthens
   **I10** in substance while changing its mechanism (design r21). Source: owner
   decision 2026-07-29.

7. **`node_id` is removed from `columns.arrow`** *(r6)*. §2.6's `node_id: uint32` had
   no reader: clustering is Phase 3, and the build wrote a billion identical
   `0xFFFFFFFF` sentinels — 4 GB of a file that is read per viewport. Phase 3's node
   table (§2.9) re-adds a row→node column as an additive change when it acquires a
   reader; nothing about that reservation requires the column to exist empty meanwhile.

8. **Per-session point handles are retired from the viewer plane** *(r6)*. §5's
   "every identity on the viewer plane is a per-session `u32` handle" becomes
   `tessera_id`. Handles remain the mechanism for Phase 3 **node** handles, where the
   identity genuinely is per-session. A point's is not: a stable identifier is what
   lets a client bookmark, share and reconcile a point across sessions, and the handle
   bought nothing that `tessera_id`'s opacity does not (design r21, C6 as revised).

9. **External-ID resolution is a per-extent lazy sidecar, not a hot-path structure**
   *(r6)*. `entities/external-ids-<k>.arrow` narrows `entity_id` to `uint32` (§1's
   `< 2³²` bound) and gains a companion `entities/ext-locator.u32` for the
   drill-down direction. Both are **exempt from the §2.3 reader protocol's readiness
   gate**: an extent is digest- and sortedness-verified on **its own** first use, never
   at open, and an extent that is never resolved against is never mapped. Rationale: at
   10⁹ the eager scan of 18.9 GB of extents at open put the whole family in the
   resident set for a path that never reads it. **There is no `tessera_id → entity`
   sidecar** — inversion is a pure function.
```

- [ ] **Step 10: Update contracts §1, §2.1, the MANIFEST table, §2.4, §2.6, §3 and §5**

**§1** — entity IDs narrow to `u32` on disk (D8). **The blanket statement contradicts §2.4**, which sizes `terms/pairs.parquet` as `(entity_id: uint64, term_id: uint32)`; the contradiction must be resolved in the text rather than left for a reader to discover:

```markdown
- All integers little-endian. IDs: entity `u32` in every fixed-width on-disk array and
  Arrow column *(r6; was `u64`. The `< 2³²` bound was always asserted for
  `bundle_format = 1`, the 4B-per-shard cap makes it exact, and the `tessera_id`
  bijection (§2.6) depends on it — the allocator refuses to issue an ID at or above
  `u32::MAX`; §16's exhaustion answer will bump the format)*, row `u32`, term `u32`,
  `tessera_id` `u64` (§2.6); node IDs and external IDs are caller-supplied (node IDs
  UTF-8 ≤ 256 bytes; external IDs byte strings **≤ 64 bytes** *(r6; was 256. Tightened
  because nothing in the format sized the sidecar for the old cap: at 10⁹ a 256-byte
  key costs over 250 GB of extents. 64 bytes covers a 36-character UUID string, a ULID,
  an ObjectId and ordinary business keys, and the contract is open exactly once — after
  a caller depends on longer keys, tightening is breaking)*. An over-length external ID
  is a **typed error at ingest and at build, never a truncation** — a truncated key is a
  different key, and two keys sharing a 64-byte prefix would collide into one entity —
  **and sidecar disk scales linearly with key length: the 10⁹ sizing in §2.4 assumes
  short keys, and a deployment near the cap pays proportionally**). **One recorded exception to the
  narrowing:** `terms/pairs.parquet` (§2.4) keeps `entity_id: uint64`. It is
  `DELTA_BINARY_PACKED`, so the declared width costs almost nothing on disk, and it is
  read only by build machinery and the DuckDB oracle — never on a request path, and
  never mmap'd as a fixed-width array. Narrowing it is available and deferred; it is
  not a contradiction once stated.
```

**If the executor finds any *other* `uint64` entity field surviving in §2 or §3 that this text does not carve out, STOP and report** — the point of writing the exception down is that there is exactly one.

**§2.1 layout tree** — replace the `entities/` block:

```
      entities/external-ids-<k>.arrow   # caller external id -> entity, byte-sorted;
                                        # sidecar, per-extent lazy (0.3 dev 9)
      entities/ext-locator.u32          # entity -> ordinal in the above; drill-down.
                                        # ONE file, not one per extent: it is indexed
                                        # by the global entity id, so a per-extent
                                        # family would be N full-length copies
```

**MANIFEST table (§2.2)** — add the `identity` object:

| `identity` | object | `{construction, rounds, key, shard_id, epoch}` — the `tessera_id` permutation (§2.6). **Required**; absent is a typed reader error, not a default. `key` is **exactly 32 lowercase hex characters** (readers reject any other case rather than folding, so MANIFEST has one canonical form under its digest), per *deployment*, carried across rebuilds; degenerate keys (`k1 == 0`, all-zero) are refused at both write and read. `epoch` is a `u32` advanced whenever the partitioning or sharding changes — see the transport-identity note below. **The key's home outside the bundle is a per-deployment configuration file named explicitly on the command line** (`tessera build --id-key-file <path>`), so a deployment rebuilt from source keeps its lineage; there is **no default search path and no environment variable**, and a build given none of `--carry-id-key-from` / `--id-key-file` / `--id-key` / `--mint-id-key` **refuses before doing any work**. The file's wider schema is not specified here |

**§2.2 also gains the transport-identity statement**, because it is a promise to consumers and belongs where the object is defined:

```markdown
**`tessera_id` is a transport identifier, not a durable key** *(r6)*. It is stable
across rebuilds — that is what carrying `identity.key` forward buys — and it is **not**
stable across a repartitioning or a reshard, because the permutation's input encodes
placement and §12.5's reindex moves points. The churn is **partial**, which is why a
signal is required rather than merely useful: without one, an identifier that named a
moved point does not fail, it silently names whichever entity now occupies that input.
`identity.epoch` is that signal. It is carried forward verbatim by a normal rebuild,
**must** be advanced by any build whose partitioning or sharding differs from the
bundle it carried the key from (refuse otherwise), and is reset to 1 by a key rotation.
`GET /meta` reports it as `identity_epoch`; `POST /v1/items/{tessera_id}` accepts an
optional `epoch` and answers `409 conflict` — *"stale identity epoch; re-resolve by
external_id"* — when it does not match. **Optional rather than required, deliberately:**
the durable identifier is `external_id`, so a consumer following this contract has no
stale `tessera_id` to present; and rotation and repartitioning are deliberate breaking
changes rather than scheduled hygiene, so the epoch fires approximately never and
requiring it would put friction on every drill-down forever to guard a
once-in-a-deployment event. A client that omits it accepts that after a repartitioning a
stale identifier may name a different item. That branch is entity-independent, taken before
inversion and identical for every identifier, so it opens no channel (Appendix C, C4).
**Consumers persist `external_id`** (§1, SA D14) and treat `tessera_id` as valid only
for the epoch it was issued under.
```

**§2.4** — replace the external-IDs bullet:

```markdown
- `entities/external-ids-<k>.arrow` — Arrow IPC `(external_id: binary,
  entity_id: uint32)`, sorted bytewise within each extent and across extents in listed
  order; extent 0 at build, one per flush after. The caller-supplied namespace (§1, SA
  D14), addressing `/control/changes` past WAL retention and `/control/ingest`'s
  duplicate check. **Sidecar (§0.3 deviation 9):** a reader opens the *one extent* a
  key falls in — selected by an O(extents) first-key/last-key scan that maps nothing —
  verifies that extent's MANIFEST digest and its sortedness at that point, and binary
  -searches. Nothing is mapped, scanned or verified at open, and `readyz` does not
  require it.
- `entities/ext-locator.u32` — **one** raw `u32` array, no header, no `<k>` suffix,
  length `entity_id_high_water` at build, indexed by entity ID, giving that entity's
  ordinal in the concatenated sorted external-ID extents; `0xFFFFFFFF` for an entity
  with no caller external ID. This is the **drill-down** direction
  (`entity → external_id`): `/v1/items` inverts `tessera_id` to an entity, indexes here,
  and reads that ordinal's key from the extent it falls in. Held as a locator rather
  than a second entity-ordered copy of the keys because the keys already exist in sorted
  form — 4 B/row against 12, which at 10⁹ is 3.7 GiB against 11.2. It is **singular by
  necessity, not by convenience**: an entity ID says nothing about which extent its key
  sorts into, so a per-extent family would need a full-length array *per extent* — 37
  GiB at ten extents rather than 3.7. Same lazy read protocol. **Never emitted on the
  viewer plane except as the drill-down response's `external_id` field** — the
  conformance byte-scanner sweeps for external IDs in viewport payloads and logs
  (conformance design §4.3).
- **Entities ingested after the build have no locator slot and no extent entry.** The
  drill-down resolves them from the server's live external-ID map first and the locator
  second; an entity at or below the allocator high-water that neither accounts for is a
  **typed error**, not an absent external ID. Reading past the array and returning
  "no external ID" for an item that has one is a wrong answer wearing a legitimate
  state's clothes.
- **One identity, supplied or derived** *(r6)*. An item's identifier is the caller's
  `external_id` when the caller supplies one and its `tessera_id` when the caller does
  not — in which case the identifier is derived, costs nothing to store, and **has no
  row here**. This store is a *translation table* between two representations of one
  identity, not a store of identities: a deployment whose callers supply no external IDs
  writes no extents and no locator at all, and nothing in the build or the ingest path
  may manufacture an external ID for an item that has none. The `0xFFFFFFFF` locator
  sentinel is the ordinary case, not a missing value.
- **This store is TRANSITIONAL** *(r6)*. It is deliberately the simplest structure that
  satisfies its two callers, and it is a **placeholder for a future adopted per-point
  metadata store** — the same slot as §8.3's vector sidecar and the design's
  per-interaction routing row (§10.3), which that store would serve together. Extend the
  replacement rather than this. **Design Appendix D does not forbid that adoption:** it
  rejects adopting a search engine, vector database or relationship-based authorisation
  service **for the access-control layer**, where a wrong or stale answer is a
  disclosure. A cold metadata store read only *after* the visibility test has returned
  "visible" (§3.2) never participates in masking and is a different question. Whatever is
  adopted inherits three conditions unchanged: **fail-closed with typed errors — never a
  `None` or an "unavailable" that reads as "no external ID"**, since a wrong mapping
  suppresses the wrong item; **off the request path** (design §10.3), per interaction and
  never per mark; and **integrity verified before any answer leaves it**, whatever the
  local equivalent of the per-extent digest and sortedness check turns out to be.
- **Compression is deliberately not used here** *(r6)*. The `pairs.parquet` licence
  above — compression is permissible off the request path — would apply, and is
  declined twice over: the store is 18.6 GB of *disk*, never resident on the render path,
  so a block format, per-block digests and a decoder on an authorisation-adjacent path
  would buy disk for no latency gain; and it is machinery invested in a component
  scheduled for replacement. **The threshold at which that changes is stated rather than
  left to be discovered: once mean external-ID length exceeds ~16 bytes the store
  dominates the bundle** — at 10⁹ it is 14.9 GB for 8-byte keys, 22.4 for binary UUIDs,
  41 for 36-character UUID strings and 67 at the §1 cap, against a locator that stays
  3.7 GB throughout — **and at that point compression, or the replacement store, pays for
  itself.** Note which: dictionary or prefix encoding pays for long *human-readable* keys;
  **random UUIDs compress essentially not at all**, so a deployment that crosses the
  threshold on binary UUIDs is a candidate for the replacement, not for compression. The
  trigger is a **measured mean** for the deployment, never an assumption from the key type.
```

**§2.6** — replace the column table:

```markdown
| Column | Type | Notes |
|---|---|---|
| `tessera_id` | uint64 | the row→wire-identity direction (deviations 2, 6) |
| `x`, `y` | float32 | as supplied (quantisation is for codes, not storage) |
| `priority` | uint16 | the **high 16 bits of this row's `tessera_id`** — `(tessera_id >> 48) as u16` *(r6)* |
| *declared scalars* | per MANIFEST | |
```

**Delete the standalone priority-function block** (§2.6's `priority = (z ^ (z >> 31)) >> 48` over the entity ID, fixed in r4) and replace it with:

```markdown
**`priority` is a prefix of the identity, and the sort order is `(morton, tessera_id)`**
*(r6; supersedes r4's standalone priority function, which was an unkeyed `splitmix64`
over the entity ID)*. `priority = (tessera_id >> 48) as u16`. There is **one** hash
construction in this format, not two: the Feistel below, of whose output `priority` is
the leading 16 bits.

`columns.arrow` is sorted by **`(morton, tessera_id)` ascending, with no further
tiebreak**. `tessera_id` is a bijection over 2⁶⁴ and there is one row per entity, so the
order is total; and because `priority` is a *prefix* of `tessera_id`, ordering by
`(morton, priority, tessera_id)` is **identically** ordering by `(morton, tessera_id)`.
An implementation may compare the prefix first as an optimisation. **The entity ID is
not a sort key at any position.** The oracle re-derives row order as
`(morton_of(x, y, extent), FPE_k(shard_id ‖ entity_id))`, so **row order is
key-dependent**: a key rotation reorders tied rows as well as invalidating every
identifier a client holds (§2.2).

**Why the column exists at all, given that its value is a prefix of another column in
the same file.** A cheap prefix must be *physically contiguous*: reading the high 2 bytes
of a `uint64` array at stride 8 touches every page holding any value (512 `u64` per page
against 2,048 `u16`) and pulls the whole cache line regardless. This is design §10.4's
column-major argument one level down. Prefix **width** is therefore a performance knob
and not a correctness parameter — the sample is correct at any width, and fall-through
volume is ≈ V/2^w.

**`priority` on the viewer plane is permitted and unused** *(r6)*. It is 16 bits of a
keyed identity the same payload already carries in full, so it narrows nothing: a viewer
holding *k* marks learns only that their priorities fall below some cut *P*, and *P* is
determined by *k* and the exact masked count §7.1 already returns. Nothing emits it
because nothing needs it. The general rule remains: **a hot column may cross the boundary
only if it is independent of the entity ID, or keyed under the deployment key** — and an
*unkeyed* derivative of the entity ID would still be forbidden, which is what r6's
earlier drafts prohibited when `priority` was one.
```

and add, after that block, **the full construction from "The identity construction" above** — input encoding, key schedule, round function, round count, forward and inverse pseudocode — introduced by:

```markdown
**The `tessera_id` permutation is contract** *(r6)*. The oracle must reproduce the
stored column byte-for-byte from `(identity.key, identity.shard_id, entity_id)`, and
`tessera verify` checks the whole column against it. It is a **balanced Feistel
network**: a permutation for any round function, so two entities cannot share an
identity and no collision detection exists or is needed. It is a pure function, so it
is stable across restart with nothing persisted. Changing the construction, the round
count or the round function is a `bundle_format` bump; changing the *key* is not a
format change but **invalidates every identifier any client holds** (§2.2).
```

Also revise §2.6's `permutation.bin` paragraph, which currently locates an entity in a **streamed** segment "via its segment's `entity_lo`/`entity_hi` and that segment's `entity_id` column" — a column this revision deletes (**Important I-7**):

```markdown
*(r6)* An entity in a **streamed** segment is located via its segment's
`entity_lo`/`entity_hi` and that segment's own `permutation.bin`, written per streamed
segment and folded away at compaction. r5 located it through the segment's `entity_id`
column, which §0.3 deviation 6 removes; without this revision Phase 2 would inherit an
unflagged hole where the documented mechanism no longer exists. Sizing: a streamed
segment's permutation is bounded by its own `entity_hi − entity_lo`, not by the global
high-water.
```

**§3.1's `404` row:**

```markdown
| 404 | `unknown` | unknown `tessera_id`, node, external ID or slice — indistinguishable from "not visible" (I10; nothing is enumerable) |
```

**§3.2**, the viewport response's second batch and the item endpoint:

```markdown
2. *points*: `(tessera_id: uint64, x: float32, y: float32, …declared scalars)`.
```

```markdown
`POST /v1/items/{tessera_id}` — `{pin?, epoch?}` → JSON scalars, the caller's
`external_id` where one exists, and drill-down fields. **`404 unknown` is returned
identically** for "no such ID" and "exists but is not visible to this principal": same
status, same code, same detail string, no branch-dependent logging or metrics.

**The endpoint answers one bit — is this entity visible to this session — and it
answers it in entity space** *(r6)*. Inversion of the `tessera_id` is a pure function
taking no I/O; the visibility test that follows is `fragment.contains(entity)` adjusted
by the overlay's `deleted > suppressed > evaluate_terms` precedence and the ingest
buffer, resolved per entity exactly as §5.2's mask composition resolves it. That is
O(1), constructs **no row-space projection**, and does **identical work for an
identifier that names nothing and one that names an invisible item** — which is what
closes the endpoint's timing channel rather than narrowing it (design Appendix C, C4
annotation). A row is looked up only *after* the answer is already "visible", and the
external-ID sidecar is read only after that. `priority` is not returned — not because it
may not be (r6 retires that prohibition; it is a prefix of the `tessera_id` in the same
response) but because a drill-down has no use for a sort key.

If `epoch` is supplied and differs from `identity.epoch` (§2.2), the response is
`409 conflict` — *"stale identity epoch; re-resolve by external_id"* — decided before
inversion and identically for every identifier.
```

**§3.4's `/control/ingest` row** gains, after the idempotency sentence:

```markdown
Duplicate detection is on the caller's `external_id` where one is supplied, against
both the in-flight batch and the bundle's external-ID sidecar: duplicates are
`409 conflict` with the offending IDs in `detail`, and **the batch has no effect**.
`external_id` is optional; an item without one is addressable only by its
`tessera_id`, which the response returns per accepted row.
```

**§5:**

```markdown
Determined by §3's Arrow schemas plus three rules: **the identity on the viewer plane
is the item's `tessera_id`** — a keyed permutation of `(shard_id, entity_id)` that
carries no entity-space order and is invertible only inside the trust boundary (I10;
byte-scan-tested in payloads and logs, which sweep for entity IDs, for the identity key
itself, and for caller external IDs outside the drill-down response — **not** for
`priority`, which r6 makes a prefix of the keyed identity and therefore harmless on this
plane (§2.6)); entity IDs
cross no process boundary and are not stored in any request-path artifact (§2.6);
buffers are uncompressed for zero-copy slicing. *(r6; the per-session `u32` handle is
retired from the viewer plane — §0.3 deviation 8 — and retained for Phase 3 node
handles. A stable identity is linkable across sessions and principals by construction:
design Appendix C, C17.)* The API section *is* the wire contract; there is no second
document to drift.
```

- [ ] **Step 11: Bump the contracts spec to r6**

```
**Status:** Draft r6 — r5 plus the boundary identity: `columns.arrow` carries a keyed `tessera_id`, `node_id` is removed, point handles are retired from the viewer plane, external-ID resolution becomes a per-extent lazy sidecar (Appendix R)
```

Append to Appendix R, above the r5 paragraph, a paragraph recording: the four deviations; that `tessera_id` is a **keyed bijection and therefore collision-free by construction, stable with nothing persisted, and needing no `tessera_id → entity` sidecar** — *conditional on the allocator refusing to issue an ID at or above `u32::MAX`, which is what makes "by construction" true rather than aspirational*; that `tessera_id` is a **transport** identifier with an epoch, not a durable key, and that consumers persist `external_id`; that a build with no explicit identity-key decision **refuses**; that entity IDs narrow to `u32` on disk under D8 with `terms/pairs.parquet` as the one recorded exception; that the premise was established by compiler-enforced enumeration rather than grep; that I10 is strengthened in substance and changed in mechanism, with the design's r21 as the companion; that §2.6's streamed-segment locator is revised because it depended on the deleted column; **that `priority` is redefined as the high 16 bits of the row's `tessera_id` and r4's standalone `splitmix64`-over-entity function is deleted, that the sort order becomes `(morton, tessera_id)` with no further tiebreak and is therefore key-dependent, that the redefinition repairs a sample which above V ≈ 2×10⁶ was ordered by permission signature through a signature-sorted entity-ID tiebreak (I7's purpose, not its letter) and restores §12.3's global-per-item-property premise under D8's shard-local IDs, that zero bytes change in the bundle, and that `priority` is consequently *permitted* on the viewer plane — a keyed prefix of an identity the payload already carries — while an unkeyed derivative of the entity ID would still be forbidden**; **that §1's external-ID cap tightens from 256 bytes to 64 (owner ruling), with over-length a typed error rather than a truncation, and that sidecar disk scales linearly with key length — with the ~16-byte threshold at which the store dominates the bundle recorded in §2.4**; **that the identity key's home outside the bundle is a per-deployment configuration file named explicitly on the command line (`--id-key-file`), with no default search path, so that a build never acquires a key nobody chose**; **that the external-ID store is marked TRANSITIONAL — a placeholder for a future adopted per-point metadata store occupying design §8.3's sidecar slot, with the note that Appendix D rejects adoption for the access-control layer and not for a cold store off the request path**; **that an item whose caller supplied no external ID has its `tessera_id` as its identifier and occupies no row in the store, so a deployment supplying no keys writes no extents at all**; and that the costs are **C6**, moved to `Accepted — caller's control`, and **C17**, a new entry for the linkability a stable identity buys.

- [ ] **Step 12: Verify no other document contradicts the new format**

```bash
grep -n "node_id\|entity_id\|per-session handle\|opaque handle" .ignore/tessera-system-architecture.md .ignore/tessera-conformance-design.md .ignore/tessera-implementation-plan.md .ignore/tessera-concurrency-lifecycle.md .ignore/tessera-visualisation-architecture.md
```

Read each hit. SA D14 (caller-supplied external IDs as the admin-plane identity) **remains correct verbatim** and needs no edit. Hits asserting that the viewer plane carries a per-session point handle are in scope for a follow-up but are **lower-precedence documents**: if any *asserts* something this change contradicts, record it in the commit message and **report to the owner** rather than editing a document this plan did not scope.

- [ ] **Step 13: Commit**

```bash
git add .ignore/tessera-architecture-design.md .ignore/tessera-contracts-spec.md docs/design-memos/2026-07-30-tessera-id-construction.md
git commit -m "docs(design,contracts): the tessera_id boundary identity — design r21, contracts r6

priority becomes high16(tessera_id); the storage sort order becomes (morton, tessera_id)."
```

---

### Task 5: The bijection in code

Thirty lines, in the crate with no dependencies, tested against Task 3's vectors and against the algebra.

**Files:**
- Modify: `crates/tessera-types/src/lib.rs` (new `identity` module; `TesseraId`; **keep** `NODE_NONE` per D7)

**Interfaces:**
- Consumes: Task 3's memo and `reference/vectors/tessera_id.json`.
- Produces: `TesseraId(u64)`; `IdentityKey([u8; 16])`; `IdentityKey::forward(shard: u32, entity: EntityId) -> Result<TesseraId, IdentityError>`; `IdentityKey::invert(id: TesseraId) -> (u32, EntityId)`; `IdentityKey::from_hex(&str) -> Result<Self, IdentityError>`; `const IDENTITY_CONSTRUCTION: &str = "feistel-splitmix64-v1"`; `const IDENTITY_ROUNDS: u32 = 8`; **`TesseraId::priority(&self) -> u16`** *(2026-07-30 fold)*.

**`TesseraId::priority()` is the one and only definition of the priority column** *(owner decision, 2026-07-30)*. `(self.0 >> 48) as u16` — the leading 16 bits of the identity, and contracts §2.6's `priority`. It lives here, beside the construction it is a prefix of, because three call sites need it and a second inline `>> 48` is how the column and the sort key drift apart: `tessera-store`'s two writers derive the column with it (Task 6), and `tessera-build`'s streaming comparator uses it as the cheap prefix in its 12-byte sort record (Task 7). **`tessera-build`'s `priority_of(EntityId)` (`lib.rs:140–143`) is deleted, not re-pointed** — it takes the wrong argument. Add to Step 1:

```rust
#[test]
fn priority_is_the_leading_sixteen_bits_of_the_identity() {
    // Contracts §2.6 r6. The point of the redefinition is that the sort prefix and the
    // full sort key are the same value, so this is not a formatting detail: if priority
    // is ever anything but a prefix, "k lowest by priority then by tessera_id" stops
    // being "k lowest by tessera_id" and the sampler acquires a composite comparator.
    let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
    for e in (0u32..1 << 16).step_by(13) {
        let id = key.forward(0, EntityId::new(e)).unwrap();
        assert_eq!(id.priority(), (id.raw() >> 48) as u16);
    }
}

#[test]
fn ordering_by_priority_then_id_is_ordering_by_id() {
    // Assert the equivalence the prefix argument rests on over a shuffled sample:
    // sort_by(|a,b| a.priority().cmp(&b.priority()).then(a.raw().cmp(&b.raw()))) must
    // produce exactly sort_by_key(|x| x.raw()).
}
```

**`forward` is fallible, and that is Important I-1, not fussiness.** `EntityId` is a `u64` newtype; D8 says entity IDs are `u32`. A bare `entity.raw() as u32` **truncates**, and two entities differing only above bit 32 would then share a `tessera_id` — at which point "collision-free by construction" is false and `invert` returns the *wrong* entity, which on `/control/changes` is a suppression against the wrong item. So `forward` performs a **checked conversion** and returns `IdentityError::EntityOutOfRange { entity }` above `u32::MAX`. Task 7's allocator cap is what makes that error unreachable; this signature is what makes its absence loud rather than silent. **Both must land, and neither substitutes for the other.**

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn forward_and_invert_round_trip_over_the_whole_low_space() {
    // A Feistel is a permutation for any round function, so this cannot fail for a
    // correct transcription -- which is exactly why it is worth running: the realistic
    // failure is a swapped half or an off-by-one round index, and both break here.
    let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
    for e in (0u32..1 << 22).step_by(7) {
        let id = key.forward(0, EntityId::new(e));
        assert_eq!(key.invert(id), (0, EntityId::new(e)));
    }
}

#[test]
fn it_is_injective_over_a_large_contiguous_run() {
    // Collision-freedom is structural, not probabilistic -- assert it anyway over a
    // run large enough that a broken round loop would show, because the whole design
    // rests on there being no collision-detection pass anywhere.
    let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
    let seen: std::collections::HashSet<u64> =
        (0u32..1 << 21).map(|e| key.forward(0, EntityId::new(e)).raw()).collect();
    assert_eq!(seen.len(), 1 << 21);
}

#[test]
fn it_matches_the_shared_known_answer_vectors() {
    // reference/vectors/tessera_id.json was generated from the spec text before either
    // implementation existed (Task 3). The Python oracle tests against the same file.
    // Disagreement here means the Rust is wrong; agreement between two independent
    // implementations and the file is the evidence the construction is reproducible.
}

#[test]
fn consecutive_entities_do_not_yield_ordered_identities() {
    // The property C6 rests on: the fraction of adjacent pairs that ascend must sit
    // near 1/2. An identity-like or lightly-perturbed permutation lands at ~1.0.
}

#[test]
fn a_different_key_gives_a_different_identity_for_the_same_entity() {}

#[test]
fn the_shard_prefix_separates_identity_spaces() {
    // shard 0 and shard 1 must not map any entity to the same u64 -- guaranteed by
    // bijectivity over the full 64-bit input, asserted because a dropped shard term in
    // the input encoding would silently collapse them and would pass every other test.
    // Phase 1 values shard_id 0 always, so this is the ONLY thing keeping a reserved,
    // never-exercised field from being tidied out of the input encoding.
}

#[test]
fn forward_refuses_an_entity_above_u32_max_rather_than_truncating() {
    // IMPORTANT I-1. A truncating cast makes "collision-free by construction" FALSE:
    // 0x1_0000_0000 and 0x0 would share a tessera_id, and `invert` would name the wrong
    // entity -- a /control/changes suppression against the wrong item. The allocator cap
    // (Task 7) makes this unreachable; this makes a bypass loud.
    let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
    assert!(matches!(
        key.forward(0, EntityId::new(1u64 << 32)),
        Err(IdentityError::EntityOutOfRange { .. })
    ));
    assert!(key.forward(0, EntityId::new(u32::MAX as u64 - 1)).is_ok());
}

#[test]
fn degenerate_keys_are_refused() {
    // k1 == 0 collapses the schedule to ONE constant round key for all eight rounds;
    // the all-zero key does the same and is additionally what an uninitialised buffer
    // supplies. Both are still permutations, so nothing fails loudly -- which is exactly
    // why they must be refused at the door.
    assert!(IdentityKey::from_hex("0102030405060708" .to_owned().repeat(1) + "0000000000000000").is_err());
    assert!(IdentityKey::from_hex("00000000000000000000000000000000").is_err());
}

#[test]
fn the_hex_form_is_lowercase_only_and_is_not_case_folded() {
    // MANIFEST has one canonical spelling because a digest is taken over it. Uppercase
    // is a typed error, not a synonym -- folding would let two MANIFESTs that differ
    // byte-wise claim the same key.
    assert!(IdentityKey::from_hex("000102030405060708090A0B0C0D0E0F").is_err());
    assert!(IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").is_ok());
}
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p tessera-types identity
```
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement**

`crates/tessera-types/src/identity.rs`, transcribed from Task 3's memo. The doc comment must state the threat model in three sentences — the key is not secret against a bundle-holder; the property defended is that a client cannot derive or order entity IDs; the key must never leave the server — and must name `splitmix64` as a non-cryptographic mixer with a pointer to the memo.

`TesseraId` is a newtype with `new`/`raw` and **no conversions to or from any other ID newtype**. `scripts/check-layers.sh` enforces that for the family, and **its I4 grep must be extended to name `TesseraId`** — it currently matches only `EntityId|RowId|TermId|Handle` (`scripts/check-layers.sh`, the `impl From` check), so an `impl From<EntityId> for TesseraId` would sail straight through the one check that exists to stop exactly that. Add it in this task, not in Task 10, because this is the task that creates the type:

```bash
if grep -rn "impl From" crates/tessera-types/src/ | grep -E "EntityId|RowId|TermId|TesseraId|Handle"; then
```

`IdentityKey` must **not** implement `Debug` or `Display` in a form that prints the key material — implement `Debug` as `IdentityKey(<redacted>)`, so no accidental `{:?}` in a log line leaks it. `from_hex` refuses anything but 32 lowercase hex characters and refuses degenerate keys (`k1 == 0`, all-zero).

**Keep `NODE_NONE`** in `tessera-types` (D7): the column goes, the sentinel stays for Phase 3.

- [ ] **Step 4: Run**

```bash
cargo test -p tessera-types
```
Expected: PASS.

- [ ] **Step 5: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash scripts/check-layers.sh
git add crates/tessera-types/src/lib.rs crates/tessera-types/src/identity.rs crates/tessera-types/tests/ scripts/check-layers.sh
git commit -m "feat(types): the tessera_id keyed bijection (feistel-splitmix64-v1)"
```

---

### Task 6: `tessera-store` — the four-column schema

**Files:**
- Modify: `crates/tessera-store/src/read.rs` (`FIXED_COLUMNS` at 541–547, `entity_id()` at 591, `node_id()` at 603, `validate_schema`)
- Modify: `crates/tessera-store/src/write.rs` (schema literals at 61–71 and 236–242; `write_columns` at 213)
- Modify: `crates/tessera-store/src/manifest.rs` (the `identity` object)
- Modify: `crates/tessera-spatial/src/tiler.rs` (`TilerItem` at 34–40, `sort_batch` at 67)
- Modify (tests): `crates/tessera-store/tests/segment_roundtrip.rs`, `crates/tessera-store/tests/bundle_read.rs`

**Interfaces:**
- Consumes: contracts §2.6 as revised in Task 4; Task 5's `TesseraId`.
- Produces: `ColumnsRef::tessera_id(&self) -> &[u64]`; `write_columns(path, tessera_id: Vec<u64>, x: Vec<f32>, y: Vec<f32>)` — **the `priority` column is derived inside the writer from `tessera_id` via `TesseraId::priority()`** *(2026-07-30 fold)*; `TilerItem { tessera_id: TesseraId, x, y, scalars }` (**no `priority` field** — it is a prefix of `tessera_id`); `Manifest::identity: IdentityDescriptor` (required).

- [ ] **Step 1: Write the failing tests**

In `crates/tessera-store/tests/segment_roundtrip.rs`, change the schema assertion (line 79):

```rust
assert_eq!(names, vec!["tessera_id", "x", "y", "priority"]);
assert_eq!(schema.field(0).data_type(), &DataType::UInt64);
assert_eq!(schema.field(1).data_type(), &DataType::Float32);
assert_eq!(schema.field(2).data_type(), &DataType::Float32);
assert_eq!(schema.field(3).data_type(), &DataType::UInt16);
```

and the with-scalars case (line 321) to `vec!["tessera_id", "x", "y", "priority", "count", "label"]`, with the two declared-scalar downcasts moving from columns 5 and 6 to columns 4 and 5.

Add the fail-closed test:

```rust
#[test]
fn a_pre_r6_columns_file_is_a_typed_error_not_a_half_read() {
    // Contracts §2.6 r6: renaming the identity column is what makes an old bundle fail
    // closed. Write a five-column pre-r6 schema by hand and assert the reader rejects
    // it by name, rather than reading `entity_id`'s bytes as `tessera_id` -- which
    // would silently publish entity IDs on the wire, the one thing I10 forbids.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("columns.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("node_id", DataType::UInt32, false),
        Field::new("priority", DataType::UInt16, false),
    ]));
    // ... write one batch, then:
    let err = ColumnsRef::load(&path).unwrap_err();
    assert!(matches!(err, StoreError::InvalidColumns { .. }),
        "a pre-r6 columns.arrow must be a typed reader error, got {err:?}");
}

#[test]
fn a_manifest_without_an_identity_object_is_a_typed_error() {
    // Contracts §2.2 r6: `identity` is required, not defaulted. A bundle read without
    // a key cannot invert a tessera_id, and a *defaulted* key would invert every
    // identifier to the wrong entity -- suppressing the wrong item on /control/changes.
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p tessera-store --test segment_roundtrip
```

- [ ] **Step 3: Change the schema**

`crates/tessera-store/src/read.rs`:

```rust
const FIXED_COLUMNS: [(&str, DataType); 4] = [
    ("tessera_id", DataType::UInt64),
    ("x", DataType::Float32),
    ("y", DataType::Float32),
    ("priority", DataType::UInt16),
];
```

Rename the accessor and **delete `node_id()`**:

```rust
/// The row→wire-identity direction (contracts §2.6, §0.3 deviations 2 and 6): the
/// `tessera_id` shown to viewers, stored at the row it is shown from. No entity ID is
/// stored here — after contracts r6 the gather cannot produce one, which is what makes
/// I10 structural rather than a discipline at the serialisation chokepoint.
pub fn tessera_id(&self) -> &[u64] {
    downcast::<UInt64Array>(&self.batch, 0).values()
}
```

Column ordinals shift: `x` 1, `y` 2, `priority` 3 (from 4), and `scalar_index` now indexes from ordinal 4 rather than 5. `validate_schema`'s length check becomes `fields().len() >= 4`.

`crates/tessera-store/src/write.rs`, both literals, first field `Field::new("tessera_id", DataType::UInt64, false)` and no `node_id`; `write_columns` loses its `node_id` parameter.

`crates/tessera-store/src/manifest.rs`: add

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityDescriptor {
    pub construction: String,
    pub rounds: u32,
    pub key: String,       // 32 lowercase hex chars
    pub shard_id: u32,
}
```
as a **required** `Manifest::identity` field — no `#[serde(default)]`, so an absent object is a deserialisation error (contracts §2.2 r6). Add a validator rejecting an unknown `construction` or a `rounds` other than `IDENTITY_ROUNDS`; a bundle written by a different construction must not be read by this one.

`crates/tessera-spatial/src/tiler.rs`: `TilerItem` loses `node_id` **and `priority`**, and its `entity_id: EntityId` becomes `tessera_id: TesseraId`. The module doc comment at `tiler.rs:4–6` — *"Priority is **not** computed here: the entity-ID allocator owns priority assignment (R3, splitmix64 over the final entity ID); callers pass it in already computed"* — is now false and must be replaced: priority is the leading 16 bits of the `tessera_id` the caller supplies, so the tiler needs no separate value and the allocator owns nothing about it. The `ScalarValue` doc at `tiler.rs:12–13`, which lists the fixed columns as `(entity_id, x, y, node_id, priority)`, becomes `(tessera_id, x, y, priority)`.

**The sort order moves onto the identity** *(2026-07-30 fold; this replaces the earlier "the sort tiebreak does not move", which is now wrong)*. `sort_batch` orders by **`(morton, tessera_id)` ascending with no further tiebreak** — see "The sort and tiebreak statement" and "The build sequence inverts" above, which are normative and must not be paraphrased. `TilerItem` therefore **loses its `priority` field**: the value is a prefix of the `tessera_id` the item already carries, and a stored second copy is a second source of truth. `sort_batch` still takes `entity_ids`, but as a **companion vector permuted with the items**, not as a sort key:

```rust
/// Sort `items` into segment (row) order: `(morton, tessera_id)` ascending
/// (contracts §2.6 r6). No further tiebreak: `tessera_id` is a bijection over 2^64 and
/// there is one row per entity, so the order is total — and because `priority` is the
/// leading 16 bits of `tessera_id`, ordering by `(morton, priority, tessera_id)` is
/// identically this order. The entity ID is **not** a sort key at any position; it is
/// passed alongside so the caller can keep `permutation.bin` and the external-ID
/// sidecars aligned with the new row order. Row order is key-dependent: a different
/// deployment key reorders rows inside a Morton cell (contracts §2.2, rotation).
pub fn sort_batch(
    items: &mut Vec<TilerItem>,
    entity_ids: &mut Vec<EntityId>,
    extent: &Extent,
) -> Vec<u32>
```

with `entity_ids` permuted identically to `items`, and the caller retaining it for the permutation and the sidecars.

**Rename the existing test rather than deleting it.** `sorts_by_morton_then_priority_then_entity_id` (`tiler.rs:108`) becomes `sorts_by_morton_then_tessera_id`, and its fixture must include **at least one pair sharing a Morton code** so the identity ordering is actually exercised. Add one more:

```rust
#[test]
fn ordering_by_the_priority_prefix_then_the_full_id_equals_ordering_by_the_id() {
    // Contracts §2.6 r6: `priority` is a PREFIX of `tessera_id`, so the two orders are
    // the same order. This is what licenses an implementation to compare the cheap
    // 16-bit prefix first (pipeline.rs's 12-byte RowRec does exactly that). Engineer
    // prefix ties — ids sharing their high 16 bits — or the test proves nothing.
}
```

**The priority column is derived in one place, and that place is `TesseraId::priority()`** (Task 5). Neither the tiler nor either build path may recompute `(id >> 48) as u16` inline. `write_segment` and `write_columns` derive the column from the `tessera_id` values they are already given, so `write_columns` **loses its `priority: Vec<u16>` parameter** along with `node_id`: `write_columns(path, tessera_id: Vec<u64>, x: Vec<f32>, y: Vec<f32>)`. The written schema is unchanged — four fixed columns, `priority` still `uint16` at ordinal 3 — and deriving it inside the writer is what makes both build paths byte-identical by construction rather than by agreement.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p tessera-store
cargo test -p tessera-spatial
```
Expected: PASS. `bundle_read.rs` lines 192–202 need their `entity_id`/`node_id` assertions replaced by a `tessera_id` assertion; delete the `node_id_col` assertion entirely.

- [ ] **Step 5: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy -p tessera-store -p tessera-spatial --all-targets -- -D warnings
cargo test -p tessera-store -p tessera-spatial
bash scripts/check-layers.sh
```

**Note:** `tessera-build` and `tessera-engine` will not compile until Tasks 7 and 9. That is expected and is the point of the compiler-enforced boundary. Commit with `-p tessera-store -p tessera-spatial` green and the workspace known-red, stating so in the commit message.

```bash
git add crates/tessera-store/src/read.rs crates/tessera-store/src/write.rs crates/tessera-store/src/manifest.rs crates/tessera-spatial/src/tiler.rs crates/tessera-store/tests/segment_roundtrip.rs crates/tessera-store/tests/bundle_read.rs
git commit -m "feat(store)!: columns.arrow carries tessera_id, drops node_id (contracts r6)

Workspace is intentionally red until tessera-build and tessera-engine follow."
```

---

### Task 7: `tessera-build` — the key, the column, the sidecars, the allocator cap

**Files:**
- Modify: `crates/tessera-build/src/lib.rs` (`BuildArgs`, the in-memory path ~315–334, `verify` at 516, `write_external_ids` at 659, `ExternalIdRow` at 679–702)
- Modify: `crates/tessera-build/src/pipeline.rs` (~328–420)
- Modify: **`crates/tessera-cli/src/main.rs`** — this is where the `tessera build` CLI actually lives (the seven identity-key flags, including `--id-key-file`, and the N-1 refusal). **There is no `crates/tessera-build/src/bin/`**; an earlier draft of this plan named that path, and aiming the most safety-critical code in the plan at a directory that does not exist would have left the N-1 refusal both unwritten and uncommitted.
- Modify: **`scripts/build_full.sh`** — argument forwarding only (Step 3a). The script is what the 10⁹ build runs through (Task 14 Step 3), and today it passes **no** key flag at all, so N-1's guarantee for the real build depends on this edit landing.
- Modify (tests): `crates/tessera-build/tests/build_equivalence.rs`, `crates/tessera-build/tests/build_smoke.rs`

**Interfaces:**
- Consumes: Task 5's `IdentityKey`, Task 6's `write_columns`/`TilerItem`/`IdentityDescriptor`.
- Produces: `BuildArgs::identity_key: IdentityKey` and `BuildArgs::shard_id: u32`; MANIFEST `identity`; `columns.arrow`'s `tessera_id` column; `external-ids-<k>.arrow` with `entity_id: uint32`; `ext-locator.u32`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn both_build_paths_produce_byte_identical_bundles_with_no_seed_threaded() {
    // The tripwire this crate is held to. It got SIMPLER under the bijection: the
    // identity is a pure function of (key, shard, entity), so there is nothing
    // random to reconcile between the streaming and in-memory paths -- no seed, no
    // RNG, no ordering dependence. If they diverge, the cause is a schema literal or
    // the sort tiebreak, not the identity.
}

#[test]
fn the_tessera_id_column_is_the_key_applied_to_the_row_s_entity() {
    // The column must be exactly forward(shard, entity_of_row), for every row --
    // the property `tessera verify` checks and the oracle re-derives. Assert it
    // through permutation.bin so the test does not assume the build's own ordering.
    // ... for every row r: cols.tessera_id()[r] == key.forward(shard, entity_of_row[r]).raw()
}

#[test]
fn the_ext_locator_addresses_the_right_key_for_every_entity() {
    // entity -> ordinal -> external_id must recover the source id the build was given.
    // A wrong locator returns another item's external ID on drill-down, which is a
    // disclosure, not a bug in a lookup table.
}

#[test]
fn a_build_refuses_a_carried_key_whose_construction_differs() {
    // Contracts §2.2 r6 refusal rule. Silently accepting a different construction
    // under the same key is the worst available outcome: every identifier changes
    // while every manifest says the key did not.
}

#[test]
fn a_build_with_no_identity_key_decision_refuses_and_writes_nothing() {
    // CRITICAL N-1. Minting is NOT the default. A rebuild where the operator forgot
    // --carry-id-key-from must fail loudly and immediately, not produce a bundle in
    // which every bookmark, shared link and consumer-DB row is silently wrong. A
    // printed warning on a 90-minute build's stdout is not a gate.
    //
    // Assert THREE things, because the failure mode is partial work:
    //   1. non-zero exit;
    //   2. the message names all FOUR sources, so the operator can act on it;
    //   3. the output directory does not exist / is empty -- the refusal happens
    //      BEFORE any work, not after 90 minutes.
    let out = tempdir();
    let st = run_build(&["--limit", "1000", "--out", out.path()]);   // no key flag
    assert!(!st.success());
    for flag in ["--carry-id-key-from", "--id-key-file", "--id-key", "--mint-id-key"] {
        assert!(st.stderr.contains(flag), "refusal must name {flag}");
    }
    assert!(std::fs::read_dir(out.path()).map(|d| d.count()).unwrap_or(0) == 0);
}

#[test]
fn the_key_file_is_read_validated_and_never_found_by_default() {
    // OWNER RULING Q6. `--id-key-file` is the key's home outside the bundle, and it
    // counts as an explicit decision under N-1 ONLY because the operator typed the path.
    //   1. a valid file builds and its key lands in MANIFEST verbatim;
    //   2. every --id-key validation applies: wrong length, uppercase, k1 == 0 and the
    //      all-zero key are typed errors naming the FILE;
    //   3. an unknown top-level section is IGNORED (forward compatibility with the wider
    //      deployment config the file will grow into);
    //   4. an unknown key inside [identity] is an ERROR -- a misspelt `kye =` must not
    //      fall through to a refusal that reads "no key given";
    //   5. THE N-1 GUARD: with a valid key file sitting in the CWD and in every plausible
    //      default location, a build that does NOT name it on the command line still
    //      REFUSES. A file the binary finds on its own is not a human deciding.
    //   6. --mint-id-key writes NO file anywhere.
}

#[test]
fn two_key_sources_that_agree_are_fine_and_two_that_disagree_refuse() {
    // Agreement is the useful case: it is how an operator checks the config file and the
    // previous bundle are the same lineage. Disagreement without --rotate-id-key is a
    // refusal, because silently preferring one source would change every client's
    // identifiers with no signal.
}

#[test]
fn mint_and_carry_and_rotate_each_do_exactly_what_they_say() {
    // --mint-id-key produces a fresh non-degenerate key at epoch 1; a carry from that
    // bundle reproduces the SAME key, the SAME epoch and a byte-identical tessera_id
    // column; --id-key differing from a carried key refuses without --rotate-id-key and
    // succeeds with it, resetting the epoch to 1.
}

#[test]
fn the_allocator_refuses_to_issue_an_id_at_or_above_u32_max() {
    // IMPORTANT I-1, in the crate that owns the invariant. `Allocator::allocate` was
    // `lo + n` on a u64 with no cap (alloc.rs:31-36), so "collision-free by
    // construction" rested on the corpus happening to be small. Past 2^32 two entities
    // share a tessera_id and `invert` returns the WRONG one.
    let mut a = Allocator::new(u32::MAX as u64 - 2);
    assert!(a.allocate(1).is_ok());
    assert!(matches!(a.allocate(10), Err(AllocError::Exhausted { .. })));
    // And the seed itself: a manifest high-water past the bound is refused at open,
    // not silently carried.
    assert!(Allocator::try_new(1u64 << 33).is_err());
}

#[test]
fn a_build_refuses_an_external_id_longer_than_64_bytes() {
    // OWNER RULING Q7, at the BUILD site. Contracts §1 r6 caps external IDs at 64
    // bytes and says over-length is "a typed error at ingest and at build, never a
    // truncation" -- and until this test existed the plan named both sites and staffed
    // neither, leaving a spec sentence asserting behaviour no code had. Truncation is
    // the failure being prevented: a truncated key is a DIFFERENT key, and two callers'
    // keys sharing a 64-byte prefix would collide into one entity.
    let long = vec![b'k'; 65];
    let err = run_build_with_external_ids(&[long]).unwrap_err();
    assert!(matches!(err, BuildError::ExternalIdTooLong { len: 65, .. }));
    // And the boundary is inclusive: exactly 64 bytes is accepted.
    assert!(run_build_with_external_ids(&[vec![b'k'; 64]]).is_ok());
}

#[test]
fn verify_rejects_a_columns_file_whose_tessera_ids_do_not_match_the_key() {
    // Contracts §2.6 r6 asserts that `tessera verify` "checks the whole column against
    // it". `tessera_build::verify` reads no columns.arrow VALUES today, so that sentence
    // would have been a spec asserting behaviour the code lacks -- C-3's shape, which
    // this plan exists partly to avoid reintroducing. Step 3b implements the check;
    // this asserts it fails on a bundle whose column was written under another key.
    let bundle = build_then_rewrite_one_tessera_id(/* corrupt a single row */);
    assert!(matches!(tessera_build::verify(&bundle), Err(BuildError::Invalid(_))));
}

#[test]
fn entity_ids_follow_signature_order() {
    // UNCHANGED and must stay unchanged (I9, §11.1). Listed here so an executor
    // does not "tidy" it while touching the same file.
}
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p tessera-build --test build_equivalence
```

- [ ] **Step 3: Thread the key, not a seed**

`BuildArgs` gains:

```rust
/// The deployment's identity key (contracts §2.2). **Not** per bundle: it must be
/// carried across rebuilds or every `tessera_id` any client holds silently breaks.
/// Resolved by the CLI from `--carry-id-key-from` / `--id-key` / a fresh mint, and
/// passed here already decided so that both build paths see the same bytes.
pub identity_key: IdentityKey,
/// The §13.3 row-range shard this build produces. Phase 1: 0.
pub shard_id: u32,
```

CLI flags per contracts §2.2, added to the `build` subcommand in **`crates/tessera-cli/src/main.rs`** (that is where `tessera build` is defined; there is no `crates/tessera-build/src/bin/`): `--carry-id-key-from <bundle-root>` (the normal rebuild path), **`--id-key-file <path>`** (owner ruling Q6 — the deployment config file, and the key's home outside the bundle), `--id-key <32 hex>`, **`--mint-id-key`**, `--rotate-id-key`, `--bump-id-epoch`, `--epoch <n>`.

**`--id-key-file` is a minimal TOML read, not a config subsystem.** Parse `[identity] key = "<32 lowercase hex>"`; apply exactly the validations `--id-key` applies (length, lowercase, non-degenerate) with a typed error naming the file and the reason; **ignore unknown top-level sections** so a later phase can extend the file without breaking this binary; **error on an unknown key inside `[identity]`**, so a misspelt `kye =` does not fall through to a refusal reading "no key given". **No default path, no `$TESSERA_CONFIG`, no fallback location** — the path is always given explicitly, which is the only reason the flag counts as an explicit decision under N-1. `--mint-id-key` **prints and does not write the file**; a build that wrote a config file as a side effect would create the artifact a later build could find on its own.

**CRITICAL N-1 — refuse when none of `--carry-id-key-from` / `--id-key-file` / `--id-key` / `--mint-id-key` is given.** Exit non-zero **before any work**, before `df`, before reading input, before creating the output directory. The message names all three flags and says which is the normal rebuild. Minting is never implicit: the previous draft's mint-with-a-printed-notice default put the irreversible outcome on the path of least typing, and a warning inside ninety minutes of build output is not a gate. Implement the other refusal rules exactly as specified; the `--rotate-id-key` help text must say *"invalidates every `tessera_id` any client holds"*, and `--mint-id-key`'s must say *"starts a NEW identity lineage"*.

- [ ] **Step 3a: Make `scripts/build_full.sh` forward the key flag — and hard-code none**

**This step is what makes N-1 true for the build that matters.** The 10⁹ build runs through `scripts/build_full.sh` (Task 14 Step 3), and the script today passes `--points`, `--pairs`, `--out`, `--extent` and `--slice` and **no key flag whatsoever** (`scripts/build_full.sh:39–44`). Left as it is, the real build hits N-1's refusal and an executor's likeliest repair is to hard-code a flag into the script — at which point the refusal is defeated by the script it exists to protect, and nobody notices because the build succeeds.

So the script gains **forwarding, not a default**:

```bash
OUT="${1:-/tmp/tessera-1e9}"
shift || true
IDENTITY_ARGS=("$@")          # e.g. --mint-id-key, or --carry-id-key-from <bundle>
if (( ${#IDENTITY_ARGS[@]} == 0 )); then
  echo "ERROR: no identity-key argument given." >&2
  echo "Pass one of --carry-id-key-from <bundle> / --id-key-file <path> / --id-key <32 hex> / --mint-id-key" >&2
  echo "as trailing arguments; this script deliberately supplies NO default (plan N-1)." >&2
  exit 1
fi
```

and appends `"${IDENTITY_ARGS[@]}"` to the existing `tessera build` invocation, leaving `--points`, `--pairs`, `--extent` and `--slice` exactly as they are. Requirements, each of which a reviewer should check by reading the script rather than by running it:

1. **No key flag appears literally in the script.** `grep -E '\-\-(mint-id-key|carry-id-key-from|id-key|id-key-file)' scripts/build_full.sh` must match only the error message above.
2. **No environment-variable fallback**, no `${TESSERA_ID_KEY:-…}`, no default file path. The same rule as `--id-key-file`: a value the script finds on its own is not a human deciding.
3. **The script's own refusal fires before the binary's**, so an operator who forgets gets a one-line message rather than a build that dies later. Both refusals must exist; the script's is a convenience and the binary's is the gate.
4. The script keeps its existing free-space check and its `set -euo pipefail`.

**Cap the allocator (Important I-1).** `crates/tessera-lifecycle/src/alloc.rs`: `allocate` becomes `Result<Range<u64>, AllocError>` and refuses to hand out any ID at or above `u32::MAX`; `Allocator::new` gains a checked `try_new` refusing a seed at or above the same bound, so a manifest high-water that already exceeds it is caught at open rather than at the first ingest. `AllocError::Exhausted { high_water }` propagates to `/control/ingest` as a typed error and the batch has no effect — this is §16's exhaustion question arriving early, and refusing is the fail-closed answer. **This is the precondition "collision-free by construction" has been resting on and which nothing enforced.**

**No seed, no RNG, and no `provenance.public_id_seed`.** If the executor finds themselves adding one, they are working from the superseded draft. (The CSPRNG appears in exactly one place: `--mint-id-key`, which draws 16 bytes once and retries on a degenerate draw.)

- [ ] **Step 3a: Redefine `priority` and move the identity ahead of the sort** *(2026-07-30 fold)*

Four edits, and the ordering between them is the point — see "The build sequence inverts" above.

1. **Delete `priority_of` and its test.** `crates/tessera-build/src/lib.rs:140–143` (`priority(e) = (splitmix64(e) >> 48) as u16`, with the R3/§2.6 doc comment) and `priority_matches_r3_splitmix64` (`lib.rs:862–872`) both go. Nothing re-points to `TesseraId::priority()` at this call site — the argument was the entity ID, which is the defect.
2. **Compute the identity before the tiler, on both paths.** The in-memory path (`lib.rs` ~315–334) constructs `TilerItem { tessera_id: …forward(shard, entity_id)?, … }` with no `priority` field. The streaming path fills `RowRec` from the identity, not from the entity.
3. **`RowRec` keeps 12 bytes and its comparator recomputes on a prefix tie.** `pipeline.rs:109–124`: keep `#[repr(C)] { morton: u32, entity: u32, priority: u16, _pad: u16 }` — `priority` is now `key.forward(shard, entity)?.priority()`, filled where the record is built — and change `order()` from a plain tuple to a comparator:

   ```rust
   // (morton, tessera_id) ascending (contracts §2.6 r6). `priority` is the leading 16
   // bits of `tessera_id`, so comparing it first is the SAME order, not an
   // approximation of it. The full identity is recomputed from `entity` only on a
   // prefix tie: `forward` is a pure function, so this is exact, and the record stays
   // 12 bytes — a `u64` here would be 16 B/row, +3.7 GiB at 10^9, immediately
   // re-spending what dropping the NODE_NONE column just freed.
   fn cmp(&self, other: &Self, key: &IdentityKey, shard: u32) -> Ordering
   ```

   The tie path costs eight `splitmix64` rounds. **If it shows in the build profile at 10⁹, the escalation is the 16-byte record and its 3.7 GiB — measure and report to the owner; do not choose silently.** Add a unit test that this comparator agrees with a naive full-`tessera_id` sort over a batch containing engineered prefix ties.
4. **The `priority` vector at `pipeline.rs:411–413` disappears.** `write_columns` derives the column from `tessera_id` (Task 6), so there is no separate vector to build and no second derivation site.

- [ ] **Step 4: Write the column and drop `NODE_NONE`**

`pipeline.rs` (~394–420): drop the `node_id` vector entirely — **this deletes the 4 GB `vec![NODE_NONE; n]` allocation at the build's tightest moment** (`pipeline.rs:416`); record the peak-RSS effect in Task 14. Build the identity column in row order:

```rust
// `forward` is fallible (Important I-1): a checked conversion, never `as u32`. At
// build the allocator cap makes the error unreachable, and collecting into a Result
// is what keeps it that way rather than assuming it.
let tessera_row: Vec<u64> = rows
    .iter()
    .map(|r| args.identity_key.forward(args.shard_id, r.entity).map(|id| id.raw()))
    .collect::<Result<_, _>>()
    .map_err(BuildError::Identity)?;
write_columns(&columns_path, tessera_row, x_row, y_row)
    .map_err(|e| BuildError::io(&columns_path, e))?;
```

*(2026-07-30 fold: no `priority` argument — the writer derives it from `tessera_id`; and `tessera_row` is not built here for the first time, since Step 3a already needed the identity **before** the sort. What happens at this point is the permutation of an identity vector that already exists, not its construction.)*

Make the in-memory path (`lib.rs` ~315–334) match: `TilerItem { tessera_id: args.identity_key.forward(args.shard_id, entity_id)?, … }`, with the parallel `entity_ids` vector passed to `sort_batch` per Task 6.

Record `identity` in MANIFEST from `BuildArgs`, identically on both paths.

- [ ] **Step 5: Narrow the external-ID extents and write the locator**

`ExternalIdRow` (679–702) keeps its 12-byte `#[repr(C)]` shape — `key_hi: u32, key_lo: u32, entity_id: u32` — which is already `u32` in memory; only the **written schema** narrows:

```rust
let schema = std::sync::Arc::new(Schema::new(vec![
    Field::new("external_id", DataType::Binary, false),
    Field::new("entity_id", DataType::UInt32, false),   // r6, D8: was UInt64
]));
```

Then, in the same pass that has the sorted rows in hand, write **one** `entities/ext-locator.u32` — **no `<k>` suffix, one file for the whole partition** (Important I-7): a raw `u32` array of length `entity_high_water`, `locator[row.entity_id] = ordinal`, initialised to `0xFFFFFFFF`, where `ordinal` counts across the *concatenated* extents in listed order rather than within an extent. A per-extent family would need a full-length array per extent (37 GiB at ten extents against 3.7) because the entity ID does not say which extent its key sorts into. **One allocation of 4 B × n**, written and dropped immediately — note it in the build's peak-RSS accounting, since it lands near the same tight moment the `NODE_NONE` vector vacated.

Both files are recorded in the side-manifest and digest-covered.

- [ ] **Step 5a: The uniformity check the redefinition is conditional on** *(2026-07-30 fold — an open verification item, not a formality)*

The priority prefix is now the top 16 bits of the left Feistel half `L`, and this plan states plainly that the construction is **a blinding permutation, not a cipher**. Sampling needs **uniformity, not unpredictability** — but the inputs are highly structured (`shard_id = 0`, `entity_id` dense from 0), and **residual structure in `L` would be inherited by the sample, which is the exact defect being fixed.** So it is verified rather than assumed.

Add to `crates/tessera-build/tests/build_equivalence.rs`, alongside the existing known-answer assertions:

```rust
#[test]
fn the_priority_prefix_is_uniform_over_dense_entity_ids() {
    // Chi-squared over high16(tessera_id) for entity_id dense from 0 at shard_id = 0 --
    // the structured input the build actually presents. 8 balanced rounds of splitmix64
    // should give uniformity; this asserts it rather than trusting it, because a biased
    // prefix reintroduces the tie-domination the redefinition exists to remove.
    // Bucket to 2^8 bins over >= 2^20 entities, assert the statistic against the 0.001
    // critical value, and state both the bin count and the threshold in the failure
    // message so a failure is diagnosable rather than just red.
}
```

**If this fails, the fold does not land: stop and report to the owner.** The fallback on the record is the review's second choice of round function (keyed SipHash-1-3, ~2% of build time), not a wider prefix — a biased prefix is biased at every width.

- [ ] **Step 6: Run the tests**

```bash
cargo test -p tessera-build
```
Expected: PASS, including `streaming_build_is_byte_identical_to_the_reference_build`, `entity_ids_follow_signature_order` and `the_priority_prefix_is_uniform_over_dense_entity_ids`.

- [ ] **Step 7: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy -p tessera-build --all-targets -- -D warnings
bash scripts/check-layers.sh
git add crates/tessera-build/src/lib.rs crates/tessera-build/src/pipeline.rs crates/tessera-build/Cargo.toml crates/tessera-build/tests/build_equivalence.rs crates/tessera-build/tests/build_smoke.rs crates/tessera-cli/src/main.rs crates/tessera-cli/Cargo.toml crates/tessera-lifecycle/src/alloc.rs crates/tessera-lifecycle/tests/ scripts/build_full.sh Cargo.lock
git commit -m "feat(build,cli)!: derive tessera_id from the deployment key, refuse a build with no key decision, cap the allocator at u32::MAX, write the ext locator, drop the NODE_NONE column

priority becomes high16(tessera_id) and the identity is computed before the tiler; the
standalone splitmix64-over-entity priority function is deleted."
```

---

### Task 8: The sidecars — per-extent lazy, digest **and** sortedness verified

Today `ExternalIdIndex::load` mmaps every extent and `validate_sorted` **linearly scans every row** (`external_ids.rs:211`), at `Engine::open`, for a structure the viewport path never reads — 18.9 GB into the resident set and a share of the 176.8 s boot. This task makes it a sidecar.

**Ruling B governs this task's design, and it changes what "done well" means here.** The sidecar is a **placeholder for a future adopted metadata store**, so the target is not the best external-ID store that can be built — it is the smallest one that satisfies its two callers and can be **taken out** without touching anything else. Three things follow, and a reviewer should check all three:

1. **Nothing clever.** No compression, no block format, no secondary index, no cache, no statistics. Sorted Arrow extents, per-extent lazy open, a `u32` locator. If an executor finds themselves optimising the search, they are investing in a component with a successor.
2. **The boundary is two operations, and the storage does not leak past them.** `resolve(&[u8]) -> Result<Option<EntityId>>` and `external_id_of(EntityId) -> Result<Option<Vec<u8>>>` (plus `resolve_external_ids` batch in Task 11, and `is_open`/`open_extents`/`locator_len` for tests and residency reporting). **No extent descriptor, Arrow type, digest, ordinal or file path may appear in `tessera-engine` or `tessera-server`.** The acceptance test is a sentence, not a lint: *replacing the storage must be a change to `sidecar.rs` plus its constructor call in `Engine::open`, and nothing else.* If a second module knows the store is sorted, or is Arrow, or is per-extent, that is a defect against Ruling B even though it violates no invariant.
3. **It is marked transitional at the type** (Step 3's doc comment) as well as in the spec, because a later contributor reads the code before the appendix.

**Ruling A applies too, and it is a rule about what must *not* be written:** the store holds rows only for items whose caller supplied a key. An item with no caller key has its `tessera_id` as its identity; `0xFFFFFFFF` in the locator is the **ordinary** case and must not be treated as missing data, and **no code path may manufacture an external ID for an item that has none**.

**Files:**
- Create: `crates/tessera-store/src/sidecar.rs`
- Modify: `crates/tessera-store/src/external_ids.rs` (folded in and deleted), `crates/tessera-store/src/lib.rs`, `crates/tessera-store/src/error.rs`

**Interfaces:**
- Consumes: Task 7's extent and locator layout.
- Produces: `ExternalIdSidecar::deferred(extents: Vec<ExtentDesc>) -> Self` (opens nothing); `resolve(&self, external_id: &[u8]) -> Result<Option<EntityId>>`; `external_id_of(&self, entity: EntityId) -> Result<Option<Vec<u8>>>` (via the locator); `is_open(&self) -> bool`; `open_extents(&self) -> usize`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn deferred_construction_opens_nothing() {
    // At Engine::open the sidecar must cost zero bytes of RSS and zero page faults.
    // A path that does not exist is the cheapest possible proof: if `deferred` opened,
    // mapped or verified anything, this would error.
    let s = ExternalIdSidecar::deferred(vec![desc("/nonexistent/external-ids-0.arrow", [0u8; 32])]);
    assert!(!s.is_open());
}

#[test]
fn resolution_opens_one_extent_not_all_of_them() {
    // CRITICAL C-6: one OnceLock over ALL extents means the first drill-down maps and
    // digests the whole family, permanently, taking the working set from 24.7 GiB to
    // ~35.9 GiB. Laziness must be per extent. The extent is selected by an
    // O(extents) first-key/last-key scan that maps nothing.
    let s = sidecar_with_three_extents();
    let _ = s.resolve(key_in_extent_1()).unwrap();
    assert_eq!(s.open_extents(), 1);
}

#[test]
fn an_out_of_order_extent_is_an_error_even_when_the_digest_matches() {
    // CRITICAL C-1: a digest does NOT subsume the sortedness check. The digest proves
    // the file is the one MANIFEST named; sortedness proves the binary search returns
    // the right answer. A build bug emitting an out-of-order extent produces a
    // correctly-digested file, and a mis-resolved ID denies the wrong entity while
    // leaving the intended target visible. Write a *correctly digested* file whose
    // rows are out of order and assert a typed error.
    let (dir, path, digest) = write_extent_fixture_unsorted(&[(b"c", 3u32), (b"a", 1), (b"b", 2)]);
    let s = ExternalIdSidecar::deferred(vec![desc(&path, digest)]);
    assert!(matches!(s.resolve(b"b"), Err(StoreError::InvalidSidecar { .. })));
    drop(dir);
}

#[test]
fn resolution_verifies_the_digest_and_fails_closed_on_corruption() {}

#[test]
fn an_unknown_external_id_resolves_to_none_not_an_error() {}

#[test]
fn a_missing_or_corrupt_locator_is_an_error_not_a_none() {
    // A `None` from the drill-down direction reads as "this item has no external ID",
    // which is a legitimate state. A corrupt locator must never be able to produce it.
}
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p tessera-store --lib sidecar
```

- [ ] **Step 3: Implement**

Move `ExternalIdExtent`'s mmap/decode/binary-search machinery from `external_ids.rs` into `sidecar.rs` and change four things:

1. **Nothing is opened in the constructor.** `deferred` stores paths, expected digests, and each extent's first/last key (from the side-manifest, so the selecting scan maps nothing).
2. **Laziness is per extent** (Critical C-6). Each `ExtentDesc` carries **its own** `OnceLock<Result<Extent, StoreError>>`. Resolution selects the extent by the O(extents) first/last-key scan, then opens **that one**. Record `open_extents()` for the residency test and for Task 15's memo, which must report steady state as *24.7 GiB + one extent*, not 24.7 GiB unconditionally.
3. **The sortedness check is KEPT** (Critical C-1). Delete any claim that the digest subsumes it — **that claim must not become spec**. Both run on first use of an extent, in **one sequential pass**: the digest is computed over the bytes as they are read, and the sortedness scan reads the same bytes. Off the hot path it costs nothing the digest pass does not already cost. Keep, in addition, the cheap cross-extent ordering check on `first_key`/`last_key` (O(extents)) that commit `669d7b5` added.
4. **Errors are typed and fail closed.** `StoreError::InvalidSidecar { path, detail }` for a digest mismatch, a schema mismatch, an out-of-order extent or an out-of-order extent list. Never a `None`.

Document at the type:

```rust
/// The caller's external-ID namespace, as a **sidecar** (contracts §0.3 deviation 9).
///
/// **TRANSITIONAL — a placeholder for a future adopted per-point metadata store**
/// (owner ruling, 2026-07-29). This is deliberately the simplest structure that
/// satisfies its two callers, and it occupies the same slot as design §8.3's vector
/// sidecar and §10.3's per-interaction routing row; the eventual store serves all of
/// them. **Extend the replacement, not this.** Everything the rest of the system knows
/// about external IDs is `resolve` and `external_id_of` — keep it that way, so the
/// storage behind them can be swapped by changing this file and its constructor call.
/// Design Appendix D does not forbid that adoption: it rejects adopting external systems
/// for the *access-control layer*, and this store is read only after the visibility test
/// has already returned "visible", so it never participates in masking.
///
/// **One identity, supplied or derived.** This is a *translation table* between two
/// representations of one identity, not a store of identities: an item whose caller
/// supplied no key has its `tessera_id` as its identifier, occupies no extent row, and
/// carries a `0xFFFFFFFF` locator slot — the ordinary case, not a missing value. Never
/// manufacture an external ID for an item that has none.
///
/// Two directions, neither on the viewport path: `external_id → entity` for
/// `/control/changes` past WAL retention and `/control/ingest`'s duplicate check, and
/// `entity → external_id` (through `ext-locator.u32`) for the `/v1/items`
/// drill-down. There is **no `tessera_id → entity` direction** — inversion is a pure
/// function of the deployment key and touches no file at all.
///
/// Nothing is mapped, scanned or verified until the first resolution, and then only
/// the one extent the key falls in. At 10⁹ the previous eager mmap-and-linear-scan put
/// 18.9 GB into the resident set at `Engine::open` for a structure the per-viewport
/// path never touches; a single lock over the whole family would have restored most of
/// that on the first click.
///
/// Integrity does *not* relax. A corrupted mapping suppresses the wrong item, so an
/// extent's digest **and its sortedness** are both verified before any answer comes out
/// of it — a digest proves the file is the one MANIFEST named, sortedness proves the
/// binary search returns the right answer, and a build bug emitting an out-of-order
/// extent produces a correctly-digested file. Every failure is a typed error, never a
/// `None` that would read as "unknown external ID".
```

- [ ] **Step 4: Run and delete the old module**

```bash
cargo test -p tessera-store
```
Delete `crates/tessera-store/src/external_ids.rs` and its `pub mod`/`pub use` lines. The `OracleIndex` test helper (the pre-mmap `Vec<Vec<u8>>` loader kept as an in-module oracle) moves to `sidecar.rs` and is retained — it is a second independent implementation of the search and has earned its keep.

- [ ] **Step 5: Remove Task 2's temporary feature**

Delete the `skip-id-index` feature from `crates/tessera-engine/Cargo.toml`, its `#[cfg]` in `session.rs`, and `ExternalIdIndex::disabled()`. The sidecar is now lazy for real; the measurement crutch has a permanent replacement.

- [ ] **Step 6: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy -p tessera-store --all-targets -- -D warnings
cargo test -p tessera-store
bash scripts/check-layers.sh
git add crates/tessera-store/src/sidecar.rs crates/tessera-store/src/lib.rs crates/tessera-store/src/error.rs crates/tessera-engine/Cargo.toml
git rm crates/tessera-store/src/external_ids.rs
git commit -m "feat(store)!: per-extent lazy external-ID sidecar, digest and sortedness both checked"
```

---

### Task 9: `tessera-engine` — gather, entity-space visibility, drill-down, a boot that opens nothing

**Files:**
- Modify: `crates/tessera-engine/src/viewport.rs` (`PointOut` at 41–54, `Engine::item` at 95–147, `row_to_point` at 309–338)
- Modify: `crates/tessera-engine/src/session.rs` (the `Engine` field at 151, `Engine::open` at 221–238, `resolve_external_id` at 410–415)

**Interfaces:**
- Consumes: Task 6's `ColumnsRef::tessera_id`, Task 8's `ExternalIdSidecar`, Task 5's `IdentityKey`, `crate::compose`'s precedence rule.
- Produces: `PointOut { tessera_id: TesseraId, x, y, scalars }`; **`Engine::visible_to(&self, session: &Session, gen: &Generation, entity: EntityId) -> bool`**; **`Engine::item(&self, session: &Session, id: TesseraId) -> Result<Option<ItemOut>, StoreError>`**; `Engine::resolve_external_id(&self, external_id: &[u8]) -> Result<Option<EntityId>, StoreError>`; `Engine::external_id_of(&self, entity: EntityId) -> Result<Option<Vec<u8>>, StoreError>`.

#### The design change this task carries: the visibility test moves into entity space

The previous draft answered "may this principal see this item?" in **row space**: project the fragment through the permutation, then test the row. That forced the endpoint to depend on a cached `RowProjection`, which produced three problems it then had to manage — a residual timing gap (C-5 narrowed but not closed), a pin/cache-key entanglement, and the behaviour that the endpoint **404s everything until the session has drawn a viewport**.

**All three dissolve, because the endpoint needs exactly one bit and that bit is an entity-space fact.** `compose` resolves visibility **per entity**, and the row space it produces is only ever a *representation* of that per-entity answer for range arithmetic. Read `compose` and the precedence is explicit:

```
visible(e) =
    match overlay.get(e):
        Some(entry) if entry.deleted || entry.suppressed        -> false
        Some(entry) with evaluate_terms = Some(terms)           -> terms.any(satisfied)
        Some(entry) otherwise (neutral: present, no verdict)    -> fragment.contains(e)
        None, and buffer has e with e.raw() >= fragment.watermark
                                                                -> item.terms.any(satisfied)
        None                                                    -> fragment.contains(e)
```

**Verified against the shipped code, by reading rather than by assertion:**

- `crates/tessera-engine/src/compose.rs:145–166` — rules 1–3: the overlay loop, per entity, with `deleted || suppressed → Some(false)`, else `evaluate_terms.map(|terms| terms.iter().any(|t| satisfied.contains(t)))`, else **no verdict at all** (the comment at `:141–144` states that a neutral entry deliberately yields none, because the fragment already reflects it).
- `compose.rs:171–189` — rule 4: buffered entities at or past `fragment.watermark` **with no overlay entry** (`overlay.get(entity).is_some() → continue` at `:175`), verdict `item.terms.iter().any(|t| satisfied.contains(t))`.
- `compose.rs:196–198` and `:102–107` — the clamps and `contains_row`. **This is the step that proves the equivalence.** `minus = fail ∩ base`, `plus = pass ∖ base`, and `contains_row(r) = !minus.contains(r) && (base.contains(r) || plus.contains(r))`. Take an entity `e` with row `r`: a `false` verdict puts `r` in `fail`, so either `r ∈ minus` (when `e ∈ base`) and `contains_row` is false, or `r ∉ base ∪ plus` and it is false anyway — **false either way**. A `true` verdict puts `r` in `pass`, so either `e ∈ base` and `base.contains(r)` is true, or `r ∈ plus` and it is true — **true either way**. No verdict leaves `r` in neither diff, so the answer is `base.contains(r)`.
- `crates/tessera-store/src/permutation.rs:182–196` — `project` is exactly *"for every entity set in `mask`, look up its row"*, so `base.contains(row_of(e)) ⟺ fragment.contains(e)`. **Hence `contains_row(row_of(e)) ≡ visible(e)` above, with no projection anywhere in it.**
- `crates/tessera-authz/src/fragment.rs:203` — `FrozenFragment::view()` returns a borrowed `BitmapView` straight from the mapping, so `contains` is an O(1) Roaring probe with no copy and no deserialisation. `watermark` is a plain field (`:186`).
- `crates/tessera-lifecycle/src/overlay.rs:56` and `crates/tessera-lifecycle/src/buffer.rs:196` — both expose `get(entity) -> Option<&_>`, so both lookups are single hash probes. **The per-entity resolution is genuinely available without a projection.**

**What this buys, and it is more than a performance note:**

1. **C-5 is CLOSED, not narrowed.** An identifier naming nothing and one naming an invisible item do **the same three constant-time lookups** and return the same `404`. There is no per-ID work to correlate against, at any scale, warm or cold. The row-space formulation could only ever narrow the gap by requiring the projection to be pre-warmed.
2. **The "404s everything until a viewport has been drawn" behaviour is gone.** The endpoint no longer depends on the projection cache at all, so a client may drill down as its first request.
3. **The pin and cache-key problems are gone.** There is no `(token, slice, pin)` key to choose, because there is no cached artifact in play. Pins fix geometry, never authorisation (lifecycle §2.3), and this path touches only authorisation.
4. **It is strictly simpler.** `Engine::item` loses the projection construction, the ad-hoc `RowProjection::new` at `viewport.rs:126–129`, and the whole `compose` call.

**One thing it does NOT change:** `compose` remains the authority. `visible_to` must be written **beside** `compose`, in `compose.rs`, sharing the precedence with it — ideally by factoring the verdict out of `compose`'s loop into a `fn verdict(overlay, buffer, satisfied, watermark, entity) -> Option<bool>` that **both** call, so the two cannot drift. A second, independent transcription of the precedence rule is exactly the fail-open the lifecycle design warns about twice.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn item_lookup_goes_through_the_permutation_not_a_column_scan() {
    // Before contracts r6 this was a linear scan of columns.arrow's entity_id over
    // every segment of every slice -- O(total rows) per drill-down click. Inversion is
    // now a pure function and the permutation is the only entity→row bridge (I4, §5.1).
    // Asserted behaviourally: the answer must be right for a row far from the start,
    // which a truncated or first-segment-only scan would miss.
}

#[test]
fn an_unknown_id_and_an_invisible_one_are_indistinguishable() {
    // Owner ruling: identical 404. At the engine layer that means one `Ok(None)`, from
    // one code path, with no error variant distinguishing the two.
    assert_eq!(engine.item(&session, TesseraId::new(0xDEAD_BEEF_DEAD_BEEF)).unwrap(), None);
    assert_eq!(engine.item(&session, id_of_a_suppressed_item).unwrap(), None);
}

#[test]
fn the_item_path_never_constructs_a_row_projection() {
    // CRITICAL C-5, now closed rather than narrowed. Instrument the projection cache
    // with a `builds` counter, call `item` with an unknown id and with an invisible one
    // on a session that has NEVER served a viewport, and assert `builds == 0` in both
    // cases -- not merely equal, ZERO. Under the entity-space test the endpoint has no
    // reason to construct a projection at all, so the strong assertion is available and
    // is the one to make: `equal` would still pass an implementation that built one for
    // both, which is option (b) and was rejected for putting 9.5-19.3 s on a click.
}

#[test]
fn drill_down_works_on_a_session_that_has_never_drawn_a_viewport() {
    // The behaviour the row-space formulation could not offer, and the reason Q4 is
    // retired: a client's FIRST request may be a drill-down, and a visible item must
    // return 200 -- not the uniform 404 the cached-projection design would have given.
    let s = authorise(&engine, grants_covering(item_a));
    assert!(engine.item(&s, tessera_id_of(item_a)).unwrap().is_some());
}

#[test]
fn visible_to_agrees_with_compose_over_every_precedence_case() {
    // The equivalence this task rests on, asserted rather than argued. For a fixture
    // exercising ALL FIVE branches -- deleted, suppressed, evaluate_terms pass,
    // evaluate_terms fail, neutral overlay entry, buffered-past-watermark, and plain
    // fragment membership -- assert for every entity with a row:
    //     visible_to(session, gen, e) == compose(...).contains_row(row_of(e))
    // If `verdict` was factored out of `compose` as specified, this test is what proves
    // the factoring did not change `compose`'s behaviour.
}

#[test]
fn a_sidecar_error_on_drill_down_is_an_error_not_a_missing_external_id() {
    // CRITICAL N-3. `.ok().flatten()` would turn a digest mismatch, an out-of-order
    // extent or a short locator into `external_id: null` on a 200 -- fail-open, and
    // exactly what Task 8's typed errors exist to prevent. The item path propagates.
    let engine = engine_with_corrupt_sidecar();
    assert!(matches!(engine.item(&session, id_of_a_visible_item),
                     Err(StoreError::InvalidSidecar { .. })));
}

#[test]
fn drill_down_resolves_an_external_id_for_a_post_build_entity() {
    // IMPORTANT I-9. An entity ingested after the build has no locator slot and no
    // extent entry. Returning `None` would report "this item has no external ID" for an
    // item that has one. The live map answers first.
    ingest(&engine, &[(b"post-build-key", terms)]);
    let e = engine.resolve_external_id(b"post-build-key").unwrap().unwrap();
    assert_eq!(engine.external_id_of(e).unwrap().as_deref(), Some(&b"post-build-key"[..]));
}

#[test]
fn engine_open_does_not_touch_the_sidecar() {
    // Residency. Compare VmRSS across an open with and without a large sidecar
    // present; assert the delta is under a page-cache-noise threshold (1 MiB).
}
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p tessera-engine --test viewport
```

- [ ] **Step 3: Rewrite the gather**

`row_to_point` (viewport.rs:314): `let tessera_id = TesseraId::new(cols.tessera_id()[idx]);`. `PointOut`'s field becomes `tessera_id: TesseraId`. Replace the doc comment at 43–49:

```rust
/// **I10, strengthened (contracts r6):** no entity ID leaves the engine on this path,
/// because none is stored. `columns.arrow` carries `tessera_id` at the row, so the
/// gather reads the identity it is allowed to show and cannot read the one it is not.
/// Entity IDs survive only in entity-space structures and as `permutation.bin`'s
/// index — never as a value on any path reaching `tessera-wire`.
```

- [ ] **Step 4: Factor the per-entity verdict out of `compose`, and add `visible_to`**

In `crates/tessera-engine/src/compose.rs`, lift the verdict logic out of `compose`'s two loops (`:145–166` and `:171–189`) into one function that **both** `compose` and `visible_to` call. Do not transcribe it twice; a second copy of a precedence rule is how fail-open arrives.

```rust
/// The per-entity verdict, `deleted > suppressed > evaluate_terms > buffered`, or `None`
/// when the overlay and the buffer have no opinion and the frozen fragment already
/// carries the answer. **The single source of this precedence** — `compose` turns it
/// into row-space diffs for range arithmetic, `visible_to` reads it directly for a
/// single entity. Two transcriptions of a precedence rule is how a suppression stops
/// suppressing (lifecycle §3, caught twice in review).
fn verdict(
    overlay: &Overlay, buffer: &IngestBuffer, satisfied: &FxHashSet<TermId>,
    watermark: u64, entity: EntityId,
) -> Option<bool> { … }

/// Is `entity` visible to this session — the ONE BIT `/v1/items` needs.
///
/// This is an **entity-space** question and is answered in entity space: the overlay
/// and buffer are hash probes, `fragment.contains` is an O(1) Roaring probe on a
/// borrowed mmap view, and no `RowProjection` is constructed or consulted. It is
/// therefore **identical work for an entity that does not exist, one that exists and is
/// invisible, and one that exists and is visible** — which is what closes the
/// `/v1/items` timing channel outright rather than narrowing it (design Appendix C, C4
/// annotation; Critical C-5).
///
/// Equivalent to `compose(...).contains_row(perm.row_of(entity))` wherever a row
/// exists — see this module's clamp doc: a `false` verdict lands in `minus` or outside
/// `base` and is false either way, a `true` verdict lands in `base` or `plus` and is
/// true either way, and no verdict falls through to `base`, which is `project`'s image
/// of the fragment. The row-space form exists for *range cardinalities*; a
/// single-entity test does not need it.
pub fn visible_to(
    fragment: &FrozenFragment, satisfied: &FxHashSet<TermId>,
    overlay: &Overlay, buffer: &IngestBuffer, entity: EntityId,
) -> bool {
    verdict(overlay, buffer, satisfied, fragment.watermark, entity)
        .unwrap_or_else(|| fragment.view().contains(entity_as_u32(entity)))
}
```

`compose`'s own behaviour must not change; `visible_to_agrees_with_compose_over_every_precedence_case` is the test that proves the factoring was a refactor.

- [ ] **Step 4a: Rewrite `Engine::item` — invert, test in entity space, and propagate**

```rust
    /// Returns `Ok(None)` both when `id` names nothing in this bundle and when it names
    /// an item the principal may not see — deliberately one outcome from one code path,
    /// so the server cannot differentiate what the engine does not tell it (owner
    /// ruling; contracts §3.2).
    ///
    /// **The timing channel is closed, not narrowed** (Critical C-5; design Appendix C,
    /// C4 annotation). Inversion is a pure function taking no I/O. The visibility test
    /// that follows is an entity-space question — three constant-time probes — and is
    /// **the same three probes for an identifier that names nothing and one that names
    /// an invisible item**. No `RowProjection` is constructed or read, so there is no
    /// per-ID cost for an attacker to correlate against, warm or cold. A row is located
    /// only after the answer is already "visible", and the sidecar is read only after
    /// that.
    ///
    /// **Returns `Err` rather than a fail-open `None`** (Critical N-3). A digest
    /// mismatch, an out-of-order extent or a short locator is a `500`, never an item
    /// served with `external_id: null` — `.ok().flatten()` would discard exactly the
    /// typed errors Task 8 exists to produce. This does not reopen C-5: the sidecar is
    /// touched only for an item already established as visible, so no attacker-drivable
    /// path can raise it.
    pub fn item(&self, session: &Session, id: TesseraId)
        -> Result<Option<ItemOut>, StoreError>
    {
        let generation = self.generation.load_full();
        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // ONE BIT, in entity space, O(1), before anything is looked up in row space.
        if !visible_to(
            &session.fragment, &session.satisfied,
            &generation.overlay, &generation.buffer, entity,
        ) {
            return Ok(None);
        }

        // Visible. Now — and only now — find the row, so the cost below is never
        // reachable by an identifier the principal may not see.
        // The permutation is the only entity→row bridge (I4, §5.1).
        for partition in generation.bundle.partitions.values() {
            for slice_data in partition.slices.values() {
                let Some(row) = slice_data.permutation.row_of(entity) else { continue };
                let Some(segment) = slice_data.segment_containing_row(row) else { continue };
                return Ok(Some(ItemOut {
                    scalars: row_to_point(segment, row, declared_scalars).scalars,
                    external_id: self.external_id_of(entity)?,   // N-3: propagate
                }));
            }
        }
        // Visible in entity space but with no row anywhere: a buffered item awaiting
        // flush. Same `Ok(None)`, same 404 — it has no geometry to return.
        Ok(None)
    }
```

Three consequences the executor must handle rather than paper over:

1. **`row_of` is reached only for a visible entity**, and is itself an O(1) bounds-checked slot read (`permutation.rs:160–171`), not a scan. The `for` loops are over *slices*, not rows — Phase 1 has one.
2. **The drill-down sidecar read happens only for a visible item** — after the visibility test, never before. Reading it earlier would make the sidecar's first-open cost observable for invisible IDs, reintroducing C-5 through the back door.
3. **`priority` is not in `ItemOut`** — but the reason has changed *(2026-07-30 fold)*. It is **no longer forbidden**: Important I-4 is retired, `priority` is `high16(tessera_id)`, and the response already carries the full `tessera_id`. It stays out because a drill-down has no use for a sort key, not because emitting it would disclose anything. **Do not add a guard for it, and do not re-add the retired sweep.**

- [ ] **Step 4b: `external_id_of` — the drill-down direction, live map first (Important I-9)**

The locator is written at build over the then-current high-water. An entity ingested afterwards has **no locator slot and no extent entry**, and a `None` for it would report *"this item has no external ID"* for an item that has one:

```rust
    /// `entity → external_id` for drill-down. Ordering mirrors `resolve_external_id`'s
    /// live-map-first rule, running the other way: post-build ingest is not in the
    /// bundle's locator or its extents, so the live map is consulted FIRST.
    ///
    /// `Ok(None)` means "this item genuinely has no caller external ID" — a legitimate
    /// state, since `external_id` is optional on ingest. It must never mean "I could not
    /// find out".
    pub fn external_id_of(&self, entity: EntityId) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(k) = self.established_inverse.lock().unwrap().get(&entity) {
            return Ok(Some(k.clone()));
        }
        if entity.raw() < self.sidecar.locator_len() {
            return self.sidecar.external_id_of(entity);   // 0xFFFFFFFF -> Ok(None)
        }
        if entity.raw() < self.allocator_high_water() {
            // Past the locator, below the high-water, and unknown to the live map:
            // an inconsistency, not an absent external ID. Fail closed.
            return Err(StoreError::InvalidSidecar { .. });
        }
        Ok(None)
    }
```

`established_inverse: Mutex<FxHashMap<EntityId, Vec<u8>>>` is maintained beside the existing `established` map (`session.rs:156`) and written by the **same two** writers — replay (`session.rs:237`) and accept (`session.rs:482–487`). It is bounded by post-build ingest volume, hence by WAL retention; it is not a second copy of the corpus. **Both maps must be updated in the same critical section**, or a `/control/changes` and a drill-down can disagree about the same item.

- [ ] **Step 5: Make `Engine::open` open nothing**

`session.rs:221–238`: replace `ExternalIdIndex::load(...)` with `ExternalIdSidecar::deferred(...)` built from the side-manifest's extent descriptors. Store the `IdentityKey` from MANIFEST on the `Engine`.

The `resolve_from_bundle` closure passed to `tessera_lifecycle::overlay::replay` (session.rs:237–238) now resolves through the sidecar and **must propagate errors rather than swallow them** — WAL replay resolving an external ID against a corrupt sidecar must fail closed, not return "unknown". `resolve_external_id` (session.rs:410) keeps its live-`established`-map-first ordering and gains a `Result`:

```rust
pub fn resolve_external_id(&self, external_id: &[u8]) -> Result<Option<EntityId>, StoreError> {
    if let Some(e) = self.established.get(external_id) { return Ok(Some(*e)); }
    self.sidecar.resolve(external_id)
}
```

- [ ] **Step 6: Run**

```bash
cargo test -p tessera-engine
cargo test --workspace   # tessera-server still red until Task 10
```

- [ ] **Step 7: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy -p tessera-engine --all-targets -- -D warnings
bash scripts/check-layers.sh
git add crates/tessera-engine/src/viewport.rs crates/tessera-engine/src/session.rs crates/tessera-engine/src/compose.rs crates/tessera-engine/tests/viewport.rs
git commit -m "feat(engine)!: gather tessera_id, invert on drill-down, test visibility in entity space, propagate sidecar errors"
```

---

### Task 10: The boundary — `tessera-wire` and `tessera-server`

Implements owner decision D5: **per-session handles are retired from the viewer plane.**

**Files:**
- Modify: `crates/tessera-wire/src/payload.rs`, `crates/tessera-wire/src/handles.rs`, `crates/tessera-wire/src/lib.rs`
- Modify: `crates/tessera-server/src/viewer.rs` (route at 25, `item` at 274–305, viewport handle minting at 147), `crates/tessera-server/src/error.rs`
- Modify: `scripts/check-layers.sh`

**Interfaces:**
- Consumes: Task 9's `PointOut.tessera_id`, `Engine::item(_, TesseraId)`.
- Produces: the viewer-plane points batch `(tessera_id: uint64, x: float32, y: float32, …)`; `POST /v1/items/{tessera_id}`.

- [ ] **Step 1: Write the failing tests**

In `crates/tessera-wire/tests/wire.rs`, keep `payload_bytes_never_contain_a_raw_entity_id_encoding` (line 145) **exactly as it is** — it is the I10 test and it must keep passing — and add:

```rust
#[test]
fn the_points_batch_identity_column_is_tessera_id() {
    let batch = decode_points(&viewport_ipc(&points, &tiles).unwrap());
    assert_eq!(batch.schema().field(0).name(), "tessera_id");
    assert_eq!(batch.schema().field(0).data_type(), &DataType::UInt64);
}

#[test]
fn payload_bytes_never_contain_the_identity_key() {
    // The key inverts every tessera_id. It is not secret against a bundle-holder and
    // is secret against a client; leaking it on the viewer plane would hand a client
    // entity space, which is exactly what I10 forbids.
}
```

In `crates/tessera-server/tests/http.rs`:

```rust
#[test]
fn item_404s_identically_for_unknown_and_invisible() {
    // Owner ruling: no differentiation. Status, error code and detail must match
    // byte-for-byte; anything that differs is an oracle for "this ID exists".
    let unknown = post_item(&client, &token, 0xDEAD_BEEF_DEAD_BEEFu64);
    let invisible = post_item(&client, &token, tessera_id_of_suppressed);
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(invisible.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown.text().unwrap(), invisible.text().unwrap());
}
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p tessera-wire && cargo test -p tessera-server
```

- [ ] **Step 3: Change the payload schema and retire the handle from the viewer plane**

`crates/tessera-wire/src/payload.rs`: the points batch's first field becomes `Field::new("tessera_id", DataType::UInt64, false)` built from `PointOut.tessera_id`. This **doubles the identity column's wire width** (4 B → 8 B) — at k=1000 (143,857 marks) that is **+0.58 MB on a 2.00 MB payload**. Record it; it is an input to the drawn-mark plan's Task 7.

`crates/tessera-wire/src/handles.rs`: **do not delete the module** — retire it with a recorded reason, because deleting it silently would erase the I10 argument from the codebase:

```rust
//! Per-session handle tables.
//!
//! **Retired from the viewer plane by owner decision (2026-07-29; contracts §0.3
//! deviation 8).** Handles existed because entity IDs could not cross the trust
//! boundary and the gather had nothing else to show. `columns.arrow` now carries a
//! `tessera_id` at the row — a keyed permutation of `(shard, entity)` that is
//! order-free and invertible only inside the boundary — so the engine shows an
//! identity that was always safe to show, and a point's identity is stable across
//! sessions, which is what lets a client bookmark, share and reconcile it. I10 is not
//! weakened by the retirement: it is what made the retirement possible, since after
//! contracts r6 no request-path artifact stores an entity ID at all (see
//! `tessera_engine::viewport::row_to_point`).
//!
//! Kept, not deleted, because Phase 3's node handles (`/v1/labels` returns
//! `node_handle`) are genuinely per-session and need exactly this machinery — and
//! because deleting the type would delete the argument with it.
```

Mark `HandleTable` `#[allow(dead_code)]` with a `// Phase 3: node handles` note, or gate it behind a `phase3` feature — either is acceptable; do not leave it dead and unexplained.

- [ ] **Step 4: Change the server**

`crates/tessera-server/src/viewer.rs`:
- Route: `.route("/v1/items/{tessera_id}", post(item))`, path extractor `AxumPath<u64>`.
- Handler: drop the handle-table lookup entirely; call `state.engine.item(&entry.session, TesseraId::new(raw))`, which now returns `Result<Option<ItemOut>, StoreError>` (Critical N-3). **Two outcomes map to two statuses, and the two `None`-shaped cases stay one:**

```rust
    // If the client supplied an epoch, it is checked HERE -- before inversion, and
    // identically for every identifier, so it opens no channel (contracts §2.2).
    if let Some(e) = body.epoch {
        if e != state.identity_epoch {
            return Err(ApiError::Conflict("stale identity epoch; re-resolve by external_id"));
        }
    }

    let item = match state.engine.item(&entry.session, TesseraId::new(raw)) {
        // A corrupt or unreadable sidecar is a SERVER fault, not "no such item".
        // `.ok().flatten()` here would serve a 200 with `external_id: null` and call a
        // digest mismatch a missing field -- fail-open, and precisely what Task 8's
        // typed errors exist to prevent (Critical N-3).
        Err(e) => return Err(ApiError::from(e)),          // -> 500
        // Owner ruling: identical 404 for "no such ID" and "exists but not visible".
        // ONE arm, one message, no branch above it -- a second construction site with a
        // different detail string would be the oracle this rule prevents.
        Ok(None) => return Err(ApiError::Unknown("unknown".to_string())),   // -> 404
        Ok(Some(item)) => item,
    };
```

**The `Err` arm must not be reachable by anything an attacker chooses.** It is raised only by the sidecar, which is read only after the item is established visible, so a probing client can never distinguish `500` from `404` by choosing identifiers — it can only see a `500` for an item it can already see. Assert that in the handler doc, because a future edit that moves the sidecar read earlier would silently make the status a visibility oracle.

- Viewport handler (line 147): replace `point_handles.push(handles.handle_for(point.entity_id).raw())` with the direct `point.tessera_id`.
- **Check for branch-dependent observability.** Grep the handler and `error.rs` for any `tracing` call inside the 404 path that could differ between the two cases; there must be none. Metrics likewise: one counter, not two. The identical-404 rule is defeated by one `tracing::debug!` in one arm.
- The response body carries `external_id` (base64) when the item has one and the field is absent when it does not. **This is the only place a caller external ID appears on the viewer plane** and it is by design (D4, D6); the conformance byte-scanner's viewer-plane sweep must be scoped to exclude this endpoint's response and to still cover viewport payloads and every log line (Task 13).

- [ ] **Step 5: Extend the layer check**

`scripts/check-layers.sh` currently asserts `grep -n "EntityId" crates/tessera-wire/src/payload.rs` is empty (I10). Add:

```bash
# I10 (contracts r6): no request-path artifact stores an entity ID. `columns.arrow`
# carries `tessera_id`, so the store's column reader must expose no entity_id accessor.
if grep -n "fn entity_id" crates/tessera-store/src/read.rs; then
  echo "FAIL: ColumnsRef exposes an entity_id accessor; contracts r6 removed the column"
  fail=1
fi

# The identity key inverts every tessera_id and must never reach the wire.
if grep -rn "IdentityKey" crates/tessera-wire/src/ crates/tessera-server/src/viewer.rs; then
  echo "FAIL: the identity key must not appear in the wire or viewer layers"
  fail=1
fi
```

**Do NOT add a `priority` grep** *(2026-07-30 fold)*. Earlier copies of this plan specified one here, because `priority` was then an *unkeyed* `splitmix64` of the entity ID and emitting it would have published a 16-bit residue of entity space per mark. `priority` is now `high16(tessera_id)` — a keyed prefix of a value the payload carries in full — so **Important I-4 is retired and this check must not be written.** The general rule it enforced still holds and would justify a grep for a *future* unkeyed derivative of the entity ID; it does not justify one for `priority`. See the historical note under "The routing principle" before reinstating anything here.

**And confirm Task 5's amendment landed.** The existing I4 check greps `impl From` against `EntityId|RowId|TermId|Handle` and does **not** name `TesseraId`, so an `impl From<EntityId> for TesseraId` — the single most natural convenience someone will reach for — passes the one guard that exists to stop it. Task 5 adds `TesseraId` to that alternation; verify here that it is present, because this is the task that owns the script.

- [ ] **Step 6: Run the whole workspace**

```bash
cargo test --workspace
```
Expected: PASS — the first fully green workspace since Task 6.

- [ ] **Step 7: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
git add crates/tessera-wire/src/payload.rs crates/tessera-wire/src/handles.rs crates/tessera-wire/src/lib.rs crates/tessera-wire/tests/wire.rs crates/tessera-server/src/viewer.rs crates/tessera-server/src/error.rs crates/tessera-server/tests/http.rs scripts/check-layers.sh
git commit -m "feat(wire,server)!: tessera_id is the viewer-plane identity; handles retired; identical 404 on /v1/items"
```

---

### Task 11: `/control/ingest` duplicate detection — the sidecar's admin-plane caller

Contracts §3.1 has always specified `409 conflict` for *"duplicate external IDs (detail lists them)"* with *"a 409 batch had **no effect**"*. It is **not implemented**: `crates/tessera-server/src/control.rs:154–257` never checks, within a batch or against existing state, and a re-ingest silently allocates a fresh entity ID and orphans the old one at `crates/tessera-engine/src/session.rs:484`.

**Files:**
- Modify: `crates/tessera-server/src/control.rs` (the ingest handler)
- Modify: `crates/tessera-engine/src/session.rs` (`resolve_external_ids` batch form)

**Interfaces:**
- Consumes: Task 9's `Engine::resolve_external_id`.
- Produces: `Engine::resolve_external_ids(&self, ids: &[Vec<u8>]) -> Result<Vec<Option<EntityId>>, StoreError>` — **one sorted pass over the batch touching each extent once**, not one open per row.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn ingest_rejects_duplicate_external_ids_within_one_batch() {
    // Contracts §3.1: 409, the detail lists them, and the batch has NO effect.
    let resp = post_ingest(&client, batch_with_external_ids(&[b"a", b"b", b"a"]));
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(resp.json::<serde_json::Value>().unwrap()["error"], "conflict");
    assert_eq!(status(&client)["entity_id_high_water"], high_water_before);
}

#[test]
fn ingest_rejects_an_external_id_already_in_the_bundle() {}

#[test]
fn ingest_rejects_an_external_id_ingested_after_the_build() {
    // IMPORTANT I-8. The sidecar covers the BUNDLE. An id ingested five minutes ago
    // lives in `Engine::established` (session.rs:156) and in the WAL, and in NO extent.
    // A dedup check that consults only the sidecar therefore misses precisely the
    // duplicates most likely to occur -- a retried client batch -- and silently
    // allocates a second entity ID, orphaning the first (session.rs:484). Dedup must
    // consult BOTH, live map first, exactly as `resolve_external_id` already does.
    post_ingest(&client, batch_with_external_ids(&[b"z"]));           // accepted
    let resp = post_ingest(&client, batch_with_external_ids(&[b"z"])); // fresh batch id
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(status(&client)["entity_id_high_water"], high_water_after_first);
}

#[test]
fn ingest_refuses_rather_than_wrapping_when_the_entity_space_is_exhausted() {
    // IMPORTANT I-1 at its serving edge: `AllocError::Exhausted` is a typed refusal and
    // the batch has no effect. Past u32::MAX two entities would share a tessera_id.
}

#[test]
fn ingest_without_external_ids_is_accepted_and_returns_tessera_ids() {
    // external_id is optional: an item without one is addressable only by its
    // tessera_id, which the response must return per accepted row.
}

#[test]
fn an_idempotent_retry_of_an_accepted_batch_is_a_200_not_a_409() {
    // Ordering matters and is not incidental: the batch-id replay check
    // (control.rs:197-209) stays FIRST.
}

#[test]
fn a_batch_resolution_opens_each_extent_at_most_once() {
    // Assert `sidecar.open_extents()` after a 10,000-key batch is bounded by the
    // extent count, not by the batch size. A per-row open would map the whole family.
}
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test -p tessera-server --test http ingest_rejects
```

- [ ] **Step 3: Implement, validate-first**

Follow `/control/changes`' existing shape (`control.rs:298–339`), which already validates the whole batch before applying any of it:

```rust
    // Validate the whole batch before any of it is applied (contracts §3.1: "a 409
    // batch had no effect"). THREE checks, in this order, because each is cheaper than
    // the next and eliminates work for it:
    //   1. duplicates within this batch, by a hash set over the supplied bytes;
    //   2. collisions against LIVE state -- `Engine::established` (session.rs:156),
    //      which holds every id ingested since the build and is the only place a
    //      post-build duplicate exists at all (Important I-8). Consulting only the
    //      sidecar would miss exactly the duplicate most likely to occur: a client
    //      retrying a batch under a fresh batch id, which today silently allocates a
    //      second entity and orphans the first (session.rs:484);
    //   3. collisions against the BUNDLE, in ONE batched sidecar call that sorts the
    //      keys first so each extent is opened at most once.
    // This is `resolve_external_id`'s existing live-map-first ordering (session.rs:407)
    // in batch form -- reuse it, do not restate it.
```

Make `external_id` optional in `parse_ingest_batch` (`control.rs:59–105`): a null or absent column value yields `None`. Return `tessera_ids` in the 200 body alongside `accepted`, `over_bound`, `over_bound_ids` — computed by `identity_key.forward(shard, entity)?` on the allocated entity IDs, **not** looked up. There is no minting, no collision check on the identity, and no re-mint path; if the executor is writing one, they are working from the superseded draft.

**Note for the record, not a defect to fix here:** returning those `tessera_id`s hands the control-plane principal exact known-plaintext pairs for the deployment key, and `/status`'s `entity_id_high_water` tells it which entity IDs they were. That is **accepted and intended** — the control plane is outside the set the blinding defends (Important I-3, and see the threat model above). It is recorded here so a later reader does not mistake this response field for an oversight, and so nobody proposes "fixing" it by weakening the admin API.

`Engine::resolve_external_ids` checks `established` first for the whole batch, then sorts the residual keys, walks the extents in order, and opens each at most once.

- [ ] **Step 4: Run**

```bash
cargo test -p tessera-server
```

- [ ] **Step 5: Quality gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash scripts/check-layers.sh
git add crates/tessera-server/src/control.rs crates/tessera-engine/src/session.rs crates/tessera-server/tests/http.rs
git commit -m "feat(server): 409 on duplicate external IDs, validate-first, batch has no effect"
```

---

### Task 12: The oracle — reimplement the construction, do not port it

The Python oracle is an **independent re-derivation** and its independence is the point. Change it only where the format changed; never to make it agree.

**Step 1 MUST be executed by a subagent that has not read `crates/tessera-types/src/identity.rs`** *(review round 2)*. "Do not read the Rust" is not enforceable against an agent that wrote the Rust three tasks ago and holds it in context — the instruction would be followed in form and violated in substance, and the resulting agreement would prove only that one model is self-consistent. **Dispatch a fresh subagent** whose entire input is `docs/design-memos/2026-07-30-tessera-id-construction.md`, contracts §2.6, and `reference/vectors/tessera_id.json`, with no access to `crates/tessera-types/`. If Task 12 is being run in the same session that ran Task 5, this is mandatory, not advisory. The rest of Task 12 (Steps 2–6, which are format plumbing rather than the construction) may be done in the main session.

**Files:**
- Create: `reference/oracle/identity.py`, `reference/tests/test_identity.py`
- Modify: `reference/oracle/bundle.py` (`Segment` at 37, `_read_segment` at 192–226, `external_id_of` at 151–166), `reference/oracle/viewport.py` (49, 60, 98), `reference/oracle/harness.py` (150), `reference/tests/test_differential.py`

**Interfaces:**
- Consumes: the contracts r6 schemas; Task 3's memo and `reference/vectors/tessera_id.json`.
- Produces: `identity.forward/invert`; `Segment.tessera_id` and a **derived** `Segment.entity_id`; `Bundle.tessera_id_of(entity_id)`; **`identity.priority_of(tessera_id)` and a row-order re-derivation from `(morton, tessera_id)`** *(2026-07-30 fold)*.

- [ ] **Step 1: Reimplement the bijection in Python, from the spec text**

`reference/oracle/identity.py`. **Write it from `docs/design-memos/2026-07-30-tessera-id-construction.md` and contracts §2.6 — do not read `crates/tessera-types/src/identity.rs` while doing so.** The oracle's independence is the whole reason it exists; a port proves only that copy-paste works.

`reference/tests/test_identity.py` asserts against `reference/vectors/tessera_id.json` (Task 3), asserts round-tripping, and asserts that `tessera_id_of(entity)` reproduces the bundle's stored column for every row of a small bundle.

Mind Python's unbounded integers: every step needs an explicit `& 0xFFFF_FFFF_FFFF_FFFF` (and `& 0xFFFF_FFFF` on the halves). This is the single most likely defect and it will show as a vector mismatch, which is what the vectors are for.

- [ ] **Step 2: Derive entity IDs from the permutation, chunked (Important I-3)**

`_read_segment` reads three columns by name (`entity_id`, `x`, `y`); `entity_id` becomes `tessera_id`. The oracle **still needs entity IDs** — `viewport.py:98` tests mask membership in entity space, and `conformance/tests/test_byte_scan.py:260` needs the entity-ID set to sweep for.

Two independent derivations are now available, and the oracle should prefer the one that does not depend on the key:

```python
def _entity_of_rows(perm: np.ndarray, rows: np.ndarray) -> np.ndarray:
    """row -> entity, by inverting permutation.bin's entity_to_row for TOUCHED rows only.

    Contracts r6 removed the entity_id column from columns.arrow; the permutation is
    now the only key-independent artifact relating the two spaces, which is what §5.1
    and I4 always said it was.

    Computed for the rows asked about rather than materialised whole: a full inversion
    at 10^9 needs ~17-20 GB transient, concurrent with a ~24 GiB server on a 47 GiB
    box, and would swap the machine the differential run is measuring. Chunk the scan
    over `perm` in slices of 2^24 and collect only the entries landing in `rows`.
    """
```

`Segment` gains `tessera_id: np.ndarray  # uint64, row order` and keeps `entity_id: np.ndarray  # uint32, row order, DERIVED`. A **cross-check** the oracle should assert once per bundle on a sample of rows: `identity.forward(shard, entity_of_row[r]) == tessera_id[r]`. That is the only test that catches a key/column disagreement, and it uses both derivations against each other.

- [ ] **Step 2a: Re-derive `priority` and the row order from the identity** *(2026-07-30 fold)*

The oracle re-derives two things it previously derived differently, and both must come from the spec text, not from the Rust:

1. **`priority = (tessera_id >> 48) & 0xFFFF`** — a prefix of the identity, **not** `splitmix64` over the entity ID. Assert it against the stored column for every row of a small bundle; a mismatch means the build's derivation and the spec's disagree, which is exactly what this check exists to catch.
2. **Row order is `(morton, tessera_id)` ascending, with no further tiebreak** — see "The sort and tiebreak statement" above; transcribe it, do not paraphrase. Add an assertion that re-sorting a small bundle's `(morton_of(x, y, extent), tessera_id)` pairs reproduces the stored row order exactly. **Row order is now key-dependent**, so this derivation reads `identity.key` and `identity.shard_id` from MANIFEST — which `identity.py` already does, so it adds a dependency and no new artifact. Any previous oracle code re-deriving order from `(morton, priority, entity_id)` is wrong for r6 and must go.

- [ ] **Step 3: Re-point `external_id_of` and add the sidecar round-trip**

`external_id_of` currently builds an `entity → external_id` inverse map by scanning the extents. It stays — that direction is real and is now the drill-down path — but it should read `ext-locator.u32` and assert agreement with its own inverse scan on a sample, which is the only check that catches a bad locator.

Add `tessera_id_of(entity_id) -> int` (pure, no file read) and:

```python
def sidecar_round_trips(self) -> None:
    """For a sample of entities: external_id_of(e) resolves back to e through the
    sorted extents. Both directions of a mapping stored once sorted by key and once
    indexed by entity; they must agree or /control/changes addresses the wrong item."""
```

- [ ] **Step 4: Update the differential harness**

`harness.py:150` `change(...)` still posts the caller's `external_id` base64 — **unchanged**, that is the admin plane. Add an `item(tessera_id)` method posting to `/v1/items/{tessera_id}`. `test_differential.py:188–189, 248–250` switch to the external-ID path where they address `/control/changes` and to `tessera_id_of` where they address `/v1/items`. Read each call site and choose by plane, not by search-and-replace.

Add a differential assertion that `/v1/items` on a visible item returns the same `external_id` the oracle derives.

- [ ] **Step 5: Run at small scale**

```bash
cargo build --release
uv run pytest reference/tests/ -v
```
Expected: PASS against a 250k bundle. **If it fails, the engine is the suspect until proven otherwise.**

- [ ] **Step 6: Commit**

```bash
git add reference/oracle/identity.py reference/oracle/bundle.py reference/oracle/viewport.py reference/oracle/harness.py reference/tests/test_identity.py reference/tests/test_differential.py
git commit -m "test(reference): oracle reimplements the tessera_id bijection and derives entity IDs from the permutation"
```

---

### Task 13: Conformance — the byte-scanner's premise is now stronger, and its scan needs a scope

*(The title previously cited Important I-4, which the 2026-07-30 priority fold retires. The scoping work — deciding the sweep's width against a random-looking `u64` identity column — is unaffected and is still the substance of this task.)*

**Files:**
- Modify: `conformance/tests/test_byte_scan.py` (18–23, 112–116, 260, 301), `conformance/tests/test_restart_replay.py` (schema 112–117, `ext_b64` 170–171)

**Interfaces:**
- Consumes: Task 12's derived `Segment.entity_id` and `identity.py`.
- Produces: a byte-scan that sweeps for entity IDs, the identity key, and caller external IDs outside the drill-down response.

- [ ] **Step 1: Decide the scan's width and scope explicitly**

The current scan decodes a `handle` `UInt32` column and sweeps for 4-byte entity-ID encodings. Two changes, both of which must be **decided, not defaulted**, or the suite flakes:

1. The wire identity column is now `tessera_id: UInt64`. The decode moves to `u64`.
2. **The 4-byte sweep over a payload containing a random-looking `u64` column will hit by chance.** With 143,857 8-byte identities and a 10⁹-entity ID space, a naive "does any 4-byte window equal any entity ID" scan expects on the order of tens of spurious hits per run on the full fixture (the review estimated ~44). **Rule: the sweep excludes the `tessera_id` column's own buffer and scans every other buffer, every metadata field and every log line.** Record that scoping decision in the test docstring with its reason — an unscoped scan either flakes or gets weakened silently later, and a weakened I10 test is worse than none.

3. ~~**Add a `priority` sweep** *(Important I-4)*.~~ **DO NOT ADD IT** *(2026-07-30 fold)*. The sweep was specified because `priority` was an *unkeyed* `splitmix64` of the entity ID, so its appearance on the viewer plane would have published a 16-bit residue of entity space per mark. `priority` is now `high16(tessera_id)`, a keyed prefix of a value the payload already carries in full, and **Important I-4 is retired by argument** — the sweep has nothing left to protect. It was also the weakest test in the suite as specified: a 2-byte sweep hits by chance far more often than the 4-byte one, which is why it needed scoping to schema field names and declared-scalar buffers. **Retiring it removes a flaky test and a false guard at the same time; do not reinstate it without first re-establishing that `priority` is unkeyed.**

Add the identity-key sweep (the key must appear nowhere on the viewer plane) and an **explicit negative control**: assert that a known `tessera_id` **is** found in the payload, so the test cannot pass vacuously by scanning nothing.

- [ ] **Step 2: Strengthen the docstring**

```python
"""I10: no entity ID appears in any viewer-plane payload or log.

Contracts r6 made this structural rather than a discipline: columns.arrow no longer
stores an entity ID, so the gather cannot produce one, and the handle table is no
longer the only thing standing between entity space and the wire. The entity IDs swept
for here are derived by the oracle from permutation.bin, so their absence from the wire
is a property of the format rather than of a mapping step.

The sweep additionally covers (a) the deployment identity key, which inverts every
tessera_id and must never leave the server; (b) caller-supplied external IDs, which are
admin-plane identifiers (SA D14) and appear on the viewer plane in exactly one place by
design: the /v1/items drill-down response (D4). Viewport payloads and all logs are swept;
that one drill-down response body is excluded, by name, and the exclusion is narrow on
purpose.

Not swept: `priority`. Contracts r6 defines it as high16(tessera_id) -- a KEYED prefix of
a value this payload already carries in full -- so it narrows nothing and there is nothing
to protect. An earlier draft of this suite swept for it, when priority was an unkeyed
splitmix64 of the entity ID; that prohibition (plan Important I-4) was retired by argument
on 2026-07-30. An UNKEYED per-mark derivative of the entity ID would still be forbidden,
and would need its own sweep.

Scope: the tessera_id column's own buffer is excluded from the 4-byte entity sweep,
because a random-looking u64 column produces chance 4-byte matches against a 10^9
entity space. Every other buffer, metadata field and log line is scanned.
"""
```

- [ ] **Step 3: Update the restart-replay ingest schema**

`test_restart_replay.py:112–117` constructs the ingest Arrow stream with `external_id` non-null. Make it nullable to match Task 11, add a case ingesting rows without one, and assert the accepted rows come back with `tessera_ids`. Keep `ext_b64` (170) addressing `/control/changes` by the caller external ID — that path is unchanged, and the test's value is that it proves so.

- [ ] **Step 4: Run**

```bash
cargo build --release
uv run pytest conformance/tests/ -v
```
Run it **five times** and confirm zero flakes; the scan-scope decision is the thing being validated.

- [ ] **Step 5: Commit**

```bash
git add conformance/tests/test_byte_scan.py conformance/tests/test_restart_replay.py
git commit -m "test(conformance): sweep derived entity IDs, the identity key and misplaced external IDs"
```

---

### Task 14: The 10⁹ rebuild — once, behind an enforced precondition

**Files:**
- Modify: `scripts/build_full.sh`

**Interfaces:**
- Consumes: everything above; the format is final at this point and must not change afterwards.
- Produces: the new 10⁹ bundle, its wall time, and its peak RSS.

- [ ] **Step 1: Validate at small scale first — twice**

```bash
cargo build --release
./target/release/tessera build --limit 250000   --out /tmp/tessera-250k
./target/release/tessera build --limit 2422486  --out /tmp/tessera-2m4
./target/release/tessera verify /tmp/tessera-250k
./target/release/tessera verify /tmp/tessera-2m4
cargo test --workspace
uv run pytest reference/tests/ conformance/tests/ -v
```
All green before proceeding. A format error found at 10⁹ costs 90 minutes and a bundle that cannot be un-deleted.

**Also test the key-carry path explicitly at small scale**, since it is the one thing that cannot be fixed after clients hold identifiers:

```bash
# N-1: no key flag at all must REFUSE, before any work and before any directory is made.
./target/release/tessera build --limit 250000 --out /tmp/tessera-refuse ; echo "exit=$?"
# assert: exit != 0, message names all FOUR sources, /tmp/tessera-refuse absent or empty

# N-1 again, with a valid key file sitting where a careless implementation might find it.
printf '[identity]\nkey = "0f0e0d0c0b0a09080706050403020100"\n' > /tmp/tessera.toml
cp /tmp/tessera.toml ./tessera.toml
./target/release/tessera build --limit 250000 --out /tmp/tessera-refuse2 ; echo "exit=$?"
# assert: STILL exit != 0. A file the binary finds on its own is not a human deciding.
rm -f ./tessera.toml

# Q6: the key file, named explicitly, is an explicit decision and its key lands verbatim.
./target/release/tessera build --limit 250000 --id-key-file /tmp/tessera.toml --out /tmp/tessera-250k-f
# assert: MANIFEST identity.key == the file's key; a carry from this bundle reproduces it

./target/release/tessera build --limit 250000 --mint-id-key --out /tmp/tessera-250k
./target/release/tessera build --limit 250000 --carry-id-key-from /tmp/tessera-250k --out /tmp/tessera-250k-b
# assert: identical identity.key, identical identity.epoch, identical tessera_id column,
#         byte-identical columns.arrow
./target/release/tessera build --limit 250000 --carry-id-key-from /tmp/tessera-250k --bump-id-epoch --out /tmp/tessera-250k-c
# assert: same key, epoch+1, IDENTICAL tessera_id column (the epoch signals staleness;
#         it is not an input to the permutation)
```

**`scripts/build_full.sh` must take the key flag as a required argument and pass it through — never hard-code one.** A script that supplies `--mint-id-key` or `--carry-id-key-from` on its own defeats N-1 entirely: the refusal exists so that a *human* decides, and a default in a shell script is not a human deciding. If the script cannot be run without the operator naming the flag, the gate holds; if it can, it does not.

- [ ] **Step 2: Check disk, verify the baseline precondition, then delete the old bundle**

**OWNER RULING (Q5, 2026-07-29): YES — Task 14 may delete `/tmp/tessera-1e9`, conditional on Task 2 having captured the current-format baseline first.** The condition is not a courtesy and it is not advisory. Deletion is irreversible and the old-format numbers can never be re-measured afterwards; the entire value of the ruling is that by the time it is exercised, deletion costs *only the ability to re-run*, and that is true **only if Task 2's artifacts actually exist**. So the ordering is **enforced by a precondition check that must pass before `rm` is typed**, not by an executor remembering that Task 2 came first.

```bash
df -h /
du -sh /tmp/tessera-1e9
```

**CORRECTED (Task 2 Step 3a, measured):** `terms/` is 0.25 GiB, not the 3.7 GiB residual or contracts §2.4's ~5.6 GiB estimate this section previously branched on — see the "Whole bundle on disk" table in the Arithmetic section above for the full measured breakdown and root cause (a GB/GiB unit conflation, not a real per-file cost). The new bundle is projected at **~43.6 GiB**, not ~47.0/48.9. The old is 47.63 GiB measured (51.1 GB decimal); free space is ~15 GiB. **They still cannot coexist**, which is why the ruling exists — the corrected numbers change the margin, not the conclusion.

**Step 2a — the enforced precondition. Every one of these must pass. If ANY fails, STOP and report to the owner; do not delete anything, and do not "just re-run Task 2" against a bundle you are about to destroy without saying so.**

```bash
# 1. Task 2's four-arm baseline exists, is committed, and covers the OLD bundle.
test -f probes/tail-discrimination.json                       || { echo "MISSING baseline JSON"; exit 1; }
test -f docs/design-memos/2026-07-30-tail-discrimination.md    || { echo "MISSING baseline memo"; exit 1; }
git log --oneline -1 -- probes/tail-discrimination.json        # must be a real commit, not untracked
git status --porcelain probes/tail-discrimination.json docs/design-memos/2026-07-30-tail-discrimination.md
#    ^ must be EMPTY: an uncommitted baseline is one `git checkout` from gone.

# 2. It contains all four arms and their per-arm numbers, not a stub.
python3 - <<'PY'
import json,sys
d=json.load(open("probes/tail-discrimination.json"))
arms=d.get("arms",{})
missing=[a for a in "ABCD" if a not in arms]
assert not missing, f"baseline missing arms {missing}"
for a,v in arms.items():
    for k in ("p50","p99","max","majflt_delta","peak_rss"):
        assert v.get(k) is not None, f"arm {a} missing {k}"
print("baseline complete:", {a:arms[a]["p99"] for a in arms})
PY

# 3. Task 2 Step 3a's component `du` of the OLD bundle is recorded in the memo.
grep -q "terms/" docs/design-memos/2026-07-30-tail-discrimination.md || { echo "MISSING terms/ du"; exit 1; }

# 4. The memo carries an explicit CONFIRMED or REFUTED verdict.
grep -Eq "CONFIRMED|REFUTED" docs/design-memos/2026-07-30-tail-discrimination.md || { echo "NO VERDICT"; exit 1; }
```

**One further precondition, added by the 2026-07-30 priority fold.** The confirming measurement for the priority defect — the count of (tile, principal) pairs with V > 2×10⁶ over the existing 10⁹ k-sweep — reads **this** bundle and cannot be re-run once it is gone. It is being run concurrently by another agent, so this gate must confirm its result is **recorded and committed** before deleting, exactly as it does for Task 2's baseline:

```bash
# 5. The priority-defect confirmation over the OLD bundle is recorded and committed.
#    (Owner decision 2026-07-30; the file is whatever the concurrent measurement lands
#    as -- confirm the path with the owner rather than guessing it, and do NOT delete on
#    an absent result.)
```

**If that result is not yet recorded, STOP and report** rather than deleting. Unlike Task 2's baseline this measurement is not a gate on the *plan* — the redefinition is justified by argument — but it is irreplaceable evidence about a defect in the shipped format, and deleting its only corpus to save 47 GiB an hour early is not a trade anyone would choose deliberately.

**And one judgement the checks cannot make: if Task 2's verdict was REFUTED, deletion is NOT authorised by Q5.** The ruling was given on a plan whose premise was live. A refutation sends the executor back to Task 2 Step 4's instruction — *stop and report; do not soften a refutation* — and the owner decides again with the refutation in hand. Q5 answers "may the baseline's *storage* be reclaimed once the baseline is *captured*", not "may the rebuild proceed regardless of what the baseline said".

**Step 2b — report the numbers, then delete.** Report to the owner: free space, the old bundle's measured size, the new bundle's projected size using Task 2's `terms/` measurement, and confirmation that every precondition above passed. Then, and only then:

```bash
rm -rf /tmp/tessera-1e9
df -h /
```

**Two fallbacks remain on the record**, because a precondition failure or a refutation puts them back in play:

- **(b)** Build at `--limit 250000000` (2.5 × 10⁸, ~12 GiB) alongside the existing bundle, measure there, extrapolate. Fits today. Weaker evidence — the hypothesis is specifically about exceeding RAM, and a quarter-scale bundle does not exceed it — so this arm tests correctness at scale, not the residency claim.
- **(c)** Attach or free storage elsewhere and build both.

**Nothing else on the disk is in scope.** Q5 authorises the deletion of `/tmp/tessera-1e9` and of nothing else.

- [ ] **Step 3: Build, once, against the final format**

**The key decision for this bundle, made here and not left to the executor: the 10⁹ bundle MINTS A NEW LINEAGE at `epoch = 1`.** There is no prior key to carry — the existing `/tmp/tessera-1e9` predates `identity` in MANIFEST entirely, so `--carry-id-key-from` has nothing to read and `--id-key-file` has no file to read from yet. `--mint-id-key` is therefore the correct and only available source, and it is typed **explicitly on the command line**, which is exactly what N-1 requires. **Nothing else in this plan may be built with a different source, and the mint is a one-time event: every later rebuild of this deployment carries this key forward.**

**Record the key off-repo before anything else proceeds** — see Step 4, which is where the round trip is verified. The key is printed by `--mint-id-key`; it exists nowhere else outside the bundle until it is written down; and the bundle it is in is the bundle a later mistake deletes.

The build runs **through `scripts/build_full.sh`**, not through a bare binary invocation, because the script is what supplies `--points`, `--pairs`, `--extent` and `--slice` for the full corpus (`scripts/build_full.sh:39–44`). A bare `tessera build --out <path>` would refuse under N-1 *and* would be missing every input:

```bash
df -h /                      # again, immediately before
/usr/bin/time -v bash scripts/build_full.sh /tmp/tessera-1e9 --mint-id-key 2>&1 | tee /tmp/claude-1000/build-1e9.log
```

The script forwards `--mint-id-key` from its trailing arguments (Task 7 Step 3a) and hard-codes no key flag of its own; if it refuses because no key argument was given, that is N-1 working, and the fix is to type the flag rather than to edit the script.

Expected ~90 minutes. Peak RSS was 26.3 GiB on the old format. Two changes pull in opposite directions and **both must be recorded separately**: the 4 GB `vec![NODE_NONE; n]` at `pipeline.rs:416` is deleted (−4 GB), and the new 4 GB `ext-locator` array is allocated near the same moment (+4 GB). The identity column costs **no extra allocation** — it replaces the entity column in place — so unlike the superseded random-mint design there is no 8 GB `ids` vector and no 4 GB `order` vector. If peak RSS rises at all, say so.

**Record the identity key** printed at build into the deployment's key file before anything else — that is what `--id-key-file` exists for (Q6), and it is the only copy that survives the bundle. Without it, no future rebuild can carry the lineage. Write it **outside** the repository and outside `/tmp`; the file is never committed and never created by the build itself. **This is the moment the deployment's identity lineage begins**, and it is the one step in this plan whose omission cannot be repaired by re-running anything.

- [ ] **Step 4: Verify, and settle the pairs/postings figure**

```bash
./target/release/tessera verify <path>
du -sh <path>
du -sh <path>/*/partitions/*/slices/*/segments/*/columns.arrow
du -sh <path>/*/partitions/*/entities/
du -sh <path>/*/partitions/*/terms/
```
Record every component against the "Arithmetic" projection. **`terms/` was already settled on the OLD bundle at Task 2 Step 3a** — measuring it there rather than here is deliberate, because the disk gate at Step 2 above depends on it and measuring after the ruling informs nothing. This step's job is the *before/after comparison*: confirm the new bundle's `terms/` matches the old (the term index is untouched by this change, so a difference is a finding), and record every other component. A material miss anywhere means the arithmetic was wrong and Task 15's conclusions need it corrected first.

**Record `identity.key` and `identity.epoch` from the new MANIFEST into the deployment key file (Q6)** before anything else, alongside the build log. Without the key, no future rebuild can carry the lineage; without the epoch, a later repartitioning cannot signal staleness correctly. Verify the round trip once, immediately: a small build with `--id-key-file <that file>` must reproduce the same `identity.key`.

- [ ] **Step 5: Commit the script, not the bundle**

```bash
git add scripts/build_full.sh
git commit -m "chore(scripts): 10^9 build against the contracts-r6 format"
```

---

### Task 15: Re-measurement — confirm or refute, against Task 2's baseline

**Files:**
- Modify: `scripts/bench_p99.py`
- Create: `docs/design-memos/2026-07-30-identity-results.md`

**Interfaces:**
- Consumes: Task 2's four-arm baseline; Task 14's bundle.
- Produces: the numbers Task 16's `phase1-results.md` quotes, and the input the drawn-mark plan's Task 7 needs before it sets `DEFAULT_MAX_K`.

- [ ] **Step 1: Sweep k on the new bundle**

Run `scripts/bench_p99.py` at k ∈ {30, 50, 500, 1000, 2500, 5000} — same sweep, same seed, same principal (w = 10⁴ random grants), ≥ 2,000 viewports per k, one warm-up pass reported separately. Record p50/p99/max server-side and end-to-end, payload bytes, `majflt` delta, peak RSS.

- [ ] **Step 2: Re-run the warm/cold arms**

Run `scripts/discriminate_tail.py` against the new bundle (arms A and B only — arms C and D used the `skip-id-index` feature, which Task 8 removed because the sidecar is lazy for real; the new arm A *is* the old arm C). This closes the loop.

- [ ] **Step 3: Measure the drill-down path separately**

New, and load-bearing under D4/D6 because drill-down is now a designed feature rather than an incidental cost:

- p50/p99 of `/v1/items` on a **cold** sidecar (first click after boot) and on a warm one. Report both; the cold number is one extent's sequential digest-and-sortedness pass (~1.5 GiB) plus the locator's touched pages.
- `open_extents()` and RSS delta after 1, 10 and 1,000 drill-downs **deliberately spread across the key space so that every extent is touched**. **Assert the working set is 24.7 GiB + one extent per touched extent, not 24.7 GiB + the whole family** — Critical C-6's regression test at scale — and **report the all-extents-touched figure against the predicted 43.3 GiB** (24.7 + 10 × 1.49 + 3.7 locator). A residency claim that holds only for a clustered click pattern is not a residency claim; this is the pathological case and it is the one worth measuring.
- `/v1/items` timing for **three** populations, ≥ 1,000 samples each: an unknown ID, a known-but-invisible one, and a **visible** one, on a session that has **never drawn a viewport** as well as on a warm one. The first two **must be indistinguishable** — and under the entity-space visibility test they should be indistinguishable *on a cold session too*, which the row-space design could not have delivered. Report the visible population separately; it is legitimately slower (it does row lookup and a sidecar read) and that is not a channel, because reaching it already required visibility. This is Critical C-5's empirical check; the structural check is Task 9's `builds == 0` counter test, and the memo reports both.

- [ ] **Step 4: Write the results memo, with the verdict stated first**

`docs/design-memos/2026-07-30-identity-results.md`. Lead with the verdict, not the tables:

- **Bundle:** before/after component sizes against the projection; the `terms/` reconciliation from Task 14 Step 4; build wall time and peak RSS, with the `NODE_NONE` deletion and the `ext-locator` allocation called out **separately**.
- **Residency:** working set before/after; RSS at steady state stated as **24.7 GiB + one extent**; `majflt` per viewport before/after. This is the direct evidence. **State the qualifier with the pathological number, not after it.** The all-extents figure (predicted 43.3 GiB against 47 GiB) is a property of **this corpus's 8-byte synthetic keys under the placeholder sidecar design** — the pathological case is bounded by the sidecar's own size, which is bounded by the deployment's key length, not by anything intrinsic to the format. The memo must **not** report 43.3 GiB as a general property of Tessera, and must **not** project it onto other key lengths: what a deployment with real caller-supplied keys costs *resident* is a property of Ruling B's replacement store and is out of scope for this plan (design §10.3's per-interaction row). Report what was measured, against the corpus it was measured on. The *disk* sizing table in the next bullet is the deployment-dependent number the memo does carry, and the two must not be conflated.
- **Latency:** the k sweep beside the baseline table. **The specific claim to test: does `p99 − p50` stop being a near-constant ~42–47 ms?** If it does, the hypothesis is confirmed and the tail was residency. If p50 improves but the constant tail survives, the hypothesis is **refuted** and the memo must say so in those words and identify what the tail did correlate with.
- **Drill-down:** Step 3's numbers, including the unknown-vs-invisible timing comparison.
- **The k=30 exit gate (p99 < 10 ms):** whether it passes now, reported alongside the drawn-mark plan's finding that this criterion *"would pass against a mark budget the product does not want"* — a pass at k=30 is necessary for Phase 1 and not sufficient for the product.
- **Separated attribution, stated plainly:** how much came from dropping `node_id` (−3.8 GiB), how much from de-residenting the external-ID extents (−18.9 GiB), and how much from the identity swap (**zero bytes, by construction — it is width-neutral**). The reader must be able to tell, and the two must not be conflated in favour of the more interesting change. **The priority redefinition costs and saves nothing either** *(2026-07-30)*: it is the same column at the same width, and the memo must say so rather than let a correctness fix collect credit for a residency result. What it *may* legitimately note is that row order changed, so any before/after comparison at the row level is between different orders.
- **Input to the drawn-mark calibration:** the measured transport cost at 8 B/point, and the note that the handle-table ceiling (P3) is no longer an input under D5.
- **The sidecar's cost is a property of the deployment, not of Tessera** *(Ruling A)*. State that the measured 14.9 GiB of extents and 3.7 GiB of locator are what **this corpus** costs — 8-byte keys the build synthesises from `source_id` for a corpus whose callers supply nothing — and that under the owner's framing those items' identity is their `tessera_id`. A deployment whose callers supply no external IDs pays **zero** sidecar disk; the synthesis is kept as the only 10⁹ test article for the two sidecar directions and for Step 3's pathological residency case. Do not let the number read as a floor.
- **The sizing threshold, restated against the measurement** *(owner, 2026-07-29)*. Reproduce the table — 14.9 GiB at 8-byte keys, 22.4 at 16-byte binary UUIDs, 41 at 36-character UUID strings, 67 at the new 64-byte cap, with the locator flat at 3.7 GiB throughout — and the threshold: **once mean key length exceeds ~16 bytes the store dominates the bundle and compression, or Ruling B's replacement store, pays for itself.** Phase 1 builds neither; the point of the number in this memo is that the first deployment with long keys meets a decision that already exists. Note which remedy applies where: dictionary or prefix encoding for long human-readable keys, the replacement store for random UUIDs, which compress essentially not at all.

- [ ] **Step 5: Commit**

```bash
git add scripts/bench_p99.py docs/design-memos/2026-07-30-identity-results.md
git commit -m "test(bench): 10^9 re-measurement against the contracts-r6 identity format"
```

---

### Task 16: Close Phase 1

The owner has settled that Phase 1 closes on the **new** format. Phase 1 Task 16's Steps 4 and 5 were deferred for exactly this; they run now.

**Files:**
- Create: `docs/superpowers/plans/phase1-results.md`
- Modify: `docs/superpowers/plans/2026-07-28-phase1-walking-skeleton.md`, `CLAUDE.md`

**Interfaces:**
- Consumes: Tasks 14 and 15.
- Produces: the Phase 1 exit record.

- [ ] **Step 1: Phase 1 Task 16 Step 4 — full suites against the 10⁹ server**

```bash
./target/release/tessera serve --bundle <path> &
uv run pytest conformance/tests/ -v
uv run pytest reference/tests/ -v -k "differential"   # 5 grants x 5 viewports; the oracle is slow by design
```

- [ ] **Step 2: Phase 1 Task 16 Step 5 — write `phase1-results.md`**

Every number from Tasks 14–15 against each plan-§5 exit criterion, pass/fail, plus every deviation taken. Work through the Phase 1 plan's own exit checklist item by item:

- p99 viewport latency < 10 ms server-side at 10⁹ with a real (w=10⁴) mask — **state the k it was measured at**, and state the k-sweep result beside it.
- Zero entity IDs observable in any wire payload or log — byte-scan green, and note that after contracts r6 this is a property of the format, and that the sweep now also covers the identity key.
- Signature-sorted entity allocation in effect — unchanged by this work; cite `entity_ids_follow_signature_order`.
- WAL ack contract + positional CRC rule + restart-replay deny survival.
- Differential oracle agreement on counts, geometry bytes, first-k — **and on the `tessera_id` column**, reproduced independently from the spec text.
- Placeholder sampler is first-k and commented as deliberately wrong — **and the storage order it reads is now `(morton, tessera_id)`**, so the first-k it returns is no longer ordered by permission signature above V ≈ 2×10⁶ *(2026-07-30 fold)*. Cite the chi-squared uniformity check alongside it: what makes the placeholder's output unbiased is the prefix's uniformity, and that is measured, not assumed.
- `tessera verify` passes; corrupted-byte red paths green.
- Layer checks green.

Add a section recording what this change did to the exit criteria themselves: the contracts r6 format, design r21's I10 mechanism change, the C6 revision, the C4 annotation, and the sidecar's read protocol.

- [ ] **Step 3: Tick the boxes and update the status**

Tick Task 16 Steps 4 and 5 in `docs/superpowers/plans/2026-07-28-phase1-walking-skeleton.md`, and add a one-line note under Task 16 that the exit measurement was taken against the contracts-r6 format, pointing at this plan.

Update `CLAUDE.md`'s Status section: Phase 1 complete, results at `docs/superpowers/plans/phase1-results.md`, contracts spec at r6, design at r21, and the reading that **the boundary identity is `tessera_id`, a keyed permutation of `(shard_id, entity_id)` under a per-deployment key that must be carried across rebuilds** (its home outside the bundle is the deployment config file, `--id-key-file`), while entity IDs never leave. Add the routing principle to the Non-negotiables list in one line, pointing at design §10.3.

Add two more one-liners, both of which a future contributor will otherwise get wrong:

- **The identity framing (Ruling A):** *all data has one stable global identifier — the caller's external ID when supplied, the `tessera_id` when not, and when derived it simply is the `tessera_id` at zero storage. The wire always carries the fixed-width `tessera_id`.* This is the sentence that stops the next reader treating them as two namespaces.
- **The external-ID sidecar is transitional (Ruling B):** *a placeholder for a future adopted per-point metadata store, the same slot as §8.3's vector sidecar; keep it minimal and its boundary narrow, and note that Appendix D rejects adoption for the access-control layer, not for a cold store off the request path.*

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/plans/phase1-results.md docs/superpowers/plans/2026-07-28-phase1-walking-skeleton.md CLAUDE.md
git commit -m "docs: Phase 1 exit record against the contracts-r6 identity format"
```

---

## Questions to the owner — ALL NINE RESOLVED

**Nothing here is open.** Three were closed by analysis or review before the owner saw them (Q1 resolved against the code, Q2 ruled by review round 2, Q4 retired by the reviewer's better construction), and the owner answered the remaining six on 2026-07-29 along with Rulings A and B. The entries are kept because a resolved question with its reasoning is worth more than a deleted one: each records what was actually decided and why, so the decision is not re-litigated by the next reader. **An executor needs no decision from the owner to proceed with any task in this plan.**

| | question | status |
|---|---|---|
| 1 | entity-ID uniqueness across partitions | **RESOLVED** against the code and design — prefix is the §13.3 shard, reserved at 0 |
| 2 | is `splitmix64` an acceptable round function | **RULED** by review round 2 — keep it, 8 rounds, conditional on three fixes, the third of which the 2026-07-30 fold satisfies differently (keyed `priority`, not forbidden `priority`). One **new verification item**: a chi-squared check that `high16` is uniform under structured inputs (Task 7 Step 5a) |
| 3 | locator or a duplicate column | **ANSWERED** — locator; build for the real-world externally-provided ID |
| 4 | `/v1/items` with no cached projection | **RETIRED** — the visibility test is in entity space |
| 5 | may Task 14 delete `/tmp/tessera-1e9` | **ANSWERED — yes**, conditional on Task 2's baseline, enforced at Task 14 Step 2a |
| 6 | where the identity key lives outside the bundle | **ANSWERED** — a per-deployment config file, `--id-key-file`; not an env var |
| 7 | the 256-byte external-ID cap | **ANSWERED** — tighten to 64 bytes |
| 8 | epoch enforced or advertised | **ANSWERED** — advertised on `/meta`, optional on `/v1/items`, as assumed |
| 9 | ordering against the drawn-mark plan's Task 7 | **CONFIRMED as written** |

1. ~~**Are entity IDs globally unique across §12 partitions?**~~ **RESOLVED** against the code and the design — see "Which prefix" above. Entity IDs are bundle-global (**one** `Manifest::entity_id_high_water`, **one** `Allocator`); §12.4 makes a partition's identity a canonical content hash rather than a dense small integer, so it could not be a `u32` prefix in any case; and §12 partitions exist in the format today while §13.3 shards do not. **The bijection's prefix is the row-range shard, reserved and valued 0.** What survives is narrower and blocks nothing: *if* a future multi-shard deployment allocates entity IDs per shard, the reserved prefix becomes load-bearing; if it keeps allocating globally, the four bytes buy only the option. The encoding is identical either way. **No decision needed from you now** — Task 4 Step 7 records the resolution in §16.

2. ~~**Is `splitmix64` an acceptable round function?**~~ **RULED** by review round 2: **keep `splitmix64` at 8 rounds.** The construction was independently verified invertible (balanced 32/32 split ⇒ a permutation of 2⁶⁴ for any round function; round-count parity irrelevant; the output packing is itself a bijection), and the ruling is conditional on three fixes this revision applies: the allocator cap at `u32::MAX`, the explicit statement that the control plane is outside the defended set, and closing the unkeyed-`priority` channel — **which since the 2026-07-30 fold is closed by *keying* `priority` rather than by forbidding it. The condition is satisfied differently, not dropped**, and the ruling still rests on something real. Second choice was keyed SipHash-1-3 at ~80–120 s added to a ninety-minute build (~2%). **No decision needed from you** unless you want the stronger round function anyway — say so before Task 5 if you do; nothing else in the plan changes if you do.

3. ~~**The drill-down structure: locator (3.7 GiB) or a second copy (11.2 GiB)?**~~ **ANSWERED (2026-07-29): the locator.** The owner's reason is *"build around the more likely real-world scenario of an externally provided ID"* — and that reason is **stronger than the arithmetic the plan first offered**, which is why it is worth recording rather than just ticking. The plan had argued 7.5 GiB on the synthetic corpus's 8-byte keys, i.e. a constant. Under the owner's framing it is not a constant: the duplicate column costs `mean_key_len + 4` bytes per row and **scales with the caller's key length**, while the locator is a flat 4 B/row whatever the keys look like. At 16-byte UUIDs the margin is ~15 GiB, at 36-character UUID strings ~34 GiB, at the 64-byte cap ~60 GiB — and the duplicate column's cost is, exactly, *the whole external-ID store a second time*. **The indirection is the cheap half of the trade.** See "Why a locator rather than a second copy" above for the table. **No decision outstanding**; Task 7 Step 5 writes `ext-locator.u32`, singular.

4. ~~**`/v1/items` on a session with no cached row projection returns `None` for every ID.**~~ **RETIRED.** The question existed only because the visibility test was formulated in row space. It is an **entity-space** question — `fragment.contains(entity)` under the overlay's `deleted > suppressed > evaluate_terms` precedence and the ingest buffer, exactly as `compose` resolves it per entity (verified against `crates/tessera-engine/src/compose.rs:145–198`) — which is O(1), needs no `RowProjection`, and does **identical work for an unknown ID and an invisible one**. That closes C-5 outright rather than narrowing it, removes the "404s everything until a viewport has been drawn" behaviour that made this question painful, and dissolves the pin/cache-key entanglement with it. **No decision needed from you.**

5. ~~**May Task 14 delete `/tmp/tessera-1e9`?**~~ **ANSWERED (2026-07-29): YES — conditional on Task 2 having captured the current-format baseline first.** The condition is the whole ruling: deletion is irreversible and the old-format numbers can never be re-measured, so the authorisation only holds once deletion costs nothing but the ability to re-run. **The plan now enforces that ordering rather than advising it** — Task 14 Step 2a is a hard precondition check (the baseline JSON and memo exist, are *committed*, carry all four arms with their per-arm numbers, carry Task 2 Step 3a's component `du`, and carry an explicit verdict) and refuses to delete if any of it fails. Task 2 Step 2's JSON schema is now a contract with that gate. **One judgement the gate cannot make and the executor must:** if Task 2's verdict was **REFUTED**, Q5 does not authorise deletion — the ruling was given on a live premise, and a refutation returns the decision to the owner. Fallbacks (b) 2.5 × 10⁸ alongside and (c) more storage remain on the record for exactly that case. Q5 authorises deleting `/tmp/tessera-1e9` and **nothing else on the disk**.

6. ~~**Where does the identity key live outside the bundle?**~~ **ANSWERED (2026-07-29): a per-deployment configuration file, `--id-key-file <path>` — NOT an environment variable, and not "the owner's notes".** The owner's framing: *"we'll ultimately need something similar to elastic index configuration."* So the key is the first tenant of a deployment config file that will later carry more than the key, and **this plan does not design that file** — it adds one flag, one minimal `[identity] key = …` TOML shape, and a recorded direction so the file is extended rather than replaced. The load-bearing detail, because it is what keeps Critical N-1 intact: **there is no default search path and no environment variable.** `--id-key-file` counts as an explicit key decision *only because the operator typed the path*; a file the binary finds on its own is not a human deciding, and an implementation that adds a fallback location defeats N-1 without touching N-1's code. `--mint-id-key` prints and does not write. See "The deployment config file" above; Task 7 Step 3 implements it, Task 14 Step 1 tests the N-1 guard with a key file planted in the working directory.

7. ~~**The 256-byte external-ID cap: tighten it, or state the cost?**~~ **ANSWERED (2026-07-29): tighten it to 64 bytes.** The plan had recommended the softer option (b) — leave the cap, document the scaling — on the grounds that the namespace is the caller's. The owner chose (a), and Task 4 Step 10 now does **both**, because they are complementary: §1's cap becomes **64 bytes**, *and* §1 and §2.4 state that sidecar disk scales linearly with key length. 64 bytes covers a 36-character UUID string, a ULID, an ObjectId and ordinary business keys; the old cap was a number nothing in the format was sized for, at over 250 GB of extents at 10⁹. The contract is open exactly once, and tightening after a caller depends on longer keys is breaking. **Over-length is a typed error at ingest and at build, never a truncation** — a truncated key is a different key, and two keys sharing a 64-byte prefix would collide into one entity.

8. ~~**Does the identity epoch need to be *enforced*, or only advertised?**~~ **ANSWERED (2026-07-29): advertised, not required — confirming what this plan already assumed.** `/meta` reports `identity_epoch`; `/v1/items` accepts `epoch` optionally and answers `409` on mismatch. The owner's reasoning, recorded because it is what a future reader would otherwise re-derive badly: **the durable identifier is the `external_id`** (Ruling A, owner ruling 10), so a consumer following the contract has no stale `tessera_id` in its database to present — the mandatory epoch would guard a class of caller the contract already tells not to exist; and **key rotation and repartitioning are deliberate breaking changes, not scheduled hygiene**, so the epoch fires approximately never and requiring it would put friction on every drill-down forever to guard a once-in-a-deployment event. A required field is also a required round trip, making `/meta` a precondition of the first drill-down. The accepted cost — a client that ignores the epoch may, after a repartitioning, get a `200` describing a different item — is the caller's choice, C6's and C12's shape, and is now stated in contracts §2.2 next to the mechanism so "optional" does not read as an oversight.

9. ~~**Ordering against the drawn-mark plan's Task 7.**~~ **CONFIRMED as written (2026-07-29).** `DEFAULT_MAX_K`'s *value* is not committed until this plan's Task 15 has run: two of its three inputs move (the handle-table ceiling P3 vanishes under D5; the transport ceiling changes at 8 B/point) and the current numbers were measured on a box that was swapping. **The calibration *method* is unaffected** — only the numbers it consumes — so drawn-mark Tasks 3–6 proceed in parallel as the "Coordination" section describes, and only Task 7's committed value waits. The owner adds that `DEFAULT_MAX_K`'s value waits on re-measurement regardless; nothing about the method needs revisiting.

---

## Out of scope for this plan

- Streaming flush, compaction, and the sidecars' per-flush extents beyond writing extent 0 at build (Phase 2). Contracts §2.6's revised streamed-segment locator (a per-segment `permutation.bin`) is *specified* here and *implemented* in Phase 2.
- Phase 3's node table, which will re-add a row→node column to `columns.arrow` as an additive change when it acquires a reader, and Phase 3's node handles, for which `tessera-wire::handles` is retained.
- Entity-ID exhaustion (§16) beyond the allocator's `u32::MAX` refusal, and whether a future multi-shard deployment allocates entity IDs per shard (the narrowed residual of Open Question 1; partition uniqueness itself is resolved).
- Building a compressed or block-indexed external-ID store (brief Part 9): recorded as a conditional future option with the `pairs.parquet` precedent, not built. The **~16-byte mean-key-length threshold** at which it becomes worth doing is recorded (arithmetic section, Task 15's memo) so it is scheduled by evidence; Phase 1 builds neither it nor the replacement.
- **Choosing or designing the replacement metadata store** (Ruling B). Phase 1 builds the placeholder and, more importantly, the *narrow boundary* that makes the swap a one-file change. Which technology, when, and how it satisfies the fail-closed and off-the-request-path conditions are all later questions.
- **The wider deployment configuration file** (Q6). This plan adds `--id-key-file` and a minimal `[identity]` section and records the owner's direction — *"something similar to elastic index configuration"* — and specifies nothing else about that file: no schema, no precedence rules beyond the key-source refusal, no secret management, and no entry in the contracts spec beyond naming the flag.
- Repartitioning and resharding themselves (§12.5, §13.3). This plan specifies the identity **epoch** that makes their identifier churn detectable; it does not implement either.
- Closing C4 (response timing) for the **viewport** path. `/v1/items`' identical 404 removes the status, code and message channels and its timing channel is closed structurally; the viewport correlation remains and C4 stays `Open`.
- Changing the placeholder first-k sampler (I7), the mask-build path, or anything in `tessera-authz`. **The 2026-07-30 priority fold does not change this**, and the distinction matters: it changes the **storage order the placeholder sampler reads in**, which is where the signature-ordering defect lived, and not a line of the sampler. Building the real selection comparator — prefix scan, fall-through to the full `tessera_id`, and the candidate-list path — remains out of scope; this plan lands it as **spec text** in Task 4 Step 7a (design §7.2) and implements none of it.
- Any change to §11.1's signature-sorted assignment, which is permanent under I9.
- **Measuring the defect the priority fold repairs.** The count of (tile, principal) pairs with V > 2×10⁶ over the existing 10⁹ k-sweep is being run concurrently by another agent. It sizes the defect; it does not establish it, and this plan does not wait on it. It does read the **existing** bundle, so it must be complete before Task 14 Step 2 deletes it.

## Risks the executor should watch

- **`build_equivalence.rs` byte-equality is the tripwire for Task 7** — but it got *simpler*, not harder. The identity is a pure function of `(key, shard, entity)`, so there is no seed to thread and no RNG to reconcile. If the two paths diverge, look at the two schema literals (`write.rs:61`, `write.rs:236`), the validator (`read.rs:541`), and the sort tiebreak — not at the identity.
- **The sort order is contract, and since 2026-07-30 it IS the identity.** `sort_batch` orders by **`(morton, tessera_id)`** with no further tiebreak, and the oracle re-derives that order from build inputs — which now include the key. *(This risk previously said the opposite: "do not let the tiebreak drift onto `tessera_id`". That instruction is **wrong** after the owner's priority-as-prefix decision and is recorded here only so an executor holding a stale copy recognises which way round the plan now goes.)* What to actually watch: **the entity ID must not appear at any position in the sort**, since it is signature-sorted under I9 and that is the defect being fixed; `priority` must be derived at exactly one place, `TesseraId::priority()`, or the column and the sort key drift apart; and the identity must be computed **before** the tiler, not written at the row afterwards.
- **A digest is not a sortedness check** (Critical C-1). If an executor finds the sortedness scan "redundant with the digest", they have rediscovered the bug: the digest proves the file is the one MANIFEST named; sortedness proves the binary search returns the right answer, and a build bug emitting an out-of-order extent produces a correctly-digested file. A mis-resolved ID denies the wrong entity and leaves the intended target visible.
- **Laziness must be per extent** (Critical C-6). One lock over the whole family turns the first drill-down into a permanent 11 GiB residency increase, silently undoing the change this plan exists to make.
- **A `None` that should be an error.** Every sidecar failure mode (missing file, digest mismatch, schema mismatch, out-of-order extent) must be a typed error. A corrupt sidecar returning `None` reads as "unknown external ID" and would let `/control/changes` silently fail to suppress — fail-open, and precisely the shape the lifecycle design warns about twice. The same applies to Task 2's temporary feature (Important I-2), and to the item path: **`.ok().flatten()` is forbidden anywhere on it** (Critical N-3), because it converts every typed error Task 8 exists to produce into a `200` with `external_id: null`.
- **Two transcriptions of the precedence rule.** `visible_to` and `compose` must share one `verdict` function. A second copy of `deleted > suppressed > evaluate_terms > buffered` that drifts is how a suppression stops suppressing on the drill-down path while still applying on the viewport — the exact fail-open the lifecycle design warns about, arriving through a refactor rather than a design error. Task 9's `visible_to_agrees_with_compose_over_every_precedence_case` is the guard; do not delete it as redundant.
- **`--mint-id-key` becoming implicit again — and now `--id-key-file` too.** N-1's refusal is worth nothing if `scripts/build_full.sh`, a Makefile, a CI job or a wrapper supplies any of the four key sources on the operator's behalf. The refusal exists so a *human* decides. **`--id-key-file` is the new and more tempting way to break this:** adding a default search path, an `$TESSERA_CONFIG` fallback, or "look for `./tessera.toml`" would defeat N-1 completely while touching none of N-1's code, and it would look like a usability improvement in review. The flag counts as an explicit decision **only because the operator typed the path.** Task 7's `the_key_file_is_read_validated_and_never_found_by_default` and Task 14 Step 1's planted-file arm are the guards; check both after any change to configuration handling or the build scripts.
- **The sidecar's storage leaking past its boundary** *(Ruling B)*. The sidecar is a placeholder and its value is that it can be taken out. The failure mode is gradual and looks harmless: an extent descriptor threaded into `Engine` for a test, a file path in an error message the server formats, an `open_extents()` call that becomes load-bearing rather than diagnostic. The check is one sentence — *replacing the storage must be a change to `sidecar.rs` plus its constructor call* — and it should be asked at every review that touches `tessera-engine` or `tessera-server`, because no compiler enforces it.
- **Manufacturing an external ID for an item that has none** *(Ruling A)*. `0xFFFFFFFF` in the locator is the **ordinary** case — an item whose identity is its `tessera_id`, not an item with missing data. A well-meaning "fill in a synthetic key so every item has one" would invent identity, double the sidecar for no benefit, and make Ruling A false. The build's existing `source_id`-derived keys are scaffolding for the synthetic corpus and must not be generalised into a rule.
- **Deleting `/tmp/tessera-1e9` before the baseline is safely captured.** Q5 authorises the deletion; it authorises it **conditionally**, and the condition is enforced at Task 14 Step 2a rather than remembered. Do not weaken those checks because they are "obviously satisfied" — an uncommitted results JSON is one `git checkout` from gone, and there is no second chance at an old-format measurement. A **REFUTED** Task 2 verdict does not carry the authorisation at all.
- **The allocator cap being "tidied" as an unnecessary bound.** `allocate` returning a `Result` looks like ceremony on a `lo + n`. It is the precondition "collision-free by construction" rests on: past `u32::MAX` two entities share a `tessera_id` and a suppression lands on the wrong item.
- **Reinstating the retired `priority` guards by reflex** *(2026-07-30 fold; this risk replaces "`priority` reaching the wire", which was its inverse)*. An executor or reviewer working from the design corpus, from Task 3's memo before Task 4 Step 0 amends it, or from a stale copy of this plan will find `priority` described as an unkeyed `splitmix64` of the entity ID and forbidden on the viewer plane, and will "restore" the layer-check grep and the byte-scanner sweep. **Both are retired, by argument, and the argument is at "The routing principle" above.** The rule that survives is narrower and worth keeping straight: a hot column may cross the boundary only if it is **independent of the entity ID or keyed under the deployment key** — so a future *unkeyed* derivative of the entity ID does need a guard, and `priority` does not.
- **A second derivation of `priority`.** `(id >> 48) as u16` is four characters of arithmetic and will be inlined wherever it is needed. Two sites is how the sort prefix and the written column drift apart, and the drift is silent: the bundle still verifies, the oracle still agrees on identities, and only the *sample* is wrong. One definition, `TesseraId::priority()`, called by both writers and by the streaming comparator.
- **Deriving row order without the key.** Row order was key-independent until 2026-07-30 and is not any more. An oracle, test or tool that re-derives row order from `(morton, priority, entity_id)`, or that compares row indices between two bundles built under different keys, is wrong for r6 and will fail in a way that looks like a build bug.
- **Branch-dependent observability on `/v1/items`.** The identical-404 rule is defeated by one `tracing::debug!` inside one arm, or by two metric counters. Task 10 Step 4 checks for both; re-check after any later edit to that handler.
- **The identity key must never be logged.** `IdentityKey`'s `Debug` is redacted for this reason; a `{:?}` on a struct that contains it must not leak it either. Task 10's layer check greps for it in the wire and viewer layers.
- **Do not "fix" the oracle to agree.** If the differential suite fails after Task 12, the engine is the suspect until proven otherwise. And do not write the Python bijection by reading the Rust one — its independence is the only thing that makes agreement evidence.
