# The Python SDK: a Tessera database in a notebook

**Status:** Provisional — under review. Reviewed once under the user-experience and capability
lenses, re-reviewed on §3, §4, §6 and §12 after the rulings of §11.1, and built through every
stage of §12 by 2026-09-18, each stage refereed before merge, the verbs re-cut on 2026-09-18
(§11.1) and rebuilt; the examples of §10 run as the demo notebooks under `clients/py/examples/`
and a headless test asserts what each serves. Before this becomes normative: the rulings still
open in §11.2, and a pass to make the text describe the package as built where it still
describes the plan.

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
  sources/            parquet files the SDK wrote from frames; paths inserted from files are read in place
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
    Declared --> Declared: declare_*(), insert()
    Declared --> Built: commit()  [check, build, serve]
    Built --> Built: declare_*(), insert(), commit()  [pages through the control plane, flush]
    Built --> [*]: close()
    [*] --> Built: open()
```

*The first commit builds; every later one ingests. The verbs before and after are the same.*

Three verbs carry the model, and each does one thing. `declare_*` says what exists: a view, an
attribute, a layer, a label set, a vocabulary, with its types and its gates, and takes no data.
`insert(target, table, ...)` hands a table to a declared thing and names the columns it needs.
`commit()` sends what has been inserted since the last commit and forgets it: the first time
through the build, after that through the control plane. A user who wants more data runs the
same `insert` calls again. `check()` is `commit()` with nothing sent.

## 3. Inserting, and identity

```python
db.insert(target, table, **columns)
```

`target` is the name of a declared view, view group, attribute, layer, label set or vocabulary;
`table` is a pandas or polars frame, a pyarrow table, or a path to a parquet file, which is
read in place. Every column the target needs is named on the call, as the notebook-widget
libraries name `x=` and `y=`, and a column the call does not name is ignored, so a frame with
more columns than the target reads is the ordinary case. The SDK guesses no column name and
rewrites no data: a frame is written to `sources/` as it was given and the declaration names
the columns. Every insert prints two lists, the columns it read and the columns it ignored.

| Target | Columns named on the call |
|---|---|
| a view | `id=`; `x=`, `y=` (or `lon=`, `lat=` under a projection); `access=`, a list-of-strings column, absent meaning every row takes the view's default label. Attribute value columns are read by name (below) |
| a view group | as a view, plus either `view=`, the column naming which view of the group each row belongs to, or `view_key=`, one view for the whole table (a group whose views each have their own file inserts one table per view); a roster of views with their metadata is `insert(group, roster=table, key=, **metadata_columns)` |
| an attribute | `id=`, `value=`; on a group-scoped attribute also `view=` |
| a layer, by key | `id=`, `key=`: one key per row, or a list of one key per level on a tiered layer; on a group-scoped layer also `view=` |
| a layer, artifacts | `insert(layer, artifacts=table, key=, parent=, contents=, attached_layer=, attached_key=, members=, excluding=, space=, level=, attached_level=, shape=)`, each named where the table carries it. `level` and `attached_level` are read by the build only under their own names, so those two take the canonical name or the column is renamed in the table; `shape=` takes the kind word and the shape columns (`min_x`…, `cx, cy, r`, `geometry`) are read under their canonical names |
| a layer, members | `insert(layer, members=table, id=, key=, rank=, level=)`, `level` under its own name as above |
| a label set | a mapping `{key: text}`, or a table with `key=` and `text=` (a plain string column) or `contents=` (a layer's ranked contents column). Where the SDK writes the table (a mapping, or `text=`) it writes the attachment from `of` and the label's own key; where it reads the table as given (`contents=`) the attachment columns are in it and named, and the table may carry the rest of an artifacts table's columns, a label set being a layer. A generating set is `insert(labels, members=table, id=, key=, rank=)` |
| a vocabulary | `key=`, `title=`, `code=` |

An artifacts table and a members table are two inserts on the same layer, each with its own
column names, since both carry `key` and `level`. A table in Tessera's own shape is no exception
to the rule: a canonical column the call did not name (`members`, `excluding`, `rank`, `parent`,
`contents`, `attached_layer`, `attached_key`, `space`, `code`) is refused naming the column and
the two remedies, name it or drop it, so nothing is read silently at either door. Several inserts
on one target before a commit accumulate: the build reads them all and ingest pages each, so a
corpus in parts is loaded by the same calls as one file; a second part whose schema differs from
the first is refused naming the two types.

The columns that carry identity or disclosure are named on every call and never matched: the
id, the coordinates, the access labels, a layer's key, a group's view column. An attribute's
value column is the one place a name match is what the user meant, as SQL's `INSERT BY NAME`
is: the attribute was declared, and a column of its name in the frame inserted into the
**allocation view** fills it; on any other view's insert attribute-named columns are ignored,
so a frame inserted for its coordinates alone carries nothing it was not meant to. A frame with
no column of that name fills nothing. `columns={attribute: column}` on the allocation view's
insert names one explicitly.

**Identity.** A row is named one of two ways, and the SDK keeps no map between them.

- **`id=` names a column**, and its values are the external id at both doors: the build takes
  the column as supplied, in any type, mints its own entity ids, writes the external-id index
  from it and joins every other insert on it (configuration.md §8); the ingest route takes the
  same bytes as `external_id`. Every insert that names rows names the same column, under
  whatever name it has in that table.
- **No `id=`** on a view's insert means Tessera ids. The build mints no external id; rows are
  addressable only by the `tessera_id` a viewer gets back from a pick or a drill-down, the
  ingest route returns the ids it assigned, and a later insert or `remove()` names them with
  `addressing: tessera`.

The SDK holds nothing about which rows the database has. A commit sends what was inserted; a
row whose id the database holds is refused by the server, whole page, and the report says so. A
re-run of a cell is a re-run: databases are stateful, and the SDK does not make a second
`commit()` of the same table silent.

**Before the first commit** an insert binds the table to its target for the build: the SDK
writes the table under `sources/` (or records the path) and names it and its columns on the
target's block. **After the first commit** the same insert is sent at the next `commit()` to
the route its target owns (§6.2), and nothing is written under `sources/`: the declaration's
`source` on a block names the first load alone, a block declared later names none, and a
closed vocabulary declared later carries its inserted values inline so `tessera check
--payloads` can emit its body.

## 4. Declarations

A `declare_*` verb adds one block of configuration.md's declaration and takes no data: no
source, no table, no column. What a block reads comes from `insert` (§3), and until something
is inserted a declared thing is declared and empty, which configuration.md §2 allows. The SDK
writes the TOML and keeps it as `db.declaration`, and `tessera check` reads that file, so the
mapping from verb to block is checked by the binary rather than mirrored in Python.

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
db.declare_view(name, extent=None, default_label="public", projection="none",
                visibility="public", anchor=False, title=None)
```

