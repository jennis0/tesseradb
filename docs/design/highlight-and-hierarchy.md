# Highlight, member-of, and browsing a hierarchy — design

**Date:** 2026-09-02
**Status:** Normative — r6, 2026-09-02. Reviewed once (Appendix R) and every finding
dispositioned; **all four rulings of §9 taken**. C32 and C33 are in `architecture.md` Appendix C
(r59). `contracts.md` §3.2 is amended as each server stage of §10 lands, and the conformance
comparator's schema with it. **§10's three server stages are built** (`member_of`, `highlight`
with `point_rows`, and `browse` with `max_browse_rows`), and §7's two ⊘ costs are measured at r6;
⊘ **§5's client is not.** Owner
direction 2026-09-02, recorded because the design turns on it: **a filter has a mode — `filter`
narrows the draw, `highlight` keeps every point and lights the matched ones — and the same
capability serves an artifact selection and a general filter alike; the operand is named
`highlight` and the interface says "matched"; a hierarchy must be browsable independently of the
viewport; a layer whose artifacts are spread across the map draws nothing and is a filter layer.**

**Owns:** the `highlight` operand on `/v1/viewport` and the three columns it adds; the `member_of`
filter leaf; the verb that serves a layer's hierarchy by lineage rather than by viewport; the
client's hierarchy panel, its selection modes and the density wash; and what a layer with no
computed content means to a client.

