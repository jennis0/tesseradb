# multiview — a fixture for `docs/design/views.md`

**Status:** Working code, never normative, like every other directory in `test_corpora/` (see the
parent README). Not a rung on the dataset ladder — it measures nothing and is outside the ingest
campaign. Its job is to be a `corpus.toml` a two-view build can be pointed at, once one exists.

**`tessera check` passes against it; `tessera build` refuses it.** The declaration surface —
`[[view_group]]`, both roster forms, `members`, typed metadata, `scope` on an attribute and on a
layer — parses and validates as of the build-surface stage, and `check` reports the group shape
beside the views. ⊘ The multi-view build is still specified and not implemented (views.md §7), so
a build against this file refuses naming that, which is expected.

## What is real-derived and what is synthetic

No network call, no GPU, no staged dataset — `prepare.py` reads exactly one file,
`data/ladder/geonames/points.parquet`, the GeoNames rung's own **built** output already on local
disk from an earlier run of `test_corpora/geonames/prepare.py`. Three of that sample's columns are
carried straight through and are real:

| Column here | Source | |
|---|---|---|
| `world.lon`/`.lat`, `quarter_alt.lon`/`.lat` | GeoNames' `x`/`y` | un-projected back to WGS84 degrees through `test_corpora.common.projection.unproject` — real longitude and latitude that made a round trip through Web Mercator |
| `attrs-constant.importance` | GeoNames' `population` | unchanged |
| `attrs-constant.kind`, `vocab-kind` | GeoNames' `feature_class` | unchanged; the vocabulary text is `featureCodes_en.txt`'s own nine classes |

**Everything else is synthetic and seeded** (`SEED = 20260830`, `numpy.random.default_rng`, plus a
splitmix64 hash keyed by entity id so a position never depends on iteration order or on which file
computes it first):

- `quarter`'s four per-quarter layouts (`quarter-2026-Q*.parquet` `x`/`y`) are an abstract
  embedding with no geographic meaning — a base 2D point per entity, put through a different
  rotation/scale/translation per quarter. `projection = "none"` in `corpus.toml` says exactly this.
- `sentiment`, the group-scoped attribute, is a seeded normal, clipped to `[-1, 1]`.
- The access term's presence (~4% of entities carry none) and the four entity-overlap buckets are
  seeded coin flips over a hash of the entity id, not the source data's own distribution.
- `collections` and `quarter_clusters`, the two layers' membership.

If `data/ladder/geonames/points.parquet` is missing, `prepare.py` refuses rather than substituting
anything — run `python -m test_corpora.geonames.prepare` first, or pass `--geonames-points` at any
points parquet carrying `entity_id, x, y, country, feature_class, population`.

## Regenerate

```bash
python3 -m test_corpora.multiview.prepare              # ~100,000 rows total, well under a second
python3 -m test_corpora.multiview.prepare --scale 3.0   # ~300,000 rows
python3 -m test_corpora.multiview.validate               # structural checks; see below for output
```

Output goes to `$TESSERA_LADDER/multiview/` (default `data/ladder/multiview/`, `test_corpora/common/paths.py`).
`prepare.py` copies `corpus.toml` there beside the parquets, matching every other rung.

## Files

| File | Rows (default scale) | What it is |
|---|---|---|
| `corpus.toml` | — | the declaration; committed here, copied beside the derived data |
| `world.parquet` | 12,816 | the plain view `world`: `entity_id, lon, lat, access` |
| `quarter-2026-Q1..Q4.parquet` | ~8,200 each | the `quarter` group, form A: `entity_id, x, y, access, sentiment` |
| `quarter-alt.parquet` | 32,833 | the `quarter_alt` group, form B: `entity_id, quarter, lon, lat, access` |
| `attrs-constant.parquet` | 21,300 | the two entity-scoped attributes: `entity_id, importance, kind` |
| `vocab-kind.parquet` | 9 | `kind`'s closed vocabulary: `key, code, title` |
| `collections.parquet` | 6 | the unscoped layer: `key, contents, members, access` |
| `clusters-quarter.parquet` | 24 (6 × 4 quarters) | the scoped layer: `key, quarter, contents, members` |

## What each `views.md` feature is exercised by

This table is the fixture's point — read it as the implementation's checklist, not as commentary.

