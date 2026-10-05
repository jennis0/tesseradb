/**
 * One principal the viewer offers, by its name in the dataset's catalogue, with the terms it holds
 * and the visible-set size measured for it.
 */
export type Preset = {label: string; principal: string; terms: string[]; visible: number};

/**
 * One servable bundle: where it is, and enough about it to label the choice. Presets belong to the
 * dataset because term ids are per bundle.
 */
export type Dataset = {
  id: string;
  label: string;
  items: number;
  /** The text columns this bundle indexes. */
  prose: string[];
  /** The record field that titles a point in the hover and the item card; absent, points show their ids. */
  titleField?: string;
  viewerUrl: string;
  sessionUrl: string;
  /** An API key whose principal holds `authorise-as` on this dataset's server, which mints each preset's session. */
  apiKey: string;
  presets: Preset[];
};

export type ViewerConfig = {
  /**
   * Every dataset a server is running for, from the document `?datasets=` names, else one entry
   * built from the environment. Not empty.
   */
  datasets: Dataset[];
  /**
   * Bytes look-ahead may fetch per pause (`?ring=`, in MB). Its cost is decode-worker time on a
   * local server, and bandwidth and server CPU over a network, which the client cannot see.
   */
  ringBytes: number;
  /**
   * Zoom layers held but not drawn beneath the current depth (`?layers=`, default 1), so a zoom
   * lands on decoded ground. Each costs up to about four times the viewport's bytes over new ground.
   */
  prefetchLayers: number;
  /** The most artifacts one level shows in one tile (`?per-tile=`, default 50). */
  artifactsPerTile: number;
};

/**
 * Read from the address. A dataset's `apiKey`, from the dataset document or
 * `VITE_TESSERA_API_KEY`, puts a key that can mint a session for any principal into the browser,
 * which is for development only, as is the server's `serve.dev_cors_origins` that lets this page
 * call it. Neither belongs in an integration.
 */
export function readConfig(): ViewerConfig {
  const query = typeof location === 'undefined' ? null : new URLSearchParams(location.search);
  return {
    ringBytes: (Number(query?.get('ring') ?? '') || 8) * 1_000_000,
    prefetchLayers: query?.has('layers') ? Math.max(0, Number(query.get('layers')) || 0) : 1,
    artifactsPerTile: query?.has('per-tile') ? Math.max(0, Number(query.get('per-tile')) || 0) : 50,
    datasets: []
  };
}

/**
 * Load the dataset list from the document `?datasets=<url>` names, else the single server the
 * environment names. Fetched at run time, so a different set of bundles needs no rebuild.
 * `run_demo.sh` writes the document outside the source tree and names it through Vite's `/@fs/`
 * route. A missing, unreachable or malformed document falls back to the environment.
 */
export async function loadDatasets(): Promise<Dataset[]> {
  const env = import.meta.env;
  const fallback: Dataset = {
    id: 'default',
    label: 'the running server',
    items: 0,
    prose: [],
    viewerUrl: env.VITE_TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
    sessionUrl: env.VITE_TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
    apiKey: env.VITE_TESSERA_API_KEY ?? '',
    presets: []
  };
  // `?datasets=` on the address, else the document named in the environment.
  const source =
    typeof location === 'undefined'
      ? null
      : (new URLSearchParams(location.search).get('datasets') ?? env.VITE_TESSERA_DATASETS ?? null);
  if (!source) return [fallback];
  try {
    const response = await fetch(source, {cache: 'no-store'});
    if (!response.ok) return [fallback];
    const body = (await response.json()) as {datasets?: Dataset[]};
    const datasets = (body.datasets ?? []).filter((d) => d.id && d.viewerUrl && d.sessionUrl);
    return datasets.length > 0 ? datasets : [fallback];
  } catch {
    return [fallback];
  }
}