**Reads with:** `architecture.md` §4 (the invariants), §8.1–§8.2 (the filter contract this
extends) and Appendix C; [`contracts.md`](contracts.md) §3.2 (`filters`, `layers`,
`artifact_rows`, the *tiles*, *points* and *artifacts* frames); [`dag-hierarchies.md`](dag-hierarchies.md)
§6–§7 (the cut, `parent_ids`, C29); [`selection-operand.md`](selection-operand.md) (the leaf
this one is spelled like, and the argument that a row-space operand adds no register row);
[decision 0104](../decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md) (the
artifacts frame's `matched` bit, which `highlight` generalises); [`artifact-shapes.md`](artifact-shapes.md)
§8 C (`computed`, and the empty list); [`client-components.md`](client-components.md) §5.3
(the catalogue this adds one component to).

---

## 1. Summary

Rung 3 put a 30,217-descriptor DAG into a bundle and the viewer could show none of it. The
viewport serves a budget cut — roughly 48 artifacts at zoom 0, doubling per level — and a MeSH
descriptor's members are spread over the whole layout, so the cut deepens uniformly and the leaves
arrive at a zoom nobody reaches. The card shows children and not parents; the list is the cut,
sorted by count. Selecting a descriptor drew a hull, which for a spread artifact is the map's
outline. Three things follow, and they are one design because they share an operand.

- **`highlight`** *(§2)* — a second field of the one `/v1/viewport` request, beside `filters`
  and in its grammar, evaluated over the candidate `filters` leaves, answering per tile (`highlighted` count), per point (`highlighted`
  bit) and per artifact (`highlighted` bit). The draw keeps every point; the client lights the
  matched ones and dulls the rest, and renders the per-tile count as a wash so the points that were
  *not* drawn show too.
- **`member_of`** *(§3)* — a filter leaf naming one artifact of one layer, resolved to that
  artifact's membership inside `M_auth`. It sits in `filters` or in `highlight` identically, so
  "narrow to this cluster" and "light this descriptor" are the same clause in two positions, and
  the date range beside it works the same way.
- **`/v1/artifacts/browse`** *(§4)* — a layer's hierarchy by lineage: its roots, one artifact's
  children and parents, and a name search, each row carrying the masked count. Independent of the
  viewport, paged, and gated per artifact by the same criterion the viewport applies.

And a client rule *(§5)*: **a layer declaring `computed = []` is a filter layer** — never named in
a viewport's `layers`, never drawn, never labelled; reached through the panel and applied as a
clause. `mesh/descriptors` is declared so from 2026-09-02.

## 2. The `highlight` field

**Filter and highlight are two fields of one request, never two endpoints**: a viewer narrowed by
one clause and lit by another sends both in the same `POST /v1/viewport` and reads both answers
off the same frames. The request gains `highlight?`, a boolean expression in exactly `filters`' grammar
(`contracts.md` §3.2: leaves over declared columns, `region`, and §3's `member_of`; the three
combinators; the same nesting bound). It is evaluated **over the candidate `filters` produced** —
absent `filters`, over the masked candidate — and it never changes which rows the response holds.
What it adds:

| frame | column | position | value |
|---|---|---|---|
| *tiles* | `highlighted: uint64` | fifth, after `served`; always present | of this tile's `matched`, how many also satisfy `highlight`; equal to `matched` when `highlight` is absent |
| *points* | `highlighted: bool` | after the render scalars and before the `membership:<layer>` columns; present only when the request carried a `highlight` | whether this served point satisfies `all_of[filters, highlight]` — which for a served point is `highlight`, every served point being in the candidate |
| *artifacts* | `highlighted: bool?` | fifteenth fixed column, after `matched` | decision 0104's `matched` bit computed for **`all_of[filters, highlight]`**: `true` where a member this principal may see, inside the request's tiles, satisfies both; `null` when the request carried no `highlight` |

Every answer is the **conjunction** with the `filters` candidate — the three rows say one thing.
`highlighted` joins `region` and `member_of` as a **reserved column name, refused at the build**:
a corpus declaring a render column of that name would put two columns of one name on the points
frame, and a by-name reader would take the wrong one.

**The draw is unchanged by `highlight`.** The cap clause, the density sampling and `served` run
over the `filters` candidate exactly as before, so the set of points a viewer sees is the same
with and without it. This is what makes it a highlight rather than a filter: the map does not
move, the marks do not resample, and a point the viewer was looking at stays where it is with its
brightness changed. A client that wants the map to narrow uses `filters`; one that wants it lit
uses `highlight`; the interface offers both on every clause *(§5.2)*.

**Composition.** `highlight` is a conjunction with the candidate by construction. Where both are
present, `highlighted ≤ matched ≤ visible` per tile. Two highlights are one expression under
`all_of` or `any_of`; there is no list of highlights on the wire, for the reason `filters` is one
expression — a second grammar is a second surface.

**The counts the interface shows.** Beside *visible* and *matched* the strip gains the highlight's
own line, and the word is the one the owner ruled: **the highlight matched N points**, N being the
sum of `highlighted` over the request's tiles. Under `filters` the strip already says what the
filter matched; the two lines read the same way about two clauses.

**Cost.** One more expression evaluated over the candidate, and one more bit tested per served
point and per served artifact. The engine already evaluates `filters` to a row-space set and
tests the artifacts frame's `matched` against it (0104); `highlight` is the same evaluation over
the same routes with a second expression, and the per-tile count is the `and_cardinality` the
`matched` count already is. §2.1 says where each part runs and what it scales with.

### 2.1 How it is served

A highlight is served on the filter's own machinery, and every one of its three answers scales
with what is on screen rather than with what matched.

1. **Evaluate.** The expression is evaluated exactly as `filters` is (`viewport.rs`,
   `filter::evaluate_routed`): leaves over the postings produce an **entity-space** verdict, and
   a `region` or `member_of` leaf produces a **row-space** one directly. ⊘ **The filter clause
   cache does not exist** (`filter-result-cache.md` is provisional and unbuilt): today a
   highlight's entity-space verdict is re-evaluated on every request, at the filter's own
   per-request cost, exactly as `filters` is. When that cache lands — keyed by the canonical
   top-level clause, `token_id`, `segments_version` and prefix, and holding `Entity` verdicts
   only — a clause held across pans is evaluated once, and a clause moved between the two
   positions is a hit where it routes entity-space and a re-evaluation where it routes row-space.
2. **Cross, over the request's tiles only.** A filter has two crossings into row space — project
   the whole verdict, at ~20–30 ns per matched entity, or walk the rows the request's tiles span
   and ask each whether its entity matched, at ~20–100 ns per row on screen
   (`probes/2026-08-11-viewport-crossing/`; the crossover rule in `viewport.rs`). A filter needs
   the whole-view form for nothing this design adds, and **a highlight never needs it**: its
   three answers are all inside the request's tiles, so it asks for the per-tile walk whatever the
   measured crossover rule would have chosen, and its cost is then bounded by the rows on screen —
   a 300,000-row viewport is ~18 ms whatever the highlight matched corpus-wide, where projecting a
   10⁷-entity verdict is ~216 ms.

   ⊘ **The walk is preferred, not guaranteed, and the fallback is the projection.** Both crossing
   sites take it only where the view's row space can be inverted — a view that published no
   `row-entity.u32` has no walk to take — so on such a view a highlight silently projects and pays
   the ~216 ms rather than the ~18 ms. That is the same silent fallback the measured rule itself
   takes, and it is a latency difference and never an answer difference: the two routes agree over
   every range a request can ask about. Stated here because *always* would read as an assurance
   (decision 0013) and the test that pins the
   route asserts the counter over a view that can invert.
3. **Count per tile.** `highlighted` is one `and_cardinality` of the crossed set against each
   tile's row range, the operation the `matched` count already is; bitmap cost is containers
   touched, and a tile is a contiguous run.
4. **Bit per point.** The served points are chosen from the `filters` candidate exactly as now,
   so the selection — and the row-projection cache behind it — is untouched by a highlight. Each
   served point is then one `contains` against the crossed set: at most `k_max_marks` lookups,
   ~66,000 at the demo's budget, microseconds in total.
5. **Bit per artifact.** 0104's `matches` probe with the highlight's crossed set in place of the
   filter's: early-exiting intersection per served artifact, the same cost the `matched` bit has.

**A `member_of` highlight is the cheap case.** An artifact-major membership is already a
row-space bitmap (`MembershipRows`, built for every level at open and at a generation move, on no
request path — a filter layer's included, since §5.4 keeps it out of `layers` but not out of the
bundle), so step 1 is a lookup and step 2 is an intersection with
the tile ranges — no postings read, no entity-space verdict, no crossing. A root descriptor's
27 million members cost the same as a leaf's fifty: containers touched inside the viewport.

**And that is the route whatever the level's serving layout is.** A row-major level (decisions
0093/0094) carries the artifact-major form beside its column — the residency saving that would
drop it is ⊘ not taken — so the leaf reads the bitmap there too. Reading the *column* instead is a
walk of every visible row of the view asking each of its labels whether it is this ordinal, which is a
correct answer at the wrong price: **measured at 2.85 s against 22 ms** on rung 3's
`mesh/descriptors`, a `dag` served `RowMajorList` at ~46 labels over 3.6 × 10⁷ rows (2026-09-02).
The walk survives only as the fallback for a level with no artifact-major form, which no level is,
and `Engine::member_of_column_walks` counts the times it is taken so that stays checkable rather
than remembered.

