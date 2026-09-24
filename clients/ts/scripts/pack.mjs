#!/usr/bin/env node
// Around `npm pack` in a package directory: `before` puts the repository's licence at the package
// root and drops the `tessera-source` conditions from the manifest, since the tarball holds no
// sources; `after` restores the manifest and removes the licence.
import {copyFileSync, readFileSync, renameSync, rmSync, writeFileSync} from 'node:fs';

const saved = '.package.json.unpacked';

function withoutSource(exports) {
  if (typeof exports !== 'object' || exports === null) return exports;
  return Object.fromEntries(
    Object.entries(exports)
      .filter(([key]) => key !== 'tessera-source')
      .map(([key, value]) => [key, withoutSource(value)])
  );
}

if (process.argv[2] === 'before') {
  copyFileSync(new URL('../../../LICENSE', import.meta.url), 'LICENSE');
  const text = readFileSync('package.json', 'utf8');
  writeFileSync(saved, text);
  const manifest = JSON.parse(text);
  manifest.exports = withoutSource(manifest.exports);
  writeFileSync('package.json', JSON.stringify(manifest, null, 2) + '\n');
} else if (process.argv[2] === 'after') {
  renameSync(saved, 'package.json');
  rmSync('LICENSE', {force: true});
} else {
  console.error('pack.mjs: give `before` or `after`.');
  process.exit(1);
}
