import {spawn, spawnSync, type ChildProcess} from 'node:child_process';
import {randomBytes} from 'node:crypto';
import {chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, symlinkSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {delimiter, dirname, join, resolve} from 'node:path';
import {createInterface} from 'node:readline';

/**
 * A real `tessera serve` over the notebook corpus, for the live tests.
 *
 * The binary is `TESSERA_BIN`, then this checkout's own `target/release` or `target/debug`, then
 * `tessera` on `PATH`, then a target directory above the checkout; the one used is printed. The
 * corpus is `TESSERA_NOTEBOOK_DATA`, then `data/notebook/` beside the git common directory, which
 * every worktree of a checkout shares. Where either is missing, `start` returns the reason instead
 * of a server, and each test skips with it. A `TESSERA_BIN` naming no file is an error.
 *
 * The declaration served is the notebook's own `schema.toml` with one view group added, two views
 * over the same points, so `/v1/meta` has a group and a roster to decode. A caller may pass another
 * corpus instead: a directory of Parquet sources and the declaration that reads them.
 */
export type Served = {
  viewerUrl: string;
  sessionUrl: string;
  sessionCredential: string;
  controlUrl: string;
  operatorCredential: string;
  stop(): void;
};

const here = import.meta.dirname;

/** The view group the live declaration adds. */
export const GROUP = {name: 'copies', title: 'Two copies', keys: ['first', 'second'], labels: ['First copy', 'Second copy']};

/** The binary and where it was found, or null where there is none. */
function findBinary(): {path: string; from: string} | null {
  const named = process.env.TESSERA_BIN;
  if (named) {
    if (!existsSync(named)) throw new Error(`TESSERA_BIN names ${named}, which does not exist; build it or unset TESSERA_BIN`);
    return {path: named, from: 'TESSERA_BIN'};
  }
  const inTarget = (root: string) =>
    ['release', 'debug'].map((profile) => join(root, 'target', profile, 'tessera')).find((candidate) => existsSync(candidate));
  // This checkout's own build first, so a worktree tests the server built from its own sources.
  const top = spawnSync('git', ['rev-parse', '--show-toplevel'], {cwd: here, encoding: 'utf8'});
  const own = top.status === 0 ? inTarget(top.stdout.trim()) : undefined;
  if (own) return {path: own, from: "this checkout's target"};
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    if (dir && existsSync(join(dir, 'tessera'))) return {path: join(dir, 'tessera'), from: 'PATH'};
  }
  for (let dir = here; dirname(dir) !== dir; dir = dirname(dir)) {
    const found = inTarget(dir);
    if (found) return {path: found, from: 'a target directory above this checkout'};
  }
  return null;
}

function findCorpus(): string | null {
  const named = process.env.TESSERA_NOTEBOOK_DATA;
  if (named) return existsSync(join(named, 'schema.toml')) ? named : null;
  const roots: string[] = [];
  const common = spawnSync('git', ['rev-parse', '--git-common-dir'], {cwd: here, encoding: 'utf8'});
  if (common.status === 0) roots.push(dirname(resolve(here, common.stdout.trim())));
  roots.push(resolve(here, '../../../..'));
  for (const root of roots) {
    if (existsSync(join(root, 'data', 'notebook', 'schema.toml'))) return join(root, 'data', 'notebook');
  }
  return null;
}

/** The notebook declaration, anchored on its own view, with the group appended. */
function declaration(corpus: string): string {
  const schema = readFileSync(join(corpus, 'schema.toml'), 'utf8');
  const anchored = schema.replace(/^\[defaults\]\n/m, '[defaults]\nallocation_view = "s0"\n');
  if (anchored === schema) throw new Error(`${corpus}/schema.toml has no [defaults] table to anchor the added group on`);
  const views = GROUP.keys
    .map((key, i) => `\n[[view_group.view]]\nkey    = "${key}"\nsource = "points"\nlabel  = "${GROUP.labels[i]}"\n`)
    .join('');
  return `${anchored}
[[view_group]]
name             = "${GROUP.name}"
title            = "${GROUP.title}"
extent           = "auto"
point_visibility = { field = "categories", default = "public" }
metadata         = { label = "text" }
${views}`;
}

const DEPLOYMENT = `[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "schema.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer                   = "127.0.0.1:0"
session                  = "127.0.0.1:0"
control                  = "127.0.0.1:0"
session_credential_file  = ".tessera/session.cred"
operator_credential_file = ".tessera/operator.cred"
`;