**A highlight change re-sends bits, not points.** Because the served set does not depend on the
highlight, a client that changes only the highlight holds every point it needs and wants only the
bits. `/v1/viewport` gains `point_rows`, mirroring `artifact_rows` (r44): omitted or `"full"` is
every column; `"highlight"` is the same points as `(tessera_id, highlighted)` — the row set and the
`served` split identical under either value, only the columns change, so it discloses nothing the
full answer would not. Nine bytes a point against the render columns' width, and the *tiles*
frame beside it carries the wash. The bits join the held points by `tessera_id`, and the served
set is deterministic (`select.rs` carries no random draw) **within one generation**: a stamp move
(`x-tessera-stale`) means the held set may no longer be what the same request serves, and the
client re-asks with `"full"`. A client asking for it while holding nothing meets identifiers it
cannot draw, knows it, and re-asks with `"full"`, exactly as `artifact_rows` does.

**What is not cheap, and where it is paid.** A broad highlight over a *category* or *text* leaf
still reads the postings to make its entity-space verdict on first use — the filter's own cost,
inside the filter budget, and cached after. Nothing in this design adds a whole-view crossing for
the viewport; the one whole-view intersection it adds is browse's filtered count (§4), paid once
per page row against a verdict the cache already holds.

## 3. The `member_of` leaf

