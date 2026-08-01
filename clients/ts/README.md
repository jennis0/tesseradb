# Tessera clients — an instrument, not a product

Two TypeScript packages and a headless-browser smoke test, built to answer one question:
**does a running Tessera actually work?**

- `core/` — `@tessera/client`. The four viewer/session verbs, the framed-Arrow decoder, the
  coordinate arithmetic. Stateless: no cache, no epoch, no replica state.
- `viewer/` — a Vite + deck.gl app. All UI, all state.
- `spike/` — the deck.gl tile-convention spike, kept as a regression guard.

Design: [`docs/superpowers/specs/2026-08-01-mvp-client-and-deckgl-viewer-design.md`](../../docs/superpowers/specs/2026-08-01-mvp-client-and-deckgl-viewer-design.md).
It is the first slice of
[`2026-07-31-client-interaction-architecture-design.md`](../../docs/superpowers/specs/2026-07-31-client-interaction-architecture-design.md),
which owns the client architecture proper.

---

## ⚠ Development only

Two things here exist **only** to let a browser talk to a local bundle, and neither is a pattern
to copy:

- **`serve.dev_cors_origins`** in `tessera.toml` lets an enumerated browser origin call the viewer
  and session planes. It is off unless typed, has no wildcard and no environment variable, and the
  server logs a warning at `warn` when it is on.
- **`VITE_TESSERA_SESSION_CREDENTIAL`** puts the deployment's *session credential* into the browser
  bundle, because `POST /session/authorise` is gated by it and the viewer re-authorises whenever
  you switch principal.

The documented integration topology is **T2 with verified assertions** — credential construction
at the integrator's app server, where the authority is (client-interaction §7). Nothing here
revises that.

## Prerequisites

- Node 22 (`node -v`)
- A release binary: `cargo build --release`
- A built bundle. `data/bench-fixtures/{2m4,1e8,1e9}` are the development ones.

## Running it

Write `clients/ts/dev-server.toml` (untracked):

```toml
[bundle]
path = "data/bench-fixtures/2m4"
cache = "clients/ts/.dev/cache"
wal = "clients/ts/.dev/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
min_visible_members = 10
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
dev_cors_origins = ["http://localhost:5173"]
```

and `clients/ts/viewer/.env.local` (untracked):

```
VITE_TESSERA_VIEWER_URL=http://127.0.0.1:37585
VITE_TESSERA_SESSION_URL=http://127.0.0.1:49303
VITE_TESSERA_SESSION_CREDENTIAL=dev-session-credential
```

Then:

```bash
cargo build --release
mkdir -p clients/ts/.dev
export TESSERA_SESSION_CRED=dev-session-credential
export TESSERA_OPERATOR_CRED=dev-operator-credential
./target/release/tessera serve -c clients/ts/dev-server.toml &

cd clients/ts && npm install
node scripts/measure-principals.mjs --terms 0..200   # writes viewer/presets.json
npm run dev -w @tessera/viewer                       # http://localhost:5173
```

The Vite port is `strictPort`: the origin is enumerated in `dev_cors_origins`, so a silent
fallback to 5174 would produce a CORS failure that reads as a broken server.

## Regenerating the two generated artifacts

**`viewer/presets.json` — per fixture.** The term dictionary differs between bundles, so presets
measured against `2m4` are meaningless against `1e8`. Each preset's `visible` is measured with a
`zoom = 0` full-extent call, whose `visible` *is* that principal's visible-set size.

```bash
TESSERA_SESSION_CRED=… node scripts/measure-principals.mjs --terms 0..200
```

**`core/test/fixtures/*` — per wire change.** A decoder test passing against a stale golden is
worse than no test.

```bash
TESSERA_SESSION_CRED=… node scripts/capture-golden.mjs --terms 0
```

## Testing

```bash
npm test                       # spike + core: tile arithmetic, framing, decode, coords
cd core && TESSERA_LIVE=1 TESSERA_SESSION_CRED=… npx vitest run test/client.live.test.ts
cd viewer && node smoke.mjs    # drives the page in headless chromium
```

`smoke.mjs` reports what the page actually did — requests and their statuses, the counts each
principal reported, whether marks accumulate on zoom, lit canvas pixels, console errors — and
writes a screenshot. It is **not** the assertion. The owner looking at the map is; the smoke test
exists so "it builds" can be upgraded to "it ran" without a human in the loop.

Headless chromium needs `npx playwright install chromium-headless-shell` once.

## What this deliberately does not do

Each of these is a recorded decision (design §7), not an oversight:

- **Epochs and cross-channel consistency.** The bundle is static for this exercise; the counts
  panel sums responses fetched at different times, which is wrong the moment ingest runs.
- **Reconciliation, prefix declarations, the session cursor.**
- **The change signal**, and the refresh affordance client-interaction §4 makes mandatory.
- **`{shown, total}` as an inseparable type.** The discipline is honoured in the panel; it is not
  enforced by the type system, so a future panel can still render a bare sample count.
- **The four display states.** Only the failure case survives: a failed tile is listed, and a
  viewport with no tiles and a recent failure reads *counts unavailable*, never *zero*.
- **The *k* non-decreasing rule (P6).** The slider can lower `k`. This survives only until the
  replica store owns `k`.
