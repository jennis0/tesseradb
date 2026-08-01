# Slices and multi-table shards — design

**Date:** 2026-08-01
**Status:** Brainstormed design, pending independent review. Positioned like the client-interaction spec: a forward design that feeds later amendments into the corpus, not an edit to the architecture design today. Where this document and the corpus disagree, the corpus governs until the amendments in §11 are folded in by owner decision.
**Reads against:** architecture design §5.1, §7.2, §9, §11–§13, §16, Appendix C; contracts §2.2, §2.6, §3.1; lifecycle §3, §5; implementation plan §14; probes/optimisations §4.

---

## 1. Summary

Two capabilities fall in the same place and are designed together:

- **Slices** — named, orthogonal coordinate systems over the shared entity space: disjoint temporal sets, multiple embedding spaces, multiple datasets. A point may belong to several slices with independent coordinates, one identity, one set of metadata and labels. This generalises §9's temporal slices; the engine does not distinguish the three flavours.
- **Multi-table layout** — within a shard, points with identical term signatures grouped into physically separate row tables, chosen at build/compaction time from the current signature histogram, to push row-table reads above ~95% contiguous. This is plan §14's "row-space signature-major layout" taken to physically separate tables, and it is explicitly a **shard-implementation decision** — unlike §12 partitions, which are a security boundary.

The unification both rest on: **a *table* is the one physical unit of row space** — its own Morton ranking, tile table and candidate lists, occupying a reserved sub-range of its slice's row-ID space. Ingest segments are tables keyed by epoch; signature-group tables are tables keyed by group; their combination is still just tables. A slice is a named coordinate system plus a set of tables. A tile is a set of ranges, one per table it intersects — already the Phase 1 type. §7.2 (r22)'s cross-segment merge is the cross-table merge, unchanged. The mask never learns tables exist; it meets them only at the permutation.

## 2. The table

A **table** is keyed `(slice, group, epoch)`:

- `slice` — which coordinate system its rows belong to;
- `group` — a promoted signature-group ID, or `residual`;
- `epoch` — ingest generation (what the corpus today calls a segment).

What the corpus calls a *segment* becomes the table `(slice, residual, epoch)` — the degenerate case with an empty promotion set. A deployment whose histogram promotes nothing (the author-like policy: 1.54M signatures over 2.42M items) stays in that case forever, running today's code path. The walking skeleton is the single-table special case, not a casualty.

**Row-ID space.** One `u32` space per slice per bundle generation. Each table receives a base offset at build/compaction, **aligned to a multiple of 2¹⁶** so Roaring containers never straddle tables — per-table operations stay O(containers touched) with no boundary case. Offsets are generation-scoped; this is free because row IDs already renumber at compaction and mask caches are already generation-keyed (r19). Alignment waste is bounded by `tables × 2¹⁶` IDs, negligible at any plausible table count.

**The mask is table-blind.** Entity→row remains one permutation array per slice; a row-space mask fragment is one bitmap per slice covering all its tables; counts are range cardinalities summed across tables; marks are the global bottom-*m* by `tessera_id` across tables — §7.2 (r22)'s merge, which is exact, verbatim.

## 3. Slices

**Definition.** A slice is `{name, optional gate label, projection provenance, table set}`. The manifest carries a slice registry. §9's temporal slices become the first instances rather than a special case; r17 (current credentials govern every slice) and §9's rejection of time-in-the-Morton-code both stand unchanged.

**What is shared, what is per-slice.** Shared, in entity space: identity, the term index, labels and generating sets, metadata, the mask. Per-slice, in row space: coordinates, the permutation array, the table set (tile tables, candidate lists), θ (r24 already gives one θ per slice). This is §5.1's factoring stated as a rule: *entity space is the invariant plane; a slice owns everything downstream of the permutation and nothing upstream of it.* One token authorises across all slices; slice membership is implicit — the permutation's sentinel — never a stored bitmap.

**Addressing.** The slice is named in the request body/query of each verb, not a header. This amends contracts §3.1's `x-tessera-slice`: a semantic parameter belongs in the request proper. Engine-side cache keys already carry the slice explicitly (r19), so nothing else moves.

**Gating.** A slice's gate is a **label**, evaluated by the identical machinery as an item's label — plugin evaluation, conservative label join, yielding a required term set. Not a parallel mechanism, and not necessarily a single term. Semantics:

