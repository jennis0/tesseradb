# Design memo — secondary attribute indexing: a technology survey

**Date:** 2026-07-29
**Status:** **survey, not a specification.** No design document should be amended on the
strength of this memo. It enumerates mechanisms, scores them, and records why each
candidate was kept or set aside. Where it recommends, it recommends a *direction* and names
the measurement that would settle the question.
**Scope:** storing, filtering and (conditionally) searching seven additional attribute
families — category, short-form text, keywords, long-form text, dates, integer ranges,
vectors.
**Origin:** owner exploration of future capability, 2026-07-29. Widened twice during the
session on owner direction: first to include storage-engine and KV substrates rather than
only search-engine-shaped candidates, then to require that every major candidate be
explicitly dispositioned with its trade-offs rather than silently omitted.

---

## 0. What this memo is for

Phase 1 is the walking skeleton. None of this is Phase 1 work, and some of it is not work
at all until a deployment asks for it. The memo exists because the *shape* of the answer
constrains decisions that are being made now — principally the bundle layout and the
dictionary namespace — and because getting the shape wrong is expensive in a system where
entity space is append-only and never reordered (**I9**).

Two things the memo deliberately does **not** do.

It does not pick winners for all seven families. Four of the seven are close to forced by
machinery that already exists; two are genuinely contested; one (long-form text) turns on a
requirement nobody has yet stated.

It does not treat any candidate as vetoed by category. Every entry in §3 carries a
disposition *and* what that disposition costs. Several rejections in earlier drafts of this
survey were rejections on grounds that turn out not to hold — the pure-Rust argument in
§4.4 is the clearest example, and it was wrong in a way that would have quietly narrowed
the option set.

**A note on the numbers.** Storage figures at 10⁹ are derived from Appendix A and the
Phase 0 probes. The probes' compression and union figures are policy-dependent and were
measured over a synthetic corpus; per CLAUDE.md they must be re-run before any deployment
with real labels trusts them. Every figure below that descends from them inherits that
caveat.

---

## 1. The socket already exists

The most useful thing to say about this whole problem space is that the interface is
already specified and does not need to be invented. §8.2's filter contract says:

> Every filter returns a set of entity IDs as a bitmap. Composition is intersection.

with four rules — threshold never top-*k*; optional candidate push-down; the mask goes in
first; pre-intersection cardinality structurally unreachable — and a fifth instruction:
keep it a pipeline, not a planner.

So the question is not "how do we add search to Tessera". It is: **for each attribute
family, which mechanism fills a socket whose shape is already fixed?** That reframing is
load-bearing, because it converts a build-vs-buy argument into a set of narrow, mostly
independent decisions, and because it disqualifies some otherwise-attractive candidates for
reasons that have nothing to do with performance.

### 1.1 Scoring axes

The owner's five criteria, plus two the design adds:

| | Axis | Test |
|---|---|---|
| **A1** | Bitmap-producing | Does it yield an entity-space bitmap that can be intersected with `M_auth`, or does it yield rows/hits requiring translation? |
| **A2** | Mask push-down | Can it *accept* a candidate bitmap and evaluate inside it, or only post-filter? |
| **A3** | Disk-backed | Can it serve without holding the structure resident? |
| **A4** | Compressed on disk | Is the on-disk form compressed, and at what CPU cost? |
| **A5** | Embeddable | In-process, no additional service? |
| **A6** | Shard-local | Can one instance run per shard, with no cross-shard coordination? |
| **A7** | Reuse | *Reuse if possible, reinvent only if not* — and reinvention needs a stated reason. |
| **A8** | Lifecycle-compatible | Does it coexist with immutable content-addressed prefixes, pins over segment-set versions, and the three retirement rules? |

A7 is the owner's general principle and it is doing real work here: several sections below
conclude "reinvent" and each is required to say why.

A8 is the axis this survey found to be the most consequential and the least obvious. It is
§4.1's subject.

### 1.2 The one place the requirement is genuinely soft

Mutability of attributes and phrase search on long-form text are both **desirable, not
required** (owner, this session). That matters because both are the deciding consideration
for a different layer — mutability for the substrate, phrase search for the text index — so
in both cases the survey can recommend the cheaper construction while recording precisely
what a later requirement would cost.

---

## 2. Four layers, and where the decisions actually are

Almost every candidate in §3 is really a bundle of answers to four separable questions.
Separating them is what makes the option space tractable.

| Layer | Question | Status |
|---|---|---|
| **A. Substrate** | Where do the bytes live, and who owns durability and consistent reads? | **Contested** (§5) |
| **B. Value encoding** | What is a posting list? | **Settled**: Roaring. `croaring` is already a workspace dependency, used by `tessera-authz`, `-engine`, `-store`. |
| **C. Key → bitmap** | How does an attribute value find its bitmap? | Small code; proven reference designs exist (§6) |
| **D. Not bitmap-native** | Vectors, and ranked text if it lands | **Contested** (§7) |

Layer B deserves one sentence of justification rather than a section, because the decision
is already made and made correctly: the measured cost model is that *bitmap operations cost
O(containers touched), not O(cardinality)*, and every attribute operand composes with
`M_auth` by intersection, so anything that is not a Roaring bitmap of entity IDs has to be
converted into one before it is useful. That is the whole argument. It also means the
interesting property of a candidate is not how fast it filters but **what it hands back**.

Layer C is where a first pass at this survey spent all its attention, and it is the least
interesting layer. A term dictionary handles categories and keywords; a hierarchical bitmap
level tree or a range-encoded bit-sliced index handles dates and integers; an FST or
trigram index handles prefix and substring. All are well-documented constructions with
production implementations to copy. Nothing here is a research question.

---

## 3. Candidate register

Every candidate considered, with its disposition and — this is the point of the section —
what the disposition costs. "Set aside" never means "unsuitable in general"; it means
"unsuitable *here*, for this stated reason, and here is what we give up".

### 3.1 Adopt as libraries

