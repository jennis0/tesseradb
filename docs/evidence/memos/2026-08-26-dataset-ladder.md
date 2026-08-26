# A dataset ladder above arXiv — what is available, 2026-08-26

**Status:** Evidence — survey, never normative. Every figure is **as fetched from its source on
2026-08-26** unless marked *unverified* (a page that refused the fetch, or a figure a survey agent
carried from memory). Nothing here is adopted by being listed; adoption is per-dataset work with
its own probe. **Reads with** [`2026-08-07-external-dataset-candidates.md`](2026-08-07-external-dataset-candidates.md),
which surveyed for the *measurement* fixture gaps; this memo surveys for a **ladder of real
corpora for testing and demonstration**, rung by rung, chosen so that between them they exercise
the artifact catalogue ([`2026-08-15-artifact-configurations.md`](2026-08-15-artifact-configurations.md)'s
eight types) and the record surface (categories, timestamps, keywords, text).

---

## 1. The ladder

Two ladders, because the two kinds of corpus differ in what they cost. An **embedding** corpus
needs a projection before it is a map, and above ~10M that is real work on this box (a 10 GB
GPU does not hold the kNN graph; the 2.4M build already does PCA on the CPU). A **geographic**
corpus skips projection — quantise, Morton-rank, done — which is why it reaches 10⁹ cheaply and
why it is the one that can carry the boundary layers.

| Rung | Embedding corpus | Geographic corpus | What the rung adds |
|---|---|---|---|
| 2.4M *(have)* | arXiv | — | clusters, labels, categories, prose, dates |
| ~10M | — | **GeoNames** 12M (401 MB) | first real map; feature class→code (9→645) and admin1–4; polygons from Natural Earth / geoBoundaries |
| ~40M | **PubMed + MedCPT** ~36M of 40.45M (147 GB, keyless) | **OS Open UPRN + EPC** 42M / ~30M (UK, 1 m points) | a real deep taxonomy (MeSH) as a **tiered** layer; the ONS OA→LSOA→MSOA→LAD boundary ladder |
| ~100M | **OpenAlex + PaperSeek embeddings** 102M (240 GB, CC0, keyless) — or SPECTER2 120M (~840 GB gz; key) | **Overture Places** 73.6M + **division_area** 1.07M polygons | scholarly labels with hierarchy; a global boundary layer from the same publisher as the points |
| ~250M | **TreeOfLife-200M-Embeddings** 233M (346 GB fp16, CC0) — or Cohere Wikipedia 247M (536 GB), or Commons 147M *(no vectors)* | **OpenAddresses** 578M (12.6 GB, licence-split) | 7-level taxonomy over real vectors; **the same items also have coordinates** (§3) |
| ~1B | DataComp-1B 1.39B (8.9 TB) — flat labels | **Overture buildings** 2.5B centroids / addresses 473M | scale only; the embedding side is thin here |
| 3B+ | *nothing open ships vectors at this scale* (§4) | **GBIF** 3.5B (parquet on S3) | the real top rung: taxonomy, publisher, dates, and a **real per-record licence compartment** |

Recommended order of adoption, by value per unit of plumbing: **GeoNames → PubMed/MedCPT →
Overture → OpenAlex/PaperSeek → TreeOfLife-200M → GBIF**. OpenAddresses is a cheap 0.5B stress
(12.6 GB) to take whenever the write path or the density underlay wants one.

## 2. Coverage of the artifact catalogue

