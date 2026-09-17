# The Python SDK: a Tessera database in a notebook

**Status:** Provisional — under review. Reviewed once under the user-experience and capability
lenses and re-reviewed on §3, §4, §6 and §12 after the rulings of §11.1. Before this becomes
normative: the owner's rulings on §11.2, and the examples in §10 run against the arXiv corpus. Nothing in this document is built except the
widget `Map` (client-components §7), which it uses unchanged.

**Owns:** the `tesseradb` package's verbs for creating, filling and reading a Tessera database,
and the shape of a local instance. It does not own the wire (contracts §3), the ingest model
(ingest.md), the declaration (configuration.md) or the widget's protocol (client-components §7).
Where it restates one of those it cites it; where they disagree, they win.

## 1. Who this serves, and what it does not

A data scientist with a table of points and something to say about their structure: a
DataFrame or a parquet file, coordinates from a projection, cluster labels, some text and
attributes. They want to see the map in the notebook they are working in, filter it, and go on
working with the data. The comparison set is datamapplot and embedding-atlas: a few lines from a
frame to an interactive map, in Jupyter or marimo.

The same person, later, wants the map served to other people, each seeing the items their
terms allow. Tessera's model is per-viewer aggregates over an authorised mask, and the local
case keeps that model rather than bypassing it: a local user holds every term, so they see
everything, and they can view the map as any principal to see what a viewer with fewer terms
would get.

The SDK covers three things:

- **A local database.** Created in a directory, filled from frames and files, served by a
  child process of the `tessera` binary, read through the widget and through query verbs.
- **Reading a hosted deployment.** `connect(url, token)` gives the same widget and the same
  read verbs against a deployment somebody else runs.
- **One lifecycle for the first load and every load after it.** Decision 0091 says a build is
  ingest into an empty database. The SDK has one set of verbs, and the first commit builds where
  later commits ingest.

The goal is the whole declaration: every block and key of configuration.md §1 has a verb
parameter, and every corpus declaration under `test_corpora/` can be regenerated through the
SDK. §12 stages the implementation; the stages are the order of building, not a narrower goal.

It does not cover:

- **Writing to a hosted deployment from a notebook.** The control plane has one operator
  credential and no per-principal authority, so an analyst publishing into a shared deployment
  is a write-authorisation question the system has not designed. That work precedes any hosted
  write verb.
- **The interactive surface of the map** beyond what the widget has today: what a selection
  returns, how it joins back to the user's frame, publishing a drawn region as an artifact.
  These take a design pass of their own.
- **Linking the engine into the kernel.** Issue #51 opens with a ruling on which guarantees
  survive the boundary's removal. A child process keeps the boundary, and the browser fetches
  tiles over HTTP either way, so embedding buys the notebook nothing.
- **A static export.** Tessera is served; a file with no server behind it is a different product.

## 2. The model

A database is a directory:

```
<dir>/
  tessera.toml        the deployment file (system-architecture §7), written by the SDK
  schema.toml         the declaration (configuration.md), written by the SDK
  sources/            parquet files the SDK wrote from frames; paths staged from files are read in place
  bundle/             what the build wrote
  .tessera/           cache, WAL, the SDK's JSON copy of the declaration
```

Everything in it is what `tessera build` and `tessera serve` read. `tessera serve --deployment
<dir>/tessera.toml` on another machine serves the same database, which is how a notebook
prototype becomes a deployment.

```python
db = td.create(path=None, replace=False)
db = td.open(path)
```

The SDK keeps its own copy of the declaration as JSON under `.tessera/`, written at every
`declare_*`, so `open()` reads the blocks back without parsing TOML and a saved database that has
not committed yet reopens in the Declared state.

`create()` with no path makes a temporary directory that `close()` removes, on a RAM-backed
filesystem where the platform has one (`/dev/shm` on Linux and WSL2) and on disk otherwise, and
says which. The build reads and the server maps that directory, so a small corpus on the
RAM-backed path touches no disk; the engine itself has no in-memory backend and this document
does not ask for one. `save(path)` copies a temporary database out. `create(path)` on a directory that is not empty is refused naming `open()` and
`replace=True`, which removes what is there first. `open(path)` opens a saved database, serves
it, and its next `commit()` ingests, the bundle being present.

One lifecycle:

