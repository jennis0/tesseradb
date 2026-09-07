# Ingest at any scale — design review

**Status:** review, 2026-09-07, requested by the owner ("all types of data must be creatable and extendable during ingest at any scale"). Findings and the rulings it needs; nothing here is decided. Evidence cites file and line on main at fa1e2dd2.

## Is the concern justified?

**Partly, and the part that remains is sharper than the pattern that prompted it.** The membership case the owner watched being declined is now closed on the wire: decision 0127's `PATCH /control/layers/{name}/artifacts` is built and the driver uses it (`control.rs:3597`; `ingest_cycle.py:1272-1281`), decision 0128's column route carries point-derived memberships with no publication-route cap at all, `--max-member-rows` is gone, and an artifact's membership therefore has no ceiling — only a pagination unit. What is *not* closed is everything on an artifact that is **not** its member list. `GrowBody` is `deny_unknown_fields` (`control.rs:3546-3551`), so content, generating sets, parents, shape and attachment travel **only** on `PUT`, whose 64 MiB body cap is the commit unit — and there is no smaller spelling for a single artifact. For the generating set this is not an oversight that a second PATCH verb could fix: **I8 forbids growing a generating set at either entry point** (architecture §4 I8; §12 lists "adding newly ingested items to a generating set" as a forbidden operation), so chunked growth is structurally unavailable and a Phase-3 label derived from more than ~4.4 million documents is **unexpressible at ingest by construction**. Beyond artifacts, three whole kinds are build-only with no runtime route at all: the attribute schema (a new column, family or width), declared vocabularies, and view groups. And membership-by-exclusion (`excluding`) exists at build (`tessera-build/src/config.rs:610`) and appears nowhere in `control.rs` or `tessera-lifecycle` — a membership form, not an acquisition detail, so it is a straight decision 0091 breach. The pattern the owner distrusts is real, but the residue is small and specific: it is not "large artifacts are declined", it is "everything except the member list is capped at one request, and three kinds cannot be declared at ingest at all".

The deeper design point: **decision 0091 does not say what the owner's requirement says.** 0091 is an *expressibility* rule — "anything a caller can express at one entry point they can express at the other" — and it explicitly carves out "cost and acquisition" and "scheduling and packing". Nowhere in the corpus is there a normative statement that every kind must be creatable and extendable at any scale, nor any stated maximum membership, publish body or per-artifact size. If the owner wants the requirement he stated, **it has to be written down**; today the caps are lawful under 0091 as read, because an over-cap artifact is not a refusal of something *sayable*, it is a request-shaping obligation on the publisher.

---

## Inventory

Create/extend as reachable **over the wire at ingest**. "Any scale" means no per-object ceiling, only a pagination unit or backpressure.