/** Resolves with the three bound addresses from the child's `listening` line, or rejects. */
function listening(child: ChildProcess, errors: () => string, timeoutMs: number): Promise<{viewer: string; session: string; control: string}> {
  return new Promise((resolveListening, reject) => {
    const timer = setTimeout(() => reject(new Error(`tessera serve announced no listening line within ${timeoutMs} ms:\n${errors()}`)), timeoutMs);
    child.once('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`tessera serve exited with ${code} before listening:\n${errors()}`));
    });
    createInterface({input: child.stdout!}).on('line', (line) => {
      if (!line.startsWith('{')) return;
      try {
        const event = JSON.parse(line) as {event?: string; viewer?: string; session?: string; control?: string};
        if (event.event !== 'listening' || !event.viewer || !event.session || !event.control) return;
        clearTimeout(timer);
        resolveListening({viewer: event.viewer, session: event.session, control: event.control});
      } catch {
        // A line of the child's own logging that happens to start with a brace.
      }
    });
  });
}

/**
 * Build a corpus into a temporary deployment and serve it: the notebook's, or `corpus`.
 *
 * Returns a string, the reason, where the binary or the notebook corpus is not on this machine. A
 * build or a serve that fails throws: those are failures, not absences.
 */
export async function start(
  options: {corpus?: {directory: string; schema: string}} = {}
): Promise<Served | string> {
  let corpus = options.corpus;
  const found = findBinary();
  if (!found) return 'no tessera binary: set TESSERA_BIN, put tessera on PATH, or run cargo build --release -p tessera-cli';
  const binary = found.path;
  console.log(`live test: serving with ${binary}, from ${found.from}`);
  if (!corpus) {
    const notebook = findCorpus();
    if (!notebook) return 'data/notebook/ is not in this checkout; set TESSERA_NOTEBOOK_DATA';
    corpus = {directory: notebook, schema: declaration(notebook)};
  }

  const directory = mkdtempSync(join(tmpdir(), 'tessera-ts-live-'));
  try {
    for (const name of readdirSync(corpus.directory)) {
      if (name.endsWith('.parquet')) symlinkSync(join(corpus.directory, name), join(directory, name));
    }
    writeFileSync(join(directory, 'schema.toml'), corpus.schema);
    writeFileSync(join(directory, 'tessera.toml'), DEPLOYMENT);
    const secrets = join(directory, '.tessera');
    mkdirSync(join(secrets, 'cache'), {recursive: true});
    chmodSync(secrets, 0o700);
    const sessionCredential = randomBytes(24).toString('hex');
    writeFileSync(join(secrets, 'session.cred'), `${sessionCredential}\n`, {mode: 0o600});
    const operatorCredential = randomBytes(24).toString('hex');
    writeFileSync(join(secrets, 'operator.cred'), `${operatorCredential}\n`, {mode: 0o600});
    const deployment = join(directory, 'tessera.toml');

    const build = spawnSync(binary, ['build', '--deployment', deployment], {cwd: directory, encoding: 'utf8'});
    if (build.status !== 0) throw new Error(`tessera build failed (${build.status}):\n${build.stderr}${build.stdout}`);

    const child = spawn(binary, ['serve', '--deployment', deployment], {cwd: directory, stdio: ['ignore', 'pipe', 'pipe']});
    let stderr = '';
    child.stderr!.on('data', (chunk: Buffer) => {
      stderr = (stderr + chunk.toString()).slice(-8192);
    });
    // By pid, and at exit too, so a runner that stops early leaves no server behind.
    const kill = () => {
      if (child.exitCode === null && child.signalCode === null && child.pid !== undefined) process.kill(child.pid, 'SIGTERM');
    };
    process.once('exit', kill);
    const stop = () => {
      kill();
      process.off('exit', kill);
      rmSync(directory, {recursive: true, force: true});
    };
    try {
      const at = await listening(child, () => stderr, 30_000);
      return {
        viewerUrl: `http://${at.viewer}`,
        sessionUrl: `http://${at.session}`,
        sessionCredential,
        controlUrl: `http://${at.control}`,
        operatorCredential,
        stop
      };
    } catch (error) {
      stop();
      throw error;
    }
  } catch (error) {
    rmSync(directory, {recursive: true, force: true});
    throw error;
  }
}
