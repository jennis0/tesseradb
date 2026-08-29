# Overture Maps places + divisions — the ladder's second geographic rung

**73,631,092 places** and **1,074,177 division polygons**, from the release staged at
`overture/2026-08-19.0`. Places is CDLA-Permissive 2.0; **divisions is ODbL**, which is share-alike
— this rung is a measurement fixture and is not distributed, but the boundary layer is the ODbL
half and anything published from it inherits that.

```bash
~/venvs/ingest/bin/python -m test_corpora.overture.prepare
cd "$TESSERA_LADDER/overture" && tessera check && tessera build
```

**Status: prepared, and validated end to end on one of the sixteen places parts.** The whole corpus
has not been run.

| one part, 2026-08-28, local NVMe | |
|---|---|
| `prepare.py --parts 1` | **98 s** — divisions 29 s, join 47 s, entity ids 18 s |
| places | 4,599,286 |
| `tessera check --payloads` | OK — 9 vocabularies, 12 attributes, 3 layers |
| `tessera build` | **55 s wall, 1.33 GB peak RSS** |
| served viewport, zoom 0, 3-country principal | 8,469 artifacts — 8,448 divisions, 13 taxonomy, 8 predicate |
| bundle | 484,326,539 bytes — 105 B/point |
| artifacts | 1,955 taxonomy minted · 17,544 division declared · 8 from the predicate layer |
| in no artifact | 204,422 taxonomy (4.4%, no category path) · 5,062 divisions (0.11%, in no polygon) |

Sixteen parts extrapolate to roughly **20 minutes** of `prepare.py`.

⊘ The build's `RESOLUTION LOST — only 6.5%` warning is the slice, not the corpus. `part-00000` is
Latin America alone — 47 countries, x spanning 0.00 to 0.29 of the frame — inside a whole-world
extent. The figure to hold against GeoNames' 85.7% is the one the full run gives.

## What it is for

**This is the rung the campaign expects to break** (plan §8). At 7.4×10⁷ points it is the first
past the 5×10⁷ wall, and both of the campaign's build-side walls are expected here:

- **W1** — a whole-corpus root cluster over 7.4×10⁷ members is exactly the enumerated membership
  that failed its own Roaring round trip at 5×10⁷.
- **W2** — `tessera build`'s peak RSS is not bounded by `--memory-budget`; above ~5×10⁷ points
  carrying artifacts it was OOM-killed at the machine's size, three runs, one number.

Everything else it is the only source of: a **tiered category taxonomy six levels deep over 2,117
categories**, a **boundary tree that is genuinely a tree rather than a ladder**, and a place set
5.5× GeoNames' with the same synthetic country compartment, so the two rungs' access figures are
directly comparable.

## What the source turned out to be

Measured over the staged parquet 2026-08-28, not taken from the publisher's page. Distributions
marked *(one part)* are `part-00000`, 4,599,286 of the 73,631,092 places; counts are over
everything.

| | |
|---|---|
| places | **73,631,092** — matches the release notes exactly |
| `division_area` | **1,074,177** — 1,035,702 polygons, 38,475 multipolygons, 380,935,687 vertices |
| `division` (point form) | 4,658,700 |
| division subtypes present | **nine, not twelve** — locality 553,493 · neighborhood 320,613 · microhood 87,148 · macrohood 46,455 · county 39,882 · localadmin 21,386 · region 4,717 · country 378 · dependency 105 |
| taxonomy depth *(one part)* | 1 → 211,575 · 2 → 815,356 · 3 → 2,027,912 · 4 → 1,181,629 · 5 → 120,785 · 6 → 37,607 · none → 204,422 (4.4%) |
| taxonomy roots *(one part)* | 13 of the publisher's 14; `services_and_business` largest at 941,416 |
| `taxonomy.primary` *(one part)* | 1,847 distinct — the publisher claims 2,117 across the whole set |
| `basic_category` *(one part)* | **278 distinct**, equal to `taxonomy.primary` on 38% of rows |
| `country` *(one part)* | present on all but 47 rows, from `addresses[1].country` |
| `source_dataset` | eight — meta, Microsoft, BrightQuery, Foursquare, AllThePlaces, DAC, PinMeTo, RenderSEO |
| `confidence` | [0, 1], no nulls |
| `operating_status` *(one part)* | null on 84.5%; `open` 682,834, `permanently_closed` 29,773 |
| `names.primary` | never null, mean 20.8 bytes, max 259 — ~1.5 GB whole |
| `update_time` *(one part)* | 2007-11-05 to 2026-08-14 |
| brands *(one part)* | 251,035 places carry one, 612 distinct |

