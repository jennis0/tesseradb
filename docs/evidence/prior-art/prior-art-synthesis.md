# Prior Art: Synthesis and Build-vs-Buy Verdict

**Companion to** `architecture.md` (current at r14; written against r5, with a second research pass on 2026-07-26 — see section 9).
**Based on** four independent research reviews: `prior-art-1-search-engines.md`, `prior-art-2-visual-analytics.md`, `prior-art-3-databases.md`, `prior-art-4-authorization-disclosure.md`.
**Date:** 2026-07-25.

---

## 1. The verdict in one paragraph

**No existing technology or combination of technologies can replace this build, and the reason is consistent across all four domains: every mature system in the field is architecturally committed to rebuilding the user's selection on every query.** The design's load-bearing move is the opposite — build the visible set once per session, then reuse it thousands of times while panning. That is not a feature anyone forgot to add; it is incompatible with the stateless-query-engine model that search engines, vector databases and OLAP engines are all built on, and it is incompatible with the pre-bake-once model that every large-scale scatterplot uses. Separately, four of the systems that come closest — Elasticsearch DLS, Solr JSON faceting, Vespa grouping, every SDC-derived tool — **document that they permit aggregate counts over unauthorised records**, which is a direct contradiction of the design's hard boundary rather than a performance shortfall. The build is justified. What is *not* justified is the belief that much of it is novel.

---

## 2. What is genuinely novel

Only two things survive scrutiny, and they are narrower than the design document currently implies.

**The composition, not the pieces.** Exact bitmap-derived per-tile counts, *plus* sampling performed after per-user masking, *plus* nesting across zoom levels, all at once. Each ingredient has prior art. Nothing combines them. The design doc should claim this composition explicitly and stop implying the ingredients are inventions.

**Containment-gated shared LLM summaries.** The gating rule itself is textbook — see §3 — but applying it to *shared, precomputed, LLM-generated cluster summaries served to differently-cleared viewers* appears genuinely unpublished. Microsoft GraphRAG builds exactly this artefact (hierarchical clustering producing "community reports") and **has no permission model at all**; every enterprise-search vendor sidesteps the problem by generating fresh per query and never sharing a derived artefact between users. This is the real claim, and it is worth making loudly.

Two further gaps in the literature, weaker but worth noting: nobody publishes numbers for materialising a 10⁶–10⁸-member visible set per user at hourly churn; and ACL-aligned clustering (cluster by permission-equivalence class first, then semantically) appears unpublished, despite being the obvious mitigation for the failure mode in §5.

---

## 3. What has a name, and what that name is

The single highest-value output of this research is vocabulary. Using established terms will make the design doc dramatically more credible to a security reviewer, and each term brings a literature with it.

| Design concept | Established name | Primary source |
|---|---|---|
| "Label served iff generating set ⊆ visible set" | **Conservative label join / Derivation Axiom**; also *high-water mark*, *DLM join*, *conservative taint propagation* | Denning et al., *Views for Multilevel Database Security*, IEEE S&P 1986: `V.level = ⊔ {z.level \| z ∈ V.source}` |
| Five ANDed dimensions, one large | **Compartmented / lattice-based MAC**; category-set component of Bell–LaPadula; **dominance** | BLP; ISM.ACES (IC CIO, V2021-NOV) |
| Per-document boolean predicate over tokens, no NOT | **Security label / `ColumnVisibility`** | Apache Accumulo |
| Normalising predicates to DNF terms and indexing them | **Boolean expression indexing**; "term" = their *conjunction*, "unit" ≈ their *key* set | Whang & Garcia-Molina, VLDB 2009; Fontoura et al., SIGMOD 2010 |
| Compiling a policy into a data filter | **Partial evaluation**, producing residual policy in DNF | OPA; Oso "data filtering" |
| Refusing rather than silently filtering | **Non-Truman model** | Rizvi et al., SIGMOD 2004 |
| Filtering results to caller ACLs | **Security trimming** (pre- vs post-trimming) | SharePoint 2007 SDK; Büttcher & Clarke, USENIX FAST '05 |
| Morton sort as clustered index + range skip structure | **Index sorting + doc-values skip index** | Lucene `IndexWriterConfig.setIndexSort`, `DocValuesSkipIndexType.RANGE`; Elasticsearch TSDS |
| Precomputed accessible-set materialisation | **Leopard index** | Zanzibar, USENIX ATC 2019 |
| Per-point z-independent retention scalar | **`feature_minzoom`** | tippecanoe |
| Over-retain N× then filter | **`--retain-points-multiplier` / multiplier clusters** | tippecanoe 2.41.0+ (Felt) |