```mermaid
stateDiagram-v2
    [*] --> Declared: create()
    Declared --> Declared: stage(), declare_*()
    Declared --> Built: commit()  [check, build, serve]
    Built --> Built: stage(), declare_*(), commit()  [pages through the control plane, flush]
    Built --> [*]: close()
    [*] --> Built: open()
```

*The first commit builds; every later one ingests. The verbs before and after are the same.*

Three verbs carry the model. `stage(name, data)` binds a named source to a frame or a file.
`declare_*` adds a block to the declaration and names the sources it reads. `commit()` makes the
staged data part of the database: through the build the first time, through the control plane
after. `check()` is `commit()` with nothing sent.

## 3. Sources and identity

```python
db.stage(name, data, id=None, default=False)
```

`name` is a key in the declaration's `[sources]` table, and every declaration block that reads
data names one. `data` is a pandas or polars frame, a pyarrow table, or a path. A frame is
written to `sources/<name>.parquet`; a path is recorded and read in place, so a large file is
not copied. Staging a name twice before the first commit replaces the earlier data.

`default=True` makes this source the one a declaration block that names no source reads. The
SDK fills that source onto every such block when it writes the declaration and never writes
`source` under `[defaults]` (the block carries `allocation_view` alone, §4.2), so the written
TOML is fully explicit and has one shape before and after the first commit; an attribute
declared after the first commit names no source, since nothing reads it from a file. Without a
default, a block with no source is refused at `check()` naming the block. A second `default=True` replaces
the first and the call says so.

**Identity.** A row is named one of two ways, and the SDK keeps no map between them.

- **An explicit id column** (`id=` on `stage`, or a column named `id`) is the external id at both
  doors: the build takes it as supplied, in any type, mints its own entity ids, writes the
  external-id index from it and joins members tables on it; the ingest route takes the same
  bytes as `external_id`. Members tables and attribute sources name rows by the same column.
  The SDK writes the column under the name `[defaults].entity_id_field` declares and names it
  on a members table through `[layer.members].fields = { entity = <column> }`.
- **No id column** means Tessera ids. The build mints no external id; rows are addressable only
  by the `tessera_id` a viewer gets back from a pick or a drill-down, the ingest route returns
  the ids it assigned, and `remove()` and a members table name them with `addressing: tessera`.

The SDK holds nothing about which rows the database has. A commit sends what was staged; a row
whose id the database holds is refused by the server, whole page, and the report says so. A
re-run of a cell is a re-run: databases are stateful, and the SDK does not make a second
`commit()` of the same frame silent.

After the first commit, `stage(name, data)` binds a delta: rows to add to what the source
already holds. The declaration says what the source feeds, so the SDK knows that a delta on the
points source feeds the view, the attributes and every layer minted from one of its columns,
and that a delta on a members source feeds one layer's memberships. A name the declaration
does not know is refused.

## 4. Declarations

Each `declare_*` verb adds one block of configuration.md's declaration. The SDK writes the
TOML and keeps it as `db.declaration`, and `tessera check` reads that file, so the mapping from
verb to block is checked by the binary rather than mirrored in Python.

### 4.1 The generic form

```python
db.declare(kind, block)
```

`kind` is `view`, `view_group`, `vocabulary`, `attribute` or `layer`, and `block` is the block
as a dict spelled with configuration.md's keys. The typed verbs below build this dict from
their parameters and call it. Every block is expressible through the generic form from the
first cut; a typed verb is the documented path, and the generic form is the escape hatch for a
key whose verb has not landed.

### 4.2 Views

```python
db.declare_view(name, source, x="x", y="y", access=None, default_label="public",
                extent=None, projection="none", visibility="public", anchor=False,
                title=None)
```

`x` and `y` name the coordinate columns on `source` (`lon` and `lat` under a projection);
`access` names a list-of-strings column of labels, one list per row, and `default_label` is
what a row with no labels takes (decision 0133). With no `access` column every row takes
`default_label`. `extent` is the frame: a box, a projection's domain, or `None`, which lets the
first commit fit one (§6.1). `visibility` is the view's own gate.

The first declared view is the allocation view (decision 0112) unless another says
`anchor=True`. The SDK writes `allocation_view` into the TOML in either case, so a rebuild
that reorders the blocks cannot re-key the corpus.

