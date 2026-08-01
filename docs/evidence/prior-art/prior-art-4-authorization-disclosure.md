# Prior Art Review 4 — Authorization Systems and Statistical Disclosure Control

**Scope:** Zanzibar family and the reverse-index problem; policy engines with partial evaluation; boolean expression indexing; lattice-based/compartmented security and defence document marking; statistical disclosure control; permission-aware RAG.
**Questions:** Is the label-gating rule known or novel? Can any Zanzibar-family system materialise a per-user visible set at scale?
**Date:** 2026-07-25.

---

## Priority 1 — Is the label-gating rule a known technique?

**It is known. It is not an independent invention, and the team should stop describing it as one.** But the precise statement has two halves, and the second is where the novelty sits.

"Serve a derived artefact only if every input it was derived from is readable by the viewer" is the **conservative label join** of information-flow control, with a primary source and an exact formula. Denning, Akl, Morgenstern, Neumann, Schell & Heckman, *"Views for Multilevel Database Security,"* IEEE Symposium on Security and Privacy 1986, pp. 156–172, states the **Derivation Axiom**:

> V.level = ⊔ {z.level | z ∈ V.source}

— the label of derived data is the least upper bound of the labels of its sources ([PDF](https://conferences.computer.org/sp/pdfs/sp/1986/00044473.pdf), formula confirmed directly). Compose it with the Bell–LaPadula simple security property and instantiate the lattice as the powerset of categories ordered by ⊇ — which is BLP's compartment-set component — and "clearance dominates the LUB of the constituents" *is literally* "generating set ⊆ visible set". **The rule is the compartment-lattice special case of a forty-year-old axiom.**

The same paper anticipates the other two options and names them: a **Sanitization Axiom** for deliberately-lossy derivation (i.e. "the LLM summary destroys information, may we declassify it?" — declining to say yes is defensible but is a choice with a name), and an **Aggregation Classification Axiom**, `W.level > g.level ∀ g ∈ W.target`, for aggregates classified strictly above every constituent.

Three other names for the same thing:

- **High-water mark**, the dynamic form, from Weissman's ADEPT-50 (1969), predating Bell–LaPadula.
- **The DLM join**, Myers & Liskov's Decentralized Label Model: `L₁ ⊕ L₂ = L₁ ∪ L₂` — union of policies, hence *intersection of effective readers* ([PDF](https://www.cs.cornell.edu/andru/papers/iflow-tosem.pdf)).
- **Conservative taint propagation**, in the LLM literature. Siddiqui et al., *"Permissive Information-Flow Analysis for Large Language Models"* (arXiv:2410.03055): *"Taint is usually propagated conservatively: the output of an operation is labeled as the most restrictive... label of its inputs,"* and *"the output label would be the upper bound of all inputs (i.e., the context) used for inference. With LLMs having the ability to retrieve documents from different sources, this can quickly become unnecessarily restrictive, a phenomenon known in the literature as **label creep**"* ([ar5iv](https://ar5iv.labs.arxiv.org/html/2410.03055), quotes verified directly). Their paper exists specifically to argue the rule is *too strict*, proposing propagation only of labels of inputs that were *influential*.

Teresa Lunt's *"Aggregation and Inference: Facts and Fallacies"* (IEEE S&P 1989) settles the status question. Defining the genuine aggregation problem, she notes it only arises when the aggregate strictly dominates every subset, *"otherwise, **the simple mandatory rules would suffice to protect the collection of information**"* ([PDF](https://conferences.computer.org/sp/pdfs/sp/1989/00044312.pdf)). A 1989 statement that containment/LUB is the *assumed default*.

**What is not prior art:** nobody applies this rule where you are applying it. Across Glean, Microsoft 365 Copilot, Elastic, Kendra/Q Business, Azure AI Search and Vertex AI Search, enforcement is uniformly at the *retrieval boundary* — filter candidates by caller identity, then generate — and every published guarantee is about the retrieval set, not the generated text. Microsoft's is closest: *"Copilot can only summarize or reference content that the user is authorized to access"* ([learn.microsoft.com](https://learn.microsoft.com/en-us/microsoft-365/copilot/microsoft-365-copilot-architecture-data-protection-auditing)) — but Copilot sidesteps the problem by **never sharing a derived artefact between users**, grounding fresh per query. The hard case — one artefact precomputed once and served to many differently-cleared viewers — is not what they solve.

Most tellingly, **Microsoft GraphRAG builds exactly this artefact** (hierarchical Leiden clustering producing "community reports" fed wholesale into global search) **and has no permission model at all.** Neither the indexing overview, dataflow docs, global search docs nor the MSR announcement mentions permissions, ACLs or multi-tenancy ([indexing](https://microsoft.github.io/graphrag/index/overview/), [global search](https://microsoft.github.io/graphrag/query/global_search/)). Their answer to confidentiality is *provenance*, not authorisation.

**The defensible claim is narrow and true: the rule is textbook; the application of it to shared, precomputed, LLM-generated cluster summaries appears to be unpublished.** Cite Denning 1986 and claim the application, not the principle. A security reviewer will find the lineage; the document is stronger for having found it first.

Two things to take from the literature:

**Label creep is the named, predicted failure mode.** At 10⁷–10⁹ documents, if clusters form on semantic similarity alone, ⋂ readers(d) over a cluster collapses toward empty and most labels become unservable to almost everyone. This is the strongest argument for clustering *within* ACL-equivalence classes so containment holds by construction — which is what the design's term-based generating sets do.

**Simulatable auditing is a citable virtue.** Kenthapadi, Mishra & Nissim (PODS 2005, [PDF](http://theory.stanford.edu/~kngk/papers/SimulatableAuditing.pdf)) prove that a *refusal* to answer is itself a leak channel whenever the refusal decision depends on data the user cannot see. The containment rule is simulatable — the decision is a function of the visible set and the generating set, both on the user's side of the boundary — so it has a property most suppression schemes lack. Real, non-obvious, citable.

---

## Priority 2 — Can any Zanzibar-family system materialise 10⁶–10⁸ objects per user in tens of ms?

**No. Not one, and the implementers say so.**

**The Zanzibar paper has no list-objects API.** Its five operations are Check, Read, Expand, Write, Watch ([PDF](https://www.usenix.org/system/files/atc19-pang.pdf)). Headline numbers — >2 trillion tuples in ~100TB, >10M QPS, p95 <10ms — are all for point checks.

**SpiceDB has written publicly about this: [authzed/spicedb#207](https://github.com/authzed/spicedb/issues/207)**, "Proposal: Lookup Watch API and Tiger Cache for fast ACL-aware filtering." Baseline: *"Various tests show approximately ~100ms for a LookupResources call on a SpiceDB with 100-250K relationships, and a graph nesting of 3-5 deep."* That is 100ms at a quarter of a million relationships — five to six orders below target set size, at ten times the latency budget. The proposal, whose design was Roaring-Bitmap materialised accessible-resource sets per subject, **was abandoned**: *"We've spent a lot of time investigating this approach and found that it doesn't reach the scale to truly solve this problem long term."*

The SpiceDB FAQ: LookupResources is for when *"the number of accessible resources is relatively small,"* moving to CheckBulkPermissions when *"accessible resources are too large"* ([FAQ](https://authzed.com/docs/spicedb/getting-started/faq)). AuthZed quantifies the post-filter alternative: *"At 10ms per check, 1,000 results takes 10 seconds"* ([use case](https://authzed.com/use-cases/permission-aware-lists-search)).

**OpenFGA ships a hard cap that answers the question directly: *"By default, both ListObjects and ListUsers have a maximum results limit of 1,000"*** (`OPENFGA_LIST_OBJECTS_MAX_RESULTS`), with *"The higher the quantity of potential results in the system, the more time and resource-intensive it becomes"* ([production docs](https://openfga.dev/docs/best-practices/running-in-production)). Their 2025 rewrite — weighted-directed-graph traversal with pub/sub workers and backpressure — is about making ListObjects *predictable* under fanout, and **publishes no benchmark numbers at all** ([Auth0 blog](https://auth0.com/blog/openfga-improved-listobjects-algorithm/)). Known pathologies: `but not` causing disproportionate latency ([#1338](https://github.com/openfga/openfga/issues/1338)) and silent deadline truncation ([#1961](https://github.com/openfga/openfga/issues/1961)) — a correctness hazard where false negatives also matter.

**Ory Keto, Permify, Warrant:** no published performance figures for reverse-index operations found for any of the three — reported as an absence, not a negative result. Permify names its reverse operation "Lookup Entity (Data Filtering)" ([docs](https://docs.permify.co/api-reference/permission/lookup-entity)) with no scale guidance located. Warrant's OSS build is self-described as *"only capable of handling low-to-moderate throughput."*

**The Leopard index is architecturally right and validates the design rather than replacing it.** Leopard exists because *"recursive pointer chasing during check evaluation has difficulty maintaining low latency with groups that are deeply nested or have a large number of child groups."* It abandons graph traversal for precomputed set operations: `(T, s, e)` tuples *"stored as ordered lists of integers in a structure such as a skip list, thus allowing for efficient union and intersections among sets,"* intersection *"requires only O(min(|A|,|B|)) skip-list seeks."* It serves 1.56M QPS median at **<150µs median, <1ms p99** — but sustains only *"roughly 500 index updates per second at the median, approximately 1.5K at the 99th percentile."* That update ceiling is the point: Leopard buys read latency with a heavily asymmetric write path, exactly the trade the design's hourly recomputation makes.

Leopard is not open-source. Its commercial reimplementation is **AuthZed Materialize**, explicitly *"drawing from Google's Zanzibar paper's 'Leopard index' concept"* ([docs](https://authzed.com/docs/authzed/concepts/authzed-materialize)). Read its shape carefully: Materialize does *not* serve queries. It streams permission deltas (`WatchPermissionSets`) plus a cursor-resumable snapshot API, and **you build the consumer and write the materialised sets into your own database**. Asynchronous with unbounded lag, 24-hour event retention, cannot materialise paths containing caveats, wildcard subjects, `.all` intersections, or expiring relationships. No published latency or set-size numbers.

The independent critique for skeptics is Mathieu Larose's [*"Authorization as a Service: Data Filtering is Still Hard"*](https://mathieularose.com/authorization-as-a-service-data-filtering-is-still-hard), enumerating the four possible architectures and concluding that filtering-heavy workloads largely negate the value of an external authorization service.

**Verdict: the Zanzibar family cannot do this; its most sophisticated member solves the problem by becoming a change-data-capture feed into your own materialised store; and that is precisely the architecture already designed.**

---

## Established prior art, with the names to use

### 1. Boolean expression indexing — the actual problem, with a literature

The most useful naming find, and it is not in the authorization literature at all.

The problem shape — a large collection of boolean predicates over attribute-value pairs, one assignment (the user's grants), find every predicate satisfied — is the **boolean expression matching problem** from computational advertising and content-based publish/subscribe.

- **Whang, Garcia-Molina et al., "Indexing Boolean Expressions," VLDB 2009** ([PDF](https://theory.stanford.edu/~sergei/papers/vldb09-indexing.pdf)). Expressions normalised to **DNF or CNF** over `∈`/`∉` predicates; the index is **inverted lists**, with exactly the needed vocabulary: **conjunctions**, **keys** (attribute-value pairs), **posting lists**, **K-indexes** partitioning by conjunction size. 1M DNF expressions build in 7 minutes / 35MB, beating Le Subscribe by 1.58–2.97× and SIFT by 11.2–20.5×.
- **Fontoura et al. (Yahoo), "Efficiently Evaluating Complex Boolean Expressions," SIGMOD 2010** ([PDF](https://theory.stanford.edu/~sergei/papers/sigmod10-index.pdf)). A *warning*: it exists because DNF normalisation *"suffer[s] from exponential blow-up in the size of expressions."* Measured: DNF *"becomes infeasible beyond depth 2"* and exceeded available RAM at depth 3, where Dewey-ID and interval-mapping alternatives scale linearly (35MB at depth 3) and evaluate in 1–8ms.

**Use "boolean expression indexing" as the name for the ingest-side half of the design.** The terminology is already almost theirs — "term" is their *conjunction*, "unit" is close to their *key* set. Align the vocabulary and inherit twenty years of literature. And **take the Fontoura result seriously**: monotone AND/OR over five dimensions bounds DNF blow-up by formula depth and width, but if authors can write deeply nested formulas, ingest-time normalisation is where it fails, not the query path. **If the DNF expansion factor on real predicates has not been measured, that is the single highest-value experiment in the design.**

Note the direction: this literature indexes the *expressions* and probes with an *assignment* — the reverse-index direction, not the Zanzibar direction.

### 2. The Zanzibar data model, and why this is the easier problem

Worth stating explicitly to pre-empt "why aren't you using SpiceDB": **Zanzibar-family systems model authorization as a relationship graph and answer by traversal.** LookupResources is hard because it is reverse reachability over an arbitrary graph with unbounded fanout — OpenFGA attributes performance to *"model shape"* and *"tuple distribution (cardinality, fanout, skew)."*

**This data model is not a graph.** Attribute predicates over five fixed dimensions, no relationship traversal, and crucially **monotone** predicates. Monotonicity is load-bearing: it makes satisfaction upward-closed in the grant set, which is what makes bitmap intersection sound and the whole thing decomposable per-dimension. A strictly weaker expressiveness traded for a tractable reverse index — argue it as a deliberate design decision, not an implicit one.

### 3. Apache Accumulo `ColumnVisibility` — the closest deployed system

The prior art the team most needs to know about, and a near-exact match for the per-document predicate. Accumulo (originally NSA, now Apache) attaches to every cell a **security label** that is a boolean expression over tokens using `&`, `|` and parentheses — **and, exactly like this design, no NOT.** A client scans with an `Authorizations` set, and *"if the Authorizations are determined to be insufficient to satisfy the security label, the value is suppressed from the set of results sent back to the client"* ([docs](https://accumulo.apache.org/docs/2.x/security/authorizations)).

The differences are the interesting part: Accumulo evaluates **per-cell at scan time**, forward direction; never materialises a per-user visible set; publishes no performance guidance on label evaluation. **If someone asks "why not Accumulo": it has the predicate language but not the reverse index, and no aggregate story at all.**

### 4. Defence/intelligence document marking — ISM.ACES

The "one big dimension plus several small ones ANDed together" instinct is right, and the IC has standardised the machine-readable form. **ISM.ACES** (Information Security Marking — Access Control Encoding Specification, V2021-NOV, IC CIO) exists to define *"combinational logic between data attributes and user/entity attributes"* enabling *"consistent enterprise-wide Boolean access decisions"* ([dni.gov](https://www.dni.gov/index.php/who-we-are/organizations/ic-cio/ic-technical-specifications/information-security-marking-access)). Its motivation is precisely the failure mode to avoid: *"access control decisions have been made in local environments based on local interpretations."* Siblings: **ISM.XML**, **NTK**, **TDF**.

The formal underpinning is **Bell–LaPadula's category-set component**: a label is (level, category-set), dominance requires the subject's category set to *contain* the object's. The five-dimension model is a product lattice of category sets. **Say "compartmented / lattice-based mandatory access control" and "dominance"** — reviewers from a defence background will immediately understand, and it puts the ⊆ test in its proper frame.

**Gap:** NATO STANAG marking specs, ACCM/SAP compartment tooling, and "Effective and Efficient Bitmaps for Access Control" ([IEEE 6824485](https://ieeexplore.ieee.org/document/6824485/), paywalled, title suggests direct relevance) were not researched. Someone with IEEE access should read the last one before the design is frozen.

### 5. Partial evaluation — the name for "compile a policy into a filter", and it produces DNF

**OPA's partial evaluation is a documented instance of compiling a policy into a data filter, and its output is structurally the term set.** Rego inputs split into *known* and *unknown*; known values evaluate away; each rule body yields a conjunction; multiple bodies OR together — *"two sets of conditions, A and B, which form the basis of translation into SQL queries"*, i.e. **a disjunction of conjunctions** ([docs](https://www.openpolicyagent.org/docs/filtering/partial-evaluation), [policy fragment](https://www.openpolicyagent.org/docs/filtering/fragment), [data_filter_example](https://github.com/open-policy-agent/contrib/tree/main/data_filter_example)). Oso ships the equivalent as **"data filtering"** ([docs](https://www.osohq.com/docs/guides/enforce/filter-lists)).

**So "partial evaluation producing a residual policy in DNF" is the established name for the ingest-time normalisation step, and OPA could plausibly generate the term set.** Neither OPA nor Oso does the second half — they emit a filter for a database and stop. Nothing about materialising a 10⁷-member bitmap, nothing about aggregates, nothing about derived artefacts.

**Gap: AWS Cedar's partial evaluation was not researched.** Needs a dedicated pass if Cedar is a candidate.

### 6. Statistical disclosure control — established, and the wrong frame

Mature methodology: **primary suppression** plus **secondary/complementary suppression**. Canonical rules from the τ-ARGUS v4.1 manual ([PDF](https://research.cbs.nl/casc/Software/TauManualV4.1.pdf)): the **minimum frequency rule**, the **(n,k) dominance rule** (*"n=3 and k=70% is not uncommon"*), the **p%-rule**. References: Willenborg & de Waal (Springer LNS 155, 2001); Hundepool et al. (Wiley 2012); **τ-ARGUS/μ-ARGUS** from the EU **CASC** project; the [SDC Handbook](https://sdctools.github.io/HandbookSDC/).

Operational thresholds: UK secure-research-environment handbook uses **N=10** default (*"used by the Office for National Statistics… and subsequently adopted by a number of other Safe Settings"*, range 3–30) with a **40%** dominance rule ([PDF](https://securedatagroup.org/wp-content/uploads/2019/10/sdc-handbook-v1.0.pdf)); DfE/ONS SRS checks against a low count threshold of 10; US Census FSRDC uses *"unweighted cell size must be at least three"* with Title 26 tiers of 3/10/20/100. The modern replacement is perturbative: ONS Census 2021 used targeted record swapping (7–10% of households) plus the cell key method (~14% of counts perturbed) explicitly to defeat *"disclosure by differencing"* ([ONS](https://www.ons.gov.uk/peoplepopulationandcommunity/populationandmigration/populationestimates/methodologies/protectingpersonaldataincensus2021results)). The productised modern form is **Snowflake aggregation policies** ([docs](https://docs.snowflake.com/en/user-guide/aggregation-policies)).

**None of this is prior art here, and the mismatch is structural.** Every SDC regime assumes the recipient has *zero* record-level access and accepts that *something* — a perturbed count, a rounded figure, a suppressed-but-bounded interval — is transmitted about invisible records. This design forbids that outright, and the viewer sees most of the corpus. Comparing it to cell suppression is a category error.

**The payoff is worth stating in the design doc:** because every label served is derived exclusively from documents inside the viewer's visible set, **no two served outputs can be differenced to learn anything about a document outside it.** A k-threshold rule would not have that property. The rule makes the entire differencing literature moot by construction.

Two adjacent framings worth citing. **Truman vs non-Truman** (Rizvi, Mendelzon, Sudarshan & Roy, SIGMOD 2004, [ACM](https://dl.acm.org/doi/abs/10.1145/1007568.1007631)): under *Truman* the system silently rewrites the query against the authorised view and answers something distorted (what all BI row-level security does); under *non-Truman* it **rejects** queries not answerable from what the user may see. **This design is a non-Truman policy** — refusing the label rather than regenerating a filtered one — and that is the crisp name for the choice. And **query-set-overlap control** from Denning & Schlörer's *"Inference Controls for Statistical Databases"* (IEEE Computer 16(7), 1983, [PDF](https://faculty.nps.edu/dedennin/publications/InferenceControlsStatisticalDB.pdf)) is the nearest-shaped thing — and genuinely not the same rule: it compares a new query set against *previously answered* query sets to defeat trackers, stateful and adversarial, where this is stateless and compares a fixed generating set against a fixed authorization set.

### 7. "Security trimming" — and a twenty-year-old Microsoft page that already made the argument

The industry name for filtering results to the caller's ACLs is **security trimming**, from SharePoint. Earliest authoritative source: the Office SharePoint Server 2007 SDK, where the trimmer runs *"before they are returned to the user… at execution time"* ([Office 12 SDK](https://learn.microsoft.com/en-us/previous-versions/office/developer/sharepoint-2007/aa980904(v=office.12))) — i.e. it began as post-retrieval pruning. Microsoft later split it into **pre-trimming** (query *"rewritten to add security information"* before index matching) and **post-trimming**, recommending pre-trimming *specifically because post-trimming leaks refiner counts and hit counts* ([docs](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/custom-security-trimming-for-search-in-sharepoint-server)).

The academic version is **Büttcher & Clarke, "A Security Model for Full-Text File System Search in Multi-User Environments," USENIX FAST '05** ([paper](https://www.usenix.org/legacy/events/fast05/tech/full_papers/buettcher/buettcher_html/index.html)), demonstrating that post-filtering lets an attacker infer content of unreadable files from *"the relevance scores or the relative ranks"* — plant test files, watch BM25 shift, recover corpus term statistics — and proposing **query integration** so corpus statistics reflect only the user's searchable files. **This is the "not even aggregate counts" requirement, proven necessary, twenty-one years ago, in the IR setting.** The best single citation for that part of the threat model, and no RAG paper found cites it.

---

## What is genuinely unusual

Four things with no prior art found:

1. **Materialising a 10⁶–10⁸-member visible set *per user*, refreshed hourly, for 10⁴ effectively-unique grant sets.** Leopard and AuthZed Materialize precompute *permission sets*, and Elastic materialises per-identity ACL filters cached as Lucene BitSets — but nobody publishes numbers at this set size with this churn. Leopard's ~500 index updates/sec median is the only comparable figure, for a differently-shaped workload.
2. **Applying the containment test to shared, precomputed, LLM-generated cluster summaries.** The real claim.
3. **Any permission treatment of GraphRAG-style community reports.** The field is empty. This matters beyond this project — the reference implementation feeds *all* community reports at a hierarchy level into global search, and per-document ACL filtering cannot be bolted on afterwards because the report is a single opaque text spanning arbitrary ACLs.
4. **ACL-aligned clustering / per-permission-group summary precomputation.** The natural alternative to per-user regeneration — cluster by ACL-equivalence class first, then semantically within — appears unpublished. Adjacent work on per-silo LoRA adapters (arXiv:2505.22860) and per-tenant index partitioning exists, but tenant ≠ ACL group, and Microsoft's Graph connector guidance explicitly warns *against* denormalising group membership into item ACLs ([docs](https://learn.microsoft.com/en-us/graph/connecting-external-content-manage-items)).

**Not unusual, and should not be claimed:** the predicate language (Accumulo, ISM.ACES), DNF normalisation with inverted/bitmap indexes (Whang/Garcia-Molina, Fontoura, Leopard), per-user materialised visible sets as bitmaps (Elastic ships this), the containment rule as a principle (Denning 1986), and the insistence that counts are inside the boundary (Büttcher & Clarke; SharePoint pre-trimming).

---

## Where an existing system could be adopted

**Adopt the vocabulary, not the systems.**

- **The Zanzibar family should not be adopted for the reverse index.** Evidence is clear (SpiceDB ~100ms at 250K relationships; OpenFGA capped at 1,000 results; Tiger Cache abandoned). AuthZed Materialize is architecturally closest if you want a relationship-graph source of truth for *grants* and will consume a delta stream — but it is proprietary, asynchronous with unbounded lag, restricted in what it can materialise, and still requires you to build the consumer and store. **That is most of your system. Not worth it.**
- **OPA partial evaluation is a real adoption candidate for the ingest-time compile step.** If predicate authoring becomes expressive or policy-driven, OPA's compile API gives residual DNF output free, with a documented policy fragment and existing translators. Risk: the DNF blow-up Fontoura measured — OPA will happily hand you an exponential term set. Worth a prototype specifically to measure expansion factor on real predicates.
- **Elastic's DLS is worth studying as an implementation reference.** A separate `.search-acl-filter-*` index of per-identity access-control documents, `_allow_access_control` on content docs, and the role query *"cached as a BitSet"* wrapping the Lucene reader such that *"DLS filtering works as a prefilter for approximate vector search"* ([Elastic](https://www.elastic.co/search-labs/blog/vector-search-filtering)) — the retrieval-side dual of this design, in production, with published mechanics. Note its fail-open hazard: *"Omitting the `query` parameter entirely disables document level security."*
- **Copy Azure AI Search's failure semantics.** The only vendor documenting fail-closed behaviour: if ACL evaluation fails, *"the service returns 5xx and does not return a partially filtered result set"* ([docs](https://learn.microsoft.com/en-us/azure/search/search-query-access-control-rbac-enforcement)). Everyone else fails open. Given the hard boundary, make this an explicit requirement.
- **Take vendor limits as calibration.** Microsoft Graph: <2,049 external group memberships per user or *"search results become unpredictable"*, >10,000 → HTTP 400. Vertex AI Search: 3,000 readers per document, and access control *"must select this setting during data store creation."* Kendra: 200 ACL entries. **10⁴ grants per user exceeds every published per-user limit in commercial enterprise search by 5–50×.** Worth knowing before anyone proposes buying rather than building.
- **Do not adopt SDC tooling.** τ-ARGUS, k-thresholds and Snowflake aggregation policies all transmit something about invisible records.

---

## Coverage and gaps

**Verified directly against primary sources:** the Zanzibar paper (Leopard mechanics and all quoted numbers); SpiceDB issue #207 including the abandonment quote and the ~100ms figure; SpiceDB FAQ, performance docs, Materialize docs and use-case page; OpenFGA production docs and the Auth0 ListObjects blog; OPA partial evaluation docs; the Larose critique; Whang & Garcia-Molina VLDB 2009 and Fontoura SIGMOD 2010; Accumulo authorizations docs; ISM.ACES; the WorkOS comparison; **Denning et al. 1986 (PDF fetched, all three axiom formulas confirmed)**; **Siddiqui et al. arXiv:2410.03055 (label-creep and conservative-propagation quotes confirmed)**. The last two are what the priority-1 answer rests on.

**Delegated, URLs live but not personally re-verified:** the statistical disclosure control material (ONS/Eurostat/Census thresholds, τ-ARGUS rules, Denning & Schlörer 1983, simulatable auditing, Lunt 1989, Truman/non-Truman, Snowflake, Power BI); and the RAG-vendor material (Glean, Copilot/Graph, Elastic, Kendra/Q, Azure, Vertex, Vespa, GraphRAG, OWASP). Strong but second-hand.

**Not reached:**
- **AWS Cedar's partial evaluation** — not researched at all.
- **Ory Keto and Permify reverse-index performance** — no published figures found.
- **"Effective and Efficient Bitmaps for Access Control"** (IEEE 6824485) — paywalled, title suggests direct relevance.
- **NATO STANAG marking specs, ACCM/SAP compartment tooling** — not researched.
- **TDI (NCSC-TG-021) and NCSC-TR-005 *Inference and Aggregation*** — located but not readable; cited from secondary description only.
- **HCI/visualisation research on access-controlled dashboards** — searched, found nothing. An absence, not a confident negative.
- Whether **Glean and Google pre-filter or post-trim** — neither documents it.