Two citations are worth putting in the design doc verbatim. **Büttcher & Clarke (2005)** proved that post-filtering leaks unreadable-file content through relevance scores and rank shifts — that is the "not even aggregate counts" requirement, demonstrated as necessary twenty-one years ago, and no RAG paper cites it. **Kenthapadi, Mishra & Nissim (PODS 2005)** proved that a refusal is itself a leak channel whenever the refusal decision depends on invisible data — the containment rule is *simulatable* because the decision is a function of the visible set and the generating set, both on the user's side. That is a genuine, non-obvious virtue most suppression schemes lack.

---

## 4. Could anything replace part of the build?

**Access control layer: no, and the blocker is correctness rather than performance.**

Elasticsearch is the only surveyed system with real per-document security, and its official limitations page states a user "could still... count how many inaccessible documents contain a given term," with scoring using corpus-global statistics that ignore the role query. Solr is worse: `domain.excludeTags` is a *supported feature* for computing facet counts outside the security filter. Vespa and Quickwit have no per-document access control at all. Every SDC tool transmits something about invisible records by design.

Beyond correctness, every system keys its filter cache on (segment, query), so per-user hourly-recomputed predicates get zero reuse — in the most expensive possible way, a dense uncompressed bitset over `maxDoc` (~125 MB at 10⁹), rebuilt per segment, invalidated by every merge. Elastic's own `DocumentSubsetBitsetCache` javadoc anticipates this scenario and warns about it; [issue #46817](https://github.com/elastic/elasticsearch/issues/46817) documents a 30 ms → 26 s regression from it.

**Reverse index / accessible-set materialisation: no.** SpiceDB measures ~100ms for LookupResources at 100–250K relationships and **abandoned** its Roaring-bitmap materialisation proposal as "doesn't reach the scale to truly solve this problem long term." OpenFGA caps ListObjects at 1,000 results by default. AuthZed Materialize — the commercial Leopard reimplementation — solves it by becoming a change-data-capture feed into a store you build yourself, which is most of this system.

**Storage and mask layer: no.** Of every database examined, exactly one exposes `range_cardinality` as a first-class primitive: **`pg_roaringbitmap`**, which has `rb_range_cardinality`, `rb_rank`, `rb_index`, and an `rb_select` that fuses range-restriction with select-k. Even it stores the bitmap as a *column value* deserialised per query. ClickHouse is the sharper irony: it vendors CRoaring, which has every needed primitive, and calls none of them — implementing `bitmapSubsetInRange` as a linear scan from zero. No system anywhere lets you pin a live session-scoped bitmap into its execution context.

**Frontend and tiling: no, but closer than expected.** deepscatter is the closest architectural match and is blocked by **CC-BY-NC-SA** (noncommercial, apparently deliberate to protect the Atlas product). Nomic Atlas is the closest *product* match and has dataset-level-only RBAC. Mosaic is the best-engineered and best-benchmarked option and has no security model at all, plus answers 10⁹ with rasterization rather than sampling — a different product. Apple's Embedding Atlas is the best renderer and labeller under a usable licence but is brute-force by design, capping around 10⁷ with no server-side filtering seam.

**Row-level access control across the entire visualisation category: zero systems. Not one.** The granularity ceiling is universally the dataset.

---

## 5. What the research changes about the design

**The DNF expansion factor is now the highest-value experiment.** Fontoura et al. (SIGMOD 2010) measured DNF normalisation becoming "infeasible beyond depth 2" and exceeding available RAM at depth 3. The design's sixty-four-term cap and default-deny overflow list (§6.2) is the right shape, but the cap's adequacy is entirely unmeasured. If predicate authors can nest deeply, ingest-time normalisation is where this fails — not the query path. Measure the expansion factor on real predicates before anything else.

**Label creep is the named, predicted failure mode of the label lattice.** The information-flow literature calls it exactly that. If clusters form on semantic similarity alone, the intersection of readers over a cluster collapses toward empty at 10⁷–10⁹ documents and most labels become unservable to almost everyone. The design's term-based generating sets already mitigate this by construction — generating sets are built per (cluster, term) rather than per cluster — but the risk should be named alongside label gating, with the ACL-aligned-clustering fallback recorded as the escalation. *(Now §7.8. The scaling analysis argues this is not only a label-creep mitigation but the mechanism that makes masks tractable above 10⁹.)*

**Toponymy's prompts are built from *sampled* documents.** So "every item it was generated from is visible" is a check over a small recorded sample set, not over whole cluster membership. That makes the gate cheap but weaker than the current wording implies. The design (§7.6) should state explicitly which set is recorded as the generating set — the prompt sample or the full cluster — because they differ materially and the choice is a security decision.