- If the principal's satisfied set fails the gate: the slice is absent from slice discovery, and a request naming it is indistinguishable from naming a slice that never existed — C4's closure applied at slice granularity, fail-closed.
- The gate is **conjunctive**, never substitutive: items inside a gated slice remain individually governed by their own labels. A gate can only narrow, never widen — the I12 direction.

**Ingest contract.** An ingest row carries a `{slice → (x, y)}` map instead of a single coordinate pair; membership in a slice is presence in that map. Adding an existing entity to a further slice later is ordinary ingest, resolved by `external_id`, landing in that slice's current epoch table.

**Slice creation is a runtime control-plane operation.** A WAL'd registry entry creates an empty slice; points join by ordinary ingest carrying coordinates for it; flush gives rows. Entity space is untouched, so the cost is row-space artifacts only.

**Caller obligations (extends §2.4).** Projection stability within a slice; entity identity *across* slices is by `external_id` — "the same point in two embedding spaces" is exactly the caller saying so at ingest.

**Deliberately out of scope.** Coordinate updates within a slice (a row move; same rarity class as predicate changes, deferred to the same compaction machinery). Removing an entity from a single slice is a caller-facing API question deferred with it.

## 4. Promotion policy

A signature group is promoted to its own table iff

```
count(group) ≥ max(abs_min, p × corpus_size)
```

- The **proportion floor `p`** bounds the promoted count at ⌊1/p⌋ **structurally** — no histogram shape can exceed it.
- The **absolute floor `abs_min`** stops small deployments from promoting noise.
- A **hard cap** on promoted-table count exists as independent config, so an operator can clamp fan-out below ⌊1/p⌋ regardless of the histogram.
- **Hysteresis**: promote at ≥ the threshold, demote below half of it, so groups oscillating around the floor don't thrash rows at every compaction.
- All four parameters are **config with measured defaults, never constants in code**. `p` in particular is a measured parameter — §9's fan-out sweep is what sets it, because the high-coverage principal pays the fan-out (§9).

Evaluated at each build/compaction from the current histogram. On the synthetic corpus (top 500 groups = 82.4% coverage) a modest `p` captures most of the coverage with far fewer than 500 tables; on the author-like histogram nothing promotes and the layout degrades to today's, which is the correct behaviour for that shape.

## 5. Lifecycle

**Grouping is a property of the compacted base, not of the write path.** A flush emits one pending epoch table per slice touched — Morton-sorted, group-**agnostic**, tiny. The write path (WAL, group-commit allocation, ack contract) is untouched by this design. Live tables per slice are bounded by `N_promoted (≤ ⌊1/p⌋, hard-capped) + live epochs (bounded by compaction cadence, as today) + 1 residual`.

**Compaction is where grouping happens.** At each compaction: re-evaluate the promotion set from the current histogram; merge pending epochs into per-`(slice, group)` base tables; carve newly-promoted groups out of the residual; fold demoted groups back in. All of it is row movement within a slice at a generation boundary, where row IDs already renumber and caches already invalidate by key. Minor merges (epoch-into-epoch, never touching the base) stay under §11.3's existing bound.

**Rapid ingest fragments at today's rate, by construction.** Group-agnostic epochs mean sustained ingest adds tables exactly as it adds segments today — not epochs × groups. What is new: **the contiguity win lives only in the base**, so under rapid ingest with lagging compaction the contiguous fraction degrades gracefully toward the unpromoted state — a performance decay, never a correctness or disclosure event. The knob is compaction cadence; `/control/status`'s fragmentation metric is the observability hook.

**Predicate changes ride the existing third retirement rule.** A signature change may leave an item in the wrong group table until its compaction fold. That is safe, not merely tolerable, because of this design's load-bearing invariant:

> **Tables are performance layout, never an authorisation boundary.** Visibility is decided entirely by the entity-space mask meeting the permutation; the table a row physically occupies has zero authority. An item in the wrong table is a contiguity regression, never a disclosure.

This is the property that distinguishes tables from §12 partitions, which *are* an isolation boundary with the overlay covering moves. The lifecycle's three deny-retirement rules gain no fourth case: deletes and suppressions are entity-space mechanisms and do not know tables exist.

