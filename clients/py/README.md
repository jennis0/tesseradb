# `tesseradb`

Tessera's Python package: one package whether you want the widget, the SDK or
both. The widget and the SDK are here; the in-process instance joins them later. It shares no
code with `reference/`, the test-only oracle.

```
pip install tesseradb            # read, create, declare, insert, commit; with pyarrow and the binary
pip install 'tesseradb[widget]'  # + anywidget and the notebook widget, Map
```

Every table the package returns is a pyarrow table; `.to_pandas()` on it gives a DataFrame where
pandas is installed. `pip install tesseradb --no-deps` installs neither pyarrow nor the
`tesseradb-native` wheel that carries the `tessera` binary: install `pyarrow>=14` by hand, and
put a `tessera` binary on `PATH` or name it with `TESSERA_BIN` to make or serve a database.

From this checkout, `pip install -e 'clients/py[widget]'` — the wheel's build hook
(`hatch_build.py`) runs `npm ci` (when the install is stale) and `npm run build -w
@tesseradb/components` in `clients/ts` and copies the single-file bundle into
`tesseradb/static/`, which is why a checkout install needs Node and a PyPI install does not.
Nothing built is committed. `check.sh` is the package's half of the gate: the tests, and a wheel
built and opened to prove the bundle is inside it.

## Try it

Two notebooks under `examples/`, the same guided walk in each front end:

```
marimo edit clients/py/examples/notebook_marimo.py
jupyter lab clients/py/examples/notebook.ipynb
```

Six sections: a DataFrame with a cluster column, mapped; the arXiv corpus from files, with access
terms, a hierarchy of topics and their titles, mapped as the database's own reader and as two
narrower readers; filters and counts; one view per year with a clustering of each year; a week of
new papers into the database while it serves, with a suppression; and the database saved and
reopened.

They need `pip install -e 'clients/py[widget]'`, a `tessera` binary on `PATH` or named by
`TESSERA_BIN`, and the corpus at `data/notebook-2m4-live/`, or `data/notebook-sample/` with
`SCALE = "sample"`, which `TESSERA_NOTEBOOK_DATA` names elsewhere.
Three things are in no extra, because the package needs none of them: `pandas`, which the first
section's frame is built with, and the front end you are running, `marimo` or `jupyterlab`. So
`pip install pandas marimo` for the first notebook, `pip install pandas jupyterlab` for the
second.

`notebook.ipynb` is generated from the marimo file with `marimo export ipynb`.
The Jupyter copy needs marimo installed, and its year slider does not drive the map there.
`tests/test_sdk_examples.py` executes the marimo notebook's cells with no browser at the sample
scale, asserts the counts each section shows and that every map it draws is served the layers it
draws and colours by, and checks that `notebook.ipynb` is what a fresh export makes. A notebook
that drifts from the package fails the gate.

## A database in a directory

The SDK makes a Tessera database out of frames and files. Three verbs carry it,
and each does one thing. `declare_*` says what exists and takes no data. `insert(target, table,
**columns)` hands a table to a declared thing and names every column it reads. `commit()` sends
what was inserted since the last commit and forgets it: the first time through the build, after
that through the control plane. `check()` is `commit()` with nothing sent.

```python
import tesseradb as td

db = td.create()                                   # a temporary directory, on /dev/shm where there is one
db.declare_view("map")
db.declare_columns(df, skip=["paper", "x", "y", "cluster"], index=["title"])
db.declare_layer("clusters", kind="flat")
db.insert("map", df, id="paper", x="x", y="y")     # title is read by name
db.insert("clusters", df, id="paper", key="cluster")
db.check()                                         # what the declaration reads, and what refuses it
db.commit()                                        # tessera check, tessera build, tessera serve
```

No call prints. Each returns a report that shows as a short summary of what happened, in numbers,
with every refusal and finding in full; a notebook cell that ends in one shows it. The detail is
on the report's attributes: an insert's `read` and `ignored` columns, a check's or a first
commit's `findings` and `log` (the declaration check's and the build's text), a later commit's
`plan`, `rows_accepted` and `refusals`. `db.path` is where the database is and `db.binary` the
`tessera` program it runs.

`db.declaration` is the TOML the SDK wrote, and the declaration check reads that file: the
mapping from verb to block is checked below the SDK rather than mirrored in Python. `check()` and
`commit()` run that check in this process where the `_tessera` extension module is installed, and
through `tessera check` where it is not; the two read one declaration with one parser, and what
the extension adds is a refusal naming the block it is about. Every block names the source
and the column names its inserts gave it. The directory is everything the binary reads, so
`db.save("~/somewhere")` and `tessera serve --deployment ~/somewhere/tessera.toml` on another
machine serve the same database.