## Six things the survey settled, four of which the plan had wrong

**`hierarchies` is on `type=division`, not on `division_area`.** Both the staged README and the
campaign plan say the polygons carry the explicit hierarchy array. They carry `division_id`,
`parent_division_id` and `subtype`; the ancestry lives on the point form. `prepare.py` joins the
two once so the spatial join returns a whole path rather than a single id.

**There is no polyhierarchy.** All 4,658,700 divisions carry exactly **one** hierarchy path, so
`hierarchies[1]` is the whole of it. Asserted at every run rather than assumed — a division with
two paths would need a ruling, not a subscript. The polyhierarchy the campaign expects to force a
decision is still MeSH at rung 3.

**The division subtypes are not levels, so the boundary layer is `nested` and not `tiered`.** A
division's path runs 1 to 9 entries deep, and `locality` appears at every path position from 1 to
8 — a locality contains a locality. Position 3 alone holds county (3,197,815), locality (259,336),
localadmin (16,412) and region (7,117). A ladder cannot hold a same-level edge; `nested` is a tree
in the edges with every artifact at level 0, which is what the data is. This is the plan's §9.2
"one tiered layer over twelve subtype columns" refuted by the data it names.

**The places taxonomy *is* a clean tree, and that was checked rather than assumed.** Every category
appears at exactly one depth; every category but the 13 roots has exactly one parent; the last
entry of a path is always `taxonomy.primary`. So position is level, `tiered` is legal, and the
member file is a fixed six-element list with nulls past the leaf. The two layers on this rung are
therefore the two hierarchy kinds side by side over one point set — which is a sharper version of
the contrast GeoNames was picked for.

**`basic_category` is Overture's *basic level* rollup, not the leaf.** 278 values against 1,847
leaves, and it sits inside the same path as an ancestor. So it is a third category column between
the 14 roots and the leaf rather than a substitute for either, and colouring by each in turn is
three resolutions of one tree without touching the artifact layer.

**Three of the plan's seven columns are not columns.** `country` is `addresses[1].country`;
`source_dataset` and `update_time` are on the one `sources` entry whose `property` is empty —
exactly one per place, the rest being property-level provenance (`/properties/confidence` on every
row). Counting all the source entries makes `Overture` look like the dataset every place came from.

## The join, and the 300× it took to make it affordable

The campaign plan's shape — one `ST_Within` against all 1.07M `division_area` polygons — ran for
**over 50 minutes on 500,000 places** before it was killed. That is 29 hours for the corpus.

The cost is not the polygon count; it is that a few enormous polygons are tested against nearly
every point. Summed bounding-box area over the release, against a world of 64,800 deg²:

| tier | polygons | vertices | Σ bbox area | max vertices |
|---|---|---|---|---|
| fine — locality, neighborhood, microhood, macrohood, localadmin | 1,029,095 | 252.1M | **16,652 deg²** | 228,355 |
| county | 38,908 | 81.5M | 37,835 deg² | 345,467 |
| coarse — region, dependency, country | 4,191 | 35.2M | **148,532 deg²** | 255,252 |

Only the *deepest* containing area is wanted, so **the join runs finest tier first and each tier
sees only what the last left unplaced**. The same 4.6M places against the same 1.07M polygons,
**45 seconds**:

| tier | time | placed | left |
|---|---|---|---|
| fine | 9 s | 2,170,224 | 2,429,062 |
| county | 13 s | 2,410,516 | 18,546 |
| coarse | 11 s | 13,484 | 5,062 |

**County places more than half**, which is Overture's own coverage rather than a choice: 553,493
localities have a polygon against 3,481,755 that exist only as points.

⊘ Two false economies were paid for on the way, and both are recorded in `prepare.py`.
`CREATE INDEX … USING RTREE` on the polygon table costs **over an hour and the join never reads
it** — `SPATIAL_JOIN` plans as a sequential scan of both sides and builds its own index.
Materialising the polygons whole beside their own partition doubles a ~11 GB resident table for
nothing.

## Two decisions about the join

