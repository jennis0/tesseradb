#!/usr/bin/env node
// Import each built package from `dist/` in Node, as an installed consumer would resolve it, and
// fail if an entry does not load or lacks what a host uses. Run after `npm run build`.
import {existsSync} from 'node:fs';
import {fileURLToPath} from 'node:url';

const entries = {
  '@mosaica/client': ['createStore', 'setWorkerFactory', 'MosaicaClient', 'RecordsRead', 'Control'],
  '@mosaica/client/internal': ['compose', 'workerDecoder'],
  '@mosaica/deck': ['MosaicaLayer', 'viewInputOf', 'resolvePick'],
  '@mosaica/deck/internal': ['MarkSlab'],
  '@mosaica/components': ['MosaicaExplorer', 'MosaicaMap', 'MosaicaStore', 'storeContext', 'tokens', 'PARTS'],
  '@mosaica/components/count': ['MosaicaCount'],
  '@mosaica/react': ['useMosaicaStore', 'useProjection'],
  '@mosaica/react/components': ['MosaicaExplorer', 'MosaicaMap']
};

let failed = 0;
for (const [specifier, names] of Object.entries(entries)) {
  const url = import.meta.resolve(specifier);
  if (!url.includes('/dist/')) {
    console.error(`smoke-dist: ${specifier} resolved to ${url}, not to its dist/ build`);
    failed++;
    continue;
  }
  try {
    const module = await import(specifier);
    const missing = names.filter((name) => module[name] === undefined);
    if (missing.length) {
      console.error(`smoke-dist: ${specifier} does not export ${missing.join(', ')}`);
      failed++;
    }
  } catch (error) {
    console.error(`smoke-dist: ${specifier} does not load: ${error instanceof Error ? error.message : error}`);
    failed++;
  }
}

// The decoder loads its worker from a file beside it.
const worker = fileURLToPath(new URL('./decode.worker.js', import.meta.resolve('@mosaica/client')));
if (!existsSync(worker)) {
  console.error(`smoke-dist: the decode worker is not at ${worker}`);
  failed++;
}

if (failed) process.exit(1);
console.log(`smoke-dist: ${Object.keys(entries).length} entries load from dist/`);
