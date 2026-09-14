# GBIF occurrence — the ladder's largest rung, and the first to spend most of one entity space

**3,654,488,638 records of what was found where**, published by GBIF as 8,369 parquet parts,
258.5 GiB compressed over 50 columns. One Web Mercator view, a three-level tiered taxonomy over
family → genus → species, four attributes — one per type — and **the country the occurrence was
recorded in as the compartment**.

It is a **demonstrator and a speed benchmark** (owner ruling, 2026-09-01). What the rung measures
is the row count. 3.65×10⁹ rows is 85.1% of one `u32` entity space and artifact ids allocate
downward into the same space ([`sharding.md`](../../docs/design/sharding.md)), so this is the first
rung whose entity space is nearly spent and the first where the question "does it need sharding"
had to be answered before anything was built. It does not
([`probes/2026-09-09-gbif-census/`](../../probes/2026-09-09-gbif-census/README.md)).

**Three things make this rung different from the five below it.**

1. **15.5× rung 5's rows through a schema a fifth as wide.** Rung 4 measured what 118.9 GB of
   abstracts do to a build and rung 5 measured seven tiered levels over 2.33×10⁸ rows. This rung
   holds four attributes, one rendered byte per row, and no text index.
2. **The taxonomy starts at family, not at kingdom.** That is a concession to a build bound rather
   than a fact about taxonomy, and §"Why family" says what the bound is.
3. **A record with no coordinate is dropped rather than carried.** There is one view, so a
   positionless entity could be drawn nowhere and would still spend an entity id.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# a fraction that finishes, into $TESSERA_LADDER/gbif-64p
python3 -m test_corpora.gbif.prepare --parts 64 --spread

# the whole corpus, into $TESSERA_LADDER/gbif
python3 -m test_corpora.gbif.prepare

cd "$TESSERA_LADDER/gbif" && tessera check --payloads && tessera build --stage-timings
./run_demo.sh --deployment "$TESSERA_LADDER/gbif/tessera.toml" \
  --terms-file "$TESSERA_LADDER/gbif/country-terms.txt" \
  --ranks "$TESSERA_LADDER/gbif/country-ranks.json"