## 6. Whole-table shortcuts: excluded, with the safe half specified for later

Two directions, sharply different:

- **Rule-in** (group signature satisfied → serve the whole table, skip mask intersection) is plan §14's flagged shortcut: **fail-open** under suppressions, deletions and predicate-change overlays, and it buys little — visible rows in a promoted group are contiguous for the gather whether or not the intersection ran, and bitmap ops over a contiguous group touch ~2 containers. **Excluded outright.**
- **Rule-out** (signature unsatisfied → skip the table) is fail-closed by direction, but not unconditionally safe: a predicate change can make an item visible while it still sits in a group whose signature the principal fails; skipping that table hides an acknowledged change and breaks the caller-observes-own-write rule. The guard is a per-group **dirty bit** (any overlay entry touching the group since base → normal bitmap path). And the prize is small, because invisible tables already cost only absent-container checks.

**Specified as optional, not built:** a per-session **table visibility vector** — one gate evaluation per promoted group yielding satisfied / unsatisfied / dirty — used for rule-out only, dirty-bit-guarded, adoptable only after the conformance suite grows a canary for it. The layout earns its keep through contiguity alone.

## 7. On-disk layout, mmap and the write path

The corpus's storage model (§10.1: one file per column per segment, raw fixed-width Arrow IPC buffers, page-aligned, synced to NVMe, mmap'd zero-copy, immutable versioned prefixes) survives intact; multi-table forces exactly one decision inside it.

**Per-table column files, not slice-wide files.** Two candidate serialisations of a slice's row space:

- *Slice-wide*: one file per column per slice, tables as extents at their base offsets, alignment gaps as sparse-file holes. Preserves today's mmap count and pure-arithmetic gather (`row × width`), but carving one group out of the residual at compaction rewrites the whole slice's columns — ~8 GB per column at 10⁹ — a write-amplification cliff attached to the most routine compaction event.
- *Per-table*: one file per column per table. Compaction rewrites only the tables it touches, and — the substantive win — **an untouched table is carried into the next generation by manifest reference, not by copy**. A stable promoted group is precisely what compaction rarely touches, so the tables that pay the layout's rent are the ones that stop costing anything to carry. Costs: the gather goes through a table directory (row ID → (file, local offset), a small sorted lookup over base offsets — cacheable, and the fan-out cost is already priced in §9); and mmap count rises to tables × columns × slices — thousands at the hard-capped counts, well inside VMA and fd limits.

Per-table files are the recommendation. Page alignment and container alignment both hold trivially per file, since each file starts a table.

**The write path, itemised:**

- **WAL: untouched.** Entity-space; tables are row-space artifacts downstream of flush.
- **Flush: untouched in shape.** §5's group-agnostic epoch tables mean flush writes one small table per touched slice — no per-group file spray at the flush cadence, which is the §5 choice paying a second dividend.
- **New, priced:** a point belonging to *k* slices writes coordinates into *k* epoch tables — write amplification proportional to slice membership. Inherent to independent coordinates, bounded by the ingest map, and visible at flush rather than on the request path.
- **Compaction:** carve/fold writes only the residual and the promoted tables whose membership changed; untouched tables carry by reference.

**The honest price: generation GC.** Manifest-reference sharing breaks "delete the old prefix" — unreferenced table files need refcounted or mark-sweep collection across manifests. This is deferrable: below ~10⁸ items, strict prefix-copy (rewrite everything, delete old prefix) remains simple and affordable, and the switch to reference-sharing is a serving-node and build concern invisible to the wire. The trigger for adopting it is compaction write volume, observable operationally; it needs no format change if the manifest's table entries are keyed references from day one — which is therefore on §12's foreclosure list.

## 8. Identity stability

Slices sharpen the value of stable identity: the entity is the join key across coordinate systems, and `tessera_id` is already identical for an entity in every slice (the slice is not an input to the keyed bijection) — cross-slice join on the wire works today. The remaining instabilities are the three break events, and the honest position is tiered, because stability, dense machinery and placement freedom cannot all live in one integer — *permanent identity, dense machinery, placement freedom: pick two per integer* — which is precisely what plan §14's ι-ordinal split answers with two.

