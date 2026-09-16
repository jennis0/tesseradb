# `tesseradb`

Tessera's Python package (decision 0095): one package whether you want the widget, the SDK or
both. The widget is here; the SDK and the in-process instance join it later. It shares no code
with `reference/`, the test-only oracle.

```
pip install tesseradb            # authorise, Token — the standard library and nothing else
pip install 'tesseradb[widget]'  # + anywidget and the notebook widget, Map
pip install 'tesseradb[local]'   # + pyarrow and the SDK: create, stage, declare, commit
```

From this checkout, `pip install -e 'clients/py[widget]'` — the wheel's build hook
(`hatch_build.py`) runs `npm ci` (when the install is stale) and `npm run build -w
@tesseradb/components` in `clients/ts` and copies the single-file bundle into
`tesseradb/static/`, which is why a checkout install needs Node and a PyPI install does not.
Nothing built is committed. `check.sh` is the package's half of the gate: the tests, and a wheel
built and opened to prove the bundle is inside it.

## A database in a directory

The SDK (`python-sdk.md`) makes a Tessera database out of frames and files. `create()` makes the
directory, `stage()` binds a name in `[sources]` to a frame or a file, `declare_*` adds a block of
the declaration, and `commit()` builds it and serves it.

```python
import tesseradb as td

db = td.create()                                   # a temporary directory, on /dev/shm where there is one
db.stage("points", df, default=True)               # index as id; x, y, title, year, cluster
db.declare_view("map", source="points")
db.declare_layer("clusters", kind="flat", from_column="cluster")
db.declare_labels("topics", of="clusters", source=topic_names)   # {cluster_key: text}
print(db.check())                                  # what the declaration reads, and what it discloses
print(db.commit())                                 # tessera check, tessera build --mint-external-ids, tessera serve
```

`db.declaration` is the TOML the SDK wrote, and `tessera check` reads that file: the mapping from
verb to block is checked by the binary rather than mirrored in Python. The directory is everything
the binary reads, so `db.save("~/somewhere")` and `tessera serve --deployment
~/somewhere/tessera.toml` on another machine serve the same database.

What is built is the first commit and every commit after it: `create`, `open`, `stage` with the id
map, `declare` and the typed verbs for plain views, vocabularies, attributes, enumerated layers of
every kind and labels, inference, `check()`, the build and the server the first commit starts, and
the paged commit below.

Not built yet, and what each does instead:

- **`map()` and `viewer(terms)`.** `tesseradb.Map(db.viewer_url, token=...)` is the widget, and
  `tesseradb.authorise(db.session_url, db.session_credential, terms)` is the token for it.
- **An attribute, a vocabulary, a view or a view group declared after the first commit.**
  `tessera check --payloads` emits the runtime declaration body for a layer and for nothing else,
  so those four verbs refuse on a built database and name a rebuild. A layer and a label set are
  declarable at any commit.
- **A view group, a spatial or attribute membership, a shape and an inline artifacts table.** The
  typed verbs refuse each, saying it is not built; `declare(kind, block)` writes any block it is
  given, so the declaration surface is reachable in full.

## The commit after the first

The first commit builds. Every commit after it pages the staged deltas through the control plane
of the server the database is already running, and then waits for the publication that makes them
visible.

```python
db.stage("points", new_papers)                 # a delta: rows to add to what the source holds
db.stage("kmeans", clusters)                   # its artifacts table
db.stage("kmeans_members", members)            # (level, key, rank, entity)
print(db.check())                              # the plan and the pre-flight, with nothing sent
print(db.commit())                             # the report
```

`stage(name, data)` binds a delta on a source the declaration already knows. `check()` returns the
plan and the pre-flight and sends nothing; `commit()` runs the same plan and returns the report:
rows accepted per view, artifacts minted, memberships joined, parts already present, refusals by
row and part, and how long the flush took.

The order is fixed: declarations, then points per view with the allocation view first, then values
on entities that already exist, then artifacts per layer in dependency order, then a flush. A row
is addressed by its external id, which is the source id in eight little-endian bytes at both doors,
and the first commit passes `--mint-external-ids` so that every built row is addressable.

A page's batch id is derived from the source name, the page index and a hash of the bytes, and the
commit log under `.tessera/` records each acknowledgement. Re-running a cell therefore sends
nothing: the same frame produces the same pages, and the log already holds them. A value that
changed is a `409` on that part, reported and not retried, because an edit is a delete and a
re-ingest. A `429` is backpressure and is retried after its `Retry-After` with identical bytes.

The pre-flight runs before a byte is sent. Rows outside a view's frame are dropped and listed with
the frame, rows whose id this database already holds are listed and not sent, and a column no block
declares is refused by name.

`remove(ids)`, `suppress(ids)` and `unsuppress(ids)` take the user's own ids and map them; a
removed id staged again goes as a point row. `leave(layer, key, ids, rank)` shrinks a content's
generating set, which is the one set that may shrink.

The binary is `TESSERA_BIN` when set, else the first `tessera` on `PATH`, else a checkout's target
directory, release before debug; `create()` names the one it found. The database directory keeps
its own session credential, operator credential and identity key under `.tessera/`, each file
owner-only. `commit()` starts `tessera serve` as a child process on loopback at port 0 and reads
the three bound addresses from the JSON line the child prints once all three planes are listening;
`db.viewer_url`, `db.session_url` and `db.session_credential` are what a token is minted against.
The child is killed by its pid at `close()` and at interpreter exit.

**Not built yet: the viewer plane's origin list for a notebook page.** The SDK writes
`serve.cors_origins` from `TESSERA_NOTEBOOK_ORIGIN`, and a widget served from an origin that names
none is refused by the browser. `serve.cors_loopback`, which would admit any page served from a
loopback address, is a disclosure ruling (python-sdk.md §11.2 B).

## The entry point is a token

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
