# @mosaicajs/components

The `mosaica-*` web components for [Mosaica](https://github.com/jennis0/mosaica), which serves an interactive map over billions of points from one machine, with every count, cluster and sample computed over what the signed-in viewer may see. The elements include a map, a filter panel, counts, item and field cards, layer and view pickers, and `<mosaica-explorer>`, which puts them together. They are built with Lit and draw through [`@mosaicajs/deck`](https://www.npmjs.com/package/@mosaicajs/deck).

```bash
npm install @mosaicajs/components lit @deck.gl/core @deck.gl/layers @deck.gl/aggregation-layers @luma.gl/core
```

```ts
import '@mosaicajs/components/explorer';

const explorer = document.createElement('mosaica-explorer');
explorer.setAttribute('viewer-url', 'http://localhost:8080');
// A viewer token that your own server obtained from Mosaica's session plane.
explorer.setAttribute('token', token);
document.body.append(explorer);
```

Each element has its own subpath, such as `@mosaicajs/components/map`. A page with no build step can load `dist/mosaica-components.js` instead: every element and its dependencies in one ES module, with its `sha384` integrity hash in `dist/mosaica-components.js.sri` and the notices of the third-party code inside it in `dist/mosaica-components.js.LICENSE.txt`.

Documentation: [Components reference](https://github.com/jennis0/mosaica/blob/main/docs/reference/components.md). Licence: Apache-2.0.
