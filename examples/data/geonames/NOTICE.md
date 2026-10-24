# GeoNames example dataset

150,000 features from the GeoNames gazetteer, on a world map: every populated place of 15,000 people or more, and a uniform sample of all the other features. The files are Parquet, compressed with zstd. `schema.toml` declares them as a Mosaica corpus, and `manifest.json` records the counts.

## What is here

| file | rows | contents |
|---|---|---|
| `points.parquet` | 150,000 | `geonameid`, `lon`, `lat`, `name`, `feature_class`, `feature_code`, `country`, `admin1`, `population` |
| `vocab-feature-class.parquet` | 9 | the feature classes, each a short word with GeoNames' description as its title |
| `vocab-feature-code.parquet` | 475 | GeoNames' feature codes, keyed `class.code`, with their names |
| `vocab-country.parquet` | 244 | ISO country codes and country names |
| `vocab-admin1.parquet` | 3,460 | first-level divisions, keyed `country.code`, with their names |
| `members-admin.parquet` | 150,000 | each feature's country and first-level division |
| `artifacts-admin.parquet` | 3,705 | the name of each country and division |

Coordinates are WGS84 degrees as GeoNames gives them. A population of 0 in GeoNames is null here. The feature classes are shown as words rather than GeoNames' letters:

| letter | word | GeoNames' description |
|---|---|---|
| A | region | Country, state, region |
| H | water | Stream, lake, other water body |
| L | area | Park, area, other region |
| P | settlement | City, village, other populated place |
| R | road | Road, railroad |
| S | building | Spot, building, farm |
| T | mountain | Mountain, hill, rock |
| U | undersea | Undersea feature |
| V | forest | Forest, heath, other vegetation |

47 features have no class in GeoNames and none here.

## Where it came from

[GeoNames](https://www.geonames.org/), the dump `allCountries.zip` downloaded on 2026-08-27 (staged as `geonames/2026-08-27`), with the code tables published beside it: `countryInfo.txt`, `admin1CodesASCII.txt` and `featureCodes_en.txt`.

The cut keeps all 34,143 populated places (feature class P) whose population is 15,000 or more, which is about GeoNames' own `cities15000`. The other 115,857 features are drawn uniformly, with seed 0, from the remaining 13,429,714, so the mix of classes among them is the gazetteer's own. Administrative regions also carry a population in GeoNames; they are left to the sample, where they are 4,595 of the 150,000.

Two features lie south of 85.0511° S, the limit of Web Mercator; the build places them on the map's edge. No feature lies outside longitude -180 to 180.

## Licence and attribution

GeoNames data is licensed under [Creative Commons Attribution 4.0](https://creativecommons.org/licenses/by/4.0/). The licensed material is the GeoNames export at <https://download.geonames.org/export/dump/>. This dataset is a modified extract of it: a subset of the features and columns, with the feature classes renamed, populations of 0 made null, and administrative codes prefixed by their country code.

Attribution: GeoNames, [www.geonames.org](https://www.geonames.org/).

GeoNames' disclaimer, kept as the licence requires: "The Data is provided "as is" without warranty or any representation of accuracy, timeliness or completeness."

## Making it again

From the repository root, with GeoNames' `allCountries.txt` and the vocabularies `test_corpora/geonames/prepare.py` writes in `data/ladder/geonames/`, and `duckdb` installed:

```bash
python -m test_corpora.geonames.example --out examples/data/geonames --min-population 15000 --features 150000 --seed 0
```

It takes about fifteen seconds.