**Land areas only.** 1,983 divisions carry a maritime area beside their land one — 1,074,177 areas
against 1,072,194 distinct `division_id`. Joining both puts a point at sea inside a country, which
is true of the territorial claim and not of the boundary layer this rung is for.

**The lineage comes from the divisions' own hierarchy, not from stacking the containing polygons by
subtype.** One spatial join finds every containing area; the deepest one's path is the whole
ancestry. Stacking would not be consistent: `division_area` polygons are generalised for
cartography — the staged README says so — so a point can sit inside a locality and outside the
county that locality belongs to, and a stacked chain would then assert a containment the publisher
does not make.

## What is declared

Three layers, and their three membership kinds are the rung's real subject.

| Layer | Membership | Hierarchy | What it tests |
|---|---|---|---|
| `places/taxonomy` | enumerated, fixed six-element list | `tiered`, 6 levels | a deep published taxonomy with a zoom→level map |
| `boundaries/divisions` | enumerated, variable-length lineage | `nested` | a real tree in the edges, ~600,000 artifacts |
| `programmes/source` | `{ attribute = "source_dataset" }` | `flat` | a predicate membership — the tagged-programme case |

Alongside them, `division_country`, `division_region` and `division_county` are lifted out of the
lineage into indexed columns, so **the same containment can be asked for as an attribute predicate
as well as through the nested layer**. Which of the two the surface prefers is what the plan wants
measured here, and carrying both makes that a measurement rather than an argument — the comparison
itself has not been run. `locality` is deliberately not among them: 553,493 values belongs to the
layer.

**The disclosure split lands differently from GeoNames', and the reason is containment.** The three
category vocabularies, `source_dataset` and `operating_status` are `public`: a category names no
country, so serving them discloses nothing about a compartment. `country` and the three division
vocabularies are `derived`: a division sits inside exactly one country and the country *is* the
compartment. ⊘ Every vocabulary here is minted from the corpus, Overture publishing its taxonomy as
documentation rather than as a staged file — where GeoNames could point `feature_code` at
`featureCodes_en.txt` and call it published, these are the corpus's own distinct values.

**The access side is synthetic, as at GeoNames and for the same reason.** Overture carries no
rights or audience field, so each place's access term is the country of its first address. A
principal here is a set of countries, and every figure this rung produces says *synthetic policy
over real data*.

## The three membership kinds all serve, and one build report said otherwise

`programmes/source` is the attribute predicate. The build's artifact-pass report printed
`0 artifact(s), 0.000 everywhere, 0.0 blocks/artifact` for it, which reads as a layer that is
declared, reachable and serving nothing. It is not: the manifest carries its eight artifacts and a
served viewport returns all eight with masked counts, beside 13 taxonomy and 8,448 division
artifacts, for a three-country principal at zoom 0.

The zeros were honest about the walk and wrong about the layer. The pass observes a level through
its **stored** Roaring memberships; an attribute predicate has none, its members being the value
column evaluated per request, so every figure in the shape comes back zero. `artifact_pass.rs`
already said so where it declines to write such a level a row-major column; the report a line above
did not. It now prints the registry's count and says the shape is not observed, and
`a_predicate_over_a_category_column_mints_its_values` covers the case — the path's existing test
reads a bare indexed `u32` and asserts the level's *version* rather than its count, so it would
have passed whether or not an artifact existed.

⊘ This declaration is still **the only one in the repository that uses an attribute membership**,
so the kind's only end-to-end exercise is the one this rung brought.

## ⊘ A nested layer has no levels, so it has no zoom bound

Rung 1's first serving finding was that **a tiered layer returns every level whatever the zoom**,
and that the corpus's own zoom→level map is what bounds it — 254 artifacts against 464,655 at zoom
0 (`docs/ingest-campaign.md` §4). `boundaries/divisions` is `nested`, which refuses
`[[layer.levels]]` outright, so it has **no such map to offer** at roughly 600,000 artifacts.
Expect that finding to bite harder here, and expect it to be this rung's first serving result. It
is named in the declaration at the layer it applies to.

## The frame, and the standing deferral

Web Mercator, whole domain, normalised to the unit square, **y south**, exactly as at GeoNames — so
the two corpora share a tile address and a 16-bit cell is an XYZ tile at zoom 16. Latitude is
clipped to ±85.0511° and `frame.json` carries the clip count, which the build's clamp report
structurally cannot see. **This is redone when native projection lands**; see `../README.md`.
