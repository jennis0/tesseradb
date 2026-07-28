# Prior Art Review 3 — Databases as a Storage and Access-Control Layer

**Scope:** Lance/LanceDB, vector databases, analytic databases with Roaring support, row-level security features, DuckDB, Roaring-native systems.
**Question:** Could any database replace the hand-rolled Roaring + mmap layer?
**Date:** 2026-07-25.

**Coverage.** Complete. First pass, from primary sources: Lance/LanceDB; six vector databases (Milvus, Qdrant, Weaviate, Pinecone, Turbopuffer, Vespa); ClickHouse, Druid and Pinot at source level; direct checks on `pg_roaringbitmap` and FeatureBase project health.

**Second pass** (2026-07-26) closed the four gaps left by the spend-limit termination: row-level security (§4), DuckDB (§5), Doris and StarRocks (§6), and Apache Accumulo (§7, added on request). Sections 4, 5 and 7 include **original measurement** — a PostgreSQL 16.13 instance, DuckDB 1.5.5, and compiled `accumulo-access` sources respectively — rather than inference. Remaining unverified items are listed per section and consolidated at the end.

---

## The primitive everything hinges on

The design asks a database for two things that are unusual together:

1. **A per-user selection that persists across queries** — built once per session, reused thousands of times while panning.
2. **`range_cardinality(mask, lo, hi)`** — exact count of set bits inside an arbitrary contiguous row-ID range — plus `rank`/`select` over that range.

**Of every system examined, exactly one exposes (2) as a first-class primitive, and none at all offers (1).** (1) is the rarer capability and the more decisive one.

---

## 1. Lance / LanceDB

Genuinely well-engineered random-access columnar format. Adaptive encoding on a 128-byte-per-value threshold, "full-zip" guaranteeing ≤1 IOP for fixed-width random access, and a bounded metadata search cache — 24–41 bytes per mini-block chunk, capped ~1.28 GiB per billion rows, against Parquet-rs's 20 bytes/page which can reach ~20 GiB per billion rows for large types ([arxiv.org/abs/2504.15247](https://arxiv.org/abs/2504.15247), §4 and §6).

**The headline number does not survive contact.** "100x faster random access than Parquet" is measured against *default-configured* Parquet on an image dataset. The paper — LanceDB-authored but unusually honest — gives the real picture on 1B rows: default Parquet ~5,500 rows/sec, **tuned Parquet (8KiB pages) ~350,000 rows/sec**, Lance 2.1 "ties or beats" that, while conceding Lance **slightly underperforms tuned Parquet on scalar data** due to mini-block decode overhead. This design's rows are two float32s and three integers — precisely the regime where the vendor's own paper says Lance loses. **No independent non-LanceDB benchmark exists**, and no absolute per-call latency figure is published anywhere.

