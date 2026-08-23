# Tessera in a Python process — investigation

**Status:** Non-normative investigation. Nothing here is specified; the rulings it names are the
owner's. Written 2026-08-18 against `artifacts/stage-4`. §2's three modes and the scope ruling that
follows them are the owner's (2026-08-18); everything else is the investigation behind it.

**Reads with:** [`roadmap.md`](../../roadmap.md) §2 (Python SDK, [#47]) and §3 ([#51] embed the
engine in the caller's process) — this memo is the investigation those two entries were waiting
for; [`design/client-interaction.md`](../../design/client-interaction.md) §7 (the recommended
topology, and why the demo's is development-only);
[decision 0014](../../decisions/0014-i10-weakened-to-construction.md) (what the identifier
defends, and against whom).

---

## The result

**The shape is an embedded library first, with the server as a thing built on top of it rather than
a mode of it** — the DuckDB/SQLite/LanceDB position (§7, camp 1), and the one Tessera's layering
already anticipates. `tessera-engine` and `tessera-store` are synchronous libraries that
`check-layers.sh` forbids a `tokio` edge; every async, HTTP and configuration concern lives in
`tessera-server` alone, and `EngineConfig`'s own documentation addresses "an embedder constructing
this struct directly" in three places. The build pipeline is a library call. Responses are already
Arrow IPC, which pyarrow reads without a copy.

**Three embedding modes, and only the third involves authorisation** (§2). Local development and a
data scientist building a widget over their own large corpus are both single-principal — the caller
already holds the bundle, and masking is scenery. A datamap service inside an existing Python stack
is not: there the Python process is the trusted party and its *users* are the ones being masked
from, and it needs the mask to be exactly right.

**Modes (a) and (b) are the target; (c) is deferred, and the reason is availability rather than
disclosure** (owner ruling, 2026-08-18). The masking survives embedding intact — in-process it is a
**correctness** property and not a **containment** one, which is exactly what a trusted intermediary
would need. What does not survive is everything the server layer does *above* the engine: compute
admission, the queue bound, the admission timeout and the stream budgets are all `tessera-server`
configuration, so an embedded application serving many end-users has no gate in front of an
expensive viewport and nothing to hand back but the wait (§3.3). (a) and (b) are single-principal
and do not care; (c) is the only mode with users to starve, and it is deferred on that.

**What that buys is a much smaller first delivery** — no session plumbing to design for a caller
who is their own principal, no admission layer to hoist — and §4 states the four things that keep
the path to (c) open rather than closing it by accident.

**The one thing that can fail rather than merely cost** is reaching a kernel-local port from the
browser under JupyterHub (§6). It should be spiked before the widget is built.

---

## 1. Three shapes, and they are not one project

**E — the engine in the process.** A pyo3 extension over `tessera-engine`. No HTTP, no socket,
Arrow straight out to pyarrow. Roadmap [#51], and the primary deliverable under camp 1.

**S — the server in the kernel.** `tessera.serve(bundle) -> handle`: the existing axum server on a
background thread or a child process, ephemeral ports, control over a unix socket. Not a rival to
E — the thing the viewer talks to, and the cheapest way to prove the lifecycle before any binding
exists.

**V — the viewer in the notebook.** An anywidget wrapping the deck.gl viewer, pointed at S.

**V rides on S and not on E, and that is a design decision rather than a convenience.** The viewer
consumes a streamed sequence of length-prefixed Arrow IPC frames over `fetch` (`tessera-wire`'s
`payload.rs`; `streamed-serving.md`), and every pan is a fresh masked viewport. Carrying that over
the Jupyter comm channel would be a **second transport for one contract**, and therefore a second
conformance surface forever. Against a kernel-local HTTP server it is the code that already exists,
unmodified — which is also what every comparable interactive-at-scale notebook tool does (§7.3).

---

## 2. The three modes, and the scope ruling

| | Who holds the bundle | Who is masked from whom | Auth |
|---|---|---|---|
| **(a) Local development** | the developer | nobody | none |
| **(b) A data scientist's own widget over a large corpus** | the scientist | nobody | none |
| **(c) A datamap service inside a Python stack** | the application | the application's **users** | **required** |

**(a) and (b) are the scope; (c) is future work** (owner ruling, 2026-08-18) — see §3.3 for the
reason and §4 for what keeps it reachable.

(a) and (b) are single-principal. The caller has the bundle, so a mask withholds nothing from them
that they could not read directly; the value Tessera delivers there is the Morton/Roaring machinery
at scale, not the access control. Both should be able to run with a trivially permissive principal
and never think about terms.

**(c) is where the invariants keep working, and the reason they do is that the session surface is
the *engine's*, not the server's.** This is why deferring (c) is a deferral and not an exclusion:
nothing about it needs a different engine. `Engine::authorise` returns a `Session`; the mask composition,
the post-mask sampling, the label containment and the one-way filter frontier all sit below the
HTTP layer. An embedded (c) application authorises once per end-user and queries per session
exactly as the server does, with the socket removed. Camp 1 therefore does **not** mean dropping
the authorisation surface — only restating what it defends.

### What (c) would inherit as its own obligation

Recorded now because it is what a later ruling has to price, not because anything acts on it yet.
Three things the service does today that an embedding application would be taking on:

- **The term mapping (I5).** The obligation to make the data-side and auth-side meanings of a term
  agree is already the caller's and already unverifiable. In (c) the caller is application code
  rather than a deployment's plugin, which makes it easier to get wrong and no easier to check.
- **Not leaking the handle.** The application must not pass an engine handle, a bundle path or
  another user's session to its own users. This is ordinary application discipline, but it is the
  whole boundary.
- **Everything the server does above the engine.** See §3.3 — the one that is a gap rather than a
  note, and the one that defers the mode.

---

## 3. What E costs

### 3.1 Mechanics

- **`croaring` is a C dependency and the store mmaps.** Wheels need cibuildwheel/manylinux —
  routine, not free. `memmap2` means the bundle must be on a real filesystem (no zipimport), and
  Windows holds the file open for the process's life.
- **The GIL must be released around every engine call.** They are all blocking and some are long.
  Deliberate, not automatic. DuckDB's binding is the reference for this and for the Arrow handoff.
- **Threads.** `compute_threads` defaults to filling the machine — right for a server, wrong for a
  notebook sharing a laptop with a browser and deck.gl. An embedded default should be a stated
  fraction of the cores.
- **Lifecycle.** The write executor is a background thread, and `WritePath` owns the only handle
  precisely so its drop can join it. That must happen on kernel shutdown or the kernel hangs on
  exit: an explicit `close()`, an `atexit` hook, a context manager.
- **Layering applies to the binding.** `check-layers.sh` denies `tessera-server → tessera-store`
  and `→ tessera-authz` so the server sees engine API types only. A `tessera-py` crate sits in the
  same position and should acquire the same deny rows **in the same change** — afterwards is after
  it has already reached past the engine.

### 3.2 The fail-closed refusals must not be routed around

`Config`'s loader refuses `k_min = 0` rather than clamping, because at zero §7.2's floor clause is
silently off and the sparsest principals' maps go blank (I7); it refuses `tier_width < 2`, because
below 2 merging is disabled rather than eager. `EngineConfig` admits it cannot enforce these itself
— "an embedder constructing this struct directly is on its own honour". **An embedded constructor
should acquire those refusals rather than inherit the honour system**, whether by generating TOML
through the existing loader or by lifting validation into a constructor both routes use. One
refusal site either way.

### 3.3 Why (c) is deferred: availability lives in the layer it would skip

Compute admission, the queue bound, the admission timeout, stream flush/stall/deadline budgets,
ingest admission and batch bounds are all `tessera-server` configuration, above the engine. An
embedded (c) application serving many end-users gets **none** of them: one expensive viewport
occupies the shared rayon pool with no gate in front of it, and there is no `retry_after` to hand
back.

This is not a disclosure problem — I13a's refusals and cancellation are engine-side and survive
intact, as does every masking invariant (§2). It is an availability problem, and it lands entirely
on the one mode with users to starve. **So (c) is deferred** (owner ruling, 2026-08-18): (a) and
(b) are single-principal and the gap cannot reach them, and a deployment that needs (c) today runs
the server, which is where the gate already is.

Reaching (c) later means choosing one of three, and none is foreclosed by shipping (a) and (b)
first: hoist admission into the engine, expose it as an embeddable middle layer between engine and
server, or rule that (c) runs S in-process and E stays single-principal for ever.

---

## 4. What keeps the path to (c) open

Four things that are cheap while E is being designed and expensive once it has callers. None of
them is work *for* (c); each is the absence of a shortcut that would have to be un-built.

**The binding's only entry point is a session.** A caller who is their own principal still
authorises and still queries through the mask. The tempting shortcut — an "unmasked" or
"single-principal fast path" that skips composition because there is nothing to hide — is the one
thing that must not be built: it would be an I1/I2 bypass living permanently in the tree, reachable
by any later caller, and removing it is what reaching (c) would then cost. A permissive principal
costs almost nothing; the mask is a bitmap intersection either way.

**The binding does not reach past the engine.** `check-layers.sh` denies `tessera-server →
tessera-store` and `→ tessera-authz`; a `tessera-py` crate takes the same rows **in the change that
creates it**. Afterwards is after it has already reached.

**The fail-closed refusals are shared, not duplicated** (§3.2). Two validation sites disagree
eventually, and the one embedded callers hit would be the newer and thinner.

**§3.3's absence is documented at the site.** An embedded engine has no admission gate, and that
should be a stated property of the Python surface rather than something a (c) user discovers under
load. Present tense about absent machinery reads as an assurance
([decision 0013](../../decisions/0013-mark-specified-vs-implemented.md)).

---

## 5. What S costs

Small, and mostly already built.

**Ports.** Three listeners — `viewer_addr`, `session_addr`, control. Bind the two TCP ones to `:0`
and read back what was assigned; control takes `ControlListen::Unix`, which already exists. The
viewer/session split is a deployment-topology property (client-interaction §7) and collapsing it in
an embedded shape is a configuration choice, not a specification change — but an explicit one,
because a proxied notebook has to reach both.

**Configuration and lifecycle** are §3.2 and §3.1 verbatim.

**The judgement: the session credential reaches the browser.** `run_demo.sh` already flags this and
`dev_cors_origins` exists only to permit it. For one user on their own machine — modes (a) and (b)
— that is honest, since the credential and the bundle are already theirs. It is **not** a hosting
shape, and mode (c) serving real users needs the real topology.

---

## 6. What V costs, and the risk that is real

The widget is ordinary. The risk is the network path.

| Environment | Reaching a kernel-local port | Status |
|---|---|---|
| VS Code notebooks | automatic port forwarding | works |
| Colab | `output.serve_kernel_port_as_iframe` | works |
| Local Jupyter Lab | direct to `localhost:<port>` | works, subject to CORS |
| JupyterHub / remote | `jupyter-server-proxy`, under a path prefix | **needs work — below** |

**Two concrete blockers under a path prefix, both present today and both small.** The viewer
fetches `/datasets.json` and `/clusters.json` from the origin root, so a prefix breaks them; and
`dev_cors_origins` is an enumerated list, while a notebook's origin varies by host and port and is
not knowable until runtime. Fixes in existing code — a configurable base path, and a CORS origin
the embedding layer supplies — not new mechanism. **Spike this before building the widget.**

Smaller: the client creates a decode web worker in a browser, and worker construction inside a
bundled anywidget ESM module is fiddly. The fallback exists — `decoder:` on
`TesseraClientOptions` runs it inline — so this is an optimisation to get right, not a
precondition.

---

## 7. Prior art

### 7.1 The three camps

**Camp 1 — embedded-first, no authorisation model.** DuckDB, SQLite, LanceDB, RocksDB, Lucene. The
library is the product; a server, where one exists, is a separate thing built on top (MotherDuck
over DuckDB, Elasticsearch over Lucene) rather than a mode of it. They get a clean in-process story
because **there is no access-control claim to weaken** — security is the filesystem. LanceDB is the
closest architectural match: Rust core, pyo3, Arrow-native, and `connect()` chooses embedded or
remote from the URI scheme behind one API. DuckDB is the reference for the binding mechanics — GIL
released around the core, zero-copy Arrow in both directions, and replacement scans that let a
query name a live Python object as a table.

**Camp 2 — server-first, with a local mode that is a *second implementation*.** Qdrant.
`QdrantClient(":memory:")` does not embed the Rust engine; it is a Python reimplementation of the
query semantics over numpy, documented for prototyping and testing and not for scale. Same client
API, two engines. **This route is already closed here** — the Python reference oracle must not
become the SDK, and its entire value is in not sharing code with the engine (roadmap, [#47]). A
Python "local mode" reimplementing masking would leave the conformance suite testing one
implementation while callers ran the other, which is the exact failure the suite exists to catch.

**Camp 3 — never embedded.** Postgres. Row-level security means something precisely because the
boundary is a process the querier does not control, and the absence of an in-process Postgres is
not an oversight.

Tessera's *claims* are camp 3 and its *architecture* is camp 1, which is why [#51] opens with a
ruling rather than with work.

### 7.2 The precedent that supports the ruling

**SQLite's authorizer callback.** It lets an application restrict the SQL it will accept, and its
documented purpose is to run *less-trusted* queries safely within the application's own process —
not containment against the process, correctness within it. That is the same claim §2 makes for
mode (c), in a system that has been widely audited for twenty years. Worth citing when the ruling
is written, because it establishes that the smaller claim is a respectable position rather than a
retreat.

**chdb** — the in-process ClickHouse for Python — began as a wrapper around the existing
`clickhouse-local` binary before becoming a real binding. That is §9's staging: prove the API and
the lifecycle against a subprocess, then decide whether the address space earns its cost.

### 7.3 The notebook viewer

**Arrow over the Jupyter comm channel does work.** `lonboard` runs deck.gl in a notebook via
anywidget and ships Arrow rather than JSON precisely to get past the ~10⁵-point wall that limits
`pydeck`. So the channel is not the obstacle it first appears to be — but lonboard **pushes a fixed
dataset once**, which is not Tessera's interaction model.

**The matching precedent is Bokeh, Panel and datashader**, which run a real HTTP server inside the
kernel and point the widget at it, with `jupyter-server-proxy` for the remote case. Datashader is
the closest functional analogue Tessera has — server-side rasterisation, viewport requests, more
data than can be shipped — and it uses the kernel-local server. §1's recommendation follows the
tool whose interaction model matches, not the one whose rendering stack does.

---

## 8. The ingest-development loop

Less of a gap than it looks. `tessera_build::build` takes Parquet paths for points and the
`(entity_id, term_id)` pairs relation, and the streaming pipeline reads by row group — the right
seam, not an inconvenience, since Python writes Parquet natively. `tessera.build(points_df,
pairs_df, …)` is a wrapper that writes a temp directory and calls the library. `build_in_memory`
exists but is the byte-equality oracle for the streaming path and should stay that.

What wants ergonomic work is the **identity key**. A build needs one carried across rebuilds or
every `tessera_id` any client holds silently breaks (I9), and a loop rebuilding twenty times an
hour wants a throwaway key that is *obviously* throwaway — the CLI's `--mint-id-key` semantics,
surfaced so the difference between "scratch" and "the deployment's key" cannot be got wrong by
accident.

---

## 9. What to do first

1. **Write [#51]'s ruling down** — §2's three modes, the correctness/containment statement, and the
   scope: (a) and (b) now, (c) deferred on §3.3. Mostly settled already; what it owes the corpus is
   a decision record, because "run it embedded and the masking still protects me" is exactly the
   plausible mistake this system exists to prevent.
2. **The proxy spike** (§6). The existing viewer under a path prefix behind `jupyter-server-proxy`
   against a running `tessera serve`. Go/no-go for V, and it costs a day.
3. **S as a managed subprocess**, plus [#47]'s Python client to talk to it — the chdb staging.
   Proves configuration, ports, lifecycle and shutdown with no Rust binding written.
4. **V**, on what 2 and 3 establish.
5. **E**, under §4's four constraints — the camp 1 deliverable, and the only genuinely new epic
   here.

## What this memo does not establish

- **Nothing here is measured.** No probe was run. In particular the assumption that localhost Arrow
  is fast enough that E's advantage over S is API ergonomics rather than latency is *modelled*, and
  if E is ever justified on latency it needs a number first.
- The three-listener collapse, the CORS shape and the path-prefix fixes are read from the source,
  not exercised.
- §3.3's admission gap is identified from the configuration surface, not from a starvation test. It
  is the reason (c) is deferred, and it has not been measured.
