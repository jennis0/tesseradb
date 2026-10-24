# @mosaicajs/deck

`MosaicaLayer`, a [deck.gl](https://deck.gl) layer that draws a [Mosaica](https://github.com/mosaicadb/mosaica) store, for a page that builds its own `Deck`. Mosaica serves an interactive map over billions of points from one machine, computed over what each viewer may see. The store comes from [`@mosaicajs/client`](https://www.npmjs.com/package/@mosaicajs/client).

```bash
npm install @mosaicajs/deck @mosaicajs/client @deck.gl/core @deck.gl/layers @deck.gl/aggregation-layers @luma.gl/core
```

```ts
import {Deck, OrthographicView} from '@deck.gl/core';
import {createStore} from '@mosaicajs/client';
import {MosaicaLayer, viewInputOf} from '@mosaicajs/deck';

const store = createStore({viewerUrl: 'http://localhost:8080', token});
let viewState = {target: [256, 256, 0], zoom: 0};

// Tell the store where the camera is looking. viewInputOf is null until the store has its frame.
function tell() {
  const input = viewInputOf(store, viewState, deck.width, deck.height);
  if (input) store.setView(input);
}

const deck = new Deck({
  parent: document.getElementById('map'),
  views: new OrthographicView({flipY: true}),
  viewState,
  controller: true,
  layers: [new MosaicaLayer({id: 'mosaica', store})],
  onViewStateChange: ({viewState: next}) => {
    viewState = next;
    deck.setProps({viewState});
    tell();
  }
});

// The first view goes out once the store has its frame, which arrives with `meta`.
store.subscribe('meta', (meta) => {
  if (meta) tell();
});
```

deck.gl and luma.gl are peer dependencies.

Documentation: [TypeScript client reference](https://github.com/mosaicadb/mosaica/blob/main/docs/reference/typescript.md). Licence: Apache-2.0.
