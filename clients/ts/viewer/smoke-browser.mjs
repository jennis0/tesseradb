// How the smoke scripts launch and address a browser: argument parsing, the launcher, the one
// console error a healthy run produces, and URL parameters.
import {chromium} from 'playwright';

/**
 * `--flag value` pairs and bare `--flag` switches. A switch followed by another flag takes no
 * value, so `--headed --url http://…` reads as both; test a switch with `'headed' in args`.
 */
export function flags(argv = process.argv.slice(2)) {
  /** @type {[string, string | undefined][]} */
  const pairs = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg?.startsWith('--')) continue;
    const next = argv[i + 1];
    pairs.push([arg.slice(2), next !== undefined && !next.startsWith('--') ? next : undefined]);
  }
  return Object.fromEntries(pairs);
}

/**
 * The browser a script drives. Headless is the default and uses software GL (swiftshader), which
 * takes seconds per frame over a few million marks. `--headed` runs the browser on a display
 * (WSLg, or `xvfb-run`), so GPU and frame timings are real. `--executable` points at a Chromium
 * other than Playwright's own.
 *
 * @param {Record<string, string | undefined>} args parsed by {@link flags}
 */
export function launchBrowser(args) {
  const executablePath = args.executable;
  const beyond = executablePath ? {executablePath} : {};
  // `--any-origin` drops the browser's origin checks for this run, so a viewer on a port the
  // server's `serve.dev_cors_origins` does not list can reach it. The browser is this process's own.
  const origins = 'any-origin' in args ? ['--disable-web-security'] : [];
  return chromium.launch(
    'headed' in args
      ? {headless: false, args: ['--disable-gpu-sandbox', ...origins], ...beyond}
      : {args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox', ...origins], ...beyond}
  );
}

/**
 * The one console error a healthy run produces: Chromium logs `ERR_INCOMPLETE_CHUNKED_ENCODING`
 * for a streamed response the client aborted because the view moved on.
 *
 * @param {string} text
 */
export const isSupersededAbort = (text) => /ERR_INCOMPLETE_CHUNKED_ENCODING/.test(text);

/**
 * Add query parameters to a URL that may already carry some, such as `?prefetch=0` onto
 * `--url …/?dataset=notebook-2m4`.
 *
 * @param {string} url
 * @param {Record<string, string | number | undefined>} params
 */
export function withParams(url, params) {
  const query = Object.entries(params)
    .filter(([, value]) => value !== undefined)
    .map(([key, value]) => `${key}=${encodeURIComponent(String(value))}`)
    .join('&');
  if (!query) return url;
  return `${url}${url.includes('?') ? '&' : '?'}${query}`;
}
