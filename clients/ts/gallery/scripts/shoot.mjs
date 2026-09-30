import {mkdirSync} from 'node:fs';
import {join, resolve} from 'node:path';
import {ELEMENTS, launch, open, root, serve} from './serve.mjs';

/**
 * One PNG per element in the light and dark schemes at the default theme and natural width, and one
 * per element in each foreign theme, each cropped to that element's specimens.
 *
 * Usage: `npm run shoot -w gallery -- [out-dir] [element ...]`. The directory defaults to
 * `gallery/shots/`; naming elements shoots only those.
 */

const [dir, ...only] = process.argv.slice(2);
const out = resolve(dir ?? join(root, 'shots'));
const elements = only.length > 0 ? only : ELEMENTS;
const unknown = elements.filter((e) => !ELEMENTS.includes(e));
if (unknown.length > 0) {
  console.error(`shoot: no element named ${unknown.join(', ')}. Name one of: ${ELEMENTS.join(', ')}.`);
  process.exit(1);
}
mkdirSync(out, {recursive: true});

/** Each shot's query and file name suffix. */
const SHOTS = [
  {query: {scheme: 'light', theme: 'default'}, suffix: 'light'},
  {query: {scheme: 'dark', theme: 'default'}, suffix: 'dark'},
  {query: {scheme: 'light', theme: 'editorial'}, suffix: 'editorial'},
  {query: {scheme: 'dark', theme: 'console'}, suffix: 'console'}
];

const server = await serve();
const browser = await launch();
try {
  const page = await browser.newPage({viewport: {width: 1600, height: 900}, deviceScaleFactor: 2});
  for (const el of elements) {
    for (const {query, suffix} of SHOTS) {
      await open(page, server.url, {...query, width: 'natural', el, shoot: '1'});
      const path = join(out, `${el}-${suffix}.png`);
      await page.locator(`section[data-el="${el}"]`).screenshot({path});
      console.log(`shoot: ${path}`);
    }
  }
} finally {
  await browser.close();
  await server.close();
}
console.log(`shoot: wrote ${elements.length * SHOTS.length} screenshots to ${out}`);
