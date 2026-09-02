// How a smoke script gets a browser — one launcher, so the six of them stop carrying six copies
// of it.
//
// The copies had drifted apart in the way copies do: five passed `--disable-gpu-sandbox` and
// `smoke-budget.mjs` did not, two exempted the one console error a healthy run produces and four
// did not, and three took a `--url` while three addressed `http://localhost:5173` in a string
// literal. None of them had the two flags the acceptance harness grew at step 3, and that
// difference decided what they could drive: headless Chromium rasterises with swiftshader, so a
// frame over the 2.4M corpus takes seconds and outlasts the window every one of these scripts
// settles in. `smoke-artifacts.mjs` did not finish its layer × principal grid inside ten minutes
// there, and `smoke-budget.mjs` read its marks off paints that had not happened yet.
//
// This module is about **how the browser is launched and addressed** and nothing else. What each
// script checks is its own business and is unchanged.
import {chromium} from 'playwright';

/**
 * `--flag value` pairs and bare `--flag` switches, as every script parsed them.
 *
 * A switch whose next token is another flag takes no value, so `--headed --url http://…` reads as
 * both rather than as a `headed` of `--url`; `'headed' in args` is the test, in either case.
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
 * The browser a script drives.
 *
 * Headless is the default and is software GL: `--use-gl=swiftshader` with the unsafe-swiftshader
 * consent, which draws a 2.4M-mark frame in seconds. `--headed` runs the real browser on a display
 * (WSLg's, or under `xvfb-run`), which is where the GPU and the frame cadence are measured rather
 * than emulated — the same pair of shapes `harness.mjs` has taken since step 3.
 *
 * `--executable` points at a Chromium other than the one Playwright bundles. It is not a
 * convenience: Playwright 1.62's own headed build could not be downloaded on this machine (the CDN
 * timed out), so a headed run happens through the 1208 build already installed or not at all.
 *
 * @param {Record<string, string | undefined>} args parsed by {@link flags}
 */
export function launchBrowser(args) {
  const executablePath = args.executable;
  const beyond = executablePath ? {executablePath} : {};
  // `--any-origin` drops the browser's origin checks for this run. The server enumerates the
  // viewer's origin (`serve.dev_cors_origins`) and a deployment names one port, so a second
  // viewer on a second port — a worktree's, beside the one already holding the deployment's —
  // cannot reach it at all. Nothing this drives is about CORS, and a browser launched with it is
  // this process's own; it is never a way to reach a server from a page a user loaded.
  const origins = 'any-origin' in args ? ['--disable-web-security'] : [];
  return chromium.launch(
    'headed' in args
      ? {headless: false, args: ['--disable-gpu-sandbox', ...origins], ...beyond}
      : {args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox', ...origins], ...beyond}
  );
}

/**
 * The one console error a healthy run still produces.
 *
 * Chromium logs `ERR_INCOMPLETE_CHUNKED_ENCODING` against a streamed response the client abandoned
 * mid-flight, which is what a superseded request *is* — the driver aborts the one in flight when
 * the view moves. The harness has exempted it since step 3. Nothing else is excused.
 *
 * @param {string} text
 */
export const isSupersededAbort = (text) => /ERR_INCOMPLETE_CHUNKED_ENCODING/.test(text);

/**
 * Add query parameters to a URL that may already carry some.
 *
 * `?dataset=<id>` is how a run chooses which of the dataset document's servers to drive (the
 * document itself is named by `?datasets=`, which `run_demo.sh` prints), and `?prefetch=0`
 * is the look-ahead knob two of these scripts turn; a script given `--url …/?dataset=notebook-2m4`
 * has to add the second without dropping the first.
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
