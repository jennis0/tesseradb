#!/usr/bin/env node
// Emit the subresource-integrity hash beside the single-file bundle: a page with no build step
// loads `tessera-components.js` with `integrity="sha384-…"`, and the browser refuses a file that
// differs from this build.
import {createHash} from 'node:crypto';
import {readFileSync, rmSync, statSync, writeFileSync} from 'node:fs';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const file = join(dist, 'tessera-components.js');
// Vite also emits the worker file the unbundled decoder would load. The bundle uses its inlined
// worker, so the file is removed and the distribution is the one file the hash names.
rmSync(join(dist, 'assets'), {recursive: true, force: true});
const bytes = readFileSync(file);
const sri = `sha384-${createHash('sha384').update(bytes).digest('base64')}`;
writeFileSync(join(dist, 'tessera-components.js.sri'), `${sri}\n`);
writeFileSync(
  join(dist, 'tessera-components.json'),
  JSON.stringify({file: 'tessera-components.js', bytes: statSync(file).size, integrity: sri}, null, 2) + '\n'
);
console.log(`${file}: ${statSync(file).size} bytes, ${sri}`);