`declare_view_group` is the group surface: its views and their metadata come from
`insert(group, roster=table, key=, **metadata_columns)`, and its rows from one of the two
roster forms. One file for every view names the column that says which with `view=`; a group whose
views each have their own file inserts one table per view, naming the one view it is for with
`view_key=`, and the SDK writes a roster record per table with the metadata that key carries. An
attribute or a layer scoped to a group takes `scope={"group": name}`, and every insert into one
names its view column.

`declare_layer` carries the whole layer surface: spatial and attribute membership, shapes and
spaces, per-level zoom, pruning, the serving-layout pin, attached and dependent layers, and an
authored roster written inline. A membership may be spelled by exclusion, and such an artifact is
published once: the complement is taken over the entities that exist at that moment, so a key the
database already holds takes no second exclusion.

## Inserting: what is named, and what is ignored

Every column a target needs is named on the call, as the notebook-widget libraries name `x=` and
`y=`, and a column the call does not name is ignored. Every insert returns a record whose summary
names the columns it read and counts the ones it ignored, and whose `read` and `ignored` list
them, so a frame with more columns than the target reads is the ordinary case rather than a
surprise.

| Target | Columns named on the call |
|---|---|
| a view | `id=`; `x=`, `y=` (or `lon=`, `lat=` under a projection); `access=`, a list-of-strings column |
| a view group | as a view, plus `view=`, the column saying which view each row belongs to, or `view_key=`, one view for the whole table; its roster is `insert(group, roster=table, key=, **metadata)` |
| an attribute | `id=`, `value=`; on a group-scoped attribute also `view=` |
| a layer, by key | `id=`, `key=`: one key per row |
| a layer, artifacts | `insert(layer, artifacts=table, key=, parent=, contents=, attached_layer=, attached_key=, members=, excluding=, space=, level=, attached_level=, shape=)` |
| a layer, members | `insert(layer, members=table, id=, key=, rank=, level=)` |
| a label set | a mapping `{key: text}`, or a table with `key=` and `text=` or `contents=`, with `attached_layer=`, `attached_key=` and `level=` where it carries them; its members are `insert(labels, members=table, id=, key=, rank=)` |
| a vocabulary | `key=`, `title=`, `code=` |

An artifacts table and a members table are two inserts on the same layer, each with its own column
names, since both carry `key` and `level`. Several inserts on one target before a commit
accumulate, so a corpus in parts is loaded by the same calls as one file; a second part whose
schema differs from the first is refused naming the two types, and where the first part was a path
read in place it is copied, a block reading one file. A path is accepted wherever a table is and is
read where it lies, with two exceptions, whose record's `in_place` is false: a label set given a
mapping or a `text=` column, where the SDK writes the table the publication takes, with the attachment its `of` names.

A table in Tessera's own shape (an artifacts table, a members table, a roster, a value set) is no
exception to the rule that every column a target reads is named on the call. A canonical column the
call did not name is refused naming the column and the two remedies, name it or drop it, so nothing
is read silently at one door and ignored at the other. `level` and `attached_level` are read by the
build under their own names and by no `fields` map, so those keywords take the canonical name and a
column called anything else is renamed in the table; a membership shape is named by its kind,
`shape="polygon"`, and its columns (`min_x`…, `cx, cy, r`, `geometry`) are read under theirs.

