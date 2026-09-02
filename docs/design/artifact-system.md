# The artifact system — how it works

**Status:** Descriptive. This document orients an architect in what is built; it decides nothing.
The normative designs win wherever they and this document disagree:
[`annotations.md`](annotations.md) (the model),
[`annotation-representation.md`](annotation-representation.md) (the representation),
[`annotation-write-cycle.md`](annotation-write-cycle.md) (the write cycle),
[`configuration.md`](configuration.md) (the declaration surface),
[`artifact-serving-at-scale.md`](artifact-serving-at-scale.md) (the serving structures), and
`architecture.md` §4's invariants above all of them. Measurements are cited here only as
justification; the campaigns hold the detail. Gaps are marked ⊘ at the claim.

## 1. What an artifact is

An **artifact** is a named object over a set of points: a cluster, a label, an administrative
boundary, a tagged programme, an analyst's selection. It has:

- a **membership** — the set of entities (points) it is about;
- a **layer** and a **level** — a layer is a published collection of artifacts of one kind, with
  one declaration; levels are declared resolutions within it;
- **content** — *derived* properties computed from the visible part of the membership (count is
  intrinsic; `centroid`, `box`, `hull` are opt-in), and *supplied* properties written by the
  publisher (label text, a fitted shape), carried as **ranked contents** where each rank has its
  own **generating set** `G` — the documents that content was produced from;
- **edges** — a *parent* edge giving hierarchy (all within a level or all between levels, never
  mixed: `flat`, `nested` for clusterings, `stacked` for independent resolutions, `tiered` for
  levelled taxonomies), and a *dependency* edge attaching one artifact to another (a label to its
  cluster), which carries both deletion and visibility.

Identity is three-layered: internally an artifact is an **entity** like any point — its ids
allocated downward from the top of the entity space in 2¹⁶-aligned blocks, where point ids ascend
from zero, so which rule governs a row is a range check — outwardly it is an opaque `tessera_id`,
and at the declaration and control surfaces it is named by its **key**. Ordinals within a level are
dense and are never on the wire.

## 2. The disclosure model

Everything else in this document serves one idea: **every quantity a viewer sees is computed from
inside their own visible set `M_auth`** — never computed globally and then gated. Concretely:

- The **masked count** beside every served artifact is `|membership ∩ M_auth|`, and the same
  number is the input to the layer's **existence criterion** (`require_member_visibility`:
  a count, a fraction, or none): an artifact below its bar is *absent*, indistinguishable from one
  that never existed. There is no service-imposed floor; an undeclared criterion declares no test.
- Derived geometry is computed over `membership ∩ M_auth` only — a hull is the hull of what the
  viewer can see.
- Supplied content gates on **containment**: rank *i* is served only to a viewer for whom
  `|Gᵢ ∩ M_auth| = |Gᵢ|` — they can see *everything* the content was generated from — and the
  first satisfied rank is served entire, or no artifact at all. Which documents, never how many.
- A **dependent** artifact is served only where its target is served and is deleted when its
  target is deleted; a response never carries the name of an artifact it does not contain
  (`parent_id` is null for a root and for a withheld parent identically, and a dependent whose
  target the response dropped is dropped with it, server-side).
- Every route answers alike: an artifact withheld in the viewport is a `404` on the identifier
  route, and a gate-failed layer is indistinguishable from one never registered.

The serving predicate runs in one place, in one order, for every route: the artifact's own
**overlay disposition** (suppression/deletion — live, uncached, first), the **layer gate**, the
**dependency's verdict**, the artifact's own terms, the **criterion** on the masked count, then
**containment** for the rank. ⊘ The own-terms gate (`artifact_visibility.field`) has no
acquisition path yet; a layer declaring it withholds everything, fail-closed.

## 3. Declaring a layer

One corpus document declares everything; `tessera build` takes no flags. A `[[layer]]` block
carries the name, views, the two visibility axes (`visibility`: the access label a viewer must
hold; `require_member_visibility`: how much of the membership they must already see), the
hierarchy kind, and the membership source:

- **Enumerated** — a member table (`[layer.members]`), membership columns on the points file
  (a scalar key per point, or a list whose positions mean what the hierarchy kind declares),
  inline `artifacts`, or an `excluding` complement. `value_set = "open"` lets a key that names no
  artifact mint one; `"closed"` refuses it.
- **Attribute predicate** — `membership = { attribute = "<field>" }` names a single-valued
  indexed category column; the column's distinct values *are* the artifacts and every point
  carries its membership. Multi-valued, keyword and text columns are refused.
- **Spatial predicate** — `membership = "spatial"` with `[layer.shape] = { kind = "bbox",
  depth = d }`: each artifact's declared box is decomposed to Morton ranges at depth `d`, and
  **the ranges are the membership** — a point inside the box is a member by construction.

Content is declared per layer (`computed`, `[[layer.content.supplied]]` with its own
member-visibility requirement), labels have a sugar (`[layer.labels]`) that expands to a real
dependent layer, and a `layout =` key can pin the serving layout (§5). Predicate layers may not
declare supplied content, dependencies, or a proportional criterion. Every retired or unknown key
refuses; nothing is aliased or defaulted.

## 4. How it is stored

Two coordinate spaces carry everything. **Entity space** is permanent: memberships, generating
sets, content and edges are entity-addressed and survive every rebuild of the map. **Row space**
is per view and per generation: rows are Morton rank, so spatial locality is row locality, and a
tile is a contiguous row range. The permutation between them is rebuilt at every compaction fold.

Durably, a bundle (or a served generation) holds, per `(layer, level)`:

- the **membership extents** (`TSMB`) — the packed entity-space memberships, ordinal-addressed;
- supplied **content in the record blob**, at the artifact's entity, beside every point's
  blob-resident fields;
- the registered layer's declaration, its reserved entity runs, its per-level **version** — a
  counter moved by every publication, growth, removal and retirement — and its chosen
  **serving layout**, all in the manifest.

Beside those, a **derived family** is written at every fold *and* at the end of every build, and
opened by mapping, never by deserialising into anonymous memory:

| structure | file | what it answers |
|---|---|---|
| row forms | (recomposed) | per-artifact row-space membership, artifact-major levels |
| tile index + extents | `TSTI` | which artifacts a viewport can touch (§5) |
| containment partition | `TSCP` | interned `(artifact, rank)` term expressions |
| row-major columns | `TSLB`/`TSLL` | one label (or list) per row, plus declared sizes |

Every derived file's manifest entry carries the coordinate it was composed at —
`(layer, level, level_version)`, plus the view for row-space structures — and is adopted at open
**only on exact equality**; anything else recomposes lazily. A version the manifest does not
carry is a version it does not claim; nothing defaults. The build and the fold write these files
through one implementation (`tessera-store::derived`), so there is no second transcription of any
format to disagree with the first.

## 5. How a request is served

A session presents a token; authorisation materialises the credential's **fragment** (the union of
its terms' postings — `M_auth` in entity space) and, per view and generation, its **row
projection**. The viewport request that serves points also serves artifacts: after the density
counts and before any point, the response carries an **artifacts frame** (kind 5) with each served
artifact's identifier, masked count, contents, and parent-in-this-response.

Per requested layer and level, the route is the level's recorded layout:

- **Artifact-major** (the default): the **tile index** — a hierarchy of row-range nodes in which
  each node holds the artifacts whose whole membership lies inside it — is walked top-down at the
  viewport's perimeter, classifying artifacts as *settled* (membership ⊆ viewport), *open*
  (straddling the edge), or *everywhere* (too wide for any node). Geometry only chooses which
  question to ask: **every candidate then pays a masked probe** — for a settled artifact one
  early-exiting intersect against `viewport ∩ M_auth` (exact, because membership ⊆ viewport),
  for the rest the full masked intersects. Counts come from the row form.
- **Row-major** (levels whose artifacts are everywhere — chosen when the *everywhere fraction*
  crosses a ⊘ provisional threshold, or pinned): candidacy is one scan of `viewport ∩ M_auth`
  over a label-per-row (or list-per-row) column; counts come from a per-`(session, layer)`
  **masked-count histogram** — the one structure held per session over an artifact population,
  byte-budgeted, keyed on the overlay version so a suppression is unreachable at the next
  request. The membership never exists per artifact; the declared-size column beside it carries
  the unmasked denominators.
