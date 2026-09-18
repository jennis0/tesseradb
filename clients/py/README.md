# `tesseradb`

Tessera's Python package (decision 0095): one package whether you want the widget, the SDK or
both. The widget and the SDK are here; the in-process instance joins them later. It shares no
code with `reference/`, the test-only oracle.

```
pip install tesseradb            # authorise, Token — the standard library and nothing else
pip install 'tesseradb[widget]'  # + anywidget and the notebook widget, Map
pip install 'tesseradb[local]'   # + pyarrow and the SDK: create, declare, insert, commit
```

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

Six sections: a DataFrame with a cluster column, mapped; the 50,000-paper arXiv corpus from files,
with terms, three clusterings and their topic lines, mapped as its own principal and as two arXiv
categories; a week of new papers into the database while it serves; a second clustering over rows
it already holds; one set of points under two projections; and the database saved, reopened and
handed to `tessera serve --deployment`.

They need `pip install -e 'clients/py[widget,local]'`, a `tessera` binary on `PATH` or named by
`TESSERA_BIN`, and the corpus at `data/notebook/`, which `TESSERA_NOTEBOOK_DATA` names elsewhere.
Three things are in no extra, because the package needs none of them: `pandas`, which the first
section's frame is built with, and the front end you are running, `marimo` or `jupyterlab`. So
`pip install pandas marimo` for the first notebook, `pip install pandas jupyterlab` for the
second.

`tests/test_sdk_examples.py` executes the marimo notebook's cells with no browser, asserts the
counts each section prints and that every map it draws is served the layer it colours by, and
compares the Jupyter twin cell for cell. A notebook that drifts from the package fails the gate.

## A database in a directory

The SDK (`python-sdk.md`) makes a Tessera database out of frames and files. Three verbs carry it,
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
print(db.check())                                  # what the declaration reads, and what it discloses
print(db.commit())                                 # tessera check, tessera build, tessera serve
```

`db.declaration` is the TOML the SDK wrote, and `tessera check` reads that file: the mapping from
verb to block is checked by the binary rather than mirrored in Python. Every block names the source
and the column names its inserts gave it. The directory is everything the binary reads, so
`db.save("~/somewhere")` and `tessera serve --deployment ~/somewhere/tessera.toml` on another
machine serve the same database.

`declare_view_group` is the group surface: its views and their metadata come from
`insert(group, roster=table, key=, **metadata_columns)`, and its rows from `insert(group, table,
id=, x=, y=, access=, view=)` with `view=` naming the column that says which view each row belongs
to. An attribute or a layer scoped to a group takes `scope={"group": name}`, and every insert into
one names its view column.

`declare_layer` carries the whole layer surface: spatial and attribute membership, shapes and
spaces, per-level zoom, pruning, the serving-layout pin, attached and dependent layers, and an
authored roster written inline. A membership may be spelled by exclusion, and such an artifact is
published once: the complement is taken over the entities that exist at that moment, so a key the
database already holds takes no second exclusion.

## Inserting: what is named, and what is ignored

Every column a target needs is named on the call, as the notebook-widget libraries name `x=` and
`y=`, and a column the call does not name is ignored. Every insert prints two lists, the columns it
read and the columns it ignored, so a frame with more columns than the target reads is the ordinary
case rather than a surprise.

| Target | Columns named on the call |
|---|---|
| a view | `id=`; `x=`, `y=` (or `lon=`, `lat=` under a projection); `access=`, a list-of-strings column |
| a view group | as a view, plus `view=`; its roster is `insert(group, roster=table, key=, **metadata)` |
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
read where it lies, with two exceptions the call prints: a label set given a mapping or a `text=`
column, where the SDK writes the table the publication takes, with the attachment its `of` names.

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
`category` choose those families for string columns, which are `text` otherwise. It reads the
frame's schema and never its values, and it never chooses `render`, which is fixed at the first
commit.

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
frame is inserted again and sent again, and what happens then is the database's answer: a replay
where the bytes and the batch id are the ones first sent, a `409` on the page where the ids are
ones it holds. Databases are stateful, and this one says so rather than guessing.

Every declaration is made at any commit, and the next commit sends it to the running service: a
vocabulary, an attribute, a layer, a label set, a plain view and a view group. Two of them are
narrower after the first commit than before it:

- A **view** names its own `extent=`, there being no rows at a running service to fit a frame
  against.
- An **attribute** declared there is not a render column: a rendered value is served from the hot
  row that carries it, and `PUT /control/attributes` declares a column against entities that
  already exist, so `render=True` is refused at the verb (decision 0136's amendment). An indexed
  column is added at any time, and an insert into it fills it.

One rule the engine does not carry yet shows as a pre-flight finding: a label with no members of
its own is the label of its cluster (decision 0145), and until the engine places one that way such
a label is served to nobody, so a label set given text and no members of its own is named and the
commit is refused.

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
print(db.check())                              # the plan and the pre-flight, with nothing sent
print(db.commit())                             # the report
```