A second plain view over the same entities names a source carrying the same ids, a second
pair of coordinates, and the same access column: the build refuses an entity whose labels
disagree between views, and the ingest route refuses a join row whose labels differ from the
held ones (views.md §4). A frame for a second view that lacks the column is refused naming it;
the SDK copies nothing. A declaration form under which a second view's file needs only ids and
coordinates, its labels being the entity's, is a views.md question for later.

### 4.3 View groups

```python
db.declare_view_group(name, views=None, source=None, view_field=None, members=None,
                      metadata=None, access=None, default_label="public", extent=None,
                      projection=None, visibility="public", title=None)
```

`views` is form A, a list of `{key, source, visibility?, ...metadata}`, each view its own
source. `source` with `view_field` is one points source for every view, the column naming which
view a row belongs to; `members` names another group whose views this group shares. `metadata`
is the per-view values as `{name: type}`; `access` and `default_label` are the group's
`point_visibility`. Form B's roster as a table (`[view_group.views]`) has no parameter and is
declared through the generic form. A group-scoped attribute or layer names the group in
`scope` (§4.5, §4.6). views.md owns the semantics.

### 4.4 Vocabularies

```python
db.declare_vocabulary(name, width=None, closed=False, visibility="public", source=None,
                      values=None, reserved=None, fields=None, title=None)
```

A closed vocabulary names a `source` of `(key, title?)` rows or gives `values` inline; an open
one minted from the data needs neither and may name a source for titles. `width` is the code
space's width and is fixed at the first commit, so when not given the SDK takes one width above
what the distinct count needs, `u16` at least, and prints it: an open vocabulary at the width
its first values fill has no code for the next one. `visibility` is `public` or `derived`.

### 4.5 Attributes and inference

```python
db.declare_attribute(name, type, source=None, field=None, vocabulary=None, render=None,
                     index=None, analyser=None, scope="entity", fields=None, title=None)
```

An attribute joins by entity id and belongs to no view, so it reads the source it names or the
default source. `declare_view` claims id, x, y and access on its source, and a layer's
`from_column` claims its column; if that source is the default, every column not claimed
becomes an attribute by the rules below, and the SDK
prints the table once, stating each vocabulary it declared as open and public. An explicit
`declare_attribute` on the same name overrides the inferred block. On a source that is not the
default nothing is inferred.

| Column dtype | Declared as | `render` | `index` |
|---|---|---|---|
| integer, float, bool | the matching width | yes | yes |
| datetime | `timestamp_us` | yes | yes |
| string, few distinct values | `category` over an open, public vocabulary of the same name, width one above the count (§4.4) | yes | yes |
| string, many distinct values, short | `keyword` | no | yes |
| string, long | `text` | no (the build refuses it) | yes |
| list of strings | not inferred; declare it, or name it as `access` | | |
| anything else | not inferred; declare it | | |

"Few" and "short" are thresholds the SDK states in the printed table (assumed: at most 4,096
distinct values, and a median length under 64 characters). They decide a default; the user
overrides one column with one call.

An open public vocabulary publishes value names minted from the data to every principal, and
`tessera check` prints a warning per such vocabulary. On a local database the user is the
authority the warning asks for, so the SDK's table states the choice once and the build's
warning is not repeated per column. Issue #83 is open on serving `derived` visibility, under
which a value exists for a viewer only if they can see a point carrying it; when it serves,
`derived` becomes the inferred default.