A leaf of its own, spelled like `region` (`selection-operand.md` §2): `member_of` is a reserved
column name, refused at the build, and takes `{layer: <name>, artifact: <tessera_id>}` — the
identifier a **decimal string**, as `region`'s published-artifact spelling already carries it, a
bare JSON number losing the top of a `u64`.

```json
{"all_of": [{"member_of": {"layer": "mesh/descriptors", "artifact": "546790"}},
            {"published": {"range": ["2015-01-01", "2020-12-31"]}}]}
```

**Resolution.** The artifact is resolved by its `tessera_id` within the named layer to its
membership — the stored or derived set the artifact pass already reads for its masked count —
intersected with `M_auth`, in row space over the whole view, which is the shape every filter
operand takes (0104's three membership layouts; `selection-operand.md` §5's composition). A
`region` leaf spelled by published artifact already does this for a shape's membership; this leaf
does it for any artifact's, whatever its layer draws.

**Gating, and the one rule that matters.** The artifact must pass **its own existence criterion
for this principal** — the layer's `visibility`, the artifact's label under `artifact_visibility`,
and `require_member_visibility` against `M_auth` — exactly as it must to be served on the
artifacts frame. **An artifact that does not pass is an empty operand, never a refusal**, on
`contracts.md` §3.2's rule that an unknown *value* is an empty operand where an unknown *column*
is `422`: an identifier the principal was never served is a value, and answering `422` to it
would make this leaf an existence oracle over exactly what the criterion withholds. An unknown
layer is `422`, being deployment schema. The gate runs before any membership is read.

**Why not a `category` column of memberships instead.** A closed DAG puts an article under
fifty-five descriptors on average (`dag-hierarchies.md` §8), which is a list column with a
1.7 × 10⁹-entry index beside the membership that already holds the same pairs. The leaf reads
the membership; nothing is stored twice.

## 4. `POST /v1/artifacts/browse`

`{layer, level?, parent?, q?, filters?, limit?, cursor?}` → JSON `{artifacts: [...], parents: [...], next}`.
JSON rather than Arrow: a page is at most `max_browse_rows` small rows, and the verb is read by
the panel and the notebook alike.

Three forms, one gate:

- **Roots** — `parent` and `q` absent: the layer's artifacts with no served parent, by count
  descending, paged. On a `stacked` or `tiered` layer `level` names which level's artifacts are
  the roots — a `stacked` level has no edges and every artifact is a root of it; `tiered` roots
  are level 0 and `level` starts the walk lower. On `flat`, `nested` and `dag`, which have one
  level, `level` is `422` — a kind's levels are deployment schema, and a parameter accepted and
  ignored is a wrong answer that looks right.
- **Children** — `parent` given: the artifacts naming it among their parents, same order, paged;
  `parents` carries the requested artifact's own parents. On a `dag` layer a child is served under
  each parent that is served, as the artifacts frame already does (0117).
- **Search** — `q` given: the layer's artifacts whose key, or whose first supplied text content,
  contains `q` case-insensitively, same order, paged. ⊘ Whether search is in this verb or its own
  is §9 (b).

A row is the artifacts frame's row under `artifact_rows = "identity"` plus its key, its first
supplied text content where the principal may read it, `parent_ids`, `masked_count`, and — when
the request carried `filters` — `matched_count`:
`(tessera_id, key?, name?, masked_count, matched_count?, rung, parent_ids)`. No geometry — this
verb serves a hierarchy, and a layer that draws is drawn by the viewport.

**Across the kinds.** `flat` is one page of roots and no row has children; `nested` and `dag`
walk their edges; `tiered` walks containment edges level to level, a row's children being the
next level's artifacts naming it; `stacked` is a page per `level`. A layer that attaches to
another — a label layer hanging from a clustering — has no lineage of its own and its text is what
its target shows as a name, so it is not browsed separately.