| Kind | Create at ingest | Extend at ingest | Scale | Binding cap | Enforced at |
|---|---|---|---|---|---|
| **Points (rows, coords, labels)** | Yes | Yes (more batches) | **Any scale** | 16 MiB body / 10,000 rows per batch; 1M-row buffer → 429 | `control.rs:238`, `control.rs:1736`, `control.rs:1938` |
| Row coordinates, in place | — | **No** — 409, edit = delete + re-ingest (0047) | n/a | — | `control.rs:1908` |
| Access label on an existing row | — | **No** — 409, delete + re-ingest | n/a | — | `control.rs:2256` |
| Entity joining a second view | Yes | Yes | Any scale | batch caps | `control.rs:1889` |
| **Scalar / keyword / render-only values** | Yes (new rows) | Yes | **Any scale** | batch caps | `control.rs:1145` |
| **Text values** | Yes | **No** — a flushed text cell refuses a second string | one value ≤ one 16 MiB batch | batch caps | contracts §3.4 |
| **Category values, `discovered`** | Yes, minted at window close | Yes | **Any scale** | batch caps | `control.rs:977` |
| **Category values, `declared`** | **No** — 422, declare-then-use | No | — | vocabulary is build-only | `control.rs:968` |
| **Group-scoped attribute values** | Yes, under plain names | Yes | Any scale | batch caps | `control.rs:1176` |
| **A new attribute column / family / width** | **Does not exist at ingest** | — | build-only; rebuild | — | `control.rs:1132` (422) |
| **Layers (all five kinds)** | Yes | n/a (declaration immutable; drop tombstones for ever) | small | axum's inherited 2 MiB | `control.rs:257` |
| **Views** | Yes, within a declared group | **No** — record immutable, drop + recreate | small | axum's 2 MiB | `control.rs:263` |
| **View groups** | **Does not exist at ingest** | — | build-only | — | 404 unknown group |
| **Artifacts, enumerated, via point column** (0128) | Yes, mints on `value_set = open` | Yes | **Any scale** — "never meets the publication route's cap" | batch caps only | `control.rs:679` |
| **Artifact membership, via publication** | Yes | **Yes — `PATCH`, any scale** | Any scale, ~4.4M members/request | 64 MiB per request | `control.rs:486`, `:3597` |
| **Artifact supplied content** | Yes, on `PUT` only | **No growth route** | ≤ 64 MiB with key+parents in one body | 64 MiB | `control.rs:3546` |
| **Generating set (`generated_from`)** | Yes, on `PUT` only | **No, and forbidden by I8** | **≤ 64 MiB, hard wall** | 64 MiB + I8 | `control.rs:3444`; architecture §4 I8 |
| **Parents / DAG edges** | Yes, on `PUT` and on the ingest column | On the ingest column yes; on `PATCH` **no** | per-artifact parent lists are small | 64 MiB | `control.rs:3080`, `:832` |
| **Attachment (`attached_to`)** | Yes, `PUT` only | No | small | 64 MiB | `control.rs:3068` |
| **Spatial shapes** | Yes, `PUT` only | No | ≤ 10⁶ vertices | `max_shape_vertices` | `control.rs:3148`, `:3265` |
| **Attribute-predicate membership** | Yes — declared, publishes nothing | Grows with the column | **Any scale** | none | engine `artifacts.rs:1492` |
| **Membership by exclusion (`excluding`)** | **Does not exist at ingest** | — | build-only | — | absent from `control.rs` |
| **Group-scoped layers' per-view artifact sets** | **Does not exist at ingest** | — | build-only (`view` column) | — | `PublishBody` carries no view key |
| Suppress / delete | Yes | Yes | ~10,000 ops/request, never shed | 2 MiB, hard-coded | `control.rs:471` |

**Build can, ingest cannot:** the attribute schema, declared vocabularies, view groups, plain ungrouped views, `excluding`, group-scoped layer artifacts, `fields` on `[layer.members]` (acquisition — lawful), external-id minting, identity-key construction. **Ingest can, build cannot:** mint a roster key at runtime (`tessera-build/src/config.rs:798-803`) — 0091 calls that equally unfinished.

---

## The caps, and what each binds

| Cap | Default | Configurable | Reason recorded | Binds |
|---|---|---|---|---|
| `ingest_max_batch_bytes` | 16 MiB | Yes, ceiling 64 MiB | Body is buffered whole before the handler; a term in the WAL startup relation and the 16 GiB resident relation | **Wire** — "streaming the upload would close this properly and is deliberately not attempted here" (`config.rs:1610`) |
| `INGEST_MAX_BATCH_BYTES_CEILING` | 64 MiB | No | Per-connection cost, because the *count* of authenticated connections is bounded by nothing in-process | **Wire** |
| `ingest_max_batch_rows` | 10,000 | Yes | Matched to the commit window so no client picks the sort scope | **Unit of work** |
| `commit_window_max_items` | 10,000 | Yes | Posting run length; heap; sort is n log n | Unit of work |
| `ingest_buffer_max_items` | 1,000,000 | Yes | Backpressure when flush falls behind — **429, not a refusal** | Unit of work |
| `ingest_queue_bound` / `ingest_admission` | 32 / 64 | Yes | Heap and blocking threads; 429 | Neither — concurrency |
| `INGEST_RESIDENT_CEILING_BYTES` | 16 GiB | No | Machine-scale startup refusal, explicitly "not a memory budget" | Configuration |
| **`PUBLISH_MAX_BODY_BYTES`** | **64 MiB** | **No — hard-coded, no config key, not on `/v1/meta`** | Buffered body; *"a publication is corpus-sized"*; sized on the measured MeSH descriptor (756,640 members, ~11 MB) | **Wire for members** (0127 made it a pagination unit); **unit of work for content, generating sets, parents, shapes** — the batch is the commit unit and ordinals are claimed contiguously |
| `CHANGES_MAX_BODY_BYTES` | 2 MiB | No | ~10,000 suppressions; a latency cost on a batch, never a refusal of a deny | Wire |
| `max_shape_vertices` | 10⁶ | Yes | The one input a caller can simplify (`ST_Simplify`) | Unit of work |
| `EXTERNAL_ID_MAX_LEN` | 64 B | No | Truncation would merge two callers' entities | Correctness |
| Layer / view declaration | 2 MiB inherited | No | "three orders above the largest declaration anyone can write" | Wire |

