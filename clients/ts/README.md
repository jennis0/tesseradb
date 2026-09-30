# TypeScript clients

This directory holds the browser side of Tessera: the client library, a deck.gl layer, the web components, React bindings, the demo viewer and four example pages. It is one npm workspace. This page is for someone working on these packages or embedding them in a page. What each package exports, and every element's attributes, events and parts, is in the reference on the documentation site: [TypeScript client](../../docs/reference/typescript.md) and [Components](../../docs/reference/components.md). How the client works, and why, is in [Clients](../../docs/system/clients.md).

## The packages

| Directory | Package | What it is |
|---|---|---|
| `core/` | `@tesseradb/client` | The headless store (`createStore`), the viewer and session client (`TesseraClient`), the control-plane client (`Control`) and the bulk reads. No DOM. Depends on `apache-arrow` and `fzstd`. |
| `deck/` | `@tesseradb/deck` | `TesseraLayer`, a deck.gl layer that draws a store, for a host with its own `Deck`. deck.gl and luma.gl are peer dependencies. |
| `components/` | `@tesseradb/components` | The `tessera-*` custom elements, built with Lit, drawing through `@tesseradb/deck`. One subpath per element, and a single-file bundle. |
| `react/` | `@tesseradb/react` | `useTesseraStore` and `useProjection`. `@tesseradb/react/components` wraps each element as a React component, behind optional peer dependencies, so the hooks alone pull in neither Lit nor deck.gl. |
| `viewer/` | `@tesseradb/viewer` | The demo: `<tessera-explorer>` with dataset and principal pickers and measurement panels. Private. |
| `examples/` | | Four host pages, described below. |
| `harness/` | | Checks the elements' behaviour in a browser against a running page. |
| `wire-example/` | | Decodes a `/v1/viewport` body with `apache-arrow` and no Tessera code. [Wire framing](../../docs/openapi/README.md) walks through it. |
| `spike/` | | Tests that deck.gl's tile indices match Tessera's tile grid. |

The packages depend on each other in one line: `components` on `deck`, and both on `client`. `react` depends on `client`, and on `components` only for its `/components` entry.

`deck` and `components` also import `@tesseradb/client/internal`, and `components` imports `@tesseradb/deck/internal`. These entries hold what those packages share and a host does not use. They are not a public API and change without notice, so the packages that use them depend on an exact version of the package they import from.

None of the packages is published to npm.

## Building

You need Node 20.19 or later (22.12 or later on the 22 line), which Vite 8 requires. Install from the lockfile:

```bash
npm ci
```

Inside the workspace the packages resolve to their TypeScript sources, through the `tessera-source` export condition. `tsconfig.base.json` names it, and so does every Vite and Vitest config that loads a workspace package, so the unit tests, the viewer and the examples run without a build. The components' browser config leaves it out, because that suite tests the built `dist/`; the plain-HTML, spike and wire-example configs load no workspace package.

```bash
npm run build
```

builds `client`, `deck`, `components` and `react`, in that order, into each package's `dist/`: ES2022 JavaScript and declarations from `tsc`. It also writes two things a host loads directly:

- `core/dist/decode.worker.js`, the decode worker, with Arrow bundled into it. The decoder loads it from beside itself.
- `components/dist/tessera-components.js`, every element and its dependencies in one minified file with the decode worker inlined, and `tessera-components.js.sri`, its `sha384` integrity hash.

`npm pack` in a package's directory makes a tarball. It drops the `tessera-source` condition from the packed manifest and adds the licence (`scripts/pack.mjs`).

The elements use standard decorators with `accessor`. Vite 8 does not lower them, so `components/vite-plugin-decorators.ts` runs esbuild over the component sources wherever Vite serves or bundles them: the viewer, the React example and the single-file bundle. `dist/` is lowered by `tsc`, so a host that installs the package needs no plugin.

## Testing

```bash
bash scripts/check-clients.sh     # from the repository root: all of the below, in order
```

The check runs these steps, each of which can be run alone from this directory:

| Step | Command | Needs |
|---|---|---|
| Typecheck every package and the operator scripts | `npm run typecheck` | |
| Build, then load each package entry from `dist/` in Node | `npm run build && node scripts/smoke-dist.mjs` | |
| Unit tests of every package | `npm test` | |
| Live tests of core against a real server | `npm --prefix core run test:live` | a `tessera` binary and the notebook corpus |
| Browser tests of the elements | `npm --prefix components run test:browser` | Chromium for Playwright, and a build |

