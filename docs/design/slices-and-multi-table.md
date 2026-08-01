# Slices and multi-table shards — design

**Date:** 2026-08-01
**Status:** Provisional — under review. Graduated into the corpus 2026-08-01. Three independent reviews (performance, security/invariants, maintainability) completed 2026-08-01 and their findings folded in. **To become normative:** the amendments in its §11 folded into `architecture.md` and `contracts.md` by owner decision. Until then the rest of the corpus governs where they disagree.
**Reads against:** architecture design §5.1, §7.2, §7.3, §9, §10.2–§10.3, §11–§13, §16, Appendix A, Appendix C; contracts §2.1–§2.3, §2.6, §3.2, §3.4; lifecycle §3, §5.3; system architecture §3, §7; implementation plan §14; probes/optimisations §4; design memo 2026-07-30 (viewport hot path, B9 tiered decode).
**Citation convention:** unprefixed §n is the architecture design, per CLAUDE.md; this document's own sections are cited as **spec §n**.

---

## 1. Summary

Two capabilities fall in the same place and are designed together:

- **Slices** — named, orthogonal coordinate systems over the shared entity space: disjoint temporal sets, multiple embedding spaces, multiple datasets. A point may belong to several slices with independent coordinates, one identity, one set of metadata and labels. This generalises §9's temporal slices; the engine does not distinguish the three flavours.
- **Multi-table layout** — within a shard, points with identical term signatures grouped into physically separate row tables, chosen at build/compaction time from the current signature histogram. This is plan §14's "row-space signature-major layout" taken to physically separate tables, and it is explicitly a **shard-implementation decision** — unlike §12 partitions, which are a security boundary.

**What the layout is for, with its measured anchor.** The B9 tiered decode (design memo 2026-07-30, landed) routes a read to run-decode when visible density in the read range is ≥ 95% — the measured crossover sits in (0.90, 0.95) at cap 500, and at 100% density the contiguous route is roughly twice the sparse gather. That threshold is a **per-read property**: a grant-aligned viewer reading a promoted group's table sees near-total density and rides the fast route. Promoted-group *coverage* (top 500 groups = 82.4% on the synthetic corpus, 88% at 1,000) therefore determines what **fraction of reads** are route-eligible — it is not itself the 95%, and the two must not be conflated.

The unification both rest on: **a *table* is the one physical unit of row space** — its own Morton ranking, tile table and candidate lists, occupying a reserved sub-range of its slice's row-ID space. Ingest segments are tables keyed by flush generation; signature-group tables are tables keyed by group. A slice is a named coordinate system plus a set of tables. A tile is a set of ranges, one per table it intersects — already the Phase 1 type. §7.2 (r22)'s cross-segment merge is the cross-table merge, unchanged. The mask never learns tables exist; it meets them only at the permutation.

## 2. The table

A **table** is keyed `(slice, group, flush)`:

- `slice` — which coordinate system its rows belong to;
- `group` — a promoted signature-group ID, or `residual`;
- `flush` — the flush generation (what the corpus today calls a segment), or `base` for compacted tables.

**The valid population is constrained; the key is not a free cross product.** At any moment a slice's tables are exactly: `(slice, residual, flush_i)` for each live pending flush, plus `(slice, g, base)` for each promoted group `g`, plus `(slice, residual, base)` — where `base` names the current compacted generation. **`group ≠ residual ⇒ flush = base`** is a structural invariant: promoted tables exist only in the compacted base (spec §5), and a manifest schema admitting per-flush group tables would be the flushes × groups explosion spec §5 exists to prevent. Births and deaths: pending tables are born at flush and die at their minor merge or fold; base tables are born at a compaction and die at the next compaction that touches their group.

