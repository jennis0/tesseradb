# Divergence register — corpus versus system

**Status:** Working document, 2026-08-01. Deleted when the rewrite closes; its rulings move to
`../decisions/` and its remaining work to GitHub issues.

Produced by independent audits of each corpus document against the code, the conformance suite,
the probe records and the memos. Each row is classified:

- **metadata** — stale pointer, wrong quoted figure. Correct without a ruling.
- **framing** — forward-looking prose about completed work. Correct without a ruling.
- **substantive** — the document and the code disagree. **Needs an owner ruling.**
- **absent** — the code does something material the corpus never describes. **Needs a ruling** on
  whether it enters the specification and where.

Nothing here has been corrected. That is deliberate: the rewrite applies rulings, it does not
make them.

## Owner rulings, 2026-08-01

1. **Specified versus implemented is marked per claim**, at the point the claim is made — not in a
   preamble the reader has forgotten by §5 — plus a summary table in the corpus index.
2. **I10 is weakened to match the construction** (S6). It states what is actually defended: a
   blinding permutation preventing viewer-plane correlation and enumeration, explicitly not a
   cryptographic guarantee and explicitly not a defence against a bundle-holder. The threat model
   is promoted from `identity.rs` into the specification. No code changes.
3. **Scope of this rewrite:** every metadata and framing row, plus the substantive rows where a
   wrong document misleads about a guarantee — S1, S6, S7, S10, S12, S18, S19, S20, S21, S22, S23
   and the retirement rules. The remainder becomes GitHub issues against the epics that will build
   the machinery.

---

## The systemic finding

**Both audits independently reported the same thing: the corpus is written in the present tense
about machinery that does not exist.**

The documents describe the intended end state — flush, compaction, merge, the router/worker
split, partitions, candidate lists, the tile table. The code is at stage 2.1. In most places
this is harmless ambition. In three places it is not, because the present tense reads as an
assurance about a security property:

1. **Of the three retirement rules, one is built.** Suppression retires only on unsuppress, as
   specified. Deletion's epoch ledger does not exist — `retirement_floor`, `epoch_counts` and
   `min_live_epoch` have zero occurrences in `crates/`; `deleted` is a terminal `bool`. The
   evaluate-entry fold does not exist because compaction does not. Both are currently *safe*, by
   never retiring at all, which is fail-closed but is not the mechanism the document describes.
   §3.2's retirement floor — the guard the review record calls "the one that could have
   reintroduced a fail-open path" — has no code.
2. **The conformance suite is the stated deliverable, and 3 of 13 invariants are covered as
   designed** (I1, I7, I10). Two more are covered in substance but in Rust rather than the suite
   (I9, I11). Six are not covered at all (I3, I4, I5, I6, I8, I12), four of those because there
   is nothing yet to test. Zero of the eight scripted interleavings exist. No CI exists.
3. **I13 names two different properties.** All 35 `I13` annotations in the code concern
   single-flight panic and cancellation. The invariant's partition half — "a partition not
   consulted fails closed" — has one hardcoded partition, no required-set gate, and no test. A
   reviewer grepping `I13` concludes it is covered.

The ruling needed is not row-by-row. It is: **how does the corpus distinguish specified from
implemented?** Everything else follows from it.

---

## Substantive — needs a ruling