`default_label` is what a row with no labels takes (decision 0133). `extent` is the frame: a
box, a projection's domain, or `None`, which lets the first commit fit one (§6.1). `visibility`
is the view's own gate. The first declared view is the allocation view (decision 0112) unless
another says `anchor=True`; the SDK writes `allocation_view` into the TOML in either case.

A second plain view over the same entities takes its own insert with the same id column, a
second pair of coordinates, and the same access column: the build refuses an entity whose
labels disagree between views and the ingest route refuses a join row whose labels differ from
the held ones (views.md §4). An insert for a second view without `access=` is refused naming
it; the SDK copies nothing.

### 4.3 View groups

```python
db.declare_view_group(name, metadata=None, members=None, default_label="public",
                      extent=None, projection=None, visibility="public", title=None)
```

`metadata` is the per-view values as `{name: type}`; `members` names another group whose views
this group shares; `default_label` is the group's `point_visibility` default. The group's views
and their metadata come from `insert(group, roster=table, key=, ...)`, and its rows from
`insert(group, table, id=, x=, y=, access=, view=)` with `view=` naming the column that says
which view each row belongs to, or `view_key=` naming the one view a whole table is for (§3); a
view's metadata comes from the roster insert in either form. A group-scoped attribute or layer names the group in
`scope` (§4.5, §4.6) and its view column on the insert. views.md owns the semantics.

### 4.4 Vocabularies

```python
db.declare_vocabulary(name, width="u16", closed=False, visibility="public", values=None,
                      reserved=None, title=None)
```