| Artifact type (08-15 memo §) | Exercised by |
|---|---|
| Nested clustering, HDBSCAN (§2) + its labels (§3) | every embedding rung; arXiv already |
| Tiered boundaries with zoom-ranged levels (§4) | ONS OA/LSOA/MSOA/LAD over UPRN+EPC; TIGER state→block over NAD/OpenAddresses-US; Overture `division_area` (country→…→neighbourhood) over Places; geoBoundaries ADM0–2 globally |
| Tiered **taxonomy** as a levelled layer (`tiered`) | MeSH over PubMed (depth 10+); GBIF/TreeOfLife kingdom→species (7); CPC over patents (5 — HUPD 4.5M, no vectors) |
| Tagged programme by attribute predicate (§5) | Overture `sources` / `basic_category`; GBIF `datasetkey` / `publishingcountry`; OpenAlex institution |
| Fitted circles, supplied corpus-derived content (§6) | any clustering rung — k-means over PubMed or TreeOfLife |
| Analyst selections (§7) | any rung |
| Restricted analytic — layer gate + `min_fraction` (§8) | best where the **mask is real**: GBIF licence, Europe PMC / OpenAlex licence facet, Commons per-file licence, HathiTrust rights code, Zenodo `access_status` (§6) |
| Density cells (§9) | every rung; the geographic ones show real clumping rather than UMAP blobs |
| Records — categories, timestamps, keywords, text | PubMed (MeSH, year, journal, abstracts 43 GB); GBIF (Darwin Core tail, null-heavy); GeoNames (names, alternate names); Overture (names, categories) |
| Views / multi-table | Cohere Wikipedia: language × article; **TreeOfLife ⋈ GBIF: one item in two geometries** (§3) |
| Churn, time-ordered ingest | GBIF monthly snapshots; Overture monthly releases; Zenodo embargo dates flipping open (a natural *unsuppress* stream); Wikipedia revisions (08-07 memo §5) |

Text at scale has a build ceiling: `probes/build_demo_datasets.py` records the attribute pass
OOM-killed at 250M with titles, so prose above ~25M waits on the text column streaming rather
than on data. PubMed abstracts at 36M (~35 GB of strings) sit past that line; titles do not.

## 3. Findings worth the owner's attention

- **TreeOfLife-200M-Embeddings is the strongest new find.** 233,055,986 images, BioCLIP-2
  768-d fp16 vectors in parquet (~346 GB), sorted by taxonomy, CC0, ungated, full seven-rank
  hierarchy plus publisher and `basisOfRecord`. Its sources are GBIF and EOL occurrences, so a
  large fraction should join back to a GBIF record **with coordinates** — the same entity in an
  embedding view and a geographic view. *Join rate unverified*; it is the probe to run first.
- **LAION-5B in 2026 is metadata-only.** Re-LAION-5B (5.53B pairs, gated HF parquet, ~481 GB for
  the 2B-en research split) ships **no embeddings and no kNN index** (open HF discussion); the
  original ~9 TB of ViT-L/14 vectors were withdrawn with the December-2023 takedown, and the
  `deploy.laion.ai` URLs the ClickHouse docs still cite are *unverified* as resolving. A 5B rung
  with vectors means CLIP inference over billions of images — not a download. **DataComp-1B is
  the honest substitute** (1.39B, ViT-B/32 + L/14 features, 8.86 TB, ungated, CC BY 4.0), and
  CommonPool-medium (128M, ~780 GB) its 100M rung; both are label-thin (url, caption,
  similarity), so the policy side is synthetic over real geometry. The 08-07 memo's "LAION as
  reserve" line should be read with this.
- **OpenAlex ships no embeddings itself** (413M exist internally; not in the snapshot), but two
  **third-party CC0 releases on HF** do: PaperSeek 102M works (~240 GB) and `colonelwatch`
  abstracts at 1024-d (~816 GB) — *neither inspected beyond the card*. With OpenAlex's own 4-level
  topics (4 domains → 26 fields → 252 subfields → 4,516 topics), per-work licence (CC-BY on
  29.2M) and OA status, institutions with country and lineage, this is a keyless ~100M scholarly
  rung that carries hierarchy **and** a real compartment, and it displaces SPECTER2 unless
  SPECTER's vectors are wanted specifically.
- **SPECTER2 is heavy in its published form**: 30 × 28 GB gzipped JSON with vectors as JSON
  arrays; ~184 GB at fp16 once decoded. Free key by request form.
- **Overture is the boundary story in one publisher.** Release 2026-08-19: Places 73,631,092,
  buildings 2,529,582,613, addresses 472,797,160, divisions 4,658,700 with 1,074,177
  `division_area` polygons. Places' `categories` is deprecated for `basic_category` + `taxonomy`
  in the September 2026 release — adopt after the schema break, not before.
