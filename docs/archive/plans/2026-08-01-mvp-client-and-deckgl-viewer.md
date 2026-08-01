> **ARCHIVED 2026-08-01 — SUPERSEDED by the epic model. Live at the time of archiving; remaining work is tracked in GitHub issues.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# MVP Client and deck.gl Viewer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a runnable instrument — a TypeScript client and a deck.gl viewer — that can be pointed at a live `tessera serve` process so the owner can see that marks land correctly, counts are honest, masking is real, and it stays interactive at scale.

**Architecture:** Two npm-workspace packages under `clients/ts/`: `core` (stateless — the four verbs, the framed-Arrow decoder, coordinate arithmetic) and `viewer` (a Vite app owning all state and UI, with deck.gl's `TileLayer` over an `OrthographicView`). The engine's 2¹⁶×2¹⁶ Morton cell grid is mapped onto a 512×512 deck.gl world so that a deck tile `z` equals a Morton depth exactly. The only Rust change is one dev-only, off-by-default CORS key.

**Tech Stack:** TypeScript 5.9, Vite 8, Vitest, deck.gl 9.3 (`@deck.gl/core`, `@deck.gl/layers`, `@deck.gl/geo-layers`), apache-arrow 21, Node 22, npm workspaces. Rust side: axum 0.8, tower-http 0.6 (`cors` feature).

## Global Constraints

- **Spec:** `docs/archive/plans/2026-08-01-mvp-client-and-deckgl-viewer-design.md`. Where this plan and the spec disagree, the spec governs; where the spec and `docs/design/contracts.md` disagree, the contracts spec governs.
- **British spelling** in prose, comments and identifiers where a choice exists (`authorise`, `visualisation`, `colour`). The HTTP route is `/session/authorise` — spelled that way in the server already.
- **The server is authoritative for every masked quantity.** The client renders `visible`/`matched`/`served` as received; it never computes, estimates or interpolates one. A drawn-mark count is never presented as a total.
- **Node 22, npm workspaces.** No pnpm or yarn — `pnpm` is not installed on this machine.
- **`data/` is gitignored.** Fixtures live at `data/bench-fixtures/{2m4,1e8,1e9}`; `2m4` (109 MB, 2,422,486 items) is the development fixture.
- **Fixture facts, verified 2026-08-01:** slice id `s0`; `quantisation` is `x_min=0, x_max=65536, y_min=0, y_max=65536`; `declared_scalars` is `[]`; terms are numeric strings `"0"`…~`"170"`; no external ids were minted.
- **`x-tessera-stage-ns` is optional.** It is emitted only in a binary built with the `bench-timing` cargo feature *and* with `[serve] stage_timing = true`. Every panel reading it must degrade to absent. `x-tessera-server-us` and `x-tessera-admission-us` are always present on `/v1/viewport`.
- **Error bodies** are `{"error": "<code>", "detail": "<string>"}` with an optional `retry_after_s`. Codes seen on the viewer plane: `bad-credential` (401), `expired-token` (403), `unknown` (404), `pin-expired` (410), `contract` (422), `backpressure` (429), `fail-closed` (500), `not-ready` (503).
- **A failed request never renders as an empty region.** This is the one display-state rule the MVP keeps (spec §5, §7).

---

## File Structure

```
clients/ts/
  package.json                     npm workspace root; scripts: build, test, dev
  tsconfig.base.json               shared compiler options
  README.md                        how to run the whole thing (Task 8)
  spike/
    package.json
    vitest.config.ts
    index.html                     the visual spike page
    src/convention.ts              TILE_SIZE, WORLD_SIZE, tileToCellBox — the measured convention
    src/convention.test.ts         headless assertions against deck.gl's own Tileset2D
    src/spike.ts                   synthetic getTileData drawing index + bbox
  core/
    package.json
    vitest.config.ts
    src/index.ts                   public surface re-exports
    src/coords.ts                  cell/world/data-space transforms; tile → data bbox
    src/frame.ts                   split the concatenated Arrow IPC streams
    src/decode.ts                  framed bytes → ViewportResult
    src/client.ts                  TesseraClient: authorise, meta, viewport, item
    src/types.ts                   wire and result types
    test/fixtures/                 golden payloads captured from the running server
    test/*.test.ts
  viewer/
    package.json
    index.html
    vite.config.ts
    src/main.ts                    wiring: config, client, deck instance, panels
    src/config.ts                  reads VITE_* env; the principal preset list
    src/map.ts                     OrthographicView + TileLayer + sublayers
    src/underlay.ts                sub-cell counts → RGBA image, eq_hist colouring
    src/state.ts                   the single mutable app state + subscribe/notify
    src/panels/counts.ts
    src/panels/principal.ts
    src/panels/stats.ts
    src/panels/item.ts
    src/panels/errors.ts
    src/style.css
    presets.json                   generated by the measuring script (Task 5)
  scripts/
    capture-golden.mjs             Task 3: capture Arrow fixtures from a live server
    measure-principals.mjs         Task 5: measure each term's visible-set size
crates/tessera-server/
  src/cors.rs                      NEW: the dev-only CORS layer
  src/config.rs                    MODIFY: serve.dev_cors_origins
  src/state.rs                     MODIFY: AppState.dev_cors_origins
  src/viewer.rs, src/session.rs    MODIFY: apply the layer in router()
  src/lib.rs                       MODIFY: pass the config through; startup warning
  tests/common/mod.rs              MODIFY: the new AppState field
  tests/cors.rs                    NEW
```

---

### Task 1: The tile-arithmetic spike

Gates every later task. Contains no Tessera. Its purpose is to settle deck.gl's non-geospatial tile convention empirically — spec §4.2, and client-interaction §15's first open question.

**The hypothesis under test.** deck.gl's docs say: *"For non-geospatial views like OrthographicView or OrbitView, x and y increment from the world origin, with tile dimensions defined by the `tileSize` prop."* So with `tileSize = 512` and a world of `512 × 512` units, tile `(0,0,0)` covers the whole world and a tile at `z` covers `512/2^z` world units — which maps one-to-one onto a Morton cell block of `65536/2^z` cells. Hence `WORLD_SIZE = 512`, `CELLS_PER_WORLD_UNIT = 128`, and **deck tile `z` ≡ Morton depth**.

The test asserts that. If deck.gl disagrees — particularly on the y direction — fix `convention.ts` to match what deck.gl actually does, not the other way round.

**Files:**
- Create: `clients/ts/package.json`, `clients/ts/tsconfig.base.json`
- Create: `clients/ts/spike/package.json`, `clients/ts/spike/vitest.config.ts`, `clients/ts/spike/index.html`
- Create: `clients/ts/spike/src/convention.ts`, `clients/ts/spike/src/convention.test.ts`, `clients/ts/spike/src/spike.ts`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: nothing.
- Produces: `WORLD_SIZE: number`, `TILE_SIZE: number`, `CELL_GRID: number`, `MAX_DEPTH: number`, `type TileIndex = {x: number; y: number; z: number}`, `type CellBox = {cx0: number; cy0: number; cx1: number; cy1: number}`, `tileToCellBox(index: TileIndex): CellBox`. Task 3 moves these verbatim into `core/src/coords.ts`.

- [ ] **Step 1: Scaffold the workspace**

`clients/ts/package.json`:

```json
{
  "name": "tessera-clients",
  "private": true,
  "workspaces": ["spike", "core", "viewer"],
  "scripts": {
    "test": "npm run test --workspaces --if-present",
    "build": "npm run build --workspaces --if-present"
  }
}
```

`clients/ts/tsconfig.base.json`:

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "lib": ["ES2022", "DOM"],
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "declaration": true,
    "skipLibCheck": true,
    "esModuleInterop": true,
    "forceConsistentCasingInFileNames": true
  }
}
```

`clients/ts/spike/package.json`:

```json
{
  "name": "@tessera/spike",
  "private": true,
  "type": "module",
  "scripts": {
    "dev": "vite",
    "test": "vitest run"
  },
  "dependencies": {
    "@deck.gl/core": "^9.3.7",
    "@deck.gl/geo-layers": "^9.3.7",
    "@deck.gl/layers": "^9.3.7"
  },
  "devDependencies": {
    "typescript": "^5.9.0",
    "vite": "^8.2.0",
    "vitest": "^3.2.0"
  }
}
```

`clients/ts/spike/vitest.config.ts`:

```ts
import {defineConfig} from 'vitest/config';

export default defineConfig({
  test: {environment: 'node', include: ['src/**/*.test.ts']}
});
```

Append to `.gitignore`:

```
node_modules/
clients/ts/**/dist/
```

Run: `cd clients/ts && npm install`
Expected: installs without error; `node_modules/` created.

- [ ] **Step 2: Write the failing convention test**

`clients/ts/spike/src/convention.test.ts`:

```ts
import {describe, expect, it} from 'vitest';
import {OrthographicViewport} from '@deck.gl/core';
import {_Tileset2D as Tileset2D} from '@deck.gl/geo-layers';
import {CELL_GRID, MAX_DEPTH, TILE_SIZE, WORLD_SIZE, tileToCellBox} from './convention.js';

/** The tile indices deck.gl itself would request for a viewport, via its own Tileset2D. */
function indicesFor(zoom: number, width = 1024, height = 1024) {
  const viewport = new OrthographicViewport({
    width,
    height,
    target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0],
    zoom
  });
  const tileset = new Tileset2D({
    tileSize: TILE_SIZE,
    maxZoom: MAX_DEPTH,
    minZoom: 0,
    extent: [0, 0, WORLD_SIZE, WORLD_SIZE],
    getTileData: async () => null
  });
  tileset.update(viewport);
  return tileset.tiles.map((t) => ({x: t.index.x, y: t.index.y, z: t.index.z, bbox: t.bbox}));
}

describe('the deck.gl non-geospatial tile convention', () => {
  it('covers the whole world with a single tile at z = 0', () => {
    const tiles = indicesFor(0);
    expect(tiles.every((t) => t.z === 0)).toBe(true);
    expect(tiles).toHaveLength(1);
    expect(tiles[0]!.x).toBe(0);
    expect(tiles[0]!.y).toBe(0);
  });

  it('maps deck tile z one-to-one onto Morton depth', () => {
    // A tile at depth z spans CELL_GRID / 2^z cells on each axis.
    for (const z of [0, 1, 4, 8, 16]) {
      const box = tileToCellBox({x: 0, y: 0, z});
      expect(box.cx1 - box.cx0).toBe(CELL_GRID / 2 ** z);
      expect(box.cy1 - box.cy0).toBe(CELL_GRID / 2 ** z);
    }
  });

  it('agrees with deck.gl on every tile bbox it requests', () => {
    for (const zoom of [0, 1, 2, 3, 4]) {
      for (const tile of indicesFor(zoom)) {
        const box = tileToCellBox({x: tile.x, y: tile.y, z: tile.z});
        const b = tile.bbox as {left: number; top: number; right: number; bottom: number};
        // Both axes converted to cell space; top/bottom are compared as a sorted pair so the
        // assertion does not itself assume a y direction — the next test pins that.
        expect(box.cx0).toBeCloseTo(b.left * (CELL_GRID / WORLD_SIZE), 6);
        expect(box.cx1).toBeCloseTo(b.right * (CELL_GRID / WORLD_SIZE), 6);
        const ys = [b.top, b.bottom].sort((p, q) => p - q).map((v) => v * (CELL_GRID / WORLD_SIZE));
        expect([box.cy0, box.cy1].sort((p, q) => p - q)).toEqual([
          expect.closeTo(ys[0]!, 6),
          expect.closeTo(ys[1]!, 6)
        ]);
      }
    }
  });

  it('increases tile y in the same direction as cell y', () => {
    const lower = tileToCellBox({x: 0, y: 0, z: 4});
    const higher = tileToCellBox({x: 0, y: 1, z: 4});
    expect(higher.cy0).toBeGreaterThan(lower.cy0);
  });

  it('nests: a tile’s four children exactly partition it', () => {
    const parent = tileToCellBox({x: 3, y: 5, z: 4});
    const children = [
      tileToCellBox({x: 6, y: 10, z: 5}),
      tileToCellBox({x: 7, y: 10, z: 5}),
      tileToCellBox({x: 6, y: 11, z: 5}),
      tileToCellBox({x: 7, y: 11, z: 5})
    ];
    expect(Math.min(...children.map((c) => c.cx0))).toBe(parent.cx0);
    expect(Math.max(...children.map((c) => c.cx1))).toBe(parent.cx1);
    expect(Math.min(...children.map((c) => c.cy0))).toBe(parent.cy0);
    expect(Math.max(...children.map((c) => c.cy1))).toBe(parent.cy1);
  });
});
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cd clients/ts/spike && npx vitest run`
Expected: FAIL — `Cannot find module './convention.js'`.

- [ ] **Step 4: Write the convention**

`clients/ts/spike/src/convention.ts`:

```ts
/**
 * The mapping between deck.gl's non-geospatial tile indices and Tessera's Morton cell grid.
 *
 * The engine's world is a 2^16 x 2^16 cell grid, quantised per axis (design §2.5), so a tile is
 * square in CELL space and rectangular in data space. The viewer therefore uses cell space as its
 * deck.gl world, scaled down by CELLS_PER_WORLD_UNIT so that a tile is TILE_SIZE world units at
 * depth 0 — which makes a deck tile `z` identically a Morton depth.
 *
 * Every constant here was checked against deck.gl's own Tileset2D in convention.test.ts. Change
 * them only with that test.
 */