**Lucene's segment lifecycle should be adopted wholesale, not re-derived.** `TieredMergePolicy`'s parameter set and its separation of natural / forced / deletes-driven merges; `BPReorderingMergePolicy`'s decorator shape with `minNaturalMergeNumDocs` and skip-on-RAM-exhaustion — which restructures the nightly re-rank into a continuous property of sufficiently large merges, a strictly better shape than a scheduled re-rank job; and `SearcherLifetimeManager` + `SnapshotDeletionPolicy` as the canonical session-pinning pattern, which is exactly what **I11** specifies and Lucene has had for years.

**CRoaring's frozen format is the mask-loading answer.** `roaring_bitmap_frozen_serialize` / `frozen_view` gives zero-deserialisation, zero-allocation mask loading — write at hourly recompute, mmap and take a frozen view at session start. This is precisely what ClickHouse fails to do and what Druid's `ImmutableRoaringBitmap`-over-mapped-buffer does correctly. §7.4 should specify it.

**Two calibration constants worth stealing:** ClickHouse's small-set threshold of **32** for the long tail of tiny per-term bitmaps, and Druid's **0.12** probe-to-dictionary ratio heuristic for choosing between per-term lookups and a single sorted merge during mask build (10⁴ grants against 10⁶ terms is 1%, so lookups win clearly; against 10⁵ terms it is 10%, right at the boundary — measure).

**Density is being thrown away.** Already added as §6.3 in rev 5, but the visual-analytics review independently reached the same conclusion: uniform k-per-tile flattens the one thing a UMAP scatter communicates, and exact per-tile counts are already free.

**Fail-closed should be an explicit requirement.** Azure AI Search is the only vendor documenting it — if ACL evaluation fails, "the service returns 5xx and does not return a partially filtered result set." Everyone else fails open. Elastic's DLS fails open in a specific documented way: omitting the `query` parameter entirely disables document-level security.

---

## 6. On tippecanoe's multiplier clusters, and why the design's mechanism is stronger

The visual-analytics review identified tippecanoe's `--retain-points-multiplier` as the only published system that samples after filtering with cross-zoom stability, and correctly identified its limit: it degrades gracefully only while the mask's pass rate stays above roughly 1/N, after which clusters exhaust and tiles go empty.

That limit does not apply to the design as specified in rev 5 §6.2, and the difference is worth preserving explicitly because it is the kind of thing an "optimisation" would remove. Tippecanoe's multiplier clusters are a **fixed-width structure with no recovery path**. The design's candidate lists are an **optimisation over an exact definition** — "the k lowest-priority items in this tile inside the user's mask" — with a descent fallback: when fewer than k survive, merge the four children's candidate lists, recursing until enough survive, terminating in a direct scan of the masked row-ID range at the deepest stored level. A user with 0.1% coverage descends four levels (256 nodes visited) rather than seeing an empty tile.

**Dropping the exact path to "simplify" would reintroduce exactly tippecanoe's failure mode**, and the design doc now says so where the selection routes are specified (r14, §7.2). *(Corrected 2026-07-27: this originally claimed the cliff was replaced by a cost increase proportional to log(1/coverage). Descent depth is logarithmic but node count is the geometric sum, so the cost is proportional to 1/coverage — see the addendum to review 2.)*

---

## 7. Sobering context

Three well-resourced teams built an embedding-map product and exited:

- **Arize Phoenix removed its embedding visualisation entirely** in v13.0.0 — "Model inferences, dimensions, embeddings, and the pointcloud (UMAP) visualization have been removed" — while the project itself thrives, having pivoted to LLM tracing and evals.
- **Lilac was archived** on 25 July 2025, ~16 months after the Databricks acquisition.
- **Aquarium Learning** wound down into Notion.

The differentiator here is not the scatterplot. It is the access control — which is also the only part nobody else has built.

Two further calibration points. **10⁴ grants per user exceeds every published per-user limit in commercial enterprise search by 5–50×** (Microsoft Graph: unpredictable above 2,049 external group memberships, HTTP 400 above 10,000; Vertex AI Search: 3,000 readers per document; Kendra: 200 ACL entries). And **nobody has demonstrated 10⁹ identifiable, filterable, labelled points** — Falcon and Mosaic reach 10⁹ by aggregating to bins, deepscatter reaches it on a static star catalogue with no masking. Prove 10⁸ first and treat 10⁹ as a research risk rather than a roadmap item.

---

## 8. Recommended actions, in order