```

The pipeline is `sources.py`, `prepare.py` and `corpus.toml`, and it runs on the system `python3`:
pyarrow and numpy, no GPU, no embedding, no layout. The share is read **once** and everything after
that is local.

## Why family

`merge_member_runs` (`crates/tessera-build/src/layers.rs`) holds the largest single artifact's
members resident as `u64` while it sorts them, so a level-0 artifact sets the `layers` stage's
peak. Extrapolated from the census's 200-part sample:

| level | largest artifact | members | resident |
|---|---|---|---|
| kingdom | Animalia | 2,809,414,577 | **22.5 GB** |
| phylum | Chordata | 2,313,453,660 | 18.5 GB |
| class | Aves | 2,138,854,540 | 17.1 GB |
| family | Anatidae | 177,354,694 | **1.4 GB** |
| species | *Zenaida macroura* | 28,173,879 | 0.2 GB |

A taxonomy starting at kingdom needs half of a 47 GB box for one artifact. **Owner ruling,
2026-09-09:** rung 6 starts its tiered layer at family, `kingdom` rides as a rendered category
column instead, and the unbounded `layers` peak is a later pass. This is the third instance of the
shape the text index and, from 2026-09-08, the keyword dictionary both resolved by
chunk-spill-merge.

`kingdom` as a rendered category is not a consolation. Nine values over three and a half billion
points is the iconic view of this corpus and it costs one byte in every row.

## A record with no coordinate is dropped

`prepare.py` drops a row whose `decimallatitude` or `decimallongitude` is null, not finite, or
outside ±90/±180, before it assigns an entity id. With one view there is nowhere for such a row to
be drawn, counted in a tile or sampled from, and it would still spend an entity id out of a space
that is 85.1% used. Rung 5 carried its unplaceable rows because it has a second view they appear
in; this rung has no second view.

**Measured, 64 parts evenly spaced: 96.95% of rows carry a coordinate.** 814,437 of 26,660,444 do
not. Over a *prefix* of 64 parts the same figure is 72.19%, which is the reason `--spread` exists
and the reason a coverage fraction should be read off a spread selection. The part order on the
share is the publisher's export order, not a shuffle.

**A record beyond ±85.0511° latitude is kept and clamps to the extent boundary.** Web Mercator has
no position for it. 11 of 25,846,007 in the spread sample, 74 of 17,721,800 in the prefix; the
build reports them as `CLIPPED` and builds anyway. They are real polar observations and dropping
them would be a rendering decision taken in a pipeline.

## The compartment is real, and every row carries a term

`countrycode`, 251 values on the whole corpus. A record whose country is null or empty carries
`UNRECORDED`, written into the column by `prepare.py` rather than left to the view's `default`, so
the access column is never empty and **a principal holding no term sees nothing**. Rung 5's
`unpublished` and rung 4's `unlicensed` have the same shape and exist for the same reason: the
campaign's principal ladder starts at 1% of the corpus and cannot be composed under a floor every
principal holds for free.

`UNRECORDED` is 0.18% of the placed rows (47,565 of 25,846,007, spread sample) against 1.57% of all
rows in the census. Records with no country largely have no coordinate either, so dropping the
unplaceable rows removes most of them.

`country-ranks.json` is `[{term, pairs}]` most-covering first, which `run_demo.sh --ranks` and
`serve_battery.py` compose their 1–100% principals from. `country-terms.txt` is the candidate list
**one term per line**, which `run_demo.sh --terms-file` reads. A country code carries no comma, so
the comma-joined form the earlier rungs write would work here; one term per line is the form that
works whatever the key holds.

## The schema is one column per type, and the sparseness is the declaration

Owner ruling, 2026-09-09. Distinct and null figures are the census's, over its 200-part sample.

| type | column | distinct | null | placement |
|---|---|---|---|---|
| category | `kingdom` | 9 | 0.0% | `render = true` — the legend and the colour |
| keyword | `specieskey` | ~10⁶ | 11% (as `species`) | `index = true` — a genuine lookup |
| numeric | `year` | 377 | 3.7% | `index = true` — "since 2020" is what a map of this is asked |
| utf8 | `scientificname` | — | 0.0% | neither flag: blob-resident, returned on drill-down |

⊘ **`gbifid` and `occurrenceid` are not taken.** Both are unique per row at 3.65×10⁹, which is a
~100 GB keyword dictionary and exactly the pathology
[`probes/2026-09-08-keyword-spill/`](../../probes/2026-09-08-keyword-spill/README.md) was written
about.

⊘ **`locality` is not taken, and this is the hook for it.** It is 48.45 GiB compressed, the
corpus's largest column at 14.2 B/row, and a text index over it is deferred past the first build.
Adding it is three edits and one measurement: put `"locality"` in `sources.COLUMNS`, add the field
to `POINTS_SCHEMA`, and declare `[[attribute]] name = "locality", type = "text", index = true` in
`corpus.toml`. What is unmeasured is what the `text_index` stage costs over 3.5×10⁹ short strings;
rung 4's 118.9 GB of abstracts is the nearest figure and its documents are an order of magnitude
longer. It is not in the config because a build that ran out of memory in `text_index` would cost
the whole pass.

`basisofrecord` (9 values, `HUMAN_OBSERVATION` 2.83×10⁹ against `PRESERVED_SPECIMEN` 380M) is the
best category the schema does *not* take. One column per type is the ruling.

## The taxonomy layer's keys

A member row is one occurrence with a three-entry list whose positions are the declared levels,
which is GeoNames' `members-admin.parquet` form. Level *j*'s key is the first *j* + 1 ranks joined
with `|`, so a species key names its whole lineage: `Cichlidae|Crenicichla|Crenicichla minuano`.

**A hole in the chain becomes an artifact, not a null.** A row with a species and no family carries
an explicit `NOT_RECORDED` key at the family level, because `parent_edges` is `windows(2)` and does
not read past a gap; a null there would state a containment no row makes. A level is filled only
where something *below* it is present, so a chain that simply ends keeps its remaining levels null.
Both markers, and `UNRECORDED`, are checked against the corpus's own values and a collision is a
refusal: a placeholder that merged with a real clade or with a real country would move records
between artifacts or between compartments with no error.

⊘ **A family key is a bare family name, so two families of one name would merge.** Measured over
the spread sample: the only level-0 key appearing under more than one kingdom is `NOT_RECORDED`
itself, at 54,387 rows (0.213%), which is what the placeholder means. No real family homonym
appeared. Level 1 and level 2 keys carry the family, so a genus homonym — and they are common — is
already separated.

## Measured

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB) unless the medium says otherwise.
The read pass is the **network-source** figure: SMB at ~67 MB/s.

### The fraction — 64 of 8,369 parts, two selections

`--parts 64` and `--parts 64 --spread`. **The whole corpus has not been run**, and §"Not run" says
what that would cost.

| | prefix | spread |
|---|---|---|
| rows read | 24,549,994 | 26,660,444 |
| **rows placed** | 17,721,800 (**72.19%**) | **25,846,007 (96.95%)** |
| no coordinate | 6,828,194 | 814,437 |
| coordinate outside ±90/±180 | 0 | 0 |
| beyond ±85.0511° | 74 | 11 |
| read, place and write | **23.5 s** | **24.4 s** |
| peak `VmHWM` | 2.76 GB | 2.47 GB |
| `points.parquet` | 250.5 MB | 430.2 MB |
| `members-taxonomy.parquet` | 159.4 MB | 187.6 MB |
| `countrycode` terms | 247 | 252 |
| top term | `GB` 5,776,288 | `US` 8,713,483 |

The two selections disagree on everything that matters, and that is the finding: **a prefix of the
parts is not a sample of this corpus.** The census's own top-country ranking (US, AU, FR) is the
spread selection's, not the prefix's.

| the spread sample's layer | artifacts | largest | rows in none |
|---|---|---|---|
| level 0, family | 8,550 | 1,390,637 | 276,003 |
| level 1, genus | 56,893 | 434,814 | 757,065 |
| level 2, species | 186,464 | 207,709 | 1,878,926 |

⊘ **Artifact counts do not scale with rows** — a taxonomy saturates — so these are a lower bound on
the whole corpus's, not a 1/137th of it. The census's 200-part sample holds 11,502 families and
463,351 species, and a whole-corpus taxonomy is on the order of a few million artifacts against the
640,478,658 of entity-space headroom.

### The build — 25,846,007 rows to a 1.60 GB bundle in 80 s

`tessera build --stage-timings` over the spread fraction. `check` passes with 0 warnings.

| stage | wall | `VmHWM` |
|---|---|---|
| `source_ids` | 1.0 s | 410 MiB |
| `dictionary` | 5.2 s | 613 MiB |
| `geometry_read` | 5.5 s | 1,307 MiB |
| `signature_sort` … `external_ids` | 6.6 s | 1,612 MiB |
| `attribute_tail` | 8.2 s | 2,151 MiB |
| **`layers`** | **20.8 s** | 2,923 MiB |
| `text_index` | 0.0 s | 3,604 MiB |
| `filter_postings` | 5.3 s | 3,604 MiB |
| `record_blob` | 5.2 s | 3,604 MiB |
| `tiler_sort` + `segment_write` | 2.2 s | 3,604 MiB |
| `manifests` | 12.1 s | 3,604 MiB |
| **whole build** | **80.1 s** | **3,604 MiB** |

`layers` is the largest stage and it is the one that does not scale linearly. `text_index` is free
because there is no text column, which is what dropping `locality` bought.

| | |
|---|---|
| `schema` | 4 columns, **1.00 B/row** against the 12 B fixed row (+8%) — 0.93 GiB per 10⁹ items |
| `kingdom` | 25,846,007 of 25,846,007 entities have a value |
| `specieskey` | 23,955,426 (92.7%); 185,418 distinct keys, the index is **101.4 MB** |
| `year` | 25,221,921 (97.6%); 508 distinct |
| `scientificname` | 25,846,007 (100%) |
| taxonomy | 251,907 artifacts across 3 levels, 276,003 rows in none |
| resolution | 25,846,007 points in 3,508,005 distinct cells — **13.6%** have a cell of their own |

⊘ **`RESOLUTION LOST` fires, and it is a property of the corpus.** A 16-bit cell against Web
Mercator's whole domain is ~600 m at the equator and occurrence records genuinely coincide at a
locality. Overture reports 12.1% and rung 5's `geo` view 6.8% for the same reason. Every position
written is correct, only coarse.

⊘ **The serving layout changed between the two fractions.** At 17.7M rows every level chose
artifact-major (`served rows`); at 25.8M rows levels 0 and 1 chose `served column`, their
`everywhere` fraction having risen from 0.204 and 0.096 to 0.501 and 0.312. The pick is automatic
and re-evaluated at every fold (decision 0094, `configuration.md` §`layout`), and nothing on the
wire names a layout, so this is a latency choice rather than a contract. It does mean a
whole-corpus build's layout cannot be read off a fraction's.

### The bundle — 1.60 GB, 61.9 B/row

| | |
|---|---|
| `views` | **656.0 MB** |
| `attrs` | **452.2 MB** — the record blob 296.6 MB, `specieskey` 101.4 MB, `year` 54.3 MB |
| `entities` | 206.8 MB |
| `members` | 179.3 MB |
| `row-column` | 103.4 MB |
| `tile-index`, `containment`, `terms` | 2.7 MB together |

The prefix fraction's bundle is 1.07 GB over 17,721,800 rows, 60.2 B/row, so the two agree to 3%.

### Served — the surface is what the declaration says

`tessera serve` on 8191–8193 against the prefix fraction's 1.07 GB bundle. **Open costs 5.85 s**,
building every level's artifact row form so no request pays for one.

`/v1/meta` publishes one view `geo`, one layer `taxonomy/tree`, and three filter operands:
`kingdom` (category: `eq`, `in`), `specieskey` (keyword: `eq`, `in`, `prefix`, `contains`) and
`year` (numeric: `eq`, `in`, `range`). `scientificname` has no operand, which is what a column
claiming neither placement flag is.

| principal | terms | zoom-0 viewport | bytes |
|---|---|---|---|
| no terms | 0 of 247 | **2.1 ms** | 790 |
| `GB` | 1 | 280.0 ms | 260,020 |
| `GB`, `US`, `NO` | 3 | 385.1 ms | 550,452 |
| all countries | 247 | 709.6 ms | 722,357 |

A principal holding no term gets an empty map, which is what filling the access column with
`UNRECORDED` is for.

## Modelled — the whole corpus

**Every figure below is a fraction's own scaled by the row ratio (×137.1 from the spread run), not
a measurement.** It is sound for sizing and it is not a substitute for the run's own report.

| | |
|---|---|
| rows read | 3,654,488,638 |
| **rows placed** | **3,542,849,433** (96.95%) |
| entity space used | **82.5%** of 4,294,967,296, leaving 752,117,863 for artifacts |
| `points.parquet` | **59.0 GB** |
| `members-taxonomy.parquet` | **25.7 GB** |
| membership entries | ~1.0×10¹⁰, against rung 5's 1.63×10⁹ |
| largest family artifact | ~1.9×10⁸ (the census's Anatidae is 177,354,694) |
| the read pass | **~0.9 h** off the share (two 64-part runs at 22.0 s and 24.4 s) |
| the build | **~3.0 h** at 61.9 B/row — a floor, `layers` and `manifests` being superlinear |
| **the bundle** | **~219 GB** |

## Measured — whole corpus

Built three times, 2026-09-13 and 2026-09-14, to measure first the bounded-assembly design and
then a set of serve-path fixes, against the fraction's projections above (`layers` at scale, disk
against the ~304 GB modelled here, and zoom-0 whole-map latency). Full figures, all three builds,
are [`../../docs/ingest-campaign.md`](../../docs/ingest-campaign.md) §4d.

**3,495,729,729 placed rows** built under `--memory-budget 24g` to a **196 GiB bundle in 2 h 52 m
33 s** (third build, 2026-09-14, record-blob format 11 with a per-segment cut index), against
3 h 30 m 55 s (format 10) after the bounded-assembly design and 4 h 09 m 35 s (format 9) before
it. `verify --deep` clean in **15 m 08 s at 0.61 GB peak anonymous**. Served a fourth time,
2026-09-14, after a fix returning freed memory to the allocator on a cadence: `tessera serve`
opens to `/readyz` in **202 s at 6.7 GB anonymous** under a 24 GiB cap, peak **10.7 GB anonymous**
(third serve: 197 s, peak 16.0 GB), `oom_kill` 0. The stage that missed the memory budget under
the second build, `filter_postings`, now fits it: its own stage peak fell to 8.9 GB anonymous,
against 31.5 GB before the fix.

Hot zoom 0, whole-map, fell 4 to 6× for the sparse principals under the third build (1% 268 → 57
ms, 5% 1,269 → 231 ms, 10% 2,653 → 453 ms) and stays linear in visible rows. Under the fourth
serve the 50% principal falls further, 13.2 s → 2.2 s: its grant is contiguous in Morton space, so
it takes the per-cell route, and the route's half of the identity column now fits the page cache
the freed memory no longer holds. The 100% principal moves only 16.9 s → 14.6 s: its per-cell
route touches the whole 28 GB identity column, which still does not fit under the cap. Open at the
time of writing; see the whole-map-selection memo cited in §4d.

Disk stayed well inside the fraction's projection: 429 GB free at the start, a minimum of 221 GB
free at the end, against a pre-flight forecast of ~539 GB that is known to overstate. The taxonomy
still starts at family, as ruled above; what a whole-corpus `layers` costs is now measured rather
than modelled, in the campaign section.