export const CELL_GRID = 65536;
export const MAX_DEPTH = 16;
export const TILE_SIZE = 512;
export const WORLD_SIZE = 512;
export const CELLS_PER_WORLD_UNIT = CELL_GRID / WORLD_SIZE; // 128

export type TileIndex = {x: number; y: number; z: number};
export type CellBox = {cx0: number; cy0: number; cx1: number; cy1: number};

/** The half-open cell block `[cx0, cx1) x [cy0, cy1)` a tile index covers. */
export function tileToCellBox({x, y, z}: TileIndex): CellBox {
  const span = CELL_GRID / 2 ** z;
  return {cx0: x * span, cy0: y * span, cx1: (x + 1) * span, cy1: (y + 1) * span};
}
```

- [ ] **Step 5: Run the tests**

Run: `cd clients/ts/spike && npx vitest run`
Expected: PASS — all five.

If the bbox or y-direction assertions fail, deck.gl's convention differs from the hypothesis. **Fix `convention.ts`, not the assertions**, and record what deck.gl actually does in its doc comment. If `z` does not equal Morton depth, introduce a `DEPTH_OFFSET` constant in `convention.ts`, apply it in `tileToCellBox`, and note it in the module doc — every later task reads depth through this function and nothing else.

- [ ] **Step 6: Write the visual spike page**

`clients/ts/spike/src/spike.ts`:

```ts
import {Deck, OrthographicView} from '@deck.gl/core';
import {TileLayer} from '@deck.gl/geo-layers';
import {PathLayer, TextLayer} from '@deck.gl/layers';
import {MAX_DEPTH, TILE_SIZE, WORLD_SIZE, tileToCellBox} from './convention.js';

type TileData = {label: string; box: number[][]; centre: [number, number]};

new Deck({
  views: new OrthographicView({flipY: true}),
  initialViewState: {target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: 0},
  controller: true,
  layers: [
    new TileLayer<TileData>({
      id: 'spike',
      tileSize: TILE_SIZE,
      minZoom: 0,
      maxZoom: MAX_DEPTH,
      extent: [0, 0, WORLD_SIZE, WORLD_SIZE],
      refinementStrategy: 'best-available',
      getTileData: async ({index, bbox, signal}) => {
        // A deliberate delay, so fast panning exercises the abort path.
        await new Promise((r) => setTimeout(r, 300));
        if (signal?.aborted) throw new Error('aborted');
        const b = bbox as {left: number; top: number; right: number; bottom: number};
        const cells = tileToCellBox(index);
        return {
          label: `${index.z}/${index.x}/${index.y}\ncells ${cells.cx0},${cells.cy0}`,
          box: [
            [b.left, b.top],
            [b.right, b.top],
            [b.right, b.bottom],
            [b.left, b.bottom],
            [b.left, b.top]
          ],
          centre: [(b.left + b.right) / 2, (b.top + b.bottom) / 2]
        };
      },
      renderSubLayers: (props) => {
        const data = props.data as TileData | null;
        if (!data) return null;
        return [
          new PathLayer({
            id: `${props.id}-box`,
            data: [data.box],
            getPath: (d: number[][]) => d,
            getColor: [90, 130, 180],
            getWidth: 1,
            widthUnits: 'pixels'
          }),
          new TextLayer({
            id: `${props.id}-label`,
            data: [data],
            getPosition: (d: TileData) => d.centre,
            getText: (d: TileData) => d.label,
            getSize: 14,
            getColor: [230, 230, 230]
          })
        ];
      }
    })
  ]
});
```

`clients/ts/spike/index.html`:

```html
<!doctype html>
<html>
  <head><meta charset="utf-8" /><title>Tessera tile spike</title>
    <style>html,body{margin:0;height:100%;background:#111}</style>
  </head>
  <body><script type="module" src="/src/spike.ts"></script></body>
</html>
```

- [ ] **Step 7: Eyeball it**

Run: `cd clients/ts/spike && npx vite`
Open the printed URL. Confirm by eye: tile labels increase in x rightwards and in y downwards (or upwards — whichever it is, `convention.ts` must already agree); zooming in replaces one tile with four; a fast pan leaves no half-drawn tiles behind.

- [ ] **Step 8: Commit**

```bash
git add .gitignore clients/ts/package.json clients/ts/tsconfig.base.json clients/ts/spike
git commit -m "feat(clients): settle the deck.gl non-geospatial tile convention

The half-day spike client-interaction §8.2 specified, with no Tessera in it.
Headless assertions against deck.gl's own Tileset2D pin tile z to Morton depth,
the bbox arithmetic, the y direction and the four-child partition; the Vite page
is there to eyeball refinement and abort."
```

---

### Task 2: The dev-only CORS key

**Files:**
- Create: `crates/tessera-server/src/cors.rs`, `crates/tessera-server/tests/cors.rs`
- Modify: `crates/tessera-server/Cargo.toml`, `crates/tessera-server/src/config.rs`, `crates/tessera-server/src/state.rs`, `crates/tessera-server/src/lib.rs`, `crates/tessera-server/src/viewer.rs`, `crates/tessera-server/src/session.rs`, `crates/tessera-server/tests/common/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config.dev_cors_origins: Vec<String>` (parsed from `[serve] dev_cors_origins`), `AppState.dev_cors_origins: Vec<String>`, and `cors::dev_layer(origins: &[String]) -> Option<tower_http::cors::CorsLayer>`.

- [ ] **Step 1: Write the failing tests**

`crates/tessera-server/tests/cors.rs`:

```rust
//! `serve.dev_cors_origins` — the dev-only browser seam (MVP client spec §3).
//!
//! Two assertions and no more: absent means no CORS headers at all, and a configured origin
//! round-trips including the exposed headers the viewer's stats panel reads.

mod common;

use common::{authorise, mount_server, generous_test_gate, tiny_bundle_engine, SESSION_CREDENTIAL};

#[tokio::test]
async fn absent_dev_cors_origins_means_no_cors_headers() {
    let (engine, _tmp) = tiny_bundle_engine().await;
    let server = mount_server(engine, 200, generous_test_gate()).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "http://localhost:5173")
        .bearer_auth(authorise(&server, &["0"]).await["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn a_configured_origin_round_trips_with_the_exposed_headers() {
    let (engine, _tmp) = tiny_bundle_engine().await;
    let mut server = mount_server(engine, 200, generous_test_gate()).await;
    server
        .restart_with_cors(vec!["http://localhost:5173".to_string()])
        .await;

    let token = authorise(&server, &["0"]).await["token"].as_str().unwrap().to_string();
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "http://localhost:5173")
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .unwrap()
            .to_str()
            .unwrap(),
        "http://localhost:5173"
    );
    let exposed = resp
        .headers()
        .get("access-control-expose-headers")
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(exposed.contains("x-tessera-server-us"));
    assert!(exposed.contains("x-tessera-stage-ns"));
    assert!(exposed.contains("x-tessera-pin"));
}

#[tokio::test]
async fn an_unconfigured_origin_is_refused_even_when_cors_is_on() {
    let (engine, _tmp) = tiny_bundle_engine().await;
    let mut server = mount_server(engine, 200, generous_test_gate()).await;
    server
        .restart_with_cors(vec!["http://localhost:5173".to_string()])
        .await;

    let token = authorise(&server, &["0"]).await["token"].as_str().unwrap().to_string();
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "http://evil.example")
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();

    assert!(resp.headers().get("access-control-allow-origin").is_none());
}
```

Read `crates/tessera-server/tests/common/mod.rs` first and use whatever its bundle-building helper is actually called — the name `tiny_bundle_engine` above is a placeholder for the existing helper that `tests/http.rs` uses to build an engine over a small fixture. Match the existing call shape exactly.

Add to `tests/common/mod.rs`:

```rust
impl TestServer {
    /// Rebind the viewer and session listeners with a CORS layer configured. Used only by
    /// `tests/cors.rs`: the layer is applied inside `router()`, so changing it means new routers.
    pub async fn restart_with_cors(&mut self, origins: Vec<String>) {
        // AppState is behind an Arc shared with the old routers; rebuild it with the new field.
        let state = Arc::new(AppState {
            engine: self.state.engine.clone(),
            sessions: Mutex::new(SessionRegistry::default()),
            max_k: self.state.max_k,
            compute_gate: self.state.compute_gate.clone(),
            stage_timing: self.state.stage_timing,
            min_visible_members: self.state.min_visible_members,
            session_credential: self.state.session_credential.clone(),
            operator_credential: self.state.operator_credential.clone(),
            dev_cors_origins: origins,
        });
        let viewer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.viewer_addr = viewer_listener.local_addr().unwrap();
        let session_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.session_addr = session_listener.local_addr().unwrap();
        let viewer_router = tessera_server::viewer::router(Arc::clone(&state));
        let session_router = tessera_server::session::router(Arc::clone(&state));
        tokio::spawn(async move { axum::serve(viewer_listener, viewer_router).await });
        tokio::spawn(async move { axum::serve(session_listener, session_router).await });
        self.state = state;
    }
}
```

If `Engine` or `ComputeGate` is not `Clone`, do not force it: instead give `mount_server` an extra parameter `dev_cors_origins: Vec<String>`, update its existing call sites to pass `Vec::new()`, and have `tests/cors.rs` call `mount_server` directly with the origins. Prefer that shape if either clone is unavailable — it is simpler and touches less.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tessera-server --test cors`
Expected: FAIL to compile — `no field dev_cors_origins on AppState`, `no method restart_with_cors`.

- [ ] **Step 3: Add the dependency**

In `crates/tessera-server/Cargo.toml`, under `[dependencies]`:

```toml
tower-http = { version = "0.6", features = ["cors"] }
```

- [ ] **Step 4: Write the layer**

`crates/tessera-server/src/cors.rs`:

```rust
//! `serve.dev_cors_origins` — a browser seam for the MVP viewer, and nothing else.
//!
//! **This is a development affordance and it is off unless typed.** The viewer plane's bearer is a
//! session token; the session plane's is the operator-configured session credential. Enabling this
//! lets a page served from another origin present either, which is why there is no default, no
//! environment variable, no wildcard, and a startup warning when it is on.
//!
//! The documented integration topology remains T2 with verified assertions (client-interaction
//! §7): credential construction at the integrator's app server, where the authority is. Nothing
//! here revises that, and nothing here should be read as recommending browser-direct
//! authorisation outside a laptop.
//!
//! Scope: the viewer and session planes only. The control plane is never wrapped — it carries the
//! operator credential and no browser has business reaching it.

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Response headers a browser client is allowed to read. Without these, `fetch` hides them and the
/// viewer's stats panel silently shows nothing — the failure looks like a server that emits no
/// timings rather than a CORS configuration that hides them.
const EXPOSED: [&str; 4] = [
    "x-tessera-pin",
    "x-tessera-server-us",
    "x-tessera-admission-us",
    "x-tessera-stage-ns",
];

/// `None` when no origin is configured, which is the default and means no layer is mounted at all.
///
/// An origin that is not a valid header value is dropped rather than panicking the process; if
/// that leaves the list empty, the result is `None` and the caller's startup warning does not
/// fire, so an operator who typed something unparseable sees no CORS and no claim of CORS.
pub fn dev_layer(origins: &[String]) -> Option<CorsLayer> {
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if parsed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(parsed))
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
            ])
            .expose_headers(EXPOSED.map(HeaderName::from_static)),
    )
}
```

