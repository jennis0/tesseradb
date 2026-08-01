# Tessera — Architecture Design

**Status:** Draft for review — revision 24
**Scope:** A service providing per-viewer access-controlled storage, indexing, filtering and level-of-detail retrieval for a large set of 2D-projected points with attached cluster structure and labels. Appendix E gives a reference authorisation plugin; Appendix F sketches a prospective valid-time extension; Appendix H states the general framing and its boundary; revision history is in Appendix G.

*A tessera is a single tile of a mosaic, and — in Rome — a token presented to be recognised and admitted. Both readings are load-bearing: the unit of storage is a tile, the unit of access is a token, and every viewer assembles a different mosaic from the same tiles without any of them seeing the whole picture.*

---

## 1. Purpose

This document describes a service that stores a large set of points — each with two projected coordinates, an access predicate, a cluster assignment and optional attributes — and serves interactive, pannable and zoomable views of them under per-user access control, with text, label and similarity filtering, and with cluster labels gated so that a user only ever sees a label derived entirely from data they are permitted to see.

It exists primarily to record the *invariants* (§4). Most of the failure modes found during design are not novel problems; they are cases of one invariant being quietly violated by an otherwise reasonable-looking optimisation. §15 lists those near-misses, Appendix C records the residual leaks consciously accepted, and Appendix D records what is prior art.

## 2. Service contract and scope

### 2.1 What this service is not

**The model pipeline is out of scope.** Dimensionality reduction, clustering and label generation happen outside the service; it receives their outputs and indexes them. It does not run UMAP, does not run HDBSCAN, and does not call an LLM. Because the service never receives a modelling hyperparameter, a disclosure threshold cannot be accidentally coupled to one.

**The authorisation schema is also out of scope**, in a more interesting way. The service knows only about opaque **terms**; how an item's label becomes a set of terms, and how a principal's credentials become a set of satisfied terms, are two pluggable functions (§6.1). The five-dimension model this design was originally built around is one implementation of them, recorded in Appendix E.

Both boundaries mean the caller owns guarantees the service depends on and cannot verify (§2.4).

### 2.2 The two-stage split

Retrieval is deliberately split so that expensive, rarely-repeated work is separate from cheap, frequently-repeated work:

- **Authorise** — accepts the caller's authoritative auth data, resolves it to satisfied terms, performs mask construction (§6.3), returns an opaque token. Paid once per distinct auth input.
- **Retrieve** — accepts a token plus a query (viewport, zoom, filters) and returns the rendered payload. Single-digit milliseconds, called many times per second while a user pans.

These are capabilities, not an interface specification; the real surface will be larger, particularly around filtering. What matters architecturally is the split and the three properties it forces.

*Authorisation is computed once, from an explicit input.* The service never infers, looks up or refreshes credentials (**I6**).

*Every retrieval is cheap*, because it operates against a precomputed mask rather than re-deriving authorisation.

*No filter form can affect authorisation.* Filters live entirely in the second stage; authorisation was fixed in the first. So the retrieval surface can grow arbitrarily around filtering without any new filter form introducing an access-control bug. This is **I12** at the interface level, and §8.2 gives the corresponding discipline: a filter form that cannot be expressed as an order-independent set producer should be reshaped, not special-cased.

### 2.3 Token semantics

A token is a **capability**: it confers exactly the access computed from the auth data presented, and nothing more.

*Staleness is the caller's to bound* (**I6**). A token does not track later credential changes. The caller decides how long a token lives by deciding when to re-authorise, which places the refresh policy where the authoritative information already is. The service enforces a configured maximum lifetime and a revocation call as backstops, not as the policy.

*A token is not slice-scoped.* One token authorises across every temporal slice, because masks are built in entity space and entity IDs are stable across slices (§5.1). Only the permutation into row space is per-slice.

*A token carries its reachable partition set*, computed once at authorisation (§12). The query path therefore needs no global partition map, and a token never contacts a store it cannot satisfy.

*Tokens are bearer capabilities.* Unguessable, bound to the issuing session, excluded from logs and URLs, revocable.

*Masks are content-addressed, tokens are not.* The canonical key is a hash of the **satisfied term set** together with the authorisation plugin's version **and the identity of the postings the mask was built from** (r19: the bundle's manifest digest; under fan-out, partition and postings-epoch, as the system architecture's cache key already records). The third component is not optional: term IDs are bundle-relative ordinals, so a cache that survives a rebuild would otherwise serve a mask naming a *different* entity set under the same key — a disclosure, not a staleness bug. These three are all a mask depends on, so two different auth inputs resolving to the same terms against the same postings share one mask. A hash of the raw auth data is retained as a fast path in front of it: on a byte-identical repeat it also skips re-running the auth function. The distinction matters once auth data carries volatile bytes — a signed assertion differs per issuance even for identical authority (§6.1), and keying on auth data alone would rebuild a mask per login.

*Eviction must be transparent.* The auth data is retained alongside the mask so that if a mask is evicted under memory pressure, retrieval rebuilds it rather than failing at an arbitrary moment.

### 2.4 Input contract

The caller supplies, and the service cannot verify:

**Two consistent authorisation functions** (§6.1) — the single largest unverifiable dependency in the design, stated as **I5**.

**Coordinates from a stable projection.** Stable across temporal slices and rebuilds. If it is refitted rather than transformed, the layout scrambles and every stored spatial artifact silently describes the wrong geometry.

**A cluster hierarchy with stable node identity.** The service stores a tree of nodes with membership and serves a per-user frontier over it; it does not reconcile identity across the caller's rebuilds.

**Labels with declared generating sets.** Every label arrives with the exact set of items it was derived from — the input form of **I3** and **I8**. A label supplied with an optimistic generating set is a disclosure the service will faithfully serve (C12).

**Per-item vocabulary vectors** for the extractive label tier.

### 2.5 Capabilities the caller needs

Beyond authorise and retrieve: ingest of a batch; drill-down from an opaque point identity *(r21; "point handle" — §10.6)*; token revocation; cursored iteration over a node's members; retrieval of a node's term distribution; submission of labels with their generating sets; and **notification of labels invalidated by deletion or predicate tightening**, so the caller knows what to regenerate (§7.6).

The term distribution is the load-bearing one: it lets the caller's labeller decide which generating sets are worth producing without reimplementing the authorisation model. Node iteration returns unmasked membership by design, so it requires a build-scope credential and must never be reachable from a user token.

### 2.6 The path end to end

A map of the request, placed here so the rest of the document has a spine. **This section is not normative** — every step is governed by the section it points at, and where this summary and that section disagree, that section is right. It exists because the path is otherwise distributed across §6, §7 and §10, and nobody should have to reassemble it.

**Once per session — authorise.**

1. Hash the presented auth data; look up *(auth-data hash, auth-plugin version)* in the mask cache. On a hit, return a token (§2.3).
2. On a miss, `terms_of_auth(auth_data)` yields term descriptors; the service interns them to term IDs (§6.1).
3. Compute the reachable partition set. A partition is reachable only if the token satisfies **every** term in its required set — the intersection of compartment markers across all disjuncts. Unreachable partitions are never contacted (§12.2, **I13**).
4. Per reachable partition, union the postings of the satisfied terms into `M_token`, a Roaring bitmap in **entity space** (**I4**). Postings are built from the exploded `(entity_id, term_id)` relation by semi-join (§6.3).
5. Write frozen-format, cache alongside the auth data so eviction rebuilds rather than fails, and return an opaque token.

**Per viewport — retrieve.**

1. **Pin once.** Resolve the segment-set version and use it for the tile table, columns, permutation and candidate lists alike (§10.4, **I11**); the mask fragment carries its own watermark, which is what step 2's composition uses (§11.2).
2. **Compose the effective mask.** `M_auth = (M_token \ L) ∪ direct_eval(L)`, where `L` is the overlay unioned with every entity at or above the watermark (§11.2, **I1**). Loaded through `frozen_view` over the mmap — no deserialisation, no allocation.
3. **Apply filters** to obtain `M_sel = M_auth ∧ filters`. Filters are order-independent set producers, composed by intersection, and never touch authorisation (§8.1, **I12**).
4. **Project into row space.** Iterate the mask in order, gather `entity_to_row[e]` into a flat buffer, radix sort, bulk-construct; cache per *(token, slice, pin)*. This is the only point at which the two ID spaces meet (§10.4).
5. **Decompose the viewport** into a few hundred quadtree tiles at the target depth. Each is a contiguous `[lo, hi)` row range by the Morton prefix property (§5.2).
6. **Count per tile.** `intersect_with_range` culls empty tiles; `range_cardinality` then gives the exact masked count with **no data file touched** — cost in containers, not rows (§10.4).
7. **Select per tile.** At or below *k*, take everything. Above *k*, take the *k* lowest-priority **visible** items — evaluated directly from the mask where coverage is low, or through the node's precomputed candidate list where it is high, the two crossing at a few percent and the choice decidable from the count in step 6. Priority is a keyed per-point constant — the high 16 bits of the item's `tessera_id` *(r21)* — and mask-independent, which is what makes the selection nest across zoom and compose across partitions (§7.2, **I7**).
8. **Collect** the selected row IDs into one `u32` array. Processing tiles in Morton order leaves it already sorted, so the gather is forward-sequential-with-gaps and cooperates with readahead.
9. **Gather** through tight loops over the mmap'd Arrow columns, writing directly into the output Arrow arrays (§10.4).
10. **Translate out.** Row IDs to `tessera_id`s — read directly from the gathered row, since the identity is stored where it is shown from. Entity IDs never cross the boundary (§10.6, **I10**).
11. **Serve labels separately**, gated on `M_auth` and never on `M_sel`, so filtering narrows the points without dissolving the map. Evaluated per request, with nothing cached above the check (§7.6, **I3**).

**Three properties carry the design.** Steps 6 and 7 are the substance of it: exact counts and post-mask sampling both come from bitmap arithmetic over contiguous ranges, so cost scales with screen area rather than corpus size. Step 4 is the sole meeting point of the two ID spaces, which is what makes **I4** enforceable as a type rule rather than a convention. And nothing between steps 2 and 10 reads a column except through a masked row-ID set, which is **I2** in structural form (§10.4).

## 3. Problem statement and constraints

The system holds on the order of 10<sup>7</sup> points today, with a design target of 10<sup>9</sup>. Under the reference authorisation plugin (Appendix E) a principal's auth data resolves to roughly 10<sup>4</sup> categories and is effectively unique per principal; term counts are recorded in §16 as unmeasured.

The access control requirement is hard: data the presented credentials do not satisfy must never be transmitted, in any form, including in aggregate. This rules out serving unfiltered tiles for client-side filtering. Residual channels accepted rather than closed are recorded in Appendix C.

Some terms mark data that must be held separately at rest and in memory, not merely masked — the requirement §12 exists to serve.

Term sizes are heavily skewed, approximately exponential, with the largest plausibly covering 25–50% of all points. New points arrive continuously, with a target visibility latency of seconds to minutes. Item predicate changes are rare. *(r23, owner decision 2026-07-30)* **That seconds-to-minutes budget covers the write path in full, deny dispositions included** — suppressions and deletions may take effect on the same scale as ingest, because a human decides them and human reaction time dominates any window the system adds. This is a **bounded, configured** delay and is not the fail-open the deny rules exist to prevent: those (an overlay lost on restart, SA §6.2; a deny retired ahead of the epoch ledger, §11.3) all concern a deny that is *lost or reversed*, which is unbounded exposure of a different kind. One rule keeps the distinction sharp and costs nothing: **a deny's acknowledgement stays coupled to its application** — hold the 200 until the entry is fsync'd and swapped, never acknowledge a deny that is not yet in force. The caller then still observes its own accepted change on its next request, and no window is ever open between "accepted" and "applied". The write path's latitude is therefore in *when work is batched*, never in whether an acknowledged security operation has taken effect. Data is held in an object store. Several temporal slices exist and must be independently browsable.

## 4. Invariants

Load-bearing guarantees: cheap to state, expensive to recover once violated. Rationale lives in the referenced sections.

**I1 — One effective mask, composed before use.** Define the *live set* `L` as the union of the in-flux overlay and every entity at or above the ingest watermark (§11.2). Effective visibility is

```
M_auth = (token_mask \ L) ∪ direct_eval(L)
```

composed at fetch time, before any consumer sees it — because most consumers (counts, density pyramids, cluster frontiers, hulls, label containment) read the mask directly and cannot be filtered afterwards. A serialisation chokepoint is retained for point payloads as a second line.

**I2 — Derived quantities are functions of visible data only.** Any aggregate shown — centroid, hull, count, density, label — must be computable from the items inside `M_auth` alone. A quantity derived from the full dataset and merely *gated* on a threshold is a disclosure, not a filtered view. Exceptions are enumerated in Appendix C; an exception not in that table is a bug. Enforced by construction rather than by discipline: the mask is the only entry point to the geometry arrays (§10.4), so there is no path along which an aggregate over unmasked rows can be built.

**I3 — Labels are served iff their generating set is a subset of `M_auth`.** Never of the filtered selection mask (§8.1). Evaluated per request, with nothing caching a label decision above the check (§7.6).

**I4 — Permissions live in entity space; geometry lives in row space.** The term index, cluster memberships and generating sets are expressed over entity IDs; spatial ordering is per-slice and expressed over row IDs; the two are related only by an explicit permutation (§5.1).

**I5 — The two authorisation functions must agree on what a term means.** If the data function indexes item *i* under term *T*, then every principal for whom the auth function yields *T* must be authorised for *i*. Everything downstream — masks, label containment, partitioning — rests on this and none of it can check it. The service reduces the surface by owning term interning, so agreement is byte-equality of canonical descriptors, but the semantic obligation is the caller's (§6.1).

**I6 — Authorisation comes only from the token.** The service never infers, looks up or refreshes credentials; a token's mask reflects exactly the auth data presented and nothing later. Staleness is bounded by token lifetime, which the caller controls (§2.3).

**I7 — Sampling happens after masking, never before.** The sample of an authorised set is not the authorised portion of a global sample. Any level-of-detail step must be *defined* over the visible set; precomputed unmasked structures may be used only as a fast path with an exact fallback (§7.2).

**I8 — A label's generating set is immutable once supplied.** Items arriving later are not part of it and must not be added. A label whose node has since grown is *stale*, not unsafe. Members leaving is an availability problem, addressed in §7.6.

**I9 — Entity IDs are append-only and never reused.** Masks and generating sets are sets of entity IDs held in caches with non-zero lifetime; reissuing a deleted item's ID grants the new item every access the old one had.

**I10 — Entity IDs never cross the trust boundary.** They are dense and, within each append-only batch **and only within one** *(r23; the qualifier is load-bearing and this sentence read as a global property — §11.1 records what the per-batch scope costs)*, assigned in **term-signature order** *(r21; earlier revisions said "ingest order" here, which §11.1 has always contradicted — a correction, not a rewrite)*, so the gap between two visible IDs is a count of unauthorised items allocated in the same window and their proximity is a statement about shared permission signatures. Clients receive an opaque `tessera_id` instead — a keyed permutation of `(shard_id, entity_id)` under a per-deployment key (contracts §2.6), which is order-free, collision-free by construction, and invertible only inside the trust boundary. *(r21; previously "per-session opaque handles", retired from the viewer plane by owner decision — see Appendix G.)* This is also what makes the signature-sorted assignment in §11.1 safe (C6).

**I11 — Row-space artifacts are versioned together.** Any cached structure expressed in row IDs carries the *(segment-set version, watermark)* pin it was built against, and a request resolves the segment-set version once and uses it throughout. The watermark component records what the artifact was built from; the watermark governing I1's composition is always the mask fragment's own (§11.2) — pins fix row-space geometry, never authorisation state. A row-space mask applied across a compaction boundary selects arbitrary rows — not stale-restrictive but simply wrong (§10.4).

**I12 — Filters narrow rendering; they never touch authorisation.** A filter may reduce which authorised items are drawn. It may never enlarge the authorised set, relax a label's containment test, or permit the cluster frontier to descend below the depth `M_auth` alone would allow. Operationally: **a filter may move the frontier up, never down** (§8.4).

**I13 — A partition not consulted fails closed.** Where a query or a containment test spans partitions (§12), a partition the token cannot reach counts as contributing *nothing satisfied* — never as vacuously satisfied. The natural implementation, which checks only the partitions it queried, serves labels it should withhold.

## 5. Core data model

### 5.1 Entity IDs and the permutation

Every item has a permanent **entity ID**, stable across temporal slices and rebuilds, never reused (**I9**), and never exposed (**I10**). All permission data is expressed in this space. IDs are allocated in append-only batches; the order *within* a batch is a free choice and §11.1 spends it deliberately.

Each temporal slice separately assigns **row IDs** by Morton rank (§5.2). A slice stores a `u32` permutation array mapping entity ID to row ID, sized by maximum live entity ID rather than item count, with a sentinel for absent entities; the inverse direction is not stored at all. Row→entity is the **inverse of the keyed bijection at the row** — a pure function of the `tessera_id` that `columns.arrow` already carries, needing no file, no map and no consistency obligation (contracts §2.6, §0.3 deviations 2 and 6). *(r22 — a correction. Earlier revisions said the inverse "is the entity-ID column of the row-ordered store", naming an artifact contracts r6 removed: `columns.arrow` carries `tessera_id` in its place and no request-path artifact stores an entity ID at all. The policy is unchanged — the inverse permutation was never stored, and after r6 it is not stored anywhere.)* With deletions and append-only IDs the gap between maximum live entity ID and item count grows, which is the exhaustion concern in §16.

**Only the entity→row direction is stored, and it is irreducible** *(r22, recorded because the question recurs in exactly this form: now that the wire identity is order-free, is the indirection still needed?)*. It is not, and never was, a disclosure mechanism — what the keyed identity retired was the per-session handle table and the stored inverse, neither of which is this array. Three reasons keep it, none of them a leak argument:

- **Permanence against churn.** Entity IDs are permanent and never reused (**I9**); a row ID is a Morton *rank*, and one new item interleaving into the ranking shifts a large fraction of it (§11.1). No single integer holds both properties.
- **The entity ordering is already spent.** §11.1 assigns entity IDs in term-signature order within each batch — measured at 8.9–36.7× on posting storage and up to 130× on union cost (r18). An ordering spent on posting contiguity cannot also be spatial rank.
- **One index, many row spaces.** Slices rank independently, and so do partitions (§12.3), while the term index exists once per partition in entity space. Collapsing the two spaces duplicates the index per slice — which is what the next paragraph says this factoring prevents.

Nor is this direction derivable the way the inverse now is: entity→row is a function of the item's *geometry*, and no key encodes a rank. What remains genuinely open is the array's **encoding**, not its existence — it is a flat uncompressed `u32` array precisely because entity order and row order are unrelated, making the values maximum-entropy; a signature-major row layout would make it near-monotone within groups and worth compressing (implementation plan §14).

This factoring is what stops the term index being duplicated per slice. Within a partition there is exactly one index, in entity space, shared across all slices; a mask fragment is built once there and permuted into a slice's row space on demand.

### 5.2 Morton ranking