The one place a name is matched is an attribute's value column: the attribute was declared, and a
column of its name in the frame inserted into the **allocation view** fills it, as SQL's `INSERT BY
NAME` does. On any other view's insert, attribute-named columns are ignored.
`columns={attribute: column}` on the allocation view's insert names one explicitly.

`declare_columns(frame, skip=, render=, index=, keyword=, category=)` declares every column of a
frame from its dtype, as details: stored in the record blob, shown at drill-down, neither rendered
nor indexed. `render` and `index` apply their flags to the columns named, and `keyword` and
`category` choose those families for string columns, which are `text` otherwise. A categorical
column (a pandas `Categorical` or an Arrow dictionary column of strings) is declared as a
category unless `keyword=` names it. Every category it declares reads a new open vocabulary of
the column's name, with codes of width `u16`, so at most 65,535 values; that width cannot be
changed after the first commit, and `declare_vocabulary` with `declare_attribute` is how to
choose another. It reads the frame's schema and never its values, and it never chooses `render`,
which is fixed at the first commit.

`insert` reads a categorical column as the values it holds, wherever it reads a column of those
values: a category attribute's values, a layer's key, a view's access labels. A Parquet file
read in place with a dictionary column is refused by the declaration check; decode it first, or
insert it as a frame.

## How a row is named

`id=` names the column that names the rows. Its bytes are that row's external id at every door: a
string's UTF-8, an integer's eight little-endian bytes, binary as it stands, which is what the build
reads and what `/control/ingest`, `/control/values` and `/control/changes` take. The declaration is
what says where identity is: a view's `fields.entity_id`, an attribute's `entity_id_field`, a
members table's `fields.entity`. The SDK rewrites no column to say it.

An insert that names no `id=` is the other route: the build writes no external id, and a row is
addressed by the `tessera_id` a pick or the ingest route hands back, which is what `remove()` then
sends.

The SDK holds nothing about what the database contains. A re-run of a cell is a re-run: the same
frame is inserted again and sent again, and what happens then is the database's answer: a `409` on
the page where the ids are ones it holds, and rows with no id loaded a second time. Databases are
stateful, and this one says so rather than guessing.

Every declaration is made at any commit, and the next commit sends it to the running service: a
vocabulary, an attribute, a layer, a label set, a plain view and a view group. Two of them are
narrower after the first commit than before it:

- A **view** names its own `extent=`, there being no rows at a running service to fit a frame
  against.
- An **attribute** declared there is not a render column: a rendered value is served from the hot
  row that carries it, and `PUT /control/attributes` declares a column against entities that
  already exist, so `render=True` is refused at the verb. An indexed
  column is added at any time, and an insert into it fills it.

A label set takes its text and needs nothing else: a label with no members of its own is the label
of its cluster, drawn where the cluster is drawn, counted over its members and
served to whoever is served it. `insert(labels, members=…)` is for a generating set, the documents
a content gated `all` was written from.

Which declarations are new is read from `/v1/meta`, and a vocabulary reaches it through the column
that names it. A vocabulary no attribute names yet is therefore declared again at each commit; the
route answers an identical redeclaration as held, applies its values as a page and changes nothing,
and the report counts it under the parts already present.

## The commit after the first

The first commit builds. Every commit after it pages what was inserted since the last one through
the control plane of the server the database is already running, and then waits for the publication
that makes it visible.

```python
db.insert("s0", new_papers, id="entity_id", x="x", y="y", access="categories")
db.insert("clusters/kmeans", artifacts=new_clusters, key="key", level="level")
db.insert("clusters/kmeans", members=new_members, id="entity", key="key", level="level")
db.check()                                     # the plan and the pre-flight, with nothing sent
db.commit()                                    # the report
```

`check()` returns the plan and the pre-flight and sends nothing; `commit()` runs the same plan and
returns the report: rows accepted per view, artifacts minted, memberships joined, parts already
present, refusals by row and part, and how long the wait for its publication took.

`commit()` raises `Refusal` when nothing it was asked to do happened: a pre-flight finding stopped
it before a byte was sent, or the server refused every page. The exception's text is the report's,
and `refusal.report` is the report itself. A commit some of whose pages landed has happened, and
returns its report with the refused pages listed. `check()` is a query and returns its findings
without raising.

The order is fixed: declarations, then points per view with the allocation view first, then values
on entities that already exist, then artifacts per layer in dependency order. Where a commit
carries both rows and values, the rows are flushed between the two, so a value addresses a row the
database holds; and where a publication attaches to or grows a key the values step mints, the
values are flushed before it, a minted artifact being resolvable only from its publication. Which
declarations are new is read from `/v1/meta`, so the database is what says what it holds.

Every acknowledgement names the publication its work becomes visible in. The pages all go
unwaited and one `POST /control/flush?wait=visible` closes the commit: the flush arms a cycle and
holds its answer until that cycle has completed, which covers every page before it. So the commit
blocks once, not once per page, and the next cell sees the rows. Past the server's
`serve.visible_wait_max_secs` the answer says `visible: false`, which is a finding; the write is
durable and reaches the served forms at the next cycle either way.

A page the server answers as a replay — the same bytes under the batch id they were first sent
under — says so, and the report lists it under `replayed` and counts it in its summary rather than
counting rows it did not land. That is what a retry inside one commit gets.

A page carries a fresh random batch id, made once when the request is built, and a retry of that
request carries it again: a `429` is backpressure and is retried after its `Retry-After` with the
same id and identical bytes. Nothing is kept beyond the commit, and no id is derived from what a
page contains, so the same frame inserted and committed five times is five loads — rows carrying an
id column are refused as duplicates on the second, and rows without one are loaded again. A request
that reached no server is a refusal of that page; whether it landed is the database's to say. A value that changed is a `409` on that part, reported and not retried, because
an edit is a delete and a re-ingest.

The pre-flight runs before a byte is sent and it sends nothing while a finding stands: `check()`
reports the finding and `commit()` raises with it, and neither drops or rewrites a row. Rows
outside a view's frame are listed with the frame, rows with no id where the insert names an id
column are listed, a key column inserted into a layer that declares supplied content is named with
the artifacts-table route as the remedy, a label insert whose clustering is neither held nor
inserted is named, and a polygon inserted after the first commit as WKB is named with both
encodings: the build reads a `geometry` column as WKB and the publication route takes WKT text.

`remove(ids)`, `suppress(ids)` and `unsuppress(ids)` take the ids the id column holds, or the
`tessera_id`s where no insert named one; a removed id inserted again goes as a point row.
`leave(layer, key, ids, rank)` shrinks a content's generating set, which is the one set that may
shrink.

The binary is `TESSERA_BIN` when set, else the first `tessera` on `PATH`, else a checkout's target
directory, release before debug; `create()` names the one it found. The database directory keeps
its own session credential, operator credential and identity key under `.tessera/`, each file
owner-only. `commit()` starts `tessera serve` as a child process on loopback at port 0 and reads
the three bound addresses from the JSON line the child prints once all three planes are listening;
`db.viewer_url`, `db.session_url` and `db.session_credential` are what a token is minted against.
The child is killed by its pid at `close()` and at interpreter exit. A database `create()` made
with no path is removed at both, after its child has stopped; a directory the user named, through
`create(path)`, `open(path)` or `save(path)`, is never removed.

The deployment file the SDK writes sets `serve.cors_loopback`, which admits a page served from a
loopback address on the viewer plane. A notebook page's origin is the front end's, unknown at start
and not enumerable for a webview, so an origin list cannot state it; the three planes bind
loopback, so what this admits are pages on this machine.

## Reading it: the map, a reader, and the queries

```python
db.map(colour_by="cluster:clusters/kmeans")   # the interactive map of everything, in this cell
db.viewer(["cs.LG"]).map()                    # what someone holding only "cs.LG" sees
```

A reader is who is asking. It holds a set of access terms, and it sees an item when it holds one
of the item's labels. Every count, map, cluster and label a reader is given is computed over the
items it may see, so two readers can get different answers from the same database.

`db` reads as a reader holding every label its rows carry, plus each view's default label, so it
sees everything. `db.viewer(terms)` is a reader holding exactly the terms named. A term no row
carries is accepted and reaches nothing, and an empty list is refused.

`map(view=None, layers=None, colour_by=None, filters=None, height=480)` is the notebook widget
described below, pointed at this database's server with a token made for the reader.

### A selection

A view is one layout of the items: a map with its own coordinates. `view(name)` returns a
selection: one view, as one reader sees it. `filter` and `within` narrow it, and each returns a
new selection, leaving the one it was called on unchanged.

```python
papers = db.view("s0")                                      # every item in the view
cs = papers.filter({"archive": {"eq": "cs"}})
ml = cs.filter({"primary_category": {"in": ["cs.LG", "stat.ML"]}})
corner = ml.within((0.0, 0.0, 5000.0, 5000.0))              # (min_x, min_y, max_x, max_y)