- [ ] **Step 5: Wire it in**

In `crates/tessera-server/src/lib.rs`, add `mod cors;` beside the other module declarations (`pub mod cors;` if the tests need it directly — they do not, so `mod cors;` suffices).

In `crates/tessera-server/src/state.rs`, add to `AppState`:

```rust
    /// `serve.dev_cors_origins`. Empty is the default and means no CORS layer is mounted. See
    /// `crate::cors` for why this is a development affordance rather than an integration feature.
    pub dev_cors_origins: Vec<String>,
```

In `crates/tessera-server/src/viewer.rs` and `crates/tessera-server/src/session.rs`, change `router` to:

```rust
pub fn router(state: Arc<AppState>) -> Router {
    let layer = crate::cors::dev_layer(&state.dev_cors_origins);
    let router = Router::new()
        // ... the existing .route(...) calls, unchanged ...
        .with_state(state);
    match layer {
        Some(cors) => router.layer(cors),
        None => router,
    }
}
```

In `crates/tessera-server/src/config.rs`, add to the `[serve]` deserialised struct and to `Config`:

```rust
    /// Dev-only browser seam (MVP client spec §3). **No default beyond empty**, no environment
    /// variable, no wildcard: absent means no CORS layer is mounted at all. See `crate::cors`.
    #[serde(default)]
    pub dev_cors_origins: Vec<String>,
```

Populate it wherever `Config` is constructed from the parsed TOML, and in `lib.rs`'s `prepare` (or wherever the startup log lines live) emit, when the list is non-empty:

```rust
    if !config.dev_cors_origins.is_empty() {
        tracing::warn!(
            origins = ?config.dev_cors_origins,
            "serve.dev_cors_origins is set: browser origins may present session tokens and the \
             session credential to this process. This is a DEVELOPMENT affordance — do not enable \
             it in a deployment."
        );
    }
```

Pass `dev_cors_origins: config.dev_cors_origins.clone()` where `AppState` is built in `run`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p tessera-server --test cors`
Expected: PASS — all three.

- [ ] **Step 7: Check nothing else broke**

Run: `cargo test -p tessera-server && ./scripts/check-layers.sh`
Expected: PASS. `check-layers.sh` has no rule about `tower-http`; if it fails, read its output and report rather than editing the script.

- [ ] **Step 8: Add the config default test**

Add to `crates/tessera-server/src/config.rs`'s `mod tests`, following the shape of the existing knob tests in that module:

```rust
    #[test]
    fn dev_cors_origins_defaults_to_empty() {
        let config = parse_minimal_config();
        assert!(
            config.dev_cors_origins.is_empty(),
            "absent serve.dev_cors_origins must mean no CORS at all — it is a dev affordance and \
             a default would let it ride into a deployment"
        );
    }
```

Use whatever helper that module already has for a minimal valid config; `parse_minimal_config` is a placeholder for it.

Run: `cargo test -p tessera-server config::tests`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/tessera-server
git commit -m "feat(server): serve.dev_cors_origins, a dev-only browser seam

One key, off unless typed, no wildcard, viewer and session planes only, with a
loud startup warning. It exists so the MVP viewer can run in a browser against a
local bundle; T2 with verified assertions remains the documented integration
topology and this does not revise it."
```

---

### Task 3: `@tessera/client` — the stateless core

**Files:**
- Create: `clients/ts/core/package.json`, `clients/ts/core/vitest.config.ts`, `clients/ts/core/tsconfig.json`
- Create: `clients/ts/core/src/{index,types,coords,frame,decode,client}.ts`
- Create: `clients/ts/core/test/{frame,decode,coords}.test.ts`
- Create: `clients/ts/scripts/capture-golden.mjs`
- Create: `clients/ts/core/test/fixtures/{viewport-plain.bin,viewport-underlay.bin,meta.json}`

**Interfaces:**
- Consumes: Task 1's `convention.ts` constants, moved into `coords.ts`.
- Produces:
  - `class TesseraClient` with `constructor(opts: {viewerUrl: string; sessionUrl: string; sessionCredential?: string})`, `authorise(terms: string[]): Promise<Session>`, `meta(token: string): Promise<Meta>`, `viewport(token: string, req: ViewportRequest): Promise<ViewportResponse>`, `item(token: string, tesseraId: bigint): Promise<ItemDetail>`.
  - `type Session = {token: string; tokenId: number; expiresAt: number}`
  - `type Meta = {apiVersion: number; identityEpoch: number; slices: {id: string; displayName: string}[]; quantisation: {xMin: number; xMax: number; yMin: number; yMax: number}; declaredScalars: {name: string; arrowType: string}[]; selection: {kMin: number; kMaxMarks: number; maxK: number; thetaTargetMarks: number; maxUnderlayOffset: number}}`
  - `type ViewportRequest = {slice: string; zoom: number; bbox: [number, number, number, number]; k?: number; underlayOffset?: number}`
  - `type TileCounts = {tile: bigint; visible: bigint; matched: bigint; served: bigint}`
  - `type ViewportResult = {tiles: TileCounts[]; ids: BigUint64Array; positions: Float32Array; scalars: Record<string, unknown[]>; subCells: {cell: bigint; count: bigint}[] | null}`
  - `type ViewportResponse = {result: ViewportResult; timings: {serverUs: number; admissionUs: number; stageNs: number[] | null}; pin: string | null; bytes: number}`
  - `type ItemDetail = {scalars: unknown[]; externalId: string | null}`
  - `class TesseraError extends Error` with `status: number`, `code: string`, `detail: string`
  - `coords.ts`: everything Task 1 produced, plus `tileToDataBbox(index: TileIndex, q: Meta['quantisation']): [number, number, number, number]` and `dataToWorldXY(x, y, q): [number, number]`.

- [ ] **Step 1: Scaffold the package and capture golden fixtures**

`clients/ts/core/package.json`:

```json
{
  "name": "@tessera/client",
  "private": true,
  "type": "module",
  "main": "./src/index.ts",
  "scripts": {"test": "vitest run", "build": "tsc -p tsconfig.json"},
  "dependencies": {"apache-arrow": "^21.0.0"},
  "devDependencies": {"typescript": "^5.9.0", "vitest": "^3.2.0"}
}
```

`clients/ts/core/vitest.config.ts`:

```ts
import {defineConfig} from 'vitest/config';
export default defineConfig({test: {environment: 'node', include: ['test/**/*.test.ts']}});
```

`clients/ts/core/tsconfig.json`:

```json
{"extends": "../tsconfig.base.json", "include": ["src"], "compilerOptions": {"outDir": "dist", "rootDir": "src"}}
```

`clients/ts/scripts/capture-golden.mjs`:

```js
#!/usr/bin/env node
// Capture golden payloads from a live `tessera serve` for core's decoder tests.
//
// Usage:
//   TESSERA_SESSION_CRED=… node clients/ts/scripts/capture-golden.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 --terms 0
//
// Writes core/test/fixtures/{meta.json,viewport-plain.bin,viewport-underlay.bin}. Re-run it
// whenever the wire format changes; a decoder test passing against a stale golden is worse than
// no test.
import {writeFile, mkdir} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const viewer = args.viewer ?? 'http://127.0.0.1:37585';
const session = args.session ?? 'http://127.0.0.1:49303';
const terms = (args.terms ?? '0').split(',');
const cred = process.env.TESSERA_SESSION_CRED;
if (!cred) throw new Error('set TESSERA_SESSION_CRED to the session credential');

const authorise = await fetch(`${session}/session/authorise`, {
  method: 'POST',
  headers: {authorization: `Bearer ${cred}`, 'content-type': 'application/json'},
  body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
});
if (!authorise.ok) throw new Error(`authorise: ${authorise.status} ${await authorise.text()}`);
const {token} = await authorise.json();

const metaResp = await fetch(`${viewer}/v1/meta`, {headers: {authorization: `Bearer ${token}`}});
const meta = await metaResp.json();

async function viewport(body) {
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify(body)
  });
  if (!r.ok) throw new Error(`viewport: ${r.status} ${await r.text()}`);
  return Buffer.from(await r.arrayBuffer());
}

const q = meta.quantisation;
const full = [q.x_min, q.y_min, q.x_max, q.y_max];
const base = {slice: meta.slices[0].id, zoom: 2, bbox: full, k: 50};

const dir = join(dirname(fileURLToPath(import.meta.url)), '..', 'core', 'test', 'fixtures');
await mkdir(dir, {recursive: true});
await writeFile(join(dir, 'meta.json'), JSON.stringify(meta, null, 2));
await writeFile(join(dir, 'viewport-plain.bin'), await viewport(base));
await writeFile(join(dir, 'viewport-underlay.bin'), await viewport({...base, underlay_offset: 2}));
console.log('captured to', dir);
```

To run it, start a server first. Write `clients/ts/dev-server.toml` (untracked — add `clients/ts/dev-server.toml` and `clients/ts/.dev/` to `.gitignore`):

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

Run:

```bash
cargo build --release
mkdir -p clients/ts/.dev
export TESSERA_SESSION_CRED=dev-session-credential
export TESSERA_OPERATOR_CRED=dev-operator-credential
./target/release/tessera serve -c clients/ts/dev-server.toml &
cd clients/ts && npm install && node scripts/capture-golden.mjs --terms 0
```

Expected: three files in `clients/ts/core/test/fixtures/`, `viewport-underlay.bin` strictly larger than `viewport-plain.bin`.

- [ ] **Step 2: Write the failing frame test**

`clients/ts/core/test/frame.test.ts`:

```ts
import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {splitFramedStreams} from '../src/frame.js';

const fixture = (name: string) =>
  new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));

describe('splitFramedStreams', () => {
  it('finds three parts, the last absent, in a payload with no underlay', () => {
    const parts = splitFramedStreams(fixture('viewport-plain.bin'));
    expect(parts.tiles.byteLength).toBeGreaterThan(0);
    expect(parts.points.byteLength).toBeGreaterThan(0);
    expect(parts.subCells).toBeNull();
  });

  it('finds a sub-cell stream when the underlay was requested', () => {
    const parts = splitFramedStreams(fixture('viewport-underlay.bin'));
    expect(parts.subCells).not.toBeNull();
    expect(parts.subCells!.byteLength).toBeGreaterThan(0);
  });

  it('consumes the whole payload exactly', () => {
    const raw = fixture('viewport-underlay.bin');
    const parts = splitFramedStreams(raw);
    const consumed =
      4 + parts.tiles.byteLength + parts.points.byteLength + (parts.subCells?.byteLength ?? 0);
    expect(consumed).toBe(raw.byteLength);
  });

  it('leaves the plain payload byte-identical to a pre-underlay one', () => {
    // The server emits ZERO trailing bytes when no underlay was asked for — not an empty stream.
    const raw = fixture('viewport-plain.bin');
    const parts = splitFramedStreams(raw);
    expect(4 + parts.tiles.byteLength + parts.points.byteLength).toBe(raw.byteLength);
  });
});
```

- [ ] **Step 3: Run and watch it fail**

Run: `cd clients/ts/core && npx vitest run test/frame.test.ts`
Expected: FAIL — `Cannot find module '../src/frame.js'`.

- [ ] **Step 4: Write the splitter**

`clients/ts/core/src/frame.ts`:

```ts
import {Message} from 'apache-arrow';

/**
 * Split a `/v1/viewport` response into its concatenated Arrow IPC streams.
 *
 * The wire frame (see `tessera-wire`'s `payload` module doc) is:
 *
 *     u32 LE  byte length of the tile stream
 *     <tile stream>       Arrow IPC stream: tile, visible, matched, served (all uint64)
 *     <points stream>     Arrow IPC stream: tessera_id uint64, x float32, y float32, ...scalars
 *     <sub-cell stream>   Arrow IPC stream: cell uint64, count uint64 — ABSENT ENTIRELY
 *                         (zero bytes) unless the underlay was requested
 *
 * Only the tile boundary carries a length prefix; the server documents that relaxation
 * deliberately, so the points stream's end must be found by walking its IPC messages. That walk is
 * what `streamLength` does, and it is why this file parses framing rather than calling
 * `tableFromIPC` and hoping.
 */
export type FramedStreams = {
  tiles: Uint8Array;
  points: Uint8Array;
  subCells: Uint8Array | null;
};

const CONTINUATION = 0xffffffff;

/** Round `n` up to the next multiple of 8 — Arrow pads both metadata and bodies to 8 bytes. */
function align8(n: number): number {
  return (n + 7) & ~7;
}

/**
 * Byte length of the single Arrow IPC stream beginning at `offset`, including its end-of-stream
 * marker. Walks encapsulated messages: continuation (u32), metadata length (u32), metadata
 * flatbuffer, then a body whose length only the flatbuffer knows.
 */
export function streamLength(buf: Uint8Array, offset: number): number {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let at = offset;
  for (;;) {
    if (at + 8 > buf.byteLength) {
      throw new Error(`truncated Arrow stream: no end-of-stream marker before byte ${at}`);
    }
    const continuation = view.getUint32(at, true);
    if (continuation !== CONTINUATION) {
      throw new Error(`bad Arrow continuation 0x${continuation.toString(16)} at byte ${at}`);
    }
    const metadataLength = view.getUint32(at + 4, true);
    if (metadataLength === 0) return at + 8 - offset; // end-of-stream marker
    const metadata = buf.subarray(at + 8, at + 8 + metadataLength);
    const bodyLength = Message.decode(metadata).bodyLength;
    at += align8(8 + metadataLength) + align8(bodyLength);
  }
}

export function splitFramedStreams(buf: Uint8Array): FramedStreams {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const tileLength = view.getUint32(0, true);
  const tiles = buf.subarray(4, 4 + tileLength);

  const pointsStart = 4 + tileLength;
  const pointsLength = streamLength(buf, pointsStart);
  const points = buf.subarray(pointsStart, pointsStart + pointsLength);

  const subStart = pointsStart + pointsLength;
  const subCells = subStart >= buf.byteLength ? null : buf.subarray(subStart);
  return {tiles, points, subCells};
}
```

- [ ] **Step 5: Run the frame tests**

Run: `cd clients/ts/core && npx vitest run test/frame.test.ts`
Expected: PASS — all four.

If `Message.decode` reports a `bodyLength` that leaves the walk misaligned, the padding assumption is wrong. Log `at`, `metadataLength` and `bodyLength` per message and compare against the raw bytes before changing `align8` — the two alignment calls are the only degrees of freedom, and one of them is unnecessary in Arrow's current writer.

- [ ] **Step 6: Write the failing decode test**

`clients/ts/core/test/decode.test.ts`:

```ts
import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, expect, it} from 'vitest';
import {decodeViewport} from '../src/decode.js';

const fixture = (name: string) =>
  new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));
const meta = JSON.parse(
  readFileSync(join(import.meta.dirname, 'fixtures', 'meta.json'), 'utf8')
);

describe('decodeViewport', () => {
  it('returns one counts row per non-empty tile, with served <= visible', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    expect(r.tiles.length).toBeGreaterThan(0);
    for (const t of r.tiles) {
      expect(t.served).toBeLessThanOrEqual(t.visible);
      expect(t.matched).toBeLessThanOrEqual(t.visible);
    }
  });

  it('returns exactly sum(served) points, interleaved as x,y pairs', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const served = r.tiles.reduce((acc, t) => acc + Number(t.served), 0);
    expect(r.ids.length).toBe(served);
    expect(r.positions.length).toBe(served * 2);
  });

  it('places every point inside the requested extent', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    const q = meta.quantisation;
    for (let i = 0; i < r.ids.length; i++) {
      expect(r.positions[i * 2]).toBeGreaterThanOrEqual(q.x_min);
      expect(r.positions[i * 2]).toBeLessThanOrEqual(q.x_max);
      expect(r.positions[i * 2 + 1]).toBeGreaterThanOrEqual(q.y_min);
      expect(r.positions[i * 2 + 1]).toBeLessThanOrEqual(q.y_max);
    }
  });

  it('returns null sub-cells without the underlay and rows with it', () => {
    expect(decodeViewport(fixture('viewport-plain.bin')).subCells).toBeNull();
    const withUnderlay = decodeViewport(fixture('viewport-underlay.bin'));
    expect(withUnderlay.subCells!.length).toBeGreaterThan(0);
    for (const c of withUnderlay.subCells!) expect(c.count).toBeGreaterThan(0n);
  });

  it('never loses precision on a tessera_id', () => {
    const r = decodeViewport(fixture('viewport-plain.bin'));
    // BigUint64Array, not number[]: a u64 identity does not survive a double.
    expect(r.ids).toBeInstanceOf(BigUint64Array);
  });
});
```

- [ ] **Step 7: Run and watch it fail**

Run: `cd clients/ts/core && npx vitest run test/decode.test.ts`
Expected: FAIL — `Cannot find module '../src/decode.js'`.

- [ ] **Step 8: Write the types and the decoder**

`clients/ts/core/src/types.ts`:

```ts
export type Session = {token: string; tokenId: number; expiresAt: number};

export type Quantisation = {xMin: number; xMax: number; yMin: number; yMax: number};

export type Meta = {
  apiVersion: number;
  identityEpoch: number;
  slices: {id: string; displayName: string}[];
  quantisation: Quantisation;
  declaredScalars: {name: string; arrowType: string}[];
  selection: {
    kMin: number;
    kMaxMarks: number;
    maxK: number;
    thetaTargetMarks: number;
    maxUnderlayOffset: number;
  };
};

export type ViewportRequest = {
  slice: string;
  zoom: number;
  bbox: [number, number, number, number];
  k?: number;
  underlayOffset?: number;
};

/** One tile's exact masked counts. These come from the server and are never recomputed. */
export type TileCounts = {tile: bigint; visible: bigint; matched: bigint; served: bigint};

export type SubCell = {cell: bigint; count: bigint};

export type ViewportResult = {
  tiles: TileCounts[];
  /** Wire identity, u64 — never narrowed to a number. */
  ids: BigUint64Array;
  /** Interleaved x,y in DATA space, ready for deck.gl once transformed to world space. */
  positions: Float32Array;
  scalars: Record<string, unknown[]>;
  subCells: SubCell[] | null;
};

export type Timings = {serverUs: number; admissionUs: number; stageNs: number[] | null};

export type ViewportResponse = {
  result: ViewportResult;
  timings: Timings;
  pin: string | null;
  bytes: number;
};

export type ItemDetail = {scalars: unknown[]; externalId: string | null};
```

`clients/ts/core/src/decode.ts`:

```ts
import {tableFromIPC} from 'apache-arrow';
import {splitFramedStreams} from './frame.js';
import type {SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: ReturnType<typeof tableFromIPC>, name: string): BigInt64Array | BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
}

/**
 * Decode a framed `/v1/viewport` body.
 *
 * Two rules this function exists to hold:
 *
 * 1. `ids` stays a `BigUint64Array`. A `tessera_id` is a u64 and does not survive a double.
 * 2. Positions are interleaved here, once, into the layout deck.gl's `getPosition` wants — the
 *    server ships separate `x`/`y` columns (client-interaction §8.2 records the cost). Coordinates
 *    are left in DATA space; `coords.ts` converts to world space, because that conversion needs
 *    the bundle's quantisation extent and this function does not have it.
 */
export function decodeViewport(body: Uint8Array): ViewportResult {
  const parts = splitFramedStreams(body);

  const tileTable = tableFromIPC(parts.tiles);
  const tile = u64Column(tileTable, 'tile');
  const visible = u64Column(tileTable, 'visible');
  const matched = u64Column(tileTable, 'matched');
  const served = u64Column(tileTable, 'served');
  const tiles: TileCounts[] = [];
  for (let i = 0; i < tile.length; i++) {
    tiles.push({
      tile: BigInt(tile[i]!),
      visible: BigInt(visible[i]!),
      matched: BigInt(matched[i]!),
      served: BigInt(served[i]!)
    });
  }

  const pointTable = tableFromIPC(parts.points);
  const ids = pointTable.getChild('tessera_id')!.toArray() as BigUint64Array;
  const xs = pointTable.getChild('x')!.toArray() as Float32Array;
  const ys = pointTable.getChild('y')!.toArray() as Float32Array;
  const positions = new Float32Array(ids.length * 2);
  for (let i = 0; i < ids.length; i++) {
    positions[i * 2] = xs[i]!;
    positions[i * 2 + 1] = ys[i]!;
  }

  const scalars: Record<string, unknown[]> = {};
  for (const field of pointTable.schema.fields) {
    if (field.name === 'tessera_id' || field.name === 'x' || field.name === 'y') continue;
    scalars[field.name] = [...pointTable.getChild(field.name)!];
  }

  let subCells: SubCell[] | null = null;
  if (parts.subCells) {
    const t = tableFromIPC(parts.subCells);
    const cell = u64Column(t, 'cell');
    const count = u64Column(t, 'count');
    subCells = [];
    for (let i = 0; i < cell.length; i++) {
      subCells.push({cell: BigInt(cell[i]!), count: BigInt(count[i]!)});
    }
  }

  return {tiles, ids, positions, scalars, subCells};
}
```

- [ ] **Step 9: Run the decode tests**

Run: `cd clients/ts/core && npx vitest run test/decode.test.ts`
Expected: PASS — all five.

- [ ] **Step 10: Write the coordinate test**

`clients/ts/core/test/coords.test.ts`:

```ts
import {describe, expect, it} from 'vitest';
import {
  CELL_GRID,
  WORLD_SIZE,
  dataToWorldXY,
  tileToCellBox,
  tileToDataBbox
} from '../src/coords.js';

const q = {xMin: 0, xMax: 65536, yMin: 0, yMax: 65536};
const skewed = {xMin: -10, xMax: 10, yMin: 0, yMax: 1000};

describe('coords', () => {
  it('maps the z=0 tile onto the whole data extent', () => {
    expect(tileToDataBbox({x: 0, y: 0, z: 0}, skewed)).toEqual([-10, 0, 10, 1000]);
  });

  it('partitions the extent across a depth’s tiles with no gap or overlap', () => {
    const z = 3;
    const n = 2 ** z;
    for (let x = 0; x < n - 1; x++) {
      const left = tileToDataBbox({x, y: 0, z}, skewed);
      const right = tileToDataBbox({x: x + 1, y: 0, z}, skewed);
      expect(left[2]).toBeCloseTo(right[0], 9);
    }
  });

  it('handles a non-square extent per axis', () => {
    const box = tileToDataBbox({x: 0, y: 0, z: 1}, skewed);
    expect(box[2] - box[0]).toBeCloseTo(10, 9);   // half of 20
    expect(box[3] - box[1]).toBeCloseTo(500, 9);  // half of 1000
  });

  it('sends data-space corners to world-space corners', () => {
    expect(dataToWorldXY(q.xMin, q.yMin, q)).toEqual([0, 0]);
    expect(dataToWorldXY(q.xMax, q.yMax, q)).toEqual([WORLD_SIZE, WORLD_SIZE]);
  });

  it('keeps the cell box and the data bbox describing the same block', () => {
    const cells = tileToCellBox({x: 5, y: 2, z: 4});
    const data = tileToDataBbox({x: 5, y: 2, z: 4}, q);
    expect(data[0]).toBeCloseTo((cells.cx0 / CELL_GRID) * 65536, 6);
    expect(data[1]).toBeCloseTo((cells.cy0 / CELL_GRID) * 65536, 6);
  });
});
```

