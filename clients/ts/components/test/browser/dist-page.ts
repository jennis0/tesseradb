import {MosaicaExplorer} from '@mosaicajs/components';

// Built by `dist.browser.ts` with Vite and no `mosaica-source` condition, so every package
// resolves to its `dist/` build as it would for an installed consumer.
const explorer = document.createElement('mosaica-explorer');
explorer.setAttribute('layout', 'overlay');
const map = document.createElement('mosaica-map');
map.setAttribute('density', 'hex');
map.setAttribute('no-points', '');
document.body.append(explorer, map);
await explorer.updateComplete;
await map.updateComplete;
(window as unknown as {result: unknown}).result = {
  instance: explorer instanceof MosaicaExplorer,
  layout: explorer.layout,
  density: map.density,
  noPoints: map.noPoints,
  controls: explorer.shadowRoot?.querySelector('mosaica-map')?.shadowRoot?.querySelector('[part="controls"]') !== null
};