- **The conformance kit, the obligations list, the tile-addressed GET alias, labels, filters,
  export, and the Python client.**

## Things worth knowing before you read a number

**`k` is usually not binding.** §7.2 selects by a threshold θ *and* a cap `k`; the cap only bites
when θ would admit more than `k`. Against these fixtures θ decides everywhere sampled, so moving
the `k` slider changes nothing and the map is right to ignore it. The panel says which clause is
deciding, measured from the served counts. Raise `serve.theta_target_marks` to make `k` bite.

**The request bbox is inset to cell centres, deliberately.** `tessera-spatial`'s `tile_corners`
quantises both corners and iterates inclusively, so a bbox closed on the tile boundary names the
neighbouring row and column too — one request would answer for four tiles, double-counting and
over-plotting. See `core/src/coords.ts`'s `tileToRequestBbox`.

**`x-tessera-stage-ns` is usually absent.** It needs both the `bench-timing` cargo feature and
`[serve] stage_timing = true`. Absence is a configuration fact, not an error, and the stats panel
says so.

## A free positional ground-truth check

The synthetic corpus carries **deliberate structures** (`probes/dataset.md` §"Replicas 0–4 pin
deliberate edge cases"), and they are the cheapest correctness check the viewer has: if the
positional pipeline — quantisation, Morton, the request bbox, the data→world transform — were
wrong anywhere, they would smear or move.

What to look for at 1e9 with a broad principal:

- **A sharp vertical line at the extent midpoint.** Confirmed in the data, not a rendering
  artefact: the midpoint column carries 1.37× its neighbouring column and 1.51× the equivalent
  row, with 1,339 of ~1 M sampled points at exactly `x = 32767`.
- **Dense edges at `x`/`y` = 0 and 65535** — the corner-pinned replicas (2 and 3) hitting the
  quantisation clamps. 26,444 sampled points sit at `x = 0`.

If the line renders crisp and vertical and the edges are dense, positions are right. *(Note for
whoever next touches the fixtures: `probes/dataset.md:131` describes replica 4 as "degenerate line
(y collapsed)", which would render **horizontal**. The measured structure is constant-**x**. One of
the two is mislabelled — harmless, but it cost an investigation once.)*

## Measured

_Machine: WSL2. Figures from the stats panel and `smoke.mjs`; re-measure rather than trusting
these._

### 2m4 (2,422,486 items, 109 MB)

Principal presets, measured:

| preset | terms | visible |
|---|---|---|
| narrow — term 14 | 1 | 243 |
| medium — term 112 | 1 | 12,465 |
| broad — term 79 | 1 | 181,900 |
| everything | 176 | 2,422,486 |

At depth 2 over the full extent, principal *everything*: 6 non-empty tiles, `served` 221 of
`visible` 1,994,089, ~1.2 ms server time, ~4.2 KB per tile with a `+3` underlay. Panning and
zooming are immediate.

### 1e8 (4.4 GB)

Presets: 1,205 / 19,327 / 4,273,751 / **53,300,931** (201 terms). All 201 measured in 3.5 s.

Per-tile median over five runs, principal *everything*:

| depth | server | wall | bytes |
|---|---|---|---|
| 0 | 129.4 ms | 131.3 ms | 2,132 |
| 2 | 11.1 ms | 12.7 ms | 2,132 |
| 4 | 0.88 ms | 1.6 ms | 2,324 |
| 6 | 0.29 ms | 0.8 ms | 2,836 |
| 8 | 0.23 ms | 0.9 ms | 5,460 |
| 6, underlay +3 | 0.41 ms | 0.9 ms | 4,252 |

### 1e9 (44 GB)

It runs. Opening the bundle took **35 s**; process RSS stayed ~3 GB with the rest in page cache.
Presets: 1,366 / 21,006 / 42,025,228 / **518,502,081** (201 terms), all measured in 26 s.

| depth | server | wall | bytes |
|---|---|---|---|
| 0 | 1,276 ms | 1,278.7 ms | 2,324 |
| 2 | 88.2 ms | 89.6 ms | 2,324 |
| 4 | 1.85 ms | 2.6 ms | 2,068 |
| 6 | 0.41 ms | 1.1 ms | 2,132 |
| 8 | 0.23 ms | 0.9 ms | 2,324 |
| 6, underlay +3 | 0.53 ms | 1.2 ms | 3,676 |

**Cost falls with depth, and payload is flat.** From depth 0 to depth 8 the server time drops by
four orders of magnitude while the response stays ~2 KB — screen area, not corpus size. The
corpus grew 10× from 1e8 to 1e9 and depth-6-and-below timings did not move.

**The shallow end is the cost, and at 1e9 it sheds.** A depth-0 tile over a 518M-item visible set
takes ~1.3 s, which is long enough that stacking several — switching principal mid-load, say —
saturates the compute-admission gate and the server sheds with `429 backpressure`. The viewer
reports that honestly (*counts unavailable — requests failed*, and the failure listed with its
code) rather than drawing an empty map. **The MVP does not retry**, though the server sends
`Retry-After: 1`; a client with a replica store should. Zoom in one or two levels and it is
immediate again.

At depth 3 over the full extent, principal *everything*: **21 of 518,502,081 shown**.