- [ ] **Step 11: Run and watch it fail, then write coords**

Run: `cd clients/ts/core && npx vitest run test/coords.test.ts`
Expected: FAIL — no `../src/coords.js`.

`clients/ts/core/src/coords.ts` — copy `clients/ts/spike/src/convention.ts` verbatim (constants, `TileIndex`, `CellBox`, `tileToCellBox`, including its doc comment and any `DEPTH_OFFSET` the spike introduced), then append:

```ts
import type {Quantisation} from './types.js';

/** The data-space bbox `[x0, y0, x1, y1]` a tile covers, for `POST /v1/viewport`. */
export function tileToDataBbox(index: TileIndex, q: Quantisation): [number, number, number, number] {
  const cells = tileToCellBox(index);
  const spanX = q.xMax - q.xMin;
  const spanY = q.yMax - q.yMin;
  return [
    q.xMin + (cells.cx0 / CELL_GRID) * spanX,
    q.yMin + (cells.cy0 / CELL_GRID) * spanY,
    q.xMin + (cells.cx1 / CELL_GRID) * spanX,
    q.yMin + (cells.cy1 / CELL_GRID) * spanY
  ];
}

/**
 * Data space to deck.gl world space. Each axis is scaled independently, exactly as §2.5 quantises
 * them — which is what makes a tile square in world space despite a rectangular data extent.
 */
export function dataToWorldXY(x: number, y: number, q: Quantisation): [number, number] {
  return [
    ((x - q.xMin) / (q.xMax - q.xMin)) * WORLD_SIZE,
    ((y - q.yMin) / (q.yMax - q.yMin)) * WORLD_SIZE
  ];
}

/** In-place data→world conversion of an interleaved x,y buffer. One pass, no allocation. */
export function positionsToWorld(positions: Float32Array, q: Quantisation): Float32Array {
  const sx = WORLD_SIZE / (q.xMax - q.xMin);
  const sy = WORLD_SIZE / (q.yMax - q.yMin);
  for (let i = 0; i < positions.length; i += 2) {
    positions[i] = (positions[i]! - q.xMin) * sx;
    positions[i + 1] = (positions[i + 1]! - q.yMin) * sy;
  }
  return positions;
}
```

Run: `cd clients/ts/core && npx vitest run test/coords.test.ts`
Expected: PASS — all five.

- [ ] **Step 12: Write the client**

`clients/ts/core/src/client.ts`:

