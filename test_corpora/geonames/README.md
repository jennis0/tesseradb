# GeoNames — the ladder's first geographic rung

**13,463,857 features**, from the gazetteer staged at `geonames/2026-08-27` (CC BY 4.0). No
embedding, no GPU, no projection in the UMAP sense: the coordinates are already WGS84 and become
positions by transform and quantisation, which is what makes this the cheapest real map in the
corpus and the right place to shake out an ingest pipeline.

```bash
~/venvs/ingest/bin/python -m test_corpora.geonames.prepare
cd "$TESSERA_LADDER/geonames" && tessera check && tessera build
```

## What it built

Local NVMe, 47 GB machine, 2026-08-28. `--memory-budget` not set, so the build derived its own.

| | |
|---|---|
| `prepare.py` wall | ~2 min (19 s to read `allCountries.txt`, 24 s to project and assign ids) |
| `tessera build` wall | **6:05** |
| build peak RSS | **4.2 GB** |
| bundle | **1,329,553,710 bytes** — 98.7 B/point |
| `tessera verify` | OK in 0.94 s |
| artifacts minted | 465,343 — 688 feature, 464,655 admin |
| points with a cell of their own | **85.7%** of 13,463,857, in 11,543,951 distinct cells |
| rows in no artifact | 5,003 feature (blank class), 398 admin (no country and no admin code) |
| stored row | 13 columns, 25.00 B/row against the 12 B fixed row |

The plan's estimate for this rung was a 1.4 GB bundle against ~4 GB transient; both held. Neither
wall the campaign expects — the Roaring round trip at 5×10⁷ members, and peak RSS ignoring
`--memory-budget` — is anywhere near being reached at this scale, which is what makes this rung the
right place to shake out the pipeline rather than the mechanisms.

The build's own layout choice is worth keeping. Every admin level went **row-major and disjoint**,
with `everywhere` falling 0.453 → 0.001 as the levels get finer and blocks per artifact settling at
1.0 — while `features/taxonomy` level 0 is `1.000 everywhere` at 197.4 blocks per artifact, because
all nine feature classes are spread across the entire tile index. Two layers over one point set with
opposite spatial character, which is exactly the contrast this rung was chosen for.

## What it is for

Two **orthogonal hierarchies over one point set**, which nothing else in the ladder gives cheaply:
a published feature taxonomy (9 classes → 684 codes) and a five-level administrative tree
(464,000 artifacts). Plus a violently skewed numeric, two more with opposite kinds of missingness,
a 33-year date column that is also the ingest order, and 0.18 GB of names — small enough that prose
search over the whole corpus is a measurement rather than an aspiration.

**The access side is synthetic and every figure says so.** GeoNames carries no rights or audience
field, so each point's access term is its own country code — a real column standing in for a
compartment. A principal here is a set of countries. The 6,997 rows with a blank country carry no
term and take the declared `public` default, which the build reports.

## What the source turned out to be

Measured over `allCountries.txt`, not taken from the publisher's page.

| | |
|---|---|
| rows | 13,463,857 |
| latitude / longitude range | exactly [-90, 90] and [-180, 180] |
| beyond ±85.0511° | 571 — 18 north, 553 south (Antarctic) |
| feature classes | 9, plus a blank on 5,003 rows |
| `class.code` pairs | 688 in the data; `featureCodes_en.txt` covers every real one |
| countries | 253, plus a blank on 6,997 rows and one `YU` |
| admin1–4, qualified | 4,823 · 51,199 · 175,362 · 231,645 |
| population | nonzero on 4.9%; max 6,814,400,000; **min -12** |
| elevation | present on 17.8%, range [-10,911, 8,848] |
| dem | present on 97.9% after its -9999 sentinel is nulled, range [-415, 8,794] |
| names | mean 13.1 bytes, max 180 — 0.18 GB whole |
| modification date | 1993-09-24 to 2026-08-27 |

## Five decisions the data forced

**Admin codes are country-qualified.** `admin1` has 823 distinct codes standing for 4,823 real
regions, so a bare code merges Scotland with an unrelated Brazilian state. GeoNames' own
`admin1CodesASCII.txt` and `admin2Codes.txt` are qualified, so this is the publisher's convention.

**A feature code is keyed by its `class.code` pair**, which is how `featureCodes_en.txt` is keyed
and what keeps the taxonomy a tree — a bare code would put one artifact under several parents.

**An absent value is null, never a key.** The empty string is refused as a category key and a null
key means *absent* at the reserved code 0, so a feature whose code was never recorded belongs to
its class and to no code — which is what the source says.

**A hole in the admin chain becomes an artifact.** 115,042 rows carry an admin4 with no admin3.
Requiring the chain would place every one of them a level shallower than the source does; leaving a
null would keep them but lose the containment edge, because `parent_edges` is `windows(2)` and
deliberately does not read past a gap. So the level gets an explicit `-` artifact naming exactly
that set — 1,373 of them across the whole corpus, against 464,000 real ones. Nothing is dropped and
every edge exists.

**`population` is `i64`.** Two reefs in Kiribati carry negative populations. That is plainly an
error in GeoNames and is what the file says; carrying it means a range filter shows it.

## What is declared, and why the disclosure settings split

`feature_class` and `feature_code` are `public`: they are orthogonal to geography and come from
GeoNames' own published file, so serving their names discloses nothing about this corpus.

**`country`, `admin1`–`admin4` and `timezone` are `derived`**, and the reason is containment rather
than caution. An administrative value sits inside exactly one country, and the country *is* the
compartment — so telling a principal barred from France that `FR.84.69` exists names a compartment
they do not hold. `admin3`, `admin4` and `timezone` are additionally minted from the corpus itself,
there being no published file for them, so `public` would disclose the corpus's own contents.

That makes this rung the ladder's only test of `derived` at scale: a 231,645-value vocabulary
listing computed per request from inside the mask.

`index` is uniform across the whole administrative hierarchy — one rule for the tree, the same at
every level. `render` follows cardinality, which is a property of the data rather than a decision:
254 countries is a legend, 231,645 fourth-level divisions is a lookup.

## The frame, and the standing deferral

Web Mercator, whole domain, normalised to the unit square, **y south** — the direction
`clients/ts/core/src/coords.ts` pins against deck.gl's tile addressing and the one a symmetric
frame in metres gets backwards. A 16-bit cell is therefore exactly an XYZ tile at zoom 16.

Latitude is clipped to ±85.0511° and the projected result is then held inside the frame, which is
not the same thing: `MAX_LATITUDE` is itself an `atan`/`exp` output, so projecting it overshoots
[0, 1] by an ULP, and the build counts a clamp as `v > max` — without the hold, all 571 clipped
points would be reported as clamped. `frame.json` carries the clip count, which the clamp report
structurally cannot see.

**This is redone when native projection lands.** `prepare.py` then emits `lon`/`lat` unchanged, the
declaration names a `projection` and writes its `extent` in WGS84 — the box `frame.json` already
records — and the corpus is rebuilt. See `../README.md`.

## Not built yet

`places/containment` — the parent/child edges between features themselves, from `hierarchy.txt`, as
a `nested` lineage. It needs a DAG walk and will meet genuine multiple parents, which is the
polyhierarchy refusal for real rather than as a keying artefact.