| Candidate | What it gives | Cost of adopting |
|---|---|---|
| **`croaring`** (CRoaring, C) | Already in tree. SIMD-optimised set algebra, `range_cardinality`, portable and frozen serialisation | A C dependency in the trusted path — already accepted (§4.4) |
| **`roaring-rs`** (pure Rust) | Same algebra, no FFI; what `hannoy`/`heed` ecosystem uses | Slower than CRoaring on some ops; running both means two bitmap representations |
| **`heed`** (LMDB) | mmap'd zero-copy reads, multi-process sharing, the Meilisearch-proven path | Single writer; ~2–4 GiB value ceiling; **no block compression** (§5) |
| **`hannoy`** | KV-backed HNSW in LMDB, Filtered-DiskANN-style traversal **with Roaring candidate bitmaps** — i.e. A2 by construction. Default in Meilisearch since v1.29 | Ties the vector path to LMDB; young (0.0.x) |
| **`usearch`** | Apache-2.0, v2.26, **user-supplied u64 keys** so entity IDs go in directly; mmap "view" from disk; f16/i8/**b1** quantization; `filtered_search` applies a predicate *during graph traversal* | C++ dependency; top-*k* API, so §8.2's threshold rule needs care (§7.2) |
| **`fst`** / **`tantivy-fst`** | Automaton-based prefix and fuzzy matching over a term dictionary | `tantivy-fst`'s own README says use upstream `fst`; FST requires the whole dictionary to search (see `tantivy-sstable` for the locality fix) |
| **`tantivy-sstable`** | Sorted-string table with an index, so a `get` is one fetch rather than a whole-dictionary download | Reusing a component of a project whose engine we are not adopting — version drift risk |
| **`tantivy-columnar`** | Column-oriented storage for per-item scalars, bitpacked, with dense-block rank in nanoseconds | Same drift risk; overlaps with our own fixed-width columns |
| **`charabia`** + **`lindera`** | Meilisearch's tokenisation and normalisation, multilingual, CJK via Lindera | Adopting it makes *their* normalisation rules our contract — and contracts §2.10 already says client-side token normalisation becomes contract when the text operand lands |
| **`simsimd`** | 350+ SIMD kernels; L2/cosine/dot plus **Hamming and Jaccard** for binary vectors; AVX2/AVX-512/NEON/SVE | C dependency; header-only, so light |
| **`rabitq-rs`** | Pure-Rust RaBitQ (1-bit and multi-bit) with IVF and MSTG; ~32× compression at better accuracy than PQ/SQ | Young; we would use only the quantiser, not the index |
| **`coitrees`** / **`nclist`** | Static interval trees; van Emde Boas layout for cache locality | Returns interval hits, not bitmaps — useful only as an intermediate (§6.5) |

### 3.2 Adopt the design, not the code

| Source | What to take |
|---|---|
| **Meilisearch `milli`** | The hierarchical facet level tree (§6.6). MIT, so reuse is unencumbered — but the crate is [not published and explicitly not maintained as a reusable library](https://github.com/meilisearch/meilisearch/issues/3367), manages one index only, and owns its docid space |
| **FeatureBase / Pilosa** | [Range-encoded bit-sliced indexing](https://www.featurebase.com/blog/range-encoded-bitmaps): a *k*-bit integer as *k*+1 bitmaps, range queries in O(*k*) bitmap ops, plus masked min/max/sum for free |
| **Filtered-DiskANN** | Candidate-bitmap-constrained graph traversal — the shape `hannoy` implements |
| **Lucene** | `DocValuesSkipper`, `BPReorderingMergePolicy`'s decorator shape, `SearcherLifetimeManager` — all already captured in prior art §1 |

### 3.3 Set aside — the mask would have to cross a process boundary

Elasticsearch, OpenSearch, Solr, Vespa, Quickwit, Meilisearch-the-server, Typesense,
Manticore, Qdrant-the-server, Weaviate, Milvus, ClickHouse (and `chDB`), **ParadeDB /
`pg_search` + `pgvector`**.

ParadeDB deserves the argument stated properly, because it is the strongest one-stop answer
in the entire register: Postgres for the data, tantivy-backed BM25 for the text, pgvector
for the vectors, one system, nothing to synchronise, and a real story for every one of the
seven families. The reason it is set aside is not operational taste and not "we don't want
another service".

It is §12.3: **the mask never exists whole.** Each partition builds its own fragment from
its own term index, so *no process outside a compartment holds a bitmap containing its
entity IDs*. That is the isolation property, and it is why authorisation fans out. Pushing
attribute filters into Postgres requires one of two things: shipping `M_auth` across a
process boundary into a process that is not in the compartment, or letting Postgres compute
the intersection — which puts Postgres inside the trusted computing base. The same
reasoning that excludes Python from the TCB (not because Python is bad, but because the
TCB should be small, auditable and deterministic) excludes a general-purpose RDBMS with a
query planner whose execution time is a function of statistics over data the principal
cannot see. §8.2's "keep it a pipeline, not a planner" is the same objection in miniature.

**What this costs us:** everything ParadeDB would have given for free — hybrid search,
mature text analysis, an aggregation engine, operational familiarity — has to be built or
assembled. That is a large bill and the memo should not pretend otherwise. It is the price
of the compartment property, and the compartment property is the product.

*Residual option worth naming:* a deployment that does **not** need compartmentation (single
partition, one trust domain, no fan-out) has a materially weaker version of this objection.
If such a deployment is ever the target, ParadeDB should be re-evaluated rather than
inherited as rejected.

### 3.4 Set aside — hands back its own IDs, not entity bitmaps

| Candidate | Detail | What it costs us |
|---|---|---|
| **Tantivy** | DocIds are segment-local, insertion-ordered, and renumbered by merges; the index is mmap'd via `MmapDirectory`; scoring is BM25 by default with `ConstScoreQuery` available to skip it | The most mature Rust text index. We give up incremental multithreaded indexing, a tested query parser, block-max WAND, and positions |
| **Lance** | Row addresses are fragment ID ‖ offset — **but stable row IDs exist and are not experimental** (§11.2), which materially weakens this objection. Ships BITMAP / BTREE / LABEL_LIST scalar indexes and a genuine prefilter that [feeds an allow-list into the vector index before distance computation](https://deepwiki.com/lancedb/lance/5.4-filtering-and-row-masking) | A2 done properly, plus one format for columns *and* vectors *and* scalar indexes |
| **DataFusion / Parquet** | Predicate pushdown produces row selections and record batches; Parquet is already a workspace dependency | A whole query engine we already ship. But it optimises with statistics, which §8.2 forbids |
| **DuckDB** | Automatic zonemaps, FTS with `match_bm25`, VSS with HNSW — all in-process | Zonemaps are exactly the "Morton range ≡ contiguous row range" trick, generically implemented |
| **SQLite + FTS5 + `sqlite-vec`** | Smallest possible embedded footprint; FTS5 and vectors cross-referenced by rowid | Ubiquity and auditability; SQLite is arguably the most-tested code in the register |
| **Vortex** | Claims [100–200× faster random access than Parquet at comparable compression](https://vortex.dev/), with **compute pushdown into the encoding** — a range predicate evaluated without full decode; ships `vortex-roaring` | Strong A3+A4, and pushdown-into-encoding is close to what §6.6 wants |

**The cost is a gather, and it is not fatal.** Since we assign the entity ID at ingest, any
of these can store it as a column or fast field. What we cannot get is the *result* as an
entity-space bitmap: a hit set in the engine's ID space needs a per-hit lookup to translate,
which reintroduces O(cardinality) cost into a pipeline whose whole cost model is
O(containers touched). For a highly selective filter that is cheap and these candidates are
live. For a broad filter over 10⁹ items it is the wrong shape.

Lance and Vortex are the two entries here that most deserve a second look, and §10 proposes
the measurement that would settle it.

### 3.5 Set aside — licence

**Xapian.** Mature (1.4.29, April 2025), proven to hundreds of millions of documents,
structured boolean queries, simultaneous search and update. **GPL**, which makes linking it
into this system a licensing decision about the whole system rather than a technical one.
Set aside on that basis alone; the technical fit is otherwise reasonable.

**Groonga** is Lesser-GPL and embeddable, actively released (16.0.8, July 2026), with a
column store as well as full text. Not evaluated further here, and it is a genuine gap in
this survey rather than a rejection — noted in §10.

### 3.6 Set aside — maturity

| Candidate | Finding |
|---|---|
| **infinilabs/diskann** | Pure Rust, complete disk-resident DiskANN, MIT, billion points on 64 GB + SSD — **archived 2025-12-25**, 22 commits |
| **`sled`** | Alpha; rewrite incomplete |
| **`fjall`** | LSM in pure Rust, but [deliberately does not mmap, and active feature development winds down into 2026](https://fjall-rs.github.io/post/fjall-3/) |
| **`faiss-rs`** | Pinned to Faiss 1.7.2, dynamically linked against a system install. `faiss-next` is newer (1.14.x, optional CUDA) but young |
| **`redb`** | Pure Rust, copy-on-write B+trees, LMDB-inspired. **Not an mmap store: `create_mmapped`/`open_mmapped` were removed in 2.0 because the backend's soundness could not be proven** (§11.3). That removes the zero-copy mmap property §8 leans on, so it is not the pure-Rust LMDB analogue this memo first called it |

Archival of the only pure-Rust DiskANN is the single most consequential maturity finding in
this survey: it removes the option that would otherwise have been the best fit for
disk-resident vectors under A5–A7.

### 3.7 Set aside — build weight and non-Rust dependencies, with the trade-off stated

**RocksDB** has exactly the right primitives: zstd block compression (A4), merge operators
so postings append without read-modify-write, column families per attribute, prefix seek,
and a block cache (A3). The costs are real — [slow builds and toolchain
sensitivity](https://github.com/rust-rocksdb/rust-rocksdb), a large C++ dependency, and a
[history of merge-operator-with-column-family binding
bugs](https://github.com/rust-rocksdb/rust-rocksdb/issues/426) — but per §4.4 these are
engineering costs, not a principled objection, and the *real* objection is §4.1's second
lifecycle. If mutability of attributes becomes a requirement rather than a plus, RocksDB
moves back to the front of the queue and should be evaluated properly.

**BitMagic** implements succinct bit-slicing with rank-select compression and serialises
with Binary Interpolative Coding — a materially stronger on-disk compression story than
Roaring, and bit-slicing is exactly what §6.6's BSI wants. Costs: C++ with no Rust binding,
its own format, and [Lemire's comparison notes it uses more memory in RAM than
CRoaring](https://lemire.me/blog/2017/03/31/compressed-bitset-libraries-in-c-and-c/). Set
aside for want of a binding, not for want of merit. If A4 ever becomes the binding
constraint, this is where to look.

**FAISS** is the reference implementation for quantisation and IVF and remains the thing to
benchmark against. Set aside as a dependency on binding maturity (§3.6), not on quality.

### 3.8 Set aside — architecture mismatch

**Qdrant's filterable HNSW** builds *additional graph edges per indexed payload value at
index time*, constructing subgraphs per payload value and merging them into the full graph.
This is a good design for a system where the filter vocabulary is known in advance and
stable. It is structurally wrong here: our filter is a per-session mask, unknown at build
time, different for every principal, and there are as many of them as there are
authorisation states. The graph would have to encode the authorisation lattice. The
`segment` crate is not published for library use in any case.

**Vespa's predicate field** allocates a byte per document over the entire local doc-id space
before matching begins — 1 GB per query at 10⁹ — with a `BitVectorCache` capped at 32
features globally (prior art §1). Predicate cost is Θ(corpus) regardless of selectivity.

**SeekStorm** (Rust, in-process library and server, vector + lexical, in production since
2020, open-sourced 2024) is the least-examined live candidate in this register. It is
in-process, which clears A5, and it is Rust. Whether it hands back anything bitmap-shaped
was not established. A gap, not a rejection — §10.

### 3.9 Set aside on invariants rather than engineering

This class matters because these are the rejections a performance-led survey would miss.

- Any candidate whose native mode is **top-*k* then filter**. §8.2: a top-*k* nearest-neighbour
  query evaluated alone is computed over the whole corpus, so intersecting afterwards *is*
  post-filtering, and the principal's neighbours vary observably with items they cannot see
  (C10). This is the default mode of most vector libraries.
- **Solr's `domain.excludeTags`** — a supported feature for computing facet counts outside
  the security filter. Directly an I2 violation, and it is *documented behaviour*.
- **Elasticsearch DLS**, which documents that a user may "count how many inaccessible
  documents contain a given term".
- Any facet or aggregation engine that computes counts before the mask is applied. This is
  the norm, not the exception, and §9 argues it is the leak surface these attribute families
  actually bring with them.

---

## 4. Where our own constraints are the binding constraint

The register above reads as though the candidates are the problem. For several of them the
truthful account is the reverse: our design forecloses the option, and the foreclosure has
a price. This section states those prices.

### 4.1 Immutable prefixes versus a second lifecycle

Tessera's storage model is immutable content-addressed prefixes plus a WAL, with pins over
per-partition *(prefix, segments-version, watermark)* triples, generation-based retirement,
and three distinct retirement rules for denies. Cache keys are content-addressed precisely
so that a mask fragment need never be invalidated.

Every KV engine in the register brings its own lifecycle: LSM compaction or copy-on-write
B-tree versioning, its own durability semantics, its own notion of a consistent read
snapshot. Adopting one does not add a dependency, it adds a **second lifecycle** that must
be reconciled with pins, with the segment-set version, and with the retirement rules. The
concurrency-lifecycle document's hardest-won property — that a suppression applies to a
pinned request the moment it is accepted, while geometry stays pinned — has to hold across
that boundary too. That reconciliation is where fail-open bugs live, and the corpus records
that conflating retirement rules was caught in review *twice*.

**The price of not adopting one:** if attributes are mutable, we hand-roll mutable postings,
delta layers and compaction for the attribute index. That is reinventing an LSM, which A7
says needs a stated reason. The stated reason is this paragraph — but it is a reason with a
cost, and the cost scales with how mutable attributes turn out to be.

Because mutability is a plus rather than a requirement, the defensible position is:
**build the attribute index as immutable extents alongside the existing bundle, and treat
mutability as a deferred decision with a named escalation** (RocksDB or LMDB for the
attribute layer only, kept out of the geometry path). What must not happen is drifting into
a hand-rolled LSM one delta layer at a time without ever making that decision.

### 4.2 Signature-sorted entity order is an asset that most engines destroy

Entity IDs are assigned in signature-sorted order from day one, and the Phase 0 probes
measured what that buys: **8.9–36.7× on posting storage and up to 130× on union at equal
coverage** (probes, results §4–5). §13.3 records the consequence — the lean against a
row-space sharded term index is precisely that re-scattering into Morton order would forfeit
these wins, since realistic masks measured essentially scattered in row space (run ratio
1.03–1.15).

Any engine that imposes its own document ordering forfeits the same wins, for the same
reason. This is the *substantive* version of the "hands back its own IDs" objection in
§3.4: the problem is not translation cost, it is that tantivy's insertion order, Lance's
fragment layout and an LSM's key order are all *some other order*, and our compression and
union speed are properties of ours.

The exception is instructive. A KV engine keyed by *(attribute, value)* with a Roaring
bitmap of entity IDs as the value does not reorder anything — the ordering lives inside the
value, which the engine treats as an opaque blob. **This is why the KV-substrate option is
qualitatively different from the search-engine option**, and it is the strongest argument in
LMDB's and RocksDB's favour: they are the candidates that do not touch our ordering.

### 4.3 The compartment boundary forecloses the best one-stop answer

§3.3, above. Stated once more in one line because it is the largest single cost this design
imposes on this problem: *the mask never exists whole*, therefore nothing outside the
compartment may compute the intersection, therefore the best-integrated products in the
field are unavailable.

### 4.4 The pure-Rust argument does not hold, and an earlier draft of this survey was wrong to use it

`croaring` is a Rust FFI wrapper around CRoaring, a C library. It is already a workspace
dependency and it is in the *authorisation* path — `tessera-authz` depends on it. The tree
is therefore already not pure Rust, and it is not pure Rust in the most security-sensitive
component.

So "adds a C++ dependency" is not by itself an argument against RocksDB, `usearch`,
`simsimd` or FAISS. The defensible version of the concern is narrower and worth stating
precisely, because it is a real concern:

- **Memory safety in the trusted path.** A memory-safety bug in a native dependency that
  processes attacker-influenceable input inside the compartment is a confidentiality bug,
  not merely a crash. CRoaring is small, extremely widely deployed (ClickHouse, Doris,
  Redpanda, StarRocks, YDB), and heavily fuzzed. RocksDB is vastly larger. DuckDB and
  Postgres larger still. The axis is *auditable surface area inside the compartment*, and it
  ranks the candidates differently from "is it Rust".
- **Determinism.** The auth plugin's determinism obligation (I5, I6) is a hard constraint on
  anything in the *authorisation* path. Nothing in this memo belongs there: attribute
  filters live in `M_sel`, and I12 guarantees they cannot affect authorisation. So the
  determinism argument does not apply to the attribute layer at all — which is a genuine
  freedom that an earlier framing of this survey obscured.
- **Build and supply chain.** Slow builds and toolchain sensitivity are real costs. They are
  costs, not vetoes.

**Consequence:** C and C++ dependencies should be scored on audited surface area inside the
compartment, and only on that. `simsimd` (header-only C99) and `usearch` score well.
RocksDB scores moderately. An embedded RDBMS scores badly, and that is consistent with §3.3
reaching the same conclusion by a different route.

### 4.5 Threshold-not-top-*k* is the constraint that genuinely disqualifies default modes

Unlike §4.4, this one holds. Most vector libraries' primary API is top-*k*, which §8.2
forbids as an independent operand. The mitigation is candidate push-down, and the two
candidates that implement it properly — `hannoy` with Roaring candidate bitmaps, `usearch`
with a traversal-time predicate — are exactly the two that survive. Lance's prefilter is a
third.

This is the axis where the design's discipline *narrowed the field usefully* rather than
costing us something, and it is worth noting that the narrowing is the same as the
mitigation for C10.

### 4.6 The design already contains the trick that makes ranking safe

§8.3 says *filter, do not rank*, on the basis that relevance scores and rank shifts computed
from corpus-global statistics are a demonstrated channel for inferring the content of
unreadable documents (Appendix D). The owner's position is that this is a strong preference
rather than a hard requirement, since every other system in the field ranks.

There is a cheaper resolution than relaxing the invariant, and the design already uses it
elsewhere. §7.7 requires the extractive labeller's background document frequencies to come
from **a fixed public reference corpus, not the live corpus**, precisely because live
frequencies are an aggregate over mostly-unauthorised data and silently violate I2.

Apply the same substitution to IDF. With IDF drawn from a fixed reference corpus:

- A BM25 score becomes a pure function of *(document, query, fixed reference table)*. Term
  frequency, document length and saturation are all per-document; nothing is corpus-global.
- Two principals with different visible sets compute **identical** scores for the same
  document, so score and rank carry no information about what else is in the corpus.
- Composition is unchanged: threshold or filter first, intersect with `M_auth`, then take
  top-*k* over survivors — §8.2's rule, satisfied.
- The reference table is a build-time artifact with a version, so it keys the filter-result
  cache exactly as the plugin version keys the mask cache.

Costs, honestly: relevance quality drops where the corpus's term distribution diverges from
the reference corpus, which is precisely the domain-specific case; the table is another
artifact to build, ship and version; and per-field length normalisation needs care. And this
resolves *lexical* ranking only — it says nothing about learned rankers or cross-encoders,
which reintroduce the problem in a form no reference table fixes.

**This is the most consequential finding in the memo**, because it converts "ranking is
forbidden" into "ranking costs a reference IDF table and one leak-register entry", which
in turn makes the tantivy-shaped candidates in §3.4 worth re-examining rather than set
aside on principle.

---

## 5. Layer A — the substrate

| Option | A3 disk | A4 compression | A8 lifecycle | Notes |
|---|---|---|---|---|
| **Own mmap'd extents** (status quo: `memmap2` + `croaring`) | mmap, page-cache resident on demand | Roaring itself; extents can be zstd-framed | **Native** — one lifecycle, content-addressed | Weak on mutability. Frozen Roaring views permit zero-copy mmap reads (verify against `croaring` 2.x's Rust surface) |
| **LMDB via `heed`** | mmap, zero-copy, multi-process | **None at block level** — values compressed only insofar as Roaring compresses them | Second lifecycle, but a simple one: single writer, MVCC snapshots | The Meilisearch-proven path. `hannoy` requires it anyway. ~2–4 GiB value ceiling constrains very large postings |
| **RocksDB** | Block cache, optional mmap reads | **zstd block compression** | Second lifecycle, a complex one: LSM compaction, write amplification | Merge operators are the right primitive for appending postings. §3.7, §4.1 |
| **`redb`** | mmap'd copy-on-write B-trees | None built in | Second lifecycle; pure Rust | The pure-Rust LMDB analogue. Under-evaluated here |
| **Parquet** (already a dependency) | Row-group granularity | zstd/snappy, already enabled in the workspace | Immutable files — **fits A8 natively** | Row-group min/max = zonemaps. Poor at random access |
| **Vortex** | Fine-grained random access within encoded segments | FastLanes, ALP, FSST; ~Parquet+zstd ratios | Immutable files — fits A8 | **Compute pushdown into the encoding.** The most interesting unexamined option |
| **Lance** | Disk-first, 100× Parquet random access | Columnar encodings | Immutable + versioned — fits A8 | Brings scalar indexes, FTS and vector index in one library |

**Reading of the table.** Three options fit A8 natively because they are immutable file
formats: our own extents, Parquet, and Vortex/Lance. The KV engines all buy mutability with
a second lifecycle. Since mutability is a plus and not a requirement, the immutable options
should be preferred *now*, with §4.1's escalation named rather than forgotten.

The one place this does not hold is vectors, where `hannoy` — the best-fitting candidate on
A2 — is LMDB-native. That is a coherent outcome: LMDB for the vector sidecar only, which is
already specified as living in a different format and being read in a completely different
pattern (§8.3), keeps the second lifecycle out of the hot path and out of the authorisation
path.

---

## 6. Layer C — per-family mechanisms

### 6.1 Category

**Mechanism: the existing term index, in a separate dictionary namespace.** A category is a
low-cardinality label; the term index already maps a term to a Roaring bitmap of entity IDs;
resolution is one lookup and composition is one intersection. Zero new format, zero new
code paths, and it inherits signature-sorted posting compression.

**The one thing that must not be conflated.** Auth terms and attribute terms must occupy
**separate dictionary namespaces**, because auth terms gate label containment (I3) and
maximum frontier depth, while attribute terms may only ever narrow `M_sel` (I12). Sharing a
namespace would make it possible for an attribute value to be mistaken for an authorisation
term by any code that resolves a term ID without knowing its provenance — which is I3
violated by a naming collision. Two dictionaries, or one dictionary with a namespace tag in
the descriptor and a type check at every resolution site.

Storage at 10⁹, one category per item: Appendix A's "term index, ~1 posting/item" row —
~2 GB naive, ~31 MB compressed.

### 6.2 Keywords

**Mechanism: the same, plus an FST or sstable over the dictionary for prefix queries.**
Multi-valued, moderate-to-high cardinality, one posting per (item, keyword). Appendix A's
"~10 postings/item" row: ~20 GB naive, ~310 MB compressed at 10⁹.

Prefix matching for an autocomplete affordance wants an automaton over the dictionary:
`fst` gives that, `tantivy-sstable` gives it with better locality when the dictionary does
not fit comfortably in memory. **§9 argues the autocomplete affordance is a leak, not a
feature, unless the offered vocabulary is containment-filtered.**

### 6.3 Short-form text (titles, names)

Two distinct requirements hide here and they want different structures.

*Token matching* is §6.2 with tokenisation in front — `charabia` is the reuse answer, and
adopting it makes its normalisation rules part of the contract, which contracts §2.10
anticipates.

*Substring matching* ("does this title contain `smith`") is not a token query, and a term
index cannot answer it. The established mechanism is a **trigram index**: postings keyed by
character trigram, a query decomposes into a conjunction of trigrams, and the result is a
superset requiring verification over survivors. Well-trodden (Postgres `pg_trgm`, Zoekt),
and the verification pass is the same shape as §8.3's brute-force-over-candidates and
Appendix F's definite-overlap refinement: verify *after* intersecting with `M_auth`, when
survivors are few.

Cost: a trigram index over short fields is roughly proportional to total text length rather
than item count, so it wants measuring against real field lengths before commitment.

### 6.4 Long-form text

The contested family, and it is contested on a requirement rather than a technology.

**If boolean filtering suffices** (phrase search being a plus, not a requirement), then
contracts §2.10 has already specified the answer and it is the cheapest thing in the memo:
`text/` mirrors `terms/` exactly — the same dictionary extents, the same portable postings,
the same deltas, the same parser, **no new format**. Positionless postings, conjunction and
disjunction by bitmap algebra, tokenisation by `charabia`. This is A7 satisfied by reusing
our own machinery, and it inherits every property of the term index including the
signature-sorted compression.

**If phrase search is later required**, positions must be stored, and positions are the
expensive part of an inverted index — the standard accounts note they both inflate the index
and slow document-level querying because the reader must skip over positional data. Order-
of-magnitude expectation is a multiple, not a percentage, but the honest statement is that
this needs measuring against the real corpus rather than quoting a literature figure.

Three routes to phrase support, in increasing cost:

1. **Positionless index plus verification.** Filter by conjunction of the phrase's terms,
   intersect with `M_auth`, then verify the phrase against the stored text of the survivors.
   Exactly the trigram and definite-overlap pattern again. Cheap in index size, requires the
   text to be retrievable, and costs a read per survivor.
2. **Positions on a subset of fields.** Only where phrase queries are actually wanted.
3. **Adopt tantivy** for the long-form field only, accepting the §3.4 translation cost and
   §4.2's ordering loss on that one index. With §4.6's reference-corpus IDF this also brings
   ranking safely, which makes it a materially better trade than it looked before.

Route 1 is recommended, precisely because it defers the decision at low cost — and because
it is the same construction the design already uses twice.

### 6.5 Dates

Appendix F already specifies this and the memo's job is only to note the reuse position.

The model is the bitemporal quad — earliest/latest possible start, earliest/latest possible
end — canonicalised at ingest (`LPS' = min(LPS, LPE)`, `EPE' = max(EPE, EPS)`), indexed on
the outer hull `[EPS, LPE]`, with *definitely overlaps* evaluated as a refinement pass over
post-mask survivors. Storage: four `u32` per row, 16 bytes, **16 GB at 10⁹ — two-thirds the
size of the entire hot column set** — hence loaded for refinement and display only.

Appendix F specifies a **segment tree of bitmaps**: an interval stored at O(log n) canonical
nodes, a query decomposing into O(log n) nodes, the result their union. Two observations
this survey adds:

- **This construction already exists in production.** Meilisearch's `facet_id_f64_docids`
  is a hierarchical bitmap level tree with a configurable branching factor: level 0 keys a
  value to a docid bitmap, level 1 keys a *range* of level-0 bounds to the union over that
  range, and so on. [Issue #589](https://github.com/meilisearch/milli/issues/589) records
  the build cost as *n·log_b(n)* bitmap unions with a recursive improvement to
  *n + n/(b−1)*, which is the kind of detail one only learns by building it. Reuse the
  design; the branching factor is a tunable we would otherwise have to discover.
- **`coitrees`/`nclist` are the wrong shape**, despite being the obvious "interval index"
  crates: they return interval hits, not bitmaps, so they would sit inside a mechanism
  rather than being one. Useful for the refinement pass, not for the operand.

### 6.6 Integer ranges

Two proven constructions, and **cardinality decides between them** — which is the most
directly actionable finding in this section.

**Range-encoded bit-sliced index.** A *k*-bit integer becomes *k* range-encoded bitmaps plus
a not-null bitmap (FeatureBase: a 16-bit integer needs 17 bitmaps). A range query is O(*k*)
bitmap operations *regardless of cardinality*, and masked min/max/sum come free — which is
I2-clean because they are computed inside `M_auth`, and is a genuinely useful future
affordance.

Cost, stated plainly: range-encoded slices are roughly half-dense, so Roaring degrades to
bitset containers and compresses poorly. At 10⁹ items a `u32` attribute is ~33 bitmaps of
~125 MB ≈ **4 GB per attribute**, largely incompressible. This is where BitMagic's
bit-slicing with interpolative-coded serialisation (§3.7) would earn its place if A4 ever
binds.

**Hierarchical bitmap level tree** (§6.5's structure). Level 0 has one bitmap per distinct
value, so total level-0 postings equal item count and inherit signature-sorted compression;
the levels add ~1/(b−1) overhead. Serves equality and range alike.

**The decision rule:** the level tree's level 0 has one entry per *distinct value*, so at
high cardinality — `u32` timestamps, prices, any near-unique numeric — it degenerates to a
billion singleton bitmaps. BSI is indifferent to cardinality. So:

- **Low-to-moderate cardinality** (hundreds to millions of distinct values): level tree.
  Cheaper, compresses, one structure for equality and range.
- **High cardinality** (approaching item count): BSI. O(bits) queries, bounded size, free
  masked aggregates.

Both are pure bitmap machinery, both compose natively under §8.2, both are shard-local, and
both use a Roaring library already in the tree. Reinvention here is a few hundred lines
against two published designs, which is the reason A7 requires — the alternative is an
engine that reorders our entity space (§4.2).

### 6.7 Vectors

§7.

---

## 7. Layer D — vectors, and ranking

### 7.1 What the design already decided, and why it got stronger

§8.3: vectors are a cold sidecar in a different format; *the query matters more than the
storage*; filtered ANN is the genuinely unsolved problem but the other filters usually solve
it first — if label and text filtering plus the mask reduce candidates below ~10⁵–10⁶,
**brute-force scan the masked candidates**: a few milliseconds with SIMD, and exact.

Three developments strengthen this rather than undermining it.

**Binary quantisation makes the scan cheap and the storage tractable.** RaBitQ quantises to
1 bit per dimension with a proven error bound (SIGMOD 2024; the 2025 extension proves
asymptotic optimality against Alon–Klartag's lower bound), and scoring is popcount —
AVX-512 `VPOPCNTDQ` on x86, SVE on ARM. At 10⁹ items and 768 dimensions: f32 is 3072 B/item
≈ **3 TB** (Appendix A's figure), 1-bit is 96 B/item ≈ **96 GB**. That is the difference
between "must live in object storage" and "can be resident on a well-provisioned shard", and
it makes the two-stage shape natural: scan 1-bit resident, rescore survivors from the f32
sidecar.

**The 2025 filtered-ANN benchmark literature vindicates the selectivity argument.** Multiple
independent 2025 studies find graph methods degrade sharply at low selectivity while
pre-filtering brute force performs better, and that production systems (Qdrant,
Elasticsearch) ship planners that switch on estimated selectivity. Our situation is the
low-selectivity one by construction — the mask goes in first and is usually the most
selective operand. **Caveat we must not import:** a selectivity-driven planner choosing
between strategies makes execution time a function of how much the principal can see, which
§8.2 forbids. Fix the strategy at design time by cost class.

**Candidate push-down is now available off the shelf.** §7.2.

### 7.2 If an index is wanted anyway

| Candidate | A1/A2 | Trade-off |
|---|---|---|
| **`hannoy`** | Filtered-DiskANN-style traversal constrained by a **Roaring bitmap** — A2 natively, in our value encoding | LMDB-native, so it brings §4.1's second lifecycle to the sidecar. Young (0.0.x). Reported 2× smaller on disk and ~10× faster search than arroy |
| **`usearch`** | **User-supplied u64 keys** — entity IDs go in directly, no translation. `filtered_search` predicate applied *during traversal* | C++; top-*k* API, so the threshold discipline is on us. mmap view from disk. Apache-2.0, mature |
| **Brute force + `rabitq-rs` + `simsimd`** | Trivially A1/A2: the candidate bitmap *is* the scan list | No graph to build, no index lifecycle, exact after rescore. Cost is linear in survivors — fine below ~10⁶, wrong above |
| **Lance** | Real prefilter, allow-list into the index before distance computation | Brings its own row addressing (§3.4) but also scalar indexes and FTS in one library |
| **`arroy`** | Roaring-based filtering, LMDB | Superseded by `hannoy` in its own ecosystem |

Recommendation: **brute force over post-mask candidates as the primary route** — it is what
§8.3 specifies, it is now cheap, and it is the only option with no index lifecycle at all —
with `hannoy` as the named escalation if measurement shows survivor sets exceeding ~10⁶.
`usearch` is the alternative escalation if avoiding LMDB matters more than avoiding C++.

### 7.3 Ranking

§4.6 is the argument; the branch is:

**Filter-only (recommended default).** No scores cross the boundary. Cheapest, and no new
leak-register entry.

**Ranked, with reference-corpus IDF.** Costs: a versioned reference IDF table as a build
artifact; relevance quality degradation where the corpus diverges from the reference; a new
Appendix C entry documenting that scores are corpus-independent *by construction* and that
this is load-bearing rather than incidental; and a conformance test that two principals with
different visible sets receive identical scores for the same document. That last test is the
thing that makes the property real rather than asserted, and it belongs in the definitions
oracle.

**Ranked, with live corpus statistics.** Not recommended. This is the demonstrated inference
channel, and unlike most leaks in Appendix C it is not bounded — it leaks continuously and
proportionally to query volume.

---

## 8. Cross-cutting

**Compression.** Three independent layers, and they should not be confused. (i) Roaring
compresses postings, and signature-sorted entity order is what makes it effective — 8.9–36.7×
measured. (ii) Extent framing (zstd) compresses everything else, and Parquet in the
workspace already has zstd enabled. (iii) Vector quantisation is the only place where
compression is *lossy* and the loss must be corrected by rescoring. BSI slices (§6.6) are
the one structure that resists (i) and would benefit most from a stronger bitmap codec.

**Disk-backed mode.** Roaring's frozen/portable serialisation permits mmap'd zero-copy
views, which is the mechanism that makes an attribute index disk-backed without a second
engine — worth verifying against `croaring` 2.x's actual Rust surface before relying on it.
The residency argument in §10.5 applies unchanged: order the structures by access
*cadence*, not size. Attribute filters are per-*query*, not per-viewport and not per-session,
which puts them in a third cadence class the design has not previously had to reason about,
and that is worth stating explicitly when this lands.

**Cache keying.** §8.5's table extends cleanly: filter results key on *(filter identity,
partition, segment-set version)* and are **shared across all principals**, because a filter
result depends on the query and not on the token. Two cautions. First, r19's lesson applies
directly — term IDs are bundle-relative ordinals, so the postings identity (manifest digest)
must be in the key or a cache surviving a rebuild could serve a bitmap naming different
entities. Second, a tokenisation-rule change or a reference-IDF-table change must invalidate
filter results, exactly as a plugin version change invalidates masks.

**Shard-locality.** Every mechanism in §6 is partition-local by construction, because they
are all keyed in entity space and each partition has its own entity IDs (§12.3). Nothing
here requires cross-shard coordination, and nothing here changes §13.3's open question about
mask fragment construction under fan-out.

---

## 9. The disclosure surface these families bring

The mechanisms are the easy part. Each attribute family invites a UI affordance that is a
corpus-wide aggregate, and each such affordance is an I2 violation that looks like chrome.
None of these is a new invariant; all are new *instances* of registered leaks, arriving
attached to attribute types rather than to queries — which is exactly the kind of thing a
mechanism-focused review misses.

| Affordance | Family | Leak | Registered form |
|---|---|---|---|
| Facet with counts | category, keywords | Counts computed before masking are corpus-wide counts over unauthorised records | C8; the fix is to compute over `M_auth`, which is one `and_cardinality` per value |
| Value list / autocomplete | category, keywords, short text | Offering a value reveals that *something* carries it — possibly only items the principal cannot see | **C11 restated.** The offered vocabulary must be containment-filtered exactly as the label vocabulary is |
| **Range-slider bounds** | dates, integers | A slider's min/max is a corpus-wide extremum. Textbook I2, and invisible to review because it looks like layout | **Proposed new entry.** Bounds must be masked extrema, or fixed and data-independent |
| "N results" before intersection | all | Pre-intersection cardinality | C8; structurally unreachable per §8.2 |
| Similarity threshold / neighbours | vectors | Post-filtered neighbours vary with invisible items | C10; closed by candidate push-down |
| Relevance score or rank | long text | Corpus-global statistics | §4.6; either avoided or made corpus-independent by construction |
| Empty-result disclosure | all | "No matches" over a masked set is safe; "no matches" over an unmasked set then gated is not | Same shape as §7.7's refusal argument — the decision must be a function of data on the principal's own side |

The free affordance in §8.1 is the constructive counterpart and should be the recommended
pattern for every family: `range_cardinality` over both masks on the same range gives
matched-and-visible against total-visible, exactly, for two bitmap operations. Highlight in
context rather than removing everything else — and it is I2-clean by construction, because
both operands are inside `M_auth`.

---

## 10. Forced, contested, and what needs measuring

**Forced by machinery that already exists** (low risk, no research):

- Category and keywords → the term index in a separate dictionary namespace, with the
  namespace separation being the one thing that must not be got wrong (§6.1).
- Long-form and short-form text, boolean → `text/` mirroring `terms/`, as contracts §2.10
  already specifies.
- Dates → Appendix F's segment tree, with Meilisearch's level-tree parameters as the
  reference implementation to copy.
- Value encoding → Roaring, already in the tree.

**Contested, with a recommendation and a named escalation:**

- *Substrate.* Immutable extents now; RocksDB or LMDB for the attribute layer only if
  mutability becomes a requirement. Do not drift into a hand-rolled LSM (§4.1).
- *Integer ranges.* Level tree below ~10⁶ distinct values, BSI above (§6.6). Needs the real
  cardinality distribution to settle.
- *Vectors.* Brute force over post-mask candidates with 1-bit RaBitQ and SIMD rescoring;
  `hannoy` if survivor sets exceed ~10⁶; `usearch` if avoiding LMDB matters more than
  avoiding C++.
- *Phrase search.* Positionless plus verification over post-mask survivors. Revisit only on
  a stated requirement.
- *Ranking.* Filter-only by default; reference-corpus IDF is the safe route if it is wanted,
  and it costs a build artifact, a leak-register entry and a conformance test (§7.3).

**Genuine gaps in this survey**, recorded so they are not mistaken for rejections:

- **SeekStorm** — in-process, Rust, lexical + vector, production since 2020. Whether it
  yields anything bitmap-shaped was not established. The most under-examined live candidate.
- **Groonga** — LGPL, embeddable, column store plus full text, actively released. Not
  evaluated.
- **`croaring` 2.x frozen views** — the disk-backed story in §8 depends on this and it was
  not verified against the crate's actual Rust API.

Vortex, Lance and `redb` were flagged as gaps in the first draft of this memo and have since
been examined; §11 records what was found, including two corrections to §3.

**Measurements that would settle the contested items.** All are cheap relative to the
decisions they inform, and all should follow the Phase 0 pattern of measuring over the
synthetic 10⁹ corpus:

1. **Attribute cardinality distribution.** Decides §6.6 outright.
2. **Survivor-set size after mask ∧ label ∧ text**, over the realistic mask population from
   the probes. Decides whether the vector index is needed at all, and it is the single
   highest-value measurement in this list because it determines whether Layer D exists.
3. **BSI storage and query cost at 10⁹** for one `u32` attribute — validates or refutes the
   ~4 GB estimate in §6.6.
4. **Positional index inflation** on real long-form text, if phrase search is ever asked
   for.
5. **Trigram index size** against real short-field lengths (§6.3).
6. **1-bit scan throughput** with `simsimd` over a mask-sized candidate list, to locate the
   crossover in §7.2 rather than assuming ~10⁶.

---

## 11. Deep dive: Vortex, Lance, redb, and off-the-shelf bitmap-index implementations

Added 2026-07-29 on owner request, after the three formats above were flagged as gaps in §10
and the question was raised of whether an optimised implementation of Roaring-based indexes
over a KV store already exists rather than being written here.

Two of the four findings correct §3. One of them corrects a claim this memo made, and the
other weakens an objection this memo relied on.

### 11.1 Vortex — the most interesting substrate, and the least settled

**The model.** Vortex separates in-memory *Arrays* (logical type + physical encoding) from
lazy *Layouts*, which permit query planning without fetching data. Built-in layouts are
`FlatLayout` (one serialised array in one segment), `StructLayout` (one child layout per
field), `ChunkedLayout` (one child per row partition), `ZoneMapLayout` (statistics per
logical zone) and `DictLayout` (dictionary values plus indices). Layouts reference lazily
fetched *segments* whose physical location is abstracted, so the storage backend is
pluggable. The footer holds a segment index mapping row ranges to segment offsets.

**Why it is attractive here.** Three properties line up with things this design already
needs:

- **Compute pushdown into the encoding.** Encodings supply compute kernels, so a range
  predicate over a delta-encoded column is evaluated without full decode. That is §6.6's
  requirement — evaluate a range without materialising the column — arriving as a general
  facility rather than a bespoke BSI.
- **Random access ~200× faster than Parquet from local disk.** The design's gather is a
  sorted list of row IDs and a request for a gather (§10.1); it is *the* access pattern,
  and it is precisely what Parquet is bad at. This is relevant well beyond attributes — it
  bears on the hot columns and the drawn-mark budget's residency question (§10.5, r20).
- **Immutable files.** A8 satisfied natively, no second lifecycle.

**Why it is not settled.** Format stability is managed by date-stamped *editions*
(`YYYY.MM.DD`) rather than semver, with a deliberately minimal footer so older readers can
consume newer files that avoid unsupported features, and writers can target older reader
editions. That is a thoughtful compatibility story, and it is also an admission that the
format is still moving. Planned-but-absent: forwards compatibility via embedded WebAssembly
decompressors. **Licence was not confirmed** and must be before any commitment.

**Disposition.** Promoted from "set aside" to **candidate worth a measurement**, but for the
*gather*, not for the attribute index. The attribute operands want bitmaps; Vortex hands back
Arrow. Where it might genuinely win is Appendix A's hot columns, and that is a different
memo.

### 11.2 Lance — stable row IDs exist, which weakens my objection, but the mechanism is in flux

§3.4 set Lance aside because row addresses are fragment ID ‖ offset and therefore unstable.
**That was incomplete.** Lance has *move-stable row IDs*, and they are an established
feature rather than an experiment: auto-incrementing `u64` IDs, `max_row_id` tracked in the
manifest and assigned during the commit loop, with each fragment carrying a small row-ID
index mapping row ID to row address — usually a simple range after an append. The stated
motivation is exactly ours: compaction changes row addresses, which invalidates index files
and degrades queries.

Two qualifications, and they matter.

**"Move-stable" is not "update-stable."** IDs survive compaction and other physical rewrites,
but *updated rows are deleted and re-appended under new IDs*. Under I9 an entity ID is
permanent — never recycled, never reassigned — so if entity ID were Lance's row ID, an
attribute update would silently mint a new identity for an existing item. Every mask, node
membership and generating set naming the old ID would then be wrong in the worst possible
way: not stale, but *pointing at nothing*, while the item still exists. So entity ID cannot
be Lance's row ID under any mutable-attribute scenario, which returns us to storing entity
ID as a column and paying translation — the original objection, narrowed but intact.

**The mechanism is under active re-evaluation.** [Discussion #6933](https://github.com/lance-format/lance/discussions/6933)
asks whether stable row IDs should stay a purpose-built storage path or become a
system-managed identity column backed by a maintained index, and the maintainers' own list
of concerns is instructive: the manifest-based `row_id_meta` path fragments metadata
encoding, lookup construction, caching and maintenance semantics; it can inflate
manifest-adjacent state over time; it overlaps conceptually with the Fragment Remapping
Index; and the mapping degrades under frequent row-wide updates.

**Disposition.** The objection in §3.4 stands but for a better-stated reason: not "row IDs
are unstable" (they need not be) but **"identity is the one thing we cannot delegate."**
I9's permanence, signature-sorted assignment, and the fact that identity assignment is
*permanent under I9 and cannot be retrofitted* together mean the entity-ID allocator has to
be ours. Lance remains a strong candidate for the **vector sidecar and its scalar
prefilter**, where identity is ours and Lance's row IDs are a private detail — and there the
prefilter is genuinely §8.2-shaped.

### 11.3 redb — the mmap claim in §3.6 was wrong

§3.6 described `redb` as "mmap'd copy-on-write B-trees" and the closest pure-Rust analogue to
LMDB. **The first half is no longer true.** `Builder::create_mmapped` and
`Builder::open_mmapped` were **removed in redb 2.0**, because it was infeasible to prove the
mmap backend sound; the remaining file-based backend is reported to be within a constant
factor. The trade was made deliberately, in favour of an entirely safe API.

That is decisive for A3 as this memo frames it. §8's disk-backed story is *mmap'd zero-copy
Roaring frozen views* — the page cache does the residency management and a bitmap is used in
place without deserialisation. A file-backed store must copy the value into a buffer on every
read. For a per-query attribute operand that copy may well be acceptable; for anything on the
per-viewport path it is not. So `redb` is not the LMDB substitute here; LMDB's mmap is
precisely the property being substituted for.

What `redb` does have, and what is worth stealing conceptually, is its **savepoint and
epoch-reclamation model**: copy-on-write B+trees where committed pages are never modified in
place; ephemeral and *persistent* savepoints, the latter surviving restart, with
constant-time creation and restoration; and pages moving to "pending free" and being
reclaimed once all referencing transactions are orphaned. That is structurally the same
mechanism as our generations, pins and the epoch ledger — a pin *is* a savepoint, and
retirement *is* epoch-based reclamation. If a KV engine is ever adopted, `redb`'s model is
the one whose consistency semantics would map onto the lifecycle document with least
friction, which partly offsets the mmap loss. Durability is configurable per transaction
(non-durable; 1PC + XXH3 checksum; 2PC), which is the right shape for a WAL-fronted design.

**Disposition.** Set aside for the postings path on the mmap regression. Retained as the
reference model for lifecycle semantics if §4.1's escalation is ever taken.

### 11.4 Is there an off-the-shelf Roaring-index-over-KV implementation?

The question is the right one to ask — if several attribute families each want "a keyspace
mapping values to Roaring bitmaps, persisted, compressed, disk-backed", that is one component
written once, and it is exactly the sort of thing that should already exist.

**The honest answer: no mature Rust library does this as a reusable component.** What exists:

| Candidate | What it is | Why it does not serve |
|---|---|---|
| **Meilisearch `milli` facet databases** | The real thing: LMDB keyspaces mapping facet values and value-ranges to Roaring bitmaps, with the hierarchical level tree | [Not published, explicitly not maintained as a library](https://github.com/meilisearch/meilisearch/issues/3367). Reuse the design (§6.5–6.6), not the code |
| **GreptimeDB `index` + `puffin`** | Per-column inverted index: an FST mapping values to bitmap (offset, size) pairs, bitmaps containerised in Puffin as `greptime-inverted-index-v1`. A `Bitmap` abstraction supporting **both `BitVec` and `RoaringBitmap`** | **Bitmaps are over fixed-size row groups (e.g. 4,096 rows), not individual rows.** It is a skip index that narrows *which groups to scan*, not an exact posting list. Under I2 we need exact masked counts, so a superset-with-rescan is not an operand — it is a candidate generator |
| **`bitrush-index`** | Serialisable bitmap index with `memory_index` and `storage_index` modes; claims millions of values/sec on one thread | Directly on-point and the closest to what was asked for. Small, young, single-author; needs evaluation before it could be trusted with an invariant-bearing structure |
| **FeatureBase / Pilosa** | The canonical range-encoded BSI implementation | Go, and a service (§3.3) |
| **`edgesearch`** | Term → Roaring bitmap postings, chunked for Cloudflare Workers KV | A demonstration that the pattern is standard practice, not a reusable component |

**But one layer *is* reusable off the shelf, and it is the layer worth reusing: the container
format.**

**Puffin** is Apache Iceberg's format for auxiliary index blobs — a file holding typed,
independently-addressable blobs with metadata and a footer. GreptimeDB uses it for exactly
this purpose. And Iceberg's `deletion-vector-v1` blob type specifies a **standard on-disk
serialisation for 64-bit Roaring bitmaps**: 64-bit positions split into a 32-bit high key and
a 32-bit sub-position, one 32-bit Roaring bitmap per key, bitmap count as 8 bytes
little-endian, bitmaps ordered by unsigned key comparison, with big-endian length and CRC
fields for Delta compatibility.

That matters because it is precisely the decision this memo would otherwise have to invent:
how do we lay many Roaring bitmaps out in one immutable file, addressably, with checksums and
room for metadata? There is a published spec, multi-language readers, and a production Rust
user. Adopting it costs a little awkwardness (the endianness split is a compatibility scar,
and `deletion-vector-v1` is semantically a delete vector, so we would want our own blob type
alongside rather than misusing theirs) and buys a format decision we do not have to defend.

**Recommendation.** Split the question in two, and the reuse answer differs by half:

- **Container and codec: reuse.** Puffin as the blob container, Iceberg's Roaring
  serialisation as the bitmap encoding, `croaring`'s portable/frozen format as the in-process
  representation. No format invention.
- **The index logic: write it.** A few hundred lines per family over §6's constructions.
  This is the A7 justification, stated: the only implementations that exist are unpublished
  (milli), coarse-grained by design (GreptimeDB), or too young to carry an invariant
  (`bitrush-index`) — and the logic itself is small, whereas the format is not.

One design idea to adopt outright regardless: **GreptimeDB's `Bitmap` abstraction over both
`BitVec` and Roaring**. §6.6 identified that range-encoded BSI slices are roughly half-dense
and therefore compress badly under Roaring; a bitmap type that switches representation by
density resolves that without a new codec, and it is the same insight Lucene acts on when it
prefers `FixedBitSet` above 1% density. The caution from prior art §1 applies and must be
carried across: a dense `FixedBitSet` has **no cheap range rank**, and `range_cardinality` is
load-bearing for us (§8.1's free affordance), so the switch may be taken for BSI slices —
which are never range-ranked — but *not* for postings, which are.

**Added measurement.** Evaluate `bitrush-index` against a hand-rolled Puffin-framed index on
one attribute family, and settle the Puffin blob-type question, before writing the second
family. If it holds up, it removes most of the code this memo assumes we write.

---

## 12. Accumulo — what travels, and what must not

Added 2026-07-29 on owner request. Accumulo is the right place to look and the only place in
this survey where the question is not "can we tolerate its security model" but "what can we
learn from a security model taken as seriously as ours". Appendix E's label syntax is already
adopted from it. Two of the findings below are *validations* of existing decisions, one is a
directly reusable artifact, and one is the clearest external evidence in this memo that I2 is
a real differentiator rather than pedantry.

### 12.1 `accumulo-access` — the reusable artifact is the specification, not the code

The library is Java-only, Apache-2.0, and **no Rust port exists**. So Appendix E's plan — reimplement
the grammar natively rather than take a JVM dependency, and use the reference implementation
as a differential-test oracle in CI — stands unchanged.

What is better than expected: the repository carries a **`SPECIFICATION.md`** and an **ANTLRv4
grammar**. That upgrades the plan from "match the behaviour of an implementation" to "implement
a specification and differentially test against its reference implementation", which is a
materially stronger position for **I5** (two implementations must not disagree about what a
label means). The ten-line ABNF Appendix E describes should be derived from the ANTLR grammar and the
specification, and any divergence between our ABNF and theirs is a defect in ours.

**Action this suggests:** cite `SPECIFICATION.md` in the contracts spec alongside the grammar,
and make the differential oracle test against the specification's stated examples as well as
against random expressions.

### 12.2 The iterator stack — scope-parameterised filters, and correctness under partial input

Accumulo's filtering abstraction is a stack of `SortedKeyValueIterator`s constructed per
request, where system iterators form the foundation and user iterators layer above, and the
same iterator can be configured at three **scopes**: `scan`, `minc` (minor compaction) and
`majc` (major compaction).

Two things travel.

**One filter pipeline, applied at more than one lifecycle point.** The same composable
abstraction serves reads and compactions. Our analogue is that predicate-change entries retire
*at their compaction fold* (lifecycle §3) — the same logic has to be correct both when serving
and when folding, and Accumulo's answer is to make that a property of the abstraction rather
than of two parallel code paths.

**A filter must be correct under partial input.** The documentation is explicit that during
compaction "Iterators will not necessarily see all of the Key-Value pairs in every
invocation", and that combiners must not perform any function that would be incorrect due to
missing values. This is a stronger and more useful statement of a discipline we need: any fold
that computes an aggregate must be associative and tolerant of seeing a subset. It is worth
stating as an explicit obligation on anything that folds deltas.

**One claim I could not verify and am therefore not making.** The documentation does not spell
out where the visibility filter sits in the stack or whether it can be bypassed. The design's
own structural guarantee — that the mask goes in first (§8.2) and that filters live entirely
in the second stage (I12) — should not be justified by analogy to an Accumulo ordering I have
not confirmed.

### 12.3 RFile — three constructions worth copying

RFile is Accumulo's on-disk sorted key-value format (ISAM-organised: data blocks, index blocks,
meta blocks). Three features map onto things this design either has or wants:

- **Locality groups.** Sets of column families stored separately within one file, so a scan
  over frequently-co-accessed columns avoids reading the others. This is exactly Appendix F's
  "storage goes in its own column group" for the bitemporal quad, and exactly the residency
  argument in §10.5 (r20) that structures should be grouped by access *cadence*. Accumulo
  makes it a first-class, configurable table property rather than a layout convention, and the
  constraint it enforces is worth copying: locality groups must have mutually exclusive column
  family sets, and the default group is written last.
- **Multi-level index.** Index blocks may point to other index blocks, so *the entire index
  never has to be resident, even when the file is written*. Directly relevant to the
  external-ID index recently built as a binary search over mmap'd extents: a multi-level index
  is the construction that keeps that bounded as the corpus grows, and the "not resident even
  at write time" property matters for the streaming build under a fixed memory budget.
- **Bloom filters in meta blocks.** A cheap negative test before opening a file. Under fan-out
  a partition could skip reading a postings extent when a term is provably absent, which is a
  real saving on the mask-build path — and it is safe under I2 because a false positive costs
  work, never disclosure, and a bloom filter over *terms the principal already satisfies*
  reveals nothing new.

All three are designs to copy, not code to link: RFile is Java and inside HDFS assumptions we
do not share.

### 12.4 The shard / wikisearch design — our architecture, independently arrived at

Accumulo's canonical search pattern is a **document-partitioned (sharded) index**: row is the
shard ID, column family is the term, column qualifier is the document ID; the
`IntersectingIterator` then performs a boolean AND across terms **server-side, within a
shard**, returning only documents carrying all specified terms. Documents are stored in the
same row as their index, so retrieval needs no second round trip.

This is Tessera's model — partitioned index, intersection evaluated locally, no cross-partition
coordination for the boolean part — reached independently by the most security-serious system
in the field. That is worth recording as validation of §12.3 (architecture design) and §13.3
rather than as something to adopt.

**One part of the pattern must not be taken.** The documented optimisation is that queries run
on all servers but *only those partitions for which low-cardinality terms can be found* are
consulted, using aggregated reverse-index information. That is statistics-driven pruning, and
§8.2 forbids it for a specific reason: it makes execution time a function of how much the
principal can see, and the aggregated cardinality it consults is a corpus-wide count over
mostly-unauthorised records (C8). Accumulo can do this because its threat model does not
include timing and cardinality inference; ours does.

### 12.5 D4M's degree table — the clearest thing we must decline

The D4M 2.0 schema stores four tables: the raw text, a table of unique column|value pairs, its
**transpose** as an index to every unique string, and a **degree table** holding the sums of
unique column|value pairs under an accumulator column. The stated purpose of the degree table
is that it "enables efficient query planning by allowing queries to estimate the size of their
results prior to executing queries", and it is computed at ingest precisely because fast
cardinality access is useful for planning, load balancing and filtering.

This is a well-regarded, widely deployed design whose central optimisation is **simultaneously
both things our leak register forbids**: a materialised pre-intersection cardinality (C8) and a
statistics-driven planner (§8.2). It is the most useful negative example in this memo, because
nothing about it looks like a security decision — it looks like table design.

The transpose-table idea, by contrast, travels fine: it is an inverted index, and we have one.

### 12.6 Sampling — a fail-closed rule worth adopting verbatim

Accumulo's per-table sampling uses a `RowSampler` configured with a hash function
(`murmur3_32` recommended, `md5` and `sha1` available) and a modulus; the decision is
deterministic, so *the same subset of rows is selected for every RFile's sample set*, and the
configuration is persisted inside each RFile.

This validates the priority scheme fixed in contracts r19 (splitmix64-high-16): a deterministic
hash-derived per-item value, computed once at build, giving a reproducible sample. Accumulo
arrived at the same construction for the same reason.

**The rule worth adopting verbatim is the failure behaviour.** A sample scan succeeds *only if
every RFile it spans was written with the same sampler configuration*; mismatched
configurations fail rather than silently returning a mixed sample. Our analogue: a sampled read
spanning extents written under different priority-function versions must **refuse**, not blend.
Silently mixing two sampling schemes would produce a coverage claim that is false — which under
I7 (sampling happens after masking, and direct evaluation is the main selection route) is a
correctness failure that would present as sparse principals' maps being subtly wrong rather
than empty. This belongs in the conformance suite as a byte-scanner or interleaving check.

### 12.7 Bulk import — the pattern we already use, plus one refinement

Files are built offline (historically by MapReduce, now `RFile.newWriter()`), then brought in
by `importDirectory` against an online or offline table, with `offline()`/`online()` table
operations available. Newer 2.x methods deliberately examine files *outside* holding a table
lock.

This is §14's "write to a new immutable prefix and flip the pointer", and the lock-avoidance
refinement matches I11's drain semantics. One refinement we do not have: **files that fail to
import are moved to a failure directory** rather than failing the whole operation. For a
streaming build under a fixed memory budget that is a useful shape — a partition that fails
validation is quarantined and named, and the rest of the build completes — provided the
manifest cannot then describe a bundle as complete when part of it was quarantined. Fail-closed
would require the flip to be refused, with the quarantine used for diagnosis.

### 12.8 How Accumulo handles the aggregate problem — the finding that matters most

Accumulo generates **summary statistics** embedded in each RFile, cached per tablet server,
used for compaction decisions and for user insight into "what data exists in their table
without scanning all records". Available summarizers include `CountingSummarizer`,
`FamilySummarizer`, `DeletesSummarizer`, `EntryLengthSummarizer`, and — notably —
`VisibilitySummarizer` and `AuthorizationSummarizer`.

Access is controlled by a table permission:

> Users must have the table permission `GET_SUMMARIES` in order to retrieve summary data.
> Because summary data may be derived from sensitive data, requesting summary data requires a
> special permission.

Read that carefully. The most security-serious system in this survey — cell-level visibility,
a formally specified label algebra, monotone predicates by construction — handles
aggregate-derived-from-unauthorised-data by **gating the aggregate behind a coarse permission**,
not by computing it over what the requester may see. The documentation is candid that the
summary *may be derived from sensitive data*; the mitigation is that you need a permission to
ask.

That is precisely the construction **I2 rejects**: *a quantity derived from the full dataset
and then gated is a disclosure, not a filtered view.* And `VisibilitySummarizer` sharpens it —
summarising the visibility labels themselves is the label-existence channel that I3 and C11
exist to close, offered as a feature.

Three consequences.

1. **This is the best external evidence in the memo that I2 is load-bearing.** The gap between
   "gate the aggregate" and "compute the aggregate inside the mask" is the entire product, and
   here is a mature, security-focused, widely deployed system landing on the wrong side of it
   deliberately and documenting why. It belongs in Appendix D's provenance discussion, and it is
   a better citation than the Elasticsearch DLS term-count channel because it is not a bug or an
   acknowledged limitation — it is the designed behaviour.
2. **It is a warning about our own summary structures.** §7.9's (term × tile) count matrix and
   any per-file statistics we add are the same shape as an Accumulo summary. The reason ours is
   sound is specific and fragile: a visible count for a tile is the sum over *satisfied* terms,
   so every quantity read is inside the principal's own authorisation. Anyone extending that
   matrix with a term-independent total, or a per-tile grand count, reproduces Accumulo's design
   exactly — and it would look like a harmless denominator.
3. **It argues against a `GET_SUMMARIES`-style escape hatch.** If a future deployment wants
   corpus-wide statistics, the honest mechanism is a separate, explicitly-out-of-band reporting
   path that is not part of the viewer surface — not a permission on the viewer API. The moment
   the viewer surface can return a corpus-wide quantity, the leak register stops being
   exhaustive, and the leak register is exhaustive *because* the surface is enumerable (five
   viewer verbs).

### 12.9 Summary of dispositions

| From Accumulo | Disposition |
|---|---|
| `accumulo-access` `SPECIFICATION.md` + ANTLR grammar | **Adopt as the normative source** for Appendix E's ABNF; keep the Java library as differential oracle only |
| Iterator scopes; "correct under partial input" | **Adopt as a stated obligation** on delta folds |
| RFile locality groups | **Adopt the design** for column groups (Appendix F's quad, cadence grouping) |
| RFile multi-level index | **Adopt the design** for the external-ID index and postings extents |
| Bloom filters in meta blocks | **Adopt** as a negative test on the mask-build path; safe under I2 |
| Shard + `IntersectingIterator` | **Validation**, not adoption — we already have it |
| Sampler config mismatch fails the scan | **Adopt verbatim** as a fail-closed rule; add a conformance check |
| Bulk import: offline build, atomic flip, failure quarantine | **Already ours**; consider the quarantine refinement, fail-closed |
| Low-cardinality partition pruning | **Decline** — statistics-driven planning (§8.2), and consults a pre-intersection cardinality (C8) |
| D4M degree table | **Decline** — C8 and the planner rule, simultaneously |
| Summaries behind `GET_SUMMARIES` | **Decline, and cite as a counter-example.** Gating a corpus-wide aggregate is what I2 forbids |
| `VisibilitySummarizer` | **Decline** — the label-existence channel I3/C11 close |

---

## 13. Sources

Roaring and bitmaps: [CRoaring](https://github.com/RoaringBitmap/CRoaring) ·
[`croaring`](https://crates.io/crates/croaring) · [`roaring`](https://docs.rs/roaring) ·
[Lemire on compressed bitset libraries](https://lemire.me/blog/2017/03/31/compressed-bitset-libraries-in-c-and-c/) ·
[BitMagic](http://bitmagic.io/)

Range and facet indexing: [FeatureBase, range-encoded bitmaps](https://www.featurebase.com/blog/range-encoded-bitmaps) ·
[FeatureBase bit-slice docs](https://docs.featurebase.com/docs/cloud/cloud-faq/cloud-faq-bitmaps-bit-slice/) ·
[milli #589, facet level trees](https://github.com/meilisearch/milli/issues/589) ·
[Startin, how a bitmap index works](https://richardstartin.github.io/posts/how-a-bitmap-index-works)

Text: [tantivy ARCHITECTURE.md](https://github.com/quickwit-oss/tantivy/blob/main/ARCHITECTURE.md) ·
[tantivy 0.24 release notes](https://quickwit.io/blog/tantivy-0.24) ·
[Quickwit, a compressed indexable bitset](https://quickwit.io/blog/compressed-indexable-bitset) ·
[`tantivy-sstable`](https://crates.io/crates/tantivy-sstable) ·
[`charabia`](https://github.com/meilisearch/charabia) ·
[Xapian](https://en.wikipedia.org/wiki/Xapian) · [Groonga](https://github.com/groonga/groonga) ·
[SeekStorm](https://github.com/SeekStorm/SeekStorm) ·
[ParadeDB architecture](https://docs.paradedb.com/welcome/architecture)

Vectors: [USearch](https://github.com/unum-cloud/USearch) ·
[Hannoy](https://github.com/nnethercott/hannoy) ·
[From trees to graphs: Hannoy](https://blog.kerollmops.com/from-trees-to-graphs-speeding-up-vector-search-10x-with-hannoy) ·
[arroy](https://github.com/meilisearch/arroy) ·
[RaBitQ (SIGMOD 2024)](https://dl.acm.org/doi/pdf/10.1145/3654970) ·
[`rabitq-rs`](https://github.com/lqhl/rabitq-rs) ·
[SimSIMD](https://github.com/ashvardanian/SimSIMD) ·
[ACORN](https://arxiv.org/html/2403.04871v1) ·
[Filtered ANN benchmark, 2025](https://arxiv.org/html/2509.07789v1) ·
[Attribute filtering in ANN: experimental study](https://arxiv.org/html/2508.16263v1) ·
[Qdrant filterable HNSW](https://qdrant.tech/course/essentials/day-2/filterable-hnsw/) ·
[infinilabs/diskann (archived)](https://github.com/infinilabs/diskann)

Storage: [`heed`/LMDB via Meilisearch](https://deepwiki.com/meilisearch/meilisearch) ·
[rust-rocksdb](https://github.com/rust-rocksdb/rust-rocksdb) ·
[rust-rocksdb #426, merge operators with column families](https://github.com/rust-rocksdb/rust-rocksdb/issues/426) ·
[Fjall 3.0](https://fjall-rs.github.io/post/fjall-3/) · [redb](https://github.com/cberner/redb) ·
[Vortex](https://vortex.dev/) ·
[Lance filtering and row masking](https://deepwiki.com/lancedb/lance/5.4-filtering-and-row-masking) ·
[LanceDB scalar indexes](https://lancedb.com/docs/indexing/scalar-index/) ·
[DuckDB VSS](https://duckdb.org/docs/current/core_extensions/vss) ·
[DuckDB indexing and zonemaps](https://duckdb.org/docs/current/guides/performance/indexing)

Internal: architecture design §4 (invariants), §7.7, §7.9, §8.1–8.5, §10.5, §12.3, §13.3,
Appendix A, Appendix C, Appendix D, Appendix F · contracts spec §2.10 ·
concurrency-lifecycle §2.3, §3 · prior art §1 · probes results §4–5