- **Spatial ranges**: candidacy is range-versus-tile arithmetic and the count is
  `count_range` summed over the artifact's ranges; no row is walked.

Containment resolves through the **partition**: each `(artifact, rank)`'s generating set composes
at build/fold time to a canonical expression over its members' term signatures, interned and
shared; a request answers by expression, corrected live against the overlay
(`deleted ∪ suppressed` members defeat containment the moment the deny is acknowledged). The
partition exists only under the builtin authorisation plugin — any other plugin disables it and
containment falls back to the masked-count test, loudly. Expression sharing is an authoring
property: per-term generating sets compose to a handful of expressions; sets drawn across
signatures compose to roughly one each, and such levels answer per candidate,
viewport-bounded.

After the verdicts, the **cut**: the request's `artifact_budget` is met by climbing to *passing*
ancestors — never by sampling, never by revealing anything that failed its own test — via a
downward walk from the roots when everything passes and a sweep otherwise. Dependents whose
targets the cut removed are dropped, chains cascading; then parents resolve, null where absent.
Drill-down (`POST /v1/artifacts/{tessera_id}`) runs the same predicate and answers one `404` for
every withheld case.

**Caches and their cadences**, which is most of what an operator needs to reason about cost:

| held | keyed / invalidated by |
|---|---|
| fragment | per credential; dictionary/watermark |
| row projection | per (session, view); every geometry publication |
| row forms, tile index, lineage, partition | per (layer, level) version — one write moves one level in one store; the fold's new prefix moves all |
| count histogram | per (session, layer); overlay version, level version, geometry |

Nothing else is held per session, and nothing anywhere is sized by the artifact population per
token except the histogram, which exists only for row-major levels.

## 6. What a client sees and does

`/v1/meta` lists the layers this viewer's gate admits — identity, structure, levels, declared
content kinds; never an artifact count. The viewport request takes a `layers` selector (narrows,
never widens) and an `artifact_budget`; the artifacts frame returns identifiers, masked counts,
ranked content (first satisfied rank only), and `parent_id` where the parent is in the same
response. A client **selects requests and never filters responses** — trimming to fit is the
server's job, because what is visible is the server's decision — and caches strictly by
credential and version coordinate, never loosely.

Two interaction models fall out of the machinery without a serving mode:

- **Small layers are fetched whole** — a whole-map request once per (session, generation),
  cached client-side, so panning costs the server nothing; the verdict state lives with the
  client, which is why nothing needs holding per token server-side.
- **Large layers are fetched as the budgeted coarse cut** at whole map — pan-stable at that
  zoom — and refined per viewport on zoom-in.

⊘ A wide response over a large flat layer is currently unbounded (a quarter-gigabyte is
expressible); streamed continuation is the anticipated shape, owned by the client epic. ⊘ Nothing
in `/v1/meta` yet tells a client which model a layer suits.

## 7. Coupling to build and ingest

**Build is ingest into an empty database.** The build registers layers and publishes artifacts
through the same registry, allocator and publication the control plane uses, so a declaration
refused online is refused at build with the same words, and no id is ever reissued between the
two. What differs is acquisition, not capability: the build reads member tables and points
columns in bulk; ingest receives the same values on the wire.

At **ingest**, a batch may carry a column named for a layer — a key or list per point, exactly a
member table's values. Keys resolve at admission (one caller's typo refuses that batch alone);
the joins become growth records inside the batch's own fsync, so **there is no state in which a
point is ingested and its membership is not**. Under `value_set = "open"`, a key naming nothing
**mints** its artifact at the commit window's close — where the ordinal cursor is safe against a
concurrent publication — and the batch is told what it minted. An attribute layer's novel value
mints the same way; a publication *into* a predicate layer is refused, because its membership is
the rule.