```ts
import {decodeViewport} from './decode.js';
import type {ItemDetail, Meta, Session, ViewportRequest, ViewportResponse} from './types.js';

/** A Tessera error body, `{"error": code, "detail": string}`, with its HTTP status. */
export class TesseraError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    readonly detail: string
  ) {
    super(`${status} ${code}: ${detail}`);
    this.name = 'TesseraError';
  }
}

async function fail(response: Response): Promise<never> {
  let code = 'unknown';
  let detail = response.statusText;
  try {
    const body = (await response.json()) as {error?: string; detail?: string};
    code = body.error ?? code;
    detail = body.detail ?? detail;
  } catch {
    // A non-JSON body (a proxy's, say) still deserves a typed error rather than a parse crash.
  }
  throw new TesseraError(response.status, code, detail);
}

export type TesseraClientOptions = {
  viewerUrl: string;
  sessionUrl: string;
  /** Only needed to call `authorise`. See `crate::cors` for why this is a dev-only shape. */
  sessionCredential?: string;
};

/**
 * The four viewer/session verbs, and nothing else. No cache, no epoch, no session lifetime, no
 * replica state — client-interaction §10's session-client layer, which is what a REST user would
 * have written anyway. The replica store goes above this, not inside it.
 */
export class TesseraClient {
  constructor(private readonly opts: TesseraClientOptions) {}

  async authorise(terms: string[]): Promise<Session> {
    if (!this.opts.sessionCredential) {
      throw new Error('authorise needs a sessionCredential');
    }
    const authData = btoa(JSON.stringify({terms}));
    const response = await fetch(`${this.opts.sessionUrl}/session/authorise`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${this.opts.sessionCredential}`,
        'content-type': 'application/json'
      },
      body: JSON.stringify({auth_data: authData})
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {token: string; token_id: number; expires_at: number};
    return {token: body.token, tokenId: body.token_id, expiresAt: body.expires_at};
  }

  async meta(token: string): Promise<Meta> {
    const response = await fetch(`${this.opts.viewerUrl}/v1/meta`, {
      headers: {authorization: `Bearer ${token}`}
    });
    if (!response.ok) await fail(response);
    const m = (await response.json()) as any;
    return {
      apiVersion: m.api_version,
      identityEpoch: m.identity_epoch,
      slices: m.slices.map((s: any) => ({id: s.id, displayName: s.display_name})),
      quantisation: {
        xMin: m.quantisation.x_min,
        xMax: m.quantisation.x_max,
        yMin: m.quantisation.y_min,
        yMax: m.quantisation.y_max
      },
      declaredScalars: m.declared_scalars.map((s: any) => ({
        name: s.name,
        arrowType: s.arrow_type
      })),
      selection: {
        kMin: m.selection.k_min,
        kMaxMarks: m.selection.k_max_marks,
        maxK: m.selection.max_k,
        thetaTargetMarks: m.selection.theta_target_marks,
        maxUnderlayOffset: m.selection.max_underlay_offset
      }
    };
  }

  /**
   * `k` is omitted from the body unless the caller sets it, so the deployment's own ceiling is the
   * default — contracts §3.2's rule, and the reason a caller who never mentions `k` can never
   * decrease it.
   */
  async viewport(
    token: string,
    req: ViewportRequest,
    signal?: AbortSignal
  ): Promise<ViewportResponse> {
    const body: Record<string, unknown> = {slice: req.slice, zoom: req.zoom, bbox: req.bbox};
    if (req.k !== undefined) body.k = req.k;
    if (req.underlayOffset) body.underlay_offset = req.underlayOffset;

    const response = await fetch(`${this.opts.viewerUrl}/v1/viewport`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const bytes = new Uint8Array(await response.arrayBuffer());
    const stage = response.headers.get('x-tessera-stage-ns');
    return {
      result: decodeViewport(bytes),
      timings: {
        serverUs: Number(response.headers.get('x-tessera-server-us') ?? 0),
        admissionUs: Number(response.headers.get('x-tessera-admission-us') ?? 0),
        // Absent in a binary without `bench-timing`, or with `stage_timing = false`. Absent is a
        // configuration fact, not an error, and every reader must tolerate it.
        stageNs: stage ? stage.split(',').map(Number) : null
      },
      pin: response.headers.get('x-tessera-pin'),
      bytes: bytes.byteLength
    };
  }

  async item(token: string, tesseraId: bigint): Promise<ItemDetail> {
    const response = await fetch(`${this.opts.viewerUrl}/v1/items/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: '{}'
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {scalars: unknown[]; external_id?: string};
    return {scalars: body.scalars, externalId: body.external_id ?? null};
  }
}
```

`clients/ts/core/src/index.ts`:

```ts
export * from './types.js';
export * from './coords.js';
export {splitFramedStreams, streamLength} from './frame.js';
export {decodeViewport} from './decode.js';
export {TesseraClient, TesseraError} from './client.js';
export type {TesseraClientOptions} from './client.js';
```

- [ ] **Step 13: Typecheck and run the whole suite**

Run: `cd clients/ts/core && npx tsc -p tsconfig.json --noEmit && npx vitest run`
Expected: no type errors; all tests pass.

- [ ] **Step 14: Commit**

```bash
git add clients/ts/core clients/ts/scripts .gitignore
git commit -m "feat(clients): @tessera/client, the stateless session layer

The four verbs, the framed-Arrow decoder and the coordinate arithmetic — the
three things client-interaction §8.2 measured a stranger getting wrong. Holds no
cache, no epoch and no replica state; the store layer inserts above it later.
Decoder tests run against golden payloads captured from a live server, so a wire
change breaks them rather than passing quietly."
```

---

### Task 4: The viewer's map

Marks only. Panels arrive in Tasks 5–7.

**Files:**
- Create: `clients/ts/viewer/{package.json,index.html,vite.config.ts,tsconfig.json}`
- Create: `clients/ts/viewer/src/{main.ts,config.ts,state.ts,map.ts,style.css}`

**Interfaces:**
- Consumes: everything `@tessera/client` produces.
- Produces: `type AppState` and `createStore()` from `state.ts`; `buildLayers(state: AppState): Layer[]` from `map.ts`; `readConfig(): ViewerConfig` from `config.ts`.

- [ ] **Step 1: Scaffold**

`clients/ts/viewer/package.json`:

```json
{
  "name": "@tessera/viewer",
  "private": true,
  "type": "module",
  "scripts": {"dev": "vite", "build": "vite build", "preview": "vite preview"},
  "dependencies": {
    "@deck.gl/core": "^9.3.7",
    "@deck.gl/geo-layers": "^9.3.7",
    "@deck.gl/layers": "^9.3.7",
    "@tessera/client": "*"
  },
  "devDependencies": {"typescript": "^5.9.0", "vite": "^8.2.0"}
}
```

`clients/ts/viewer/tsconfig.json`:

```json
{"extends": "../tsconfig.base.json", "include": ["src"]}
```

`clients/ts/viewer/vite.config.ts`:

```ts
import {defineConfig} from 'vite';
export default defineConfig({server: {port: 5173, strictPort: true}});
```

`clients/ts/viewer/index.html`:

```html
<!doctype html>
<html lang="en-GB">
  <head>
    <meta charset="utf-8" />
    <title>Tessera viewer</title>
    <link rel="stylesheet" href="/src/style.css" />
  </head>
  <body>
    <div id="map"></div>
    <aside id="panels"></aside>
    <script type="module" src="/src/main.ts"></script>
  </body>
</html>
```

`clients/ts/viewer/src/style.css`:

```css
html, body { margin: 0; height: 100%; background: #0d0f12; color: #dfe3e8;
  font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
#map { position: absolute; inset: 0; }
#panels { position: absolute; top: 0; right: 0; width: 320px; max-height: 100%;
  overflow-y: auto; padding: 12px; background: rgba(13,15,18,.88);
  border-left: 1px solid #222; }
.panel { margin-bottom: 16px; }
.panel h2 { margin: 0 0 6px; font-size: 11px; letter-spacing: .08em;
  text-transform: uppercase; color: #7d8794; font-weight: 600; }
.row { display: flex; justify-content: space-between; gap: 8px; }
.row .v { color: #eaeef3; }
.muted { color: #7d8794; }
.bad { color: #ff8f7a; }
```

`clients/ts/viewer/src/config.ts`:

```ts
export type ViewerConfig = {
  viewerUrl: string;
  sessionUrl: string;
  sessionCredential: string;
};

/**
 * Read from Vite env. `VITE_TESSERA_SESSION_CREDENTIAL` puts the deployment's session credential
 * in the browser bundle, which is why the server key that permits this browser to talk at all
 * (`serve.dev_cors_origins`) is off unless typed. Development only.
 */
export function readConfig(): ViewerConfig {
  const env = import.meta.env;
  return {
    viewerUrl: env.VITE_TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
    sessionUrl: env.VITE_TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
    sessionCredential: env.VITE_TESSERA_SESSION_CREDENTIAL ?? ''
  };
}
```

Create `clients/ts/viewer/.env.local` (add `*.env.local` to `.gitignore`):

```
VITE_TESSERA_VIEWER_URL=http://127.0.0.1:37585
VITE_TESSERA_SESSION_URL=http://127.0.0.1:49303
VITE_TESSERA_SESSION_CREDENTIAL=dev-session-credential
```

- [ ] **Step 2: The store**

`clients/ts/viewer/src/state.ts`:

```ts
import type {Meta, Session, TileCounts, Timings} from '@tessera/client';

/** What one loaded tile contributes. Counts come from the server; nothing here is derived. */
export type LoadedTile = {
  z: number;
  counts: TileCounts[];
  pointCount: number;
};

export type RequestFailure = {tileId: string; code: string; detail: string; at: number};

export type AppState = {
  meta: Meta | null;
  session: Session | null;
  slice: string;
  termsLabel: string;
  terms: string[];
  k: number | undefined;
  underlayOffset: number;
  /** Keyed by deck tile id, replaced wholesale on each viewport load. */
  tiles: Map<string, LoadedTile>;
  /** The tiles deck.gl currently considers visible — the honest basis for the counts panel. */
  visibleTileIds: string[];
  lastTimings: Timings | null;
  lastBytes: number;
  inFlight: number;
  failures: RequestFailure[];
  selected: {id: bigint; scalars: unknown[]; externalId: string | null} | null;
};

export type Store = {
  state: AppState;
  update(fn: (s: AppState) => void): void;
  subscribe(fn: (s: AppState) => void): void;
};

export function createStore(initial: AppState): Store {
  const listeners: ((s: AppState) => void)[] = [];
  const store: Store = {
    state: initial,
    update(fn) {
      fn(store.state);
      for (const l of listeners) l(store.state);
    },
    subscribe(fn) {
      listeners.push(fn);
    }
  };
  return store;
}
```

- [ ] **Step 3: The map**

`clients/ts/viewer/src/map.ts`:

```ts
import {OrthographicView} from '@deck.gl/core';
import type {Layer} from '@deck.gl/core';
import {TileLayer} from '@deck.gl/geo-layers';
import {ScatterplotLayer} from '@deck.gl/layers';
import {
  MAX_DEPTH,
  TILE_SIZE,
  WORLD_SIZE,
  positionsToWorld,
  tileToDataBbox,
  type TesseraClient,
  type ViewportResult
} from '@tessera/client';
import type {Store} from './state.js';

export const VIEW = new OrthographicView({id: 'ortho', flipY: true});

export const INITIAL_VIEW_STATE = {
  target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0] as [number, number, number],
  zoom: 0,
  minZoom: -2,
  maxZoom: MAX_DEPTH
};

export type TilePayload = {result: ViewportResult; worldPositions: Float32Array};

/**
 * One TileLayer whose identity encodes everything that invalidates its cache: k, the term set, the
 * slice and the underlay offset. Changing any of them mints a new layer, which drops the cache
 * wholesale. Crude and correct — this is exactly where the replica store lands later.
 */
export function buildLayers(store: Store, client: TesseraClient): Layer[] {
  const {meta, session, slice, k, underlayOffset, termsLabel} = store.state;
  if (!meta || !session) return [];

  const layerId = `tiles:${slice}:${termsLabel}:${k ?? 'default'}:${underlayOffset}`;

  return [
    new TileLayer<TilePayload>({
      id: layerId,
      tileSize: TILE_SIZE,
      minZoom: 0,
      maxZoom: MAX_DEPTH,
      extent: [0, 0, WORLD_SIZE, WORLD_SIZE],
      // Correct for us rather than merely tolerable: §7.2's nesting makes every child a superset
      // of its parent's marks, so showing a parent while children load is add-only.
      refinementStrategy: 'best-available',
      maxRequests: 6,

      getTileData: async ({index, signal}) => {
        store.update((s) => {
          s.inFlight += 1;
        });
        try {
          const response = await client.viewport(
            session.token,
            {
              slice,
              zoom: index.z,
              bbox: tileToDataBbox(index, meta.quantisation),
              k,
              underlayOffset
            },
            signal ?? undefined
          );
          // deck.gl's cancellation contract is fail-closed: on abort, throw or return falsy so
          // nothing incomplete is cached.
          if (signal?.aborted) throw new Error('aborted');

          store.update((s) => {
            s.lastTimings = response.timings;
            s.lastBytes = response.bytes;
          });
          return {
            result: response.result,
            worldPositions: positionsToWorld(response.result.positions, meta.quantisation)
          };
        } finally {
          store.update((s) => {
            s.inFlight -= 1;
          });
        }
      },

      onViewportLoad: (tiles) => {
        store.update((s) => {
          s.tiles.clear();
          for (const tile of tiles ?? []) {
            const payload = tile.content as TilePayload | null;
            if (!payload) continue;
            s.tiles.set(String(tile.id), {
              z: tile.index.z,
              counts: payload.result.tiles,
              pointCount: payload.result.ids.length
            });
          }
          s.visibleTileIds = [...s.tiles.keys()];
        });
      },

      renderSubLayers: (props) => {
        const payload = props.data as TilePayload | null;
        if (!payload || payload.result.ids.length === 0) return null;
        return new ScatterplotLayer({
          id: `${props.id}-marks`,
          data: {
            length: payload.result.ids.length,
            attributes: {getPosition: {value: payload.worldPositions, size: 2}}
          },
          getFillColor: [120, 190, 255, 200],
          radiusUnits: 'pixels',
          getRadius: 1.6,
          radiusMinPixels: 1,
          pickable: true,
          parameters: {depthCompare: 'always'}
        });
      }
    })
  ];
}
```

- [ ] **Step 4: Wire it up**

`clients/ts/viewer/src/main.ts`:

```ts
import {Deck} from '@deck.gl/core';
import {TesseraClient} from '@tessera/client';
import {readConfig} from './config.js';
import {INITIAL_VIEW_STATE, VIEW, buildLayers} from './map.js';
import {createStore} from './state.js';

const config = readConfig();
const client = new TesseraClient({
  viewerUrl: config.viewerUrl,
  sessionUrl: config.sessionUrl,
  sessionCredential: config.sessionCredential
});

const store = createStore({
  meta: null,
  session: null,
  slice: '',
  termsLabel: 'all',
  terms: ['0'],
  k: undefined,
  underlayOffset: 0,
  tiles: new Map(),
  visibleTileIds: [],
  lastTimings: null,
  lastBytes: 0,
  inFlight: 0,
  failures: [],
  selected: null
});

const deck = new Deck({
  parent: document.getElementById('map')!,
  views: VIEW,
  initialViewState: INITIAL_VIEW_STATE,
  controller: true,
  layers: []
});

store.subscribe(() => {
  deck.setProps({layers: buildLayers(store, client)});
});

async function start() {
  const session = await client.authorise(store.state.terms);
  const meta = await client.meta(session.token);
  store.update((s) => {
    s.session = session;
    s.meta = meta;
    s.slice = meta.slices[0]!.id;
  });
}

start().catch((error) => {
  document.getElementById('panels')!.innerHTML =
    `<div class="panel bad">startup failed: ${String(error)}</div>`;
});
```

- [ ] **Step 5: Run it against the 2m4 fixture**

```bash
export TESSERA_SESSION_CRED=dev-session-credential
export TESSERA_OPERATOR_CRED=dev-operator-credential
./target/release/tessera serve -c clients/ts/dev-server.toml &
cd clients/ts && npm install && npm run dev -w @tessera/viewer
```

Expected: a dark page with points; panning and zooming fetch new tiles; zooming in adds marks without existing ones disappearing (the nesting property, visible). If the marks are mirrored vertically, `OrthographicView`'s `flipY` and Task 1's measured y direction disagree — fix it in one place: `VIEW`'s `flipY`, and re-run Task 1's tests to confirm they still pass.

- [ ] **Step 6: Commit**

```bash
git add clients/ts/viewer clients/ts/package.json .gitignore
git commit -m "feat(viewer): the map — OrthographicView, TileLayer, marks

Cell space as the deck.gl world, so a tile is square despite a per-axis
quantised extent and tile z is a Morton depth. Abort throws so nothing partial
is cached; layer identity carries k, terms, slice and underlay offset, which is
the whole of MVP cache invalidation."
```

---

### Task 5: Counts panel and principal presets

**Files:**
- Create: `clients/ts/scripts/measure-principals.mjs`, `clients/ts/viewer/presets.json`
- Create: `clients/ts/viewer/src/panels/{counts.ts,principal.ts}`
- Modify: `clients/ts/viewer/src/main.ts`

**Interfaces:**
- Consumes: `Store`, `TesseraClient`.
- Produces: `renderCounts(state: AppState): string`, `renderPrincipal(state: AppState, presets: Preset[]): string`, `type Preset = {label: string; terms: string[]; visible: number}`.

- [ ] **Step 1: Write the measuring script**

`clients/ts/scripts/measure-principals.mjs`:

```js
#!/usr/bin/env node
// Measure each candidate term's exact visible-set size, and emit viewer/presets.json.
//
// The size comes from the service itself: a zoom=0, full-extent viewport call returns `visible`
// for the single root tile, which IS that principal's visible-set cardinality. Nothing is
// estimated and nothing is derived from a drawn sample.
//
//   TESSERA_SESSION_CRED=… node clients/ts/scripts/measure-principals.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 --terms 0..170
import {writeFile} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {tableFromIPC} from 'apache-arrow';
import {splitFramedStreams} from '../core/src/frame.js';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const viewer = args.viewer ?? 'http://127.0.0.1:37585';
const session = args.session ?? 'http://127.0.0.1:49303';
const cred = process.env.TESSERA_SESSION_CRED;
if (!cred) throw new Error('set TESSERA_SESSION_CRED');

const spec = args.terms ?? '0..170';
const candidates = spec.includes('..')
  ? (() => {
      const [lo, hi] = spec.split('..').map(Number);
      return Array.from({length: hi - lo + 1}, (_, i) => String(lo + i));
    })()
  : spec.split(',');

async function authorise(terms) {
  const r = await fetch(`${session}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${cred}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
  });
  if (!r.ok) throw new Error(`authorise ${terms}: ${r.status} ${await r.text()}`);
  return (await r.json()).token;
}

const probeToken = await authorise([candidates[0]]);
const meta = await (await fetch(`${viewer}/v1/meta`, {headers: {authorization: `Bearer ${probeToken}`}})).json();
const q = meta.quantisation;
const slice = meta.slices[0].id;

async function visibleFor(terms) {
  const token = await authorise(terms);
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify({slice, zoom: 0, bbox: [q.x_min, q.y_min, q.x_max, q.y_max], k: 1})
  });
  if (!r.ok) throw new Error(`viewport ${terms}: ${r.status} ${await r.text()}`);
  const parts = splitFramedStreams(new Uint8Array(Buffer.from(await r.arrayBuffer())));
  const table = tableFromIPC(parts.tiles);
  const visible = table.getChild('visible').toArray();
  return [...visible].reduce((a, b) => a + Number(b), 0);
}

const measured = [];
for (const term of candidates) {
  try {
    measured.push({term, visible: await visibleFor([term])});
  } catch (e) {
    console.error(`skipping ${term}: ${e.message}`);
  }
}
measured.sort((a, b) => a.visible - b.visible);
const nonEmpty = measured.filter((m) => m.visible > 0);
if (nonEmpty.length === 0) throw new Error('no candidate term is visible to anyone');

const at = (fraction) => nonEmpty[Math.min(nonEmpty.length - 1, Math.floor(fraction * nonEmpty.length))];
const narrow = at(0.05);
const medium = at(0.5);
const broad = nonEmpty[nonEmpty.length - 1];
const everything = {
  label: `everything (${nonEmpty.length} terms)`,
  terms: nonEmpty.map((m) => m.term),
  visible: await visibleFor(nonEmpty.map((m) => m.term))
};

const presets = [
  {label: `narrow — term ${narrow.term}`, terms: [narrow.term], visible: narrow.visible},
  {label: `medium — term ${medium.term}`, terms: [medium.term], visible: medium.visible},
  {label: `broad — term ${broad.term}`, terms: [broad.term], visible: broad.visible},
  everything
];

const out = join(dirname(fileURLToPath(import.meta.url)), '..', 'viewer', 'presets.json');
await writeFile(out, JSON.stringify(presets, null, 2));
console.table(presets.map((p) => ({label: p.label, visible: p.visible})));
console.log('wrote', out);
```

Run (server up):

```bash
cd clients/ts && TESSERA_SESSION_CRED=dev-session-credential node scripts/measure-principals.mjs --terms 0..170
```

Expected: a table with four rows whose `visible` values differ by at least an order of magnitude between narrow and broad; `viewer/presets.json` written. If they do not differ, widen the candidate range or combine terms — a preset list where every principal sees the same thing makes success criterion 3 untestable.

- [ ] **Step 2: The counts panel**

`clients/ts/viewer/src/panels/counts.ts`:

```ts
import type {AppState} from '../state.js';