**Tier 1 — proposed ruling, self-contained, priced at zero today:** *repartitioning preserves identity by default.* §12.5's reindex breaks `tessera_id` only because the rebuild takes the opportunity to reallocate entity IDs, not because moving an item between partitions requires it — there is a single global allocator across partitions (§16, r21). An identity-preserving repartition keeps entity IDs and hence wire IDs; identity-breaking reallocation is demoted to what plan §14 already calls it — an escape hatch, never the default. Contracts §2.2's "must advance the epoch on any partitioning change" relaxes to "on any build that *reallocates*, and on key rotation".

**Tier 2 — kept open at zero cost:** the 64-bit identity input is *reinterpretable* as **birth-block** rather than placement — the prefix records where an ID was born, never where the item lives. Today the prefix is 0 either way; no allocation policy is pinned. Recorded consequences of the birth-block reading, for when the reshard design is written:

- Allocation stays trivial and uncoordinated (each shard mints from blocks it owns), with per-shard u32 headroom — the same exhaustion arithmetic as §16's shard-local sketch, so the exhaustion objection to "global allocation" does not apply.
- Migration (rebalancing, resharding **in either direction**) never renumbers: a retired shard's blocks freeze — never re-issued, never minted from again — and shrinking a deployment becomes symmetric with growing it, with no identity epoch fired for either. Placement-coupled schemes fail exactly this scale-down question.
- The cost is scatter: a shard's population comes to span multiple birth prefixes, so its local bitmap universe goes sparse in the upper bits and §11.1's signature-sorted contiguity does not survive assembly from foreign-born IDs. Roaring absorbs the sparsity (absent containers cost nothing); the contiguity loss is the real price and is what Tier 3 exists for. Sustained migration drift is observable via `/control/status`'s fragmentation metric, the trigger for a consolidating compaction.

**Tier 3 — recorded as contingent:** wire-identity stability across *resharding* is achievable **iff the ι-ordinal split lands** — the only structure in which scattered permanent IDs are livable, because postings, masks and the permutation index the dense renumberable ι and never the scattered layer. The split's two named safety holes (overlay keying is fail-open under renumbering; WAL replay) are **prerequisites, not footnotes**; nothing in this design depends on them closing, and the decision point is the reshard design at §13.4's own trigger, not now.

Key rotation remains the one irreducible break — invalidating identifiers is what rotation is for. `external_id` remains the durable key throughout.

## 9. Performance analysis and the fan-out sweep

**Where the win comes from.** Grants align with signature groups, so a grant-aligned viewer's visible rows in a promoted table are dense and contiguous: gathers become sequential reads, bitmap merges touch ~2 containers instead of scattered hundreds (the measured ~7,500× container-visit difference), and `entity_to_row` becomes near-monotone within groups — plan §14's permutation-compressibility consequence, unlocking Elias-Fano-class encoding behind the contracts reader interface.

**Who pays: the high-coverage principal** — already the system's worst path (the 588 ms head-principal figure). For them the layout is a transfer, not a free win: per-tile boundary ranks go from 2 to 2 per visible table; the bottom-*m* merge widens to ~⌊1/p⌋ sources; and gather locality *inverts* — one nearly-contiguous span per tile becomes N ranges in N files. Whether the transfer nets positive depends on the coverage distribution of real traffic, which is a thing to measure, not assume.

