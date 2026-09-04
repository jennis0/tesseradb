# GeoNames — the ladder's first geographic rung

**13,463,857 features**, from the gazetteer staged at `geonames/2026-08-27` (CC BY 4.0). No
embedding and no GPU: the coordinates are already WGS84, and the build transforms and quantises
them against a declared `web_mercator` frame, which is what makes this the cheapest real map in the
corpus and the right place to shake out an ingest pipeline.

```bash
~/venvs/ingest/bin/python -m test_corpora.geonames.prepare
cd "$TESSERA_LADDER/geonames" && tessera check && tessera build
```

## What it built

Local NVMe, 47 GB machine, 2026-08-30, on the declared projection. `--memory-budget` not set, so
the build derived its own.

| | |
|---|---|
| `prepare.py` wall | 2:54 (15 s to read `allCountries.txt`, 37 s to assign ids) |
| `tessera build` wall | **2:59** |
| build peak RSS | **3.55 GB** |
| bundle | **1,341,841,220 bytes** — 99.7 B/point |
| `tessera verify` | OK in 1.03 s |
| artifacts | 688 feature minted; 464,655 admin declared by `artifacts-admin.parquet` |
| points with a cell of their own | **85.7%** of 13,463,857, in 11,544,034 distinct cells |
| rows in no artifact | 5,003 feature (blank class), 398 admin (no country and no admin code) |
| stored row | 13 columns, 25.00 B/row against the 12 B fixed row |

⊘ **The wall and the peak are not comparable with the 6:05 and 4.2 GB recorded on 2026-08-28.**
Two rounds of build work landed between those runs — the mapped attribute columns and the split
text index — and neither has anything to do with the projection. The **bundle** is comparable, and
it is the comparison worth having: this build and the last `f32` build of the same declaration
differ by **176 bytes** across 1.34 GB, every one of them a compression delta on a file whose
contents shifted by a few low-order position bits.

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

**Every admin artifact carries a name, and it is the code where GeoNames has no other** (2026-08-28).
The layer declares a supplied `name`, and a layer declaring a supplied kind is refused an artifact
without one, so `artifacts-admin.parquet` names all 464,000: countries, admin1 and admin2 from the
publisher's own tables, admin3, admin4 and the placeholder levels by their qualified code. Without
it the demo showed every region as unnamed — a key is never shown as a name (the client's rule) —
and its hovered outlines were boxes belonging to nothing a viewer could read.

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

## The frame

The declaration asks for `lon = [-180, 180]`, `lat = [±85.0511287798066]` under
`projection = "web_mercator"` — the projection's whole domain — and the build snaps that outward to
the square at z0 (0, 0), which is `x [0, 1]`, `y [0, 1]`. So a 16-bit cell is exactly an XYZ tile
at zoom 16, and y runs **south**, the direction `clients/ts/core/src/coords.ts` pins against
deck.gl's tile addressing and the one a symmetric frame in metres gets backwards.

The frame report reads:

```
view 'world': web_mercator, quantising against x [0, 1], y [0, 1]
        asked for lon [-180, 180], lat [-85.0511287798066, 85.0511287798066] — snapped outward to
        the square at z0 (0, 0), lon [-180, 180], lat [-85.05112877980659, 85.0511287798066]
        the data spans x [0, 1], y [0, 1] — 65536 x 65536 of the 65536 x 65536 cells
        13463857 point(s) placed, none clamped onto the frame's edge
        571 of 13463857 point(s) (0.0%) CLIPPED at web_mercator's ±85.0511287798066° domain
```

**571 clipped and none clamped, and the two are different things** (`projections.md` §7). A
latitude past the projection's domain lands *exactly* on the frame's edge, where the clamp rule
says a point is not clamped — so the clamp counter structurally cannot see one of these however
many there are, and clipping is counted on its own. `prepare.py` surveys the same 571 rows in the
source and reports the same split, 18 north and 553 south.

**The corpus was placed by a Python module before the projection layer existed**, and rebuilding it
here is what closed that. The two bundles were compared point for point: every one of the
13,463,857 stored positions matches what `test_corpora/common/projection.py` computes from the
source degrees, exactly and with no tolerance. Against the old bundle, 40,293 points (0.30%, one in
334) sit in a different cell — each by exactly one cell on one axis, and each because the old build
narrowed the coordinate to `f32` before quantising, which resolves 256 steps per cell at this
frame. The new placement is the accurate one.

## Not built yet

`places/containment` — the parent/child edges between features themselves, from `hierarchy.txt`, as
a `nested` lineage. It needs a DAG walk and will meet genuine multiple parents, which is the
polyhierarchy refusal for real rather than as a keying artefact.