| `views.md` feature | Where in this fixture |
|---|---|
| A plain `[[view]]` (§2) | `world` |
| A view group, form A — one file per view (§3.1) | `quarter`, `[[view_group.view]]` × 4 |
| A view group, form B — one file, a discriminator column (§3.1) | `quarter_alt`, `fields.view = "quarter"` |
| Typed roster metadata (§3.1) | `quarter.metadata = { label = "text", starts = "timestamp_us", ends = "timestamp_us" }`, present on all 4 `[[view_group.view]]` blocks |
| Shared views, `members` (§3.3) | `quarter_alt` declares `members = "quarter"`; no roster or metadata of its own, its own `visibility` retained |
| An entity in several views — the join rule's ordinary case (§4) | entities in `world` and every quarter (bucket A, 3,159 at default scale) |
| An entity in exactly one quarter besides `world` (§4) | bucket B, 3,229 |
| An entity in `world` only, no quarter (§4) | bucket C, 6,428 |
| An entity in quarters only, never `world` (§4) | bucket D, 8,484, each in exactly 2 of the 4 quarters |
| Label byte-agreement across every file an entity appears in (§4, §7) | `access` is assigned once per entity and carried unchanged into every file — `validate.py`'s first substantial check |
| A point carrying no label, taking the default (§6) | ~4% of entities, `access = null`, `point_visibility.default = "public"` |
| A constant (entity-scoped) attribute, numeric, indexed (§5) | `importance` (`i64`) |
| A constant attribute, category, with a closed vocabulary (§5) | `kind`, `vocabulary = "kind"` |
| A constant attribute's own `source`, distinct from `[defaults].source` (§5, §8) | `attrs_constant`, needed because bucket D entities never appear in `world` |
| A group-scoped attribute (§5) | `sentiment`, `scope = { group = "quarter" }` |
| A group-scoped attribute read from each view's own file, no `source` declared (§5, Appendix A) | `sentiment` has no `source` key; it is a column of each `quarter-2026-Q*.parquet` |
| Presence bitmap on a group-scoped attribute — some entities missing a value in some views (§5, decision 0064) | ~15% of each quarter's rows carry `sentiment = null` |
| An unscoped layer over a view and a group at once (§3.5) | `collections`, `views = ["world", "quarter"]`, default `scope = "entity"` |
| A scoped layer, a different artifact set per view of a group (§3.5) | `quarter_clusters`, `scope = { group = "quarter" }`, `views = ["quarter"]`, `fields.view = "quarter"` |
| Two roster forms in one corpus | `quarter` (form A) and `quarter_alt` (form B), side by side |

**Not exercised, deliberately out of this fixture's scope:** the create/drop control operations
(§3.2, §3.4) — there is no running service to send them to, only a build-time declaration; the
gate (§6) — every `visibility` here is `public`, since access control is GeoNames'/Overture's
fixture territory and this one is about the view/group/attribute/layer shapes; the pinned-leaf
filter grammar (§5's `name@key`) — a filter surface, not a corpus declaration.

## `validate.py` output

Structural checks over the parquets and `corpus.toml` as written — no build, no engine. Latest run
at the default scale:

```
validating data/ladder/multiview
  [6] per-(entity, view) row uniqueness — world, four quarters, quarter_alt: OK
  [7] label byte-agreement across 21,300 distinct entities: OK
      843 entities (4.0%) carry no access term
  [8] quarter_alt's discriminator agrees with quarter's roster, all 4 keys: OK
  [9] entity-overlap pattern: full=3,159 world+one=3,229 world-only=6,428 quarters-only=8,484
  [10] every quarters-only entity appears in >= 2 quarters: OK
  [11] constant attribute (importance, kind) covers all 21,300 entities: OK
  [12] `kind` vocabulary closure — 9 declared, 9 used: OK
  [13] sentiment (group-scoped) has both present and absent values in every quarter: OK
  [14] view_group.view metadata typed and present on all 4 blocks: OK
  [15] quarter_alt declares members=quarter and no roster of its own: OK
  [16] collections layer's 8,373 member ids are all real entities: OK
  [17] quarter_clusters partitions each quarter's own entity set: OK

17 checks passed.
```

## A finding for `views.md`, not a fixture defect

**One spelling in `views.md`'s examples is not a spelling the surface has.** Spec §3.1's form A
example and Appendix A both write a group's frame as
`extent = { min = [-40.0, -40.0], max = [40.0, 40.0] }` — `min` and `max` as two-element arrays.
`configuration.md` §1, which owns the *spelling* of every key (see its preamble), gives `extent`
four forms, and `{ min, max }` is the **scalar** one — one range applied to both axes, preserving
aspect ratio — with `{ x = [a, b], y = [c, d] }` the per-axis form. A group takes every `[[view]]`
key with the same meaning, so a group's `extent` has a view's four spellings and no fifth. This
file was written from views.md's example and now carries the surface's own spelling,
`{ x = [-40.0, 40.0], y = [-40.0, 40.0] }`, which is the same box. **The example is what needs
correcting, or `configuration.md` needs the array form; neither is decided here.**

Otherwise **Appendix A's worked declaration parses, and nothing else in it had to be adapted.**
Writing this corpus.toml against the r6 spellings — one `visibility` key, `[[view_group.view]]`
vs. `[view_group.views]`, typed `metadata`, `scope` on an attribute and on a layer, `fields.view`
on a group source and on a scoped layer — turned up no other gap between the appendix and spec
§3's key table; the one place this fixture departs from Appendix A's shape at all is giving the
constant attributes their own explicit `source` (`attrs_constant`) rather than defaulting to
`[defaults].source`, which spec §5 already allows and Appendix A simply didn't need because its
`year`/`venue` attributes only ever needed values for `papers`-view entities.