`check()` returns the plan and the pre-flight and sends nothing; `commit()` runs the same plan and
returns the report: rows accepted per view, artifacts minted, memberships joined, parts already
present, refusals by row and part, and how long the wait for its publication took.

The order is fixed: declarations, then points per view with the allocation view first, then values
on entities that already exist, then artifacts per layer in dependency order. Where a commit
carries both rows and values, the rows are flushed between the two, so a value addresses a row the
database holds. Which declarations are new is read from `/v1/meta`, so the database is what says
what it holds.

Every acknowledgement names the publication its work becomes visible in. The pages all go
unwaited and one `POST /control/flush?wait=visible` closes the commit: the flush arms a cycle and
holds its answer until that cycle has completed, which covers every page before it. So the commit
blocks once, not once per page, and the next cell sees the rows. Past the server's
`serve.visible_wait_max_secs` the answer says `visible: false`, which is a finding; the write is
durable and reaches the served forms at the next cycle either way.

A page the server answers as a replay — the same bytes under the batch id they were first sent
under — says so, and the report prints "replayed, nothing landed" for it rather than counting rows
it did not land.

A page's batch id is derived from the source name, the page index and a hash of the bytes, which is
what the `429` retry and the resend of a lost acknowledgement carry. A `429` is backpressure and is
retried after its `Retry-After` with identical bytes; a request that reached no server is reported
as a refusal rather than raised. A value that changed is a `409` on that part, reported and not
retried, because an edit is a delete and a re-ingest.

The pre-flight runs before a byte is sent and it sends nothing while a finding stands: it reports,
names the finding, and drops or rewrites no row. Rows outside a view's frame are listed with the
frame, rows with no id where the insert names an id column are listed, a key column inserted into a
layer that declares supplied content is named with the artifacts-table route as the remedy, and a
label insert whose clustering is neither held nor inserted is named.

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
The child is killed by its pid at `close()` and at interpreter exit.

The deployment file the SDK writes sets `serve.cors_loopback`, which admits a page served from a
loopback address on the viewer plane. A notebook page's origin is the front end's, unknown at start
and not enumerable for a webview, so an origin list cannot state it; the three planes bind
loopback, so what this admits are pages on this machine.

## Reading it: the map, a principal, and the query verbs

```python
db.map(colour_by="cluster:clusters/kmeans")   # the explorer over this database, in this cell
db.viewer(["cs.LG"]).map()                    # what one principal sees, not the operator filtered down
```

`map(view=None, layers=None, colour_by=None, filters=None, height=480)` is the widget below,
pointed at this database's own viewer plane with a token minted from the directory's session
credential. The terms it grants are every access label the SDK inserted plus each view's default:
that is Python asserting the local principal's authority, which is admissible on a
single-operator database and nowhere else.

`viewer(terms)` mints for exactly the terms named, so the map of any principal is one call, and
every count, density, cluster and label in it is computed inside that principal's mask rather
than filtered out of the operator's. A term the database has inserted no label for is refused and
named: a typo would otherwise draw an empty map with no error anywhere.

The three query verbs are `Viewer`'s and are reached on a database through its all-terms viewer.
Each goes through the viewer plane with the token, never by reading the bundle:

```python
db.meta()                                      # the views, layers and schema this principal reads
db.viewport(bbox=None, view=None, filters=None, k=None, zoom=0)   # the points served, as a table
db.item(tessera_id)                            # one record: fields, labels, views
```

`viewport()` returns a pyarrow table of `tessera_id`, `code` and the columns the schema declares
as rendered. Those are points; a record is what `item()` returns. A served set is bounded by `k`,
so the table's schema metadata carries what the response said about the set it came from:
`tessera.counts` (`visible` is inside the mask and the tiles the box touches at the request's
zoom, `matched` is that and the filter, `served` is that and `k`), `tessera.trailer` and
`tessera.request`. `bbox` defaults to the view's whole extent and `view` to the first this
principal is served.

`item()` returns `fields` by declared column name, `labels` (the item's labels this principal
also holds), `views`, and `external_id` where the database has one: on a `Database` it comes back
as the type its id column carried, and on a `connect()` viewer as the bytes the wire carries.

`db.close()` stops the server. It invalidates nothing: a token this database minted stays good
until its lifetime runs out (`[disclosure] token_max_lifetime`, an hour), and no route withdraws
one.

## A deployment somebody else runs

```python
v = tesseradb.connect("https://tessera.example/viewer", token=my_token)
v.map(colour_by="cluster:clusters/kmeans")
v.viewport(k=64)
```

`token` is a string, a `Token` or a callable returning either, as `Map` takes one. A `Viewer` from
`connect` has `map()` and the three read verbs and nothing else: no `viewer(terms)`, minting
another principal needing the session credential, and no write verb, the control plane having one
operator credential and no per-principal authority.

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
mints any principal, and a notebook that holds it is client-interaction §7's pooled-service-token
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