The **deny lane** is the same one points use, with the same two removal rules: a suppression
applies at the acknowledgement and retires only on unsuppress; a deletion applies at the
acknowledgement and retires only at the fold that executes it. A deleted generating-set member
defeats containment immediately; a deleted membership member leaves every count at the
acknowledgement; a dependent of a deleted artifact is deleted with it, transitively.

At the **end of a build**, a post-bundle artifact pass observes every level against the bundle's
own row space, reports its shape (blocks per artifact, everywhere fraction), chooses each level's
layout, and writes the derived family — so a fresh bundle's first request adopts mapped files and
composes nothing. The build refuses up front, with the numbers named, when a footer-derived lower
bound on its own peak memory exceeds the declared budget.

## 8. Coupling to the map lifecycle

The artifact system rides the map's write cycle rather than having one of its own.

- A **flush** publishes buffered points into extent rows. An enumerated membership's new member
  has joined durably at admission but **understates the count until the fold** — the row forms
  hold base rows only, which is fail-closed and matches a buffered point's own invisibility.
  A **predicate** level counts the new point at its flush: the attribute route's live tail and
  the spatial route's ranges read the geometry the flush published.
- A **merge** renumbers only extent rows; no artifact structure is keyed on it.
- The **fold** (compaction) is where everything reconciles: memberships are rewritten into the
  new prefix minus what Rule F retires, layouts are re-evaluated from observed shape (a pinned
  layout never flips), the whole derived family is rewritten through the shared implementation,
  the log pin a growth held is released, and the report sweep names what every deletion degraded
  before anything retires. Serving continues through it: the fold under a live session load runs
  at its unloaded duration (measured 0.98×), with a latency spike for in-flight requests during
  the flip and nothing shed silently — a stream shed, if one ever occurs, is logged with its
  cause.
- On **restart**, the manifest's per-level versions seed the store before WAL replay; derived
  files whose coordinates match exactly are adopted, and **every live level's row form is then
  composed at open, before the server binds a listener** — with its lineage and its supplied
  contents beside it, none of which depends on a principal
  (`Engine::warm_artifact_projections`). Nothing here is per request: a lazy build put rung 3's
  1.66×10⁹-row membership — a *measured* 23.3 s
  ([the cold-start probe](../../probes/2026-09-02-cold-start/)) — on whichever request of a fresh
  process arrived first, and a demo restarts often. The cost moves to the start; it does not go
  away, and the open reports it.

## 9. The operating envelope

Justification-level figures, all measured on the campaign fixtures
([the campaign](../evidence/memos/2026-08-22-artifact-scale-campaign.md) holds the detail):

- Serving is census-exact — 98 of 98 cells across three corpus sizes agree with a closed-form
  oracle, every artifact, both directions, over HTTP.
- Worst measured request cells: ~140–220 ms at 10⁶ artifacts (10⁸–10⁹ points), ~900 ms at the
  10⁷-artifact probe; narrow principals run sub-millisecond, and cost tracks what a viewer may
  see rather than inverting against it.
- Session cost: the fragment (~hundreds of ms once per credential at 10⁹) and the row projection;
  ~76–220 MB per session at 10⁷–5×10⁷ points, which is what binds concurrency before CPU does.
- The fold's artifact pass threads and runs unimpaired under load; a level's write invalidates
  one level's structures, not every view's.
- ⊘ The 10⁹-point end-to-end scenario is modelled, not measured — its build does not fit the
  development machine — and the row-major residency advantage at that scale (4 GB against ~78 GB
  derived) is not yet taken, the artifact-major row form still being built alongside.

## 10. What is not built

So that absence is read as absence, not assumed: **runtime artifacts** (analyst selections and
the edit verb), **search and filters over artifacts** (membership-as-filter, text search),
the **own-terms acquisition** for `artifact_visibility.field`, **polygon spatial shapes** (bbox
only), **computed content on predicate layers**, **streamed continuation** for wide responses,
the **proportional criterion on predicate layers** (its denominator is unruled), and any
**cross-partition** behaviour (one hardcoded partition exists). The layout thresholds are
provisional pending a two-sided fixture. Each is recorded in the delivery record with its stage.
