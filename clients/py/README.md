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

**What is here is stage S1 of python-sdk.md §12:** `create`, `open`, `stage` with the id map,
`declare` and the typed verbs for plain views, vocabularies, attributes, enumerated layers of every
kind and labels, inference, `check()`, and the first commit. `map()` and `viewer(terms)` are S2;
later commits — a delta into a running database — are S3, and the verbs that would start one refuse
naming it. A spatial or attribute membership, a shape, an inline artifacts table and a view group
are S4 and S5; the typed verbs refuse each naming its stage, and `declare(kind, block)` writes
whatever block it is given in the meantime.

The binary is found at `TESSERA_BIN`, on `PATH`, or in a checkout's target directory. The database
directory keeps its own session credential, operator credential and identity key under `.tessera/`,
owner-only; `commit()` starts `tessera serve` as a child process on loopback at port 0 and reads
the three bound addresses from the JSON line the child prints. It kills that child by its pid at
`close()` and at interpreter exit.

**Not built yet on `main`: the announce line.** `tessera serve` binds the addresses the deployment
file names and announces nothing, so `commit()` builds and then waits for a line that does not come
and says so, naming python-sdk.md §11.2 A. There is no port-guessing fallback: the SDK would be
racing another process for the port it guessed.

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
