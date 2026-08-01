# Prior Art Review 2 — Large-Scale Scatterplot and Visual Analytics Systems

**Scope:** deepscatter/quadfeather, Nomic Atlas, the nanocubes→Falcon cube lineage, Mosaic, the McInnes/Tutte ecosystem, other embedding-visualisation products, geospatial vector-tile authorisation.
**Question:** Could an existing visualisation stack replace the frontend and tiling layer, and does anything already solve "sample after per-user masking, nested across zoom"?
**Date:** 2026-07-25.

**Coverage note.** Verified directly: quadfeather's tiler source, the deepscatter README, the Nomic Atlas whitepaper, the Mosaic TVCG paper, the Falcon CHI paper, the Toponymy and Thisnotthat repos, DataMapPlot interactive docs. Delegated with their own caveats: the embedding-product survey and the geospatial survey. **Gaps:** the Nanocubes paper was not read directly (figures come via Falcon's related-work section); [Sample+Seek](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/07/sigmod16sampleseek.pdf) was located but not read and is excluded from conclusions; Hashedcubes figures are from secondary paper notes; Encord, Scale Nucleus, Zeta Alpha and Superb AI were not reached.

---

## 1. The crux question, answered first

**Does anything already solve "sample after per-user masking, nested across zoom"?**

Almost nothing — but not *nothing*, and the exception matters.

**The exception is tippecanoe's `--retain-points-multiplier`**, shipped in 2.41.0 and in production at Felt ([README](https://github.com/felt/tippecanoe/blob/main/README.md)):

- At bake time, retain N× the normal number of points per tile, grouped into **multiplier clusters**, each internally **sorted by retention priority**.
- At serve time, `tippecanoe-overzoom -m` thins back to the normal count by taking **the first feature from each cluster**. With `-j` supplying a filter, it takes **the first feature from each cluster that matches the filter**.

That is "choose k after applying a mask", served from a *single* pre-baked artefact with no per-user baking. Felt explicitly engineered cross-zoom stability of the filtered choice — CHANGELOG 2.43.0: *"Sort the features within each multiplier cluster by its retention priority, for more consistency between zoom levels in filtered feature choice"*.

**The limitation:** it degrades gracefully only while the mask's pass rate stays above roughly 1/N. A user authorised for 1-in-10⁴ documents exhausts every cluster and sees empty tiles — the exact failure mode the design exists to prevent. Tippecanoe moves the cliff rather than removing it. *(See note in §9 on how the design's candidate-list-with-descent mechanism is strictly stronger on this axis.)*

Everything else surveyed fails structurally rather than incidentally. Systems reaching 10⁷ do so by fixing the sample **before any user exists** (precomputed tiles: deepscatter, Atlas) or by **shipping every point to the client** (Apple Embedding Atlas, FiftyOne, DataMapPlot), where masking can only be cosmetic. **There is no third architecture in the field.**

---

## 2. deepscatter and quadfeather