const fmt = (n: bigint | number) => n.toLocaleString('en-GB');

/**
 * Sum the exact masked counts over the tiles deck.gl currently holds AT THE DEEPEST LOADED DEPTH.
 *
 * The depth filter is load-bearing, not tidiness. `best-available` refinement keeps a parent tile
 * on screen while its children load, and a parent's counts cover the same region as its four
 * children's — summing both double-counts, and the number that would be wrong is precisely the one
 * this panel exists to be right about.
 *
 * Nothing here is derived from the drawn marks. `served` is what was drawn; `visible` is what
 * exists inside the mask. The panel shows both because a sample must never read as a set.
 */
export function renderCounts(state: AppState): string {
  const tiles = [...state.tiles.values()];
  if (tiles.length === 0) {
    return panel('Counts', '<div class="muted">no tiles loaded</div>');
  }
  const depth = Math.max(...tiles.map((t) => t.z));
  const current = tiles.filter((t) => t.z === depth);

  let visible = 0n;
  let matched = 0n;
  let served = 0n;
  let nonEmpty = 0;
  for (const tile of current) {
    for (const counts of tile.counts) {
      visible += counts.visible;
      matched += counts.matched;
      served += counts.served;
      nonEmpty += 1;
    }
  }

  return panel(
    'Counts',
    `${row('served (drawn)', fmt(served))}
     ${row('visible (in mask)', fmt(visible))}
     ${row('matched', fmt(matched))}
     ${row('depth', String(depth))}
     ${row('non-empty tiles', fmt(nonEmpty))}
     <div class="muted">${fmt(served)} of ${fmt(visible)} shown</div>`
  );
}

export function panel(title: string, body: string): string {
  return `<section class="panel"><h2>${title}</h2>${body}</section>`;
}

export function row(label: string, value: string, cls = 'v'): string {
  return `<div class="row"><span class="muted">${label}</span><span class="${cls}">${value}</span></div>`;
}
```

- [ ] **Step 3: The principal panel**

`clients/ts/viewer/src/panels/principal.ts`:

```ts
import type {AppState} from '../state.js';
import {panel, row} from './counts.js';

export type Preset = {label: string; terms: string[]; visible: number};

export function renderPrincipal(state: AppState, presets: Preset[]): string {
  const options = presets
    .map(
      (p, i) =>
        `<option value="${i}"${p.label === state.termsLabel ? ' selected' : ''}>${p.label}</option>`
    )
    .join('');
  const active = presets.find((p) => p.label === state.termsLabel);
  return panel(
    'Principal',
    `<select id="principal">${options}</select>
     ${row('terms', String(state.terms.length))}
     ${row('visible at build', active ? active.visible.toLocaleString('en-GB') : '—')}`
  );
}
```

- [ ] **Step 4: Render the panels**

In `clients/ts/viewer/src/main.ts`, import `presets.json` and the two panels, and add a render pass:

```ts
import presetsJson from '../presets.json';
import {renderCounts} from './panels/counts.js';
import {renderPrincipal, type Preset} from './panels/principal.js';

const presets = presetsJson as Preset[];
const panels = document.getElementById('panels')!;

function render() {
  panels.innerHTML = renderPrincipal(store.state, presets) + renderCounts(store.state);
  const select = document.getElementById('principal') as HTMLSelectElement | null;
  select?.addEventListener('change', async () => {
    const preset = presets[Number(select.value)]!;
    const session = await client.authorise(preset.terms);
    store.update((s) => {
      s.session = session;
      s.terms = preset.terms;
      s.termsLabel = preset.label;
      s.tiles.clear();
    });
  });
}

store.subscribe(render);
```

and set the initial `terms`/`termsLabel` from `presets[0]` in the store's initial state, and change `start()` to authorise with `presets[0].terms`.

Add `"resolveJsonModule": true` to `clients/ts/tsconfig.base.json`'s `compilerOptions`.

- [ ] **Step 5: Check it by eye**

Run the server and `npm run dev -w @tessera/viewer`. Switch principal from narrow to broad and confirm: the map visibly changes, and `visible` in the counts panel changes by the order of magnitude the measuring script reported. Confirm at a fixed viewport that `served ≤ visible` always.

- [ ] **Step 6: Commit**

```bash
git add clients/ts/scripts/measure-principals.mjs clients/ts/viewer clients/ts/tsconfig.base.json
git commit -m "feat(viewer): counts panel and measured principal presets

Counts sum the server's exact masked figures over the deepest loaded depth only
— summing parents retained by best-available refinement alongside their children
double-counts exactly the number this panel exists to be right about. Presets are
measured with a zoom=0 full-extent call, whose visible IS the principal's
visible-set size, rather than guessed."
```

---

### Task 6: Stats, the `k` control, and the error surface

**Files:**
- Create: `clients/ts/viewer/src/panels/{stats.ts,errors.ts}`
- Modify: `clients/ts/viewer/src/{main.ts,map.ts,state.ts}`

**Interfaces:**
- Consumes: `Store`, `AppState.lastTimings`, `AppState.failures`.
- Produces: `renderStats(state: AppState): string`, `renderErrors(state: AppState): string`, and the field order of `STAGE_FIELDS`.

- [ ] **Step 1: The stats panel**

`clients/ts/viewer/src/panels/stats.ts`:

```ts
import type {AppState} from '../state.js';
import {panel, row} from './counts.js';

/**
 * `x-tessera-stage-ns` is a positional CSV with no names — the field order is contract with
 * `scripts/bench_*.py`, append-only. Mirrored here; if the server appends a field, append here.
 */
const STAGE_FIELDS = [
  'generation_resolve_ns',
  'pin_resolve_ns',
  'slice_lookup_ns',
  'row_projection_ns',
  'compose_ns',
  'tiles_for_bbox_ns',
  'tile_ranges_ns',
  'count_ns',
  'select_ns',
  'gather_ns',
  'arrow_serialise_ns',
  'total_ns',
  'tiles_resolved',
  'tiles_nonempty',
  'sigma_visible',
  'rows_in_ranges',
  'select_rows_visited',
  'points_gathered',
  'row_projection_built',
  'theta_anchor_ns',
  'underlay_ns',
  'underlay_cells_evaluated'
] as const;

const INTERESTING = ['count_ns', 'select_ns', 'gather_ns', 'arrow_serialise_ns', 'total_ns'];

export function renderStats(state: AppState): string {
  const t = state.lastTimings;
  const drawn = [...state.tiles.values()].reduce((a, tile) => a + tile.pointCount, 0);

  const stage = t?.stageNs
    ? INTERESTING.map((name) => {
        const index = STAGE_FIELDS.indexOf(name as (typeof STAGE_FIELDS)[number]);
        const ns = t.stageNs![index] ?? 0;
        return row(name.replace(/_ns$/, ''), `${(ns / 1e6).toFixed(2)} ms`);
      }).join('')
    : `<div class="muted">stage timings absent — the server was built without the
        bench-timing feature, or [serve] stage_timing is false</div>`;

  return panel(
    'Last request',
    `${row('server', t ? `${(t.serverUs / 1000).toFixed(1)} ms` : '—')}
     ${row('admission', t ? `${(t.admissionUs / 1000).toFixed(1)} ms` : '—')}
     ${row('bytes', state.lastBytes.toLocaleString('en-GB'))}
     ${row('in flight', String(state.inFlight))}
     ${row('tiles held', String(state.tiles.size))}
     ${row('marks drawn', drawn.toLocaleString('en-GB'))}
     ${stage}`
  );
}

export function renderK(state: AppState): string {
  const max = state.meta?.selection.maxK ?? 1000;
  const value = state.k ?? state.meta?.selection.kMaxMarks ?? max;
  return panel(
    'k',
    `<input id="k" type="range" min="1" max="${max}" value="${value}" style="width:100%" />
     ${row('k', state.k === undefined ? `${value} (deployment default)` : String(state.k))}
     <div class="muted">lowering k narrows what the server serves; the MVP has no replica store
       to hold the non-decreasing rule, so this can decrease it</div>`
  );
}
```

- [ ] **Step 2: The error surface**

`clients/ts/viewer/src/panels/errors.ts`:

```ts
import type {AppState} from '../state.js';
import {panel} from './counts.js';

/**
 * A failed tile is not an empty tile. An empty viewport and a failed viewport are semantic
 * opposites — zero versus unknown — and rendering a failure as blank space converts a fail-closed
 * server into a fail-misleading picture. This panel is the MVP's whole display-state discipline.
 */
export function renderErrors(state: AppState): string {
  if (state.failures.length === 0) return '';
  const rows = state.failures
    .slice(-6)
    .reverse()
    .map((f) => `<div class="bad">${f.tileId} — ${f.code}: ${f.detail}</div>`)
    .join('');
  return panel(`Failures (${state.failures.length})`, rows);
}
```

- [ ] **Step 3: Record failures in the tile fetch**

In `map.ts`'s `getTileData`, wrap the client call:

```ts
        try {
          const response = await client.viewport(/* … as before … */);
          if (signal?.aborted) throw new Error('aborted');
          /* … as before … */
        } catch (error) {
          // An abort is not a failure — deck.gl aborts tiles that left the viewport, and recording
          // those would bury real failures under noise.
          if (!signal?.aborted) {
            const e = error as {code?: string; detail?: string; message?: string};
            store.update((s) => {
              s.failures.push({
                tileId: `${index.z}/${index.x}/${index.y}`,
                code: e.code ?? 'fetch-failed',
                detail: e.detail ?? e.message ?? String(error),
                at: Date.now()
              });
            });
          }
          throw error;
        } finally {
          store.update((s) => {
            s.inFlight -= 1;
          });
        }
```

- [ ] **Step 4: Render them, and wire the k slider**

In `main.ts`'s `render()`, extend the panel string and add the slider handler:

```ts
  panels.innerHTML =
    renderPrincipal(store.state, presets) +
    renderCounts(store.state) +
    renderK(store.state) +
    renderStats(store.state) +
    renderErrors(store.state);

  const kInput = document.getElementById('k') as HTMLInputElement | null;
  kInput?.addEventListener('change', () => {
    store.update((s) => {
      s.k = Number(kInput.value);
      s.tiles.clear();
    });
  });
```

Guard against a render loop: `render()` runs on every `store.update`, and `getTileData` calls `store.update` for `inFlight`. `innerHTML` replacement while a `<select>` or `<input>` has focus loses that focus. Fix it by only re-rendering the panels when a *frame* of state changed, not on every in-flight tick:

```ts
let pending = false;
store.subscribe(() => {
  if (pending) return;
  pending = true;
  requestAnimationFrame(() => {
    pending = false;
    render();
  });
});
```

Apply the same coalescing to the `deck.setProps({layers})` subscriber.

- [ ] **Step 5: Check it by eye**

Run the viewer. Confirm: the stats panel updates as you pan; `in flight` rises and falls; `marks drawn` tracks `served`; the `k` slider changes both the drawn count and `served`. Then stop the server mid-pan and confirm the failure panel fills with entries and that failed regions are not silently blank.

- [ ] **Step 6: Commit**

```bash
git add clients/ts/viewer
git commit -m "feat(viewer): stats, the k control, and a failure surface

Stats read x-tessera-server-us and x-tessera-admission-us, which are always
present, and degrade honestly when x-tessera-stage-ns is absent — that header
needs both the bench-timing feature and stage_timing = true. Failed tiles are
listed rather than rendered as empty space: a fail-closed server drawn blank is
a fail-misleading picture."
```

---

### Task 7: The density underlay and picking

**Files:**
- Create: `clients/ts/viewer/src/underlay.ts`, `clients/ts/viewer/src/panels/item.ts`
- Modify: `clients/ts/viewer/src/{map.ts,main.ts}`

**Interfaces:**
- Consumes: `ViewportResult.subCells`, `TesseraClient.item`.
- Produces: `subCellsToImage(subCells: SubCell[], offset: number): {data: Uint8ClampedArray; width: number; height: number}`, `renderItem(state: AppState): string`.

- [ ] **Step 1: The underlay raster**

`clients/ts/viewer/src/underlay.ts`:

```ts
import type {SubCell} from '@tessera/client';