Coordinates are quantised onto a 2<sup>16</sup> × 2<sup>16</sup> grid and the bits of the two integer coordinates interleaved to produce a Morton (Z-order) code. Items are sorted by this code and row IDs assigned as the *rank* in that order. The storage order is **`(morton, tessera_id)` ascending, with no further tiebreak** *(r21; the entity ID is not a sort key at any position)*: within a leaf tile the ordering is by the item's identity rather than by deeper Morton bits, and since `priority` is the leading 16 bits of that identity, ordering by `(morton, priority, tessera_id)` is *identically* ordering by `(morton, tessera_id)` — an implementation may compare the prefix first as an optimisation (§7.2, contracts §2.6).

Morton is chosen over Hilbert deliberately. Hilbert has better locality, but Morton has the prefix property: each successive pair of bits identifies a quadrant, so every quadtree tile at every zoom level occupies exactly one contiguous range of codes and therefore of row IDs. That single property underpins tile queries, density counts, LOD sampling and sharding. A precomputed **tile table** maps tile prefixes to rank ranges.

Note that a tile's *identity* is geometric — a Morton prefix — while its rank range is per-partition and per-slice. That is what lets partitions agree on tiles while ranking independently (§12.3).

At 10<sup>9</sup> items the grid gives 0.23 points per cell, so collisions are not a constraint. Past roughly 4 × 10<sup>9</sup> the grid must widen with 64-bit codes.

### 5.3 Hot columns

Per slice, fixed-width columns of **`tessera_id`**, x, y, priority and per-item scalars (Appendix A) *(r21; was "entity ID, x, y, cluster node ID, priority and per-item scalars". The entity ID is replaced by the wire identity — no request-path artifact stores an entity ID at all — and the node column, which had no reader before Phase 3, is removed: contracts §0.3 deviations 6 and 7. Not enumerated by the drafting plan; corrected here because the sentence would otherwise contradict contracts r6 and understate **I10**.)* Neither text nor high-dimensional vectors appear here; both live outside the hot path (§8.3). Sparse per-item vocabulary vectors for extractive labelling are stored separately in CSR form.

## 6. Access control

### 6.1 The authorisation plugin boundary

The core knows only this: an item carries a set of opaque **term** IDs; a token carries a set of satisfied term IDs; an item is visible iff those sets intersect. Everything about how terms are derived sits behind two functions supplied by the caller:

- **`terms_of_label(item_label) -> {term descriptor}`** — run once per item at ingest. Must be fast.
- **`terms_of_auth(auth_data) -> {term descriptor}`** — run once per authorisation. May be expensive.

This is a smaller core than it looks. The term index, mask construction's union, label containment, level of detail, filters, partitioning and storage never knew where terms came from; only the derivation did.

**The contract.** Beyond **I5**'s consistency requirement, four obligations:

*Canonical descriptors.* Both functions emit opaque byte strings; the service interns them to IDs in a shared, append-only namespace. Agreement between the two functions is then byte-equality of descriptors, which reduces an otherwise wholly semantic obligation to something partly mechanical, and keeps the namespace owned in one place. Under compartmented fan-out (§12) "one place" acquires a process address: the router holds the namespace, which is sound because descriptors are policy-side identifiers carrying no corpus data — the isolation property of §12.3 covers entity IDs and bitmaps, which never leave their partition.

*Determinism.* Same input, same descriptors — otherwise content-addressing masks by auth-data hash is unsound.

*Declared cardinality bounds.* Distinct terms, terms per item, and satisfied terms per token. The service cannot size the index or the union without them. They are sizing declarations, not enforcement triggers: exceeding one warns and never excludes (§6.2, r16).

*Cost asymmetry.* Worth repeating because plugin authors get it wrong: the data function runs per item at ingest, the auth function runs per authorisation.

**Versioning has two blast radii.** An auth-function change invalidates masks, so its version joins the mask cache key. A data-function change alters item→terms, so it requires a **full reindex** and its version belongs in segment metadata. The changes look alike and cost differently by orders of magnitude.

**Testing.** Because **I5** is unverifiable in general, it needs a property-based test: sample (principal, item) pairs, compare plugin-derived visibility against an independent reference implementation of the policy, and run it in CI. Nothing else in the design will catch a mismatch.

For the label half of that obligation the oracle already exists. Where the data function parses an access expression in the syntax of Appendix E, the reference is **`accumulo-access`** — a spec-backed, zero-dependency, long-hardened implementation of exactly those semantics. Generate random expressions and random authorisation sets, evaluate in both, assert agreement. This lives entirely in the test suite; the JVM it requires belongs in CI and never in a deployed process.

**Verifiable auth data.** Nothing requires auth data to be bare claims. A plugin may accept a signed, principal-bound assertion — a JWS, a SAML assertion, an attribute certificate — and verify it against trust anchors embedded in the plugin itself before deriving terms from the verified attributes. Signature verification is pure computation, so it is compatible with the determinism obligation and needs no capability; embedded anchors make rotation a plugin-version change, which correctly invalidates every cached mask. Two limits are inherent rather than accidental. Expiry cannot be checked inside a deterministic plugin, which has no clock; the plugin instead surfaces the credential's `not_after` and the *host* enforces it and clamps token lifetime — the plugin stays a pure function. And there is no online revocation — **I6** and the capability-free execution environment forbid credential lookups by construction — so revocation is bounded by assertion lifetime, which pushes deployments toward short-lived assertions. The practical effect is on what an `authorise` caller's credential is worth: under bare claims, whoever can call authorise can claim anything; under verified assertions, authority derives from possessing a principal's credential, and the caller's own credential merely permits submission.

**Prior art.** This boundary is where a policy engine belongs. Appendix D records that partial evaluation compiles a policy into a residual filter in disjunctive normal form, which is precisely the auth function's job — so driving authorisation from a policy engine later is a matter of writing one plugin, not a redesign.

### 6.2 The term index and bounds

The **term index** maps each term to a Roaring bitmap of entity IDs. Roaring's per-block encoding makes the skewed size distribution harmless: head terms become run containers, the long tail small sorted arrays. Store terms below a few hundred members as plain sorted `int32` arrays.

Plugins may hold whatever auxiliary structures they need to evaluate their side efficiently; the reference implementation's are described in Appendix E.

**Bounds warn; they never exclude.** Earlier revisions enforced the declared per-item term cap by excluding over-cap items from the index — invisible to every principal. That is dropped (r16, measured): a predicate is a monotone disjunction, so more terms means *broader* intended visibility, and exclusion answered "visible to many" with "visible to none" — a resource guard producing an authorisation-shaped outcome. No invariant depends on terms-per-item, and measurement found no change in the *shape* of authorise-path cost at ~130 terms per item; the true cost is pair-relation storage, which is linear and a sizing concern. So: the declared bounds remain what §6.1 always needed them for — sizing — and a runaway guard at 10⁵–10⁶ terms per item **warns** as a data-quality signal while still indexing. Performance may degrade with data shape; availability must not.

### 6.3 Mask construction

Performed once per distinct auth input, at authorisation. The auth function yields satisfied term descriptors; the service resolves them to IDs and unions the corresponding postings, per partition the token can reach (§12.3). This is the expensive step; costs are in Appendix A.

**The postings are built from a pre-exploded `(entity_id, term_id)` pair relation, joined as a semi-join.** This is a requirement rather than an optimisation. The natural alternative — hold terms as a list column per item and test containment against the presented set — is roughly three orders of magnitude slower in every implementation measured, because containment operators rebuild a probe structure per row and never hoist the loop-invariant grant set out of the row loop. The pair relation costs a second copy of the term assignments, which is small: two integers per (item, term) at roughly ten terms per item. It is called out here because it is a schema decision independent of which engine performs the join, and because the slow formulation is the one anybody writes first (§15, Appendix D).

**Composition** into `M_auth` follows **I1**.

### 6.4 Change handling

**Credential changes require re-authorisation.** There is no incremental path: the caller presents new auth data, the service hashes it, and rebuilds if the hash differs. Delta application was considered and rejected (§15) — removals cannot be applied by AND-NOT because an item may be covered by another satisfied term, and exact removal needs a forward index whose only purpose is to save a rebuild on a rare event.

**Item changes** — new items, predicate changes, deletions and administrative suppression — do not invalidate masks at all. They are handled by the overlay (§11.2). A predicate change that moves an item between partitions is the one expensive case (§12.5).

## 7. Spatial queries, level of detail, and labels

Throughout, "the mask" means `M_auth` unless stated. §8 introduces the filtered selection mask and specifies which governs each operation. §12.3 specifies how each of these composes across partitions.

### 7.1 Viewport queries

A viewport resolves to a small set of tiles, each a contiguous row-ID range within a segment. The query is an intersection of the mask with those ranges. `range_cardinality` gives the *exact* count of visible items in any tile at any zoom without touching point data, so a full per-user density pyramid is a few thousand range-count calls and exact rather than estimated. With multiple live segments a tile resolves to one range per segment and counts sum.

### 7.2 Per-user sampling

Global LOD sampling is incorrect under masking (**I7**): a principal authorised for a small or clustered slice would see a nearly empty screen while thousands of authorised items sat invisible beneath a sample that selected around them.

**The definition** *(r22 — floor ∪ threshold ∪ cap; previously "the *k* lowest-priority items in the tile", a fixed-size bottom-*k* sketch)*. Every item carries a fixed pseudo-random **priority**, derived as the **high 16 bits of the item's `tessera_id`** (contracts §2.6), which is a keyed permutation of `(shard_id, entity_id)` *(r21; previously "derived by hashing its entity ID" — an unkeyed `splitmix64`)*. For a tile *T* at depth *d*, with `vis(T)` its visible set ordered ascending by `tessera_id`:

```
cap      = min(k, K_max)
C_θ(T)   = |{ i ∈ vis(T) : tessera_id(i) < P_d }|
m(T)     = min(cap, max(min(k_min, cap), C_θ(T)))
served(T) = the min(m(T), |vis(T)|) smallest members of vis(T) by tessera_id
```

Three clauses. A **floor** of *k*<sub>min</sub> — this *is* the pre-r22 rule at a smaller budget, and it is what keeps the sparsest principals' maps from going blank, so it is the I7 guarantee and may not be removed as an optimisation. A **threshold** at `P_d`, which is where the density signal comes from: a tile with *n* visible items draws ≈ `θ_d·n` marks, and tiles at one depth cover equal screen area, so **mark count is density** rather than something recovered presentationally. And a **cap**, which bounds work, wire and overplot. Every rank and every count is taken over `vis(T)`; *k*<sub>min</sub>, *K*<sub>max</sub> and θ's anchor are either viewer-independent constants or mask-only quantities, so nothing references unmasked data and **I7** holds by construction, as it did before.