**Terminology note — the third component is `flush`, never "epoch".** The corpus once used "epoch" for three unrelated things, and decision [0026](../decisions/0026-idset-stamp-version.md) gave each its own word: **idset** for the identifier set a key rotation replaces (contracts §2.2), **stamp** for the build markers governing deny retirement (lifecycle §3's ledger), and **version** for a table's generation in this design. `flush` and `base` are this key's two forms of that version, and "pending table" is the noun for `(slice, residual, flush_i)`.

**Group identity** is a canonical hash of the sorted term signature — the same construction §12.4 uses for partition identity, for the same reason: groups are discovered, not declared, and the ID must be stable across compactions (hysteresis compares this generation's groups with the last's; the manifest's `group` key and the spec §6 dirty bit both need a durable key). `residual` is a reserved value.

What the corpus calls a *segment* becomes the table `(slice, residual, flush)` — the degenerate case with an empty promotion set. A deployment whose histogram promotes nothing (the author-like policy: 1.54M signatures over 2.42M items) stays in that case forever, running today's code path. The walking skeleton is the single-table special case, not a casualty.

**Row-ID space.** One `u32` space per slice per bundle generation. Each table receives a base offset at build/compaction, **aligned to a multiple of 2¹⁶** so Roaring containers never straddle tables — per-table operations stay O(containers touched) with no boundary case. Offsets are generation-scoped; this is free because row IDs already renumber at compaction and mask caches are already generation-keyed (r19). Alignment waste is bounded by `tables × 2¹⁶` IDs, ≤ 65M at 1,000 tables against 2³² headroom. This deliberately supersedes contracts §2.6's "row IDs are segment-local": row IDs become slice-global with tables at base offsets, and that contradiction is resolved at the first multi-segment implementation, not discovered later (spec §11, §12).

**The mask is table-blind.** Entity→row remains one permutation array per slice; a row-space mask fragment is one bitmap per slice covering all its tables; counts are range cardinalities summed across tables; marks are the global bottom-*m* by `tessera_id` across tables — §7.2 (r22)'s merge, which is exact, verbatim.

## 3. Slices

**Definition.** A slice is `{name, optional gate label, projection provenance, table set}`. The manifest carries a slice registry. §9's temporal slices become the first instances rather than a special case; r17 (current credentials govern every slice) and §9's rejection of time-in-the-Morton-code both stand unchanged.

**What is shared, what is per-slice.** Shared, in entity space: identity, the term index, labels and generating sets, metadata, the mask. Per-slice, in row space: coordinates, the permutation array, the table set (tile tables, candidate lists), θ (r24 already gives one θ per slice). This is §5.1's factoring stated as a rule: *entity space is the invariant plane; a slice owns everything downstream of the permutation and nothing upstream of it.* One token authorises across all slices; slice membership is implicit — the permutation's sentinel — never a stored bitmap.

**Per-slice hot-column schemas.** The mandatory hot-column core (§5.3: `tessera_id`, x, y, priority) is uniform, but a slice's table set may declare **additional per-slice scalar columns** in the manifest — an embedding-space slice can carry a confidence scalar the temporal slices do not. These are render-plane projections like every hot column (the routing principle's per-mark class, §10.3), surfaced to clients through the slice's entry in `/v1/meta`; they widen no query surface and carry no filter semantics — filtering stays in entity space under §8.2. A modest manifest extension (contracts §2.3 enumerates columns per slice rather than once), paid at the schema level now so it is not a format break later.

**A relational reading, for orientation.** In SQL vocabulary the design is: **one logical table** — entity space, owning identity, labels and metadata — and **N clustered covering indexes**, the slices, each a physical ordering of a subset of rows carrying copies of the columns its access pattern needs; secondary predicates are §8.2's filter bitmaps, never row gathers. A slice is *not* a table: slices **cache render-critical scalars and never own metadata**, so there is nothing to keep consistent between slices — the single source of truth is entity space and the slice-invariant `tessera_id` is its wire name. The off-hot-path attribute store this implies is already surveyed (design memo 2026-07-29, secondary attribute indexing) and reaches the viewport only as entity-space bitmap intersections, which is what keeps I2/I3 intact.

**Slice-count budget** *(review finding, accepted)*. The permutation array is sized by **maximum live entity ID**, not by slice population: a flat `u32` array is ~4 GB at 10⁹, *per slice*, sentinel-dominated when the slice is sparse — D dataset-slices of 10⁸ each over a shared 10⁹ entity space would cost ~4·D GB flat. The spec therefore permits, behind the contracts reader interface that plan §14 already insists stays abstract, a **two-level paged permutation representation** for sparse slices: a page directory over 2¹⁶-entry pages with absent pages meaning all-sentinel, chosen per slice at build. Two further per-slice multipliers are named rather than hidden: the projected-fragment cache is per `(token, slice, pin)`, so a session touching S slices multiplies projection cost and cache footprint by S; and flush emits one pending table per slice *touched*, so a row carried in k slices generates k tables' worth of segments and compaction debt system-wide. §16's open question — how many slices must be simultaneously browsable — remains open and now has a price list attached.

**Addressing.** The viewer verbs already carry `slice` in the request body (contracts §3.2 — viewport and region both name it); no amendment is needed there, and the design's §7.2 (r24) citation of an `x-tessera-slice` header against contracts §3.1 is stale. The one real header use is `/control/ingest` (contracts §3.4), which the coordinate-map contract below supersedes. The genuine contracts changes are in §3.2: the `slices` array in `/v1/meta` becomes gate-filtered (below), making meta per-principal in a second field alongside the C11-gated vocabulary — the same precedent, cited rather than rediscovered — and the discovery response keeps its shape, filtered.

**Gating.** A slice's gate is a **label**, evaluated by the plugin exactly as an item's label is. Satisfaction is the **item-visibility predicate verbatim** (§6.1): the gate label resolves to its term set, and the gate is satisfied iff that set intersects the principal's satisfied set. *This sentence is normative and deliberately worded* — an earlier draft said "conservative label join, yielding a required term set", which names §12.2's subset machinery, under which a disjunctive gate (`finance | legal`) yields an empty required set and every principal passes: a fail-open on exactly what the gate protects. Intersection semantics give a disjunctive gate its intended meaning (either grant reveals the slice). Review finding, accepted; the required-set vocabulary is struck.

- **Evaluation point:** the principal's **visible-slice set is resolved once per session** — at authorise, the gate evaluated for every registered slice regardless of outcome — so the request-time check is a single set-membership lookup, identical in work for a gate-failed name and a never-registered name. This is the same structural closure C4's annotation records for `/v1/items`, and it is what makes the claim below meet r23's **work**-indistinguishability standard rather than only outcome-indistinguishability.
- If the gate is unsatisfied: the slice is absent from discovery, and a request naming it is indistinguishable — in outcome and in work — from naming a slice that never existed. Fail-closed.
- **The gate governs every slice-valued response surface**, not only discovery: any endpoint that would return per-slice coordinates, a slice-membership list, or any other slice-keyed field omits gate-failed slices. Item visibility through the mask covers the items; this rule covers *reachability*, and both are needed.
- The gate is **conjunctive with item labels**, never substitutive: items inside a gated slice remain individually governed by their own labels. A gate can only narrow, never widen — the I12 direction.

**Ingest contract.** An ingest row carries a `{slice → (x, y)}` map instead of a single coordinate pair; membership in a slice is presence in that map. Adding an existing entity to a further slice is ordinary ingest resolved by `external_id` — which **amends contracts §3.4's duplicate rule** (today: duplicate `external_id` → 409, batch has no effect). The amended semantics, each arm ruled explicitly *(review finding, accepted)*:

- **Label must byte-match** the entity's current label. A row carrying a different label is refused: predicate changes enter through `/control/changes` and its evaluate-entry / fold-retirement machinery, never smuggled through ingest — the alternative is a widening with no overlay entry, or a revocation that bypasses the deny lanes.
- A row naming a slice the entity already occupies is a **409, loudly**: coordinate updates are explicitly deferred (below), and this arm must not become an accidental update path — the single-valued permutation cannot represent two rows for one entity in one slice.
- A row naming only new slices is accepted and lands in each named slice's current pending table.
- **Identifier forms.** A row names its entity by `external_id` (canonical, durable across every break event) **or** by `tessera_id` — in which case the `idset` is **mandatory alongside it, with no optional form**: a retained (rolled) idset translates exactly via its key; a revoked or unknown idset is a 409. Contracts §2.2's argument for optional-idset reads (stale identifiers fire approximately never; a misresolved read is bounded) does not transfer to writes: a stale `tessera_id` does not fail, it silently names a different entity, and a write against it is cross-entity corruption through the trusted plane. Mandatory-on-write, optional-on-read is the same identifier with the risk priced per path. Spec §8's Tier 1 is what makes the `tessera_id` form worth offering at all — after it, the idset advances only on key rotation — and the acked identifiers a pipeline already holds make it the convenient form for "ingest, then attach to a second slice".
- **Trust assumption, stated:** entity resolution by `external_id` across slices is sound only under a single, mutually-trusting ingest authority — the accepted/created distinction lets an ingest caller probe which `external_id`s exist corpus-wide. The admin plane is single-authority today (SA §2); if that ever changes, this is the sentence to revisit.

**Lifecycle: create, populate, drop.**

*Create* is a control-plane operation: `{name, gate label, projection provenance}`. Validation at accept: name unused (including tombstoned names — below), gate label evaluable by the plugin. The record is WAL'd; the *served* registry is the manifest's registry plus WAL-overlay additions, materialised into the manifest at the next flush — the overlay-then-fold shape the write path already uses everywhere. Ordering rule: **a slice must be acknowledged before any ingest row referencing it is accepted** — no same-batch creation, no auto-create on first reference; slices are deliberate objects that carry gates. An empty created slice is visible in discovery (gate permitting) with zero counts; creation is deliberate, so there is nothing to hide. Entity space is untouched; the cost is row-space artifacts, which the budget paragraph above prices rather than waves at.

*Populate* has two routes:

1. **Incremental** — the ingest map, exactly as above. New points carry coordinates for whichever slices they join; existing points join a new slice via the amended duplicate rule. Lands in the slice's pending tables; seconds-to-minutes visibility; group-commit untouched.
2. **Bulk backfill is a build-plane operation, not a stream of ingests.** Creating an embedding-space slice over an existing 10⁹-item corpus means 10⁹ coordinate rows — a batch job that must not ride the trickle path. `tessera build --attach-slice` consumes a Parquet of `(external_id, x, y)`, resolves IDs, builds the new slice's row-space artifacts *only* — Morton sort, tables, tile tables, candidate lists, permutation — and flips the generation pointer. Entity space is untouched by construction, and spec §7's prefix-qualified manifest references pay off a second time: the new manifest **references every other slice's tables verbatim** — a slice attach copies nothing it did not build. Rows for the new slice arriving during the attach build land in pending tables against the old generation and survive the flip as pending tables, exactly like any build-concurrent ingest.

*Drop* is the inverse control operation: a WAL'd registry tombstone. The slice vanishes from discovery on ack — acknowledgement coupled to application, deny-style; its row-space artifacts are garbage, collected at the next compaction; no entity is deleted by dropping a coordinate system it appeared in. A dropped slice's **name stays tombstoned against reuse** — a recreated "2024-Q1" with different membership would silently repoint every bookmark and cached θ that named it; a fresh name costs nothing. Dropped-versus-never-existed is indistinguishable by construction: both are simply absent from the session's visible-slice set.

*A consequence worth owning:* `--attach-slice` is also the **coordinate-migration escape hatch** — re-attach the slice under a new name (new projection fit, same members), then drop the old one. Callers get projection migration without in-place coordinate-update machinery, at the cost of the slice name changing — which is honest, since the geometry did too.

**Caller obligations (extends §2.4).** Projection stability within a slice; entity identity *across* slices is by `external_id` — "the same point in two embedding spaces" is exactly the caller saying so at ingest.

**Deliberately out of scope.** In-place coordinate updates within a slice (a row move; same rarity class as predicate changes, deferred to the same compaction machinery — and the attach-under-new-name path above covers whole-slice migration meanwhile). Removing an entity from a single slice is a caller-facing API question deferred with it.

## 4. Promotion policy

**Scope of the histogram:** signature groups are entity-space, so the histogram is taken **per partition over live entities**; `corpus_size` below is the partition's live count. A promoted group yields one base table per slice in which its members hold coordinates.

A signature group is promoted iff

```
count(group) ≥ max(abs_min, p × corpus_size)      — promote
count(group) < max(abs_min, (p/2) × corpus_size)  — demote
```

The demotion form is deliberate *(review finding, accepted)*: halving the *whole* promote threshold would keep sub-`abs_min` groups promoted, contradicting `abs_min`'s purpose; only the proportional term is halved.

- The **proportion floor `p`** bounds the promoted count at ⌊1/p⌋ — but on the measured histogram this bound is a **guard-rail, not the operative control**: promoting 256 groups needs p ≈ 0.04%, whose ⌊1/p⌋ ≈ 2,400 constrains nothing real. The **hard cap does the operative work** and the spec says so plainly.
- The **hard cap** on promoted-table count is independent config. When qualifying groups exceed it, rank by count descending with the group ID as a stable tie-break.
- **Dwell:** the promotion set is re-evaluated only at major compactions, and a group's status changes at most once per **D consecutive major compactions** (config, default small). Dwell, not hysteresis, is what governs churn at the cap boundary — the cap reintroduces at its edge exactly the thrash the threshold's hysteresis kills, and needs its own brake. The previous promotion set is an input to compaction and persists in the prior manifest.
- All parameters (`p`, `abs_min`, cap, D) are **config with measured defaults, never constants in code** — spec §9's sweep sets them. Their home is the system architecture's §7 config schema; promotion-set evaluation is assigned to the build/compaction crate in SA §3's decomposition (spec §11).

On the synthetic corpus the rank-size figures (4,213 items at rank 100 → 444 at rank 500) put top-100 coverage near 60% — a modest cap captures the head, not "most of the coverage"; the knee lives at 250–1,000 groups (optimisations §4), which is why the sweep must reach past it (spec §9). On the author-like histogram nothing promotes and the layout degrades to today's, which is the correct behaviour for that shape.

## 5. Lifecycle

**Grouping is a property of the compacted base, not of the write path.** A flush emits one pending table per slice touched — a new flush generation — Morton-sorted, group-**agnostic**, tiny. The write path (WAL, group-commit allocation, ack contract) is untouched by this design. Live tables per slice are bounded by `N_promoted (hard-capped) + live flushes (bounded by compaction cadence, as today) + 1 residual`.

**Compaction is where grouping happens.** At each major compaction: re-evaluate the promotion set (spec §4's thresholds and dwell); merge pending flushes into per-`(slice, group)` base tables; carve newly-promoted groups out of the residual; fold demoted groups back in. Minor merges (flush-into-flush, never touching the base) stay under §11.3's existing bound.

**Carve and fold are priced, not waved at** *(review finding, accepted)*. A promotion or demotion **rewrites the residual**, which at 10⁹ with heavy promotion is still ~176M rows — ~3.2 GB of hot columns before the permutation, tile table and candidate lists — a cost at the scale of §11.3's 5 GB merged-segment bound as a single unit. Therefore: promotion-set changes are **forced-compaction-only events with operator-visible cost** (surfaced on `/control/status` alongside the fragmentation metric), dwell bounds their frequency, and the residual rewrite participates in §11.3's merge budgeting rather than sneaking past it. A group oscillating across the hysteresis band cannot force more than one residual-scale rewrite per D major compactions by construction.

**Rapid ingest fragments at today's rate, by construction.** Group-agnostic flushes mean sustained ingest adds tables exactly as it adds segments today — not flushes × groups. What is new: **the contiguity win lives only in the base**, so under rapid ingest with lagging compaction the route-eligible fraction of reads (spec §1) degrades gracefully toward the unpromoted state — a performance decay, never a correctness or disclosure event. The knob is compaction cadence; `/control/status`'s fragmentation metric is the observability hook.

**Predicate changes ride the existing third retirement rule.** A signature change may leave an item in the wrong group table until its compaction fold. That is safe, not merely tolerable, because of this design's load-bearing invariant:

> **Tables are performance layout, never an authorisation boundary.** Visibility is decided entirely by the entity-space mask meeting the permutation; the table a row physically occupies has zero authority. An item in the wrong table is a contiguity regression, never a disclosure.

This is the property that distinguishes tables from §12 partitions, which *are* an isolation boundary with the overlay covering moves. The lifecycle's three deny-retirement rules gain no fourth case **for tables**: deletes and suppressions are entity-space mechanisms and do not know tables exist. (The partition-*move* deny is a different story, and spec §8 Tier 1 now rules it explicitly rather than inheriting it silently.)

## 6. Whole-table shortcuts: excluded, with the safe half specified for later

Two directions, sharply different:

- **Rule-in** (group signature satisfied → serve the whole table, skip mask intersection) is plan §14's flagged shortcut: **fail-open** under suppressions, deletions and predicate-change overlays, and it buys little — visible rows in a promoted group are dense for the B9 route whether or not the intersection ran, and bitmap ops over a contiguous group are already cheap. **Excluded outright.**
- **Rule-out** (signature unsatisfied → skip the table) is fail-closed by direction, but not unconditionally safe: a predicate change can make an item visible while it still sits in a group whose signature the principal fails; skipping that table hides an acknowledged change and breaks the caller-observes-own-write rule.

**The dirty bit is defined by fold status, not by acceptance time** *(review finding, accepted — this is the load-bearing sentence)*:

> A group is **dirty** iff any **live (unretired) overlay entry**'s entity has its row in the group's table.

Acceptance-time readings ("touched since base") are wrong: lifecycle §5.3 carries post-snapshot entries forward across compaction *unfolded*, and those entries pre-date the new base — a time-based bit goes clean while an unfolded predicate change still sits in the group, hiding an acked change. Fold status is exact because evaluate entries retire precisely at the fold that makes the base honest. Consequences, stated rather than discovered: a group holding any **suppressed** entity stays dirty for the suppression's whole lifetime (suppressions never fold; conservative and correct); an unsuppress that drops a coalesced evaluate entry cannot clear the bit while that evaluate entry is live, because the bit tracks the evaluate entry itself; and the per-session **table visibility vector** (one gate-style signature evaluation per promoted group → satisfied / unsatisfied / dirty) is **keyed by `overlay_version`** per r19's discipline, so a mid-session predicate change invalidates it rather than bypassing its own guard.

Rule-out remains **specified, optional, and unbuilt**: adoptable only after the conformance suite grows a canary for it (spec §12). The layout earns its keep through contiguity alone.

## 7. On-disk layout, mmap and the write path

The corpus's storage model (§10.3: one file per column per segment, raw fixed-width Arrow IPC buffers, page-aligned, mmap'd zero-copy; §10.2: immutable versioned prefixes, NVMe sync at boot) survives intact; multi-table forces exactly one decision inside it.

**Per-table column files, not slice-wide files.** Two candidate serialisations of a slice's row space:

- *Slice-wide*: one file per column per slice, tables as extents at their base offsets, alignment gaps as sparse-file holes. Preserves today's mmap count and pure-arithmetic gather (`row × width`), but carving one group out of the residual at compaction rewrites the whole slice's columns — ~8 GB per column at 10⁹ — a write-amplification cliff attached to the most routine compaction event.
- *Per-table*: one file per column per table. Compaction rewrites only the tables it touches, and — the substantive win — **an untouched table is carried into the next generation by manifest reference, not by copy**. A stable promoted group is precisely what compaction rarely touches, so the tables that pay the layout's rent are the ones that stop costing anything to carry. Costs: the gather goes through a table directory (row ID → (file, local offset), a small sorted lookup over base offsets — cacheable, and the fan-out cost is priced in spec §9); and mmap count rises to tables × columns × slices — thousands at the hard-capped counts, well inside VMA and fd limits.

Per-table files are the recommendation. Page alignment and container alignment both hold trivially per file, since each file starts a table.

**Pre-compaction tables: no change from today.** The corpus's model is already one file per column per *segment* (§10.3), and a pending table is a segment — flush writes exactly the files it writes now, and minor merges are the file-count consolidator, exactly as they bound segment count today. One refinement: **pending tables use a single combined file per table** (all columns, one small Arrow IPC file) rather than per-column files — they are tiny, short-lived and rewritten at the next minor merge, so per-column granularity buys nothing there. Per-column files are the *base* layout, where selective column reads at 10⁹ are the point. This cuts the flush-cadence file spray by the column count at zero read-path cost.

**File-count bound.** Live files ≈ slices × columns × (N_promoted + live flushes + 1), plus one metadata sidecar per table (tile table and candidate lists bundled, not separate files). At 4 slices × 6 columns × (64 promoted + 8 flushes + 1 residual) ≈ 1,800 column files per generation — trivial for the object store, NVMe sync, file descriptors and VMAs alike. Both terms are bounded by existing knobs: the hard cap bounds promoted tables, compaction cadence bounds flushes.

**The write path, itemised:**

- **WAL: untouched.** Entity-space; tables are row-space artifacts downstream of flush.
- **Flush: untouched in shape.** Spec §5's group-agnostic pending tables mean flush writes one small table per touched slice — no per-group file spray at the flush cadence.
- **New, priced:** a point belonging to *k* slices writes coordinates into *k* pending tables — write amplification proportional to slice membership. Inherent to independent coordinates, bounded by the ingest map, and visible at flush rather than on the request path.
- **Compaction:** carve/fold writes only the residual and the promoted tables whose membership changed (priced in spec §5); untouched tables carry by reference.

**Manifest references are a format decision paid early, and the earlier "no format change" claim was wrong** *(review finding, accepted)*. Contracts §1 makes all manifest paths prefix-relative and §2.1 says the prefix grows only by whole new files named in a newer side-manifest — a table carried by reference from an *older* prefix violates both unless table references are **prefix-qualified from day one**. That is exactly the class of thing the corpus says must be paid before a format is published, so it goes on spec §12's foreclosure list, not in a footnote.

**The honest price: generation GC.** Manifest-reference sharing breaks "delete the old prefix" — unreferenced table files need refcounted or mark-sweep collection across manifests. This is deferrable: below ~10⁸ items, strict prefix-copy (rewrite everything, delete old prefix) remains simple and affordable, and the switch to reference-sharing is a serving-node and build concern invisible to the wire. The trigger for adopting it is compaction write volume, observable operationally.

## 8. Identity stability

Slices sharpen the value of stable identity: the entity is the join key across coordinate systems, and `tessera_id` is already identical for an entity in every slice (the slice is not an input to the keyed bijection) — cross-slice join on the wire works today. The remaining instabilities are the three break events, and the honest position is tiered, because stability, dense machinery and placement freedom cannot all live in one integer — *permanent identity, dense machinery, placement freedom: pick two per integer* — which is precisely what plan §14's ι-ordinal split answers with two.

**Tier 1 — proposed ruling: repartitioning preserves identity by default.** §12.5's reindex breaks `tessera_id` only because the rebuild takes the opportunity to reallocate entity IDs, not because moving an item between partitions requires it — there is a single global allocator across partitions (§16, r21), and contracts §2.2's own stated reason ("the permutation's input encodes placement") does not hold for partitions, which never enter the input. Identity-breaking reallocation is demoted to what plan §14 already calls it — an escape hatch, never the default. Contracts §2.2's "must advance the idset on any partitioning change" relaxes to "on any build that *reallocates*, and on key rotation". Three obligations attach, from review, all accepted:

- **The move deny gets a retirement rule of its own.** §12.5's move protocol denies in the source until the item lands; that deny is caused by neither a deletion nor a suppression nor a plain predicate change, and classifying it as a deletion-deny lets it retire by the stamp ledger **while the source partition's postings still index the entity** — fail-open across an isolation boundary, in bulk, once identity-preserving repartitions make moves routine. The rule: **a move's source-side deny retires only at the source compaction that removes the entity from the source's postings** — rule-3 shape, participating in the retirement floor. This goes into the lifecycle amendment (spec §11) as a named fourth entry in the move protocol, not discovered in code review.
- **The builder must prove non-reallocation.** "Advance on reallocation" replaces a syntactic check (partitioning differs) with a semantic one; the build records and verifies an identity-preservation attestation, or advances the idset.
- **C17's row is amended alongside**: its acceptance cites the idset as a time-bound on existence probing, and Tier 1 deliberately extends that window across repartitions. The trade is the point, and the register owns it.

*Fold-in note (review recommendation, accepted):* Tier 1 is a contracts §2.2 semantic change whose coupling to this spec is motivational, not mechanical. At fold-in time it travels as **its own amendment proposal with its own review trail**; it is retained here as design context so the slice/table decisions that motivated it stay legible.

**Tier 2 — kept open at zero cost:** the 64-bit identity input is *reinterpretable* as **birth-block** rather than placement — the prefix records where an ID was born, never where the item lives. Today the prefix is 0 either way; no allocation policy is pinned. Recorded consequences of the birth-block reading, for when the reshard design is written: allocation stays trivial and uncoordinated (each shard mints from blocks it owns), with per-shard u32 headroom — the same exhaustion arithmetic as §16's shard-local sketch, so the exhaustion objection to "global allocation" does not apply; migration in **either direction** never renumbers — a retired shard's blocks freeze, never re-issued, never minted from again, so shrinking a deployment is symmetric with growing it and advances no idset (placement-coupled schemes fail exactly this scale-down question); the cost is scatter — a shard's population comes to span multiple birth prefixes, its local bitmap universe goes sparse in the upper bits, and §11.1's signature-sorted contiguity does not survive assembly from foreign-born IDs. Roaring absorbs the sparsity; the contiguity loss is the real price and is what Tier 3 exists for. Drift is observable via `/control/status`'s fragmentation metric. The birth-block prefix opens no wire channel: it is an input to the keyed bijection, and nothing of it survives to the `tessera_id`.

**Tier 3 — recorded as contingent:** wire-identity stability across *resharding* is achievable **iff the ι-ordinal split lands** — the only structure in which scattered permanent IDs are livable, because postings, masks and the permutation index the dense renumberable ι and never the scattered layer. The split's two named safety holes (overlay keying is fail-open under renumbering; WAL replay) are **prerequisites, not footnotes**; nothing in this design depends on them closing, and the decision point is the reshard design at §13.4's own trigger, not now.

**Key rotation splits into two operations, and only one breaks references** *(ruled 2026-08-01, prompted by the 10⁹-references question)*. Retaining rotation at all is deliberate: it is the sole remediation for deployment-key compromise, which would otherwise permanently open I10's inversion channel (gaps count allocations; proximity discloses shared signatures) for every identifier ever issued, and forbidding it deletes nothing — the idset machinery exists anyway for the escape-hatch reallocation. But the compromise disclosure is *already complete* the moment the key leaks, for every identifier issued under it; continuing to **honour** those identifiers afterwards discloses nothing further, while all new allocation is protected by the new key. Hence:

- **Roll** — the routine form. Mint a new key, advance the idset, **retain the old key server-side**. Responses carry current-idset identifiers; an identifier presented with a retained old idset is resolved by inverting with that idset's key and re-emitting the current form — two pure functions, no translation table, nothing rewritten at 10⁹, **no reference breaks**. Forgery under a stolen retained key buys nothing: the mask gates every response and C4 keeps invisible indistinguishable from nonexistent.
- **Revoke** — the deliberate break. Drop a retained idset's key; identifiers naming it get today's semantics — 409, re-resolve by `external_id`. Reserved for when circulating identifiers must actually die, which the mask makes nearly never.

Multi-idset acceptance is why the idset must accompany the identifier (two bijections both "succeed" on 64 bits): untagged identifiers resolve as current-idset; tagged ones translate if retained, 409 if revoked. Consumers who followed the contract — persist `external_id` — were never at risk on either path. C17's time-bound weakens correspondingly (a rolled identifier lives across rotations); that is the point, and the register entry owns it alongside Tier 1's extension. `external_id` remains the durable key throughout.

## 9. Performance analysis and the fan-out sweep

**Where the win actually comes from** *(rewritten after review — an earlier draft quoted the ~7,500× container figure here, which is an entity-space postings-union win belonging to §11.1's allocation ordering; this layout is a row-space permutation and does not touch postings. Optimisations §2.3 documents exactly that double-count; the corrected legs follow.)*

1. **Projected-mask collapse** — the strongest measured leg (optimisations §4.1): the row-space projection of a high-coverage mask measured at 8.8 s at 10⁹, and a signature-major layout collapses the scattered gather that dominates it. The sweep below instruments it directly.
2. **Read-route eligibility** — the B9 tiered decode's run-decode route engages at ≥ 95% visible density in the read range (measured crossover ∈ (0.90, 0.95), ~2× the sparse gather at full density; spec §1). Grant-aligned reads inside promoted tables sit near 100%; promoted coverage sets the eligible fraction.
3. **Priority-read contiguity under direct evaluation** — the read that touches every visible row in a tile range rather than *k*; **unmeasured**, and the probe plan §14 already owes is inherited by the sweep.
4. **Permutation encodability** — `entity_to_row` near-monotone within groups, opening Elias-Fano-class encoding behind the reader interface — **with plan §14's own qualifier kept attached**: near-monotonicity is only as good as the batch granularity (r23: nothing repairs per-batch fragmentation short of the ι split), so under continuous small-batch ingest this leg decays and must not be priced at its bulk-build ceiling.

**Who pays: the high-coverage principal** — already the system's worst retrieve-side path (the 8.8 s projection; the 2,885 ms hash-flat head-principal authorise case — an earlier draft mislabelled the 588 ms figure, which is the random-w=10⁴ scenario, as "head-principal"). For them the layout is a transfer, not a free win, and the costs *multiplied out* rather than gestured at *(review finding, accepted)*:

- **Count pyramid:** ~300 viewport tiles × (N+1) ranges — at N=256 that is ~77k `range_cardinality` calls, which is the *same figure Appendix A prices at tens of milliseconds against the 10 ms p99 budget* as its argument for capping the underlay. The base count path may not spend the underlay's budget; this alone bounds N well below 256 for count-heavy deployments unless the sweep proves otherwise.
- **The §7.3 underlay multiplies it by 4^s:** ~77k sub-cells at s=4 × (N+1) ranges ≈ 20M range calls — seconds, i.e. an unusable underlay at high N. The underlay is a load-bearing §7.3 mechanism and was **absent from this spec's first sweep design; it is now a required measured quantity.**
- **Bottom-m merge:** §7.2 (r22) has each table offer its own bottom-`cap`; a tile's merge becomes (N+1) × cap candidates — ~33k per tile at N=256, cap 128 — and the per-table **candidate lists** multiply in storage toward ×N at coarse levels (Appendix A's 179 MB baseline), with high-coverage principals exactly their clientele. Both the merge and the storage go into the sweep.
- **Gather locality inverts:** one nearly-contiguous span per tile becomes up to N+1 ranges in N+1 files.

**The fan-out sweep** (replaces the first design after review; extends plan §14's gather probe). Synthetic corpus; **N ∈ {1, 8, 32, 64, 128, 256, 512, 1024}** — the top end must clear the documented knee at 250–1,000 groups (optimisations §4), which the first design's N=256 ceiling did not; three principal shapes — grant-aligned narrow, mixed, full-coverage; measured quantities: **count pyramid, §7.3 underlay at s=3 and s=4, bottom-m merge including candidate-list merge width, column gather (priority column under direct evaluation), mask projection build time, and projected-fragment cache footprint**; every result reported **against Appendix A's budgets explicitly** — the 10 ms p99 count path and §10.4's low-single-digit-ms viewport walk. The full-coverage knee sets the hard cap; the grant-aligned crossover sets default `p`; the dwell default follows from measured carve/fold cost (spec §5).

## 10. Leak analysis

**The framing precedent: row-space layout is already a full-corpus function.** Morton rank depends on every item's geometry, and it has never been a leak because row IDs never cross the trust boundary. The promotion set is the same class of artifact — derived from the full histogram, invisible from outside. What needs checking is whether tables *escape* row space. Three channels:

**Value channels — closed by an indistinguishability property**, stated in conformance-checkable form:

> **Single-table indistinguishability:** for any principal, any request, the response under a multi-table layout must be **byte-identical** to the response the single-table layout produces for the same bundle content and generation. Layout is a physical decision with no representational residue on the wire.

**Ordering resolved itself in the strong direction** *(review finding — both reviews converged on it — accepted)*. Contracts §3.2 (r7) already fixes point batches "ordered ascending by `tessera_id` within each tile"; ordering was contract all along, and §7.2's exact bottom-*m* merge across tables produces exactly that order at no cost. So the existing contract **stands** — this spec changes nothing about it and the amendments table now says so — the ordering half of the side channel closes at the wire, and the differential above is **byte-level with no order canonicalisation**, which is strictly stronger than the canonicalised form this spec first proposed (the suite's canonicaliser sorts rows and is structurally blind to ordering regressions; a byte-level diff is not). An implementation that concatenates per-table runs now fails the suite's strongest test instead of passing it.

**Timing — one accepted register candidate, owning both its edges** *(revised per review)*. Per-tile work varies with the viewer's visible-table count — a function of their mask **and the global promotion set**, and the entry does not pretend otherwise. What an observer can resolve: that some group is promoted, i.e. holds ≥ `max(abs_min, p × corpus_size)` live items — via timing (an invisible table costs an absent-container check, sub-microsecond against tens-of-ms responses). **Hysteresis sharpens the fact**: watching a group's promotion status flip across compactions brackets its size within the promote/demote band — a two-sided estimate, C15-adjacent, and the entry owns it explicitly. **Proposed as a new Appendix C entry, argued accepted on magnitude** — listed because the register's rule is that anything not in the table is a bug. Bundle-internal artifacts (file names and sizes in the bucket and on NVMe encode the promotion set and group sizes) sit inside the trust boundary with the postings themselves and are noted in the entry's text, not separately accepted.

**Slice channels — settled in spec §3:** gated-slice existence closes by the session-resolved visible-slice set, meeting r23's work-indistinguishability standard, and the gate governs every slice-valued response surface; cross-slice `tessera_id` stability is deliberate linkage of an item to itself (C17's argument extends); slice membership of a visible item is caller-supplied data shown only through the mask; `/v1/meta` becomes per-principal in its `slices` array, the C11 precedent cited.