- **Overture buildings at 2.5B** are the geographic 10⁹ rung nobody needs to build: centroid,
  height, floors, roof shape, source. Polygons themselves are not points, but a centroid is.

## 4. Embedding corpora — the table

| Dataset | Items | Vectors | Labels | Licence / access | Size | Fit |
|---|---|---|---|---|---|---|
| PubMed + MedCPT (NCBI) | ~36M with vectors *(inferred from chunk list)* of 40,451,610 in the 2026 baseline (51.8 GB gz) | MedCPT 768-d npy, 38 chunks | MeSH: 30,594 descriptors, 64,687 tree numbers, 16 branches, **depth 13**; pub types, journal, year — join baseline XML by PMID | embeddings on NCBI FTP, no key; *licence text not found* | 104 GB npy + 43 GB JSON text | **best 10–40M rung** |
| Europe PMC (REST, live) | 48,759,604 records; 8,053,623 OA; 12.1M full text | none | MeSH + **licence facet: cc-by 6,379,418 / cc-by-nc 1,119,029 / cc-by-nc-nd 1,577,914 / cc0 69,245** | per article; weekly bulk OA XML | *not stated* | the PubMed rung's real compartment, by PMID join |
| OpenAlex + PaperSeek / colonelwatch embeddings | 102M / *(count not read)* | third-party, CC0 on HF; 1024-d for the latter | OpenAlex's topics, licence, OA, institutions | CC0, no key | ~240 GB / ~816 GB | **~100M rung** |
| Semantic Scholar SPECTER2 (release 2026-08-18) | papers 200M, abstracts 100M, **embeddings 120M** | 768-d, JSON in gz JSONL | `s2FieldsOfStudy` (flat ~23), DOI/arXiv/PMID, year, venue, OA flag | ODC-BY *(README needs key)*; free key | ~840 GB gz; ~184 GB fp16 decoded | **50–120M rung**, join OpenAlex for depth |
| OpenAlex works (June 2026) | 510M in snapshot, 322M default filter | none | topics 4 levels, institutions (ROR, country, lineage), year, language, OA, abstract inverted index | CC0, anonymous S3, AWS pays egress | ~670 GB jsonl / part of 780 GB parquet | labels only |
| Wikimedia Commons | 146,577,907 files | none (wit_base: 6.4M, ResNet-50 2048-d, 308 GB) | **per-file licence**: CC-BY-SA-4.0 34.8M, PD-Mark 16.6M, CC-BY-SA-2.0 13.8M, CC0 9.8M, CC-BY-4.0 8.3M … | per file; dumps | pages-articles 118 GB bz2 | real compartment at 147M, no vectors |
| Amazon Reviews 2023 | 571.5M reviews / 48.2M items | none | 33 categories + per-item hierarchical `categories` path (depth 2–5 *unverified*), timestamps to the second | *not stated* | 750 GB | licensed category tree at 0.5B; no audience |
| Cohere wikipedia-2023-11 | 247,154,006 paragraphs; en 41,488,110 | embed-multilingual-v3 1024-d f32 | language, title, url — categories by title join only | *not on card*; ungated HF | 536 GB; en 90 GB | 40M / 250M with weak native labels; language = view |
| TreeOfLife-200M-Embeddings | 233,055,986 | BioCLIP 2 768-d fp16 | 7-rank taxonomy, common name, source, publisher, basisOfRecord | CC0, ungated | ~346 GB parquet | **best 200M rung** |
| FineWeb2-HQ | 380M docs, 20 langs, no English | XLM-R 768-d per chunk | domain, date, language, quality score | ODC-BY | 6.1 TB | scale, thin labels |
| DataComp-1B / CommonPool | 1.39B / 128M / 1.28B | CLIP B/32 + L/14 npz | url, caption, similarity, face boxes | CC BY 4.0, ungated | 8.86 TB / ~780 GB / ~7.8 TB | 1B with vectors; flat |
| Re-LAION-5B | 5.53B | **none** | caption, similarity, watermark, unsafe, lang | CC BY 4.0, gated | 481 GB (2B-en) | end of ladder, no vectors |
| HUPD patents | 4.5M | none | CPC/IPC 5-level, decision, dates, examiner, full text | CC BY-SA | 125 GB | hierarchy without vectors; sub-rung |
| Google Patents BigQuery `embedding_v1` | ~100M | 64-d *(existence unverified)* | CPC tree, assignee, country | *unverified* | ~26 GB vectors | verify before planning |
| Stack Exchange dump | ~60M posts *(unverified)* | none | tags, site, score, dates | CC BY-SA + no-LLM click-through; archive.org mirror 2025-12-31 | 7z XML | labelled, DIY vectors |
| Hacker News | ~40M; a 28M-comment embedding release advertised Nov 2025 *(unfetched)* | *unverified* | type, author, time, score | BigQuery public | ~43 GB if real | one manual check |
| MS MARCO v2, MIRACL, BIGPATENT, YFCC100M (no full CLIP release), Reddit torrents | — | — | — | — | — | declined: no vectors, no labels, or legally grey |