corner.count()                                # how many items
corner.map(colour_by="primary_category")      # the map, on this view, filtered, framed on the box
corner.sample(zoom=3, k=256)                  # the points a map draws at zoom 3, as a table

db.viewer(["cs.LG"]).view("s0").count()       # the same view, as a narrower reader
```

A filter is a dictionary: a column name mapped to a test (`eq`, `in`, `range`, `prefix`,
`match` and so on, by the column's type), or `all_of`, `any_of` or `none_of` over a list of
filters. Filters added one after another must all match. A box is in the view's own
coordinates, the ones the rows were inserted with, and an item on its edge is inside. A view in a
view group is named `"<group>:<key>"`, and a name the reader cannot see is refused with the list
of names it can.

`count()` is the number of items the reader may see in the view that match every filter
and lie inside the box. A box whose outline is longer than the server's `max_region_cells`
setting allows is counted over the grid cells covering it, so the number can include items just
outside the box. Two boxes that do not overlap select nothing.

`map(colour_by=None, layers=None, height=480)` opens the widget on the selection's view with its
filters applied and its camera on the box. The map draws everything in frame, including items
just outside the box.

`sample(zoom=0, k=None, ...)` is what a map draws at a zoom: a display sample, thinned by density,
in which each map tile carries at most `k` points and the zoom sets how many tiles there are. It
holds fewer rows than the selection has items; `count()` is the number, and the reader's
`items(view, fields, ...)` reads the rows themselves, a response at a time: it returns the
response's rows as a pyarrow table and the cursor to pass back as `cursor`, `None` once no row
remains. Its other keywords are `POST /v1/items`' request fields, sent as given. The result reads as a pyarrow table of
`tessera_id`, `code` (the point's position on the view's grid) and the columns declared with
`render=True`. A category column holds each value's key, as a dictionary column, and null for a
value the reader may not see; the keys are looked up once per reader and kept. Its schema metadata carries `tessera.counts` (`visible`, `matched`, `highlighted`
and `served`, over the tiles the request touched), `tessera.request` and `tessera.trailer`.
Beside the points it has `artifacts`, the annotations served with them, and `sub_cells`, finer
counts that `underlay_offset` asks for; each is `None` when there were none. The other keywords
are sent as given: `tiles`, `highlight` (a second filter that marks points without changing which
are drawn), `layers`, `levels`, `computed`, `artifact_budget`, `artifact_rows`, `point_rows`,
`underlay_offset` and `pin`.

```python
drawn = db.view("s0").sample(layers="all", highlight={"primary_category": {"eq": "cs.LG"}})
drawn.num_rows                                # the points drawn
drawn.artifacts.to_pylist()                   # the annotations drawn beside them
```

Not built yet: `count(by=column)`, a count per value of a column, which needs a new route on the
server. Until then, `categories(column, prefix=...)` counts the items carrying each matching
value, over the whole of what the reader may see.

Not built yet: `items()`, the records of the items in a selection, which needs the server's
records route. Until then, `item(tessera_id)` returns one record at a time.

### The other queries

```python
db.meta()                                     # the views, layers and columns, as a dictionary
db.item(tessera_id)                           # one item's record: fields, labels, views
db.categories("primary_category")             # every value of a category column, as a table
db.categories("primary_category", prefix="cs")   # the values starting "cs", with item counts