The live tests build the notebook corpus into a temporary deployment, serve it, and call it through `TesseraClient`, `Control` and the bulk reads. They look for the binary in `TESSERA_BIN`, then this checkout's `target/release` or `target/debug`, then `PATH`, then a `target/release` or `target/debug` in any directory above this one, and for the corpus in `TESSERA_NOTEBOOK_DATA`, then `data/notebook/` at the root of the main checkout, which worktrees share. That directory is not in git. Where either is missing, each test is skipped and prints why. Build the binary with `cargo build --release -p tessera-cli`.

The browser tests run the elements in headless Chromium, and also load the built packages from `dist/`: the decode worker from its file with no bundler, and the elements built by Vite from their `dist/` modules. Install the browser once with `npx playwright install chromium`.

The reference on the documentation site is generated from the doc comments. `components/test/reference.test.ts`, part of `npm test`, fails when a public export or an element member has no doc comment. `node scripts/reference.mjs` writes the pages into `docs/reference/typescript/` and `docs/reference/components/`, which git ignores; `bash scripts/check-docs.sh` at the repository root runs both and builds the site.

`scripts/capture-golden.mjs` recaptures the decoder's fixtures in `core/test/fixtures/` and `wire-example/test/expected.json` from two servers it builds and starts. Run it after a change to the wire format. It finds the binary and the corpus as the live tests do, and needs `python3` with pyarrow.

## Running the demo

`run_demo.sh` at the repository root builds what is missing, starts `tessera serve` over one or more bundles and starts the viewer:

```bash
./run_demo.sh --scale notebook     # the arXiv notebook corpus, with its annotation layers
./run_demo.sh --scale 2m4          # 2,422,486 arXiv papers; the examples expect this one
./run_demo.sh --bundle PATH        # a bundle you already have
```

It prints the viewer's address, which carries the dataset document in `?datasets=`. Everything it writes goes under `tessera-demo/` at the repository root, which git ignores. The script's header lists its other options.

Two settings make the demo work, and both are for development only. `serve.dev_cors_origins` in the generated `tessera.toml` lets the viewer's origin call both the viewer and the session listeners from the browser, and the server logs a warning when it is set. `VITE_TESSERA_SESSION_CREDENTIAL` puts the session credential into the viewer's page, so the viewer can mint a token for each principal in its picker. A page embedding Tessera gets its tokens from its own server, as the plain-HTML example shows.

The viewer's Vite server listens on 5173, or on `VITE_PORT` where that is set, and fails to start where the port is taken instead of moving to another: the origin has to match the one in `dev_cors_origins`. `run_demo.sh` writes the viewer's port into the deployments it generates.

`viewer/smoke*.mjs` and `harness/harness.mjs` drive a running viewer in headless Chromium and report or check what it did. Each script's header says what it measures and which flags it takes. They are not part of `check-clients.sh`. The harness also runs against the plain-HTML example with `--url http://localhost:5180`.

## The examples

Each example is a workspace that `npm run typecheck` checks. They expect the demo's `2m4` scale, started with `./run_demo.sh --scale 2m4`: its viewer listener on `127.0.0.1:37585`, its session listener on `127.0.0.1:49303`, and the session credential `dev-session-credential`. The users in `examples/plain-html/users.json` hold access labels from that bundle's term dictionary. `TESSERA_VIEWER_URL`, `TESSERA_SESSION_URL` and `TESSERA_SESSION_CRED` point the examples at another deployment, whose users `users.json` then has to name.

The plain-HTML example's app server mints the tokens for all four, so start it after the demo and before the others. It listens on port 5180. The other three are Vite dev servers that proxy `/token` and `/users` to it and `/v1/*` to the viewer listener.

| Example | Port | What it shows |
|---|---|---|
| [`plain-html`](examples/plain-html/README.md) | 5180 | `<tessera-explorer>` from the single-file bundle, with no build step, and the app server that holds the session credential. |
| [`react-explorer`](examples/react-explorer/README.md) | 5181 | The React wrappers and hooks, with one slot filled by a host component. |
| [`canvas-store`](examples/canvas-store/README.md) | 5182 | The store under the host's own camera, drawn on a 2D canvas, with none of Tessera's rendering. |
| [`deck-host`](examples/deck-host/README.md) | 5183 | `TesseraLayer` in a `Deck` the page builds, beside a layer of its own. |

