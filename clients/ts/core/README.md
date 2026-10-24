# @mosaicajs/client

The browser client for [Mosaica](https://github.com/jennis0/mosaica), which serves an interactive map over billions of points from one machine, with every count, cluster and sample computed over what the signed-in viewer may see.

This package is the headless part: a store that asks a Mosaica server for the map tiles, counts, filters and items the host's camera needs, and publishes them for the host to draw. It has no DOM and no renderer. It also holds `MosaicaClient` for the viewer and session routes, `Control` for the operator's control plane, and the bulk records read.

```bash
npm install @mosaicajs/client
```

```ts
import {createStore} from '@mosaicajs/client';

// `token` is a viewer token that your own server obtained from Mosaica's session plane.
const store = createStore({viewerUrl: 'http://localhost:8080', token});

// Draw each new set of marks the store publishes.
store.subscribe('marks', (marks) => draw(marks));

// On every camera change: what the canvas shows, in the current view's data coordinates.
store.setView({bbox: [-180, -90, 180, 90], zoom: 0, width: 1024, height: 512});
```

To draw with deck.gl, use [`@mosaicajs/deck`](https://www.npmjs.com/package/@mosaicajs/deck). For ready-made elements, use [`@mosaicajs/components`](https://www.npmjs.com/package/@mosaicajs/components).

Documentation: [TypeScript client reference](https://github.com/jennis0/mosaica/blob/main/docs/reference/typescript.md). Licence: Apache-2.0.