## 5. Geographic corpora — the table

| Dataset | Items | Labels | Licence / access | Size | Fit |
|---|---|---|---|---|---|
| GeoNames | >12M features (4.8M populated places) | class (9) → code (645), admin1–4, country, population, modification date, 16M alternate names | CC BY 4.0, direct zip | 401 MB | **first geo rung** |
| Wikidata P625 | 12,402,646 coordinate statements (live SPARQL) | P31/P279 class hierarchy, P131 admin containment, multilingual labels | CC0 | dump 103 GB bz2 → filter | richer labels than GeoNames, dump-filter cost |
| OS Open UPRN | 41,629,393 (Aug 2026) | none — join key | OGL v3 | ~1.5 GB *(unverified)* | UK 1 m points; labels by join |
| UK EPC | ~30M *(unverified)* | rating A–G, property type, age band, LA, lodgement date | OGL with sign-in | ~5.6 GB | the UK label donor via UPRN |
| HMLR Price Paid | 27.45M | property type, tenure, town→district→county, date, price | OGL; address fields restricted | 5.3 GB | postcode centroids only (~1.7M distinct) |
| US National Address Database | ~80M *(unverified)* | state→county→place, address type | CC0 | 7.6 GB zip | 50–100M US rung |
| Overture Places | 73,631,092 | `taxonomy` path — 2,117 categories, 14 roots, depth ≤6; `basic_category`; brand; confidence; operating status; `sources.dataset`; names | CDLA-Permissive 2.0 (Meta ~58M) / Apache (Foursquare) / CC0 (AllThePlaces); anonymous S3 GeoParquet | **10.4 GB, 16 files** (measured) | **75M rung + division_area boundaries** |
| OpenAddresses | 578,632,464 (2021 static archive; current *unverified*) | number/street/city/district/region/postcode; **licence split attribution vs share-alike** | per-source | 12 GB + 640 MB | **cheap 0.5B** with a real licence compartment |
| Overture addresses / buildings | 472,797,160 / 2,529,582,613 | addresses: country, postcode, `address_levels[]` (US 125.8M, BR 89.9M); buildings: subtype, class, height, floors, roof, source — mostly null outside OSM | addresses per-source permissive; buildings ODbL | **21.8 GB / 276.8 GB** (measured) | 0.5B / 2.5B |
| Overture divisions | 4,658,700 points; 1,074,177 `division_area` polygons | `subtype` 12 levels country→…→microhood, `hierarchies[]`, population, names | ODbL | 0.5 / 4.4 GB | **the global tiered boundary layer** |
| OSM planet | 10.8B nodes, of which **~294M tagged** (taginfo 2026-08-25) | tags (amenity/shop/…, flat), names, timestamps in history | ODbL | 88 GB PBF; history 150 GB | tagged nodes = a 0.3B rung; all nodes = the only open source past 10⁹ points, unlabelled |
| Google Open Buildings v3 / Microsoft footprints / VIDA combined | 1.8B / 1,409,848,189 / **2,705,459,584** deduped | confidence, height (MS), source (VIDA 3-way), country partition; no semantics | CC BY or ODbL / CDLA-Permissive 2.0 / ODbL | MS 113.9 GB csv.gz; VIDA GeoParquet by country *(total unstated)* | 1–3B centroids, thin labels |
| Who's On First | ~26M (>20M venues, 2015-era) | 20-level placetype tree, `wof:hierarchy[]`, multilingual names | WOF licence (CC-BY-style) | *sizes unverified* | ~5M admin records as a deep gazetteer |
| GBIF occurrence (2026-06-01) | 3,499,090,951 | 7-rank taxonomy + taxonkey chain, datasetkey, publisher country, basisOfRecord, eventdate, **per-record licence CC0/BY/NC**, issue[], Darwin Core tail | per-record; S3 parquet 5 regions, monthly | *hundreds of GB, unmeasured* | **top rung** |
| iNaturalist Open Data, NYC TLC, GDELT, YFCC geo, AIS, ADS-B | *not surveyed — fetches refused or budget exhausted* | | | | |
| FIRMS VIIRS / MODIS | order 10⁸ *(no official total)* | confidence (3-way), day/night, FRP, timestamp | open, cite NASA | extract by request | 10⁸ with timestamps; acquisition friction |
| GHCN-Daily / ISD | ~2B values / multi-B obs | element, flags, country | CC0 / open | ~100s GB | rows stack on 10⁵ stations — wrong shape for a map |
| GOES GLM lightning | billions/yr | energy, area, flags | open netCDF 20 s files | decode project | only open 10⁹ natural-event source; label-poor |
| USGS ComCat | ~4–5M *(503'd)* | magnitude, type, network, free-text place | PD | scripted windows | ~10M rung with free text |

**Boundaries** (all verified except where marked): ONS Census 2021 — OA 188,880 / LSOA 35,672 /
MSOA 7,264 / LAD 361, exact-fit, OGL with OS attribution, generalised OA set 277 MB; Scotland DZ
7,392 / IZ 1,334; NI DZ 3,780 / SDZ 850 *(licence unverified)*; **TIGER 2025** state 56 → county
3,234 → tract 85,528 → block group 242,747 → block 8,180,866 (public domain); **GISCO NUTS 2024**
92 / 244 / 1,165 + LAU — EuroGeographics non-commercial clause; **geoBoundaries 6.0** ADM0–5 per
country, CC BY 4.0, CGAZ composite ADM0–2; **Natural Earth** admin-0 258 / admin-1 ~4,500,
public domain, 19 MB; **GADM 4.1** 400,276 areas, non-commercial, no redistribution — evaluation
only; Overture `division_area` 1,074,177. Grids: H3 r6 = 14.1M cells, r8 = 692M; S2 L12 = 100.7M.

## 6. Real access-shaped structure — the table

The 08-07 memo's negative result stands: **no large real enterprise permission corpus is
public.** What exists is per-item *rights* structure, which is a real compartment even if not
an observed grant:

| Dataset | Items | The real per-item field | Skew | Fit |
|---|---|---|---|---|
| GBIF | 3.5B | `license` CC0 / CC-BY / CC-BY-NC | *proportions unverified* | the top rung's own mask |
| Zenodo | 7,180,358 (API, live) | `access_status`: open 6,927,298 / restricted 242,055 / embargoed 11,003 | 3.4% masked; embargo dates flip open over time | API-only; the natural unsuppress stream |
| HathiTrust | 17.5M volumes | rights code (pd, pdus, ic, cc-*, und…) → full view vs search-only, **and viewer-geography dependent** (~40% full view in US) | | the closest public analogue to a principal-dependent mask; no text for the in-copyright 60% |
| Europeana / DPLA | ~53M / ~50M *(2026 counts unverified)* | `edm:rights` from 14 statements; provider → aggregator → country | CC0 ~20M, PD ~9M, CC ~8M (older figures) | licence compartments with a real provider hierarchy; thin text |
| Crossref 2026 | 180,034,490 | `license` URL per work — coverage *unmeasured* | | 10⁸ scholarly metadata; measure coverage before treating as a compartment |
| OpenAddresses | 578M | attribution vs share-alike, split at source | | licence as compartment at 0.5B |
| Bluesky | 2.43B posts, ~3.9M/day | **moderation labels, mute/moderation lists and blocks are public first-class records**; deletes are explicit firehose events | | the closest public analogue to this system's own model; no official bulk dump, ToS discourages scraping, ethics/PR exposure |
| Reddit torrents | 2005→2024-12, 3.12 TB zst | subreddit as compartment; `over_18`, `quarantine`, `removed_by_category`, `[deleted]` | | strongest natural compartments + removals; licence hostile |
| Wikipedia | 7.23M articles, 1.367B revisions | none per item; category DAG (~2.6M, cycles, depth ~35 tree-ified) | ~164k edits/day enwiki, EventStreams live | the safe **churn** stream: real edits, deletes and revdel |
| Launchpad / Bugzilla private bugs | 0.9M / ? | genuine reader sets — but the private items are not downloadable | ~10% private | structure real, masked half absent by construction |
| Calibration sets (08-07 memo §4) | 10²–10⁴ | observed grants | | calibration, never fixtures |

Not surveyed (delegated surveys did not return): Enron/mailing lists, GitHub Archive/Software
Heritage, CPC/USPTO, CourtListener, declassified-record catalogues (CREST, TNA closure status,
NARA access restriction). PMC OA's three licence buckets exist on S3 but per-bucket counts were
not obtained; Europe PMC's facet above stands in. Also seen and declined: NYT Annotated (no longer
available from LDC), GDELT GKG (no text, flat themes), Open Food Facts 4.7M (good DAG, thin
text), MusicBrainz/Discogs/IMDb/Open Library (flat facets), Mastodon (bulk collection closed).

