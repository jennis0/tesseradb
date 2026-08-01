# Prior Art Review 1 — Search Engines and Inverted-Index Systems

**Scope:** Lucene, Elasticsearch/OpenSearch document-level security, Vespa, Tantivy, Quickwit, Solr.
**Question:** Could a Lucene-family system replace the access-control and indexing layer of the scatter-plot design?
**Date:** 2026-07-25. Findings verified against javadocs, source trees, GitHub issues/PRs and official docs.

---

## 1. Apache Lucene

### What it already does that the design re-implements

**Index sorting is a mature, first-class feature.** `IndexWriterConfig.setIndexSort(Sort)` — "Set the Sort order to use for all (flushed and merged) segments" ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/index/IndexWriterConfig.html)) — lays documents out on disk in a chosen total order, at both flush and merge. History: LUCENE-6766, merge-time sorting first, flush-time sorting added in **Lucene 6.5**, which is what made it affordable since pre-sorted segments merge by merge-sort rather than re-sort ([Zucchetto & Ferenczi, 21 Aug 2017](https://www.elastic.co/blog/index-sorting-elasticsearch-6-0)).

The cost is documented: merge-time-only sorting "can divide the total throughput of indexation by a factor of 2"; flush-time recovered "almost 65%"; real-world write degradation reported at "40-50%" in bad cases. Elastic's current reference repeats it ([index sorting settings](https://www.elastic.co/docs/reference/elasticsearch/index-settings/sorting)).

**Doc values with a range skip index** is the directly relevant piece. `DocValuesSkipIndexType.RANGE` "will record the min/max values per range of doc IDs" ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/index/DocValuesSkipIndexType.html)), feeding `DocValuesSkipper` with `minValue(level)`, `maxValue(level)`, `minDocID(level)`, `maxDocID(level)`, `docCount(level)` ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/index/DocValuesSkipper.html)). When the index is sorted by field X and X carries a skip index, a range predicate on X resolves to a doc-ID interval by hierarchical skipping. **That is the "Morton range ≡ contiguous row-ID range" invariant, implemented generically.**