**Counts under a filter.** `filters` is the viewport's own object, evaluated by the same routes to
a row-space verdict, and each row's `matched_count` is `|membership ∩ M_auth ∩ filter|`, one
`and_cardinality` per row against the filter's whole-view verdict. That verdict exists for the
entity-space routes and for `region`; **for a leaf over a render-only column** (`render = true`,
`index = false`), which the viewport answers only inside its tiles, **browse scans the whole view**
— the row route's own predicate over the hot column, run over every row rather than the request's
ranges (§9 (d), owner ruling). It is priced in §7: a count on a render-only attribute is the
expensive kind, and this is where it is paid. **Existence and
`masked_count` never move with the filter** — the same anchoring as everywhere else — and a row
whose `matched_count` is zero is still served. Order is by `matched_count` when `filters` is
present and by `masked_count` otherwise. This is deliberately *not* decision 0104's rule: the
artifacts frame answers a boolean because a per-viewport count would be tile-scoped beside a
whole-view masked count with nothing on the wire to say so, and because a filter change must move
one bit of a held row. Browse has no tiles and no held payload, so a count is exact over the
whole view and costs `limit` intersections, and neither objection applies.

**The gate runs before the page.** Every artifact served passed its own criterion, the one the
viewport applies (decision 0080), and the criterion is evaluated over the form's whole candidate
set *before* `limit` and `cursor` are applied — so a page's fill and `next` count only artifacts
this principal may see, and a withheld artifact leaves no gap a caller could count. **The order
is total**: count descending, then `tessera_id` ascending, so a cursor over tied counts neither
duplicates nor drops a row. `parents` is present on the children form only and is `[]` on the
others;
`masked_count` is `|membership ∩ M_auth|`, computed per request and never precomputed (C8's
`and_cardinality`). **A relation is named only where both ends are served** — C29 per entry: a
child whose parent is withheld is a root here, a parent below its floor is absent from `parents`,
and the requested `parent` itself failing its criterion answers an empty page identically to a
leaf. `limit` clamps to `selection.max_browse_rows` *(new, published in `/v1/meta`)* and `limit=0`
is `422` on `/v1/categories`' argument.

**What it does not do.** No geometry. No subtree fetch — a client walks, and §7 bounds the
walk. No `highlight`: a highlighted view of the hierarchy is the filtered one, since a count under
the highlight's expression is what `matched_count` is when that expression is sent as `filters`.

## 5. The client

### 5.1 The hierarchy panel

One component, `<tessera-hierarchy>`, in the explorer's left column beneath the filters: a layer
picker over the bundle's hierarchical layers, then a tree. Each row is a name, a masked count and
an expander; a child fetches its children on expansion through §4 and pages under "N more". On a
`dag` layer a node appears under each of its served parents and says *also under X* by listing its
other parents in the row's detail. A search box above the tree drives §4's search form and shows
its matches as rows with their lineage. When the map carries a filter the panel sends the same
`filters` and shows each row's `matched_count` beside its masked count, so a filtered map and a
filtered tree read the same numbers. Nothing in the panel depends on the viewport: it opens on
the roots whatever the zoom and it does not move when the map does.

### 5.2 Two verbs on every clause

Every clause the interface can form — a date range, a text match, a category, a drawn region, and
now a node of the panel — carries the same two actions, and the vocabulary is the owner's:

| action | wire | what the viewer sees |
|---|---|---|
| **Filter** | the clause joins `filters` | the map narrows to the matches; counts say *matched N* |
| **Highlight** | the clause joins `highlight` | the map stays; matches are lit, the rest dulled; counts say *the highlight matched N* |

A clause moves between the two positions without being re-entered. The filter panel's chips gain
the mode as a toggle on the chip; a panel node's click is highlight by default, since that is the
action that shows where a descriptor lives without losing the map.

### 5.3 The wash

The density underlay the map already draws from the *tiles* frame gains a second channel: the
`highlighted` count per tile, rendered as the wash where a highlight is set, and the `matched`
count where a filter is set and no highlight is. It is what shows the members that the cap clause
did not draw — a highlight over 27 million articles draws 66,000 of them and washes the rest — and
it is the only picture of a spread artifact that says anything. The wash reads the count column
and never a mark, so it is exact where the marks are a sample, and the interface labels it as the
count it is.