**Why a fixed-size sketch could not carry density.** All tiles at a depth cover equal screen area, so under the pre-r22 rule *any* tile holding at least *k* visible items drew exactly *k* marks: a tile with twelve visible and one with four million rendered identically (§7.3's complaint, which that section could not answer). A bottom-*k* sketch is fixed-size by construction, so its size cannot carry information. A **threshold (Bernoulli) sketch** — every visible item below θ — has a size proportional to the visible count and nests for free, because a threshold is a constant rather than a rank. Modulating *k* by count instead is **unsound**, not merely imprecise: nesting needs *k*(child) ≥ *k*(parent), a child holds roughly a quarter of its parent's count, so *k* ∝ count inverts the requirement and representatives popped on zoom-in — the same failure the bit-reversal note below records, by a different route.

**θ, and the approximation it rests on.** θ is anchored from the viewer's own **composed** visible total `V_total` over the slice and progresses per depth. `V_total` is `|M_auth ∩ rows(slice)|` — **composed, and counted in row space** *(r24)*. Both qualifiers are load-bearing and neither is optional: *composed* is the I2 requirement two paragraphs below, and *in row space* is §11.2's rule that an entity with no row contributes to no count whatever `L` says. The case that makes the second one bite is not exotic — under §11.1's group-commit allocation a batch is acknowledged, and therefore in `M_auth`, before flush gives it rows, so the gap between the two readings is the *normal* steady state of an ingesting deployment rather than a transient. Counting `|M_auth|` instead would move θ, and with it the mark count in every tile of every viewer's map, on the acceptance of items **nobody can yet draw**.



`P_0 = ⌊m_target · 2⁶⁴ / V_total⌋`, `P_{d+1} = 4·P_d`, saturating (at θ ≥ 1 the threshold admits everything), and `P_0` **saturated** when `V_total = 0`.

**The floor and the zero case are normative, not implementation detail** *(r24)*. Both were unstated, and both are observable: the differential demands exact equality, so an implementation that rounded or took a ceiling would disagree with one that floors on roughly half of all anchors. The engine and the reference oracle both floor and both saturate at zero — **by coincidence of implementation, not because this section said so**, which is exactly the state a second implementer written from the spec alone would not reproduce. Floor is the right choice on its own merits and not merely the incumbent one: a smaller `P_0` is a stricter threshold, so rounding down errs toward **fewer** marks, never more, and the floor clause is what guarantees non-emptiness regardless. The zero case is unobservable in its effects — a viewer with nothing visible draws nothing whatever θ says — but an implementation that caches θ per session has to compute *something*, and two implementations that pick differently diverge the moment either one's cached value is compared.

The ×4 is what makes the per-tile expectation depth-stable — a child holds ~*n*/4 items, so `4θ_d · n/4 = θ_d · n` — and it makes θ monotone in depth, which is what the nesting proof needs. θ is **viewport-invariant**: it depends on the session's mask, the generation and the slice, never on `bbox` or `zoom`, so it does not move on a pan, which is the churn this whole scheme exists to avoid. **The slice term is not a qualification of that invariance and does not weaken it** *(r24)*: `V_total` is counted over one slice's rows, so a session addressing two slices has one θ per slice — but a slice is named by the request (`x-tessera-slice`, contracts §3.1) and is neither `bbox` nor `zoom`, so no pan and no zoom can change it. What the argument needs is that θ hold still under the interactions a client performs continuously; it does. It does move on an overlay swap; that is accepted, since swaps are rare against pans and the served set is a prefix, so a small θ move perturbs only the marks nearest the cut.

*Annotated 2026-08-01 (no revision here — no parameter, definition or contract changes; raised from the client-interaction design, whose §6 depends on it).* **The acceptance in the sentence above was priced against a change frequency the owner has since contradicted, and the arithmetic wants restating before the parameters are confirmed.** Owner-stated expectation (2026-08-01): the dominant change is **ingest, not denial** — continuous streams of 10²–10⁶ items/hour, or batch ingests of 10²–10⁷ a few times a day. Denies are the rare case. Three consequences. First, the frequency is coarser than the raw rates suggest and this section already says why: §7.2 r24's own rule that an entity with no row contributes to no count means arrivals are invisible until **flush**, so `V_total` — and therefore θ — advances at flush boundaries rather than per item. Second, the *magnitude* is what changes. A single deny moves `P_d` by a relative 1/`V_total`, which at 10⁶ visible displaces on the order of two marks across an entire viewport — genuinely "the marks nearest the cut". A flush adding fraction *f* of the corpus is a different quantity: because `served(T)` is a `tessera_id`-order prefix and new arrivals carry uniformly distributed identities, roughly fraction *f* of each tile's served set is displaced — new items landing below the cut pushing the highest-id members out. At *f* = 1% that is unremarkable; at *f* = 10% (a 10⁷ batch against a 10⁸ corpus, squarely inside the stated range) a tenth of the map changes under a viewer who is reading it. Third, and stated precisely so it is not over-read: **nesting across zoom is untouched** — its proof is over a fixed corpus state and nothing here weakens it. What moves is stability across *time*, which this section never claimed and which the sentence above disposes of in a clause. The question is therefore one of **parameters and flush cadence, not correctness**: flushing on a cadence coarse enough to read as a discrete update event yields a calmer map than dribbling, and it is a control the deployment already has. Owner ruling of the same date bounds the urgency — **minutes of latency are acceptable for items appearing *and* for items disappearing, provided a refresh path exists** — so this is a legibility question, not a freshness one. It is recorded here rather than acted on because *k*<sub>min</sub>, *K*<sub>max</sub> and `m_target` are already provisional pending the perceptual measurement §7.2 calls for, and churn-under-flush belongs in that same measurement rather than in a separate decision.

**The anchor must be the composed total, not the frozen fragment's** — this is an I2 requirement and not a nicety. The cached row projection is `M_auth` *before* the overlay diff, so after any accepted delete or suppression it strictly contains `M_auth`. Anchoring θ there would let a viewer aggregate mark counts across a few hundred tiles, solve for the anchor, difference it against its own summed per-tile `visible` (which §7.1 discloses exactly), and recover **a running estimate of how many of its own items have been denied** — a count of items outside `M_auth`, which Appendix C admits nowhere.

The `4^d` progression assumes the viewer's items spread over ~4^d occupied tiles. Real corpora cluster, so the true occupied-cell count `O_d` is smaller and the actual marks per tile is `m_target · 4^d / O_d` — inflated geometrically in depth for a point set of box-counting dimension below 2. Worked: 10⁶ visible, `m_target` 16, `K_max` 128, depth 6. Even spread gives 4,096 occupied tiles of ~244 items → 16 marks each, as designed. Clustered into 400 tiles → 2,500 items each → 164 marks → **pinned at the cap**, so a tile with 2,500 visible and one with 250,000 again render identically. **The cap-flat region is exactly the set of tiles with `C_θ ≥ cap`**, which after θ saturates is the set with `V_tile > cap`; its lower edge is depth 0, where the inflation is exactly 1 whatever the clustering, and its upper edge is **not** the saturation depth but the depth at which the largest occupied cell falls below `cap`. The owner accepted this (2026-07-30) over both a measured per-session anchor and a client-supplied θ; §9's floor-flat and cap-flat regions were already accepted, and §7.3's underlay backstops them.

Note what θ does *not* set. Proportionality holds while `min(k_min, cap) ≤ θ·n ≤ cap`, a density ratio of `cap/k_min` — **independent of θ**. θ positions that window on the density axis; the floor and the cap set its width. And the width is `min(k, K_max)/k_min`, **not** `K_max/k_min` — so a request default below *K*<sub>max</sub> silently narrows it. An earlier default of *k* = 30 against *K*<sub>max</sub> = 128 gave a window of 15 (~1.2 decades) rather than the 64 the parameters were chosen for; contracts §3.2 now defaults *k* to the deployment's own *K*<sub>max</sub> so the full window is realised without the client having to know to ask.

**Nesting, and the one premise it needs from the client.** An item drawn in a parent tile is still drawn in whichever child contains it. Ranks fall under a subset (`vis(T') ⊆ vis(T)`), θ is monotone in depth, each clause is a `tessera_id`-order prefix of `vis` so their union is a prefix, and both surviving clauses are monotone: if `rank_T(i) ≤ k_min` then `rank_{T'}(i) ≤ k_min`; and if `p_i < θ_d` and `rank_T(i) ≤ cap` then `p_i < θ_d ≤ θ_{d+1}` and `rank_{T'}(i) ≤ cap`. **All of that holds for a fixed `cap`.** Because `cap = min(k, K_max)` and *K*<sub>max</sub> is a server constant, `cap` varies across two requests only through the client's own *k* — so **a client that reduces *k* while zooming in forfeits nesting** and will see marks pop out. *k* must be non-decreasing on descent. This is a client obligation, recorded in contracts §3; the engine sees one request at a time and cannot enforce it.

**Parameters are provisional.** *k*<sub>min</sub> = 2, *K*<sub>max</sub> = 128 and `m_target` = 16 are chosen against a perceptual argument nobody has tested — the binding constraint is overplot legibility, which cannot be settled by reasoning. *K*<sub>max</sub> is an **overplot** ceiling and is deliberately not the same knob as the machine ceiling the drawn-mark budget's probes calibrate; conflating them would mean raising the machine ceiling on transport evidence silently dissolved the cap clause and the per-tile work bound with it.

**Fewer marks than the pre-r22 rule is the intent, not a regression** *(owner, 2026-07-30)*. Below the saturation depth a tile serves a fraction of its visible items, so sparse principals draw fewer marks than the flat-*k* rule gave them. That is the point: "constant *k* hides the actual density of cells under a map with a large number of points, and Bernoulli sampling regains some of that visual density." What is *not* intended is emptiness, which the floor clause prevents — for `cap ≥ 1`, `m ≥ min(k_min, cap) ≥ 1`.

**Why the derivation changed, in this section's own terms** *(r21)*. `priority` is a `u16`, so it holds 65,536 distinct values, and for a tile with **V** visible items the *k*-th lowest priority sits at ≈ `k·2¹⁶/V` — a resolvable value only while **V ≤ 2¹⁶·*k***, which at *k*=30 is **V ≈ 2×10⁶**. Above that threshold every candidate carries the same priority and **the tiebreak becomes the sampler** — and the tiebreak was the entity ID, which §11.1 assigns in **term-signature order**, permanently under **I9**. So above V ≈ 2×10⁶ the sample was ordered by permission signature: a principal whose visible set spans two groups, one allocated lower entity IDs, saw mostly that group at coarse zoom however much larger the other was. That is precisely the failure this section rejects for global LOD sampling, moved *inside* the visible set, and it correlated with permissions **because the entity allocator was made permission-aware**. It kept the letter of **I7** and broke its purpose, at the default overview, for head principals, with candidate lists inheriting it. A keyed bijection over 2⁶⁴ is uniform and uncorrelated with signature, so the tiebreak stops being a disclosure; and because the `u16` is a *prefix* of it, prefix width becomes a performance parameter rather than a correctness one.

An earlier design selected by rank position using a bit-reversal sequence. That does not nest across a change of population — a parent's pick at rank fraction ¼ lands at local rank 0 of its second child, which does not select its own rank 0 until far beyond any plausible *k* — so essentially every representative popped on zoom-in. The note is kept because the mistake is plausible enough to be re-derived. Priority also composes across partitions where rank position would not (§12.3).

**Two ways to evaluate the definition, chosen per tile.** Step 6 of §2.6 already yields the tile's exact masked count before any selection happens, so the choice below costs nothing to make.

*Candidate lists.* Precompute, per tile node, the top **c·*k*** items by priority, unmasked, as row IDs. *(r21: "by priority" now means "by `tessera_id` prefix", so the lists inherit the fix above rather than needing their own.)* At query time, filter by the mask and take the first *k*. A list of width *c·k* yields about *c·k·coverage* survivors, so **it produces *k* of them only above coverage 1/*c***. At *c*=4 that is 25%, which given 10<sup>4</sup> grants against 10<sup>5</sup>–10<sup>6</sup> categories describes very few principals. Widening helps linearly and costs linearly: 1% coverage needs *c*=100, which is comparable in size to the hot columns. **Treat the lists as an optimisation for high-coverage principals, not as the general path.**

*Direct evaluation.* Take the visible row IDs in the tile's range straight from the mask — `range_uint32_array`, free — read their priorities, keep the *k* lowest. Cost is bounded by the tile's priority block regardless of coverage, and it *falls* as coverage falls, because there are fewer visible items to consider.

*(r21, amended r22)* The comparator **falls through to the full `tessera_id` on prefix ties**, and this is identically "the *k* lowest by `tessera_id`" because the prefix is a prefix — so there is no composite comparator to get wrong and the sample is correct at any prefix width. Width is therefore a **performance** knob: fall-through volume is ≈ V/2^w, and the trigger for revisiting it is `w ≈ log₂(V_max/k)` — about 24 bits for a 10⁹ shard at head coverage.

*(r22)* **The implemented comparator reads the full `tessera_id` and no prefix-scan path exists.** r21 forbade building one in Phase 1 on the grounds that "the sampler is the placeholder first-k, which… never reads the priority column at query time"; that premise is gone — the placeholder is gone with it — but the instruction stands, because the construction that is obviously correct is preferred to the one that is fast (contracts §2.6 permits comparing the prefix first as an optimisation, and nothing requires it). **The cost of not having the prefix path is recorded rather than hidden:** the per-viewport *scanned* column becomes `tessera_id` at 8 B/row where it would have been `priority` at 2 B/row — a **4× rise in page traffic**, 2 GB → 8 GB at 10⁹ — which is the residency correction noted in Appendix A. The `priority` column is consequently written and unread at query time. The trigger above is when to revisit.

**The two move in opposite directions, and they cross at a few percent coverage.** Descent multiplies work rather than dividing it: merging four children's lists yields four times the candidates, so reaching *k* survivors from *d* levels down visits the geometric sum of 4<sup>d</sup> nodes — **work proportional to 1/coverage, not to log(1/coverage)**. Depth is logarithmic; the node count is not, and conflating the two badly understates the sparse case. Concretely, at *c*=4 and *k*=30 over 10<sup>4</sup>-row tiles: about 21 nodes at 5% coverage against roughly 10 pages for direct evaluation, 85 nodes at 1%, and 5,461 nodes at 0.01% against a single page. **Below roughly 5% coverage, evaluate directly; above it, use the list.** Both compute the same definition, so **I7** holds either way — what changes is only which is cheaper.

**The measurement is now in** (r18; probes, results §5): realistic masks are essentially scattered under Morton order, and the duty cycle follows — at working coverages 12–99% of occupied depth-6 tiles fall below the ~5% crossover, and for tail-only principals essentially all do. Direct evaluation is the *main* selection route; candidate lists serve the dense cores of head principals. Authorisation does not usefully correlate with position, so there is no hidden upside to wait for.

*(r22 — corrected)* Where a tile spans multiple segments, **sum `C_θ` across the segments and serve the global bottom-*m* of the union**; each segment need only offer its own bottom-`cap` for that merge to be exact. Earlier revisions said "allocate *k* across segments in proportion to visible count", which is **wrong** for a prefix definition — a proportional allocation is not the bottom-*m* of the union. θ's anchor must be the whole-slice total across segments for the same reason it must be session-global across partitions (§12.3): a per-segment anchor would make "below the cut" mean different things in different segments, and the merge would stop computing the definition. Phase 1 fails closed on a slice with more than one segment, so this is spec ahead of code.

### 7.3 Density

Exact per-tile visible counts are free from §7.1, so use them. **Two mechanisms, not three** *(r22)*:

**Selection carries it directly.** §7.2's threshold clause makes a tile's mark count ≈ `θ_d·n` in the viewer's own visible count *n*, so mark count *is* density over the window `[min(k_min, cap), cap]`. This replaces the pre-r22 clause "modulate mark alpha and *k* by count", whose *k*-by-count lever was **unsound** and is struck rather than qualified: nesting requires *k*(child) ≥ *k*(parent), and a child holds roughly a quarter of its parent's count, so *k* ∝ count inverts the requirement and marks pop out on zoom-in. Modulating by count *per unit screen area* is better — on descent a child has a quarter the area and roughly a quarter the count — but still breaks nesting wherever a child is genuinely sparser than its parent. A threshold is a constant rather than a rank, which is exactly why it nests for free.

**An underlay carries the decades beyond any mark scheme.** A continuous shaded field beneath the marks, built from exact masked counts at depth *d+s* (64–256 sub-cells per screen tile at *s* = 3 or 4), each one a range cardinality over a contiguous Morton range. Map count to colour through an **explicit log transfer function** — this is why additive mark alpha saturates and this does not: alpha accumulation is an implicit *linear* transfer, and the quantity spans 6.6 decades. Build cost zero; query cost is the same row span the whole-viewport count already walks, plus one boundary rank per sub-cell.

The counts are per-user and exact, so **no disclosure beyond §7.1** — and the reason is sharper than that: a depth-*(d+s)* sub-cell count is exactly what a `zoom = d+s` request already returns, so the underlay saves round-trips and reveals no quantity a viewer could not obtain in one further call. Omitting empty sub-cells conveys `count == 0`, itself a masked count, exactly as the existing empty-tile skip does. Recorded in Appendix C.

**Two bounds are load-bearing, not tuning.** The sub-cell count per response must be capped — the tile set for a viewport is itself unbounded and the underlay multiplies it by 4^s, so at *s* = 4 over ~300 tiles that is ~77k range cardinalities against a 10 ms p99 budget. And a request for more than the deployment allows, or for a depth beyond the §5.2 grid's 16, must be **refused rather than clamped**: a Morton prefix carries no depth of its own, so a silently reduced *s* hands back cells the client cannot interpret.

**The deep-zoom fade-out rule is undesigned.** Where tiles hold few rows the sub-cell counts quantise against bitmap container granularity and the underlay degenerates; it must fade in favour of the marks themselves. The server serves exact counts either way — this is a presentation gap, recorded rather than closed.

### 7.4 Drill-down

The set behind a rendered representative is its tile's range intersected with the mask — already computed to place it. Hovering yields the exact count; a detail panel is further selections over the same range. No adjacency structure is stored, because Morton ordering makes containment implicit in the row IDs.

Where a tile is small enough to materialise, compute breakdowns exactly; above that, sample and say so. Do not precompute per-tile summaries: a tile's dominant term globally may be one this principal cannot satisfy, violating **I2**.

### 7.5 Cluster visibility

The supplied hierarchy is used as a tree, not a flat partition, so insufficient visibility becomes rollup rather than suppression: store a membership bitmap per node, descend from the root evaluating `and_cardinality` against the mask, and stop when the visible count falls below threshold. The user's clustering is the frontier of that descent — coarse where they see little, fine where they see a lot. Nobody gets a blank region; they get a vaguer ancestor.

Restrict the descent to nodes whose extent intersects the viewport. Membership bitmaps are entity-space, so each node also stores a bounding box per slice and its members' row-ID ranges per segment; the walk uses boxes to prune and ranges to intersect without a per-node entity-to-row gather.

The governing threshold, `min_visible_members`, is a **disclosure control** — every *displayed* item is authorised either way, so it exists to bound the residual structural leak in C1: that a node's existence and shape derive from global density including items the principal cannot see. Name it separately from anything the caller's clustering uses, and review it as a security control. §8.4 specifies its interaction with filtering.

*Annotated 2026-08-01 (no revision here — no parameter, definition or contract changes; raised from the client-interaction design, whose §9.1 generalises this section and §7.6 into one gating rule).* **This threshold is small-cell suppression, and C1's outstanding review should be conducted in that field's vocabulary rather than from first principles.** Statistical disclosure control — the census-table literature — has spent five decades on exactly this rule, and its central known weakness is the **differencing attack**: two overlapping releases whose difference isolates a cell below the threshold. Two halves, one already covered. **Covered:** §8.4 fixes maximum depth against `M_auth` and never against `M_sel`, which blocks the filter-differencing route and is the operational form of **I12** — a filter may move the frontier up, never down, so no sequence of filters differences a suppressed node into view. **Not examined:** differencing the frontier across **pan, zoom and slice**, where the releases are viewport-shaped rather than filter-shaped. The descent is restricted to nodes intersecting the viewport, so two overlapping viewports return overlapping frontiers, and whether their difference can isolate a below-threshold node is an open question this design has never posed. Appendix C lists C1's owner and review date as outstanding before launch; that review is the place for it, and the finding here is that it has a literature and a named attack to be checked against rather than being a fresh judgement. A survey of the adjacent fields (2026-08-01) also found **no analogue anywhere for rollup-rather-than-suppression** — every clustering and mapping system surveyed either recomputes per query or regenerates per viewer — which is a claim of absence, recorded as such, and which raises rather than lowers the burden on this review, since there is no prior art whose failure modes we inherit and can borrow.

**One case makes the analogy literal rather than structural, and the review should treat it as first-class.** A node here is a named subset of the point set — each point is a member of some cluster set, and the node's geometry is derived from that membership rather than inherent (C2). Any *supplied* subset has the same shape: a caller-supplied boundary — city, ward, postcode — is a membership bitmap plus a corpus-independent geometry, and "how many of the caller's items fall in this boundary" is one `and_cardinality` against the mask, needing no query-time spatial join. **Counts bucketed by administrative area are exactly what small-cell suppression was invented for**, and overlapping administrative geographies — a postcode inside a ward inside a district — are the textbook differencing vector, considerably more tractable to an attacker than differencing a semantic hierarchy nobody outside the deployment can enumerate. If a deployment ever buckets by supplied boundaries, `min_visible_members` governs those counts for the same reason it governs node counts, and the frontier's rollup applies unchanged. Recorded because the geographic stretch (visualisation architecture; client-interaction design §12) makes this a plausible deployment rather than a hypothetical, and because a reviewer meeting the semantic-clustering case first may not notice that the boundary case is the one the literature is actually about.

Node *geometry* — centroid, hull, count — is recomputed per user from masked membership.

### 7.6 Label gating

Labels are supplied by the caller with their generating sets (§2.4). The service gates them: a label is served iff its generating set is a subset of `M_auth` — one bitmap operation, `and_cardinality(G, M_auth) == |G|`, evaluated per request (**I3**), and decomposed across partitions per **I13**.

This rule is not an invention; it is the compartment-lattice instance of the derivation axiom from multilevel database security (Appendix D). What appears unpublished is applying it to *shared, precomputed, generated* summaries served to differently-cleared viewers.

Terms make it tractable, because a term is **permission-homogeneous** under **I5**: everyone satisfying *T* sees every item indexed under *T*. So a generating set of the form *G* = node ∩ postings(*T*) is satisfied by exactly those principals who satisfy *T* — no coverage fraction, no threshold to defend. Note the dependency: if the two plugin functions disagreed about *T*, every such generating set would become unsound at the same instant and the containment test would still return true.

The assumption this section rests on is that a typical node draws on a modest number of terms; it is unmeasured (§16).

**Availability under deletion and tightening.** Generating sets are immutable (**I8**), so ingest can never make a label unsafe — only stale. The reverse is an availability problem: a deleted item drops out of every mask, so any generating set containing it fails containment *for every principal*. Where the caller supplies a nested chain of generating sets, the smallest element is contained in every larger one, so a single deletion can dark-ship a node's entire chain. The service notifies the caller of affected labels (§2.5) and falls through the ladder meanwhile. Shrinking a generating set to exclude deleted items is **not** automatically safe — the label was generated from content including the removed item — and requires explicit sign-off and an Appendix C entry.

### 7.7 The fallback ladder

| Tier | Generating set | Source |
|---|---|---|
| Full node label | entire node membership at generation time | supplied |
| Cumulative-union labels | progressively larger unions | supplied |
| Single-term labels | node ∩ postings(*T*) | supplied |
| Extractive term list | mask ∩ node (exactly the visible items) | computed at view time |
| No label | — | — |

The extractive tier is c-TF-IDF over the supplied per-item vocabulary vectors. **Background document frequencies must come from a fixed public reference corpus, not from the live corpus** — live frequencies are an aggregate over mostly unauthorised data and determine which terms are shown, violating I2 silently. Its generating set is by definition inside the mask, so it always satisfies **I3**, and it is the only tier covering newly ingested items before the caller regenerates.

Because only satisfied candidates are ever evaluated, **a principal never learns of the existence of a label they cannot see**. The decision to withhold is a function of data on their own side of the boundary, so the refusal itself carries no information — a property most suppression schemes lack (Appendix D).

### 7.8 Guidance for the caller's labeller

Three decisions belong to the caller's pipeline, but the service's design constrains them.

*Which generating sets to produce.* The service exposes each node's term distribution (§2.5). The natural construction is a nested chain of cumulative unions ordered by term mass, supplemented by the individual single-term sets so that a principal satisfying some terms but not the largest still gets something. The chain is only a heuristic for *ordering candidates*; correctness comes from the exact containment test evaluated per candidate.

*Which set to declare.* If prompts are built from *sampled* members rather than whole nodes, "every item it was generated from" can mean the sample or the full membership. These differ materially — the sample-based set is far easier to satisfy and therefore far more available, but protects less. **Settled (r17, owner decision): the prompt sample.** It is honest to provenance — only sampled documents influenced the label text, so the sample *is* the generating set under I3's definition — and it is the available choice. The decision is recorded in the bundle manifest's provenance; full-membership declaration remains available per deployment as a strict mode.

*Label creep is the named, predicted failure mode.* Conservative joining over many sources drives the effective reader set toward empty: if nodes are formed on semantic similarity alone, most labels become unservable to almost everyone. Per-term generating sets mitigate this by construction. If measurement shows labels still widely unservable, the escalation is **ACL-aligned clustering** — partition by term-equivalence class first, cluster semantically within.

### 7.9 Conditional: per-term tile histograms

If the single-term fraction (§16) is high, precompute a sparse (term × tile) count matrix for coarse zoom levels. A visible count for a tile is then the sum over satisfied terms — a sparse matrix-vector product — with **no mask materialisation at all**. Counts sum rather than union exactly when items carry a single term; hold the single-term majority in the matrix and correct the remainder through the mask.

At 10<sup>7</sup> this is a modest optimisation. At 10<sup>9</sup> it substantially dissolves the worst path in the system, where a zoomed-out overview otherwise forces every shard to materialise a mask fragment (§13.3). The same structure can hold per-(term, tile) representative samples; every item so served is under a satisfied term, so I7 holds.

## 8. Composable filters and the two-mask model

### 8.1 Two masks

- **`M_auth`** — what the token permits (**I1**). Governs label containment, maximum frontier depth, node geometry, and the security boundary.
- **`M_sel = M_auth ∧ filter₁ ∧ … ∧ filterₙ`** — what the query asked for. Governs which points render and matched counts.

**The failure this prevents.** With a single composed mask, containment would be tested against `M_auth ∧ text ∧ vector`. A generating set contains items that do not match the text query, so containment fails for essentially every label the instant anyone types, and every label vanishes. A frontier descending on filtered counts would likewise dissolve as the filter narrows, destroying the frame of reference exactly when it is most needed. **I3** and **I12** prevent both.

There is no leak in preserving labels under filtering: the principal could already see them before filtering, and per **I12** a filter cannot widen authorisation.

**A free affordance falls out.** `range_cardinality` over both masks on the same range gives matched-and-visible against total-visible, exactly, for two bitmap operations — supporting highlight-in-context rather than removing everything else and leaving the user staring at twelve points on a blank canvas.

### 8.2 The filter contract

**Every filter returns a set of entity IDs as a bitmap.** Composition is intersection. This is also the extension discipline for the retrieval surface: new filter forms add operands rather than changing the shape of the call (§2.2). Four rules.

**Threshold, never top-k.** A filter whose result depends on what else is in the query cannot be composed independently. A top-*k* nearest-neighbour query evaluated alone is computed over the whole corpus, so intersecting afterwards *is* post-filtering — the principal's nearest neighbours would vary observably with items they cannot see. Express ranked filters as thresholds and apply top-*k* **after** intersection.

**Optional candidate push-down.** Filters accept an optional candidate bitmap. Those that can exploit it do; those that cannot ignore it. This makes vector brute-force viable and removes the post-filter leak in the same move.

**The mask goes in first, not last.** It is usually the most selective operand, and every un-intersected intermediate contains unauthorised IDs.

**Pre-intersection cardinality is structurally unreachable.** A raw match count is a corpus-wide count over unauthorised records; do not let the pipeline expose a cardinality on an un-intersected intermediate at all.

**Keep it a pipeline, not a planner.** Intersection is the only top-level operator; fix the order at design time by cost class. Statistics-driven reordering would make execution time a function of how much the principal can see.

### 8.3 Where each filter lives

**Label filtering — core, effectively free.** Resolve labels to nodes, union membership bitmaps, intersect. The label vocabulary *offered* must itself be containment-filtered, or the existence of a filterable label reveals a label the principal is not cleared to see (C11).

**Text — an embedded index.** Text results depend on the query, not the token, so they cache globally across all principals within a partition — a different key from the mask, and conflating the two yields either a cache that never hits or a leak. And **filter, do not rank**: relevance scores and rank shifts computed from corpus-global statistics are a demonstrated channel for inferring the content of unreadable documents (Appendix D).

**Vectors — a sidecar in a different format.** Cold, large (Appendix A), read in a completely different pattern from the hot columns. This is where chunked object-store-native storage earns its place.

The query matters more than the storage. Filtered approximate nearest neighbour is the genuinely unsolved problem in this space, but the other filters usually solve it first: if label and text filtering plus the mask reduce the candidate set below roughly 10<sup>5</sup>–10<sup>6</sup>, **brute-force scan the masked candidates** — a few milliseconds with SIMD, and exact.

Execution order: label → text → intersect with `M_auth` → brute-force vectors over survivors.

**One UX trap for the caller.** The 2D coordinates are a projection; similarity in the source space is not similarity in the plane.

### 8.4 The frontier under filtering

The filter influences the frontier **only through descent depth**, never through containment (**I3**, **I12**).

The naive implementation gets the direction wrong. `M_sel ⊆ M_auth`, so filtered counts are never higher, and descending on `M_sel` against a fixed threshold stops earlier everywhere — filtering would make the frontier uniformly *coarser*, not finer around the hits.

**Two thresholds with different jobs.** `min_visible_members` against `M_auth` sets the **maximum depth** and remains a disclosure control that filtering must not relax — the operational form of **I12**. A much smaller display threshold against `M_sel` decides how far *within* that bound to descend: to maximum depth where matches concentrate, stopping early over empty regions.

**Stability.** The frontier recomputes on every filter change, so labels appear, refine and disappear as someone types. Debounce, and add hysteresis so a node does not drop out because a match count wobbled by one.

### 8.5 Rendering and caching under filtering

**Two layers.** A *context* layer sampled from `M_auth`, where coverage is normal and §7.2's candidate lists work as sized; and a *match* layer from `M_sel`, which under a selective filter is small enough to send **in full**, with no sampling and no descent. Sampling of `M_sel` is needed only when the filter is broad, and then coverage is good again — so the pathological case of a sparse mask requiring *k*-per-tile sampling does not arise.

**Cache tiers.**

| Cached object | Key | Invalidated by |
|---|---|---|
| Token mask fragment (pre-composition), entity space | (auth-data hash, auth-plugin version, partition) | nothing — content-addressed |
| Row-space permutation of a mask fragment | + (slice, segment-set version) | compaction |
| Servable-label set (containment decisions) | (auth-data hash, auth-plugin version, overlay version) | overlay change |
| Filter results | (filter identity, partition, segment-set version) | ingest — shared across all principals |
| `M_sel` and the frontier | (token, filter query, overlay version) | every keystroke |

Two things this table has to get right. The cached mask is the **pre-composition** fragment, because `M_auth` is composed at fetch time from the live set (**I1**) and cannot be cached as such. And containment decisions depend on `M_auth`, which the overlay changes under a live token — so a deletion must invalidate them, which is what the overlay version is for.

Because containment binds to `M_auth` rather than `M_sel`, the servable-label set is computed once per token and reused across every keystroke.

## 9. Temporal slices

Slices are partitioned, not interleaved. Encoding time as a third dimension in the Morton code is wrong here: slices are discrete, users view one at a time, and if the projection were ever recomputed the x/y bits would mean different things at different t.

Each slice stores its own permutation array, tile table and candidate lists. The term index, node memberships and generating sets are shared across slices in entity space (**I4**). Projection stability and node identity across slices are the caller's obligations under §2.4.

**Settled (r17, owner decision): current credentials govern every slice.** A principal's present authorisation answers historical views too — losing a grant hides that data in every slice at the next authorisation. The premise of §5.1 (one entity-space mask shared across every slice) and §2.3's not-slice-scoped tokens stand. Historical-grants viewing, if a compliance requirement ever demands it, is a versioned-mask redesign and is knowingly not provided.

## 10. Storage and serving

### 10.1 Why not a query engine

By the time the store is touched, every selection decision has been made in bitmap space. What arrives is an explicit sorted list of row IDs and a request for a gather: no query to plan, no predicate to push down, no join to optimise. The survey in Appendix D found no existing engine that both retains a caller-supplied selection across queries and exposes a range-restricted cardinality over it.

### 10.2 Object store as artifact repository

Artifacts live in the bucket under **immutable versioned prefixes**; serving nodes sync the slices they need to instance-local NVMe at boot and mmap from there, so cold start is seconds. Compress at rest; decompress once at load.

Immutable prefixes make rebuilds atomic: write a new version, flip a pointer, roll back by flipping it back. The prefix name *is* the segment-set version in **I11**. This is also what makes a repartitioning (§12.5) expensive but not risky.

### 10.3 On-disk layout

The organising property is that **the row ID is the array index**. Nothing is stored to locate row *i*; it lives at byte offset *i* × width.

One file per column per segment: raw little-endian, fixed-width, uncompressed, in **`(morton, tessera_id)`** order *(r21; the intra-leaf tiebreak is the item's identity, of which `priority` is the leading 16 bits — §7.2, contracts §2.6)*. Alongside them a sorted `morton.u32` column *(r20; the code is 32 bits because §5.2 fixes the grid at 2¹⁶ × 2¹⁶ — contracts §2.5)*, the tile table, and the per-node candidate lists. Use the Arrow IPC file format with uncompressed buffers: self-describing, and the buffers remain page-aligned raw arrays that can be mmap'd and sliced zero-copy.

**Route by access ratio, not data type** *(r21)*. The rule the sentence above is an instance of: data read once per **rendered mark** belongs in a fixed-width hot column; data read once per **query** belongs in an entity-space bitmap behind the filter contract (§8.2); data read once per **interaction** belongs in a cold sidecar keyed by the wire identity and opened on first use. A viewport draws ~10⁵ marks and a user clicks a handful, so the three cadences are four orders of magnitude apart and the placement decision follows from the ratio rather than from the type of the data. The caller's external ID is the first instance decided this way: it is per-interaction, so it is a sidecar (contracts §2.4), not a column. **The per-interaction row and §8.3's vector sidecar name one slot, not two**: per-point metadata, the full record, provenance, text and vectors are all per-interaction, and the intention is that a single adopted store eventually serves them rather than each growing its own format. The external-ID sidecar is that slot's first and deliberately transitional occupant. **Appendix D does not bar such an adoption:** it rejects adopting a search engine, vector database or relationship-based authorisation service **for the access-control layer**, where a wrong or stale answer is a disclosure. A cold store read only after the mask has already decided visibility never participates in masking; it inherits instead the ordinary conditions — fail-closed with typed errors, integrity verified before an answer leaves it, and off the request path. **Expanding the hot columnar store remains an available trade** — more per-point data on the render path, paid for in resident memory at 0.93 GiB per byte per row per 10⁹ items — and a proposal to take it should state that number against Appendix A's budget rather than treat the store as closed.

### 10.4 The query path

**Mask loading.** Write mask fragments in **CRoaring's frozen format** and take a `frozen_view` over the mmap'd bytes — zero deserialisation, zero allocation. Deserialising a bitmap per query costs in proportion to mask cardinality rather than viewport size, which is the wrong asymptotic shape for panning.

The primitives the design depends on, named so nobody reimplements them: `roaring_bitmap_range_cardinality` for tile counts; `roaring_bitmap_and_cardinality` for masked counts without materialising an intersection; `roaring_bitmap_rank` plus `roaring_bitmap_select` for positioned access; `roaring_bitmap_range_uint32_array` to write directly into a gather buffer; `roaring_bitmap_intersect_with_range` to cull empty ranges before counting.

**The permutation.** Masks are built in entity space (**I4**) but tile ranges are in row space. The naive construction — iterate, look up, insert — is slow because inserts arrive out of order. Instead iterate in order, gather `entity_to_row[e]` into a flat buffer, radix sort, and bulk-construct from the sorted array.

**Version pinning.** A request resolves its *(segment-set version, watermark)* pin once and uses it for the tile table, columns, permutation, candidate lists and mask alike (**I11**). Compaction publishes a new prefix and flips a pointer; in-flight requests complete against the old prefix, retained until drained. This is the standard session-pinning pattern from mature search engines.

**Per-viewport path.** A viewport resolves to a few hundred tiles. For each, two lookups give the `[lo, hi)` row range and `range_cardinality` gives the exact authorised count — no data file touched. Representatives come from the candidate list path of §7.2.

Collect selected row IDs across tiles into one `u32` array. Processing tiles in Morton order yields it already sorted, which matters: a sorted gather is forward-sequential-with-gaps and cooperates with kernel readahead. The mmap touch is then tight loops writing directly into the output buffers, which *are* the Arrow arrays.

**Paging.** A tile of 10,000 rows spans ten 4 KB pages in one float32 column, so selecting thirty representatives touches at most ten pages. Column-major wins decisively for dense range reads — ten pages per column against forty-nine for the full row — and Arrow is columnar, so a row-major store would require a transpose on every response.

**Structural ordering.** The mask is the sole entry point to the geometry arrays: no counting, aggregating, hulling or density path may read a column except through a masked row-ID set, and none may be composed underneath the mask. This is **I2** expressed as a code-structure rule rather than as a behavioural obligation, and it is the difference between an invariant that survives refactoring and one that erodes. The precedent is instructive — the one surveyed system that never leaks aggregates over unauthorised records gets that property purely from stacking every aggregating operator *above* its visibility filter, while the systems that do leak have a separate aggregation path that fell out of sync with the filtering one (Appendix D).

**Warmup.** Pre-faulting at boot makes the mmap effectively resident for the process lifetime, which is the real argument for mmap over an explicit cache: no eviction policy to write or tune.

**Expected latency**, warm: low single-digit milliseconds per viewport with a single segment. Roaring's `select` walks containers from the start of the bitmap rather than being constant-time, so per-tile selections must be batched from a computed base rank.

### 10.5 Node model

A warm stateful tier is **mandatory**. Masking cannot be pushed to a CDN, unmasked tiles cannot be served for client-side filtering, and tiles cannot be sharded by term when principals satisfy thousands of them.

Serving nodes hold the term index, the auth plugin's auxiliary structures and the text index resident for their partition. Token-to-mask state is held with LRU plus maximum-lifetime eviction, with the auth data retained alongside so eviction is transparent (§2.3).

**Resident for the partition, not read per viewport** *(r20)*. The sentence above sizes what a node holds; it is not a claim about per-request cost, and it has been read as one. Mask build is **per session**, and it reads only the ~10<sup>4</sup> postings the principal actually satisfies — not the index. Ordering the structures by access *cadence* rather than by size gives a much smaller per-viewport working set than the residency figure implies: `priority` is scanned per viewport, `morton` is touched sparsely (~30 pages per tile), the gather columns are per viewport but page-sparse at small *k*, and the term index and `permutation.bin` are per *session*. **At a large mark budget the gather columns join the per-viewport set** — the gather stops being a sparse point read and becomes a scan — which is what makes the drawn-mark budget a residency question and not only a latency one.

### 10.6 Wire format and the trust boundary

Responses are Arrow IPC; typed arrays go straight into GPU buffers with no parsing. Points carry an opaque `tessera_id` rather than an entity ID (**I10**), which the server inverts on drill-down. *(r21; previously per-session opaque handles. The handle mechanism is retained for Phase 3's node handles, where the identity is genuinely per-session; a point's identity is not, and a stable identifier is what lets a client bookmark, share or reconcile a point across sessions.)* The point identity and the authorisation token are different objects. **A stable identity is linkable across sessions and across principals by construction — see Appendix C's C17, which records what that costs and why it is the intended trade.** The identity is a *transport* identifier: it survives rebuilds, but not a repartitioning (§12.5), which advances the identity **epoch** at the §10.2 prefix flip. A consumer that persists an identity persists the caller's `external_id`, not this one.

Retrieval returns at most one label per frontier node with the tier it came from, and nothing for nodes where no candidate is satisfied.

**Failure semantics: fail closed.** If mask construction, composition or containment evaluation fails, return an error — never a partially filtered result set. Most systems in this space fail open; the correct model here is the opposite (Appendix D).

## 11. Ingest and change

Target visibility latency is seconds to minutes. Ingest is not coupled to any credential cadence, because under §2.2 there isn't one.

### 11.1 What ingest may and may not touch

Entity space is append-only (**I9**), so ingest appends entity IDs and appends postings to the term index. It never reorders, never rewrites, and never invalidates anything expressed in entity space — masks, node memberships and generating sets all remain valid, merely incomplete. New term descriptors are interned per §6.1.

Row space is where the churn lives. New items interleave arbitrarily into the existing Morton ranking, so a correct in-place insert would renumber a large fraction of the slice. That is why the permutation exists (**I4**).

**Spend the entity-ID ordering on posting compression.** Because entity and row space are related only by a permutation, the two orderings can be optimised independently. Row space is fixed by geometry; entity space is free *within* each append-only batch. Assign entity IDs within a batch sorted by term signature, so term postings form long runs inside each batch's ID range and head terms encode as run containers. It costs nothing, does not weaken **I9**, and is safe only because of **I10**.

*(r21)* Contracts r6 makes this stronger in substance while changing its mechanism. With `columns.arrow` carrying a `tessera_id` instead of the entity ID, no request-path artifact stores an entity ID at all — the gather cannot produce one — so the ordering freedom this section spends on posting compression is protected by construction and not only by a discipline at the serialisation chokepoint. What the viewer sees instead is a keyed permutation of `(shard, entity)`, which is order-free: signature order does not survive it, and gaps in it count nothing. The residual channel is a caller's own external IDs where the caller chooses to carry structure in them, which is C6 as revised.

Entity IDs must **not** be assigned in Morton order, which is the tempting alternative because it would make new segments permutation-free.

*(r22 — the prohibition is unchanged; its argument is restated because the one it was written on no longer carries it alone.)* As written, this rested solely on **C6**: Morton-ordered permission-space IDs would give the ID gap between two visible points spatial meaning, turning it into an estimate of how much unauthorised data lies between them. That reading remains true, but r21 put a keyed, order-free identity on the wire and moved C6 to *Accepted — caller's control*, so it is no longer load-bearing. Two grounds that were always the stronger ones, and are not disclosure arguments at all, carry it instead:

- **Morton rank is not permanent.** It is a rank in a total order that every append disturbs, so an entity ID assigned from it would have to be renumbered — which **I9** forbids outright, at the first append.
- **The ordering is already spent, on the thing that pays.** The measured posting compression comes *entirely* from term-signature grouping within the batch (*Measured (r18)*, below). Assigning entity IDs by geometry forfeits all of it to remove an indirection the churn argument above requires in any case — every slice ranks independently (§5.1), so the permutation survives for all but the first.

A future reader must not reinstate the Morton-order alternative on the grounds that C6 has been relaxed; C6 was never the reason it fails.

This is index compactness, not the avoidance of a cliff: a fully scattered mask falls back to fixed-size bitmap containers whose footprint is already the figure in Appendix A.

**Measured (r18):** created-order assignment yields run lengths of 1.00–1.26 against a 1.000 random baseline — nothing — while signature-sorted assignment buys 8.9–36.7× on posting storage and up to 130× on union cost at equal coverage (probes, results §2, §4.2, §4.4). The compression comes entirely from this ordering, and because I9 makes assignment permanent it ships in the Phase 1 allocator or not at all.

**The scope of that sort is one batch, and nothing repairs it afterwards** *(r23; stated because this section asserted the win without recording how much of it a deployment actually collects, and Phase 2's flush is about to be designed on top of it)*. The freedom this section spends is free *within* a batch and only there. Three rules close the routes back: ingest "never reorders, never rewrites" (above); §11.3 and the system architecture's compaction rule leave the term index in entity space untouched, so a compaction folds posting *content* while the entity axis stays fixed; and §5.1 makes entity IDs stable across rebuilds, so not even a full `tessera build` re-sorts them *while preserving identity* — a rebuild that reassigns them is possible and is exactly the escape hatch the implementation plan's §14 records, at the price of a caller-visible identity break of the kind §12.5 already precedents. Within the identity guarantee, the term index is a concatenation of per-batch-sorted runs over a globally unsorted entity axis, and the fragmentation is monotone in the number of batches.

**This is an unbanked gain, not a regression — and the distinction governs how to read every latency figure in the corpus.** The probe corpus assigns `entity_id` in created order (probes, dataset §2), and §4.4 simulated signature-sorted assignment by re-permuting and then measured **serialised size only**. No union has ever been *timed* under signature-sorted assignment. Every absolute authorise figure the corpus publishes — including the 588 ms realistic worst case that §13.3 and Appendix A both lean on — is therefore already a created-order measurement, i.e. already the un-banked state. Nothing degrades from those numbers; what is at stake is how much of the promised improvement a deployment ever collects. **A reader must not multiply a published union timing by a decay factor: that double-counts.** The storage figures are different and directly usable, because §4.4 measured both orderings — 8.9–36.7× is a real before-and-after (and 1.0× for the `surnames` config, which is the reminder that all of this is policy-dependent).

The shape of what is collectable follows from the container model. For a term of density *p* assigned at sort scope *B*, containers span roughly `max(N/B, p·N/2¹⁶)` against a global-sort floor of `p·N/2¹⁶` — so the benefit available to that term goes as **`max(1, 2¹⁶/(p·B))`**. Three consequences, and the second and third are why a single corpus-wide multiplier is the wrong instrument:

- **At request-sized scope there is nothing to collect at all.** `p·B < 2¹⁶` for every `p ≤ 1` once `B ≲ 6·10⁴`, so at a batch of ~10² items *every* term is fully scattered regardless of density — the "nothing" measured for created-order and rejected above. This is the finding that matters operationally: a bulk load chunked into small requests forfeits the entire win, permanently.
- **The benefit is term-dependent, and most terms have none to give.** The measured distribution is 3 / 348 / 31.9M postings at median / p99 / max with 34.4% singletons (probes, results §4.3). A term with *k* postings touches at most *k* containers under any ordering, so singletons and near-singletons are already optimal; head terms at *p* ≳ 25% span every container regardless. The collectable band is in the middle.
- **The ~130× union figure is a ceiling on what contiguity is worth, not a measurement of this mechanism.** It compares two label *configurations* at equal coverage and equal mask size (probes, results §4.2), and their grant widths differ (2,301 vs 389 terms) as well as their contiguity, so part of the ratio is width. It bounds the prize; it does not size this particular lever.

Two bounds keep this a latency-and-footprint concern rather than an alarm. **The render path is not on it**: the exposure is authorise latency, fragment-cache residency and bundle bytes, never per-frame cost — though the honest qualification is that a larger fragment evicts sooner, so "once per session" is paid more often, and a session's first viewport does wait on it. And **the storage side cannot cliff**: Appendix A budgets the term index at ~20 GB for ~10 postings/item at 10⁹, which is ~2 B/posting — array-container territory, i.e. the scattered representation already. That is a *storage* argument and does not transfer to latency; the latency ceiling is the fully-scattered union, which the same measurements put at 2,885 ms for a 25%-coverage head principal at 10⁹. Scale matters less than the earlier drafting of this section suggested: at 10⁷ with 10⁴ grants the measured union is already 237–301 ms (probes, results §7), so "small corpora are immune" is not available as a comfort.

**The sort scope is set by when the batch is acknowledged, not by when its rows arrive** *(r23; an earlier drafting of this paragraph said allocation time was immovable, which was wrong — it read contracts §3.4's "the ID must exist before the ack" as "the ID must exist on arrival", and §3's seconds-to-minutes write budget had already ruled that out)*. §3.4 requires `/control/ingest` to ack with a per-row `tessera_id`, and `tessera_id` is a bijection of the entity ID, so allocation must *precede* the acknowledgement. It need not precede the *window*: with §3's budget the request can be held open while arrivals accumulate, and the whole window then signature-sorted, allocated, fsync'd once and acknowledged together.

That is **group-commit allocation**, and it is the fix — the mechanism is already sanctioned (the lifecycle design permits group commit on the WAL) and it makes the effective sort scope the commit window rather than the request. Nothing crosses the boundary that did not before: the caller receives the same per-row `tessera_id` in the same 200, only later. It costs exactly the latency §3 already grants and nothing else — in particular no ID slack, because it issues precisely what it allocates. The lifecycle design carries the mechanism; what survives here is the general point that **the ordering is bought by batching, so a deployment collects this win in proportion to how large its commit windows are**.

Beyond what batching can reach — the win is still bounded by window size, and a corpus assembled over many windows still fragments across them — the implementation plan's §14 records the only construction that would recover it in full, splitting the permanent identity from a renumberable index ordinal. That one is a sketch with an unclosed safety argument, not a plan, and it recedes further now that group commit is available.

### 11.2 The buffer, the watermark and the overlay

Arrivals land in an **in-memory buffer**. A flush policy — size or age, whichever trips first — turns the buffer into an immutable on-disk segment, so segment count is governed by the flush interval rather than the arrival rate.

**The mask carries an entity high-water mark.** A mask fragment built at watermark *W* is authoritative below *W*. Entities at or above *W* are new and not yet folded in. Flushing advances *W* by OR-ing in the flushed segment's contribution for the token's already-known satisfied terms — a small, monotone patch rather than a rebuild.

**The overlay holds items in flux below *W*:** those whose predicate changed, those deleted, and those administratively suppressed. Each entry carries a **disposition** — *evaluate*, meaning test its current term set against the token's, or *deny*, meaning invisible regardless. The disposition is what lets one mechanism cover both a predicate change and an administrative suppression. Entries carry their own term sets inline.

Together these define the **live set** `L` = overlay ∪ {entities ≥ *W*}, and **I1**'s composition follows.

**Direct evaluation needs no index**: an item's term set is a handful of IDs, so visibility is a set intersection against the token's satisfied terms — microseconds for tens of thousands of items.

**But membership of `L` is not, by itself, visibility — for the map verbs, geometry is** *(r23; a correction, and the sentence it corrects is one this document has always implied rather than stated)*. `L` answers *may this principal see it*, in entity space. The **map** verbs then ask a question in **row** space: a viewport counts rows in a tile range, density counts rows per cell, selection picks rows. An entity with no row in any segment contributes to none of them, whatever `L` says about it — the composition resolves its verdict and then has nowhere to put it (**I4**: the two spaces meet only at the permutation, and a buffered item has no entry in one). The buffer's items acquire geometry at **flush**, and not before.

The verbs answered wholly in entity space are the exception and must be checked separately rather than assumed to follow: §7.5's cluster visibility and §7.6's label gate evaluate `and_cardinality` against membership and generating sets, both entity-space (they are unaffected here — §11.1's rule that entity-space structures stay "valid, merely incomplete" covers them), and **drill-down resolves one bit in entity space before it looks up any row** (contracts §2.6). That last one is the case to get right: a buffered item passes the entity-space test and *then* finds no row. It must return the same *unknown* outcome as an identifier naming nothing — which is also what Appendix C's C4 annotation requires, since its timing closure rests on the arms being indistinguishable. A third arm that does strictly more work before returning the same answer narrows the closure to a claim about *identical outcomes* rather than *identical work*, and is noted here as the one place this staging touches the leak register.

The consequence is worth stating plainly, because it is easy to read this section as making flush an optimisation. **Flush is the visibility mechanism, not a compaction convenience.** The claim that correctness never depends on patching a fragment (system architecture §6.4) is a claim about *flushed* entities: once an item is in a segment at or above the fragment's watermark, `L` covers it exactly and no fragment needs touching. It says nothing about items still in the buffer. A phase that implements the buffer without the flush has built durability and authorisation state, not ingest visibility: an accepted item's acknowledgement is a **durability receipt, not a visibility promise**, and the `tessera_id` it returns resolves to nothing until a flush or a rebuild gives the item a row. That is a defensible staging — it is Phase 1's — but it must be stated, because the alternative reading is that ingested data is already queryable.

`L` is bounded by change rate times the interval before masks are naturally rebuilt. A request resolves its segment-set version pin once and uses it throughout (**I11**); the watermark governing I1's composition is the one belonging to the mask fragment actually used, and is never pinned across requests — pins fix row-space geometry, not authorisation state (a suppression applies to a pinned request the moment it is accepted; see the concurrency and lifecycle design).

### 11.3 Segments, merging and compaction

A tile resolves to one contiguous range per live segment, so cost is linear in segment count and it must be bounded.

Structure the merge policy on established lines rather than as a scheduled job. Three ideas transfer directly: a **floor size**, below which segments are treated as equally small so a tail of tiny segments does not dominate decisions; a **maximum merged-segment size**, preventing any merge from becoming an unbounded rewrite; and **separating the reasons to merge** — natural tiering, forced compaction, and tombstone reclamation on a deletes-percentage trigger.

Better still, make **re-ranking a decorator on the merge policy**: reorder only merges above a minimum document count, skip rather than fail when memory is short, and always reorder on forced merges. The Morton re-rank becomes a continuous property of large merges rather than a scheduled cliff. Reference points from a widely-deployed policy: ten segments per tier, a 5 GB maximum merged segment, a 2 MB floor, a 20% deletes threshold, reordering above 2<sup>18</sup> documents.

A compaction rewrites the permutation, tile table, candidate lists and columns, publishes them under a new segment-set version, and lets in-flight requests drain (**I11**). At single-node scale it does **not** invalidate the term index, masks or generating sets.

Deletions are tombstones: remove the entity from the term index, add it to the overlay with *deny* disposition, notify the caller of affected labels (§2.5), and drop the row at the next compaction. Never recycle the ID (**I9**).

## 12. Compartmented partitions

Some terms mark data that must be held separately at rest and in memory, not merely masked. A **partition** is a set of items sharing a required term set, stored in its own files and served by its own process. It is a second sharding axis — §13.3 shards by row range for scale, this shards by term for isolation — and the two compose.

### 12.1 What the requirement actually is

One distinction decides whether this design is valid at all, and the two readings are hard to tell apart in a policy document.

If the requirement is that **data** be separated — compartment A's items not co-resident with compartment B's — the design below is correct. If the requirement is that **audiences** be separated — A-cleared and B-cleared principals must never touch the same hardware — then data visible to both cannot exist anywhere, and items spanning compartments must be rejected rather than placed.

**Settled (r17, owner decision): the requirement is data separation.** The rest of this section is operative, not conditional.

### 12.2 Required sets and the gate

**A partition's required set is the intersection of compartment markers across every disjunct of an item's predicate** — every route to the item, not merely one. If an item's terms are {a, R} and {b, R}, then R is genuinely required: no route in avoids it. If its terms are {a, R} and {b}, then R is not required, because the second term admits a principal without it.

**The gate: a token may query a partition only if it satisfies every term in the required set.** This is exactly sound rather than merely conservative — a token satisfying any of an item's terms necessarily satisfies everything in the intersection, so the gate never excludes an authorised token, and it excludes only tokens that could not have satisfied any term. It is a cheap necessary condition, checked on every request, which converts the isolation property from something hoped-for into something enforced.

**The mixed case falls out rather than needing a rule.** An item with one uncompartmented disjunct has an empty intersection and lands in the default partition automatically. If any of its disjuncts carried a compartment marker, that marker is provably doing nothing — the item is reachable without it — so **warn loudly**. It is almost certainly a labelling error, but it is not a security violation, so it must not block ingest.

Under the reference plugin (Appendix E) compartments map naturally onto the conjunctive dimension, whose semantics already are "must hold every one of these".

### 12.3 What partitions, and how queries compose

Everything keyed on entity partitions with the items: the term index, columns, permutation, tile table, candidate lists, node membership, generating sets, text index and vectors. **Each partition has its own Morton ranking and its own row-ID space**, which preserves the row-ID-is-the-array-index property that a sparse global ranking would destroy. Node metadata — bounding boxes, extents, term distributions — is held per partition too.

**The mask never exists whole.** Each partition builds its own fragment from its own term index, so no process outside a compartment holds a bitmap containing its entity IDs. That is the isolation property, and it is why authorisation fans out.

**Counts sum.** Tile identity is geometric (§5.2), so partitions agree on which tile is which while mapping it to their own rank ranges.

**Priority sampling composes exactly.** Because priority is a global per-item property, the *k* lowest-priority visible items in a tile equal the *k* lowest of the union of each partition's *k* lowest. Every partition runs §7.2 locally, including its own candidate-list descent, and the coordinator takes the global top *k*. The rank-position scheme rejected in §7.2 would not have composed, since rank positions are relative to a population.

*(r21)* **This claim is restored, not amended** — the premise it rests on had quietly lapsed and the defect is recorded here rather than left for a reader to rediscover. Once entity IDs became shard-local `u32`s, `splitmix64(entity_id)` stopped being the *global per-item property* the argument names: item 12,345 carried an **identical** priority in every shard, and a global order of `(priority, shard_id, entity_id)` made shard 0 win every tie. `high16(tessera_id)` is global by construction, because the shard is part of the bijection's input. The composition argument above stands verbatim on the repaired premise.

*(r22)* **The claim survives §7.2's three-clause definition, but θ's anchor must be session-global across partitions.** With a global θ the composition is exact in all three regimes: where `C_total > cap` each partition's own bottom-`cap` contains the global bottom-`cap`; where `k_min ≤ C_total ≤ cap` the union of the per-partition threshold sets *is* the global threshold set; and where `C_total < k_min` the floor is satisfied from the union. So each partition runs §7.2 locally, contributes its own bottom-`cap` and its own `C_θ`, and the coordinator sums the counts and takes the global bottom-*m*. **A per-partition anchor would break this** — `P_d` would differ between partitions, "below the cut" would stop being one predicate over the union, and the merge would no longer compute the definition. The same requirement holds across segments within a slice (§7.2's closing paragraph).

**Containment decomposes.** Since generating sets and masks both partition by entity, `G ⊆ M` iff `G_p ⊆ M_p` for every *p* — subject to **I13**: a partition the token cannot reach counts as failing, never as vacuously satisfied. A label whose generating set has a non-empty slice in an unreachable compartment must be withheld, and the natural implementation gets this wrong.

### 12.4 Discovery, identity and cost

**Partitions are discovered, not declared.** When an item arrives with a previously-unseen required set, its partition is created. Existing partitions are untouched.

**Identity is a canonical hash of the sorted required set**, which makes creation idempotent and lets two ingest workers racing on the first item of a new combination converge rather than conflict.

**Alarm on creation.** A new combination means either a data error or a policy change, and someone should look either way. Silent creation is how the problem is discovered at 3am.

**Cap the count.** A token fans out to up to 2<sup>n</sup> − 1 partitions where *n* is the number of compartments it holds — exponential in a parameter someone may increase casually. With few, coarse compartments it is nothing; it does not stay nothing. Each partition also carries fixed infrastructure overhead, and fan-out p99 is the slowest partition.

**A combined partition owes the union of its constituents' isolation requirements.** If A mandates particular key management and B mandates a jurisdiction, the A+B store owes both. That is a real per-combination cost and a further argument for keeping compartments coarse.

**Isolation strength is a ladder**, and which rung is being bought should be written down, because "stored separately" is read differently by different reviewers. Separate process and address space with separate mmap'd files is the cheapest meaningful rung; adding a separate memory cgroup and no shared page cache closes core dumps and swap; a separate host with its own keys and operator access closes the rest.

### 12.5 Changing the partitioning

Two categories that look alike and cost differently by orders of magnitude.

**Data changes.** An item's predicate changes such that its required set changes, so it must move stores. The overlay covers visibility during the move — *deny* in the source, evaluate once landed in the destination. Rare per §3, but this is the one case where a rare event has an expensive tail, so design the move as a first-class operation rather than discovering it.

**Policy changes.** Marking a previously-open term as compartmented, un-compartmenting one, changing a required set, splitting or merging compartments. Every one requires re-evaluating placement for every affected item, including a global scan to find items that should now move *in*. That is a reindex, and there is no incremental path worth building.

So **the compartment map is a schema-level decision, not a runtime knob** — worth stating plainly so nobody builds an admin UI implying otherwise. The mitigation is already in the design: §10.2's immutable versioned prefixes mean a repartitioning is built under a new prefix and cut over by flipping a pointer. Expensive, but not risky, and reversible.

## 13. Scaling to 10<sup>9</sup> items

### 13.1 What breaks, in order

The materialised mask breaks first: at 125 MB dense, a thousand live auth inputs is 125 GB, and the posting union over billions of postings takes seconds rather than milliseconds. Single-node residency breaks second. Index build time breaks third.

### 13.2 What does not break

The tile scheme is invariant; tiles simply go deeper. And label generation cost is the caller's, so it does not appear here at all.

**The render path's invariance is a claim under test, not a settled property** *(r20)*. Earlier revisions asserted it flatly — "a viewport shows a few thousand points at 10<sup>9</sup> exactly as at 10<sup>7</sup>" — but that held only under the assumption that a viewport draws a few thousand marks. The owner decision of 2026-07-29 inverts the assumption: the drawn-mark budget should be the largest a given client can render, plausibly 10<sup>7</sup> on a capable GPU, which changes which reads dominate and which structures must be resident (Appendix A, §10.5). Probes P1 (GPU render) and P2 (transport and decode) decide it; until they report, this section claims nothing about the render path at large *k*. See the drawn-mark budget spec.

### 13.3 Sharding, and an unresolved trade

Row IDs are a spatial ordering, so contiguous ranges are contiguous regions of the plane and the shard key exists already. Columns, tile tables and candidate lists partition trivially.

**Masks do not partition as cleanly.** Shards are row ranges; the term index is entity-space (**I4**); in the compacted base the entity-to-row mapping is an arbitrary permutation. So a shard's entities are scattered across the entity ID space, and Roaring's 2<sup>16</sup> block structure does not align with row-range shards. A shard cannot build "the fragment covering its own rows" from a broadcast satisfied-term list unless it holds row-space postings for its range. Two ways out:

- **Row-space sharded term index.** Fragments build locally from a broadcast list. Cost: compaction must permute and re-scatter the index, contradicting §11.3's asymmetry at this scale.
- **Entity-space construction plus exchange.** Shard mask construction by entity range, then shuffle fragments to row-range serving shards. Keeps the factoring, at the price of a distributed exchange per token per slice.

The first is simpler operationally and was the assumed default. **Phase 0 measurement leans against it** (r16): posting compression and union speed both come from signature-sorted *entity* order — 8.9–36.7× on storage, up to 130× on union at equal coverage, figures measured on a single bulk build and therefore subject to §11.1's per-batch decay — and row space is Morton order, where realistic masks measured essentially scattered (run ratio 1.03–1.15; probes, results §4–5). Re-scattering the term index into row space per shard would forfeit exactly those measured wins, on every shard, at every build. The lean is therefore toward entity-space construction plus exchange; still decide with at-scale measurement when 10¹⁰ is real, and §7.9's histogram path materially reduces how often fragments are needed at all.

Given a resolution, the rest holds: a zoomed-in viewport touches one or two shards; a zoomed-out overview fans out but each shard builds 1/S in parallel. Resist over-sharding: with fan-out the p99 is the slowest shard.

Note that this fan-out multiplies with §12's: a token reaching *p* partitions across *S* shards fans out to *pS* in the worst case.

### 13.4 Staging

Below roughly 10<sup>8</sup> items this is premature: a hundred million items fits on one large instance, and buying RAM is far cheaper than distributing a system. The design rule to hold now is **not to introduce anything that assumes a single global mask or a single-process index** — that costs nothing today and is what makes the sharding step available later.

Note that nobody has publicly demonstrated 10<sup>9</sup> *identifiable, filterable, labelled* points; systems reaching 10<sup>9</sup> aggregate to bins or render a static unmasked catalogue (Appendix D). Prove 10<sup>8</sup> first.

## 14. Build pipeline

The service consumes model outputs (§2.4) and builds serving artifacts. Bulk analytical work over Parquet; a columnar analytic engine suits it, to the left of the pipeline, never in the request path.

Full build: resolve each item's label to terms via the data plugin and intern the descriptors; derive each item's required set and assign its partition; then, per partition — **derive each item's `tessera_id` from the deployment key and its `(shard_id, entity_id)`, before the sort** *(r21; the build sequence inverts. Under `priority = high16(tessera_id)` a function of the identity **is** the sort key, so the identity must exist before the tiler runs — it is no longer a column value written at the row afterwards)*; quantise supplied coordinates and compute Morton codes; **sort by `(morton, tessera_id)`** and assign row ranks per slice, permuting the companion entity-ID vector identically — it is still needed, for the permutation and the external-ID sidecars, but as a companion and not as a sort key; **derive the `priority` column as `(tessera_id >> 48) as u16` over the already-sorted identity column**, a projection computed at exactly one place and never an independent hash; emit permutation arrays, tile tables and per-node candidate lists; build the term index in entity space and the plugin's auxiliary structures; build the text index; store the supplied hierarchy with per-node membership bitmaps, bounding boxes and per-segment row ranges; compute per-node term distributions and, if §7.9 applies, the term × tile count matrix; store supplied labels with their generating sets; store per-item vocabulary vectors; write to a new immutable prefix and flip the pointer.

Ingest runs a strict subset: resolve and intern terms, derive the required set and route to a partition, quantise coordinates, **derive identities, then order the batch by `(morton, tessera_id)` and project priorities from it** *(r21; same inversion)*, emit a segment with its tile table and candidate lists, append to the term and text indexes.

Parquet remains the archival and interchange format. All serving artifacts are derived and deterministically rebuildable.

## 15. Rejected approaches

Rationale lives at the referenced section; the entry exists so the decision is visibly made rather than overlooked.

- *Static tiles from a CDN with client-side filtering* — violates **I1**; ruled out by §3's requirement.
- *A precomputed global LOD sample intersected with the mask* — violates **I7** (§7.2). Distinct from the candidate lists, which are a fast path over an exact definition with a terminating fallback.
- *Rank-position sampling with a bit-reversal sequence* — does not nest across zoom, and does not compose across partitions (§7.2, §12.3).
- *Global cluster geometry or labels gated on a coverage threshold* — violates **I2** (§7.5).
- *Composing filters into a single mask used for everything* — violates **I3** and **I12**; every label vanishes on the first keystroke (§8.1).
- *Independently-evaluated top-k filters intersected afterwards* — post-filtering by another name (§8.2).
- *Ranked text search using corpus-global term statistics* — a demonstrated inference channel (§8.3).
- *Indexing items under anything finer than the terms the auth function yields* — violates **I5**; permission-homogeneity is a property of the two functions agreeing, not of terms (§6.1, Appendix E).
- *Sending raw entity IDs to the client* — violates **I10** (§11.1, C6).
- *Assigning entity IDs in Morton order within a batch* — violates **I10** more sharply, by adding location to the leak (§11.1).
- *Adding newly ingested items to a generating set* — violates **I8** (§7.6).
- *Recycling entity IDs after deletion* — violates **I9**.
- *Gating a partition on satisfying any of its terms rather than all of its required set* — over-broad; the required set is the intersection across disjuncts and the gate is a necessary condition (§12.2).
- *Treating an unconsulted partition as vacuously satisfying containment* — violates **I13** (§12.3).
- *Rejecting items whose terms span compartments* — unnecessary under data-separation semantics; give the combination its own partition (§12.1, §12.2).
- *Delta application of credential changes* — exact removal needs a forward index to save a rebuild on a rare event (§6.4).
- *Polling an external system to refresh credentials* — superseded by the two-stage split (§2.2).
- *A separate quarantine bitmap alongside the overlay* — the overlay's *deny* disposition already covers it (§11.2).
- *Bloom filters or sketches on the authorisation path* — false positives are disqualifying.
- *Interleaving time into the Morton code* — wrong for discrete slices (§9).
- *Adopting a search engine, vector database, analytic database or relationship-based authorization service for the access-control layer* — the blocker is correctness before performance (Appendix D).
- *Database row-level security, even as a secondary safety net* — discloses excluded-row counts and invisible-cluster density through query plans, and abandons the spatial index (Appendix D).
- *Array-containment predicates over a per-item term list for the mask build* — ~10<sup>3</sup>× slower than a semi-join over an exploded pair relation, and the formulation everyone reaches for first (§6.3).
- *Inventing a label syntax* — the access-expression grammar exists, is hardened, and its restrictions are what make DNF normalisation terminate (Appendix E).

## 16. Open questions

**Term cardinality under the reference plugin.** How many distinct terms exist, how many per item, and what fraction of items carry more than one? These size the index, the ingest cap and §7.9's histogram additivity. Under the reference plugin they depend on DNF expansion, which published measurement puts as infeasible beyond nesting depth 2 for comparable workloads — so **measuring the expansion factor on real predicates comes first**; it is where the design fails if it fails (Appendix E).

**How spatially clustered is a typical mask?** **Measured (r16)** on the Phase 0 corpus (probes, results §5): essentially scattered — run ratio 1.03–1.15 for the most realistic principal shapes, 2.3–5.1 for topic-correlated families that the measurement itself flags as flattered upper bounds. Direct evaluation is the main selection route with measured duty cycles; candidate lists serve the dense cores of head principals only. The residual question — whether *real access labels* behave differently from these proxies — is **closed by owner decision (r18)**: no real access-labelled corpus is available to this project, so synthetic evidence is accepted as final, and the question converts to deployment guidance — any deployment with real labels re-runs the Phase 0 measurements before trusting the policy-dependent conclusions.

**Data separation or audience separation?** (§12.1.) **Resolved (r17):** data separation.

**How many compartments, and how many combinations occur?** Sets the fan-out bound and the per-partition overhead budget (§12.4).

**Token lifetime.** **Resolved (r17):** the default backstop is **one hour**, deployment-overridable; the caller's re-authorisation cadence remains the governing policy. At measured mask-build costs the extra authorise load is negligible, and the staleness bound tightens by an order of magnitude over the earlier 12-hour working assumption.

**Which generating set the caller declares** — **Resolved (r17):** the prompt sample (§7.8), recorded in manifest provenance.

**Retroactive revocation across slices (§9).** **Resolved (r17):** no — current credentials govern all slices; the shared-mask premise stands.

**Sharded index placement (§13.3).**

**Overflow item visibility (§6.2).** **Resolved (r16):** moot — exclusion is dropped; items are always indexed and over-bound term sets warn.

**Entity ID exhaustion.** Consumed IDs exceed live items without bound. At 10<sup>9</sup>, 2<sup>32</sup> headroom is thin, and columns, permutations and standard Roaring are all 32-bit. Shard-local u32 with a (partition, shard, offset) global ID is the likely answer.

*(r23)* **Deliberate ID slack was briefly a claimant on this budget and is no longer one — the entry survives as a standing rule rather than a live cost.** An arena scheme drafted for §11.1's per-batch limit would have bought a larger signature-sort scope by leaving never-issued holes in the ID space; it was superseded by group-commit allocation, which buys the same scope by batching the acknowledgement and so issues precisely what it allocates. The rule the episode leaves behind: **holes consume this budget exactly as issued IDs do**, and they additionally widen the permutation's `bound`, the external-ID locator and every streamed segment's own permutation — all sized on the high-water or on an entity range, never on live items. Any future proposal that spends ID space to buy contiguity must be costed here first and capped explicitly; the discarded scheme's own working figure was 1.3–1.5×, and it is recorded so that a later revival starts from a number rather than from optimism. The *index*-ordinal sketch in the implementation plan's §14 would relieve this entry from the other direction — an index space renumbered at compaction stays dense, so postings, memberships and the permutation stop paying the consumed-versus-live gap — but its safety argument does not yet close.

**Entity IDs are globally unique across §12 partitions; the identity's prefix is the §13.3 shard** *(r21)*. The `tessera_id` construction (contracts §2.6) encodes `(shard_id: u32, entity_id: u32)`. Three facts settle which discriminator that is, and they are recorded here because the question keeps being asked: the entity-ID high-water is a **single** bundle-level value with a **single** allocator, so two items in different partitions cannot share an ID; §12.4 fixes partition identity as *a canonical hash of the sorted required set*, because partitions are discovered rather than declared, and a content hash is not a dense small integer; and §12 partitions exist in the bundle format today while §13.3's row-range shards do not. A partition component in the identity input would therefore encode a constant, and could not be a `u32` in any case. `shard_id` is a **reserved field**, valued 0 for as long as §13.4 rules sharding premature.

What remains open is narrower, and is a *consequence* of the exhaustion entry above rather than of this construction: if a future multi-shard deployment allocates entity IDs **per shard**, the reserved prefix becomes load-bearing and the 32/32 split is exactly right; if it keeps allocating globally, the prefix stays 0 and the four bytes buy only the option. The encoding is the same either way, so nothing is blocked.

**Segment count tolerance.** How many live segments before per-tile range fan-out is noticeable?

**Vector serving.** Are source embeddings served at view time, and what is the expected selectivity of a typical filtered view (§8.3)?

**Slice count.** How many temporal slices must be simultaneously browsable?

## Appendix A — Sizing

Figures are quoted here and referenced, not restated, elsewhere. Per partition; a deployment's total is the sum across partitions.

At 10<sup>7</sup> items:

| Structure | Size |
|---|---|
| Dense mask fragment | 1.25 MB |
| Term index, ~10 postings/item | 200–400 MB |
| Term index, ~1 posting/item | 20–40 MB |
| Hot columns (18 B/row) | 180 MB |
| Coordinates only (2 × float32) | 80 MB |
| Permutation `entity_to_row` | 40 MB |
| Candidate lists, levels 0–6 at 4*k* = 128 | 2.8 MB |
| Retained auth data (per mask) | ~40 KB |
| Wire payload, 50 k points *(assumed k; P2 replaces)* | 0.6 MB |
| Source embeddings (768-d float32), if served | 31 GB |

At 10<sup>9</sup> items:

| Structure | Unsharded | Per shard at S = 64 |
|---|---|---|
| Dense mask | 125 MB | 2.0 MB |
| Term index, ~10 postings/item | ~20 GB | ~310 MB |
| Term index, ~1 posting/item | ~2 GB | ~31 MB |
| Hot columns | 18 GB | 281 MB |
| Coordinates only | 8 GB | 125 MB |
| Permutation `entity_to_row` | 4 GB | 62.5 MB |
| Candidate lists, levels 0–9 | 179 MB | ~3 MB |
| Source embeddings, if served | ~3 TB | separate store |

**Four columns, not five** *(r21)*. Contracts §2.6 r6 removes `node_id` (no reader before Phase 3 — the build wrote a billion identical sentinels into a per-viewport file) and replaces `entity_id` with the width-neutral `tessera_id`: 22 B/row → 18 B/row. The external-ID extents, which earlier revisions did not count because they were assumed cold, were in fact mapped and linearly scanned at open; r6 makes them a per-extent lazily-opened sidecar and they leave the residency table, at the cost of one extent joining it after the first drill-down.

**One direction only** *(r20; mechanism updated at r21)*. Earlier revisions listed "permutation arrays, both directions". Contracts §2.6 stores only `entity_to_row: u32 × bound`; the row→entity direction is **derived by inverting the `tessera_id` column** of `columns.arrow` *(r21; at r20 that column held the entity ID directly)* — either way it is not a second stored array. Counting it twice inflated the residency figure by 4 GB at 10<sup>9</sup>.

**The wire figure is an assumption, not a measurement** *(r20)*. 0.6 MB at 50 k points is 12 B/point arithmetic against an assumed mark budget, not an observed Arrow IPC payload. Probe P2 measures bytes on the wire, transfer time and JS decode time across the sweep and replaces this row with a measured figure at the calibrated *k*.

**The per-viewport scanned column widens 4×** *(r22)*. §7.2's implemented comparator reads the full `tessera_id` (8 B/row) rather than the `priority` prefix (2 B/row), so the column a viewport scans under direct evaluation goes from **2 GB to 8 GB at 10⁹**. The `priority` column is written and unread at query time. This is the price of not building the prefix-scan-then-fall-through path — deliberate, since the obviously-correct construction is preferred to the fast one, and reversible under §7.2's own `w ≈ log₂(V_max/k)` trigger. Any residency table that lists `priority` as the per-viewport scanned structure should read `tessera_id` instead.

**The §7.3 underlay is a per-request cost with a required cap** *(r22)*. Sub-cell counts add ~`tiles × 4^s` range cardinalities per request, at 16 B per emitted cell on the wire. At *s* = 4 over ~300 tiles that is ~77k cardinalities and ~1.2 MB — larger than the points payload and, against probes' 0.1–0.3 ms for ~300 whole-viewport counts, tens of milliseconds against a 10 ms p99 budget. Hence the underlay is opt-in per request and the total cell count is capped; both bounds are stated in §7.3 rather than left to deployment.

Mask construction costs tens of milliseconds at 10<sup>7</sup> and seconds at 10<sup>9</sup> before sharding parallelises it.

## Appendix B — Glossary

Definitions only; the arguments are in the referenced sections.

**Roaring bitmap.** A set of integers over a bounded universe, split into blocks of 2<sup>16</sup>, each block independently encoded as a sorted 16-bit array, a literal bit array or run-length pairs. `rank` and `select` are *not* constant-time — both cost O(blocks before the position) — which is why §10.4 batches selections from a computed base rank. Scatter costs run containers, not correctness: a fully scattered mask falls back to fixed-size bitmap containers, and `range_cardinality` cost depends on the range's span rather than the mask's density.

**Space-filling curve.** A numbering of 2D grid cells such that cells with nearby numbers tend to be nearby in the plane, so a box query becomes a handful of contiguous ranges rather than a strip. See §5.2.

**Morton / Z-order code.** Bit-interleaving of integer coordinates; x = 6 = `110` and y = 3 = `011` gives `01 11 10` = 30. Each bit pair selects a quadrant, so the first 2*k* bits identify the quadtree cell at level *k*.

**Priority sampling.** Assigning each item a fixed pseudo-random value and taking the *k* lowest within a region. Nested across containment and composable across partitions. See §7.2, §12.3.

**Term.** An opaque identifier that is simultaneously a property of items and of principals: an item carries a set of them, a token carries a set of them, and visibility is intersection. Permission-homogeneous by **I5**.

**Monotone boolean formula.** Built from AND and OR without negation, so making any input true can only make the output true. Rewritable as a disjunction of conjunctions, possibly with exponential expansion. See Appendix E.

**Boolean expression indexing.** Indexing a collection of boolean predicates and, given one assignment, finding every predicate it satisfies. Its *conjunction* is this document's term.

**LSM segments.** New data forms its own small sorted segment; reads merge across segments; a background process compacts. See §11.3.

**Capability.** An opaque handle conferring a specific precomputed authority, rather than identifying a principal whose authority is looked up per request. See §2.3.

**Partition.** A set of items sharing a required term set, stored and served separately for isolation rather than for scale. See §12.

## Appendix C — Accepted residual disclosure channels

I2 requires displayed quantities to derive from visible data only. These are the known exceptions. Anything not listed is a bug, not a trade-off.

| # | Channel | What leaks | Severity | Mitigation | Status |
|---|---|---|---|---|---|
| C1 | Node membership derives from global density | That the principal's visible items in a region group together — a fact about structure including unseen items | Low | `min_visible_members` against `M_auth` bounds how finely this is exposed; filtering cannot deepen it (**I12**) | Accepted |
| C2 | Node extent and hull shape | Recomputed per user from masked members only — no leak by construction; listed to record that it was checked | None | — | Closed |
| C3 | Label existence | A principal learns only of labels they satisfy | None | Response omits all unsatisfied candidates (§10.6) | Closed |
| C4 | Response timing | Viewport service time correlates weakly with unauthorised items scanned during candidate-list descent | Low | Unmitigated; quantify before treating as acceptable | Open |
| C5 | Extractive-tier background frequencies | Corpus-wide term distributions, if drawn from the live corpus | Low | Fixed public reference corpus (§7.7) | Closed |
| C6 | External ID gaps on the wire *(r21; was "Entity ID gaps on the wire")* | Where the caller's external IDs carry structure (sequential keys, ingest-ordered surrogates), the gap between two visible IDs is a count of unauthorised items | Medium | `tessera_id` is a keyed permutation of entity space and carries no order, so it discloses nothing; a caller who supplies structured external IDs and exports them is choosing that disclosure. Entity IDs still never cross the boundary (**I10**) | Accepted — caller's control |
| C7 | Generating-set shrinking under deletion | A label reflecting content the principal may never have been entitled to | Medium | Not adopted; caller regenerates (§7.6) | Not adopted |
| C8 | Pre-intersection filter cardinality | A raw match count is a corpus-wide count over unauthorised records | High if exposed | Not exposed on un-intersected intermediates (§8.2) | Closed by construction |
| C9 | Text relevance scores and ranks | Corpus-global statistics allow inference of unreadable content | High if ranking added | Boolean filtering only (§8.3) | Closed by scope |
| C10 | Vector similarity results and thresholds | Post-filtered neighbours vary observably with invisible items | High if post-filtered | Threshold filters with candidate push-down (§8.2) | Closed by construction |
| C11 | Label filter vocabulary | Offering a filterable label reveals a label the principal cannot see | Medium | Vocabulary containment-filtered against `M_auth` (§8.3) | Closed |
| C12 | Caller-declared generating sets | A label supplied with an optimistic generating set is a disclosure the service will faithfully serve | High | Contract requirement (§2.4); provenance is unverifiable by the service | Accepted — caller's control |
| C13 | Cross-partition node metadata | A node whose members lie wholly inside a compartment reveals, by existing and having an extent, that something is there | Medium | Node metadata held per partition (§12.3); C1 crossing a physical boundary | Closed |
| C14 | Partition fan-out width | Query latency correlates with how many compartments a token reaches | Low | Unmitigated; the principal already knows their own clearances, so this reveals nothing about data | Accepted |
| C15 | Session pin rate of change | Corpus-wide ingest and compaction activity, visible in how often a viewer's pin identifier changes — activity, not content, and including activity on data the viewer cannot see | Low | Pin values are per-session scrambled so no cross-session correlation; the rate itself remains observable. C14-like in character | Accepted |
| C16 | Router-held label presence registry | That some label draws on a given compartment — held by the routing process, which already routes queries into that compartment | Low | Registry restricted to label IDs and required-set hashes: no text, no entity IDs, no cardinalities. Required for the I13 merge (a router ignorant of an unreachable slice serves labels it must withhold) | Accepted |
| C17 *(r21)* | Stable wire identity across sessions and principals | Existence-over-time probing on a held `tessera_id` (visible → 404 is a timestamped delete/suppress/grant-change signal); and cross-principal correlation, since two principals see the same identifier for the same item and can join views out of band | Medium | **This is the intended trade of the r21 mechanism change**, not a residual: a stable identifier is what lets a client bookmark, share and reconcile a point across sessions, and per-session handles bought their unlinkability by making all three impossible. Both channels are bounded to items the probing principal **already sees** — `tessera_id` is order-free, so neither yields entity space, a count of what is hidden, or anything about an item never visible to that principal (**I2** unaffected). The identity **epoch** bounds it further in time | Accepted — the point of the r21 mechanism change |

**C4, annotated** *(r21)*. `/v1/items/{tessera_id}` returns an identical `404` for "no such ID" and "exists but not visible" — same status, code and detail, with no branch-dependent logging or metrics. The **timing** channel on that endpoint is closed structurally rather than narrowed: inversion of a `tessera_id` is a pure function taking no I/O, and the visibility test that follows it is an **entity-space** question — `fragment.contains(entity)`, adjusted by the overlay's `deleted > suppressed > evaluate_terms` precedence and the ingest buffer, exactly as **I1**'s composition over §11.2's overlay resolves it per entity *(the drafting plan cited "§5.2" here, which is Morton ranking; corrected)*. That is O(1), touches no row-space projection, and performs **identical work for an identifier that names nothing and one that names an invisible item**: both take the same three constant-time lookups and return the same `404`. The endpoint therefore has no per-ID cost to correlate against. The earlier row-space formulation — project the fragment, then test the row — would have paid a 9.5–19.3 s projection build for a known-but-invisible ID and nothing at all for an unknown one: a per-click existence oracle four orders of magnitude wide. C4 itself remains `Open` for the viewport path.

| C18 *(r22)* | Mark count and sub-cell counts track the masked visible count | §7.2's threshold clause makes a tile's mark count ≈ `θ_d·n` in the viewer's own visible count, and §7.3's underlay reports exact masked counts at a finer grain than the tile | Low | **No-op, and the argument is that both quantities are already disclosed exactly.** §7.1 returns the exact masked count of any tile at any zoom, so mark count is a coarser view of a number the same response already carries in full, and a depth-*(d+s)* sub-cell count is *precisely* what a `zoom = d+s` request already returns — the underlay saves a round-trip rather than revealing anything. Omitting empty sub-cells conveys `count == 0`, itself a masked count, exactly as the existing empty-tile skip does. Differencing across zooms or pans yields only differences of masked counts. θ's anchor is the **composed** visible total, so no pre-overlay quantity is exposed (§7.2) | Accepted — no new channel |
| C19 *(r22; widened 2026-07-31, owner-approved)* | Per-tile selection work varies with the viewer's own visible count | §7.2's evaluation skips the counting pass for a tile the definition provably serves whole (`V ≤ min(k_min, cap)`, or θ saturated with `V ≤ cap`), so per-tile work varies with the viewer's own `V` and θ. **A private implementation detail, not a selectable route** — both branches return the identical served set, so nothing about the *answer* varies. **Widening (2026-07-31, landed with the three-tier adaptive decode):** the decode mechanism is additionally chosen per tile from three tiers gated on `(visible, range.len())` — both quantities the viewer already holds exactly (§7.1 discloses per-tile `visible`; the tile grid is public) — so the tier-choice timing variance reveals nothing beyond the response body. The residual is the decode-source choice (an empty overlay diff walks the cached projection directly), whose timing reflects whether any accepted change touches the session's own mask — within this entry's C4/C14 shape, since §7.1's counts already disclose those changes' effects exactly | Low | A widening of **C4**'s shape rather than a new channel: work correlates with the principal's *own* coverage, which is C14's accepted reasoning — the principal already knows its own clearances. Measured worth: ~30% of selection cost on the tiles it covers, 75 µs per viewport at `cap = 30` rising to 2.7 ms at `cap = 1000` (`crates/tessera-engine/examples/route_saving.rs`) | Accepted — C4/C14 shape |

**A row deliberately not added, recorded so it is not read as an oversight.** The density work considered *truncating* the served set to whatever a fixed-width candidate list happened to yield, rather than falling back to direct evaluation when the list under-delivers. That is **not** implemented and must not be: the drawn count would then depend on *unmasked* tile density, which is both a mild I7 regression — a partial-coverage viewer on a dense tile is under-served relative to the definition, the same failure mode as sample-then-filter in attenuated form — and a genuine new channel needing its own entry. If it is ever revisited, it is not a no-op.

Owner and review date for C1, C4, C6, C12, C14, C15, C16, C17, C18 and C19 to be assigned before launch.

## Appendix D — Prior art and provenance

An external survey covering search engines, large-scale scatterplot systems, databases, and authorization and disclosure-control literature reached three conclusions.

**No existing technology can replace this build.** Every mature system rebuilds the principal's selection on every query, because that is the only model a stateless query engine offers; and every large-scale scatterplot fixes its sample before any user exists. More decisively, systems that do have per-document security document that they permit aggregate counts over unauthorised records. Relationship-based authorization services measure their reverse-index operation in hundreds of milliseconds at a few hundred thousand relationships, and the most sophisticated implementation solves the problem by becoming a change-data-capture feed into a materialised store the consumer builds — which is this architecture.

**What is genuinely novel is narrow.** Two things: the *composition* of exact bitmap-derived per-tile counts with post-mask sampling and cross-zoom nesting; and the application of containment gating to *shared, precomputed, generated* summaries served to differently-cleared viewers. The second is the stronger claim.

**Most of the pieces have established names**, and using them saves explanation:

| This document | Established name |
|---|---|
| Label served iff generating set ⊆ visible set | Conservative label join / derivation axiom; high-water mark; conservative taint propagation |
| Compartments as conjunctive prerequisites over category sets | Compartmented / lattice-based mandatory access control; dominance |
| Per-item boolean predicate over tokens, no NOT | Security label; specifically an **access expression** (Accumulo `ColumnVisibility` / `accumulo-access`) |
| The data plugin's normalisation to terms | Boolean expression indexing |
| The auth plugin's job | Partial evaluation producing residual policy in DNF |
| Refusing rather than silently filtering | Non-Truman model |
| Filtering results to caller ACLs | Security trimming (pre- vs post-trimming) |
| Morton sort as clustered index plus range skip structure | Index sorting with a doc-values skip index |
| Priority | Per-point zoom-independent retention scalar |
| Precomputed accessible-set materialisation | Leopard-style index |
| Token conferring precomputed authority | Capability |

Three findings changed the design rather than merely naming it. Published measurement of DNF expansion elevated §16's first item. The information-flow literature's **label creep** is the named, predicted failure mode of §7.8. And the demonstration that post-filtering leaks unreadable-document content through relevance scores and rank shifts is why §8.3 filters rather than ranks — a 2005 result that no contemporary retrieval-augmented-generation work appears to cite.

**Two later findings did the same.** The survey's one system whose default read path satisfies §3's requirement achieves it structurally: every user-configured iterator, including any aggregating one, is stacked above the visibility filter and cannot observe a cell it excludes — which is why it has no equivalent of the filter-versus-aggregation divergence that leaks in search engines. That ordering discipline is now §10.4's structural rule and the second sentence of **I2**. Separately, measurement across three engines found array-containment predicates roughly three orders of magnitude slower than a semi-join over an exploded pair relation, which fixed §6.3's schema.

**A negative result worth recording**, because it will be proposed as a cheap safety net: database row-level security is not one. A fully patched instance of the most-studied implementation discloses the exact count of policy-excluded rows through its query-plan output, and discloses the density of a cluster the principal cannot see at three orders of magnitude above background — because planner statistics are computed over all rows while the comparison operators in a viewport query are marked leak-free, so the mechanism that gates this for other operators does not apply. It also abandons the spatial index, costing three orders of magnitude in latency. It is worse than not being there.

One property worth claiming explicitly: because the decision to withhold a label is a function only of the generating set and the token's own mask, the refusal itself carries no information about invisible data. Most suppression schemes lack this, and it makes the differencing literature inapplicable by construction.

Full reviews, with sources, are held alongside this document.

## Appendix E — Reference authorisation plugin

One implementation of §6.1's two functions, and the model this design was originally built around. Recorded because it is the intended first deployment and because its cost characteristics drive §16, but nothing in the core depends on it.

**The model.** Categories are drawn from five dimensions: one conjunctive dimension carrying 0–5 categories per item, three small disjunctive dimensions, and one disjunctive dimension with a vocabulary of 10<sup>5</sup>–10<sup>6</sup> categories of which any item names only a few. A **unit** is a conjunction of five clauses, one per dimension, under each dimension's declared combination semantics. An item's predicate is an arbitrary monotone nesting of AND and OR over units; normalising to disjunctive normal form yields the **terms** the core indexes — each a single unit or an interned conjunction of units.

**Label syntax is adopted, not invented.** Item predicates are written as **access expressions** in the `accumulo-access` grammar: tokens combined with `&` and `|`, parentheses for grouping, **no negation**, and no mixing of `&` and `|` at one level without parentheses. Both restrictions are load-bearing rather than stylistic — absent negation the predicate is monotone, so DNF normalisation terminates and the term index is sound; and forbidding unparenthesised mixed operators removes the precedence ambiguity that would otherwise let two implementations disagree about what a label means, which is **I5** violated at the syntax layer before either function runs. The grammar is a ten-line ABNF and is worth reimplementing natively rather than taking a JVM dependency, with the reference implementation used as a differential-test oracle in CI (§6.1). Adopting it verbatim also makes existing corpora written in this syntax ingestible unchanged.

**Combination semantics are declared per dimension, as data:**

| Dimension | Vocabulary | Categories per item | Semantics |
|---|---|---|---|
| Conjunctive | to be confirmed | 0–5 | **AND-within**: principal must hold *every* listed category |
| Small (×3) | small | few | OR-within: principal must hold *any* listed category |
| Large | 10<sup>5</sup>–10<sup>6</sup> | few | OR-within |

All five combine with AND *across* dimensions. Applying a default OR-within to the conjunctive dimension yields a predicate **more permissive than policy intends** — a silent disclosure. Assert at build that every dimension carries explicit declared semantics and that no code path supplies a default.

Compartments (§12) map naturally onto the conjunctive dimension, whose semantics already are "must hold every one of these".

**Consistency (I5) reduces to one rule in this implementation:** an item is indexed under its DNF terms and nothing finer. A disjunct that is a conjunction of units must be interned as a term in its own right, and the item must **not** appear in the postings of its constituent units — otherwise an item requiring U₁ ∧ U₂ becomes visible to a principal holding U₁ alone, every generating set built on it silently becomes unsound, and the containment test still returns true.

**Auth-side structures.** The auth function keeps a **clause index** mapping each (dimension, category) pair to a bitmap of the units whose clause in that dimension references it — over unit IDs, so kilobytes rather than megabytes — and **referenced-category sets** recording which categories appear in at least one unit clause.

Evaluation proceeds in two stages. *First*, intersect the presented categories with the referenced-category set per dimension: the large dimension has a vast vocabulary but each unit names only a few of its categories, so most of a 10<sup>4</sup>-category input references nothing. This single intersection turns the stage from ten thousand bitmap operations into a few hundred. *Then*, per disjunctive dimension, union the clause-index bitmaps for the survivors; per conjunctive dimension, use **k-of-N counting** — a `u8` counter per unit, incremented per held category, satisfied where the counter equals the clause cardinality, with units having an empty conjunctive clause precomputed as vacuously satisfied. Intersect across the five dimensions to get satisfied units, then resolve satisfied terms: single-unit terms directly, conjunctive terms by subset test in unit space, indexed under their rarest member unit.

**Canonicalisation.** A unit is canonicalised by sorting each clause's categories and hashing the five sorted lists; a term by sorting its member unit IDs and hashing. These hashes are the descriptors the core interns.

**The risk this implementation carries.** DNF normalisation can expand exponentially, and this is measured rather than theoretical: published work on boolean expression indexing found it infeasible beyond nesting depth 2 (Appendix D). Clause widths do *not* contribute, since units are atomic in the term-level DNF; the risk is entirely how deeply the caller nests units. The mitigation is plugin-side: mint synthetic terms for deep subexpressions, which keeps terms per item linear in expression size at any nesting depth (probes, results §1) — a conservation law trading item-side breadth for auth-side breadth, and measurement says to prefer the auth side, which is cheap. The core's runaway warn (§6.2) is the tripwire, not a gate.

## Appendix F — Prospective extension: valid-time filtering

Not part of the committed design. Recorded because the change turns out to be small and the reasoning is easy to get wrong.

**The model.** Each item carries a quad of instants — earliest possible start, latest possible start, earliest possible end, latest possible end — describing an event whose true interval is known only to lie within those bounds. An event known to have happened during 2016 is `[2016-01-01T00:00, 2016-12-31T23:59, 2016-01-01T00:00, 2016-12-31T23:59]`.

**Vocabulary.** Adopting this requires renaming what §9 calls temporal slices, because the two are orthogonal — a user may view the current snapshot and filter to events in 2016. These are the standard bitemporal axes: §9's slices are **transaction time**, the quad is **valid time**. Fix the terms before the extension lands.

**Two semantics, and the default.** Against a query interval [qs, qe]: *possibly overlaps* means some placement consistent with the quad overlaps the query — **this is the default**, since a user filtering to a period wants everything that might fall in it. *Definitely overlaps* means every consistent placement does.

**Canonicalise at ingest, and the reason why.** Both semantics reduce to two comparisons, but the obvious reduction is wrong because it ignores that a start cannot follow its own end. Consider `[0, 1, 0, 0]`: the only consistent placement is (0, 0), so a query at instant 0 *definitely* overlaps — yet a naive test on `LPS ≤ qe` rejects it. The bounds as supplied are not all reachable. Tighten them once:

```
LPS' = min(LPS, LPE)        # the latest start that admits a consistent end
EPE' = max(EPE, EPS)        # the earliest end that admits a consistent start
```

With that, the query-time predicates are exactly the naive ones:

```
possibly  overlaps  ⟺  EPS  ≤ qe  ∧  LPE ≥ qs      # overlap on the outer hull
definitely overlaps ⟺  LPS' ≤ qe  ∧  EPE' ≥ qs
```

Both were checked exhaustively against explicit quantification over all well-formed quads and queries on a small domain; zero mismatches. Without the canonicalisation the definite form fails on a large fraction of degenerate quads.

**Index the outer hull only.** Since definite ⊆ possible, build one index over `[EPS, LPE]` and evaluate the stricter form, when asked for, as a comparison pass over the survivors — by then already intersected with `M_auth` and therefore few. Same shape as the vector filter's brute-force-over-candidates (§8.3).

**The index is a segment tree of bitmaps**: an interval is stored at O(log n) canonical nodes, a query decomposes into O(log n) nodes, and the result is the union of their bitmaps. Structurally identical to the cluster hierarchy, and it plugs in as one more operand under the §8.2 contract.

**Storage goes in its own column group.** Four `u32` instants is 16 bytes per row: 160 MB at 10<sup>7</sup>, but **16 GB at 10<sup>9</sup>, two-thirds the size of the entire hot column set**. Load it for the refinement pass and for display only.

**Do not fold time into the Morton code.** The spatial layout is the *mandatory* query axis and the temporal filter is *optional*, so a 3D interleave would degrade the query everyone makes to accelerate one only some make — and 2D tiles would stop being contiguous row ranges.

**What does not change.** Labels remain visible under temporal filtering, because time lands in `M_sel` rather than `M_auth`. No new leak class: C8 already forbids exposing pre-intersection cardinality for any filter. No invariant changes, and no change to masks, terms, labels, level of detail, storage layout or the retrieval surface — the §8.2 extension discipline working as intended.

**One consequence of the default to watch.** Under *possible*, an item with very wide uncertainty matches almost every query and becomes noise. Consider styling marks by uncertainty width, or offering the definite form as a secondary control.

## Appendix G — Revision history

- **r24** — **§7.2's θ anchor made explicit, from a conformance finding** (owner-delegated, 2026-07-31). Phase 2's Track T transcribed §7.2 into the reference oracle as a *literal definition* — the point of writing it that way — and the transcription exposed that `V_total` is ambiguous as written: "the viewer's own composed visible total over the slice" admits both `|M_auth|` and `|M_auth ∩ rows(slice)|`. **This is a clarification, not a decision**: r23's own §11.2 already settled it in general terms ("an entity with no row contributes to no count whatever `L` says — flush is the visibility mechanism"), and r23 simply did not propagate that rule into §7.2, which predates it by one revision. Both the engine and the oracle already implement the row-space reading; they agree with each other and with §11.2, and disagree only with the sentence. What made the gap worth closing now rather than at 2.4 is that group-commit allocation (r23, lifecycle §5.1) makes the divergent state **routine** rather than transient — a batch is acknowledged, and so in `M_auth`, for a whole commit window before flush gives it rows — so under the entity-space reading, accepting a batch would move θ and with it the mark count in every tile of every viewer's map, for items nobody can draw. The second half of the finding is answered rather than amended: §7.2's "θ never depends on `bbox` or `zoom`" and its per-slice anchor were read as contradictory once more than one slice exists, and they are not — a slice is named by the request and is neither `bbox` nor `zoom`, so a session has one θ per slice while pan- and zoom-invariance, which is what the churn argument actually needs, is untouched. Stated in §7.2 so a second implementer cannot reach the other reading. **No invariant changes, no format changes, no behaviour changes, and Appendix C is unchanged** — this revision brings a sentence into line with the code and with §11.2, and nothing else. **Two further findings from the same transcription are settled in the same paragraph, on the same delegation**, since both are properties of `P_0` rather than separate decisions: **`P_0` floors**, and **`P_0` saturates when `V_total = 0`**. Neither was stated; both are observable through the differential's exact-equality comparison; and the engine and oracle agreed on both only by coincidence of implementation — the precise condition under which a second implementer written from this document would diverge. Floor is chosen on merit as well as incumbency (a smaller `P_0` is a stricter threshold, so it errs toward fewer marks, never more, with the floor clause guaranteeing non-emptiness regardless); the zero case is unobservable in effect but must still be specified, because an implementation caching θ per session has to compute something.

**One finding from the transcription is left open**, because it is a contracts change on a different surface rather than a §7.2 clarification: contracts §3.2 publishes `k_max_marks` on `/v1/meta` but not `max_k`, so a client cannot learn its own request bound and an independent implementation cannot distinguish a cap refusal from a machine-ceiling one — which matters precisely because this section insists the two ceilings are deliberately not the same knob.
- **r23** — **Two corrections to what this document claimed about ingest**, found by auditing the Phase 1 implementation against the corpus before Phase 2's streaming path is designed on top of it. Both are corrections rather than additions, and neither changes an invariant in substance. **First, §11.1's signature-sorted assignment is scoped to one batch and nothing repairs it within the identity guarantee** — §11.3's compaction leaves the entity axis untouched, and §5.1's stability means a rebuild re-sorts only by breaking identity (plan §14's escape hatch, §12.5's precedent). So the promised compression is collected only in proportion to how much of the corpus arrives in large batches, and the uncollected part is permanent. §11.1 gains the container model for *what is collectable* — `max(1, 2¹⁶/(p·B))` — with the three consequences that make a single corpus-wide multiplier the wrong instrument: at request-sized batches there is nothing to collect for **any** term (`p·B < 2¹⁶` holds for every `p ≤ 1` once `B ≲ 6·10⁴`), most terms have no benefit to give (34.4% singletons, median 3 postings), and the ~130× figure bounds what contiguity is worth *between label configurations at differing grant widths* rather than sizing this lever. It also corrects how the corpus's own numbers must be read: **the probe corpus assigns entity IDs in created order and §4.4 measured signature-sorting on bytes alone, so every published union timing — the 588 ms worst case included — is already an un-banked measurement, and multiplying one by a decay factor double-counts.** What is at stake is a gain never collected, not a regression from a measured baseline. **And it names the fix, which a first drafting of this revision wrongly argued was unavailable.** That drafting claimed "allocation time is immovable" from contracts §3.4's per-row `tessera_id` ack — reading *before the acknowledgement* as *on arrival*, and contradicting §3's own seconds-to-minutes write budget in the process. §3 is now explicit that the budget covers the whole write path, deny dispositions included (owner decision, 2026-07-30), subject to one rule: **a deny's acknowledgement stays coupled to its application**, so nothing is ever acknowledged that is not yet in force. With the acknowledgement free to wait, **group-commit allocation** makes the signature-sort scope the commit window rather than the request — at the server, not by client convention — with no ID slack, no wire change and no ordering change. The lifecycle design §5.1 carries it; it supersedes an arena sketch that an adversarial review found to have two unresolved holes, and §16's slack entry is demoted to a standing rule accordingly. What remains beyond batching's reach is recorded in the implementation plan's §14 as a sketch whose safety argument does *not* close (an ι-keyed overlay carried forward verbatim across a renumbering compaction is fail-open), and which recedes further now that group commit exists. **I10's dense-and-signature-ordered clause is qualified** to say per-batch and only per-batch; as written it read as a global property, which §11.1 never claimed. §16's exhaustion entry gains ID slack as a knowing claimant on the u32 budget, with the 1.3–1.5× cap the arena mechanism carries. **Second, §11.2 gains the distinction between membership of `L` and visibility**: `L` answers *may this principal see it*, every viewer verb answers a question in row space, and an entity with no row contributes to no count whatever `L` says — so **flush is the visibility mechanism, not a compaction convenience**, and an acknowledgement without one is a durability receipt rather than a visibility promise. Companion amendments: system architecture §6.4 (the same correction), §6.6 (batch-into-existing recorded as an open question), contracts §3.4 (a fragmentation metric on `/control/status`, which is what makes any later trigger observable), lifecycle §5.1 (the arena sketch and its two open holes), plan §14 (the index-ordinal sketch and where its safety argument fails). **Neither correction alters what crosses the boundary, and Appendix C is unchanged** — but §11.2 raises one item against it for the owner rather than settling it: while ingest is buffered and unflushed, drill-down acquires a third arm (visible in entity space, no row) that reaches C4's `404` by a longer path than either arm that annotation contemplates. Outcome indistinguishability holds; the *work* indistinguishability C4's structural closure claims does not, for as long as that state exists, and it disappears when flush lands. Whether that warrants a C4 amendment is the owner's call.
- **r22** — **Density-dependent selection** (owner decisions, 2026-07-30), implemented rather than merely specified. §7.2's definition changes from a fixed-size bottom-*k* sketch to **floor ∪ threshold ∪ cap**: a floor of *k*<sub>min</sub> (the I7 guarantee, unchanged in purpose from the old rule at a smaller budget), a Bernoulli threshold at a per-depth θ that makes mark count proportional to the viewer's own visible count, and a cap. **The reframing is the point:** a bottom-*k* sketch is fixed-size by construction, so its size could not carry density — every tile with at least *k* visible items drew exactly *k* marks, and §7.3's complaint that "twelve visible and four million render identically" was one its own remedy could not answer. §7.3's *k*-by-count lever is **struck as unsound**, not qualified: nesting needs *k*(child) ≥ *k*(parent) and a child holds a quarter of its parent's count, so *k* ∝ count inverts the requirement and reintroduces the popping failure the bit-reversal note records. θ is anchored closed-form from the viewer's **composed** visible total and progresses ×4 per depth, which makes the per-tile expectation depth-stable and θ monotone — and it is viewport-*invariant*, so it does not move on a pan. **The anchor's provenance is an I2 requirement, not a nicety:** anchoring on the cached row projection instead would let a viewer aggregate mark counts, solve for the anchor, difference it against its own summed per-tile `visible`, and estimate **how many of its own items had been denied** — a count of items outside `M_auth`. Recorded as accepted residuals: the closed form assumes items spread over 4^d occupied tiles, so clustered corpora pin at the cap over a bounded middle band of depths (the owner chose this over both a measured per-session anchor and a client-supplied θ; §9 already accepted cap-flat regions and §7.3's underlay backstops them); and **fewer marks than the old flat *k* is the intent** — "constant *k* hides the actual density of cells" — while emptiness is not, and the floor prevents it. **Nesting gains an explicit premise:** it holds for a fixed cap, and since `cap = min(k, K_max)` with *K*<sub>max</sub> a server constant, a client that *reduces k* on zoom-in forfeits it; *k* must be non-decreasing on descent, recorded in contracts §3 as a client obligation the engine cannot enforce. *K*<sub>max</sub> is an **overplot** ceiling and deliberately not the machine ceiling the drawn-mark probes calibrate. §7.3 gains the log-ramped underlay as a first-class mechanism with two load-bearing bounds (a total sub-cell cap, and refuse-rather-than-clamp, since a Morton prefix carries no depth of its own) and one recorded gap (the deep-zoom fade-out rule). §7.2's multi-segment clause is **corrected**: proportional allocation of *k* across segments is not the bottom-*m* of the union; sum `C_θ` and serve the global bottom-*m*. §12.3's composition claim survives with the added requirement that θ's anchor be session-global across partitions. Appendix A records the **4× widening of the per-viewport scanned column** (`priority` 2 B/row → `tessera_id` 8 B/row, 2 GB → 8 GB at 10⁹) as the recorded price of keeping the obviously-correct comparator, reversible under §7.2's own trigger, plus the underlay's per-request cost. Appendix C gains **C18** (mark and sub-cell counts track the masked count — a no-op, because §7.1 already discloses both exactly and a sub-cell count is what a deeper zoom already returns) and **C19** (per-tile selection route, a C4/C14-shaped widening), and records the truncation variant that was **not** implemented and would not be a no-op. No invariant changes; no bundle format changes.
- **r21** — The boundary identity (owner decision, 2026-07-29), companion to the contracts spec's r6. **I10's mechanism clause changes and its substance does not**: clients receive an opaque `tessera_id` — a keyed permutation of `(shard_id, entity_id)` under a per-deployment key — instead of a per-session handle. §2.6 step 10 and §10.6 amended to match; the handle mechanism is retained for Phase 3's node handles. Entity IDs still never cross the boundary, and after r6 no request-path artifact stores one at all, so §11.1's signature-sorted assignment is protected structurally rather than by a serialisation-time discipline. **Two corrections rather than additions, flagged as such:** I10's own text said entity IDs are assigned in *"ingest order"*, which §11.1 has always contradicted — the order within a batch is **term-signature** order, and the sentence is corrected, not merely reworded; and §5.3's hot-column list, which the drafting plan did not enumerate, still named the entity ID and the node column and would otherwise have contradicted contracts r6. Appendix C's **C6 moves from `Closed` to `Accepted — caller's control`**: the entry claimed closure by handles, and the residual disclosure is now a caller's choice to use structured external IDs and export them — C12's shape, register hygiene rather than a new exposure. **A new C17** records what retiring the handle costs and accepts it: a stable identity is linkable across sessions (existence-over-time probing on a held ID) and across principals (out-of-band correlation), both bounded to items the principal already sees, and both the *point* of the mechanism change rather than residuals; §10.6 cross-references it. **C4 annotated** with a structural closure of the `/v1/items` timing channel — the endpoint's visibility test is an entity-space question answered in O(1) with identical work for an unknown identifier and an invisible one, so the channel is closed rather than narrowed. `tessera_id` is a **transport** identifier: stable across rebuilds, not across §12.5's repartitioning, which advances an identity **epoch** at §10.2's prefix flip; consumers persist `external_id`. Appendix A's hot-column row corrected to 18 B/row; the external-ID extents leave the residency table. §10.3 records the **routing principle** (per-mark column, per-query bitmap, per-interaction sidecar), the deliberate hot-column trade, and that the per-interaction row and §8.3's vector sidecar are **one slot** whose first occupant — the external-ID store — is explicitly transitional, with the note that **Appendix D bars adoption for the access-control layer and not for a cold store off the request path**. §16 records that entity IDs are globally unique across §12 partitions and that the identity's prefix is the §13.3 shard, with the narrowed residual. No invariant changes in substance.

  **`priority` is redefined as the high 16 bits of the item's `tessera_id`** (owner decision, 2026-07-30), and the storage sort order becomes **`(morton, tessera_id)`** with no further tiebreak. Same column, same `u16`, **zero bytes changed**. Two defects are repaired. §7.2's sample was resolvable only while V ≤ 2¹⁶·*k* — V ≈ 2×10⁶ at *k*=30 — and above that threshold the tiebreak *was* the sampler; the tiebreak was the entity ID, which §11.1 assigns in signature order, so **the sample was ordered by permission signature**, keeping I7's letter and breaking its purpose, at the default overview, for head principals, with candidate lists inheriting it. And §12.3's composition argument had quietly lapsed: under shard-local `u32` entity IDs `splitmix64(entity_id)` was no longer the global per-item property the argument names. A keyed bijection over 2⁶⁴ is global, uniform and uncorrelated with signature, and because the `u16` is a *prefix* of it, "*k* lowest by priority then by `tessera_id`" is identically "*k* lowest by `tessera_id`" — so prefix width becomes a performance knob only. §5.2, §7.2, §10.3, §12.3 and §14 state the order; §14 and the build sequence **invert**, since the identity must now be derived *before* the tiler rather than written at the row after it. Consequences recorded rather than hidden: row order is now **key-dependent**, so a key rotation reorders tied rows as well as invalidating identifiers; the sample reshuffles on a re-key as well as on a reshard; and the uniformity of the Feistel's high bits under structured inputs is **taken as already established** (owner ruling), not assumed. The identity swap's viewer-plane prohibition on `priority` (contracts r6) is **retired by argument**: 16 bits of a keyed identity the payload already carries in full discloses nothing, since the cut *P* is determined by *k* and the masked count §7.1 already gives. No leak-register entry is required.
- **r20** — Corrections owed to the corpus by the drawn-mark budget spec (2026-07-29), which records the owner decision that the drawn-mark budget should be the largest a given client can render rather than the few thousand this document was written around. Appendix A's permutation row counted both directions; contracts §2.6 stores only `entity_to_row`, and the row→entity direction is the `entity_id` column already counted in hot columns — 8 GB → 4 GB at 10<sup>9</sup>. Appendix A's 50 k-point wire figure is marked as an assumption at an unstated *k*, pending probe P2. §13.2's "the render path is invariant" is demoted from settled property to claim under test, pending probes P1 and P2. §10.5's residency sentence gains a note that it sizes what a node holds and is not a per-viewport claim — mask build is per session and reads only the postings the principal satisfies, and it is at a large mark budget that the gather columns join the per-viewport set. No invariant changes; no format changes. The companion format change (`morton.u64` → `morton.u32`) is the contracts spec's r5.
- **r19** — One amendment from the Phase 1 plan's independent review (owner-decided, 2026-07-28). §2.3: the canonical mask-cache key gains the postings identity (manifest digest; partition + postings-epoch under fan-out) alongside the satisfied term set and plugin version — term IDs are bundle-relative ordinals, so a cache persisting across a rebuild could serve a mask naming different entities under an identity-free key. Aligns §2.3 with the cache key the system architecture's §3 already specified. Companion changes in the contracts spec's r4: the pair relation becomes Parquet, and the priority function is fixed as splitmix64-high-16.
- **r18** — The real-label rerun retired by owner decision (2026-07-28): no real access-labelled corpus is available (personal project), so the synthetic-corpus Phase 0 evidence is accepted as final and Phase 1 proceeds on it. The standing caveat converts to deployment guidance: re-run the Phase 0 measurements against real labels before trusting signature alignment, posting compression or union-cost conclusions in any deployment that has them. §16's spatial-autocorrelation entry updated accordingly. Also annotated in place: §11.1's measured compression baseline (created-order ≈ nothing; signature-sorting is the whole effect) and §7.2's measured duty cycle (direct evaluation is the main route).
- **r17** — Four §16 policy questions settled by owner decision (2026-07-27): compartment semantics are **data separation** (§12.1 now operative, not conditional); **current credentials govern all temporal slices** (§9 — the shared-mask premise stands; historical-grants viewing knowingly not provided); the declared generating set is the **prompt sample** (§7.8 — honest to provenance, recorded in manifest provenance, full membership available as a per-deployment strict mode); the token-lifetime backstop defaults to **one hour** (deployment-overridable).
- **r16** — Phase 0 measurement fold-ins (probes/, owner-decided). §6.2: the per-item term cap's *exclusion* behaviour dropped — bounds are sizing declarations, a runaway guard at 10⁵–10⁶ warns while still indexing; a monotone predicate with more terms intends broader visibility, and a resource guard must not produce an authorisation-shaped outcome (no invariant depends on terms-per-item; measured cost shape unchanged at ~130 terms/item). §6.1 and Appendix E updated to match (plugin-side minting is the DNF backstop, the warn is the tripwire). §13.3: the assumed-default row-space sharded index now carries a measured lean against it — re-scattering the index forfeits the signature-sorted entity-order wins (8.9–36.7× storage, up to 130× union). §16: spatial autocorrelation marked measured (scattered; direct evaluation is the main route); overflow visibility resolved as moot.
- **r15** — Amendments raised by the system architecture document (its Appendix R). §2.3: mask content-addressing re-keyed canonically on the satisfied term set, auth-data hash retained as a fast path — required once auth data carries volatile signature bytes. §6.1: the verifiable-auth-data pattern (in-plugin anchors, host-enforced `not_after`, revocation bounded by assertion lifetime) and the interning namespace's process address under fan-out. Appendix C: C15 (session pin rate of change) and C16 (router-held label presence registry) added.
- **r14** — §7.2 corrected. Descent work is proportional to 1/coverage, not log(1/coverage) — depth is logarithmic, node count is the geometric sum, and conflating them understated the sparse case by orders of magnitude. Candidate lists of width *c·k* only yield *k* survivors above coverage 1/*c*, so at *c*=4 they serve almost no realistic principal; direct evaluation from the mask is bounded, gets cheaper as coverage falls, and wins below roughly 5%. The choice is made per tile from the masked count §2.6 step 6 already computes.
- **r13** — §2.6 added: the request path written end to end in one place, non-normative and pointing at the sections that govern each step.
- **r12** — Appendix H added: the general framing (a materialised per-viewer selection; how many, where, whether, which examples), the adjacent domains the same mechanisms would serve, the counting-versus-aggregation boundary, and the four conditions under which the design earns its cost. Explicitly non-normative.
- **r11** — Second prior-art pass folded in: the mask build requires an exploded pair relation rather than array containment (§6.3); label syntax adopted from the access-expression grammar with `accumulo-access` as a CI differential oracle (§6.1, Appendix E); **I2** and §10.4 gain the structural-ordering rule that the mask is the sole entry point to the geometry arrays; row-level security recorded as rejected rather than unexamined (Appendix D, §15).
- **r10** — Authorisation factored into two pluggable functions with a consistency contract (**I5**); the five-dimension model moved to Appendix E as a reference implementation; compartmented partitions added (§12) with required-set gating, dynamic combination discovery and fail-closed cross-partition semantics (**I13**); monotonicity and DNF-indexing invariants demoted into the reference plugin.
- **r9** — Prospective valid-time filtering appendix, defaulting to *possible* overlap.
- **r8** — Normalisation pass: quarantine folded into the overlay; **I1** restated over the live set; entity-ID secrecy promoted to an invariant; rejected approaches reduced to one-liners; glossary reduced to definitions.
- **r7** — Two-stage authorise/retrieve split; model pipeline moved out of scope; credential-polling machinery removed; ingest reworked around buffer, watermark and overlay; delta credential updates removed.
- **r6** — Composable filters and the two-mask model; per-dimension combination semantics; referenced-category pre-intersection; prior art cited throughout.
- **r5** — Priority sampling replacing rank-position sampling; deny composed into the mask rather than applied at egress; version pinning; opaque wire IDs.
- **r1–r4** — Initial architecture, entity/row split, label gating, ingest and scaling.

## Appendix H — Ambition, and its boundary

**Status: not scope.** This appendix records what the machinery is generally, and what else it could serve, so the framing is not lost and is not mistaken for a commitment. Nothing here is being built. The narrow query surface is a safety property, not a stage to grow out of — see the closing paragraph.

### What this is, in one sentence

**Tessera is a materialised per-viewer selection, with every derived quantity computed only from it.** The visible set is built once per session and reused; spatial queries, counts, densities, samples and label visibility are all answered from that set alone. Every property the design claims follows from that single commitment, and it is the thing a stateless query engine structurally cannot offer, because it rebuilds the selection per query by construction.

Stated by what it answers: **how many, where, whether, and which examples — over exactly what a given viewer may see.** Those four verbs are the honest extent of it. They are not "analytics".

The differentiator is narrower than "access control", which is a crowded and unconvincing comparison. It is that **every surveyed system with per-document security permits aggregates over records the viewer cannot read** — documented as a limitation by one vendor, shipped as a feature by another, and demonstrable through query plans in a third (Appendix D). The field draws its line at retrieval and lets everything derived leak past it.

### The two mechanisms that generalise

*`range_cardinality` over a total order* in which a query region is a small number of contiguous ranges, and in which the hierarchy nests. Morton supplies that for two dimensions; nothing in the machinery is two-dimensional. Because permissions live in entity space and geometry in row space (**I4**), several orderings can coexist — one permutation each, one range structure each.

*`and_cardinality` over a set*, which needs no ordering at all: an exact masked intersection count, never materialised.

### Where else the shape fits

**Time and events** are the strongest fit, better than the plane: an interval is a *single* contiguous range rather than a few hundred tiles, and year→month→day→hour nests exactly. This is the data-cube lineage's territory, in which per-viewer filtering is structurally impossible — a per-user bitmap is a filter dimension whose cardinality is the number of users, which is not a thing a cube can contain.

**Faceted counts** need no ordering whatsoever, one `and_cardinality` per facet value, arbitrary dimensions. This is the sharpest competitive contrast available, because the incumbent behaviour is to compute facet counts *outside* the security filter.

**Geography** is the same code with different coordinates, in a field where the documented state of the art is one artifact per permission class.

**Node-level graph aggregates** — masked degree, community size, neighbour samples — are `and_cardinality` over adjacency bitmaps. Traversal and path queries are not a fit.

**Gated reuse of expensive derived artifacts** is the most transferable idea here and contains no geometry at all. The containment rule (**I3**) applies to anything too costly to regenerate per viewer: generated summaries today, but equally topic models, trained embeddings, cached reports, materialised rollups. It is a page of code and it travels anywhere.

### The boundary

**This is a counting engine, not an aggregation engine.** Counts, densities, existence, membership and samples are O(containers) — effectively free, independent of how many items are involved. Sums, means and percentiles are O(visible items), because the values must be gathered. No bitmap construction recovers this: per-term partial sums double-count, since an item carries several terms and terms do not partition the corpus. Masked counting scales to 10<sup>9</sup>; a masked global mean over 10<sup>8</sup> visible items is a scan. Within a viewport that is irrelevant, and as a claim about the system it is a hard line.

### When it earns its cost

Four conditions, all of them: aggregates are sensitive and not merely documents; permissions are high-cardinality and per-viewer rather than a handful of classes; the corpus is too large to ship to the client; and interaction is required, so per-query re-derivation is unaffordable. Drop any one and something cheaper wins — pre-baked artifacts per permission class, an existing engine's document-level security, or brute force in the browser. The intersection is narrow. It is also empty of alternatives, which is the entire argument for building.

### Why not to build the generality

Each new query shape is a disclosure channel requiring its own analysis. Appendix C can be exhaustive because the retrieval surface is roughly five shapes — masked count over a range, *k* lowest-priority visible items in a range, label containment, region selection, drill-down. A general expression endpoint cannot be enumerated that way, and the prior-art survey is a catalogue of systems whose generality is precisely where they leak: the optimizer that rewrites a predicate, the statistics computed over invisible rows, the facet path that diverged from the filter path.

The general framing costs nothing and positions the work honestly. General machinery would cost the property that makes it worth having. New capability enters through §8.2's contract — order-independent set producers composed by intersection — so that expressiveness never reaches the authorisation layer.