A closed vocabulary gives `values` inline or takes an `insert(name, table, key=, title=,
code=)`; an open one minted from the data needs neither and may take an insert for titles.
`width` is the code space's width, fixed at the first commit, and defaults to `u16`, a stated
default rather than one read from the data. `visibility` is `public` or `derived`.

### 4.5 Attributes, and the helper that declares them from a frame

```python
db.declare_attribute(name, type, render=False, index=False, vocabulary=None, analyser=None,
                     scope="entity", title=None)
```

An attribute is declared with its type and its two flags and nothing else. It is filled by an
`insert(name, table, id=, value=)`, or by name from a frame inserted into a view (§3).

Nothing is inferred by default. The helper that reads a frame is explicit and never picks the
irreversible choice:

```python
db.declare_columns(frame, skip=[...], render=[...], index=[...], keyword=[...], category=[...])
```

declares every column of the frame not in `skip` and not already declared, typed from its
dtype by the table below, as details only: stored in the record blob, shown at drill-down,
neither rendered nor indexed. `render` and `index` apply their flags to the columns named;
`keyword` and `category` choose those families for string columns, which are `text` otherwise.
The id and coordinate columns are columns like any other, so `skip` names them. The helper
prints the table it declared. A column not in `render` can be indexed later at any commit;
`render` is fixed at the first commit (decision 0136's amendment) and is why the helper never
chooses it.

| Column dtype | Declared as |
|---|---|
| integer, float, bool | the matching width |
| datetime | `timestamp_us` |
| string | `text`; `keyword` or `category` (over an open public vocabulary) where the call names it |
| list of strings | not declared; name it as `access=` on the view's insert, or declare it |
| anything else | not declared, and listed |

An open public vocabulary publishes value names minted from the data to every principal, and
`tessera check` prints a warning per such vocabulary. On a local database the user is the
authority the warning asks for, so the helper's table states the choice once. Issue #83 is open
on serving `derived` visibility, under which a value exists for a viewer only if they can see a
point carrying it; when it serves, `derived` becomes the helper's default.

### 4.6 Layers

```python
db.declare_layer(name, kind, views=None, membership="enumerated", value_set=None, levels=None,
                 prune_children=False, shape=None, default_space="view", layout=None,
                 visibility="public", artifact_visibility="inherited",
                 require_member_visibility="none", withdraw_on_member_deletion=False,
                 depends_on=None, computed=("centroid", "box", "hull"), supplied=None,
                 scope="entity", title=None)
```

`kind` is `flat`, `nested`, `stacked` or `tiered` (annotations.md), or `dag`
(dag-hierarchies.md); `levels` is a list of `(level, title, zoom?)` for the kinds that take
them. `views` defaults to every view. `membership` is `enumerated`, `spatial` (with `shape`
naming the kind and `default_space` the space) or `{"attribute": field}`. On a spatial layer
`computed` defaults to `("centroid", "box")` and `hull` is refused at the call: an artifact has
one drawn geometry, and a membership shape is it (polygon-membership.md §7.1). A group-scoped
layer takes `scope={"group": name}` and names its view column on the insert.

An enumerated layer's membership comes from its inserts, in either spelling, at any commit:

- **A key column**: `insert(layer, table, id=, key=)`, one key per row. The build, the ingest
  route and the values route all mint the artifacts from it and join the rows (decision 0128,
  and the F ruling of 2026-09-18 for the values route), so a table with an id and a key column
  is enough whether the rows are new or held. Such a layer has computed content only and the
  SDK writes `value_set = "open"`, without which a key the layer's artifacts do not declare is
  refused; the report prints it, since under `open` a mistyped key is a permanent artifact.
- **An artifacts table and a members table**: `insert(layer, artifacts=..., members=...)` with
  the columns named per §3's table. This is the only way for a layer with supplied content,
  since an artifact served without content its layer declares cannot be told from one whose
  content was withheld.

`supplied` lists the content kinds an artifacts table's contents column carries, as
`(name, type, gate)` with `gate` `all` or `inherited`; `computed` lists what the engine derives
per viewer. `visibility`, `artifact_visibility` and `require_member_visibility` are the layer's
disclosure controls (configuration.md); `artifact_visibility` is a default, or `{"field":
column, "default": label}` reading a column of the artifacts table. The local defaults are
public, inherited and none, and the commit report prints them, because a local user holds every
term and a deployment author sets them.

### 4.7 Labels

```python
db.declare_labels(name, of, content_requires="inherited", type="text",
                  require_member_visibility="none", artifact_visibility="inherited",
                  title=None)
```

A label set over a clustering: the `[layer.labels]` block, which expands to a flat layer of
supplied content attached to `of`. Its text comes from `insert(name, {key: text})` or
`insert(name, table, key=, text=)`.

**A label with no members of its own is the label of its cluster** (decision 0145): it is drawn
where the cluster is drawn, counted over the cluster's members, and served to whoever is served
the cluster. That is the default and needs no declaration key.

`content_requires="all"` is the exception that narrows: the text was generated from a specific
set of documents, given as `members=` on the insert with `key=`, `id=` and `rank=`, and is read
only by a viewer who can see every one of them. `require_member_visibility` is the layer grain,
how much of a label's membership a viewer must see before the label exists for them.

### 4.8 What the TOML always says

The SDK writes on every block the source and the column names its inserts gave it, the
allocation view, `value_set` on every layer, and the disclosure controls on every layer and
vocabulary, whether the user said them or the defaults did, and never writes `source` under
`[defaults]`. A reader of `schema.toml` sees the whole declaration without knowing the SDK's
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

- **The frame is fixed.** A view with `extent=None` has its frame fitted to its inserted rows,
  widened by half the fitted box's width on each side (assumed default; the report prints the
  frame). A frame is index configuration and does not change for the life of the view. The
  build clamps a row outside the frame and reports the count (configuration.md §1); ingest
  refuses it (write-path §2.1 step 9). A user who will add rows later declares `extent`, and
  the refusal message names `extent=`.