**The fan-out sweep (extends plan §14's gather probe).** Synthetic corpus; promoted-table count swept N ∈ {1, 8, 32, 64, 128, 256}; three principal shapes — grant-aligned narrow, mixed, full-coverage; measuring the count pyramid, the bottom-*m* merge and the column gather at overview zoom. The knee of the full-coverage curve sets the hard cap; the crossover against the grant-aligned win sets the default `p`.

## 10. Leak analysis

**The framing precedent: row-space layout is already a full-corpus function.** Morton rank depends on every item's geometry, and it has never been a leak because row IDs never cross the trust boundary. The promotion set is the same class of artifact — derived from the full histogram, invisible from outside. What needs checking is whether tables *escape* row space. Three channels:

**Value channels — closed by an indistinguishability property**, stated in conformance-checkable form:

> **Single-table indistinguishability:** for any principal, any request, the response under a multi-table layout must equal, after the suite's canonicalisation, the response the single-table layout produces for the same bundle content and generation. Layout is a physical decision with no representational residue on the wire.

Payload ordering is deliberately *outside* the property: the mark set is what §7.2 defines; order is presentational and not an invariant this system cares about. The differential oracle tests the property mechanically — build the same corpus both ways, canonicalise, diff.

**Timing and ordering — one accepted register candidate.** Per-tile work varies with the viewer's *own* visible-table count (a function of their own mask; I2-clean). The residual: a viewer might resolve, through timing (an invisible table costs an absent-container check, sub-microsecond against tens-of-ms responses) or through physical ordering of their own visible items, that some group is promoted — i.e. that it holds ≥ `p × corpus_size` items. A threshold fact about the full corpus, C1's shape, via a side channel. **Proposed as a new Appendix C entry, argued accepted on magnitude** — listed explicitly because the register's rule is that anything not in the table is a bug.

**Slice channels — settled in §3:** gated-slice existence closes by the discovery rule (C4's shape at slice granularity); cross-slice `tessera_id` stability is deliberate linkage of an item to itself (C17's argument extends); slice membership of a visible item is caller-supplied data shown only through the mask.

Net: no change to the five-verb surface; one new accepted register entry; one new conformance property that is among the strongest tests in the suite.

## 11. Proposed corpus amendments (on fold-in, owner decision each)

| Document | Change |
|---|---|
| Architecture design §5.1, §9 | Slice generalised to named coordinate system with gate label; temporal slices become instances; runtime slice creation |
| Architecture design §11.2/§13 | Segment → table `(slice, group, epoch)`; container-aligned generation-scoped base offsets |
| Architecture design §12.5, §16 | Tier 1 ruling (identity-preserving repartition; reallocation demoted to escape hatch); Tier 2 birth-block note against the exhaustion entry |
| Appendix C | New accepted entry: promotion-set threshold facts via timing/ordering. Slice-existence disclosure noted under C4's closure |
| Contracts §2.2 | Epoch-advance rule relaxes to "reallocation or rotation" |
| Contracts §3.1 | `x-tessera-slice` header → `slice` field in request body/query; slice discovery endpoint masked by gate |
| Contracts §3.4 (ingest) | Coordinate map `{slice → (x, y)}`; slice-creation control operation |
| Architecture design §10.1 / system architecture (packaging, lifecycle) | Per-table column files; manifest table entries as keyed references; generation GC note (strict prefix-copy until compaction write volume triggers reference-sharing) |
| Lifecycle | Compaction gains promotion-set evaluation and group carve/fold; note that tables add no fourth deny-retirement case |
| Conformance design | Single-table indistinguishability differential build; later, the rule-out canary |
| Implementation plan §14 | Signature-major entry superseded by this design (physical tables, threshold promotion); gather probe extended to the fan-out sweep |

## 12. Adoption gates

The layout is a per-deployment build decision, **off by default** (empty promotion set = today's layout, same code path). Gates for turning it on:

1. Working conformance suite (Phase 2), including the canonicalised single-vs-multi-table differential build.
2. The fan-out sweep (§9) run and the knee found; `p`, `abs_min`, hard cap and hysteresis set from it as config defaults.
3. A real signature histogram showing the knee — per design r18, deployment guidance, not producible by this project.
4. Rule-out skipping stays out until the suite has its canary; rule-in stays out, full stop.

**What Phase 1/2 must not foreclose (payable now, all cheap):** tile lookups typed against a set of tables (already true via segments); the permutation's representation stays behind the contracts reader interface; table base offsets container-aligned from the first multi-segment implementation; the manifest's segment entry gains a `group` key, always `residual` until promotion exists; the slice addressed in the request body from the start.

## 13. Open questions

- Removing an entity from a single slice: API shape and whether it is a deletion variant or an ingest-map update. Deferred with coordinate updates.
- Whether the slice gate label participates in `V_total`/θ anchoring in any way beyond membership (believed no: the gate only decides reachability, and θ is per-slice over rows already).
- The fan-out sweep may show the count pyramid and the merge kneeing at different N; if so, whether the cap should differ per verb.
- Tier 3's ι-split holes (overlay keying, WAL replay) — owned by plan §14, tracked here only as prerequisites.
- The generation-GC scheme once manifest-reference sharing is adopted (refcount vs mark-sweep across manifests), and its interaction with pinned generations — deferred with the reference-sharing switch itself (§7).