### 5.4 A layer with no computed content

`computed = []` on a layer's declaration already means *counts with no geometry* on the wire
(`artifact-shapes.md` §8 C). A client reads it as **a filter layer, and a filter layer is still a
layer** (owner ruling, 2026-09-02): it is in `/v1/meta`'s layer list and in the client's, listed
as a filter layer rather than presented for viewing — no draw toggle, no place in the "In view"
list, no label and no shape. It is never named in `layers` on a viewport request, so no artifact
pass runs for it and no kind-5 rows arrive; it is reached through the panel and applied through
§3. The server changes nothing for it: the declaration says what it is, and the client reads it. The card for one of its artifacts offers filter
and highlight and no *fit*. The rung's clustering keeps its shapes because its artifacts are
compact; the rule is per layer and the declaration states it.

### 5.5 What the existing pieces become

The artifact card gains parents above its children, both from §4. The "In view" list stays what it
is, the viewport's cut, for the layers that draw. *Filter to this artifact* and *outside this
artifact* on the card become §3 clauses in `filters`, replacing the shape-by-published-artifact
spelling of the `region` leaf for that use, which stays for a region drawn by hand.

## 6. The invariants, and two register rows (C32, C33)

**I2.** Every quantity served is computed inside `M_auth`: a `highlighted` count is an
`and_cardinality` over the masked candidate; a `member_of` operand is `membership ∩ M_auth`; a
browse row's count is the same masked count the artifacts frame serves; and every artifact §4
serves passed its own criterion. **I3/I12.** Neither `filters` nor `highlight` moves an artifact's
existence or its count; they move a bit, as 0104 ruled for `matched`. **I7.** The draw samples
after masking exactly as before; `highlight` adds a bit to sampled points and never chooses them.
**I10.** A `member_of` leaf carries a `tessera_id` and resolves it inside the trust boundary.

| | Surface | What is observable | Severity | Why accepted |
|---|---|---|---|---|
| **C32** | A highlight's counts and bits | Per tile, per served point and per served artifact, whether the request's second expression holds — a second filter's answer beside the first, over the same candidate | Low | **A second evaluation of the filter contract over the same masked candidate**, disclosing what one request carrying `all_of[filters, highlight]` in `filters` would have disclosed, differently arranged: `highlighted` per tile is the `matched` count that request would carry, and the per-point bit is set on points already served. The per-artifact bit is 0104's bit under a second expression. Nothing derived from outside `M_auth` and no quantity a caller could not already obtain in two requests. `point_rows = "highlight"` is a column subset of the full answer to the same request, on `artifact_rows`' argument (r44) |
| **C33** | The browse verb | A layer's artifacts by lineage — existence, masked count, name and parents — without a viewport and without a budget, so a principal can enumerate every artifact of a layer that passes their criterion | Low | **Enumeration was already available**: `artifact_budget` is a request bound and never a disclosure control (`contracts.md` §3.2 r38), so a zoom-0 viewport at a large budget with every level named serves the same set; this verb serves it paged and by relation instead of by position. Each row is the artifacts frame's identity row plus a name the principal may read, and each relation is C29's, named only where both ends passed their own criterion. The counts are C8's `and_cardinality`, per request — the filtered one against a verdict computed inside `M_auth` by the filter contract's own routes. **The residual is the search form**: a substring over keys and names that this principal may read — an existence test over artifacts already servable to them, and nothing about one that is not; an artifact below its floor matches no query. The `member_of` leaf's empty operand (§3) has a timing residual of the C4/C24–C26 family — a withheld artifact's criterion is evaluated where an unknown identifier is a lookup miss — which is the one posture the register's head note says those rows share, and it is registered here rather than as a row |

**No row for `member_of`.** It is a row-space operand over a membership the principal may already
count, composed like every other leaf; `selection-operand.md` §7's argument for the `region` leaf
holds unchanged, and the empty-operand rule of §3 is what keeps it from becoming an oracle.

## 7. What it costs

Per viewport request a highlight costs one per-tile crossing over the rows on screen, one
`and_cardinality` per tile, one `contains` per served point and one probe per served artifact —
tens of milliseconds at a 300,000-row viewport by the crossing probe's figures, independent of
how much the highlight matched. A `member_of` highlight skips the crossing.