| # | Where | The corpus says | The system does | At stake |
|---|---|---|---|---|
| S1 | §2.6.7, §7.2, §10.4, §11.3, §12.3, §14, App. A, **C4** | Two selection routes: direct evaluation, or "the node's precomputed **candidate list** where coverage is high, the two crossing at a few percent" | **No candidate-list route exists.** Declined by owner ruling (Phase 2 roadmap, ruling 1); `select.rs:31-40` records the refusal and `check-layers.sh` fails if the marker is deleted; no build stage emits them | The two-route framing licenses building the declined route, whose failure mode blanks the sparsest principals' maps — I7 inverted. **C4's stated leak source describes work that never happens** |
| S2 | §5.2, §10.3, §10.4, §11.3 | A precomputed **tile table** maps tile prefixes to rank ranges; listed as a stored file; rewritten by compaction | No such artifact. Ranges are derived per request by binary search over `morton.u32` (`store/src/read.rs:1092`) | Sizing, compaction cost and the underlay's cost argument all rest on a structure that does not exist |
| S3 | §2.6 steps 2→4, §10.4 | Compose in entity space, then project into row space per request | Inverted: projected **once per session** into a cached `RowProjection`; I1 evaluated as row-space diffs with `∩ base` / `∖ base` clamps (`compose.rs:1-16`) | The clamps are what stop a deny on an entity the fragment never held driving a tile count negative. Restating the doc's order omits where that property lives |
| S4 | §2.3, §2.6.5 | "**Eviction must be transparent.** The auth data is retained alongside the mask" | No auth data is retained. Transparency comes from a live `Arc<FrozenFragment>` plus a persistent on-disk fragment cache | Retaining credentials beside a mask is a security decision with a cost. The code deliberately does not, which is **better** than specified and unrecorded |
| S5 | §10.6 | Failure semantics admit errors only for authorisation or composition failure | An authorised, well-formed request is also refused for load or shape: 429 backpressure, `TooManyTiles`, three `UnderlayRefused` arms, `MultiSegmentSlice`, `Cancelled`→500 | Availability semantics are part of the service contract and part of C14's timing shape |
| **S6** | **§4 I10, §10.6, C17** | `tessera_id` is "a keyed permutation… **invertible only inside the trust boundary**" | An 8-round Feistel whose round function is **splitmix64, a non-cryptographic mixer**. `identity.rs:11-15` calls it "a keyed **blinding permutation**, not encryption"; the key is not secret against a bundle-holder | **I10 as written claims more than the construction supports.** The qualification exists in a source file and a memo, and nowhere in the specification |
| S7 | §4 I13 | "A partition not consulted fails closed" | One hardcoded partition; no required-set gate, no reachability computation, no cross-partition containment. The 35 `I13` code annotations are all about single-flight fail-closed | One invariant number names two properties; the partition half has no implementation and no test |
| S8 | §6.1, App. E | The label half's oracle "already exists — **`accumulo-access`**" | Zero occurrences of `accumulo`, `DNF`, `k-of-N` anywhere. No CI, no JVM. The only plugin is `Passthrough`, for which I5 is trivially true and untestable | The document's self-declared "single largest unverifiable dependency" is presented as mitigated. It is not |
| S9 | §6.3, §15 | Postings are built from a pre-exploded pair relation joined as a **semi-join** — "a **requirement** rather than an optimisation" | No semi-join. Spilled band files, cursor scatter, parallel Roaring encode. `pairs.parquet` is optional and off by default | Pins a design the implementation left behind; the actual requirement goes unstated |
| **S10** | **§10.4, §2.6, §13.2, App. A, §7.3** | "**Expected latency**, warm: low single-digit milliseconds per viewport"; "cost scales with screen area rather than corpus size" | Measured at 10⁹: **135–164 ms** p50, selection 83–89% of it, ≈4–4.5 ns per visible row. Cost correlates 0.83 with Σvisible and ~0.00 with points returned | **The central performance claim is false at 10⁹ on the implemented route.** Appendix A's budget and §7.3's 10 ms gate are written against the wrong cost model |
| S11 | §2.3, §12 (whole) | Tokens carry a reachable partition set computed at authorisation | §12 entirely unimplemented | §12.2's gate is the enforcement point for I13 and for the compartmented-MAC lineage the design claims |
| S12 | lifecycle §2.1 | "**A pin is an `Arc<Generation>`.** Superseded generations sit on a drain list" | Deliberately not: a slimmed `DrainEntry`, because holding a whole generation retains the superseded overlay and buffer — which is the R-open failure (`pins.rs:135`) | **The document's own sentence describes the fail-open the code refuses** |
| S13 | lifecycle §4 | WAL record set includes `Flush{n, wal_pos}` | Three variants, no `Flush`. Recovery replays the whole log | |
| S14 | lifecycle §1.3 | "One lifecycle thread per partition owns all mutation decisions" | Two publishers: `Engine::publish_geometry` swaps from any caller, and its own doc names the residual race and says "nothing in this file closes it" | The document's central simplicity claim is the property most at risk in the built system |
| S15 | lifecycle §5.1 | "Flush is what makes ingested items visible at all" | Ingested items are visible via `IngestBuffer` and composition rule 4. Flush does not exist; `flush_max_items` is asserted inert by a test | True of the end state, false of the current one |
| S16 | conformance §5 | Eight scripted interleavings, a `conformance` cargo feature, eight named pause points, three commands | None of it. A *different* pause mechanism exists in `lifecycle/src/faults.rs` with different vocabulary and a different home | A second pause mechanism will be built beside the existing one |
| S17 | conformance §5 | Crash realism requires **truncate-to-`fsync_offset()`**; "SIGKILL loses nothing — an engine that acked before fsync would pass" | `test_restart_replay.py` uses SIGKILL only — precisely the variant the design calls insufficient | The suite contains the test its own design pre-emptively rejects |
| S18 | conformance §4.2 | Canonicalised byte-comparison, with a **comparator positive control** proving it can fail | Decoded comparison, no canonicalisation, **no third fixture state** | A pass-only test with no proof it can fail — the failure mode the design names |

