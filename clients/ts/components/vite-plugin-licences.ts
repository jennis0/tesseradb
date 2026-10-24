import {readdirSync, readFileSync} from 'node:fs';
import {join} from 'node:path';
import type {Plugin, Rolldown} from 'vite';

/**
 * Write the notices of every third-party package inside the single-file bundle to
 * `<bundle>.LICENSE.txt` beside it: each package's name, version, licence, licence text and NOTICE
 * file where it has one, read from its directory under `node_modules`. MIT, BSD and Apache-2.0 require the notice to travel
 * with a redistributed copy, and the bundle is redistributed inside the Python wheel.
 *
 * The decode worker is bundled separately and inlined, so the worker build gets its own collector
 * (`worker`) over the same set. Vite builds the worker while it transforms the main entry, so the
 * worker's packages are in the set by the time the main build writes the file.
 */
export function mosaicaLicences(bundleFile: string): {main: Plugin; worker: Plugin} {
  const roots = new Set<string>();
  const collect = (bundle: Rolldown.OutputBundle) => {
    for (const output of Object.values(bundle)) {
      if (output.type !== 'chunk') continue;
      for (const id of output.moduleIds) {
        const path = id.replace(/^\0/, '').split('?')[0] ?? '';
        const at = path.lastIndexOf('/node_modules/');
        if (at < 0) continue;
        const base = at + '/node_modules/'.length;
        const parts = path.slice(base).split('/');
        roots.add(path.slice(0, base) + parts.slice(0, parts[0]?.startsWith('@') ? 2 : 1).join('/'));
      }
    }
  };
  return {
    main: {
      name: 'mosaica-licences',
      apply: 'build',
      generateBundle(_options, bundle) {
        collect(bundle);
        this.emitFile({type: 'asset', fileName: `${bundleFile}.LICENSE.txt`, source: notices(bundleFile, roots)});
      }
    },
    worker: {
      name: 'mosaica-licences-worker',
      generateBundle(_options, bundle) {
        collect(bundle);
      }
    }
  };
}

function notices(bundleFile: string, roots: Set<string>): string {
  const entries = new Map<string, string>();
  for (const root of roots) {
    const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as {
      name: string;
      version: string;
      license?: string;
    };
    const key = `${manifest.name}@${manifest.version}`;
    const licence = manifest.license ?? 'unstated';
    const files = readdirSync(root);
    const file = files.find((f) => /^(licen[cs]e|copying)(\.|-|$)/i.test(f));
    const text = file
      ? readFileSync(join(root, file), 'utf8').trim()
      : `The package states the licence ${licence} and ships no licence file.`;
    // Apache-2.0 §4(d): a package's NOTICE travels with every redistributed copy.
    const notice = files
      .filter((f) => /^notice(\.|-|$)/i.test(f))
      .map((f) => `\n\n${f}:\n\n${readFileSync(join(root, f), 'utf8').trim()}`)
      .join('');
    entries.set(key, `${key}\nLicence: ${licence}\n\n${text}${notice}\n`);
  }
  const header =
    `${bundleFile} contains the following third-party packages, each listed with its version,\n` +
    'its licence and the licence text it is distributed with.\n';
  const body = [...entries.keys()].sort().map((key) => entries.get(key));
  return [header, ...body].join(`\n${'-'.repeat(80)}\n\n`);
}