The distinction that matters: **only two caps genuinely bind the unit of work.** The publication's atomicity requirement (contiguous ordinals from the level's cursor) forces one artifact's *identity-bearing* payload — key, content, generating set, parents, shape — into one commit. Everything else on the list bounds a buffered body or a heap, and is a wire property that streaming or multi-part would dissolve. Over-cap is a **422 refusal with a remedy string**, never truncation (`control.rs:2895-2899`) — I verified this because contracts §3.4 reads as though the server truncates.

---

## Options

**(a) Remove or raise the caps.** Refuted by the code's own arithmetic, and I would not reopen it. `INGEST_MAX_BATCH_BYTES_CEILING`'s doc shows that at 8 GiB one buffered body satisfies every other relation and *the second concurrent upload kills the process*; the connection count is bounded by the reverse proxy, not by anything in-process, and both in-process alternatives (a tower concurrency limit, a listener cap) were assessed and declined for turning a 429 into a stall. 0127 states the same conclusion from the other side: "a cap that carries [389 MiB] is no longer a bounded buffer." Raising 64 MiB moves the failure from a typed 422 to an OOM. **Recommend against.**

**(b) Streaming / multi-part upload — a publication assembled over several requests, committed once.** This is the only route that serves the **generating set**, because I8 forbids growth and a multi-part upload supplies the set once, at commit. Shape: `POST …/artifacts/staged` opens a publication and returns a token; chunks append; one commit request claims the ordinals and appends the record. Commit unit stays exactly what it is today, so ordinals, cycle checking and the "refusal spends nothing" property are untouched. The hard question is **where the partial upload lives**. In memory it reintroduces precisely the resident term the caps exist to bound, multiplied by the number of open publications — that is the naive version and it is worse than the status quo. Staged in the WAL as chunk records with a commit record, it is durable, replayable and invisible before commit (nothing is served until the record lands, so no I2 or I13a question arises), but it needs an abandonment rule and a retention rule, and it interacts with a WAL that has **no runtime ceiling** and is already pinned by every ungrown artifact. Costs: a new record kind, a session-like staging lifecycle on the control plane, and the first thing in the system that is durable but deliberately not visible. **This is the correct answer for content and generating sets, and it is real work.**

**(c) Chunked growth — the delta route.** Already built for memberships and working. Extending it to **parents** is cheap and safe: `dag-hierarchies.md` §4 already says a growth naming an unheld parent is reported and the memberships still land, and cycle checking is publication-scoped precisely because "a growth never adds lineage" — so admitting lineage on `PATCH` reopens cycle detection across requests, which is the one thing to rule on. Extending it to **content** is a different question: content is ranked and positional, so a delta is a *replace at rank r*, not an append, and 0076 ("served whole or not at all") plus C12 make a half-written content a disclosure question rather than an incompleteness one. Extending it to the **generating set** is forbidden by I8 and should not be attempted. **Recommend: parents yes, content only as multi-part, generating set never.**

**(d) Routes the design already implies.** Two, both already real. **Predicate membership** (`membership = { attribute = … }`) publishes nothing and scales with rows — it is built and served (`artifacts.rs:1492`, `write.rs:1714`, `viewport.rs:4686`), contrary to `artifacts-from-points.md` §9. **Computed closure** — 0127's first Open — is the structurally right answer to the specific case that started this: all 45 over-cap MeSH descriptors are DAG roots whose membership is the closure of their children's, 81% of the rows are closure, and an engine deriving a parent's membership from its children would never see an over-cap body. That is a `dag-hierarchies.md` design question, not a wire question, and it would make options (b) and (c) unnecessary for hierarchies while doing nothing for k-means or labels.

### Recommendation