**A `member_of` highlight is measured** (2026-09-02, against a served rung 3 on this box): the
whole request — zoom 2, the whole extent, `k = 200`, the full principal, highlighting *Health
Occupations* and its 13.46 million members — is **22 ms**, beside **21 ms** for the same viewport
with no highlight. The first such request of a *session* is **418 ms**, which is the level's
masked-count histogram (below) and not the highlight. ⊘ The other leaf shapes are still not
measured as a whole; each part is a measured operation of the filter path (§2.1).

**A browse page is measured** (2026-09-02, `cargo run --release -p tessera-bench --bin
browse_cost`; the box: twelve cores, 47 GB, load ~10 from concurrent work; medians of five):

| corpus | layer | roots | search | rows served |
|---|---|---|---|---|
| arXiv, 2.42 × 10⁶ items | `clusters/kmeans`, 64 artifacts | **0.1 ms** | 0.1 ms | 64 |
| arXiv | `clusters/hdbscan`, 197 artifacts | **0.5 ms** | 0.5 ms | 1 root, 184 matches |
| MedCPT, 3.6 × 10⁷ items | `clusters/kmeans`, 253 artifacts | **1.0 ms** | 0.9 ms | 200, one page |
| MedCPT | `mesh/descriptors`, 30,217 artifacts | **39.7 ms** | 43.4 ms | 107 roots, 200 matches |