/**
 * Rasterise a tile's masked sub-cell counts into an RGBA image for a BitmapLayer.
 *
 * Colour is **histogram equalisation** (datashader's `eq_hist`), which client-interaction §9
 * recommends over a fixed log transfer: it is the field's hard-won answer to distributions that
 * span several decades, and it is safely client-side because it is computed only from counts the
 * principal was served.
 *
 * The counts themselves are exact masked aggregates from the server. Nothing here estimates a
 * density; this only chooses how to colour numbers that arrived correct.
 */
export function subCellsToImage(
  subCells: SubCell[],
  offset: number
): {data: Uint8ClampedArray; width: number; height: number} {
  const side = 2 ** offset;
  const data = new Uint8ClampedArray(side * side * 4);
  if (subCells.length === 0) return {data, width: side, height: side};

  // Rank each distinct count; a cell's colour is its rank among the served counts, not its value.
  const sorted = [...new Set(subCells.map((c) => c.count))].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const rank = new Map<bigint, number>();
  sorted.forEach((count, i) => rank.set(count, sorted.length === 1 ? 1 : i / (sorted.length - 1)));

  // The low `2 * offset` bits of a sub-cell's Morton code are its position within the tile.
  const mask = (1n << BigInt(2 * offset)) - 1n;
  for (const {cell, count} of subCells) {
    const local = cell & mask;
    let x = 0;
    let y = 0;
    for (let bit = 0; bit < offset; bit++) {
      x |= Number((local >> BigInt(2 * bit)) & 1n) << bit;
      y |= Number((local >> BigInt(2 * bit + 1)) & 1n) << bit;
    }
    const t = rank.get(count) ?? 0;
    const index = (y * side + x) * 4;
    data[index] = 40 + t * 200;
    data[index + 1] = 60 + t * 120;
    data[index + 2] = 110 + t * 60;
    data[index + 3] = 150;
  }
  return {data, width: side, height: side};
}
```

The Morton de-interleave above assumes the sub-cell code's low `2 × offset` bits are `(y,x)` bit-interleaved with x in the even positions. Verify against `crates/tessera-spatial`'s encoder before accepting the visual result: if the underlay looks transposed or mirrored relative to the marks, swap the `x`/`y` shifts here. Marks and underlay disagreeing is the observable symptom, and it is why this task draws both at once.

- [ ] **Step 2: Draw it under the marks**

In `map.ts`, extend `renderSubLayers` to return an array with the underlay first:

```ts
      renderSubLayers: (props) => {
        const payload = props.data as TilePayload | null;
        if (!payload) return null;
        const {underlayOffset} = store.state;
        const b = props.tile.bbox as {left: number; top: number; right: number; bottom: number};
        const layers: Layer[] = [];

        if (underlayOffset > 0 && payload.result.subCells) {
          const image = subCellsToImage(payload.result.subCells, underlayOffset);
          layers.push(
            new BitmapLayer({
              id: `${props.id}-underlay`,
              image,
              bounds: [b.left, b.bottom, b.right, b.top],
              // Translucent, with depth off: tiles render in arbitrary order, so one tile's cells
              // could otherwise overdraw an adjacent tile's marks (client-interaction §8.2).
              opacity: 0.75,
              parameters: {depthCompare: 'always'}
            })
          );
        }

        if (payload.result.ids.length > 0) {
          layers.push(/* the ScatterplotLayer, unchanged from Task 4 */);
        }
        return layers;
      }
```

Import `BitmapLayer` from `@deck.gl/layers` and `subCellsToImage` from `./underlay.js`. Add an underlay-offset control to the stats panel or beside `k`:

```ts
export function renderUnderlay(state: AppState): string {
  const max = state.meta?.selection.maxUnderlayOffset ?? 0;
  return panel(
    'Density underlay',
    `<input id="underlay" type="range" min="0" max="${max}" value="${state.underlayOffset}"
       style="width:100%" />
     ${row('offset', state.underlayOffset === 0 ? 'off' : `+${state.underlayOffset}`)}
     ${row('sub-cells per tile', String(4 ** state.underlayOffset))}`
  );
}
```

Wire its `change` handler exactly as `k`'s, setting `s.underlayOffset` and clearing `s.tiles`.

- [ ] **Step 3: Picking and the item panel**

`clients/ts/viewer/src/panels/item.ts`:

```ts
import type {AppState} from '../state.js';
import {panel, row} from './counts.js';

export function renderItem(state: AppState): string {
  if (!state.selected) {
    return panel('Item', '<div class="muted">click a mark</div>');
  }
  const {id, scalars, externalId} = state.selected;
  const body =
    scalars.length === 0 && externalId === null
      ? `<div class="muted">this bundle declares no scalars and minted no external ids, so the
          round-trip is all there is to see — a 200 here means the identity resolved and the
          principal may see it</div>`
      : `${scalars.map((v, i) => row(`scalar ${i}`, String(v))).join('')}
         ${externalId ? row('external id', externalId) : ''}`;
  return panel('Item', `${row('tessera_id', id.toString())}${body}`);
}
```

In `main.ts`, add the click handler to the `Deck` constructor:

```ts
  onClick: async (info) => {
    // TileLayer picking returns a positional index into the tile's own buffers; identity is
    // resolved app-side from the Arrow column, so tessera_id never enters the render path.
    const payload = info.sourceLayer?.props?.data as {result: {ids: BigUint64Array}} | undefined;
    if (!payload || info.index < 0) {
      store.update((s) => {
        s.selected = null;
      });
      return;
    }
    const id = payload.result.ids[info.index];
    if (id === undefined || !store.state.session) return;
    try {
      const detail = await client.item(store.state.session.token, id);
      store.update((s) => {
        s.selected = {id, scalars: detail.scalars, externalId: detail.externalId};
      });
    } catch (error) {
      const e = error as {code?: string; detail?: string};
      store.update((s) => {
        s.failures.push({
          tileId: `item ${id}`,
          code: e.code ?? 'fetch-failed',
          detail: e.detail ?? String(error),
          at: Date.now()
        });
      });
    }
  },
```

`info.sourceLayer.props.data` is the sublayer's binary-attribute object, not the tile payload. If it does not carry `result`, pass the payload through explicitly instead: give the `ScatterplotLayer` a custom prop, e.g. `tessera: {ids: payload.result.ids}`, and read `info.sourceLayer.props.tessera.ids` here. Prefer that — it is stable against deck.gl's internal reshaping of `data`.

Add a selection overlay layer in `buildLayers`, **outside** the `TileLayer` (a `TileLayer` overrides its sublayers' `highlightedObjectIndex`, so a highlight prop on the scatterplot would not survive):

```ts
    ...(store.state.selectedWorldXY
      ? [
          new ScatterplotLayer({
            id: 'selection',
            data: [store.state.selectedWorldXY],
            getPosition: (d: [number, number]) => d,
            getFillColor: [255, 210, 90, 255],
            radiusUnits: 'pixels',
            getRadius: 5,
            stroked: true,
            getLineColor: [20, 20, 20, 255],
            lineWidthUnits: 'pixels',
            getLineWidth: 1.5
          })
        ]
      : [])
```

Add `selectedWorldXY: [number, number] | null` to `AppState`, initialised `null`, and set it in the click handler from `info.coordinate` (already world space).

- [ ] **Step 4: Check it by eye**

Run the viewer. Confirm: raising the underlay offset draws a density grid beneath the marks that aligns with where the marks are dense; the underlay is never drawn over marks; clicking a mark fills the item panel and highlights it; clicking empty space clears the selection.

- [ ] **Step 5: Commit**

```bash
git add clients/ts/viewer
git commit -m "feat(viewer): density underlay and picking

The underlay is the expected default rather than a garnish (client-interaction
§9): mark count is a sample, exact sub-cell counts are not. Coloured by histogram
equalisation, drawn translucent with depth off so arbitrary cross-tile order
cannot overdraw a neighbour's marks. Selection highlight is its own layer,
because TileLayer overrides its sublayers' highlightedObjectIndex."
```

---

### Task 8: The scale pass and the README

**Files:**
- Create: `clients/ts/README.md`
- Modify: `clients/ts/dev-server.toml` (untracked; document it in the README instead)

- [ ] **Step 1: Run against 1e8**

Point `dev-server.toml` at `data/bench-fixtures/1e8`, restart the server, re-run `measure-principals.mjs` (the term dictionary differs per fixture, so the presets must be re-measured), and use the viewer for several minutes.

Record in the README, as measured numbers rather than impressions: server time at zoom 0 and at a mid-depth viewport, bytes per tile, marks drawn, and whether panning stays responsive.

- [ ] **Step 2: Run against 1e9 if the machine allows**

`data/bench-fixtures/1e9` is 44 GB. Attempt it; if the machine cannot hold the row projection, record that outcome — a fail-closed refusal or an OOM is itself a finding worth writing down, and it belongs in the README rather than being discovered again later.

- [ ] **Step 3: Write the README**

`clients/ts/README.md` must contain, with no placeholders:

- What this is: an instrument for validating a running Tessera, not a shipped client. A pointer to the spec.
- Prerequisites: Node 22, a release `tessera` binary, a built bundle.
- The exact `dev-server.toml` contents from Task 3, and the two credential environment variables.
- The three commands: build, serve, `npm run dev -w @tessera/viewer`.
- How to regenerate `presets.json` and the golden fixtures, and **when you must**: presets per fixture, goldens per wire change.
- A "what this deliberately does not do" section reproducing spec §7's list.
- **A loud note that `serve.dev_cors_origins` and `VITE_TESSERA_SESSION_CREDENTIAL` are development-only**, that the credential is in the browser bundle, and that T2 with verified assertions is the documented integration topology.
- The measured scale numbers from Steps 1–2.

- [ ] **Step 4: Full check**

Run:

```bash
cargo test -p tessera-server
./scripts/check-layers.sh
cd clients/ts && npm test
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add clients/ts/README.md
git commit -m "docs(clients): how to run the viewer, and what it measured

Includes the scale-pass numbers against the 1e8 fixture and the deliberate
absences from the spec, so the instrument's limits travel with it."
```

---

## Self-Review

**Spec coverage.** §2 shape → Tasks 1, 3, 4. §3 CORS → Task 2. §4.1 cell space → Tasks 1, 3. §4.2 spike → Task 1. §4.3 per-tile fetch and abort → Task 4. §4.4 two sublayers → Tasks 4, 7. §4.5 picking and separate highlight layer → Task 7. §4.6 layer identity → Task 4. §5 counts → Task 5; principal → Task 5; item → Task 7; stats and `k` → Task 6; errors → Task 6. §6 testing → Tasks 1, 2, 3. §7 absences → Task 8's README. §8 sequencing → the task order, which matches it.

**Known soft spots, flagged for the implementer rather than hidden.** Three places where the plan states a hypothesis the code must confirm, each with the symptom and the fix written beside it: deck.gl's y direction and z↔depth mapping (Task 1, Step 5); Arrow's metadata/body padding in the stream walk (Task 3, Step 5); the sub-cell Morton bit order (Task 7, Step 1); and deck.gl's picking payload shape (Task 7, Step 3). None can be settled without running the code, and each names what to change and what not to.

**Type consistency.** `tileToCellBox`, `tileToDataBbox`, `dataToWorldXY`, `positionsToWorld` are defined once in `coords.ts` and used under those names throughout. `ViewportResult`/`ViewportResponse` are distinct and used distinctly (`response.result`). `panel()`/`row()` are defined in `panels/counts.ts` and imported by the other panels. `AppState` gains `selectedWorldXY` in Task 7, which Task 4's initial state does not have — Task 7 Step 3 says to add it.
