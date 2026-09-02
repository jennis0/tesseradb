# Highlight, member-of, and browsing a hierarchy — design

**Date:** 2026-09-02
**Status:** Provisional r1 — under review. What remains before it is normative: one adversarial
review; the three rulings of §9; the two leak-register rows of §6 accepted into `architecture.md`
Appendix C; `contracts.md` §3.2 amended with §2–§4's wire. ⊘ **Nothing here is built.** Owner
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

- **`highlight`** *(§2)* — a second boolean expression on `/v1/viewport`, evaluated over the
  candidate `filters` leaves, answering per tile (`highlighted` count), per point (`highlighted`
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

## 2. The `highlight` operand

`POST /v1/viewport` gains `highlight?`, a boolean expression in exactly `filters`' grammar
(`contracts.md` §3.2: leaves over declared columns, `region`, and §3's `member_of`; the three
combinators; the same nesting bound). It is evaluated **over the candidate `filters` produced** —
absent `filters`, over the masked candidate — and it never changes which rows the response holds.
What it adds:

| frame | column | value |
|---|---|---|
| *tiles* | `highlighted: uint64` | of this tile's `matched`, how many also satisfy `highlight`; equal to `matched` when `highlight` is absent |
| *points* | `highlighted: bool` | whether this served point satisfies `highlight`; the column is present only when the request carried one |
| *artifacts* | `highlighted: bool?` | decision 0104's `matched` bit, computed for `highlight` instead of `filters`: `true` where a member this principal may see, inside the request's tiles, satisfies it; `null` when the request carried none |

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
`matched` count already is. ⊘ Not measured; modelled as a second filter pass, which the
filter-result cache (`filter-result-cache.md`) keys by expression and so shares between the two
positions when the same clause moves from one to the other.

## 3. The `member_of` leaf

A leaf of its own, spelled like `region` (`selection-operand.md` §2): `member_of` is a reserved
column name, refused at the build, and takes `{layer: <name>, artifact: <tessera_id>}`.

```json
{"all_of": [{"member_of": {"layer": "mesh/descriptors", "artifact": 546790}},
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

`{layer, parent?, q?, limit?, cursor?, pin?}` → `{artifacts: [...], parents: [...], next}`.

Three forms, one gate:

- **Roots** — `parent` and `q` absent: the layer's artifacts with no served parent, by
  `masked_count` descending, paged.
- **Children** — `parent` given: the artifacts naming it among their parents, same order, paged;
  `parents` carries the requested artifact's own parents. On a `dag` layer a child is served under
  each parent that is served, as the artifacts frame already does (0117).
- **Search** — `q` given: the layer's artifacts whose key, or whose first supplied text content,
  contains `q` case-insensitively, same order, paged. ⊘ Whether search is in this verb or its own
  is §9 (b).

A row is the artifacts frame's row under `artifact_rows = "identity"` plus its key, its first
supplied text content where the principal may read it, `parent_ids`, and `masked_count`:
`(tessera_id, key?, name?, masked_count, rung, parent_ids)`. No geometry — this verb serves a
hierarchy, and a layer that draws is drawn by the viewport.

**Every artifact served passed its own criterion**, the one the viewport applies (decision 0080);
`masked_count` is `|membership ∩ M_auth|`, computed per request and never precomputed (C8's
`and_cardinality`). **A relation is named only where both ends are served** — C29 per entry: a
child whose parent is withheld is a root here, a parent below its floor is absent from `parents`,
and the requested `parent` itself failing its criterion answers an empty page identically to a
leaf. `limit` clamps to `selection.max_browse_rows` *(new, published in `/v1/meta`)* and `limit=0`
is `422` on `/v1/categories`' argument.

**What it does not do.** No counts under a filter — a row's count is the masked count, and a
filtered view of the hierarchy is `highlight` with `member_of` per node, which the panel composes
client-side for the nodes it shows. No level selector — `rung` is served and a client folds by it.
No subtree fetch — a client walks; §7 bounds the walk.

## 5. The client

### 5.1 The hierarchy panel

One component, `<tessera-hierarchy>`, in the explorer's left column beneath the filters: a layer
picker over the bundle's hierarchical layers, then a tree. Each row is a name, a masked count and
an expander; a child fetches its children on expansion through §4 and pages under "N more". On a
`dag` layer a node appears under each of its served parents and says *also under X* by listing its
other parents in the row's detail. A search box above the tree drives §4's search form and shows
its matches as rows with their lineage. Nothing in the panel depends on the viewport: it opens on
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
(`artifact-shapes.md` §8 C). A client reads it as **a filter layer**: it is never named in
`layers` on a viewport request, so no artifact pass runs for it and no kind-5 rows arrive; it is
absent from the map's layer toggles and the "In view" list; it draws no label and no shape; it is
reached through the panel and applied through §3. The card for one of its artifacts offers filter
and highlight and no *fit*. The rung's clustering keeps its shapes because its artifacts are
compact; the rule is per layer and the declaration states it.

### 5.5 What the existing pieces become

The artifact card gains parents above its children, both from §4. The "In view" list stays what it
is, the viewport's cut, for the layers that draw. *Filter to this artifact* and *outside this
artifact* on the card become §3 clauses in `filters`, replacing the shape-by-published-artifact
spelling of the `region` leaf for that use, which stays for a region drawn by hand.

## 6. The invariants, and two register rows

**I2.** Every quantity served is computed inside `M_auth`: a `highlighted` count is an
`and_cardinality` over the masked candidate; a `member_of` operand is `membership ∩ M_auth`; a
browse row's count is the same masked count the artifacts frame serves; and every artifact §4
serves passed its own criterion. **I3/I12.** Neither `filters` nor `highlight` moves an artifact's
existence or its count; they move a bit, as 0104 ruled for `matched`. **I7.** The draw samples
after masking exactly as before; `highlight` adds a bit to sampled points and never chooses them.
**I10.** A `member_of` leaf carries a `tessera_id` and resolves it inside the trust boundary.

| | Surface | What is observable | Severity | Why accepted |
|---|---|---|---|---|
| **C31** | A highlight's counts and bits | Per tile, per served point and per served artifact, whether the request's second expression holds — a second filter's answer beside the first, over the same candidate | Low | **A second evaluation of the filter contract over the same masked candidate**, disclosing what one request with that expression in `filters` would have disclosed, differently arranged: `highlighted` per tile is the `matched` count that request would carry, and the per-point bit is set on points already served. The per-artifact bit is 0104's bit under a second expression. Nothing derived from outside `M_auth` and no quantity a caller could not already obtain in two requests |
| **C32** | The browse verb | A layer's artifacts by lineage — existence, masked count, name and parents — without a viewport and without a budget, so a principal can enumerate every artifact of a layer that passes their criterion | Low | **Enumeration was already available**: `artifact_budget` is a request bound and never a disclosure control (`contracts.md` §3.2 r38), so a zoom-0 viewport at a large budget with every level named serves the same set; this verb serves it paged and by relation instead of by position. Each row is the artifacts frame's identity row plus a name the principal may read, and each relation is C29's, named only where both ends passed their own criterion. The count is C8's `and_cardinality`, per request. **The residual is the search form**: a substring over keys and names that this principal may read — an existence test over artifacts already servable to them, and nothing about one that is not; an artifact below its floor matches no query |

**No row for `member_of`.** It is a row-space operand over a membership the principal may already
count, composed like every other leaf; `selection-operand.md` §7's argument for the `region` leaf
holds unchanged, and the empty-operand rule of §3 is what keeps it from becoming an oracle.

## 7. What it costs, modelled

⊘ Not measured. `highlight` is a second filter evaluation over the candidate; at rung 3's scale a
`member_of` over a root descriptor is a membership of 2.7 × 10⁷ entries intersected with a mask,
which is the artifact pass's own cost for that artifact today. A browse page is one masked count
per row — `limit` rows, one `and_cardinality` each, against memberships the engine already holds
for the artifacts frame. A search is a scan over the layer's keys and names, 30,217 strings at
rung 3, bounded by the layer's artifact count and never by the corpus. The wash costs the client
one more channel over the tile counts it already decodes.

## 8. Not in scope

Two defects the rung surfaced are fixed as defects, not designed here: the hull of the hovered and
opened artifact draws nothing on rung 3 (the wire carries the ring; the layer does not draw it),
and the label budget names three of 253 compact clusters at zoom 0. Both are on
`client-delivery.md`. The single-artifact drill-down omitting `parent_ids` is the same list's.

## 9. Owner rulings sought

- **(a) One verb or two.** `browse` as one verb with three forms (§4, recommended: one gate, one
  row shape, one page rule), or roots-and-children separate from search.
- **(b) Search in this design or deferred.** Recommended: in, because a 30,000-node tree without
  it is browsable only by patience; the register residual is stated in C32.
- **(c) A filter layer's place on the wire.** Recommended: the client simply never names it in
  `layers` (§5.4) and the declaration changes nothing on the server. The alternative — the server
  refusing a `computed = []` layer in `layers` — adds a refusal outside the disclosure surface.

## 10. Order of build

1. Server: `member_of` (§3) — the leaf, the gate, the empty-operand rule, a conformance test that
   a withheld artifact's leaf matches nothing and refuses nothing.
2. Server: `highlight` (§2) — the three columns, the per-tile count checked against `matched`
   under the same expression in `filters`.
3. Server: `browse` (§4) and `max_browse_rows`.
4. Client: the chip mode and the wash (§5.2–§5.3) over 1–2; the panel (§5.1) over 3; the filter-
   layer rule (§5.4) and the card's parents (§5.5).

1–2 and 4's first half can run as one server track and one client track in parallel; 3 and the
panel follow. Contracts §3.2 is amended with each server stage, and the conformance comparator's
schema with the frame columns.

## Appendix R — review trail

- **r1 (2026-09-02).** Drafted from the owner's direction of the same day after rung 3 was served
  and its DAG found invisible. Not yet reviewed.