| **S19** | **SA §4.5** | "**All identities are per-session `u32` handles** (I10, byte-scan tested)", issued as a router keyed permutation | Retired by design **r21** and contracts §0.3 deviation 8. The wire carries `points_tessera_ids: &[u64]`; `wire/tests/wire.rs:202` names "the r6/r21 boundary change". `handles.rs` survives `#[allow(dead_code)]` for Phase 3 node handles only | **The only place in the corpus still asserting the retired model — and it asserts it as the I10 mechanism.** A rewrite carrying §4.5 forward restates a superseded security mechanism as current |
| **S20** | contracts §2.1 | "`SEGMENTS-<n>.json` … `n` zero-padded decimal" | The writer emits `SEGMENTS-0.json`, unpadded. The reader parses `SEGMENTS-01.json` → `n = 1`, then reconstructs `SEGMENTS-1.json` — a different, absent file. `read.rs:363` records it as a known fail-open residual | **A live spec-versus-code contradiction on the fail-closed replica path.** A spec-conforming writer produces manifests this reader silently steps past. Reconcile in one direction before either document restates it |
| S21 | contracts §2.3 | "`readyz` fails if the newest verifying `n` is older than the configured lag bound — unbounded step-down would let a badly synced replica serve long-deleted items as live" | Not implemented. `read.rs:371` — "the bound on all three is time, and that bound does not exist yet"; `health.rs:19` tables it as **not enforced**; `stepped_down()` has zero non-test callers | A contract states a fail-closed gate in the present tense. Step-down itself *is* built; only its time bound is missing — and that bound is the gate that would have caught S20 |
| S22 | contracts §2.2 | `identity.key` is "exactly 32 lowercase hex"; degenerate keys "refused at both write and read" | The check lives in `IdentityKey::from_hex`, reached only from `tessera_build::verify` and `Engine::open`. A bundle with an uppercase or degenerate key **opens fine through `tessera-store::open_bundle`** — the reader §2.3's protocol actually names | "Readers reject" is true of the engine and false of the store |
| S23 | SA §3 | "Dependency rules **enforced in CI** (a `cargo-deny`-style layer check)" | There is no CI. `check-layers.sh` runs from an opt-in `pre-commit` hook, and is skipped entirely in a worktree with no `.claude/track` marker | The claim that the layering is mechanically enforced is false; enforcement is advisory unless a developer ran the installer |
| S24 | SA §4.5 | (implied) the per-session handle table is live | `state.rs:23` still allocates `handles: Mutex<HandleTable>` per session and **nothing ever reads or writes it**, while its own comment claims it "grows with the session's own drill-downs" | A dead allocation carrying a false rationale, contradicted by its own crate's documentation |
| S25 | contracts §3.1, §3.2 | An `x-tessera-api` header "everywhere"; Arrow bodies as `application/vnd.apache.arrow.stream`; `selection` is five keys | The header has zero implementation on either side; the server sends `application/octet-stream`; `selection` emits **six** keys, and `clients/ts` already consumes the sixth | Three stated wire requirements a conforming client would fail on. The sixth key is the r9 `max_k` defect recurring exactly |
| S26 | SA §6.1 | An in-memory linear build, implied throughout | Eleven-stage streaming build with external spill and receipts; the linear build was **OOM-killed at 10⁹ on a 47 GiB box** and survives only as the byte-identity oracle | The build's memory-bounded character is what makes 10⁹ reachable, and it is the largest undocumented mechanism in the system |
| S27 | SA §3, §2.2, §4.3 | Crates `tessera-labels` and `tessera-filter`; a `python/` SDK; a router/worker process split (`tessera --partition`); a wasmtime plugin host | None exist. Twelve crates, none of them those two; no `python/`; three CLI subcommands and no spawning; no `wasmtime` dependency, and `builtin:access-expressions` is **refused at startup** | SA's cache-ownership table and read-path map route steps to crates that do not exist. §2.2, §2.3, §4.5, §6.6 and decisions D7/D11/D12/D16 are written in the present tense about an unbuilt process model |
| S28 | SA §7 | The `tessera.toml` example | **Would not parse.** Every section is `deny_unknown_fields`; five documented keys are wrong (`flush_max_age`, `wal_retention`, `token_max_lifetime` as a string, a `[merge]` section, `plugin.module`) | The config philosophy is correct and verified live; the example is not. Regenerate it from the code and keep the philosophy sentence |
| S29 | SA §4.1, §4.2 | The bundle tree; ten `/control/*` verbs; a *build* credential tier; Prometheus metrics on an admin-trusted port | The tree is superseded by four §0.3 deviations. Three control verbs are mounted, not ten — **absent, not stubbed**, including `/control/allocate-ids`, which SA §6.6 makes load-bearing. One credential layer, not two. Zero hits for `prometheus`, `/metrics` or `metrics_addr` | SA §9's entire named-risk metrics list has no emitter; its substitute, `/control/status`, returns a large surface neither document describes |
| S30 | SA §4.1, contracts §0.3 | SA: "§0.3 has since grown to **nine** deviations" | Eleven. Deviations 2, 5, 6, 8 have design companions applied; **1, 3, 4, 7, 9 target SA §4.1, which is unamended** — its tree still prints `morton.u64`, `pairs.arrow`, `tiles.bin`, `candidates.bin` | All eleven remain live. The count is the stale artefact, not the list |