## Embedding the elements

The elements are custom elements, so any framework can render them. Frameworks differ in how they set a property whose value is an object, and in whether they need telling that a tag is not one of their own components.

Importing `@tesseradb/components` defines every element. A subpath such as `@tesseradb/components/count` defines one. The map, the explorer, the legend, the artifact list and the artifact card import `@tesseradb/deck` and so pull in deck.gl; the other subpaths do not. An element rendered before its module has loaded is an unknown element until it upgrades, which shows as blank space. Defining an element needs the browser's `customElements`, so a server-rendered app imports the package on the client only.

An element takes its store from its `store` property, or else from the nearest `<tessera-store>` or `<tessera-explorer>` above it in the page. `<tessera-map>`, `<tessera-explorer>` and `<tessera-store>` can also build their own from a `viewer-url` and a `token`, or an `authorise` function in place of the token. The component reference lists each element's attributes, properties and events.

### A page with no build step

The single-file bundle needs nothing else. Serve `tessera-components.js` beside the page and load it with its integrity hash, as `examples/plain-html/index.html` does:

```html
<script type="module" src="./tessera-components.js" integrity="sha384-..."></script>
<tessera-explorer viewer-url="http://localhost:5180" token="..."></tessera-explorer>
```

A page that loads the packages' `dist/` modules without a bundler needs an import map for their dependencies. For `@tesseradb/client` those are `apache-arrow` and `fzstd`, and Arrow's own imports `flatbuffers`, `tslib` and `json-with-bigint`. `components/test/browser/dist.browser.ts` builds such a page and lists the files each name maps to.

### A bundler and the decode worker

The decoder makes its worker from `new URL('./decode.worker.js', import.meta.url)`. Vite and webpack follow that reference and emit the file. esbuild does not: copy `@tesseradb/client/dist/decode.worker.js` beside the output, or install a factory with `setWorkerFactory(() => new Worker(...))` before the first store is made.

### React

`@tesseradb/react/components` sets object values as properties and turns each event into a handler prop, such as `onPick` for `tessera-pick`. React 19 sets properties on custom elements itself, so there the raw tags also work; the wrappers add the types, and are needed on React 18. `useTesseraStore` builds the store in an effect and disposes it in the cleanup, so StrictMode's double mount leaves no store running.

### Vue

Not tested here. Tell the compiler the tags are custom elements, or it warns that it cannot resolve `tessera-map` as a component:

```ts
// vite.config.ts
vue({template: {compilerOptions: {isCustomElement: (tag) => tag.startsWith('tessera-')}}})
```

Pass objects with property bindings, since an attribute carries only a string: `<tessera-status .store="store" />`, `<tessera-count .count="served" />`. Events bind as usual: `@tessera-pick="onPick"`.

### Svelte

Not tested here. Svelte sets a property when the element has one of that name, and an attribute otherwise. An element that upgrades after Svelte has set the value gets the attribute, so set objects explicitly with `bind:this` and an assignment. Events bind with `on:tessera-pick={onPick}`.

## Scripts

| Script | What it does |
|---|---|
| `scripts/capture-golden.mjs` | Recaptures the decoder's test fixtures. |
| `scripts/measure-principals.mjs` | Measures each candidate term's visible-set size on a running server and writes the viewer's principal presets. |
| `scripts/publish-clusters.mjs` | Registers a layer and publishes a k-means clustering of the corpus's points into it, so a bundle with no layers has one to draw. |
| `scripts/check-labels.mjs` | Checks on a running deployment which label description each principal is served, and that a label stops serving when its cluster is suppressed. |
| `scripts/write-cycle-demo.mjs` | Publishes a small cluster and a label, deletes a document the label was written from, and checks the label stays withdrawn. |
| `scripts/reference.mjs` | Generates the TypeScript and components reference pages. |

The operator scripts need `TESSERA_SESSION_CRED` and, where they write, `TESSERA_OPERATOR_CRED`. Each script's header gives its flags.