## 7. Practicalities

**Disk.** From WSL2 today there is **no NFS or CIFS mount** — only the three drvfs 9p drives
(C: 54 GB free, D: 745 GB, E: 115 GB) and 126 GB free on the root ext4. The NFS mount the ladder
is meant to stage on is not currently reachable from this environment; mounting it (or a
Windows-side mount surfaced through drvfs) is the first practical step, and its throughput should
be measured once from inside WSL2 before any staging time is planned (08-07 memo §7). Staging
sizes for the recommended order, raw: GeoNames 0.4 GB · PubMed/MedCPT 147 GB · Overture Places
+ divisions 15 GB · TreeOfLife 346 GB · GBIF column-pruned extract order 100
GB · SPECTER2 840 GB gz (decode to fp16 and discard) — **≈1.5 TB of NFS** without SPECTER2's gz,
and DataComp-1B's 8.9 TB is out of reach on any of it. The 08-07 memo's two rules stand: never
serve or mmap a bundle from NFS, and a build figure names its source medium.

**Projection.** Every embedding rung above PubMed needs a projection strategy that is not the
current script: sample-fit UMAP and transform the rest, parametric UMAP, or PCA-2 as a first
map to get the rung serving before a better geometry exists. The geometry is hashed, not seeded,
so each rung's projection is a build-once artifact worth archiving on the NFS tier.