## Absent — needs a ruling on whether it enters the specification

| # | What exists and is undescribed | Why it matters |
|---|---|---|
| A1 | The commit window's sizing analysis **refutes §11.1's optimism**: `run ≈ B×p` is a ceiling, not a forecast; and because `p·B < 2¹⁶` at every feasible window size, group commit collects the posting-storage win and **none of the container-count win** | §11.1 reads as though group commit recovers the 8.9–36.7× prize. The union-cost half is unreachable by this lever — and that half is what §13.3's sharding lean rests on |
| A2 | The build is **streaming with external spill** (the linear build was OOM-killed at 10⁹ on 47 GiB). **Batch size is identity-bearing under I9** — `BATCH_GRID = 1<<24` sets the signature-sort scope | A permanent, identity-bearing build parameter under I9 appears nowhere in the specification. A rebuild at a different batch size forks identities |
| A3 | A persistent on-disk frozen-fragment cache: digest-verified before an `unsafe` view, fsync-before-rename, owner-only permissions, survives restarts | A disclosure-relevant artifact persisted outside the bundle, absent from §8.5's cache table and from Appendix C |
| A4 | Single-flight caching with **non-blocking waiters** — a concurrent arrival gets `ProjectionBuilding`→429, a client-visible outcome produced by a caching decision. Duplicated in two crates because the layer graph forbids reuse | lifecycle §7 describes it in one clause implying waiters block. The duplication is the kind of thing a reviewer deletes as redundancy |
| A5 | A deny lane that can never be refused for load: drained to empty before ingest, unbounded in memory, "a sustained deny flood starves ingest completely". Measured deny ack 165 ms at 1 M buffered, dominated by an `O(buffered)` clone, not fsync | §3's write-latency reasoning and C14's timing shape depend on scheduling the corpus does not model |
| A6 | Per-tile **`served`** count on the wire, with an argued no-op disclosure justification and a "do not cite as precedent" clause | A new wire quantity whose leak argument lives only in a doc comment, with no Appendix C row |
| A7 | Tile-loop parallelism with three calibrated constants and a **response byte-equality guarantee** across `compute_threads` | Byte-equality under parallelism is a conformance property with no home in the specification |
| A8 | **B9 three-tier adaptive decode**, gated at 95% density | C19 cites the mechanism; the corpus never defines it |
| A9 | Identity-key operations: four key sources, rotation refused without `--rotate-id-key`, epoch bump, `StaleIdentityEpoch`→409 | r21 records that a re-key reorders tied rows and invalidates identifiers; the operational contract that follows is undocumented |
| A10 | Fault injection (`lifecycle/src/faults.rs`), with a **fidelity rule** — an injected failure must be indistinguishable from a real one in variant and order | Pre-empts conformance §5 by three stages, with different vocabulary |
| A11 | `ExecutorPosture` — a four-state monotone readiness signal feeding `/readyz`, where **`WalPoisoned` keeps the executor alive and still applying denies** | A deliberate choice between two fail-closed answers, and the operator-visible face of lifecycle §4 |
| A12 | Pin bounds `DRAIN_DEPTH_MAX = 4` / `DRAIN_DEPTH_ALARM = 1`, with a sizing obligation `pin_ttl_secs < DRAIN_DEPTH_MAX × publication_period`; and the per-session pin cap is "politeness, **not** the page-cache defence" — bypassable by session rotation | Two bounds and a defeated defence, none in the corpus |