**Bitmap index: real Roaring.** `lance-core` depends on `roaring ^0.11.4`; core type is `RowAddrTreeMap = BTreeMap<u32, RowAddrSelection>` with a `Full` variant so a wholly-selected fragment costs O(1) ([mask.rs](https://deepwiki.com/lancedb/lance/5.4-filtering-and-row-masking); [bitmap index spec](https://lance.org/format/index/scalar/bitmap/)). But documented cardinality guidance is **"fewer than 1,000 unique values"** ([scalar index guide](https://lancedb.com/documentation/guides/indexing/scalar-index.html)) — the design's worst dimension is 10⁵–10⁶ — and there is **no public API to retrieve a raw bitmap**.

**`array_has_any` is exactly the wrong shape.** Backed by a LABEL_LIST index, `search_values` issues **one `SargableQuery::Equals` bitmap lookup per query value**, then unions ([label_list.rs](https://docs.rs/lance-index/latest/src/lance_index/scalar/label_list.rs.html)). A 10⁴-grant predicate becomes 10⁴ independent lazy bitmap page loads against a **`WeakLanceCache`** — weakly held and evictable — with per-user-unique grants guaranteeing near-zero hit rate.

**The decisive gap is (1).** The [`Scanner` API](https://docs.rs/lance/latest/lance/dataset/scanner/struct.Scanner.html) offers `filter()`, `filter_expr()`, `with_row_id()`, `limit()` — and **no method to pass a precomputed mask, no mechanism to cache a selection across queries**. `DatasetPreFilter`'s `final_mask` is per-query.

**Stable row IDs are in trouble at exactly the target scale.** [Discussion #3694](https://github.com/lance-format/lance/discussions/3694): for **1B rows across 3,500 fragments the row-id index was estimated at 20–30 GB**, exceeding the 1 GiB metadata cache and taking `take_rows()` "from instant to tens of seconds or even minutes." Subsystem reopened for redesign in [Discussion #6933 (25 May 2026)](https://github.com/lance-format/lance/discussions/6933).

**Project health:** $30M Series A June 2025; repo moved to a `lance-format` org with a spec site at [lance.org](https://lance.org/format/table/) — foundation donation **not confirmed**. Release cadence alarming for a storage format: `lance-core` shipped **9.0.0 (2026-07-24), 8.0.0 (2026-07-01), 7.0.0 (2026-05-28)** — a major version every 3–5 weeks with documented breaking changes. No stability or LTS commitment.

**Verdict: ignore.** Borrow the fragment-keyed-Roaring layout (`BTreeMap<u32, RoaringBitmap>`).

---

## 2. Vector databases

**Calibration point framing the whole section:** the **NeurIPS'23 Big ANN filtered-search track** — the field's flagship filtered-retrieval benchmark — used a 200,386-tag vocabulary but each query carried **"one image embedding and one or two tags"** ([arxiv.org/html/2409.17424v1](https://arxiv.org/html/2409.17424v1)). The design needs 10⁴ terms per query. The entire research literature optimises a regime four orders of magnitude away.

**Pinecone** supplies the cleanest disqualifying citation: **"Each `$in` or `$nin` operator accepts a maximum of 10,000 values"** ([docs](https://docs.pinecone.io/guides/search/filter-by-metadata)). At the ceiling on day one. Ironically its internals are this design — their ICML 2025 paper describes converting a filter into "an explicit representation of the vector ids… represented in a tightly compressed form – a bitmap," giving "direct iterator access to all the vector ids that match the filter" ([paper](https://www.pinecone.io/research/ICML_2025.pdf)) — but per-slab, per-query, no documented reuse, and no filtered-count API.

**Qdrant** has the best exact-count API of the six (`/points/count` with `exact` defaulting to **true** — [api.qdrant.tech](https://api.qdrant.tech/api-reference/points/count-points)), and the cleanest blocker: **[issue #3522, "Optimize MatchAny for large amount of values"](https://github.com/qdrant/qdrant/issues/3522)**, motivating use case nearly identical ("up to a several thousand subscriptions"), **open with no merged optimization**. Its `is_tenant` payload index physically colocates a tenant's vectors — meaningless when a row has twenty tenants.

**Milvus** documents that **"bitmap indexes are most effective when the cardinality of a field is less than 500"** ([milvus.io/docs/bitmap.md](https://milvus.io/docs/bitmap.md)). Zilliz publishes a row-level RBAC pattern that is *literally this architecture* — per-row array of role strings filtered with `array_contains_any()` — with the schema capped at **`max_capacity=10`** roles per row ([blog](https://zilliz.com/blog/enabling-fine-grained-access-control-with-milvus-row-level-rbac)). The design at 1/1000th scale, shipped as a blog post. Partition keys are hash-and-modulo of a scalar, so structurally 1:1, and Milvus's docs concede partition-key multi-tenancy gives "relatively weak data isolation" — not a security boundary.

**Weaviate** is the best architectural confirmation: true pre-filtering via an allow-list of `uint64` IDs backed by Roaring bitmaps, claimed to "scale efficiently without performance penalties", flat-search cutoff around 15% selectivity ([docs](https://docs.weaviate.io/weaviate/concepts/filtering)). The 1.18 Roaring release claimed queries going "from 3-4 seconds to 3-4 milliseconds" (vendor-measured). **Blocker:** the cursor API walks in UUID order and is **explicitly incompatible with `where` filters** ([additional-operators](https://docs.weaviate.io/weaviate/api/graphql/additional-operators)) — exactly the combination needed.

**Vespa** most closely anticipates the problem. Its [feature-tuning guide](https://docs.vespa.ai/en/performance/feature-tuning.html) has a *Multi-Lookup Set Filtering* section recommending `in` over plain OR, stating that with 10M unique values and 1000 lookups per query a hash dictionary gives O(1) lookups vs log(10M) — the only vendor doc sizing guidance at ~10³ lookups against a 10⁷ dictionary. `rank: filter` switches posting lists to bit-vector representation, claimed to "reduce match latency by 75%". Its constrained-ANN machinery builds exactly the artifact wanted: "the result of this execution is a list of document IDs matching the filter" ([blog](https://blog.vespa.ai/constrained-approximate-nearest-neighbor-search/)). **But it is rebuilt every query** — nothing describes caching or sharing it. Also `maxHits` defaults to 400, `maxOffset` to 1000.

**Turbopuffer** is disqualified by physics: **"~10ms latency floor"** for consistent reads, warm p50 of **14ms at 1M documents**, "P999 queries… in the 100s of milliseconds" ([tradeoffs](https://turbopuffer.com/docs/tradeoffs), [architecture](https://turbopuffer.com/docs/architecture)). Its floor is the design's ceiling, three orders of magnitude below target scale.

**The cross-cutting negative result:** not one of the six caches a per-user filter bitset across queries. Vespa rebuilds per query; Pinecone precomputes per slab per query; Weaviate builds an allow-list per query; Qdrant re-runs cardinality estimation; Milvus's only bitset cache is for deletes ([#28031](https://github.com/milvus-io/milvus/issues/28031)); Turbopuffer caches data, not selections. And every multi-tenancy model is 1:1 — Turbopuffer's docs actively instruct you to "create one namespace per set of documents… rather than using filters."

One telling artifact: [tata1mg/fastmatch-elasticsearch-plugin](https://github.com/tata1mg/fastmatch-elasticsearch-plugin) exists to "match a set of integers with the large lists of integers stored in ElasticSearch (as serialized RoaringBitmap)." Someone had this exact problem and had to write a plugin because nothing did it natively.

**Security note:** ANN approximation produces false *negatives*, not false positives — recall loss under filtering is a UX bug here, not a leak. The real risks are approximate counts (Qdrant's `exact: false`, Vespa's `hitcountestimate`), string-DSL injection where grant sets are interpolated into Milvus `expr` or Vespa YQL, and **timing side channels** — all six switch execution strategy based on filter cardinality, making latency an observable function of how many rows a user can see. None discuss this.

---

## 3. Analytic databases with Roaring — and the answer to the primitive question

**ClickHouse ships CRoaring and calls almost none of the primitives needed.** The type is `AggregateFunction(groupBitmap, UInt*)` backed by `roaring::Roaring`. The function inventory looks encouraging — `bitmapAnd`, `bitmapSubsetInRange`, `subBitmap`, `bitmapAndCardinality`, `bitmapHasAny` ([reference](https://clickhouse.com/docs/sql-reference/functions/bitmap-functions)). The implementation is not. From [`AggregateFunctionGroupBitmapData.h`](https://raw.githubusercontent.com/ClickHouse/ClickHouse/master/src/AggregateFunctions/AggregateFunctionGroupBitmapData.h):

```cpp
for (auto it = roaring_bitmap->begin(); it != roaring_bitmap->end(); ++it)
{
    if (*it < range_start)
        continue;                    // walks every set bit below range_start
    if (*it < range_end) { r1.add(static_cast<T>(*it)); ++count; }
    else break;
}
```

`rb_range` is a **linear scan from zero**. `rb_offset_limit` advances one element at a time, so select-by-rank is O(offset). Meanwhile [`roaring.h`](https://raw.githubusercontent.com/RoaringBitmap/CRoaring/master/include/roaring/roaring.h) — in ClickHouse's own tree — provides `roaring_bitmap_range_cardinality`, `roaring_bitmap_rank`, `roaring_bitmap_rank_many`, `roaring_bitmap_select`, `roaring_bitmap_range_uint32_array`, `roaring_bitmap_intersect_with_range`, and the `frozen_view` zero-copy family. A grep for any of these in ClickHouse's bitmap layer returns **zero hits**. Even `rb_and_cardinality` materialises the full intersection rather than calling the allocation-free `roaring_bitmap_and_cardinality`.

Worse for (1): serialisation allocates and fully deserialises on **every** read, with the source carrying its own TODO admitting the unnecessary copy. Session temporary tables are genuinely session-scoped, but you have persisted the *bytes*, not the live object. There is also a hard `UInt32` ceiling — `bitmapSubsetInRange` on a UInt64 bitmap silently truncates.

Two positives worth stealing. ClickHouse's small-set threshold is **32** (`RoaringBitmapWithSmallSet<T, 32>`) — a battle-tested constant for the long tail of tiny per-term bitmaps. And **ClickHouse 26.2's `text` index** is the closest structural match anywhere: posting lists "stored as roaring bitmaps," row-level resolution, index-only direct read via `query_plan_direct_read_from_text_index`, and with the `array` tokenizer supports `hasAny` over `Array(String)` ([textindexes.md](https://raw.githubusercontent.com/ClickHouse/ClickHouse/master/docs/en/engines/table-engines/mergetree-family/textindexes.md)).

ClickHouse's own knowledge base says point queries are "among top positions in the list of cases when NOT to use ClickHouse" ([KB](https://clickhouse.com/docs/knowledgebase/key-value)).

**Druid** gets one thing exactly right that ClickHouse gets wrong: [`RoaringBitmapSerdeFactory`](https://raw.githubusercontent.com/apache/druid/master/processing/src/main/java/org/apache/druid/segment/data/RoaringBitmapSerdeFactory.java) uses `ImmutableRoaringBitmap` — **zero-copy reads straight off the memory-mapped segment buffer, never deserialised to heap**. That is the discipline to copy. But there is no user-supplied-bitmap concept; a 10⁴-way `in` filter is re-unioned per segment per query via `IndexedUtf8ValueIndexes`, which switches between sorted-merge and per-value binary search at a **0.12 ratio threshold** (directly applicable to the mask build: 10⁴ grants against 10⁶ terms is 1%, so per-term lookup wins clearly; against 10⁵ terms you're at 10%, right at the boundary — measure).

Planner hazard: `inFunctionThreshold` defaults to **100**, so a 10⁴-value SQL `IN` is silently rewritten to `SCALAR_IN_ARRAY`, which the docs say is "eligible for fewer planning-time optimizations". **Whether `SCALAR_IN_ARRAY` retains bitmap acceleration is unverified** — the one open question that could move the Druid assessment. A planner that restructures your authorization predicate behind your back is a liability regardless.

**Pinot's IdSet** looked like the answer and isn't. `ID_SET`/`IN_ID_SET`/`IN_SUBQUERY`, INT→`RoaringBitmapIdSet`, LONG→`Roaring64NavigableMapIdSet`, with an 8 MiB threshold above which it **silently degrades to a Bloom filter with 3% FPP** ([`IdSets.java`](https://raw.githubusercontent.com/apache/pinot/master/pinot-core/src/main/java/org/apache/pinot/core/query/utils/idset/IdSets.java)). A silent false-positive fallback in an authorization path is disqualifying alone. But the real problem is architectural: `InIdSetTransformFunction extends BaseTransformFunction` and is evaluated **per docId** — a scan predicate, not an index.

**So for the analytic tier: no.** ClickHouse has the right function names over a linear implementation; Druid and Pinot have no range-restricted mask-count primitive at all. None can hold a live mask across queries.

**The exception — and it is real.** `pg_roaringbitmap` ([repo](https://github.com/ChenHuajun/pg_roaringbitmap)) exposes exactly the needed primitive set:

- `rb_range_cardinality(roaringbitmap, range_start bigint, range_end bigint) → bigint`
- `rb_rank(roaringbitmap, integer) → bigint`
- `rb_index(roaringbitmap, integer) → bigint`
- `rb_select(roaringbitmap, bitset_limit, bitset_offset=0, reverse=false, range_start=0, range_end=4294967296) → roaringbitmap` — **select-k restricted to a value range, in one call**
- 64-bit variants throughout

Apache-2.0, v1.1.0 released **November 2025**, 278 stars, 6 open issues, not archived, packaged by **Alibaba Cloud, Huawei Cloud, Tencent Cloud and Google Cloud SQL**.

The catch is (1): a roaring bitmap in Postgres is a **column value**, deserialised on every query. No session-resident live object. **Not benchmarked; no published numbers found.** Treat it as the best available *reference implementation of the primitives* and a plausible **mask-build and mask-storage tier**, not as the serving path.

---

## 4. Row-level security

**Verdict up front: no RLS implementation surveyed can serve as the access-control layer.** PostgreSQL fails on both axes — it leaks exact counts and density over invisible rows through supported, unprivileged interfaces, and it is ~3,300× slower than the unfiltered baseline at 10⁷ rows. The cloud warehouses do better on leakage but none offers a persistable per-user selection or anything resembling `range_cardinality`. Measurements are on PostgreSQL 16.13 and are reproducible.

### 4.1 How PostgreSQL enforces RLS, and what `LEAKPROOF` means

Policy expressions are injected as `securityQuals` on the range-table entry and evaluated ahead of user quals — *"The only exceptions to this rule are `leakproof` functions, which are guaranteed to not leak information; the optimizer may choose to apply such functions ahead of the row-security check"* ([ddl-rowsecurity](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)). The same barrier drives subquery pushdown: `set_subquery_pathlist` sets `safetyInfo.unsafeLeaky = rte->security_barrier`, and `qual_is_pushdown_safe` refuses to push quals containing leaky functions ([allpaths.c](https://github.com/postgres/postgres/blob/master/src/backend/optimizer/path/allpaths.c)).

`LEAKPROOF` is an **unverified superuser attestation, not a checked property** ([CREATE FUNCTION](https://www.postgresql.org/docs/current/sql-createfunction.html)). Confirmed empirically with a `plpgsql` function that raises its argument, given `COST 0.0001` so cost-based ordering wants it first:

| function marking | calls over a 100,000-row table with 1,000 visible rows | argument values observed |
|---|---|---|
| default (not leakproof) | 1,000 | visible rows only |
| `LEAKPROOF` | 100,000 | full disclosure |

The barrier works as documented; its integrity rests entirely on the correctness of the leakproof markings in `pg_proc`. Note that `arrayoverlap` (the `&&` operator — the natural way to express category intersection), `arraycontains`, `textlike` and `<@(point,box)` are all **not** leakproof.

The documentation is candid that this is not a confidentiality boundary in the strong sense. [rules-privileges](https://www.postgresql.org/docs/current/rules-privileges.html) warns that a user can see the plan, can measure run time, and *"might be able to infer something about the amount of unseen data, or even gain some information about the data distribution or most common values (since these things... are also reflected in the optimizer statistics, the choice of plan)"*, concluding: *"If these types of 'covert channel' attacks are of concern, it is probably unwise to grant any access to the data at all."* That sentence is the project's own answer to this section's question.

### 4.2 The statistics leak: half-closed, and the open half is the half that matters

Two routes have been closed. `pg_stats` was patched in 2015 and now carries `AND (c.relrowsecurity = false OR NOT row_security_active(c.oid))` ([system_views.sql](https://github.com/postgres/postgres/blob/master/src/backend/catalog/system_views.sql)) — confirmed, an analyst role sees zero rows there. And **[CVE-2019-10130](https://www.postgresql.org/support/security/CVE-2019-10130/), "Selectivity estimators bypass row security policies"**, fixed the case where *"a user able to execute SQL queries with permissions to read a given column could craft a leaky operator that could read whatever data had been sampled from that column"* — by *"only allowing a non-leakproof operator to use this data if there are no relevant row security policies for the table."*

**But that fix gates only non-leakproof operators, and the entire viewport query is leakproof.** `float8lt`/`float8le` are leakproof, so the planner still consults the full-table histogram and prints the result. Constructed case: 5,000 visible rows spread uniformly over `x ∈ [0,100]`; 90,000 rows the analyst **cannot see** clustered in `x ∈ [70,71)`.

```
viewport x=[10,11)  EXPLAIN rows=3       actual visible rows=38
viewport x=[50,51)  EXPLAIN rows=3       actual visible rows=47
viewport x=[69,70)  EXPLAIN rows=3       actual visible rows=48
viewport x=[70,71)  EXPLAIN rows=4748    actual visible rows=50   <-- invisible cluster
viewport x=[71,72)  EXPLAIN rows=6       actual visible rows=60
viewport x=[90,91)  EXPLAIN rows=3       actual visible rows=46
```

The visible density is flat. The estimate spikes 1,500× at exactly the invisible cluster. **This is cluster existence and density over unauthorised rows, disclosed by a correctly configured, fully patched server** — the precise thing this design exists to prevent. The same mechanism works as an equality oracle: for a value present only in invisible rows `EXPLAIN` returns `rows=552`, for a value present nowhere `rows=1`, and the magnitude estimates the invisible-row frequency.

Two further exact disclosures, neither privileged: **`EXPLAIN ANALYZE` reports `Rows Removed by Filter: 99000`** — the exact count of rows excluded by the policy; and **`pg_class.reltuples` is unrestricted**, so the analyst reads the true total row count sitting next to a masked `pg_stats`. The `Filter:` line also prints the policy expression verbatim.

Mitigating context, stated plainly: these require the adversary to submit SQL. If the point service is the only SQL client and never proxies user SQL or `EXPLAIN`, they are not directly reachable — but in that architecture RLS buys nothing an application-level `WHERE` clause does not, and §4.3 kills it regardless.

### 4.3 Performance of a 10⁴-element membership policy — measured

10⁷ rows, `cats int[]` with 10 categories each drawn from 10⁵, a 10⁴-element allow-list per user, GiST index on `point(x,y)`, GIN on `cats`, `shared_buffers=512MB`, warm.

| configuration | viewport query (1% of area, 984 rows) |
|---|---|
| **no RLS** (superuser) | Bitmap Index Scan on GiST, **26 ms** |
| RLS, policy `cats && my_cats()` (STABLE SQL fn) | Seq Scan, **>120 s, did not complete** |
| RLS, policy `cats && (SELECT array_agg(cat) ...)` | Seq Scan, **>180 s, did not complete** |
| RLS, policy `EXISTS (SELECT 1 FROM user_cats ...)` | Seq Scan + SubPlan × 10⁷, **86,803 ms** |

The cause is structural, not a tuning failure: `<@(point,box)` is not leakproof, so the planner may not use it as an index condition ahead of the security qual and **the spatial index is abandoned entirely**. The best variant is a 3,340× regression executing an index-only subplan ten million times. GIN on `cats` does not help — a 10⁴-key `&&` scan touches ~65% of the table. This is at 10⁷; the target is 10⁹.

### 4.4 The plan-cache CVE family — three fixes for one bug over eight years

- **[CVE-2016-2193](https://www.postgresql.org/support/security/CVE-2016-2193/)** (9.5.2): *"In a session that performs queries as more than one role, the plan cache might incorrectly re-use a plan that was generated for another role ID, thus possibly applying the wrong set of policies."*
- **[CVE-2023-2455](https://www.postgresql.org/support/security/CVE-2023-2455/)** (15.3): the 2016 fix *"missed a scenario involving function inlining."*
- **[CVE-2024-10976](https://www.postgresql.org/support/security/CVE-2024-10976/)** (17.1): both prior fixes *"missed cases where a subquery, WITH query, security invoker view, or SQL-language function references a table with a row-level security policy."*

All three are wrong-policy-applied bugs — not leakage but actual unauthorised reads. The pattern is the point: **caching anything derived from an RLS policy across a role change has been gotten wrong three times, most recently in November 2024.** This design's central premise is a per-user selection cached and reused thousands of times, so the lineage is worth internalising — though note the design caches a *materialised set* keyed on a content hash of the auth data, not a *plan* keyed on a role, which is the specific thing that kept breaking.

### 4.5 Cloud warehouses

**Snowflake row access policies.** One policy per object ([ALTER TABLE](https://docs.snowflake.com/en/sql-reference/sql/alter-table)). Guidance on policy bodies is soft rather than a documented limit — *"Including one or more subqueries in the policy body may cause errors"* ([CREATE ROW ACCESS POLICY](https://docs.snowflake.com/en/sql-reference/sql/create-row-access-policy)). On aggregates Snowflake is the **only vendor to state the correct semantics explicitly**: statistics-based optimisations *"are not applicable with a row access policy"*, and *"the returned statistics are only based on what is permissible to access, not the 'true' statistical values"* ([security-row-intro](https://docs.snowflake.com/en/user-guide/security-row-intro)). Correct — and it concedes that metadata-fast `COUNT(*)` is gone, which is the operation this design needs most. On cross-query caching, memoizable UDFs are the only mechanism and cannot express this: **10 KB limit per session** (a 10⁴-element ID set is ~40–60 KB), arguments must be constants rather than columns, and OBJECT/VARIANT returns are prohibited ([scalar SQL UDFs](https://docs.snowflake.com/en/developer-guide/udf/sql/udf-sql-scalar-functions)).

**BigQuery row-level security.** The vendor **documents a side channel**: *"Side channels, such as the query duration, can leak information about rows that are at the edge of a storage shard"* ([row-level-security-intro](https://docs.cloud.google.com/bigquery/docs/row-level-security-intro)). Subquery policies impose a 100 MB limit on top-level subquery results; `IN` subqueries over `FLOAT`, `STRUCT`, `ARRAY`, `JSON`, `GEOGRAPHY` are unsupported; RLS *"does not participate in query pruning for partitioned tables"*; BI Engine, table sampling, preview, wildcard tables and (for subquery policies) the Storage Read API are all incompatible. No cross-query policy-result caching documented.

**Databricks Unity Catalog row filters.** Effectively one filter per table. The vendor claim is the exact trust question this review asks: *"the query engine must choose between optimization and protecting against information leakage from filtered or masked values, it always makes the secure choice, which can affect query performance"* — an assertion with no published verification. No row filters on views, no time travel, no clones, no path-based access, filters cannot reference filtered tables ([row-and-column-filters](https://docs.databricks.com/aws/en/tables/row-and-column-filters)).

### 4.6 Verdict

**Rejected on both axes.** On leakage: PostgreSQL's own documentation disclaims RLS as a defence against inference from plans, statistics and timing, and the measurements show that disclaimer is load-bearing — an exact excluded-row count from `EXPLAIN ANALYZE`, an invisible-cluster density signal 1,500× above background from plain `EXPLAIN`, and an unrestricted `reltuples`. The project has closed this class of hole twice and it remains open through leakproof operators. The general point generalises: a hard no-unauthorised-aggregates boundary requires the optimizer, filter pushdown, subquery rewriting, error messages, cost estimates and `EXPLAIN` output to *all* be non-disclosing. The only vendor claiming this offers no evidence; the only vendor documenting the semantics correctly simultaneously documents that it costs them fast `COUNT(*)`.

On capability: none provides a per-user selection that persists across queries, and none exposes anything near `range_cardinality`, `rank` or `select` over a persisted selection. RLS is a filter primitive, not an indexed-set primitive — a category mismatch rather than a missing feature.

Practical consequence: RLS cannot be the access-control layer, and it cannot even be cheap belt-and-braces behind an application-level mask, because enabling it costs 3,340× on the viewport query while adding an `EXPLAIN`-shaped hole the mask does not have.

---

## 5. DuckDB

**Coverage:** complete, with original measurement. DuckDB 1.5.5 ([22 July 2026](https://duckdb.org/2026/07/22/announcing-duckdb-155)), Python client, 2 vCPU / 7 GB, `threads=2`, on-disk. Synthetic corpus at 10⁵/10⁶/10⁷ rows; each row an `id BIGINT` plus a 10-element `INTEGER[]` drawn from a 10⁶-value domain; probe set 9,948 distinct IDs giving **9.5–9.6% selectivity**, close to the real regime. Absolute numbers are a floor on this hardware; the **ratios** are the finding. Nothing above 10⁸ rows was measured.

### 5.1 Roaring in DuckDB — confirmed, and worse than previously stated

The prior finding holds at code level. [`roaring.hpp`](https://raw.githubusercontent.com/duckdb/duckdb/main/src/include/duckdb/storage/compression/roaring/roaring.hpp) includes `validity_mask.hpp` and defines `UNCOMPRESSED_SIZE = ROARING_CONTAINER_SIZE / sizeof(validity_t)` with `NULLS`/`NON_NULLS` constants. It is a **compression codec for validity masks**, selected via `PRAGMA force_compression='roaring'`, introduced in v1.2.0 (PR #14878) — which is why it is absent from the 1.2.0 announcement.

Two additions. **`ROARING_CONTAINER_SIZE = 2048`**, not Roaring's standard 65,536 — DuckDB's container size is its vector size, so the on-disk representation is **not format-compatible with CRoaring in either direction**. Any hope of reading a DuckDB roaring segment as a `frozen_view` dies at the constant level. And `SELECT function_name FROM duckdb_functions() WHERE function_name ILIKE '%roar%'` returns **nothing** — there is no queryable roaring type.

### 5.2 `list_has_any` is a trap — measured, with the source-level reason

[`list_has_any_or_all.cpp`](https://raw.githubusercontent.com/duckdb/duckdb/main/extension/core_functions/scalar/list/list_has_any_or_all.cpp) is decisive. Inside a per-row `BinaryExecutor::Execute` lambda it does `set.clear()`, rebuilds a `string_set_t` from the smaller list, then probes it with every element of the larger — both lists first converted to **BLOB sort keys** via `CreateSortKeyHelpers::CreateSortKey`. With a 10-element row list and a 10⁴-element grant list, every row rebuilds a 10-element string hash set and performs up to 10⁴ BLOB hash lookups. The loop-invariant probe set is never hoisted.

| n (rows) | probe | `list_has_any` |
|---|---|---|
| 10⁵ | 10 | 583 ms |
| 10⁵ | 1,000 | 4,917 ms |
| 10⁵ | **10,000** | **39,637 ms** (best of 2, warm) |
| 10⁶ | 100 | 5,097 ms |
| 10⁷ | 100 | 58,127 ms |

Linear in rows, roughly linear in probe size. Two independent extrapolations to 10⁷ rows × 10⁴ probe agree at **≈1.1–2.1 hours on 2 cores**; at 10⁹ rows, days. **Disqualified outright** — and it is exactly the query a reasonable engineer writes first.

### 5.3 The correct formulation is fast — the actual finding

Pre-explode to `(id, category_id)` pairs and use a hash semi-join:

| query | n | time |
|---|---|---|
| `unnest` + `SEMI JOIN` (count) | 10⁶ rows | **221 ms** |
| `unnest` + `IN (subquery)` | 10⁶ rows | **167 ms** |
| `unnest` + `SEMI JOIN` (count) | 10⁷ rows | **2,020 ms** |
| same, 957,733 sorted IDs → Python list | 10⁷ rows | **1,944 ms** |
| build `docs_long` (10⁸ pairs) | — | 8,833 ms, **277 MB** (2.77 B/pair) |
| `SEMI JOIN` on `docs_long` (count) | 10⁸ pairs | **1,673 ms** |
| export 957,733 IDs → Arrow | — | **25 ms** warm |

**~3,000× faster than `list_has_any` at 10⁶ rows.** The plan is a plain `HASH_JOIN (Join Type: SEMI)` over two `SEQ_SCAN`s. Extrapolating to 10⁹ rows / 10¹⁰ pairs: ~28 GB of DuckDB storage and ~170–200 s on 2 cores, so roughly **10–25 s on 16–32 cores**. **Not measured above 10⁸ pairs — that is arithmetic, not a result.** Even at 10× pessimism it fits a per-authorisation budget.

### 5.4 Indexing: nothing helps

**ART on a LIST column is impossible** — `CREATE INDEX ix ON t(cats)` returns `InvalidTypeException: Invalid Type [BIGINT[]]: Invalid type for index key`. **ART on the exploded scalar column is legal but pointless**: building one over 10⁸ rows took 82.8 s, and the [indexing guide](https://duckdb.org/docs/current/guides/performance/indexing.html) states index scans are only chosen below `MAX(2048, 0.001 × cardinality)` — the mask build returns ~9.6% of rows, about 96× above that threshold. ART "must currently be able to fit in memory during index creation." **Zonemaps** prune on ordered scalar columns and prune nothing against a probe set spanning the domain.

### 5.5 `rowid`: do not use it as an identity

[SELECT docs](https://duckdb.org/docs/current/sql/statements/select.html): rowid is "based on the physical storage", is "stable within a transaction", and "Deletions introduce gaps in the rowids which may be reclaimed later." The docs say plainly: **"It is strongly advised to avoid using rowids as identifiers."** It is also unavailable on views. The design carries its own Morton-order ID column; keep it that way.

### 5.6 Latency floor: never on the viewport path

In-process, warm, 2 cores: **`SELECT 1` = 0.217 ms**; point lookup on 10⁸ rows = 1.177 ms; 2,000-row range scan on 10⁷ rows = 1.243 ms. That 0.2 ms is parse-plan-execute with zero data touched, against a sub-millisecond viewport budget. The obstacle is not a round trip — there isn't one — it is query-engine overhead.

### 5.7 Arrow interop: adequate, not zero-copy, and the ergonomic API is deprecated

In [`duckdb.h`](https://raw.githubusercontent.com/duckdb/duckdb/main/src/include/duckdb.h), 14 Arrow C API functions sit behind `#ifndef DUCKDB_API_NO_DEPRECATED` carrying "scheduled for removal in a future release." The surviving path is `duckdb_to_arrow_schema` / `duckdb_data_chunk_to_arrow`. Note the name — a **conversion** from DuckDB's internal vector format. The [2021 zero-copy blog post](https://duckdb.org/2021/12/03/duck-arrow) is a vendor claim about the now-deprecated API, and the current export guide makes no zero-copy claim. Measured ~300 MB/s, consistent with a copy. At mask-build cadence it does not matter: 25 ms against a ~2 s build.

### 5.8 Bitmaps and the primitive set

Complete bit inventory: `bit_and, bit_count, bit_length, bit_or, bit_position, bit_xor, bitstring, bitstring_agg, get_bit, list_bit_and, list_bit_or, list_bit_xor, set_bit`. `bitstring_agg` turned 957,733 IDs into a 10⁷-bit bitstring in 13.4 ms and `bit_count` ran in 5.5–17.2 ms — encouraging, and then it stops. **No `range_cardinality`**: there is no `substring` overload for `BIT`, so `bit_count` counts the whole bitstring or nothing. **No `rank`**, no prefix-count primitive. **No `select`** — `bit_position` finds a pattern, not the k-th set bit. And `BITSTRING` is uncompressed: 10⁹ bits = **125 MB per mask**, the same arithmetic that kills Solr's `filterCache`.

**Bloom filters** are [Parquet-only](https://duckdb.org/2025/03/07/parquet-bloom-filters-in-duckdb), for row-group skipping. Important distinction from Pinot: this is a **pruning** structure, so a false positive costs a wasted row-group read and never an extra row in the result — **not a disclosure hazard**. The community extension [`bitfilters`](https://github.com/query-farm/bitfilters) (quotient/XOR/binary-fuse, configurable false-positive rates) *is* one. Keep it out of the authorisation path.

### 5.9 The `duckdb-roaringbitmap` extension is not software

**[Smallhi/duckdb-roaringbitmap](https://github.com/Smallhi/duckdb-roaringbitmap) contains no roaring bitmap code.** One star, two commits. Its only source file is the unmodified DuckDB extension template implementing `quack(name) → "Quack {name} 🐥"`. `grep -ci roaring` returns **0**. An empty scaffold with a misleading repository name. There is no roaring extension in the [community extensions list](https://duckdb.org/community_extensions/list_of_extensions).

### 5.10 Session-scoped selection — the question changes shape

DuckDB is **in-process**, which reframes the decisive gap. You do not need DuckDB to hold a live selection: the Rust process holds the CRoaring bitmap and calls DuckDB only to build it. No socket, no cross-process serialisation, no `pg_roaringbitmap`-style re-parse per query. That is DuckDB's one structural advantage for this role. It does not change the conclusion — DuckDB offers no live bitmap object and none of the three primitives. It is a batch set-difference engine, not a selection holder.

### 5.11 Verdict

**Viable as the mask-build tier, better suited to it than `pg_roaringbitmap`, but not obviously worth the dependency.**

*Do:* store a pre-exploded `(entity_id, category_id)` pair table and run a hash semi-join against the grant set. *Never:* `list_has_any` over a LIST column; the viewport path; `rowid` as identity; `bitfilters` anywhere near authorisation.

What it buys, honestly: a mature, parallel hash join and Arrow export, in-process, for a ~30 MB dependency and a second copy of the corpus (~28 GB at 10¹⁰ pairs). The alternative is a sorted-merge or hash semi-join written directly in Rust over the Morton-ordered Arrow files already mmap'd, feeding `roaring_bitmap_add_many` — a few hundred lines, no second copy, and the Druid 0.12 heuristic already selects the algorithm. **DuckDB is right if you want the mask build working this week; the hand-rolled kernel is right given the corpus is already mmap'd Arrow.** Its most durable role is as the **reference implementation to validate the Rust kernel against**, which matters more than it sounds: a wrong mask is a disclosure bug.

---

## 6. Roaring-native and bitmap-capable systems

**FeatureBase is dead.** Pilosa → Molecula (2020) → FeatureBase (2022) → **repository archived by its owner on 21 February 2024, read-only** ([repo](https://github.com/FeatureBaseDB/featurebase)). Apache-2.0, 2.5k stars, 5,576 commits. Its shard-by-2²⁰-columns design does map contiguous column-ID ranges onto whole shards, which is architecturally interesting — but **do not build a security-critical system on archived software with no maintainer.**

**Elasticsearch/Solr:** treated in Prior Art Review 1. One arithmetic point belongs here: a Solr `filterCache` entry at 10⁹ docs is an uncompressed bitset of **125 MB per cached filter** — with per-user-unique masks that cache is unusable by construction.

### 6.1 Doris and StarRocks — verified at source level

Both repositories were cloned and read rather than fetched as rendered docs: **Apache Doris `master` @ `cb56b23`** (2026-07-26) and **StarRocks `main` @ `1299f39`** (2026-07-25). The earlier framing of Doris as "the most likely place a second `rb_range_cardinality` equivalent is hiding" was wrong. It is hiding nowhere.

**Representation.** Neither uses CRoaring's native 64-bit bitmap for its SQL `BITMAP` type. Both carry a hand-vendored `detail::Roaring64Map` — a `std::map<uint32_t, roaring::Roaring>` keyed on the high 32 bits ([Doris `bitmap_value.h:112`](https://github.com/apache/doris/blob/cb56b23822af86b7c9919a9fcff319a32e7fb431/be/src/core/value/bitmap_value.h#L112), [StarRocks `bitmap_value_detail.h:107`](https://github.com/StarRocks/starrocks/blob/1299f3903660981c844aba48deb55c378e67106a/be/src/types/bitmap_value_detail.h#L107)); StarRocks' header states it derives from Doris'.

**The pinned CRoaring versions are the damning part.** Doris pins v2.1.2, StarRocks v4.2.1. **v2.1.2 already exports `roaring_bitmap_range_cardinality`, `roaring_bitmap_rank`, `roaring_bitmap_rank_many`, `roaring_bitmap_select` and `roaring_bitmap_frozen_view`**; v4.2.1 adds the native `roaring64_*` equivalents. Every needed primitive has been in both binaries the whole time. A grep for any of them across the whole of Doris `be/` and `fe/` returns **zero hits** — and Doris' forked `Roaring64Map` does not even define `rank()` or `select()`, the upstream C++ methods having been dropped at fork time.

**Function inventory.** Neither exposes range-cardinality, rank, or select-by-rank. `bitmap_count` is whole-bitmap cardinality only. The closest candidates are `bitmap_subset_in_range` and `sub_bitmap` (offset+limit) — both would be the primitive if implemented correctly, and neither is.

**The implementations — the same linear scan, twice.** Doris' `BitmapValue::sub_range` ([`bitmap_value.h:2602`](https://github.com/apache/doris/blob/cb56b23822af86b7c9919a9fcff319a32e7fb431/be/src/core/value/bitmap_value.h#L2602)) iterates `_bitmap->begin()` one set bit at a time, `continue`-ing until `range_start` and inserting each retained element with a scalar `add()`. The scan to reach `range_start` is **O(rank(range_start))**. `offset_limit` is worse, explicitly advancing an iterator `abs_offset` times before emitting anything. StarRocks' `bitmap_subset_in_range_internal` ([`bitmap_value.cpp:1105`](https://github.com/StarRocks/starrocks/blob/1299f3903660981c844aba48deb55c378e67106a/be/src/types/bitmap_value.cpp#L1105)) is the same code with the `continue` rewritten as a separate loop. This is byte-for-byte the ClickHouse pathology recorded in §3. For a mask of 10⁸ IDs and a tile at row-ID 9×10⁷, one call walks ~9×10⁷ iterator steps. Per tile.

**The near-miss makes it worse.** StarRocks *does* call `roaring64_bitmap_range_cardinality` — exactly once, in [`deletion_bitmap.cpp:52`](https://github.com/StarRocks/starrocks/blob/1299f3903660981c844aba48deb55c378e67106a/be/src/formats/deletion_bitmap.cpp#L52), on a raw `roaring64_bitmap_t` used for Iceberg/Paimon deletion vectors, with a sibling using the O(log n) `roaring64_iterator_move_equalorlarger`. The engine knows the fast API exists and uses it correctly, three directories from the SQL bitmap path that scans linearly. Separately, StarRocks' forked `Roaring64Map` retains correct `rank()` and `select()` implementations — with **zero callers** anywhere in the tree. Dead code.

**No live session-scoped bitmap in either.** `BITMAP` is a column value; deserialisation is full `Roaring::read`, never `frozen_view`. StarRocks user variables are an explicit dead end — `decodeVariableData` accepts only scalar and array types and otherwise throws `Unsupported type: %s in user variable`. This is `pg_roaringbitmap`'s limitation with none of its compensating primitives.

**Bitmap indexes.** StarRocks' works but is unsuitable: an IN-list is executed by a loop doing one full `Roaring::read` and one pairwise `|=` per probe value — 10⁴ deserialisations and 10⁴ accumulating ORs, no `fastunion` — and it may not run at all, since `BitmapIndexEvaluator` abandons the index above 0.1% selectivity by default and `bitmap_max_filter_items` defaults to 30. **Doris' bitmap index does not exist in `master`**: the parser, validation and protobuf paths remain, but a grep for `BitmapIndexReader|BitmapIndexIterator|bitmap_index` across all 3,067 files of `be/src` returns nothing, and `bitmap_index_reader.h` 404s on `master` while returning 200 on `branch-3.0` — while [the docs still describe it as a live feature](https://doris.apache.org/docs/table-design/index/bitmap-index/) with no deprecation notice.

**Silent precision loss, both systems.** `bitmap_hash` — the documented route for bitmapping non-integer keys — is **32-bit** MurmurHash3. Collisions are certain past ~10⁵ distinct keys and silently merge identities. `bitmap_hash64` exists and is fine, but the 32-bit variant is the one in every tutorial. Doris' `to_bitmap` is worse: an unparseable string is skipped with no error and no NULL, and a negative `BIGINT` is dropped by `if (value >= 0)` — which is why a separate `to_bitmap_with_check` had to be added. **Silent row loss on ingest into an authorisation bitmap is unconditionally disqualifying.**

**Verdict: neither changes the conclusion, and Doris in particular was over-rated by the earlier draft.** This is now the **third independent confirmation of the same pattern** — ClickHouse, Doris and StarRocks all link a CRoaring exporting `range_cardinality`, `rank`, `select` and `frozen_view`, and all three call none of them from their bitmap layer. That is not an oversight to work around. It reflects that no stateless columnar engine has a reason to want a rank/select-addressable bitmap, because it never holds one across queries. The survey of bitmap-capable OLAP engines is complete and negative.

---

## 7. Apache Accumulo

Added on request, to test the instinct that Accumulo must have solved boolean-predicate evaluation at scale. Sources read directly: `apache/accumulo` @ `5c7d96d` (main, 2026-07-24) and branch `2.1`; `apache/accumulo-access` @ `d0bab94`. Benchmarks run against compiled `accumulo-access-core` sources on JDK 21.

### 7.1 How it works

Two generations, both single-pass recursive descent over raw bytes — no parser generator, no AST allocation in the hot path. Through 2.1, `ColumnVisibility.ColumnVisibilityParser` builds a `Node` tree whose `TERM` nodes are `(start,end)` offsets into the original `byte[]`, and `VisibilityEvaluator.evaluate` walks it doing `auths.contains(...)` per leaf with early return on AND-false / OR-true. In 4.0 both classes are `@Deprecated` and delegate to the extracted `accumulo-access` library, where [`ParserEvaluator.parseAccessExpression`](https://github.com/apache/accumulo-access/blob/main/modules/core/src/main/java/org/apache/accumulo/access/impl/ParserEvaluator.java) **parses and evaluates simultaneously**, returning `boolean` and never materialising a tree. Tokenizer, char buffer and a zero-copy `CharsWrapper` are `ThreadLocal`, so steady-state evaluation allocates nothing.

Per-cell complexity is O(len(expression)), one hash lookup per token, **independent of the number of authorisations**. Measured on a 10-token expression: ~0.7–1.5 µs uncached, 1–2 M evals/sec/core. The grammar forbids negation and forbids mixing `&` and `|` at one level without parentheses ([SPECIFICATION.md](https://github.com/apache/accumulo-access/blob/main/SPECIFICATION.md)) — structurally identical to this design's predicate language.

### 7.2 Is there any indexing of visibility expressions? — No

Flatly negative. The only files under `core/.../file/` mentioning visibility are `VisMetricsGatherer`, `VisibilityMetric` and `PrintInfo` — an **offline CLI tool**. There is no visibility index in RFile, no per-block visibility bloom filter, no term→cell posting list, no bitmap. `ColumnVisibility` is the 4th component of the Accumulo `Key` and participates in sort order, but because it sorts *after* row/cf/cq it can never skip blocks for a row-range scan.

Enforcement is one link in a linear iterator chain applied to every cell already being decoded: [`SystemIteratorUtil.setupSystemScanIterators`](https://github.com/apache/accumulo/blob/main/core/src/main/java/org/apache/accumulo/core/iteratorsImpl/system/SystemIteratorUtil.java#L61) builds `DeletingIterator → ColumnFamilySkippingIterator → ColumnQualifierFilter → VisibilityFilter`.

**Blunt version: Accumulo did not solve the hard problem — it arranged not to have it.** Its access model is a per-cell `accept()` predicate inside a scan that was already happening, made cheap enough to disappear into the noise of RFile decompression and Thrift serialisation. There is nothing to reuse at the indexing layer because there is no indexing layer. The DNF-terms-plus-Roaring-posting-list construction in this design is strictly more ambitious than anything in Accumulo.

### 7.3 Caching, and what it buys

**Result memoisation per scan session** is the significant one: [`VisibilityFilter`](https://github.com/apache/accumulo/blob/main/core/src/main/java/org/apache/accumulo/core/iteratorsImpl/system/VisibilityFilter.java) holds `LRUMap<ByteSequence,Boolean> cache = new LRUMap<>(1000)` keyed on the raw visibility bytes, so a scan over N cells with D ≤ 1000 distinct labels costs D evaluations and N hash lookups. Measured: cache hit ≈ **28 ns** vs ≈ **1.5 µs** full parse-and-evaluate — **~50×**, the single most important optimisation in the stack. Note `deepCopy` creates a new cache in the system filter, so it is per-thread and per-scan-session, not shared across users.

Short-circuiting swaps in a `shortCircuitPredicate` once a result is determined, bounding worst-case work without skipping parsing. An empty-auths fast path returns a length check with no evaluator. **Node reordering is not used** — `normalize()`/`flatten()` exist, are deprecated, and a grep for `.flatten()` across non-test source returns zero hits.

### 7.4 Does it scale to ~10⁴ authorisations per user?

Per-cell evaluation, yes trivially — it is a hash set:

| nAuths | evaluator build | ns/eval | serialised bytes |
|---|---|---|---|
| 100 | 0.5 ms | 208 | 1,266 |
| 1,000 | 1.8 ms | 224 | 12,966 |
| **10,000** | **4.4 ms** | **221** | **129,966** |
| 100,000 | 48.5 ms | 230 | 1,659,966 |

**Per-scan overhead is where it hurts.** Authorizations are `list<binary>` in the Thrift `startScan` struct, so the full set is sent **on every scan RPC** — ~100 KB at 10⁴ auths. On receipt, `ThriftScanClientHandler` routes to `ZKAuthorizor.isValidAuthorizations` → `getCachedUserAuthorizations` → `new Authorizations(byte[])`. `ZooCache` caches the *bytes*, not the object, so **every scan RPC re-runs split, Base64-decode, `HashSet` and `TreeSet` sort over the whole blob**: measured **3.6 ms at 10⁴ auths, 43 ms at 10⁵**, then 10⁴ hash lookups, then a fresh evaluator build (+4.4 ms). That is **~8–10 ms of fixed setup per viewport query before a single cell is read**.

No documented cap exists. The structural limit is that per-user auths live in a single ZooKeeper znode; at 10⁴ auths the serialised value is ~130 KB (fine under ZK's ~1 MB `jute.maxbuffer` default), at 10⁵ it is ~1.6 MB and would exceed it.

### 7.5 Could Accumulo be the backend?

The Morton-key idea is sound — Accumulo's key space is lexicographically sorted, a Z-order-prefixed row key makes a quadtree tile a contiguous `Range`, and tablet splits follow data density automatically. Everything downstream fails.

**Exact count of visible cells in a range without scanning every cell: no.** There is no COUNT RPC and no aggregate API. `Combiner`/`SummingCombiner` aggregate values across versions of one key, not cardinality across keys. The only correct construction is a custom counting `SortedKeyValueIterator` above `VisibilityFilter` — exact and leak-free, but **O(cells in range)** with per-cell decode. `TabletInformation.getEstimatedEntries()` and metadata `DataFileValue` counts are unfiltered and tablet-granular. **Per-tile masked counts are dead**: what `range_cardinality` does in roughly O(containers) becomes a full range scan.

**Post-filter sampling: no.** Accumulo's sampler runs at write/compaction time and is stored as a separate section inside each RFile ([sampling docs](https://accumulo.apache.org/docs/2.x/development/sampling)). In `ScanDataSource.createIterator`, `fileManager.openFiles(files, isolated, samplerConfig)` sits at the **bottom** of the stack, below `VisibilityFilter`. So it is **sample-then-filter, not filter-then-sample** — precisely the ordering this design's I7 forbids. For a user whose authorised rows are a sparse minority, a 1% sample returns ~1% of *all* rows and discards most.

**Latency.** The [official 2.1 benchmarking paper](https://accumulo.apache.org/papers/accumulo-benchmarking-2.1.pdf) reports random single-row lookup averages of 0.17–0.50 ms on 300–1000-node presplit clusters, so warm isolated point lookups can dip under a millisecond. But the [Scan Executors docs](https://accumulo.apache.org/docs/2.x/administration/scan-executors) give the realistic mixed-workload figure: small random lookups averaged **250 ms** under contention with long scans, improving to 5 ms with `IdleRatioScanPrioritizer`. Add ~8–10 ms of per-scan auth deserialisation and sub-millisecond is not conceivable — 3–4 orders of magnitude off.

**Operational weight.** `accumulo-core` compile-scope dependencies include `hadoop-client-api` and `-runtime`, `zookeeper` + `zookeeper-jute`, `libthrift`, Guava, Jackson, Caffeine, Micrometer, OpenTelemetry, datasketches, flatbuffers, snakeyaml. A deployment needs HDFS, a ZooKeeper ensemble, Manager, TabletServers, Compactors — all JVM, against a memory-mapped Arrow file.

### 7.6 Does it leak aggregates? — essentially no, and this is its strongest result

**Iterator stack ordering is correct by construction.** `ScanDataSource.createIterator` builds `visFilter` first, then `IteratorConfigUtil.loadIterators(visFilter, ...)` — **every** user-configured scan iterator, including any counting or aggregating one, is stacked *above* the visibility filter and physically cannot observe a cell the scan's auths do not admit. Unlike Elasticsearch DLS, there is no separate filter and aggregation path to fall out of sync.

**Combiners do not cross visibilities** — `Combiner.ValueIterator` compares with `PartialKey.ROW_COLFAM_COLQUAL_COLVIS`, and the class javadoc states combination is not performed across column visibilities. A secret value can never be folded into a public cell.

**The one real leak vector is Summaries, and it is explicitly gated.** `VisibilitySummarizer` counts occurrences of each visibility label per file, and `TableOperations.summaries()` supports `startRow`/`endRow` — so it genuinely is "counts per label over a key range", **not** filtered by the caller's authorizations. It requires `TablePermission.GET_SUMMARIES`, a distinct permission not implied by `READ` and not granted by default, and [the docs flag it](https://accumulo.apache.org/docs/2.x/development/summaries): *"Because summary data may be derived from sensitive data, requesting summary data requires a special permission."*

**Accumulo's default read path satisfies the hard requirement.** No count, density or existence signal over unauthorised cells reaches a scanner. This is a materially better security posture than every other system in this review, and it is worth saying so plainly.

### 7.7 What is reusable

**The parser and evaluator, standalone, with zero dependencies — already packaged that way.** As of 4.0 the logic was extracted into [`apache/accumulo-access`](https://github.com/apache/accumulo-access), whose README states it provides the same functionality, semantics and syntax *"in a standalone java library that has no dependencies (for example no Hadoop, Zookeeper, Thrift, etc dependencies)"*. Verified by compiling `modules/core/src/main/java` with bare `javac` — it builds and runs with nothing on the classpath. `pom.xml` declares only test-scope dependencies. Apache-2.0.

Do **not** depend on `accumulo-core` for this — it pulls the whole stack, and both classes are deprecated there anyway. The [ABNF grammar](https://github.com/apache/accumulo-access/blob/main/SPECIFICATION.md) is ~10 lines and an ANTLR4 example ships alongside, so reimplementing natively in Rust is a day's work and is probably the right call given DNF normalisation is happening at ingest anyway.

**Maturity caveat:** released versions are `1.0.0-beta` (Feb 2024) through `1.0.0-beta3` (Apr 2026) — **no 1.0.0 GA**.

### 7.8 Verdicts

**(a) As a backend: no.** Three failures, each fatal alone — no exact count of visible cells in a range without visiting every cell, which kills per-tile masked counts; sampling is pre-filter rather than post-filter, which kills masked top-k and directly violates I7; and latency is 3–4 orders of magnitude too high, with ~8–10 ms of per-scan authorisation deserialisation on top of a 0.2–5 ms floor. Accumulo is a write-heavy, scan-oriented, disk-resident store; this is a read-only, mask-reuse, sub-millisecond interactive workload.

**(b) Worth reusing:** the `accumulo-access` grammar and semantics as the label language, adopted verbatim rather than invented — the no-negation, no-mixed-operators constraint is exactly what makes DNF normalisation finite and safe, and it buys interoperability with existing Accumulo label corpora for free. The **iterator-ordering discipline as a design rule**: the visibility filter sits structurally below every aggregation path, which is why Accumulo does not have Elasticsearch's DLS-versus-aggregation leak; mirror it by making the mask the only entry point to the geometry array. And two negative lessons — result memoisation keyed on the label string is worth ~50× if any per-item evaluation survives, and Accumulo's per-scan auth re-deserialisation is a concrete instance of exactly the rebuild-per-query failure mode this design exists to avoid.

---

## The blunt verdict

**No system examined can replace the hand-rolled Roaring + mmap layer, and the reason is structural rather than a matter of tuning.**

The design's load-bearing move is *build the selection once, reuse it thousands of times*. Every system examined does the opposite by construction, because that is the only model a stateless query engine can offer. The absence is uniform across four surveys and is the strongest available evidence that this is a build.

The secondary primitive, `range_cardinality`, exists in exactly one place in the SQL world — **`pg_roaringbitmap`**. Three separate engines now demonstrate the same pathology: ClickHouse, Doris and StarRocks each vendor a CRoaring that exports `range_cardinality`, `rank`, `select` and `frozen_view`, and each implements its own range-restriction as an element-at-a-time scan from zero while calling none of them.

**Row-level security is not a fallback.** The PostgreSQL measurements are the sharpest result in this review: a fully patched server discloses the exact count of policy-excluded rows through `EXPLAIN ANALYZE`, and discloses invisible-cluster density at 1,500× above background through plain `EXPLAIN`, because the planner's statistics are computed over all rows and the viewport's comparison operators are leakproof. That is cluster existence and density over unauthorised data — the exact disclosure this design forbids — arriving through a supported, unprivileged interface. It also costs 3,340× on the viewport query. It is worse than nothing.

**Accumulo is the security model to imitate and the wrong engine to adopt.** It is the only system in the entire survey whose default read path satisfies the hard requirement, and it achieves that by structural ordering rather than by care: every aggregating iterator is stacked above the visibility filter and cannot see what it filters. But it has no visibility index at all, cannot count visible cells in a range without scanning them, and samples before filtering rather than after.

### What adoption would cost

The closest viable substitution is ClickHouse or Postgres + `pg_roaringbitmap` holding masks in a session temporary table. Per-viewport latency moves from sub-millisecond in-process to a network round trip plus parse plus plan plus a **full mask deserialisation** whose cost scales with mask cardinality rather than viewport size — the wrong asymptotic shape for panning. In flexibility, you trade an auditable in-process trusted computing base for one including a query optimizer that silently rewrites predicates (Druid's `inFunctionThreshold=100` → `SCALAR_IN_ARRAY`), a statistics subsystem computed over invisible rows, and — in Pinot's case — a silent Bloom-filter fallback admitting false positives into an authorisation decision.

### Keep the plan, and steal these specifics

1. **CRoaring's frozen format** — `roaring_bitmap_frozen_serialize` / `frozen_view`. Write the mask at recompute; mmap and take a frozen view at session start. Zero deserialisation, zero allocation. Precisely what ClickHouse, Doris and StarRocks all fail to do and what Druid's `ImmutableRoaringBitmap`-over-mapped-buffer does correctly.
2. **`roaring_bitmap_range_cardinality`**, and **`roaring_bitmap_and_cardinality`** for masked counts without materialising an intersection.
3. **`roaring_bitmap_rank` + `roaring_bitmap_select`** in O(log containers), with `rank_many` for batches.
4. **`roaring_bitmap_range_uint32_array`** straight into the gather buffer, and **`roaring_bitmap_intersect_with_range`** to cull empty viewport ranges before counting.
5. **The pre-exploded pair table plus hash semi-join** for the mask build — ~3,000× faster than array-containment on a list column, whether the join runs in DuckDB or in hand-rolled Rust. This is a schema decision, not a database decision, and it belongs in the implementation plan.
6. **DuckDB as the reference implementation for the mask build**, validating the Rust kernel. A wrong mask is a disclosure bug, so a second independent implementation earns its keep.
7. **ClickHouse's small-set threshold of 32** for the long tail of tiny per-term bitmaps.
8. **Druid's 0.12 ratio heuristic** for the mask build: below ~12% probe-to-dictionary ratio use per-term lookups, above it a single sorted merge.
9. **Vespa's hash-dictionary + `rank: filter`** blueprint as the reference design for the term-intersection kernel.
10. **`accumulo-access`'s grammar and semantics** as the label language — no negation, no mixed operators without parentheses, which is exactly what bounds DNF normalisation. Adopt verbatim rather than inventing syntax.
11. **Accumulo's iterator-ordering discipline** as a design rule: the mask is the only entry point to the geometry array, so no aggregation path can be constructed around it.
12. **Qdrant's `exact: true`-by-default API shape** — if a count is ever exposed, make approximation impossible to reach by accident.
13. **`pg_roaringbitmap` as the reference for primitive vocabulary**, and as a plausible mask-storage tier where a round trip is affordable.

### Consolidated unverified items

- **DuckDB semi-join scaling beyond 10⁸ pairs**, and multi-core scaling above `threads=2`. The 10⁹-row figures are linear extrapolation, not measurement. Re-running on a 16–32 core box would close this cheaply.
- **Whether any DuckDB→Arrow output path is genuinely zero-copy.** The surviving C API is a conversion; measured throughput is consistent with a copy, and the zero-copy claim traces to a 2021 blog post about a now-deprecated API.
- **Whether BigQuery or Databricks leak invisible-row cardinality through query plans or execution statistics** the way PostgreSQL's `EXPLAIN` does. Not testable without accounts. BigQuery's documented query-duration side channel is suggestive but is a different mechanism.
- **Databricks' "always makes the secure choice" claim** — vendor assertion, no independent measurement found.
- **BigQuery's maximum row access policies per table** — the quotas page is JS-rendered and did not yield to fetching.
- **Whether any `LEAKPROOF` marking in PostgreSQL's `pg_proc` is incorrect.** Only the operators relevant here were checked; CVE-2019-10130 shows the marking set has needed correction before.
- **Doris' bitmap-index removal commit** — the code state was verified, the intent was not; a shallow clone carries no history and code search was unavailable.
- **Accumulo's ZooKeeper `jute.maxbuffer` behaviour at 10⁵ authorisations** — the znode arithmetic is confirmed, the failure was not reproduced against a live cluster.
- **PostgreSQL and Accumulo timing figures** are single-run microbenchmarks on shared hardware, not controlled benchmarks. The ratios are robust; the absolute numbers are not.