1. **Write the rule down first.** 0091 as it stands does not require what the owner stated. Either amend it, or add a decision saying every kind must be creatable and extendable over the wire at any scale, with the cap-as-pagination-unit explicitly lawful and the cap-as-ceiling explicitly not. Without this the review has no standard to measure against and the next campaign will decline something else lawfully.
2. **Close the three no-route gaps**, which are the clearest 0091 breaches and are unrelated to size: `excluding` at publish, group-scoped layers' per-view artifact sets, and a runtime attribute/vocabulary declaration route (`/control/categories` is already recorded as owed). The first two are body-shape work, not architecture.
3. **Admit lineage on `PATCH`**, with a ruling on cross-request cycle detection.
4. **Take the multi-part/staged publication** for content and generating sets, staged in the WAL, or rule explicitly that a generating set over the cap is not expressible and record it as an accepted limit with the I8 argument. Either is defensible; leaving it undocumented is not.
5. **Make `PUBLISH_MAX_BODY_BYTES` a config key published on `/v1/meta`.** It is the only client-visible refusal threshold in the system that a caller cannot discover except by reading `contracts.md`, and `configuration.md` §9 says the first three shape caps are published "so a client can predict a refusal rather than discover it". This one fails its own rule.

### What needs ruling

- Does the owner's stated requirement become normative, and does "any scale" permit a pagination unit? (Governs everything else.)
- May a growth carry lineage, and what then detects a cycle spanning requests?
- Is a staged, durable, not-yet-visible publication acceptable in a write path whose WAL has no runtime ceiling — and what abandons one?
- Is a generating set over 64 MiB an accepted limit or a defect?
- Is computed closure taken up for `dag-hierarchies.md`? It removes the whole hierarchy case.

---

## Contradictions between the documents and the code

1. **`artifacts-from-points.md` §9** — "`membership = { attribute = … }` stays declared and unbuilt". It is built and served (`artifacts.rs:1492`, `write.rs:1714`/`4971`/`10191`, `viewport.rs:4686`), and decision 0127's Open cites it as working ("which is how rung 5's `publishers/source` already works"). Stale line in a normative-adjacent document, and it contradicts a decision.
2. **`per-point-attributes.md` §2.2 and §5** state a control-plane vocabulary upsert as an existing operation ("Safe, no rebuild — control-plane upsert"). No such route or code exists anywhere in the workspace — the word `upsert` appears in no Rust file. §6's amendment table corrects it ("`/control/categories` is **still owed**"), so the same document says both things.
3. **`contracts.md` §3.4** — "An artifact whose membership does not fit the cap **is published** with as many members as fit and grown below" reads as server behaviour. The server refuses with a 422 (`control.rs:2895`) and the *publisher* does the fitting. As written it licenses silent truncation of a membership, which is the exact fail-quiet `artifacts-from-points.md` §6.1 warns against.
4. **`write-path.md` does not mention artifacts at all.** No `ArtifactGrow` in §1.3's WAL record list, nothing in §4's flush or §10's configuration table, and `PUBLISH_MAX_BODY_BYTES` appears nowhere. The document that owns the write path, the WAL and the commit window is silent on the write path that this whole review is about. This is the largest structural gap.
5. **`dag-hierarchies.md` is silent on the publication cap** despite being the document that owns edges and the document 0127 defers the closure question to.
6. **`configuration.md` §9** defers all of `[ingest]` to "tuning" and never names `ingest_max_batch_bytes`; the rationale lives only in `write-path.md` §2.1 and `config.rs`.
7. **`scripts/campaign_report.py:44`** — "`layers` is gone (nothing is carried on an ingest batch's membership column any more)". Decision 0128 restored exactly that route and the driver uses it (`ingest_cycle.py:352`, `:1996`).
8. **Decision 0091's ⊘** ("the wire cannot yet say everything a file can") is declared closed by `annotation-write-cycle.md` §3.4 — "Nothing a member table can say is now unsayable on the wire". True for memberships; false for `excluding`, for group-scoped layers' artifacts, and for the roster's content and generating sets at scale. The closure was argued on the member table alone and then written as general.
9. **`config.rs:1610`** records that streaming the upload "belongs with flush" — no design document or roadmap entry records that as owed work.

**One thing I could not settle and did not test:** whether `ArtifactGrow` rides the *same* fsync as the `IngestBatch` records in a window close, which is what `artifacts-from-points.md` §6.2's "there is no state in which a point is ingested and its membership is not" rests on. `write-path.md` §2.3 describes the window close without mentioning it. Worth a targeted check; it is a durability claim, not a scale one.