`render` is decided here because it cannot be added later: a render column lives in the hot row
and a running service refuses to declare one (decision 0136's amendment). The commit report
lists the render columns it froze.

### 4.6 Layers

```python
db.declare_layer(name, kind, views=None, source=None, members=None, from_column=None,
                 artifacts=None, membership="enumerated", value_set=None, levels=None,
                 prune_children=False, shape=None, default_space="view", layout=None,
                 visibility="public", artifact_visibility="inherited",
                 require_member_visibility="none", withdraw_on_member_deletion=False,
                 depends_on=None, computed=("centroid", "box", "hull"), supplied=None,
                 scope="entity", fields=None, title=None)
```

`kind` is `flat`, `nested`, `stacked` or `tiered` (annotations.md), or `dag`
(dag-hierarchies.md); `levels` is a list of `(level, title, zoom?)` for the kinds that take
them. `views` defaults to every view. `membership` is `enumerated`, `spatial` (with `shape`
naming the kind and `default_space` the space) or `{"attribute": field}`. On a spatial layer
`computed` defaults to `("centroid", "box")` and `hull` is refused at the call: an artifact has
one drawn geometry, and a membership shape is it (polygon-membership.md §7.1). A group-scoped
layer takes `scope={"group": name}` with `fields={"view": column}`.

An enumerated layer's membership comes one of two ways, and the declaration fixes which:

- **From a column** (`from_column`): a column on the points source holding one key per row, or
  a list of one key per level for a tiered layer. The build and the ingest route both mint the
  artifacts from it (decision 0128). Such a layer has computed content only, and the SDK writes
  `value_set = "open"`, without which a key the layer's artifacts do not declare is refused. At
  the first commit it compiles to `[layer.members]` reading the points source with the column
  as `key`; declared after it, the SDK writes the column as a members table under `sources/`,
  since `tessera check` reads the declaration against the files each time and a file read in
  place does not carry the new column.
- **From tables** (`source` and `members`, or `artifacts` inline): an artifacts table
  `(level, key, parent?, contents?, attached_layer?, attached_level?, attached_key?, space?,
  a shape column?, excluding?)` and a members table `(level, key, rank?, entity)`, the shapes
  under `data/notebook/`. `members` with no `source` and no `artifacts` declares a layer whose
  artifacts are exactly the keys the members carry, and the SDK writes `open`. This is the only
  way for a layer with supplied content, since an artifact served without content its layer
  declares cannot be told from one whose content was withheld.

`supplied` lists the content kinds an artifacts table's `contents` column carries, as
`(name, type, gate)` with `gate` `all` or `inherited`; `computed` lists what the engine derives
per viewer. `visibility`, `artifact_visibility` and `require_member_visibility` are the layer's
disclosure controls (configuration.md); `artifact_visibility` is a default, or `{"field":
column, "default": label}` reading a column of the artifacts table. The local defaults are public, inherited and none, and
the commit report prints them, because a local user holds every term and a deployment author
sets them. `value_set` is printed whenever the SDK chose it, since under `open` a mistyped key
is a permanent artifact.

### 4.7 Labels

```python
db.declare_labels(name, of, source, members=None, content_requires=None, type="text",
                  require_member_visibility="none", artifact_visibility="inherited",
                  title=None)
```

A label set over a clustering: the `[layer.labels]` block, which expands to a flat layer of
supplied content depending on `of`. `source` is `(key, contents)` rows or a mapping from
cluster key to text. `content_requires` is the content-grain gate and takes `all` or
`inherited`: `all` means the text was generated from the members in `members`, the generating
set `(key, rank, entity)`, and is served only to a viewer who can see every one of them;
`inherited` means the label is served wherever its cluster is and declares no generating set.
The default follows `members`: `all` when given, `inherited` when not, and the other pairings
are refused at the call. `require_member_visibility` is the layer grain, how much of a label's
membership a viewer must see before the label exists for them.

### 4.8 What the TOML always says

The SDK writes every source name on every block, the allocation view, `value_set` on every
layer, and the disclosure controls on every layer and vocabulary, whether the user said them or
the defaults did. A reader of `schema.toml` sees the whole declaration without knowing the SDK's
defaults.

## 5. `check()`

Before the first commit, `check()` runs `tessera check` on the directory and returns its
table: what the declaration reads from each file, and the disclosure decisions it makes.
Schemas only, no rows.

After the first commit, `check()` plans the commit (§6.2) and runs the pre-flight (§6.3) and
returns the plan and the pre-flight findings with nothing sent. Both return a report object
that prints as a table.

## 6. `commit()`

### 6.1 The first commit

Runs `tessera check`, then `tessera build --mint-external-ids`, then starts `tessera serve`
(§7), and returns the build's report. Three things happen at the first commit and at no later
one, and the report says each:

- **The frame is fixed.** A view with `extent=None` has its frame fitted to its staged rows,
  widened by half the fitted box's width on each side (assumed default; the report prints the
  frame). A frame is index configuration and does not change for the life of the view. The
  build clamps a row outside the frame and reports the count (configuration.md §1); ingest
  refuses it (write-path §2.1 step 9). A user who will add rows later declares `extent`, and
  the refusal message names `extent=`.