- **The column types and render flags are fixed.** A later `declare_attribute(render=True)`
  is refused with decision 0136's wording; an indexed column can be added at any time.
- **The allocation is signature-sorted** over the whole inserted corpus, which ingest does not do.
  This affects posting compression and latency (ingest.md §5), never what is served.

A first commit with no rows inserted builds an empty database, which needs an explicit
`extent` on every view; the SDK refuses it otherwise, naming the view.

### 6.2 Later commits: the plan

A part supplied twice is accepted, a part supplied differently is a `409` on that part, and a
set grows by the delta (ingest.md §1.1). Ordering therefore matters only for existence, and
the order is fixed:

1. **Declarations** added since the last commit, as the runtime `PUT`s (ingest.md §1.3): view
   groups, then a `members` group after the group it names, roster views, plain views,
   vocabularies before the attributes that name them, layers and label sets, each body from
   `tessera check --payloads` (configuration.md §2) except the roster record, which the SDK
   builds. A closed vocabulary's body carries its first page of values, since the route refuses
   a closed set with none, and the rest follow as `PATCH` pages sized by rows and by the route's
   body cap. A vocabulary no column names yet is not on `/v1/meta`, so it is redeclared at each
   commit and the route answers it as held. A `render` column is refused at the verb after the
   first commit (decision 0136's amendment).
2. **Rows**: an insert into a view, per view, the allocation view first, in pages under the
   limits `/control/status` publishes, with `x-tessera-view` on each; the allocation view's
   attribute columns matched by name travel with its rows. A second view's insert carries ids,
   coordinates and the held labels, and joins existing entities. When step 3 has work, the rows
   are flushed with `wait=visible` here, so the values that follow address rows the database
   holds.
3. **Values**: an insert into an attribute, or into a layer by key column, on rows the database
   holds, through `POST /control/values`, which fills the cells and mints or joins the artifacts
   a key column names as the other two doors do (contracts §3.4).
4. **Artifacts**, per layer in dependency order: a clustering before its labels, a target before
   a layer attached to it, a layer before one that depends on it. Where a publication attaches to
   or grows a key that step 3 mints in this same commit, the rows and values are flushed with
   `wait=visible` first, since a minted artifact is resolvable only from its publication. A
   polygon has two encodings: the build reads a `geometry` column as WKB, and the publication
   record takes `wkt` text, so a WKB geometry inserted after the first commit is a pre-flight
   finding naming both. Within a layer, `PUT` pages
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

The commit returns a report: rows accepted per view, artifacts minted, memberships joined,
refusals by row and part, and the flush time; then the inserts are forgotten.

### 6.3 Pre-flight

Before a byte is sent, against the declaration and `/v1/meta`, `check()` and `commit()` report:

| Finding | Report |
|---|---|
| rows outside a view's frame | listed with the frame; the server refuses the page |
| rows with no id where the insert names an id column | listed |
| a key column inserted into a layer that declares supplied content | named, with the artifacts-table route as the remedy |
| a label insert whose clustering is neither held nor inserted | named |

`check()` reports and sends nothing. `commit()` reports and refuses to send while a finding
stands, naming it; nothing is dropped or rewritten. The user corrects the data or the
declaration and commits again.

### 6.4 Retries

A page is sent with a fresh random batch id, made when the page is built and reused only for
the retries of that page inside one `commit()`. A `429` is retried after its `Retry-After` under
that id with identical bytes. A request whose answer is lost is reported as unanswered and is
not resent. Nothing is kept across commits or sessions: the same frame inserted and committed
twice is two requests, so rows carrying an id are refused as duplicates the second time and rows
without one are loaded again. A `409` on a differing part is reported per row and
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
access column it inserted, plus each view's default label, in its log, and `map()` mints with
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
  inserted id column's type on a `Database` and to bytes on a `connect()` viewer, which knows no
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
db.declare_view("map")
db.declare_columns(df, skip=["paper", "x", "y", "cluster"], render=["year"], index=["year"])
db.declare_layer("clusters", kind="flat")
db.declare_labels("topics", of="clusters")
db.insert("map", df, id="paper", x="x", y="y")          # title and year read by name
db.insert("clusters", df, id="paper", key="cluster")
db.insert("topics", topic_names)                        # {cluster_key: text}
db.commit()
db.map(colour_by="cluster:clusters")
```

### 10.2 The arXiv corpus from files, with terms and three layers

The declaration under `data/notebook/schema.toml`, as calls; the files are read in place.

```python
db = td.create(path="~/tessera/arxiv")
db.declare_view("s0", title="arXiv, 50,000 papers")
db.declare_vocabulary("archive", closed=True, width="u8")
db.declare_vocabulary("primary_category", closed=True, width="u16")
db.declare_attribute("archive", type="category", vocabulary="archive", render=True, index=True)
db.declare_attribute("primary_category", type="category", vocabulary="primary_category",
                     render=True, index=True)
db.declare_attribute("submitted_at", type="timestamp_us", render=True)
db.declare_attribute("title", type="text", index=True)
db.declare_attribute("abstract", type="text", index=True)
db.declare_attribute("arxiv_id", type="keyword", index=True)
db.declare_layer("clusters/kmeans", kind="flat", value_set="open",     # §10.3 mints into it
                 require_member_visibility={"count": 50})
db.declare_labels("topics/kmeans", of="clusters/kmeans", content_requires="all")
db.declare_layer("clusters/hdbscan", kind="nested", require_member_visibility={"fraction": 0.05})
db.declare_labels("topics/hdbscan", of="clusters/hdbscan", content_requires="all")
db.declare_layer("taxonomy/arxiv", kind="tiered", levels=[(0, "archive"), (1, "subject class")],
                 require_member_visibility={"count": 1}, computed=("centroid", "box"))

db.insert("archive", "archive.parquet", key="key", title="title", code="code")
db.insert("primary_category", "primary_category.parquet", key="key", title="title", code="code")
db.insert("s0", "points.parquet", id="entity_id", x="x", y="y", access="categories")
for layer, name in [("clusters/kmeans", "clusters-kmeans"), ("clusters/hdbscan", "clusters-hdbscan"),
                    ("taxonomy/arxiv", "taxonomy-arxiv")]:
    db.insert(layer, artifacts=f"{name}.parquet", key="key", level="level", parent="parent",
              contents="contents")
    db.insert(layer, members=f"{name}-members.parquet", id="entity", key="key", level="level")
for labels, name in [("topics/kmeans", "topics-kmeans"), ("topics/hdbscan", "topics-hdbscan")]:
    db.insert(labels, f"{name}.parquet", key="key", contents="contents")
    db.insert(labels, members=f"{name}-members.parquet", id="entity", key="key", rank="rank")

print(db.check())
db.commit()
print(db.declaration)

db.map()
db.viewer(terms=["astro-ph"]).map()
db.viewer(terms=["cs.LG", "stat.ML"]).map(layers=["clusters/hdbscan"])
```

### 10.3 A week of new papers into the running database

The same calls as the first load, on the new tables.

```python
db.insert("s0", new_df, id="entity_id", x="x", y="y", access="categories")
db.insert("clusters/kmeans", new_df, id="entity_id", key="cluster")   # held keys join, new keys mint
db.insert("topics/kmeans", {"k-new": "Diffusion models for audio"})
db.insert("topics/kmeans", members=generating_rows, id="entity_id", key="key", rank="rank")
print(db.check())     # the plan and the pre-flight
db.commit()           # the report
```

### 10.4 A second clustering over the same points

```python
db.declare_layer("clusters/second", kind="flat")
db.insert("clusters/second", df, id="entity_id", key="cluster2")  # held rows; one column and an id
db.commit()
db.map(colour_by="cluster:clusters/second")
```

### 10.5 Two projections over one set of papers

```python
db.declare_view("knn")
db.declare_view("pca64")
db.declare_layer("clusters/kmeans", kind="flat", views=["knn", "pca64"])
db.insert("knn",   df_knn, id="arxiv_id", x="x", y="y", access="categories")
db.insert("pca64", df_pca, id="arxiv_id", x="x", y="y", access="categories")
db.insert("clusters/kmeans", df_knn, id="arxiv_id", key="cluster")
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

Rulings of 2026-09-18:

- Declaring and inserting are different verbs. A `declare_*` verb takes no data; `insert(target,
  table, ...)` hands a table to a declared thing and names its columns; `commit()` sends what
  was inserted and forgets it; adding data is the same `insert` calls again. `stage` goes.
- No column name is guessed. The id, the coordinates, the access labels and a layer's key are
  named on every call; an attribute's value column is matched by the attribute's name from a
  frame inserted into a view, as SQL's `INSERT BY NAME`. A column no target reads is ignored.
- Nothing is inferred by default. `declare_columns(frame, render=, index=)` is the explicit
  helper; it declares every column as details, applies the two lists, and never chooses
  `render` on its own.
- A label with no members of its own is the label of its cluster: drawn where it is drawn,
  served to whoever is served the cluster. The default; an engine rule (H).
- A table with an id column and a value column is insertable whatever the source's history: a
  layer's key column mints and joins at the values route as at the other two doors (F).
- The verb names follow the industry's: `insert`, not `write` or `stage`.
- A label shows no count. Decision 0104's D13 gave a dependent's row its target's masked count
  so a label could print its cluster's size; the number is not useful beside a label and goes.
  The amendment to client-components and 0104 is the controller's. Not built yet.
- An attached record at the publish route may omit `members` and `excluding`; an unattached one
  keeps the requirement. The SDK omits the field for a memberless label.
- Issues #151, #152, #153 and #155 are fixed as found ; #153 was already fixed on main by
  decision 0144's cycle, and #151 was the SDK reading through a session older than the view.
- A batch id is the client's own id for one request and is never derived from the request's
  content. Loading the same points again is a second load. The server remembers an id for as
  long as the log holding its record is retained (#154).
- A generating set is projected and maintained over the whole row space as a membership is, and
  the containment partition declines a set that reaches above the base rows (#150, option (a)).
  Not built yet: on a branch under review.
- A label names its target by the target's id on the wire. The copied count and the client's
  join by count go. Not built yet: on a branch.

### 11.2 Needed

- **E. The binary at release.** Platform wheels carrying it. Until then `PATH`, `TESSERA_BIN`
  or the checkout.

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
| S9 declare and insert | `declare_*` takes no data; `insert(target, table, **columns)`; `commit()` forgets; `stage` removed; `declare_columns`; no guessed names; undeclared columns ignored; the examples and the demo rewritten | every served test, every corpus regeneration and the demo's headless walk | the values route, decision 0145 |

The Rust changes (A, B, C) are small and sit in the server and the CLI; the engine is untouched.