Net: no change to the five-verb surface; one new accepted register entry; one strengthened conformance property — a byte-level differential build that is among the strongest tests in the suite.

## 11. Proposed corpus amendments (on fold-in, owner decision each)

| Document | Change |
|---|---|
| Architecture design §5.1, §9 | Slice generalised to named coordinate system with gate label (intersection semantics); temporal slices become instances; runtime slice creation; sparse permutation representation permitted behind the reader interface; stale `x-tessera-slice` citation in §7.2 (r24) corrected |
| Architecture design §11.2/§13 | Segment → table `(slice, group, flush)` with the valid-population invariant (spec §2); container-aligned generation-scoped base offsets; group ID = canonical hash of sorted signature (§12.4's construction) |
| Architecture design §12.5, §16 | Tier 1 ruling (identity-preserving repartition; reallocation demoted to escape hatch; builder attestation) — **travels as its own amendment proposal**; Tier 2 birth-block note against the exhaustion entry |
| Architecture design §10.2/§10.3 | Per-table column files; combined-file pending tables; prefix-qualified manifest table references; generation GC note (strict prefix-copy until compaction write volume triggers reference-sharing) |
| Appendix C | New accepted entry: promotion-set threshold facts via timing, including the hysteresis two-sided bracket. **C17 amended**: the idset's time-bound weakened by Tier 1 and by roll-mode rotation, accepted as the point of both changes. Slice-existence disclosure noted under C4's closure with the work-indistinguishability mechanism |
| Contracts §2.1 | "One segment per (partition, slice) at build" relaxed to the table population of spec §2; prefix-growth rule extended for prefix-qualified references |
| Contracts §2.2 | Idset-advance rule relaxes to "reallocation or rotation" (with Tier 1's own proposal); identity-preservation attestation; **rotation split into roll (multi-idset key retention, idset-tagged identifiers translate, no break) and revoke (today's 409 semantics)** |
| Contracts §2.3 | `segments` array gains `group` and base-offset fields (`group` always `residual` until promotion exists); hot-column enumeration becomes per-slice (mandatory core + optional per-slice scalars) |
| Contracts §2.6 | "Row IDs are segment-local" → slice-global row IDs with container-aligned table base offsets |
| Contracts §3.2 | **Ordering rule explicitly unchanged** (affirmed against the multi-table merge); `slices` array in `/v1/meta` gate-filtered — meta becomes per-principal in a second field, C11 precedent; discovery shape stated |
| Contracts §3.4 | Coordinate map `{slice → (x, y)}`; duplicate-`external_id` rule amended per spec §3 (byte-match labels, same-slice 409, new-slice accept); ingest identifier forms (`external_id` canonical; `tessera_id` + **mandatory** idset, 409 on mismatch); slice create and drop control operations (create-before-reference ordering, tombstoned names, ack coupled to application); header addressing retired with it |
| Lifecycle §3/§5 | Compaction gains promotion-set evaluation, dwell, and group carve/fold with its cost participating in §11.3's budgeting; **the §12.5 move deny gets its named retirement rule** (source-compaction-coupled, rule-3 shape); note that tables themselves add no fourth deny-retirement case; **the deny-retirement ledger is keyed by *stamps*, never "epochs"** (decision 0026); companion mechanical rename applied in the Phase 1 crates |
| System architecture §3, §7 | Promotion-set evaluation assigned in the crate decomposition (build/compaction side); `p`, `abs_min`, cap, dwell in the §7 config schema; per-deployment layout switch; `tessera build --attach-slice` as a build mode (slice-scoped row-space build, generation flip by reference) |
| Conformance design | Byte-level single-vs-multi-table differential build (no order canonicalisation); later, the rule-out canary and the fold-status dirty-bit checks |
| Implementation plan §14 | Signature-major entry superseded by this design (physical tables, threshold promotion); gather probe extended to the spec §9 sweep |

## 12. Adoption gates

The layout is a per-deployment build decision, **off by default** (empty promotion set = today's layout, same code path). Gates for turning it on:

1. Working conformance suite (Phase 2), including the byte-level single-vs-multi-table differential build.
2. The fan-out sweep (spec §9) run to N=1024 with the underlay and mask projection included, and the knee found; `p`, `abs_min`, hard cap and dwell set from it as config defaults, reported against Appendix A's budgets.
3. A real signature histogram showing the knee — per design r18, deployment guidance, not producible by this project.
4. Rule-out skipping stays out until the suite has its canary; rule-in stays out, full stop.

**What Phase 1/2 must not foreclose (payable now, all cheap):**

- Tile lookups typed against a set of tables (already true via segments).
- The permutation's representation stays behind the contracts reader interface (flat and two-level paged both admissible).
- Table base offsets container-aligned from the first multi-segment implementation — **and contracts §2.6's "row IDs are segment-local" is resolved to the slice-global reading at that same moment**; the contradiction is named here so it is resolved deliberately, not discovered.
- The manifest's segment entry gains a `group` key (always `residual` until promotion exists) — encoded as the canonical signature hash with a reserved residual value.
- **Manifest table references prefix-qualified from day one** (spec §7): the reference scheme is a format decision, and paying it early is what keeps reference-sharing a build-flag rather than a bundle-format break.
- **Ingest accepts the `{slice → (x, y)}` map form from the start** (single-entry maps initially): the request-schema break is free only while `api_version = 1` has no published reader (contracts deviation 10's argument), which is now.

## 13. Open questions

- Removing an entity from a single slice: API shape and whether it is a deletion variant or an ingest-map update. Deferred with coordinate updates.
- Whether the slice gate label participates in `V_total`/θ anchoring in any way beyond membership (believed no: the gate only decides reachability, and θ is per-slice over rows already).
- The fan-out sweep may show the count pyramid, the underlay and the merge kneeing at different N; if so, whether the cap should differ per verb, and whether the underlay needs its own lower cap.
- How many slices must be simultaneously browsable (§16's open question, now with spec §3's price list); whether the paged permutation should be the default rather than the sparse-slice option.
- The generation-GC scheme once manifest-reference sharing is adopted (refcount vs mark-sweep across manifests), and its interaction with pinned generations — deferred with the reference-sharing switch itself (spec §7).
- Tier 3's ι-split holes (overlay keying, WAL replay) — owned by plan §14, tracked here only as prerequisites.
