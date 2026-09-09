# What rung 6 (GBIF) is, before anything is built

**Date** 2026-09-09. **Source** `/mnt/nas/joe/tessera/datasets/gbif/2026-06-01/occurrence.parquet`,
8,369 parts, 258.5 GiB compressed, read over SMB. **Box** WSL2, 12 cores, 47 GB.

Two passes, both **measured**, run before a `test_corpora/gbif/` existed. They answer the three
questions that decide the rung: whether it fits one `u32` entity space, what the `layers` stage
would hold, and which columns a sparse schema should take.

    python3 census_footers.py > footers.json   # footers only, no row data — 103.8 s
    python3 census_taxa.py   > taxa.json       # 200 of 8,369 parts, 8 columns — 70.7 s

## It fits one entity space, and that was not obvious

| | |
|---|---|
| rows | **3,654,488,638** |
| `u32` ceiling | 4,294,967,296 |
| entity space used | **85.1%** |
| headroom for artifacts | 640,478,658 |

Artifact ids allocate downward from `u32::MAX` into the same space
([sharding.md](../../docs/design/sharding.md)), so the headroom is what the taxonomy's artifacts
have to fit in. The sample holds 463,351 distinct species and 11,502 families over 81,681,686 rows,
so a whole-corpus taxonomy is on the order of a few million artifacts against 640 million of room.
**Sharding is not needed for this rung.** The campaign table's 3.50×10⁹ was low: the vintage staged
here is 3.65×10⁹.

## The largest artifact is the constraint, not the row count

`merge_member_runs` (`crates/tessera-build/src/layers.rs`) holds **the largest single artifact's
members** resident as `u64` while it sorts them — its own doc comment states this is the bound.
Extrapolated from the 200-part sample (×44.7):

| level | largest artifact | members | resident |
|---|---|---|---|
| kingdom | Animalia | 2,809,414,577 | **22.5 GB** |
| phylum | Chordata | 2,313,453,660 | 18.5 GB |
| class | Aves | 2,138,854,540 | 17.1 GB |
| family | Anatidae | 177,354,694 | 1.4 GB |
| species | *Zenaida macroura* | 28,173,879 | 0.2 GB |

A taxonomy starting at **family** caps that at 1.4 GB; one starting at kingdom needs half the box
for one artifact. This is the same unbounded shape the text index and, from 2026-09-08, the keyword
dictionary both resolved by chunk-spill-merge (`probes/2026-09-08-keyword-spill/`), and `layers` is
the third instance. Owner ruling 2026-09-09: rung 6 starts its tiered layer at family, `kingdom`
rides as a rendered category column instead, and the `layers` bound is a later pass.

## What a sparse schema takes

One column per attribute type (owner ruling, 2026-09-09). Distinct counts are from the sample;
null fractions are the sample's own.

| type | column | distinct | null | note |
|---|---|---|---|---|
| category | `kingdom` | 9 | 0.0% | Animalia 2.81×10⁹, Plantae 518M, Bacteria 134M, Chromista 81M, Fungi 52M |
| keyword | `specieskey` | ~10⁶ | — | a real lookup; **not** `gbifid` or `occurrenceid`, unique per row |
| numeric | `year` | 377 | 3.7% | 2023 peaks at 321M |
| utf8 | `scientificname` | — | — | stored, not indexed |
| text | `locality` | — | — | 48.45 GiB, the corpus's largest column — deferred past the first build |

`countrycode` (251 values, US 1.25×10⁹, AU 320M, FR 206M) is the compartment, as `publisher` is at
rung 5. `basisofrecord` (9 values, HUMAN_OBSERVATION 2.83×10⁹ against PRESERVED_SPECIMEN 380M) is
the best category the schema does *not* take.

## The ten largest columns on the share

Compressed bytes, from the footers — what a `prepare.py` pays to read.

| column | GiB | B/row |
|---|---|---|
| `locality` | 48.45 | 14.2 |
| `occurrenceid` | 44.54 | 13.1 |
| `gbifid` | 24.17 | 7.1 |
| `catalognumber` | 18.16 | 5.3 |
| `decimallatitude` | 16.67 | 4.9 |
| `decimallongitude` | 16.45 | 4.8 |
| `recordedby` | 12.73 | 3.7 |
| `scientificname` | 10.92 | 3.2 |
| `verbatimscientificname` | 8.32 | 2.4 |
| `eventdate` | 7.00 | 2.1 |

## What is not measured here

- **The bundle's size.** Modelled at 250–450 GB without `locality`, from rung 5's bytes per row
  with one view instead of two and three tiered levels instead of seven. Not measured, and it is
  what decides whether the second volume (campaign §1) is needed.
- **Coordinate coverage.** `decimallatitude` is present in the footers of every part, but the
  fraction of rows carrying one is not counted here; rung 5's own GBIF join placed 75.90% of its
  images on the ground, which is a different denominator.
- **The whole-corpus taxonomy.** Every artifact figure above is a 2.2% sample extrapolated by row
  count, which is sound for sizing and is not a substitute for the build's own report.