## Metadata and framing — correct without a ruling

| # | Where | Correction |
|---|---|---|
| M1 | §7.2, App. A | *K*<sub>max</sub> is **500**, not 128 — owner decision 2026-07-30. The document contradicts itself; line 275 already uses 500 |
| M2 | §7.2 | The selection window is **250**, not 64 |
| M3 | §16 | Run ratio "2.3–5.1" silently drops the measured **1.7** floor (`probes/results.md:236`) |
| M4 | C19 | Three quoted figures (75 µs, 2.7 ms, ~30%) appear in no probe record or memo |
| M5 | C4 annotation | "9.5–19.3 s" conflates a 9.5/10.7 s measurement with a *millisecond* k-sweep p50. The 19.3 traces to a line warning "the box began swapping — **do not quote**" |
| M6 | App. G, r24 | The open `max_k` finding was closed at contracts r9 |
| M7 | §7.2 annotation | `max_k = 5000` is 5× the shipping default, stated without saying it is off-default |
| M8 | §16 | "Token lifetime… default backstop **one hour**". There is no default — `token_max_lifetime` is a **required** key and its absence is a startup error |
| M9 | §7.5, §13.2 | Both cite documents now archived (`visualisation.md`, the drawn-mark budget spec) |
| M10 | whole corpus | **No memo in `docs/evidence/memos/` is cited anywhere**, including `2026-07-30-tessera-id-construction.md`, which the source treats as normative |
| M11 | four documents | Cite the design as r23; it is r24. Contracts cited as r8; it is r11. `prior-art-synthesis.md` cites r14 |
| F1 | §11.1 | "It ships in the Phase 1 allocator or not at all" — it shipped, along with group commit |
| F2 | §13.2 | "Until probes P1 and P2 report, this section claims nothing" — they have reported |
| F3 | §3 | "The system holds on the order of 10⁷ points today" — no production corpus exists |