[deepscatter](https://github.com/nomic-ai/deepscatter) is a regl/WebGL scatterplot over a quadtree of Apache Arrow feather tiles fetched on demand — the same shape as the design, by Ben Schmidt, and the renderer behind Nomic Atlas.

**How it actually works**, from [`quadfeather/tiler.py`](https://raw.githubusercontent.com/bmschmidt/quadfeather/main/quadfeather/tiler.py):

- Each tile is one feather file. `first_tile_size` defaults to 1000 for the root, geometric mean at level 1, `tile_size` (default 50,000) below.
- **Each point lives in exactly one tile.** Points fill a tile's buffer until capacity; excess is partitioned recursively at x/y midpoints into four children. No duplication between parent and child.
- Because the client renders the root plus every loaded descendant, **nesting is automatic**: a point admitted to a shallow tile stays on screen as you zoom. Points don't pop.
- An `ix` column — unique sequential uint64 assigned at insertion — is carried per point, with per-tile min/max in the manifest. Extra columns can be split into **sidecar feather files**.

Two consequences. First, **the "sample" at a given tile is just whichever points arrived first in input order** — there is no explicit sampling step. Representativeness is the caller's responsibility, achieved by shuffling input beforehand. Second, the design's Morton-sort/`ix`-as-rank/sidecar arrangement is close to convergent evolution with quadfeather; the design's version is more principled, because a Morton sort makes any tile a contiguous row-ID range, which quadfeather's insertion-order `ix` does not guarantee across the tree.

**Blocker: the licence.** deepscatter is **CC-BY-NC-SA 4.0** ([LICENSE](https://raw.githubusercontent.com/nomic-ai/deepscatter/main/LICENSE)) — noncommercial *and* sharealike, apparently deliberate to protect the Atlas product. Disqualifying for commercial use regardless of fit. Secondarily, GitHub shows v2.10.0 while [npm sits at 2.4.1](https://www.npmjs.com/package/deepscatter).

**Scale, claimed vs demonstrated.** Tagline: "scales over a billion points." README examples: **5.5 million tweets** and **20 million biomedical abstracts**. Schmidt's [Gaia demo](https://benschmidt.org/gaia/gaia.html) is the billion-point artefact but is a static star catalogue. **Honest demonstrated ceiling for the full stack: ~20M.**

**Borrow:** the sidecar-column file split; the tile manifest with per-tile `ix` ranges; confirmation that one-point-one-tile plus render-ancestors is a sufficient and cheap nesting mechanism.

---

## 3. Nomic Atlas

Essentially the product the design is building, minus access control. Atlas renders through deepscatter ([Key Terms](https://docs.nomic.ai/atlas/datasets/data-maps/how-atlas-works/1-key_terms)). The [whitepaper](https://static.nomic.ai/atlas_tech_report.pdf) describes vectorization → layout → annotation → presentation, with a proprietary layout optimizer replacing UMAP and a **hierarchical clustering model over the latent vectors auto-labelled by a custom-trained LLM**. That is functionally the Toponymy layer, productised.

**Demonstrated scale:** **11 million points** (Obelics), described as "the largest interactive data map ever published." Other public maps: 5.4M tweets, 6.4M images. Marketing says "billions". Note the whitepaper's own admission: "Nomic is unable to disclose many technical details of the Atlas system at this time."

**Blocker:** access control is **org- and dataset-level only** ([Security and RBAC](https://docs.nomic.ai/atlas/help/security-and-rbac)). No per-record concept anywhere. "Access controlled datasets" means *the dataset is private*, not *rows are masked per viewer*. Tiles are baked once per dataset, so per-user masking would require re-baking per user — plus it is a closed hosted service.

**Borrow:** the multi-resolution label ontology — one label layer per clustering depth, surfaced progressively. That is the right structure for gating, because a fine label that fails the visibility test can degrade to its coarse ancestor rather than vanishing.

---

## 4. The data-cube lineage: nanocubes → imMens → Hashedcubes → Falcon

These precompute aggregate structures so binned counts under a filter return in constant time — the same niche as the design's bitmap-derived exact per-tile counts, far more expensively.

**The fatal structural point:** every system here precomputes over a **fixed, small, declared set of dimensions**, known at build time. A per-user access bitmap is a filter dimension with cardinality equal to the number of users. It cannot be a cube dimension. This is not a gap in an implementation; it is what a data cube *is*.

Costs, from [Falcon's CHI paper](https://www.domoritz.de/papers/2019-Falcon-CHI.pdf):

- **Nanocubes**: "takes up to 6 hours to build an index for a dataset with 210M objects." Memory grows combinatorially with dimensions and resolution.
- **imMens**: precomputes 3- and 4-D tile projections, at correspondingly costly precomputation; brush resolution capped at visible bins.
- **Falcon**: index size linear in the number of views, and **"the size of the data tile is independent of the size of the data"** — cumulative bin counts resolved in O(1) via summed-area tables. Demonstrated at **10M records in-browser**, **180M flights / 1.2B GAIA stars** database-backed. Trade: interaction with **a single active view only**.
- **Hashedcubes** (via [secondary notes](https://jtchen.io/blog/paper-notes-hashedcubes)): ~5.2× memory reduction vs Nanocubes — 9.4 GB where Nanocubes needed 46.4 GB, most queries under 40 ms.

**Per-user filtering: none, anywhere in the lineage.** Falcon's "filtering" is per-*view*, not per-record.

**Borrow:** Falcon's core insight — *index size proportional to the number of pixels/bins, not to the number of rows*. Also its active/passive split as a latency model: pay a one-off cost when the interaction target changes, then serve every frame from a structure sized by the screen.

---

## 5. Mosaic and DuckDB-WASM

[Mosaic](https://idl.cs.washington.edu/files/2024-Mosaic-TVCG.pdf) is a Coordinator mediating declarative SQL between visual clients and DuckDB, with Params and Selections as reactive query predicates, LRU result caching, query consolidation, and **automatic construction of sparse pre-aggregated data cube indexes** whose size "is bound by the number of bins... not the size of the input data." Wire format is Apache Arrow.

**Demonstrated scale — the best-evidenced in this survey.** Static rendering benchmarked to **10M rows**. Interactive updates on flights at **10M / 100M / 1B rows** and the **1.8B-row GAIA catalogue** at samples from 0.1% to 100%. Index construction under 5 seconds for 200M records; ~100ms interactive latency in server configurations. Real published numbers on real datasets.

**What it does differently:** Mosaic's answer to 10⁹ points is **rasterization, not sampling** — density rasters, hex binning, `denseLine`. That is a genuinely different visual product: you get a smooth density field, not k identifiable, hoverable, clickable documents. For a corpus where the point of the map is to *find and open specific documents*, rasterization solves the rendering problem by discarding the thing you wanted.

**Blockers:** (1) **No row-level security, multi-tenancy or access control** — the architecture is "trusted client issues arbitrary SQL to your database". (2) **DuckDB-WASM failed on the 1B-row flights data** and on large GAIA samples due to WebAssembly memory limits. (3) Index construction for 500M+ rows is acknowledged slow.

**Borrow:** the Coordinator pattern — one mediator consolidating overlapping queries, caching by result, prefetching on hover. Also the discipline of expressing every view as a declarative query over shared selection state, which is where you inject the access predicate exactly once rather than in twenty places.

---

## 6. The McInnes / Tutte Institute ecosystem

Supplies the **pipeline** but not the **frontend**.

**Toponymy** ([repo](https://github.com/TutteInstitute/toponymy)) builds a balanced hierarchical layered clustering via `ToponymyClusterer`, produces `topics_per_document` at every layer, and constructs LLM prompts by sampling and summarising cluster contents. Requires the high-dimensional embeddings, the 2D map, and the raw documents. States it is "designed to scale to very large corpora" but is explicitly in beta.

**Sharp implication for label gating:** because prompts are built from *sampled* documents, "every item it was generated from is visible" is a check over a small recorded sample set, not over whole cluster membership. That makes the gate cheap but weaker than it sounds. **Decide deliberately whether to gate on the prompt sample or on cluster membership** — they differ materially, and the sample-based gate leaks less but protects less.

**DataMapPlot** ([docs](https://datamapplot.readthedocs.io/en/latest/interactive_intro.html)) is the closest ready-made frontend, and its label engine is genuinely good: rendered with **deck.gl**, it "avoid[s] having cluster labels overlap, only revealing some cluster labels once sufficiently zoomed in", with **multiple layers of clustering and labelling** at differing resolutions.

But it cannot be the frontend at scale, structurally. No point-count guidance, no benchmarks, no performance section anywhere in the docs. The architecture ([api_interactive](https://datamapplot.readthedocs.io/en/latest/api_interactive.html)) is `inline_data=True` by default, embedding data "compressed and base64 encoded" in the HTML, with `offline_data_path` writing "separate files... served over an http server." **Whole-dataset-shipped-upfront.** No tiling, no LOD, no lazy loading, no server.

**Thisnotthat** ([repo](https://github.com/TutteInstitute/thisnotthat)) is a Bokeh/Panel labelling tool for notebooks. No scale figures; Bokeh's server-round-trip model puts it orders of magnitude below requirement. Not a candidate.

**Verdict:** keep UMAP, HDBSCAN, Toponymy and evoc as the pipeline. **Port DataMapPlot's label placement and progressive-disclosure logic** into the WebGL frontend rather than adopting DataMapPlot itself — it solves a fiddly problem (non-overlapping, zoom-stable label layout over a hierarchy) and is the single most reusable asset in the ecosystem.

---

## 7. Other embedding-visualisation products

*(Delegated survey. Encord, Scale Nucleus, Zeta Alpha and Superb AI not reached.)*

The most striking finding is **attrition**. Three systems that once did roughly this have exited:

- **Arize Phoenix removed its embedding visualisation entirely.** The v13.0.0 [migration guide](https://raw.githubusercontent.com/Arize-ai/phoenix/main/MIGRATION.md): *"Model inferences, dimensions, embeddings, and the pointcloud (UMAP) visualization have been removed from Phoenix, along with their GraphQL and REST APIs."* The project is thriving — it deliberately pivoted to LLM tracing and evals.
- **Lilac was archived on 25 July 2025** ([repo](https://github.com/databricks/lilac/releases)), ~16 months after the Databricks acquisition. Lilac never had an embedding map; its clustering with LLM-generated titles surfaced through histograms and faceted lists.
- **Aquarium Learning** wound down into Notion. Its explorer was the closest commercial analogue to this interaction model.

Survivors:

- **TensorBoard Embedding Projector** has a hard `LIMIT_NUM_POINTS = 100000` and — worse — **returns the first 100,000 rows, not a sample** ([issue #773](https://github.com/tensorflow/tensorboard/issues/773)). The most-used embedding visualiser in the world, and precisely the anti-pattern this design exists to avoid.
- **FiftyOne** (Apache 2.0, very active — 1.19.0, July 2026): v1 Plotly `scattergl` panel emitting one JSON dict per point; unreleased v2 three.js renderer on `develop`. Good renderer, bad data path (columnar Float32 exploded into a JS object array with a materialised hex ObjectId per point). **Team's own working number is 500K.** Its "progressive loading" is linear 100K chunks *in wire order*, so a partial load is a random-looking subset — strictly weaker than a quadtree where a partial load is a valid coarse view. Access control per-dataset only.
- **Renumics Spotlight** (MIT, 1.8.0 April 2026): three.js via react-three-fiber, **no demonstrated point count published anywhere**. No tiling, no auth.
- **Bunka**: d3/SVG, **no WebGL at all**; largest demo ~29k points. Last release May 2024, inactive.
- **Cohere / Voyage** ship nothing. Cohere's [semantic search cookbook](https://docs.cohere.com/page/basic-semantic-search) UMAPs exactly 1,000 rows into Altair. Beware "search 10 million Wikipedia vectors" material — that is retrieval, not rendering.

**The one system worth serious attention that wasn't on the brief: [Apple's Embedding Atlas](https://github.com/apple/embedding-atlas)** (MIT, [arXiv:2505.06386](https://arxiv.org/pdf/2505.06386v2)). The only project publishing real benchmarks on stated hardware: on an M1 Pro at 1600×1600, **up to 4M points at 60fps and 10M+ points at 25fps**, WebGPU with WebGL2 fallback, real-time KDE via a Deriche-approximation compute kernel. Automatic cluster labelling with map-like de-overlapping placement stable under zoom ([arXiv:2504.07285](https://arxiv.org/abs/2504.07285)). Ships as an npm package with React *and* Svelte components plus a Jupyter widget.

Its philosophy is explicitly **brute force**: "no need to pre-sample or pre-render your data." So it beats the design on labelling and single-machine rendering up to ~10⁷, and loses completely above that and on anything requiring server-side filtering. Its label placement is the best available answer to the labelling half, MIT-licensed and benchmarked.

**Row-level access control across this entire category: zero systems.** The granularity ceiling is universally the dataset.

---

## 8. Geospatial vector tiles and per-user authorisation

*(Delegated. MVT spec, open-source tile servers and tippecanoe verified against raw source; the commercial section is summariser-mediated.)*

**The MVT spec offers no seam.** Grepping [vector-tile-spec 2.1](https://raw.githubusercontent.com/mapbox/vector-tile-spec/master/2.1/README.md) for *security, auth, user, permission, access, encrypt* returns **zero hits**. A tile is a byte blob addressed by (z,x,y); all per-user variation must live outside the format.

**Not solved.** Every system degrades to one of three exits:

1. **Bypass the cache.** Tegola's cache middleware refuses to engage on any request carrying a query string — parameterised maps are **never cached at any zoom**, deliberately ([PR #867](https://github.com/go-spatial/tegola/pull/867)).
2. **One artefact per permission class.** [pg_tileserv](https://raw.githubusercontent.com/CrunchyData/pg_tileserv/master/hugo/content/usage/security.md): *"create different users with access to different tables/functions, and run multiple services."* Esri prescribes it too ([Publish hosted tile layers](https://doc.arcgis.com/en/arcgis-online/manage-data/publish-tiles-from-features.htm)) — with the vicious detail that *"once the tile layer is published, you cannot modify or remove the view definition."* Data changes propagate to tiles; **authorisation changes do not.**
3. **Reject rather than filter.** GeoServer admits the failure ([GeoWebCache Configuration](https://docs.geoserver.org/stable/en/user/geowebcache/config.html)): GWC data security is "by default... turned off"; when on there is "limited support for data access limit filters, only with respect to geographic boundaries (all other types of data access limits will be ignored)"; it "will reject requests"; and "this behaviour is different from the regular WMS, which will filter the data before serving it." A CQL read filter is **silently ignored** by the tile cache.

**Two constructive exceptions.** [Supabase's vector-tile pattern](https://supabase.com/blog/postgis-generate-vector-tiles) is architecturally cleanest: an `mvt(z,x,y)` function over PostgREST that, because it is *not* `SECURITY DEFINER`, runs with invoker rights so RLS applies inside the tile function, with identity in the `Authorization` header rather than the URL. But its demonstrated policy is `USING (true)` and it does not discuss caching — an existence proof, not a blessed pattern. And GeoServer `main` now contains `AccessLimitsKeyBuilder`, serialising a user's `AccessLimits` (including ECQL read filters) into a stable cache-key component with `securityTags` for targeted invalidation — **unreleased and undocumented**, but directional evidence that the industry now considers cache-partitioning-by-authorisation the right answer.

**Tippecanoe's nesting mechanism**, verified against `main.cpp` and `tile.cpp`. Each feature gets **one integer, `feature_minzoom`, assigned exactly once globally** during the radix sort/merge phase, via a per-zoom leaky bucket where `ds[i].interval = exp(log(droprate) * (basezoom - i))`. At tile-build time:

```c
double feature_minzoom = sf.feature_minzoom - (bit_reverse(sf.index >> 2) / pow(2, 64));
if (z >= feature_minzoom || sf.dropped == FEATURE_KEPT) { sf.dropped = FEATURE_KEPT; }
```

`feature_minzoom` is a per-feature constant independent of `z`, so `z >= feature_minzoom` is **monotone in z**: a feature kept at z5 is kept at z6 and above by construction. The `bit_reverse(index)` term is a deterministic z-independent dither.

**Important negative:** `--drop-densest-as-needed` is *not* nested by construction, because `sf.gap` depends on which features are co-present in that tile at that zoom. In practice it tends to nest; that is inference, not guarantee. Borrow the scalar, not the gap mechanism.

---

## 9. Verdict

**Can an existing stack replace the frontend and tiling layer? No — but the design should stop treating the whole thing as novel, because two of its four pillars are solved elsewhere and one is solved better.**

**The tiling layer is genuinely re-implementable and should not be conceptually built from scratch.** quadfeather already does one-point-one-tile quadtree Arrow tiling with `ix` indices and sidecar columns; tippecanoe already does provably-nested LOD via a single per-feature scalar. Morton-sort-as-clustered-index is standard practice (the same idea as Z-ORDER in lakehouse table formats and quadkey ordering in tippecanoe), not an invention. **What is novel is the composition:** exact bitmap-derived per-tile counts *plus* post-mask sampling *plus* nesting, all at once. Claim that, not the pieces.

**The frontend cannot be adopted wholesale:**

- **deepscatter** — closest architectural match, blocked by **CC-BY-NC-SA**.
- **Nomic Atlas** — closest *product* match, blocked by dataset-level-only RBAC and being closed hosted SaaS.
- **Mosaic** — best-engineered and best-benchmarked, blocked by having no security model at all, plus rasterization-not-sampling at 10⁹.
- **DataMapPlot** — right labels, wrong architecture.
- **Apple Embedding Atlas** — best renderer and labeller under a usable licence, brute-force by design, caps ~10⁷, no server-side filtering seam.
- Everything else is below 10⁶, dead, or both.

**What would still have to be built:**

1. **The post-mask sampler.** No off-the-shelf component does this. Tippecanoe's multiplier-cluster design is the blueprint but must be reimplemented against bitmaps and row-ID ranges, and its 1/N cliff engineered around for sparse users — a zoom-varying N, or a fallback that does a live ranked scan over the masked row-ID range when a tile's clusters are exhausted.
2. **The access-control layer end to end.** Nothing in the field has row-level ACL on points.
3. **Label visibility gating.** Toponymy gives labels; the gate is yours. Decide explicitly whether you gate on the prompt sample or full cluster membership.
4. **The 10⁹ path.** Nobody has demonstrated 10⁹ *identifiable, filterable, labelled* points. Falcon and Mosaic reach 10⁹ by aggregating to bins; deepscatter reaches it on a static star catalogue. Prove 10⁸ first and treat 10⁹ as a research risk.

**Four things to borrow:**

- **Tippecanoe's `feature_minzoom` scalar** — reduce each point's retention decision to one precomputed z-independent number. Nesting falls out of the representation. The design's hash-derived priority is most of the way there; make sure it is genuinely mask-*independent* so nesting survives masking.
- **Tippecanoe's `--retain-points-multiplier` cluster structure** — over-retain N× into priority-ordered clusters, take the first survivor per cluster after masking, with cross-zoom consistency of the *filtered* choice explicitly engineered.
- **Mosaic's Coordinator** — one mediator doing query consolidation, LRU result caching and hover-triggered prefetch, and one place to inject the access predicate.
- **DataMapPlot's / Apple's label layout** — hierarchical label layers with zoom-gated progressive disclosure and de-overlapping placement stable under zoom.

**One warning worth more than any technique:** Arize deleted its embedding view, Lilac was archived, Aquarium was wound down. Three well-resourced teams built this and exited. The differentiator is not the scatterplot — it is the access control, which is also the only part nobody else has built.

---

## Addendum — how the design's mechanism compares to tippecanoe's

The design (rev 5, §6.2) uses precomputed per-node candidate lists of the top 4k by priority, filtered by the mask at query time, **with a descent fallback**: when fewer than k survive, merge the four children's candidate lists, recursing until enough survive, terminating in a direct scan of the masked row-ID range at the deepest stored level.

That recovery path is what tippecanoe lacks, and it is why the 1/N cliff does not apply. Tippecanoe's multiplier clusters are a fixed-width structure with no route back when exhausted; the design's candidate lists are an optimisation over an exact definition, and the exact definition is always reachable.

**Correction (2026-07-27).** This addendum originally claimed the cliff was replaced by "a gradual cost increase proportional to log(1/coverage)". That conflates depth with work: descent *depth* is logarithmic, but each level multiplies the node count by four, so nodes visited is the geometric sum and the cost is proportional to **1/coverage**. At 0.1% coverage that is ~341 nodes per tile, not a handful, and at 0.01% it is ~5,461. Subsequent modelling established that direct evaluation from the mask — take the visible row IDs in the tile range, read their priorities, keep the *k* lowest — is bounded by the tile's priority block and gets *cheaper* as coverage falls, crossing descent at roughly 5% coverage. The design now selects between the two per tile (design doc r14, §7.2). The structural point against tippecanoe stands and is if anything stronger: the exact definition is always reachable, and at low coverage it is also the cheap one.

The distinction is worth preserving explicitly, because the obvious "optimisation" of dropping the exact path reintroduces exactly tippecanoe's failure mode.