- **The column types and render flags are fixed.** A later `declare_attribute(render=True)`
  is refused with decision 0136's wording; an indexed column can be added at any time.
- **The allocation is signature-sorted** over the whole staged corpus, which ingest does not do.
  This affects posting compression and latency (ingest.md §5), never what is served.

A first commit with no points staged builds an empty database, which needs an explicit
`extent` on every view; the SDK refuses it otherwise, naming the view.

### 6.2 Later commits: the plan

A part supplied twice is accepted, a part supplied differently is a `409` on that part, and a
set grows by the delta (ingest.md §1.1). Ordering therefore matters only for existence, and
the order is fixed:

1. **Declarations** added since the last commit, as the runtime `PUT`s (ingest.md §1.3): view
   groups, then a `members` group after the group it names, roster views, plain views, layers
   and label sets, each body from `tessera check --payloads` (configuration.md §2) except the
   roster record, which the SDK builds; vocabularies before the attributes that name them, both
   before any values page. A closed vocabulary's body carries its first page of values, since the
   route refuses a closed set with none, and the rest follow as `PATCH` pages sized by rows and by
   the route's body cap; a sourced set's `(key, title?)` rows are paged the same way. A vocabulary
   no column names yet is not on `/v1/meta`, so it is redeclared at each commit and the route
   answers it as held. A `render` column is refused at the verb after the first commit (decision
   0136's amendment).
2. **Points**, per view, the allocation view first, in pages under the limits `/control/status`
   publishes, with `x-tessera-view` on each. A from-column layer's key travels with a new row
   and mints its artifact at the window close. A delta on a second view's source carries ids,
   coordinates and the entity's held labels, which the SDK staged (§4.2), and joins existing
   entities; a join row with different labels is refused as a re-label.
3. **Values** on existing entities, one page sequence per staged attribute delta
   (`POST /control/values`). A from-column layer declared after the first commit is refused
   naming the artifacts-table route, because the values route fills a column and mints no
   artifact; the SDK does not turn a column into publish requests. Whether the values route
   should read a layer column as the ingest route does (decision 0128) is an engine question,
   §11.2.
4. **Artifacts**, per layer in dependency order: a clustering before its labels, a target before
   a layer attached to it, a layer before one that depends on it. Within a layer, `PUT` pages
   carry members, parent, shape and content; a nested batch resolves parents that are its own
   siblings, and a tiered chain goes coarse level first. Content gated `all` travels on the
   publish record with the first page of its generating set, since the route refuses a content
   fill on such a layer; further set pages are `PATCH` at the rank. A held key falls under the
   fill rule: a members delta is a `PATCH` join, and content gated `inherited` is filled by
   `PATCH`. Not built yet: filling `all`-gated content onto an artifact published without it has
   no route; the plan refuses that case and names the artifact.
5. **Flush** (`POST /control/flush?wait=visible`): the pages went unwaited, and one flush carrying
   the wait returns once the publication its number names has happened (decision 0144), so the
   next cell sees the rows; the report's flush time is that wait, and `visible: false` past the
   server's bound is a finding.

The commit returns a report: rows accepted per view, artifacts minted, memberships joined, parts
refusals by row and part, and the flush time.

### 6.3 Pre-flight

Before a byte is sent, against the declaration and `/v1/meta`, `check()` and `commit()` report:

| Finding | Report |
|---|---|
| rows outside a view's frame | listed with the frame; the server refuses the page |
| rows with no id where the declaration names an id column | listed |
| a column no block declares | named |
| a key column staged for a layer that declares supplied content | named, with the artifacts-table route as the remedy |
| a labels delta whose clustering is neither held nor staged | named |

`check()` reports and sends nothing. `commit()` reports and refuses to send while a finding
stands, naming it; nothing is dropped or rewritten. The user corrects the data or the
declaration and commits again.

### 6.4 Retries

A page is sent with a batch id derived from the source name, the page index and a hash of its
bytes. A `429` is retried after its `Retry-After` with identical bytes, and a lost
acknowledgement is resent the same way within the WAL retention window, where the server
answers it as a replay (write-path §2.4). A `409` on a differing part is reported per row and
part and not retried: an edit is refused by the server, and the SDK has no edit verb. The SDK
keeps no log of what it sent.

### 6.5 Verbs that are not stages

`remove(ids)`, `suppress(ids)` and `unsuppress(ids)` go to `/control/changes` under the two
removal rules (write-path §5.4). `leave(layer, key, ids, rank)` shrinks a generating set, the one
set that may (decision 0135). Each takes the user's ids and maps them.

## 7. The local instance

`commit()` starts `tessera serve` as a child of the kernel over the directory's `tessera.toml`,
which the SDK writes with:

- `[serve]` viewer, session and control on loopback, port 0 each. Not built yet: `serve` binds
  the addresses the file names and announces nothing; it needs to accept port 0 and print the
  three bound addresses as one JSON line on stdout, which the SDK reads (§11.2 A).
- A session credential and an identity key, generated per database and stored in the directory
  with owner-only permissions, named by the `_file` and `env` keys the deployment file takes.
- `[disclosure] token_max_lifetime`, required, at one hour.
- `serve.cors_loopback = true` (configuration.md): the notebook page's origin is a port the
  front end chose, which no list can name, and the rule admits any page served from a loopback
  address to present a token on the viewer plane.

The binary is `TESSERA_BIN` when set, else the first `tessera` on `PATH`, else a checkout's
target directory, release before debug; the create report names the one used. At release a
platform wheel carries it (§11.2 E). The SDK generates an operator credential beside the
session credential, since the deployment file requires both.

`db.close()` stops the child; the kernel's exit does the same.

## 8. Reading

```python
db.map(view=None, layers=None, colour_by=None, filters=None, height=480)
db.viewer(terms).map(...)
v = td.connect(url, token); v.map(...)
```

`map()` is the widget of client-components §7, unchanged: control and selection cross the
kernel boundary, data does not. On a local database the token is minted by the SDK from the
directory's session credential through the passthrough plugin, which grants the terms listed.
The SDK lists every term it has seen: at each commit it records the distinct labels of every
access column it staged, plus each view's default label, in its log, and `map()` mints with
that set. A database with no access column has one term, the default label, and the union is
that. This is Python asserting the local principal's authority, which is admissible on a
single-operator database and nowhere else. `viewer(terms)` mints for the named terms, so the
map of any principal is one call. `connect(url, token)` takes a token the deployment issued and
has no `viewer(terms)`.

`viewer(terms)` refuses a term outside the union the SDK recorded, naming it and listing the
union, and refuses an empty set, since either is the blank map the verb exists to prevent. A
`Token` carries the terms it was minted for and never prints its bearer string. `close()` stops
the child and invalidates nothing on the server; a token stays valid for its lifetime.

The read verbs, a first cut of issue #47, on a `Viewer` and so on a `Database` through its
all-terms viewer, each through the viewer plane with a token and never by reading the bundle:

- `meta()`: the parsed `/v1/meta`.
- `item(tessera_id, idset=None)`: the drill-down record, its `external_id` decoded to the
  staged id column's type on a `Database` and to bytes on a `connect()` viewer, which knows no
  declaration.
- `viewport(bbox=None, view=None, filters=None, k=None, zoom=0)`: the served points frames as one
  pyarrow table whose schema metadata carries the tiles frame's `visible`, `matched` and `served`
  counts, the trailer and the request, so a served set is never mistaken for the whole. `zoom` is a
  coordinate of the request that `map()` chooses and a verb cannot. `artifacts(...)` is not built.

## 9. What the first commit fixes, and what it does not

| Fixed at the first commit | Open at any commit |
|---|---|
| a view's frame and projection | rows, in any view |
| a column's type | values on existing rows |
| which columns render | indexed attributes, vocabularies, values |
| the allocation view | layers, artifacts, memberships, labels; a new clustering over existing rows |
| | plain views and view groups' views, with a stated extent |
| | deletions and suppressions |

## 10. Examples

### 10.1 Coordinates, cluster labels, hover text

```python
import tesseradb as td

db = td.create()
db.stage("points", df, default=True)          # index as id; x, y, title, year, cluster
db.declare_view("map", source="points")
db.declare_layer("clusters", kind="flat", from_column="cluster")
db.declare_labels("topics", of="clusters", source=topic_names)   # {cluster_key: text}; inherited
db.commit()
db.map(colour_by="cluster:clusters")
```

### 10.2 The arXiv corpus from files, with terms and three layers

The declaration under `data/notebook/schema.toml`, as calls. The files carry `entity_id`, so
they are read in place.

```python
db = td.create(path="~/tessera/arxiv")
db.stage("points", "points.parquet", default=True)
for name in ["kmeans", "kmeans_members", "kmeans_topics", "kmeans_topic_members",
             "hdbscan", "hdbscan_members", "hdbscan_topics", "hdbscan_topic_members",
             "taxonomy", "taxonomy_members", "archive", "primary_category"]:
    db.stage(name, f"{name}.parquet")

db.declare_view("s0", source="points", access="categories", title="arXiv, 50,000 papers")
db.declare_vocabulary("archive", source="archive", closed=True, width="u8")
db.declare_vocabulary("primary_category", source="primary_category", closed=True, width="u16")
db.declare_attribute("archive", type="category", vocabulary="archive", render=True, index=True)
db.declare_attribute("primary_category", type="category", vocabulary="primary_category",
                     render=True, index=True)
db.declare_attribute("submitted_at", type="timestamp_us", render=True)
db.declare_attribute("title", type="text", index=True)
db.declare_attribute("abstract", type="text", index=True)
db.declare_attribute("arxiv_id", type="keyword", index=True)

db.declare_layer("clusters/kmeans", kind="flat", source="kmeans", members="kmeans_members",
                 require_member_visibility={"count": 50})
db.declare_labels("topics/kmeans", of="clusters/kmeans", source="kmeans_topics",
                  members="kmeans_topic_members", content_requires="all")
db.declare_layer("clusters/hdbscan", kind="nested", source="hdbscan", members="hdbscan_members",
                 require_member_visibility={"fraction": 0.05})
db.declare_labels("topics/hdbscan", of="clusters/hdbscan", source="hdbscan_topics",
                  members="hdbscan_topic_members", content_requires="all")
db.declare_layer("taxonomy/arxiv", kind="tiered", source="taxonomy", members="taxonomy_members",
                 levels=[(0, "archive"), (1, "subject class")],
                 require_member_visibility={"count": 1}, computed=("centroid", "box"))

print(db.check())
db.commit()
print(db.declaration)

db.map()
db.viewer(terms=["math.AG"]).map()
db.viewer(terms=["cs.LG", "stat.ML"]).map(layers=["clusters/hdbscan"])
```

### 10.3 A week of new papers into the running database

```python
db.stage("points", new_df)                       # a delta of papers
db.stage("kmeans", pd.DataFrame({"level": [0], "key": ["k-new"]}))
db.stage("kmeans_members", members_of_k_new)     # (level, key, entity)
db.stage("kmeans_topics", pd.DataFrame({"level": [0], "key": ["k-new"],
                                        "contents": [["Diffusion models for audio"]]}))
db.stage("kmeans_topic_members", generating_rows)   # (level, key, rank, entity)
print(db.check())     # the plan and the pre-flight
db.commit()           # the report
```

### 10.4 A second clustering over the same points

```python
db.stage("points", df.assign(cluster2=labels2)[["id", "cluster2"]], id="id")   # held ids, one new column
db.declare_layer("clusters/second", kind="flat", from_column="cluster2")
db.commit()           # published as artifacts with members, one per key
db.map(colour_by="cluster:clusters/second")
```

### 10.5 Two projections over one set of papers

```python
db.stage("points", df_knn, id="arxiv_id", default=True)
db.stage("points_pca", df_pca, id="arxiv_id")    # arxiv_id, x, y, categories: the same labels, or refused
db.declare_view("knn",   source="points",     access="categories")
db.declare_view("pca64", source="points_pca", access="categories")
db.declare_layer("clusters/kmeans", kind="flat", from_column="cluster", views=["knn", "pca64"])
db.commit()
db.map(view="pca64")
```

### 10.6 A hosted deployment

```python
v = td.connect("https://tessera.example/viewer", token=my_token)
v.map(colour_by="cluster:clusters/kmeans")
```

### 10.7 Keep it, reopen it, serve it elsewhere

```python
db.save("~/tessera/arxiv")
db = td.open("~/tessera/arxiv")
# elsewhere:  tessera serve --deployment ~/tessera/arxiv/tessera.toml
```

## 11. Rulings

### 11.1 Made

Owner rulings on the first review's findings, 2026-09-16:

- `create(path)` refuses a non-empty directory; `replace=True` and `open()` are the two ways in
  (§2).
- A label's content gate is `all` or `inherited`; `none` at that grain would mean `inherited`
  (§4.7).
- Inferred vocabularies are open and public; the SDK states it once and does not repeat the
  build's warning per column; `derived` becomes the default when #83 serves it (§4.5).
- The local principal's terms are the union the SDK recorded at commit; an all-terms claim in
  the plugin is not asked for and may be reopened, since some users want no access control at
  all (§8).
- `declare_layer` carries `supplied` and `value_set`; from-column and members-only layers are
  written `open` (§4.6).
- A from-column delta over existing rows is published as artifacts; `all`-gated content travels
  with its first generating-set page (§6.2).
- The goal is the whole declaration surface, staged in §12, with `declare(kind, block)` as the
  generic form every typed verb compiles to (§1, §4.1).

Rulings of 2026-09-17, on the first stages' review findings:

- The SDK keeps no id map and no commit log: a row is named by an explicit id column or by its
  Tessera id, and a re-run is a re-run (§3, §6.4).
- The pre-flight reports and sends nothing until the finding is fixed; it never drops a row (§6.3).
- Edits are refused by the server and the SDK has no edit verb.
- A second view's frame carries the access column or is refused (§4.2).
- `serve.cors_loopback` is granted (§7); `tessera check` accepts a declaration whose attribute
  or view group names no source, as a note (§6.2 step 1).
- A write is visible at a numbered publication, every acknowledgement names it, and a client may
  wait for it (decision 0144).

### 11.2 Needed

- **D. The demo.** A marimo notebook and a Jupyter twin over the arXiv 50k corpus running
  §10.1 to §10.5 and §10.7, in `clients/py/examples/`. A headless test in `clients/py/check.sh`
  that creates, commits and queries a database with no browser.
- **E. The binary at release.** Platform wheels carrying it. Until then `PATH`, `TESSERA_BIN`
  or the checkout.
- **F. The values route and layer columns.** Whether `POST /control/values` reads a layer
  column and mints artifacts as the ingest route does (decision 0128), so a new clustering
  over held rows can be staged as a column after the first commit. Until ruled, refused (§6.2).
- **G. Issues #150 to #153**, engine and build defects the SDK's tests found; the SDK's second
  publication wait comes out with #153.

## 12. Order of work

Each stage is proven the same way: the SDK regenerates a corpus declaration from calls, and
`tessera check` over the regenerated file prints the disclosure table the committed file
prints. The stages order the building; the goal is all of them.

| Stage | Delivers | Proven by | Depends on |
|---|---|---|---|
| S1 the notebook corpus | `create`, `open`, `stage`, `declare` and the typed verbs for plain views, vocabularies, attributes, enumerated layers of every kind with supplied content and value sets, labels; inference; `check()`; the first commit with mint, serve, `save`, `close` | `data/notebook/schema.toml`, `test_corpora/arxiv` | A |
| S2 reading | `map()`, `viewer(terms)`, the term union, `connect(url, token)` | the widget's tests | S1, B |
| S3 pages | later commits: the plan, the pre-flight, batch ids, the flush wait, the report, layers and labels declared after the first commit; `remove`, `suppress`, `unsuppress`, `leave` | §10.3 and §10.4 against the served answers | S1 |
| S4 the rest of the layer surface | spatial and attribute membership, shapes and spaces, per-level zoom, prune, attached and dependent layers, inline artifacts and values, exclusion | `overture`, `geonames`, `gbif`, `treeoflife`, `medcpt`, `paperseek` | S1 |
| S5 groups | `declare_view_group`, `add_view`, scoped attributes and layers, `view` on the record; views and groups after the first commit | `multiview` | S1, S3 |
| S6 runtime declarations | attributes and vocabularies declared after the first commit | the served filter and category listing | S3 |
| S7 demo | the two notebooks and the headless test | | S2, S3 |
| S8 simplification | the id map, the commit log and its digests, the access-column copy, the from-column publish and the held-row logic removed; the pre-flight reports and sends nothing; the wait on the publication counter | every existing test, rewritten to the stateful reading | A, the publication signal |

The Rust changes (A, B, C) are small and sit in the server and the CLI; the engine is untouched.