1. **Measure the DNF expansion factor on real predicates.** Fontoura's result makes this the highest-risk unmeasured assumption in the design, ahead of the three counts in §13.
2. **Rewrite the design doc's novelty claims** to cite Denning 1986, Whang/Garcia-Molina 2009, Accumulo and ISM.ACES, and to claim the composition and the shared-summary application rather than the principles. A security reviewer will find the lineage; the document is stronger for having found it first.
3. **Adopt Lucene's segment lifecycle** — `TieredMergePolicy` parameters, merge-type separation, `BPReorderingMergePolicy`'s decorator shape, `SearcherLifetimeManager` + `SnapshotDeletionPolicy` for I12.
4. **Specify CRoaring's frozen format** in §7.4 as the mask-loading mechanism.
5. **Name label creep beside label gating** (now §7.8) and record ACL-aligned clustering as the escalation path.
6. **Decide and document** whether Toponymy's generating set is the prompt sample or full cluster membership.
7. **Add fail-closed as an explicit requirement**, citing Azure AI Search as the only vendor precedent.
8. **Read "Effective and Efficient Bitmaps for Access Control"** (IEEE 6824485) — paywalled, title suggests direct relevance, unread by this research.

---

## 9. Research coverage

All four reviews completed. A second pass on 2026-07-26 closed the three database-review gaps and added Accumulo; see `prior-art-3-databases.md` §4–§7 for the detail, and section 10 below for what it changed.

**Closed:**

- **Row-level security** — researched and measured (review 3 §4). Rejected: PostgreSQL discloses exact excluded-row counts and invisible-cluster density through `EXPLAIN`, and costs 3,340× on the viewport query.
- **DuckDB as the mask-build engine** — researched and benchmarked (review 3 §5). The hypothesis holds, but only under a pair-table schema; the obvious query shape is ~3,000× slower.
- **Apache Doris / StarRocks bitmap semantics** — verified at source level (review 3 §6.1). Negative, and Doris was over-rated by the earlier draft.
- **Apache Accumulo** — researched at source level (review 3 §7). Not a viable backend; the best security model in the survey.

**Still open:**

- **AWS Cedar partial evaluation** — not researched.
- **NATO STANAG marking specs, ACCM/SAP compartment tooling** — not researched.
- **Ory Keto, Permify reverse-index performance** — no published figures found (an absence, not a negative result).
- Whether **OpenSearch bitmap terms filtering composes with filter-level DLS** — undetermined.
- Whether **Druid's `SCALAR_IN_ARRAY` retains bitmap acceleration** — unverified; the one open question that could move the Druid assessment.

Per-section unverified items are consolidated at the end of review 3.

---

## 10. What the second pass changed

**The build-vs-buy verdict is unchanged and better evidenced.** Three findings sharpen it.

*Row-level security is worse than nothing, not a weaker alternative.* A fully patched PostgreSQL discloses the exact count of policy-excluded rows via `EXPLAIN ANALYZE`, and discloses the density of an invisible cluster at 1,500× above background via plain `EXPLAIN` — because planner statistics are computed over all rows and the viewport's float comparisons are leakproof, so CVE-2019-10130's fix does not gate them. That is precisely the disclosure this design forbids, through a supported unprivileged interface, on top of a 3,340× slowdown.

*The CRoaring pathology is now confirmed three times over.* ClickHouse, Doris and StarRocks each vendor a CRoaring exporting `range_cardinality`, `rank`, `select` and `frozen_view`, and each implements its own range restriction as an element-at-a-time scan from zero while calling none of them. StarRocks even calls `roaring64_bitmap_range_cardinality` correctly in its Iceberg deletion-vector path, three directories from the SQL bitmap layer that scans linearly. This is not an oversight to work around: no stateless engine wants a rank/select-addressable bitmap, because none holds one across queries.

*Accumulo is the security model to imitate.* It is the only system in the whole survey whose default read path satisfies the hard requirement, and it gets there structurally rather than by care — every aggregating iterator is stacked above the visibility filter and cannot observe what it filters. Its access model is otherwise unremarkable: no visibility index of any kind, per-cell evaluation inside scans that were happening anyway, no way to count visible cells in a range without visiting each one, and sampling that runs *below* the visibility filter, which is I7 inverted.

**One finding changes the design work rather than the verdict.** The mask build must run against a pre-exploded `(entity_id, category_id)` pair table with a hash semi-join. The natural formulation — array containment over a list column — is ~3,000× slower, because it rebuilds a hash set per row and never hoists the loop-invariant probe set. That is a schema decision independent of which engine runs the join, and it belongs in the implementation plan.