v = db.viewer(["cs.LG"])
v.browse_artifacts("s0", "clusters/kmeans")   # a page of a layer's annotations
v.artifact(tessera_id, "s0")                  # one annotation's record: its count and outline
```

Each of these exists on `db` and on any reader, and answers as that reader.

`categories(column, prefix=None, view=None, codes=None)` returns a pyarrow table of a category
column's values that the reader may see, one row each, with `key`, `code` and `title`. Without a prefix it is every value. With
one it is the values whose key or title, or a word in either, starts with it, ignoring case, and
each row adds `count`, the number of items the reader may see that carry the value; the server
returns at most its `max_suggestions` setting of these, and the table's schema metadata
`tessera.more` says whether more matched. With `codes`, such as the codes in a sample's category column, it is the values of
those codes, and a code with no value the reader may see is left out; `codes` and `prefix` cannot
be combined. A column declared for a view group holds different values in each view, so it
takes `view=`.

`item()` returns `fields` by column name, `labels` (the item's labels that the reader also
holds), `views`, and `external_id` where the item was inserted with one: on a `Database` it comes
back as the type its id column had, and from `connect()` as bytes.

An annotation, or artifact, is one member of a layer: a cluster, a region, a node in a taxonomy.
`browse_artifacts()` returns one page of a layer's annotations with `next` for the page after,
and `artifact()` one annotation's record; each takes a view. Every count on them is the reader's:
`masked_count` is how many of an annotation's items the reader may see.

`db.close()` stops the server. A token the database made stays valid until it expires, within
the hour. `db.revoke(token)` ends one sooner; only the token's id is sent, and an id that names
no live token is accepted without comment.

## The operator's verbs

```python
db.status()                                    # the watermarks, queues and pagination units
db.compact()                                   # ask for the fold that removes a deletion's rows
db.drop_layer("clusters/kmeans")               # the inverse of declare_layer
db.drop_view("slices", "a", delete_dangling=False)   # the inverse of create_view
```

`remove()` puts a deletion in the overlay, and the compaction that removes its rows is what ends
it; `compact()` is how one is asked for, and it is accepted rather than finished when the call
returns. `drop_layer()` tombstones the name rather than freeing it, so a later declaration under
it is refused and no stale reference reaches a different layer. `drop_view()` deletes no entity;
`delete_dangling=True` submits the entities holding a row in no other view as ordinary
deletions, and the answer's `deleted` says how many.

## A deployment somebody else runs

```python
v = tesseradb.connect("https://tessera.example/viewer", token=my_token)
v.map(colour_by="cluster:clusters/kmeans")
v.view("s0").count()
```

`token` is a string, a `Token` or a function returning either, as `Map` takes one. A reader from
`connect` has `view()`, `map()` and the other queries. It cannot write, and it cannot read as
anyone else, since both need credentials only the operator holds.

## The widget, and the entry point being a token

`map()` above builds this; `tesseradb.Map(url, token=my_token)` is it directly, for a URL and a
token you already hold:

```python
import tesseradb
m = tesseradb.Map("https://tessera.example/viewer", token=my_token)
m
```

`token` is the viewer token your deployment issued you — as any application's user holds one.
`m.selected` in the next cell is the picked item's id, `m.region` the drawn selection with its
counts, `m.bbox` where the camera settled; setting `m.filters`, `m.layers`, `m.colour_by` or
`m.bbox` redraws. Ids are decimal strings (a `tessera_id` is a `u64`). Marimo users: the widget's
`.value` re-runs a cell at every settle; `m.observe(fn, names="selected")` reacts to a pick alone.

`tesseradb.authorise(session_url, credential, terms)` is **operator-only**: the session credential
mints any principal, and a notebook that holds it is the pooled-service-token
anti-pattern in a cell. It is for the local single-principal case and for the demo, where
operator and analyst are one person. The credential stays in the kernel; the token it mints is
what the page gets.

## The token never leaves the kernel as state

No traitlet carries the token. The page sends `ready` once per model when it mounts, the kernel
answers with the token as a custom message, and the page's store asks again with `reauthorise`
before expiry and after a refusal that means the session ended. Nothing that saves widget state —
JupyterLab's "save widget state", `nbconvert --execute`, papermill — sees it, because it is never
state. What remains is a token in the browser's memory for its lifetime.

## Where the widget's requests go: two arms, one built

The page's JavaScript calls the viewer plane directly from the notebook page's origin —
**browser-direct**, the arm that is built. The viewer plane must therefore allow that origin:
today the development-only `serve.dev_cors_origins` (the demo lists `http://localhost:5173`);
a production list is design D10, not yet ruled. This arm can only ever be enumerated for
JupyterLab and Marimo on a known origin — a VS Code notebook renders in `vscode-webview://` and
Colab in a sandboxed iframe, which no origin list can name — and it needs a browser that can
reach Tessera, which a remote JupyterHub often cannot.

