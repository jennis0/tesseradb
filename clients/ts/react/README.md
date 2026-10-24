# @mosaicajs/react

React bindings for [Mosaica](https://github.com/jennis0/mosaica), which serves an interactive map over billions of points from one machine, with every count, cluster and sample computed over what the signed-in viewer may see. `useMosaicaStore` and `useProjection` hold a [`@mosaicajs/client`](https://www.npmjs.com/package/@mosaicajs/client) store in a component. `@mosaicajs/react/components` wraps each `mosaica-*` element of [`@mosaicajs/components`](https://www.npmjs.com/package/@mosaicajs/components) as a React component; it needs `@mosaicajs/components` and `@lit/react`, which the hooks alone do not.

```bash
npm install @mosaicajs/react @mosaicajs/components @lit/react
```

```tsx
import {useMosaicaStore, useProjection} from '@mosaicajs/react';
import {MosaicaExplorer} from '@mosaicajs/react/components';

function Map({authorise}: {authorise: () => Promise<{token: string; expiresAt: number}>}) {
  // `authorise` asks your own server for a viewer token, and is called again to renew it.
  const store = useMosaicaStore({viewerUrl: 'http://localhost:8080', authorise});
  const status = useProjection(store, 'status');
  if (!store) return null;
  return <MosaicaExplorer store={store} title={status?.status} />;
}
```

Documentation: [TypeScript client reference](https://github.com/jennis0/mosaica/blob/main/docs/reference/typescript.md). Licence: Apache-2.0.
