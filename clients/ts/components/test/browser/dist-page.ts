import {TesseraExplorer} from '@tesseradb/components';

// Built by `dist.browser.ts` with Vite and no `tessera-source` condition, so every package
// resolves to its `dist/` build as it would for an installed consumer.
const explorer = document.createElement('tessera-explorer');
explorer.setAttribute('layout', 'overlay');
const map = document.createElement('tessera-map');
map.setAttribute('wash', '');
document.body.append(explorer, map);
await explorer.updateComplete;
await map.updateComplete;
(window as unknown as {result: unknown}).result = {
  instance: explorer instanceof TesseraExplorer,
  layout: explorer.layout,
  wash: map.wash,
  controls: explorer.shadowRoot?.querySelector('tessera-map')?.shadowRoot?.querySelector('[part="controls"]') !== null
};