**⊘ The proxy arm — documented, not built.** The widget's `url` becomes a path on the notebook
server (`/tessera/<name>/`), and a small Jupyter server extension answers it:

- it holds the session credential (or a per-principal token store) on the server, never in the
  kernel or the page;
- per notebook user it calls `/session/authorise` with that user's terms, caches the token, and
  renews it before expiry — which is more than `jupyter-server-proxy`'s static header injection,
  so it is an extension of it rather than a configuration of it;
- it forwards `/v1/*` to the viewer plane with the user's token in `authorization`, streaming
  the response body through (the points frame is a streamed Arrow IPC body with a trailer);
- the page then makes same-origin requests, so no CORS list is involved, and the kernel stays
  off the pan path because the proxy is not the kernel.

The widget under this arm sends no `ready`, since the page holds no token; the protocol above is
skipped and `Map(url)` takes no `token`. D4 chooses which arm ships first, conditional on D10.

## The protocol, for a reader of both halves

| direction | what | when |
|---|---|---|
| page → kernel | `{type: "ready"}` | once per model, at mount |
| kernel → page | `{type: "token", token, expires_at}` | in answer to `ready` and `reauthorise` |
| page → kernel | `{type: "reauthorise"}` | before expiry; after a session-ended refusal |
| kernel → page | `{type: "refused", detail}` | the token source raised |
| page → kernel | `{type: "error", what, detail}` | a `filters` expression the panel cannot hold |

Traitlets: `url`, `view`, `explorer_layout`, `height` down; `bbox`, `layers`, `colour_by`,
`filters` both ways, synced up at the settle; `selected`, `selected_artifact`, `region` up.
`last_error` is kernel-side only. The JavaScript half is `clients/ts/components/src/widget.ts`.