**Roaring exists — but not the Roaring you mean.** `org.apache.lucene.util.RoaringDocIdSet` uses the same container taxonomy and 2^16 blocking ([source](https://github.com/apache/lucene/blob/main/lucene/core/src/java/org/apache/lucene/util/RoaringDocIdSet.java)). Its entire public surface is `Builder.add`/`build`, `iterator()`, `cardinality()`, `ramBytesUsed()`, `toString()`.

### What it does differently, and why

Lucene's Roaring is a **read-once iterator**, not a bitmap algebra. No `or`, no `and`, no `rank`, no `select`, and **no range-restricted cardinality**. `cardinality()` is whole-set only. Same for `FixedBitSet`: `cardinality()`, `approximateCardinality()`, static `intersectionCount`/`unionCount`/`andNotCount`, `nextSetBit(start, upperBound)` — but **no `cardinality(from, to)`, no rank, no select** ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/util/FixedBitSet.html)). This is deliberate: Lucene's abstraction is the streaming `DocIdSetIterator`, and set algebra is query composition evaluated lazily, not materialised bitmap operations.

Filter caching lives in `LRUQueryCache` ([source](https://github.com/apache/lucene/blob/main/lucene/core/src/java/org/apache/lucene/search/LRUQueryCache.java)). Verified mechanics:

- Key is **(segment core cache key, Query)**. Segment-scoped, so every merge invalidates.
- Value is `CacheAndCount`: `private final DocIdSet cache; private final int count;` — Lucene does cache an exact count alongside the bitset, but it is the whole-segment count, not a range count.
- Representation: `if (scorer.cost() * 100 >= maxDoc) return cacheIntoBitSet(...); else return cacheIntoRoaringDocIdSet(...);` — Roaring below ~1% density, `FixedBitSet` above.
- Eligibility: `new MinSegmentSizePredicate(10000)`, `skipCacheFactor = 10`.

**The policy layer is where per-user filters die.** `UsageTrackingQueryCachingPolicy` requires `frequency(query) >= minFrequencyToCache(query)` — minimum **2** for costly queries, **4** for `BooleanQuery`/`DisjunctionMaxQuery`, **5** otherwise, tracked in a 256-entry ring buffer. A predicate unique to one user and recomputed hourly is seen once and **never reaches the threshold**. Elasticsearch inherits and documents this ([node query cache settings](https://www.elastic.co/docs/reference/elasticsearch/configuration-reference/node-query-cache-settings)).

Read the right way: Lucene's cache will not *pollute* under this workload, because the policy declines to cache one-shot queries. But it gives **zero reuse**. There is no API to hand `IndexSearcher` a bitmap and say "this is the visible universe for this session."

*Unverified:* on `main`, `IndexSearcher` reads `DEFAULT_QUERY_CACHE = null`; in Lucene 9 this was a live `LRUQueryCache`. Which release flipped it could not be confirmed.

### Is Morton/Z-order an established Lucene pattern?

**No, not as a sort key — but the shape of the pattern is.**

- **Not Morton:** `LatLonDocValuesField` is frequently misdescribed as bit-interleaved. Lucene's javadoc says otherwise: "the upper 32 bits are the encoded latitude, and the lower 32 bits are the encoded longitude" ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/document/LatLonDocValuesField.html)). That is concatenation, not interleave. Solr's ref guide states the opposite and **appears to be wrong**.
- **Genuinely the same pattern:** Elasticsearch's TSDS "uses internal index sorting to order shard segments by `_tsid` and `@timestamp`", with "docvalue skippers... enabled on these fields because `tsid` and `@timestamp` are part of the index sort" ([TSDS docs](https://www.elastic.co/docs/manage-data/data-store/data-streams/time-series-data-stream-tsds)). A synthetic composite sort key chosen so a query's selection region is a contiguous doc-ID interval, with a skip index over that key — that is the design with `_tsid` in place of a Morton code. **The technique is established; the space-filling-curve instantiation appears to be novel.**

Also relevant: `BPIndexReorderer` implements recursive graph bisection, reordering for *posting compression* rather than range contiguity ([javadoc](https://lucene.apache.org/core/10_0_0/misc/org/apache/lucene/misc/index/BPIndexReorderer.html)).

### The concrete blocker

Lucene cannot answer "how many bits of this mask fall in doc-ID range [a,b)" without iterating. `Weight.count(LeafReaderContext)` is explicitly whole-leaf: "The default implementation returns -1... This indicates that the count could not be computed in sub-linear time" ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/search/Weight.html)). Per-tile counts become one query per tile — hundreds to thousands per pan frame, each O(matched docs).

---

## 2. Elasticsearch and OpenSearch document-level security

### How it works

ES DLS is a `LeafReader` wrapper, not a query rewrite. `DocumentSubsetReader` is "a reader that only exposes documents via `getLiveDocs()` that matches with the provided role query". Because enforcement is at `getLiveDocs()`, search, `_count` and aggregations all see the filtered view. Multiple role queries are OR'd. Architecturally the same trick as the design's mask.

**The cache key is where it comes apart.** In `DocumentSubsetBitsetCache`:

```java
final BitsetCacheKey cacheKey = new BitsetCacheKey(indexKey, query);
private static final class BitsetCacheKey {
    final IndexReader.CacheKey indexKey;
    final Query query;
```

Per-user-unique predicates yield **zero cache reuse**. Elastic's own class javadoc anticipates this verbatim:

> DLS uses `BitSet` instances to track which documents should be visible to the user... an index with 10 million document will use more than 1Mb of bitset memory for every unique DLS query, and an index with 1 billion documents will use more than 100Mb of memory per DLS query. Because DLS supports templating queries based on user metadata, there may be many distinct queries in use for each index, even if there is only a single active role.

Representation is a dense `FixedBitSet` over `maxDoc`, uncompressed, on heap — ~125 MB per distinct mask per shard at 10^9.

Settings ([security settings 8.19](https://www.elastic.co/guide/en/elasticsearch/reference/8.19/security-settings.html)): `xpack.security.dls.bitset.cache.ttl` = **2h**, `xpack.security.dls.bitset.cache.size` = **10% of heap**. Cache added by [PR #43669](https://github.com/elastic/elasticsearch/pull/43669); defaults changed from `50mb`/`168h` by [PR #50535](https://github.com/elastic/elasticsearch/pull/50535), merged 13 Jan 2020, motivated by [#49260](https://github.com/elastic/elasticsearch/issues/49260) which called the old default "effectively useless".

There is also an undocumented second cache in `DocumentSubsetReader`: `NUM_DOCS_CACHE`, `setMaximumWeight(1000)` per reader, whose producer carries the javadoc "This method is SLOW." — a full `cardinality()` walk.

### Real reports at scale

[elastic/elasticsearch#46817](https://github.com/elastic/elasticsearch/issues/46817), "DLS search performance/canMatch impact": **a 30 ms query taking 26 s for a DLS-filtered user.** Structural — `IndexShard.acquireSearcher` applies the reader wrapper during the `can_match` pre-filter phase, so bitsets get built for shards `can_match` is about to discard.

**No published benchmark of DLS cost as a function of the number of distinct role queries exists.** Elastic declined to quantify overhead when asked directly ([discuss thread](https://discuss.elastic.co/t/document-level-security-performance-impact/97380)).

### Terms lookup: not available

From [ES security limitations](https://www.elastic.co/docs/deploy-manage/security/limitations): "Any query that makes remote calls to fetch query data isn't supported, including... `terms` query with terms lookup". So 10^4 grants must be materialised inline, under `index.max_terms_count` (default 65536).

### The disqualifier: DLS does not protect counts

From the official limitations page:

> While document-level security prevents users from viewing restricted documents, it's still possible to write search requests that return aggregate information about the entire index. A user whose access is restricted to specific documents in an index could still learn about field names and terms that only exist in inaccessible documents, and **count how many inaccessible documents contain a given term**.

And: "Document level security doesn't affect global index statistics that relevancy scoring uses."

**The design's spec says nothing unauthorised may be transmitted, including aggregates and counts. Elasticsearch documents that it does not provide that guarantee.**

### OpenSearch

Three DLS modes — `lucene-level`, `filter-level`, `adaptive` (default) — via [security#1541](https://github.com/opensearch-project/security/pull/1541). In practice, verified against `opensearch-project/security` at `ce50241`:

1. `DlsFlsValveImpl.Mode.get()` accepts underscores (`lucene_level`); the docs tell you to write hyphens.
2. `plugins.security.dls.mode` is **not a registered node setting** — [security#3794](https://github.com/opensearch-project/security/issues/3794); startup fails with "unknown setting". Still unregistered on `main`.
3. **No DLS bitset cache at all.** `DlsFlsFilterLeafReader.DlsGetEvaluator` builds `new FixedBitSet(maxDoc)` inline per reader construction, with `searcher.setQueryCache(null)`.

One genuinely interesting capability: **roaring-bitmap terms filtering, OpenSearch 2.17** — `"value_type": "bitmap"` with a base64 RoaringBitmap, motivated in the docs at exactly this scale ("around 10,000 terms"). Caveats: filters field *values*, not doc IDs; yields no counts. Whether it composes with filter-level DLS is undetermined.

---

## 3. Vespa

### What it does better

Vespa's `predicate` field type is a mature production implementation of boolean-expression indexing ([docs](https://docs.vespa.ai/en/schemas/predicate-fields.html)) — the inverse orientation of the design but the same problem, and **strictly more expressive**: it supports NOT via a "Z-star" pseudo-feature, and requires no DNF normalisation, annotating the expression tree in place using an interval algorithm with a per-document min-feature (k-of-N) count. This is the Yahoo/Stanford interval-based Boolean expression index (Whang et al., *Indexing Boolean Expressions*, PVLDB 2(1):37–48, 2009, [PDF](http://infolab.stanford.edu/~euijong/vldb09.pdf)) — attribution inferred from algorithm shape; no Vespa source cites the paper.

Tuning: `arity`, `lower-bound`/`upper-bound`, `dense-posting-list-threshold` (default 0.40). The docs warn: "Using predicate fields is complex and tuning the configuration for performance requires insight in the underlying algorithms."

### The concrete blockers

**(a) Predicate query cost is Θ(corpus) regardless of selectivity.** In `queryeval/predicate_blueprint.cpp`, `fetchPostings` allocates a byte-per-document count vector over the entire local doc-id space before matching begins. At 10^9 docs that is a 1 GB allocation per query. The mitigating `BitVectorCache` is capped at **32 cached features globally**.

Undocumented hard limits found in source: leaf-interval count per document is `uint16_t` with `assert(size <= UINT16_MAX)` — a process abort, not a feed error; min-feature is `uint8_t`, saturating at 255; posting-list index is `uint16_t`.

**(b) No way to inject an external bitmap.** No API, query parameter, YQL operator or config to hand a content node a client-computed doc-ID mask, and no supported C++ plugin API on the content node.

**(c) No orderable doc-ID space.** Local IDs are assigned lowest-free-first with free-list reuse, and lid-space compaction "mov[es] documents from high to low LIDs" ([Proton docs](https://docs.vespa.ai/en/content/proton.html)). Bucket locality derives from the **least-significant** bits — the opposite end from the MSB-prefix containment a quadtree needs.

**(d) No access-control framework, and a default that breaks exactness.** A grep of the docs repo finds `access control` only in infrastructure-security pages and `multi-tenan` **not at all**. Separately, `count()` on a group list is "an estimate using HyperLogLog++", and "Grouping is, by default, tuned to favor performance over correctness" ([grouping](https://docs.vespa.ai/en/querying/grouping.html)). `ranking.softtimeout.enable` **defaults to true**, so under load `totalCount` is silently computed from partial coverage.

**(e) The 10^4-grant query is the wrong shape.** `predicate()` does not support YQL parameter substitution. The `in` operator (Vespa 8.293.15, 30 Jan 2024, [announcement](https://blog.vespa.ai/announcing-in-query-operator/)) benchmarks at 10M docs: 100 values → 60 ms, 1,000 values → 95 ms. Extrapolating puts 10^4 values in the 400–500 ms range before the 100× scale-up. **No published data above 1,000 values.**

**Streaming search** is the right idea at the wrong cardinality: 45 bytes/doc, ~500,000 docs/sec/node, budget 50,000 docs per user per node for a 100 ms p99 ([Bratseth](https://blog.vespa.ai/efficient-personal-search-at-large-scale/)). Its unit of visibility is one immutable group per document, and predicate fields are unsupported in streaming mode.

---

## 4. Tantivy, Quickwit, Solr

### Tantivy — the negative result is the interesting one

**Index sorting was removed, not merely limited.** The 0.24 CHANGELOG's *Breaking API Changes* section contains one bullet: `remove index sorting #2434`. [PR #2434](https://github.com/quickwit-oss/tantivy/pull/2434) closes [issue #2352](https://github.com/quickwit-oss/tantivy/issues/2352). Maintainers' reasons: it "adds considerable complexity to the indexing and merging parts of the code", users misread it as sorted *results*, and the range-query payoff was never realised. Deprecated 0.22, removed **0.24 (2025-04-09)**.

The maintainers' recommended workaround for anyone who needs it: drop to `SegmentWriter`, feed documents in your chosen order, implement your own range logic over fast fields. **That is this design.** Tantivy explicitly declined to own the thing you would adopt it for.

No Roaring anywhere. `DocSet::count()` counts the whole docset. The one range-aware primitive is `Column::get_docids_for_value_range(value_range, selected_docid_range, doc_ids)` — accepts a doc-ID window, but *materialises* IDs into a `Vec<u32>` rather than counting.

**Confirmed: no query cache.** All 30 modules in `src/query/mod.rs` inventoried; none contains "cache". The only hook is the [`Warmer` trait](https://docs.rs/tantivy/0.26.1/tantivy/trait.Warmer.html) — structurally good news, and a clean generation-aware place to hang session state.

### Quickwit — right architecture, wrong latency, maintenance problem

Splits are self-contained tantivy indices on object storage with a hotcache ([architecture](https://quickwit.io/docs/overview/architecture)). Published adversarial-benchmark numbers: hotcache fetch **~450 ms/split**, end-to-end **1.4–2.6 s**, 2,000–3,000 GETs/query. Two to three orders off the design's requirement.

`quickwit-indexing/src/actors/indexer.rs` hard-codes `sort_by_field: None`. **No per-document security whatsoever** — the [node config](https://quickwit.io/docs/configuration/node-config) documents only TLS.

Maintenance: Datadog acquired Quickwit **9 Jan 2025**, delivered **v0.9.0 on 25 July 2025**, and nothing since. `Cargo.toml` on `main` still reads `version = "0.8.0"`; 672 open issues; **no user-facing release in roughly twelve months.**

### Solr — the one materially different mechanism

`DocSet` has two representations, `BitDocSet` or `SortedIntDocSet`, crossover at `(maxDoc >> 6) + 5`. No Roaring. filterCache defaults to `CaffeineCache size=512 autowarmCount=128`; autowarming actively re-executes old filter queries against the new searcher — burning CPU regenerating masks for logged-off users. `maxBooleanClauses` defaults to **1024**.

**The genuinely material thing is the post-filter.** `if (eq.getCost() >= 100 && eq instanceof PostFilter)`. [`PostFilter`](https://solr.apache.org/docs/10_0_0/core/org/apache/solr/search/PostFilter.html) is a one-method interface — `DelegatingCollector getFilterCollector(IndexSearcher)` — and post filters are **never cached**. This is the only mechanism in any of the five systems that lets you inject an opaque per-user predicate into the collection loop without the engine trying to cache it: a custom `PostFilter` holding your session Roaring mask doing `mask.contains(docId)` in `collect()`, with `{!cache=false cost=100}`. It does *not* give O(1) range arithmetic — it is per-document and post-hoc.

**Solr has a structural count leak that is a supported feature:** JSON facet `domain.excludeTags` "discard[s] or ignore[s] particular tagged query filters", and `domain.query` computes a facet "regardless of the original domain" ([domain changes](https://solr.apache.org/guide/solr/latest/query-guide/json-faceting-domain-changes.html)). Either will count documents the security `fq` excluded.

Index sorting: no first-class `<indexSort>`; [SOLR-13681](https://issues.apache.org/jira/browse/SOLR-13681) still Open. `SortingMergePolicyFactory` sets a real Lucene index sort but only *within* each merged segment, breaking the global "row ID = rank" invariant.

---

## 5. The `range_cardinality` question, answered directly

**No system in the Lucene family, nor Vespa, has a cheap exact masked count over an arbitrary contiguous doc-ID range. In all of them it degrades to a scan.**

The reason is architectural: **internal doc IDs are not a queryable dimension.** They are segment-local, dense, insertion-ordered, and renumbered by merges. Lucene exposes exactly one hook — Solr's `sort=_docid_` — and it is sort-only. There is no `fq={!docid l=… u=…}` in Lucene, tantivy, Quickwit or Vespa.

The bitsets are equally uncooperative. Lucene: whole-set cardinality only. Solr: `intersectionSize` against a per-tile DocSet costs a full O(maxDoc/64) word scan **per tile**. Tantivy: whole-set. Vespa: aggregators are `count`, `sum`, `avg`, `min`, `max`, `xor`, `stddev`, `quantiles` — nothing sublinear.

**`range_cardinality` on a session-cached Roaring mask is the one primitive this design has that nothing off the shelf offers.**

---

## 6. What to borrow rather than re-derive

**Merge policy.** `TieredMergePolicy` defaults worth stealing as starting points: `maxMergeAtOnce` **10**, `segmentsPerTier` **10.0**, `maxMergedSegmentMB` **5 GB**, `floorSegmentMB` **2 MB**, `deletesPctAllowed` **20%**, `forceMergeDeletesPctAllowed` **10%**. Three ideas matter: (a) the **floor size**, stopping tiny segments dominating merge decisions; (b) the **max merged segment cap**, preventing unbounded rewrite and making "nightly compaction that re-ranks" tractable at 10^9; (c) **non-adjacent merging with a deletes-percentage trigger**, decoupling tombstone reclamation from re-ranking. The design conflates re-ranking and compaction into one nightly job; Lucene's separation of `findMerges` / `findForcedMerges` / `findForcedDeletesMerges` is the distinction to import.

**Reordering as a merge-policy decorator.** `BPReorderingMergePolicy` — "A merge policy that reorders merged segments... When reordering doesn't have enough RAM, it simply skips reordering in order not to fail the merge" ([javadoc](https://lucene.apache.org/core/10_0_0/misc/org/apache/lucene/misc/index/BPReorderingMergePolicy.html)). Two decisions to copy exactly: **reordering is a wrapper around a merge policy, not a special job**; and `setMinNaturalMergeNumDocs` (suggested 2^18) means small merges skip reordering while large ones get it. A strictly better shape than "nightly compaction re-ranks everything": the Morton re-rank becomes a property of sufficiently large merges, continuously.

**Version pinning.** `SearcherLifetimeManager` is a **session-token pattern isomorphic to the design's need**: "Keeps track of current plus old `IndexSearcher`s, closing the old ones once they have timed out" ([javadoc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/search/SearcherLifetimeManager.html)). Client calls `record(searcher)` for a `long` token, returns it on follow-ups; `acquire(token)` returns the same searcher or null. Affordable because old searchers "usually share almost all segments". Pair with `SnapshotDeletionPolicy`.

**From LRUQueryCache, borrow `CacheAndCount`** — storing exact cardinality alongside the cached set. **Reject** the density-based representation switch: Lucene picks `FixedBitSet` over Roaring above 1% density because iteration is faster, but a dense `FixedBitSet` has no cheap range rank, so for this design the tradeoff runs the other way.

---

## 7. Verdict

**No. A Lucene-family system cannot replace the access-control layer, and the blocker is correctness, not performance.**

Elasticsearch — the only candidate with a real per-document security feature — documents that DLS permits a user to "count how many inaccessible documents contain a given term". Solr is worse: `domain.excludeTags` is a *supported feature* for computing facet counts outside the security `fq`. Vespa and Quickwit have no per-document access control at all. Tantivy is a library and makes no claim.

Even setting correctness aside, every one of these keys its filter cache on **(segment, query)**. Per-user, hourly-recomputed predicates get zero reuse, in the most expensive possible way — a dense uncompressed `FixedBitSet` over `maxDoc`, rebuilt per segment, invalidated by every merge.

**Partially, yes — and the split is clean. Adopt the indexing and lifecycle layer; do not adopt the access-control layer.**

- **Adopt Lucene's segment lifecycle wholesale**: `TieredMergePolicy` parameters and merge-type separation; `BPReorderingMergePolicy`'s decorator shape; `SearcherLifetimeManager` + `SnapshotDeletionPolicy` for session-pinned commits. That last closes a correctness-and-therefore-security hole in the design as originally specified.
- **Adopt the index-sort-plus-skip-index pattern and cite the precedent** (Elasticsearch TSDS).
- **Do not adopt Lucene's Roaring.** Use the real RoaringBitmap library. Do borrow `CacheAndCount`.
- **Keep the mask.** No system here has any expression for a session-scoped, client-supplied doc-ID mask. Solr's `PostFilter` at `cost>=100` is the only injection point in the entire survey.

Two secondary findings: **OpenSearch 2.17's bitmap terms filtering** is the only place in the ecosystem shipping Roaring across the query boundary — worth reading for serialisation plumbing. And **Vespa's predicate field is the best existing implementation of the predicate model** — interval algebra, min-feature pruning, native NOT, no DNF required; `searchlib/src/vespa/searchlib/predicate/` and Whang et al. (PVLDB 2009) are the prior art to read before re-deriving it.

**Gaps not closed:** exact ES version shipping the DLS cache default change; which Lucene release set `DEFAULT_QUERY_CACHE` to null; whether OpenSearch bitmap terms filtering composes with filter-level DLS; any published DLS benchmark parameterised by number of distinct role queries (none exists); Vespa predicate-field memory or latency at any scale (no public figures).