---

## Load-bearing arguments the rewrite must not lose

Both audits were asked to name what must survive verbatim. Consolidated:

**On the three retirement rules — the derivation, not just the rules:**

1. Suppression is *"non-retirable while active, by construction: no fragment rebuild ever
   excludes a suppressed entity, so its invisibility rests on the overlay entry for as long as
   the suppression stands."* Without this, a reader sees three rules and one mechanism and
   unifies them.
2. *"r1 assigned every deny a retirement epoch; for suppressions that is fail-open."* The
   counterexample is what makes the rule non-negotiable.
3. The floor generalisation: *"a floor raised only on deletion retirements passes the deletion
   test and still fails open through a pre-fold fragment."* The only sentence explaining why
   deletion and evaluate share a floor while suppression touches neither.
4. §3.4's bidirectionality — a pre-fold fragment misreads *both* ways: a revoked term still
   present is fail-open, a granted term absent is wrong counts.
5. §3.1's "Reflected in postings?" column — yes / **never** / not until compaction. The
   three-way answer is the structural cause of the three rules. It must stay a table.
6. From `overlay.rs`: `delete → suppress → unsuppress` must not re-expose a deleted item, and
   three independent fields make that **structurally impossible rather than merely tested**.

**Elsewhere:**

- §7.2's descent arithmetic — work proportional to **1/coverage, not log(1/coverage)** — with the
  21 / 85 / 5,461-node figures. Reproduced almost verbatim in `select.rs` as the third of four
  reasons the candidate-list route was declined. The conclusion changed; this argument is *why*.
- The floor clause is the I7 guarantee and may not be removed as an optimisation. `k_min = 0` is
  refused at startup, not clamped.
- θ's anchor must be the composed total, counted in row space — and the differencing attack that
  makes it an I2 requirement.
- Overlay precedence `deleted > suppressed > evaluate_terms`, single-sourced: *"Two
  transcriptions of a precedence rule is how a suppression stops suppressing."*
- I11's two named failure modes, R-open and R-wrong — *"not stale-restrictive but simply wrong."*
- I10 in structural form: no request-path artifact stores an entity ID, so the gather cannot
  produce one.
- C4's structural closure of the `/v1/items` timing channel — identical work for an unknown and
  an invisible identifier.
- Pin reclaim ordering: *"verify-then-remove is the use-after-free the review caught;
  remove-then-verify is the fix, and it costs nothing."* Both halves — the second defeats the
  objection.
- The forbidden restart alternatives for pins. Both look like helpfulness; the code repeats them
  verbatim because they are what a maintainer reaches for.
- The positional CRC rule: *"truncate-at-first-bad-CRC applied mid-log would silently drop acked
  denies."*
- The disk-full triple: never a 200 without fsync, never a silent drop, **never a refusal that
  leaves the item visible** — the third is what a naive fail-closed implementation breaks.
- Conformance §4.3's retirement of the C17 decorrelation check: *"a conformance test that keeps
  checking a retired prohibition reports green while checking nothing real."*
- Appendix H's counting-versus-aggregation boundary — the sentence that keeps the query surface
  enumerable, which is what makes Appendix C exhaustible at all.
- Two arguments currently living only in code that belong in the corpus: a drain entry must not
  be an `Arc<Generation>`; and the `RETIRED` marker is safe to be absent **only** while nothing
  deletes a file.