**The first page a session asks for is a different number, and on a row-major level it is the
larger one.** The order is by count, so a page needs every artifact's masked count; on a row-major
level the only route to those is one walk of the composed mask reading every visible row's labels
(decision 0093's named exception), cached per `(session, layer, level, mask)`. So the figures above
are every page after the first, and the first pays the walk — **2.7 s** over MedCPT's
3.6 × 10⁷ rows × ~46 labels single-threaded, **0.52 s** since that walk was split across the
engine's pool (2026-09-02). An artifact-major level pays nothing here: `clusters/kmeans` is 106 ms
for its first page and 1.2 ms after. A `member_of` leaf over the same level pays the same walk
once, and shares the cache with browse.

**What the figures say is that the page is bounded by the layer and never by the corpus.** A
36-million-item corpus browses its 253-artifact clustering in a millisecond and its
30,217-descriptor DAG in forty; the corpus is the same on both rows and the artifact count is
not. The 39.7 ms is the gate over 30,217 artifacts — a verdict and one masked `and_cardinality`
each, ~1.3 µs an artifact — which is what §4's *the gate runs before the page* costs, and what
paging by relation buys against a viewport that could not show the layer at all.

**The whole-view scan is measured, and the r4 ceiling was pessimistic by a factor of about
forty.** A browse `filters` whose leaf routes row space — a render-only column — runs the row
route's own predicate over every row of the view:

| rows | scan | per row |
|---|---|---|
| 2,422,486, arXiv's count | **2.2 ms** | 0.9 ns |
| 36,000,000, MedCPT's count | **20.7 ms** | 0.6 ns |

⊘ **The row counts are real and the corpus is not.** Neither ladder corpus declares a
`render = true, index = false` column — arXiv's `archive`, `primary_category` and `submitted_at`
are rendered *and* indexed, and MedCPT's `published` is too — so a both-routes column takes the
entity route here and the scan is unreachable on either at all. It is measured instead on a
bundle whose one attribute is declared render-only, at each corpus's own row count. What that
costs the figure is the corpus's *shape*: one `i32` in a single build segment against a real
view's segment ladder, where the scan's work is the same fixed-width read and compare either way.

The r4 estimate — *under about a second at 3.6 × 10⁷, tens of seconds at 10⁹* — was bounded from
the per-tile crossing's 20–30 ns per row and is wrong by that whole factor, for a reason the
derivation could not carry: **the row route runs under rayon**, split over the same domain the
tile sweep splits, and it reads a slot where the crossing reads a row's entity and tests a bitmap.
At 0.6 ns a row a 10⁹-row view is **about 0.6 s** — still the largest single cost this design
adds, still once per page request, and no longer the figure that decides whether the feature is
affordable. §9 (d)'s ruling stands with a measured argument under it rather than a bound.

⊘ **The scan is admission-gated and NOT cancellable**, and the r4 wording that said otherwise is
corrected here rather than carried. `Engine::browse` takes no cancellation token, and it could
not usefully hold one: `scan_rows` carries no checkpoint on any path — the viewport's own
coarse-zoom whole-view scan is equally uninterruptible — and the verb is a unary JSON route with
no consumer-gone signal of the kind the streamed viewport reads off its sink. So a browse under a
row-routed filter **holds its compute permit for the whole scan**: ~2 ms at 2.4 × 10⁶ rows, ~21 ms
at 3.6 × 10⁷, ~0.6 s at 10⁹ — which is what a concurrent request waits behind at the gate's width,
and is the figure to weigh rather than the scan's own. Making it cancellable is a change to the
shared row route and not to this verb, and it is not made here. ⊘ Until the clause cache exists
the scan repeats on every page of one walk; when it does, the whole-view verdict is what it holds
and a walk pays the scan once.

A search is a scan over the layer's keys and names, 30,217 strings at rung 3, bounded by the
layer's artifact count and never by the corpus — measured above as the `search` column, within a
few per cent of the roots form on every row, the difference being a substring test per artifact
against a lineage walk. The wash costs the client one more channel over the tile counts it
already decodes.

## 8. Not in scope

Two defects the rung surfaced are fixed as defects, not designed here: the hull of the hovered and
opened artifact draws nothing on rung 3 (the wire carries the ring; the layer does not draw it),
and the label budget names three of 253 compact clusters at zoom 0. Both are on
`client-delivery.md`. The single-artifact drill-down omitting `parent_ids` is the same list's.

## 9. Owner rulings sought

All four taken 2026-09-02.

- **(a) One verb or two.** `browse` as one verb with three forms (§4) — **ruled: one verb.**
- **(b) Search in this design or deferred.** — **Ruled: in**; the register residual is C33's.
- **(c) A filter layer's place.** — **Ruled: a filter layer is still a layer.** It stays in the
  meta's and the client's layer lists as a layer that lists no visible elements; it is not
  presented for viewing and the client never names it in a viewport's `layers`. The server
  changes nothing for it (§5.4).
- **(d) Browse under a filter that routes row-space — ruled: (ii), scan, and price it.** "Counts
  on render-only attributes are expensive; a scan even over many points should be seconds" (owner).
  A leaf over a render-only column
  (`render = true`, `index = false`) is evaluated only over a request's tiles today, and browse
  has none; served naively its `matched_count` would be silently zero (review finding 1, the
  failure 0104 exists to prevent). Two shapes: **(i) refuse** — a browse `filters` whose leaves
  route row-space is `422`, a column's placement being deployment schema a caller reads off
  `filter_operands`; or **(ii) scan** — a whole-view pass over the row column, priced in §7.
  Recommended: (i), because (ii) is a new whole-view route whose cost is the corpus's row count
  per request and whose only user would be a count in a side panel. `region` is unaffected, being
  whole-view by construction. §4 and §7 carry the ruling and the price.

## 10. Order of build

1. Server: `member_of` (§3) — the leaf, the gate, the empty-operand rule, a conformance test that
   a withheld artifact's leaf matches nothing and refuses nothing.
2. Server: `highlight` (§2) — the three columns and `point_rows`, the per-tile count checked
   against `matched` under the same expression in `filters`, and a test that the served set is
   identical with and without a highlight.
3. Server: `browse` (§4) with `level`, `filters` and `matched_count`, and `max_browse_rows`.
4. Client: the chip mode and the wash (§5.2–§5.3) over 1–2; the panel (§5.1) over 3; the filter-
   layer rule (§5.4) and the card's parents (§5.5).

1–2 and 4's first half can run as one server track and one client track in parallel; 3 and the
panel follow. Contracts §3.2 is amended with each server stage, and the conformance comparator's
schema with the frame columns.