**Text.** Titles at 36M are fine; abstracts at 36M are not until the text column streams
(§2). PubMed's 43 GB of JSON text is the case that would force that work.

## Links

Embeddings: [MedCPT FTP](https://ftp.ncbi.nlm.nih.gov/pub/lu/MedCPT/pubmed_embeddings/) ·
[S2 release listing](https://api.semanticscholar.org/datasets/v1/release/latest) ·
[OpenAlex snapshot](https://help.openalex.org/download/snapshot-format) ·
[OpenAlex topics](https://api.openalex.org/topics?per-page=1) ·
[Cohere wikipedia-2023-11](https://huggingface.co/datasets/CohereLabs/wikipedia-2023-11-embed-multilingual-v3) ·
[TreeOfLife-200M-Embeddings](https://huggingface.co/datasets/imageomics/TreeOfLife-200M-Embeddings) ·
[FineWeb2-HQ](https://huggingface.co/datasets/epfml/FineWeb2-HQ) ·
[DataComp README](https://github.com/mlfoundations/datacomp/blob/main/README.md) ·
[datacomp_1b](https://huggingface.co/datasets/mlfoundations/datacomp_1b) ·
[Re-LAION-5B](https://laion.ai/blog/relaion-5b/) ·
[Re-LAION no-embeddings discussion](https://huggingface.co/datasets/laion/relaion2B-en-research/discussions/3) ·
[HUPD](https://huggingface.co/datasets/HUPD/hupd) ·
[Stack Exchange 2025-12-31 mirror](https://archive.org/details/stackexchange_20251231).
Geographic: [GeoNames dump](https://download.geonames.org/export/dump/) ·
[Wikidata dumps](https://dumps.wikimedia.org/wikidatawiki/entities/) ·
[OS Open UPRN Aug 2026](https://docs.os.uk/os-downloads/products/addresses-and-names-portfolio/os-open-uprn/release-notes/august-2026.md) ·
[EPC](https://get-energy-performance-data.communities.gov.uk/) ·
[Price Paid](https://www.gov.uk/government/statistical-data-sets/price-paid-data-downloads) ·
[NAD](https://catalog.data.gov/dataset/national-address-database-nad-text-file) ·
[Overture 2026-08-19 release](https://docs.overturemaps.org/blog/2026/08/19/release-notes/) ·
[Overture categories](https://raw.githubusercontent.com/OvertureMaps/schema/main/docs/schema/concepts/by-theme/places/overture_categories.csv) ·
[taginfo statistics](https://taginfo.openstreetmap.org/reports/database_statistics) ·
[VIDA combined buildings](https://source.coop/vida/google-microsoft-osm-open-buildings) ·
[Microsoft footprints](https://github.com/microsoft/GlobalMLBuildingFootprints) ·
[OpenAddresses archive](https://results.openaddresses.io/) ·
[GBIF on AWS](https://github.com/gbif/occurrence/blob/master/aws-public-data.md) ·
[FIRMS](https://firms.modaps.eosdis.nasa.gov/download/) · [GHCN-D](https://registry.opendata.aws/noaa-ghcn/).
Boundaries: [ONS Open Geography](https://geoportal.statistics.gov.uk/) ·
[ONS licences](https://www.ons.gov.uk/methodology/geography/licences) ·
[TIGER 2025](https://www.census.gov/geographies/mapping-files/time-series/geo/tiger-line-file.2025.html) ·
[Census tallies](https://www.census.gov/geographies/reference-files/time-series/geo/tallies.html) ·
[GISCO NUTS](https://ec.europa.eu/eurostat/web/gisco/geodata/statistical-units/territorial-units-statistics) ·
[geoBoundaries](https://github.com/wmgeolab/geoBoundaries) ·
[Natural Earth](https://www.naturalearthdata.com/) · [GADM licence](https://gadm.org/license.html) ·
[H3 tables](https://h3geo.org/docs/core-library/restable/) · [S2 statistics](https://s2geometry.io/resources/s2cell_statistics.html).
Access-shaped: [Zenodo API](https://zenodo.org/api/records?size=1) ·
[HathiTrust rights](https://www.hathitrust.org/the-collection/preservation/rights-database/) ·
[Crossref 2026 file](https://www.crossref.org/blog/2026-public-data-file-now-available/) ·
[Europeana rights](https://pro.europeana.eu/page/available-rights-statements) ·
[Europe PMC REST](https://www.ebi.ac.uk/europepmc/webservices/rest/search) ·
[PaperSeek OpenAlex embeddings](https://huggingface.co/datasets/Grozkal/PaperSeek-OpenAlex-Embeddings) ·
[colonelwatch abstracts-embeddings](https://huggingface.co/datasets/colonelwatch/abstracts-embeddings) ·
[Commons dumps](https://dumps.wikimedia.org/commonswiki/latest/) ·
[Amazon Reviews 2023](https://huggingface.co/datasets/McAuley-Lab/Amazon-Reviews-2023) ·
[AT Proto labels](https://atproto.com/specs/label) ·
[DPLA bulk](https://pro.dp.la/developers/bulk-download).
