/** One term set the demo offers as a principal, with the visible-set size measured for it. */
export type Preset = {label: string; terms: string[]; visible: number};

/**
 * One servable bundle: where it is, and enough about it to label the choice.
 *
 * **Presets travel with the dataset**, because a term dictionary is per bundle: the term ids that
 * name a 10% principal in one bundle name something else entirely in another, so a single shared
 * `presets.json` would silently mislabel every principal on the dataset it was not measured against.
 */
export type Dataset = {
  id: string;
  label: string;
  items: number;
  /** The text columns this bundle indexes — the reason one dataset differs from another. */
  prose: string[];
  viewerUrl: string;
  sessionUrl: string;
  presets: Preset[];
};

export type ViewerConfig = {
  /**
   * Every dataset a server is running for, from `datasets.json`.
   *
   * Never empty: with no document to read, this falls back to a single entry built from the
   * environment, which is the shape every earlier version of this viewer had.
   */
  datasets: Dataset[];
  sessionCredential: string;
  /**
   * Mark radius in pixels, and whether marks are pickable.
   *
   * **Two knobs that exist to answer one question the platform will not.** `painted` — the gap from
   * handing deck.gl its layers to the next frame — is the largest remaining cost, and deck's
   * `gpuTime` reads zero on this hardware because the GPU timer query extension is absent under
   * ANGLE, so the GPU half of that gap cannot be measured directly. It can be measured by
   * difference: halve the radius and, if `painted` falls, the cost is fill rate; turn picking off
   * and, if it falls, the cost is the per-instance picking-colour buffer deck regenerates whenever
   * the data object changes.
   *
   * Debug knobs, not settings. Both alter what is drawn or what can be clicked, so neither is
   * something to leave changed.
   */
  radius: number;
  pickable: boolean;
  /**
   * Bytes anticipation may absorb per still pause (`?ring=`, in MB).
   *
   * A knob rather than a constant because the right value depends on what is invisible from the
   * client: on a local socket against one server the budget's only real cost is decode-worker
   * occupancy ahead of foreground fetches, while over a real network and a shared fleet it is
   * bandwidth and aggregate select CPU — the documented unmeasured costs of the ring. The default
   * is deliberately modest for that reason; a dev box exploring a large corpus wants more.
   */
  ringBytes: number;
  /**
   * Whether the slab owns its GPU buffers and uploads dirty spans itself (`?gpu=0` to disable).
   *
   * The off switch exists because the external-buffer path leans on deck internals that have
   * surprised this client before (see the note at the end of `viewportLayer.ts`): if marks ever
   * misrender, `?gpu=0` restores the typed-array path in one reload and names the culprit.
   */
  gpuBuffers: boolean;
  /**
   * Zoom layers kept resident-but-undrawn beneath the current depth (`?layers=`, default 1).
   *
   * 0 for a resource-starved machine; 2+ where GPU memory and bandwidth afford it. Each layer
   * costs up to ~4x the viewport's bytes over novel ground and nothing over held ground; what it
   * buys is the one interaction the pan ring cannot help — a zoom notch landing on ground that
   * is already decoded, resident, and one partition swap from drawn.
   */
  prefetchLayers: number;
};

/**
 * Read from Vite env.
 *
 * `VITE_TESSERA_SESSION_CREDENTIAL` puts the deployment's session credential into the browser
 * bundle. That is a development shape and nothing else — it is why the server key permitting this
 * browser to talk at all (`serve.dev_cors_origins`) is off unless typed, and why neither is
 * anything to copy into an integration. See client-interaction §7 for the topology that is.
 */
export function readConfig(): ViewerConfig {
  const env = import.meta.env;
  const query = typeof location === 'undefined' ? null : new URLSearchParams(location.search);
  return {
    radius: Number(query?.get('radius') ?? '') || 1.6,
    pickable: query?.get('pickable') !== '0',
    ringBytes: (Number(query?.get('ring') ?? '') || 8) * 1_000_000,
    gpuBuffers: query?.get('gpu') !== '0',
    prefetchLayers: query?.has('layers') ? Math.max(0, Number(query.get('layers')) || 0) : 1,
    datasets: [],
    sessionCredential: env.VITE_TESSERA_SESSION_CREDENTIAL ?? ''
  };
}

/**
 * Load the dataset list — `datasets.json` if `run_demo.sh` wrote one, else the single server the
 * environment names.
 *
 * **Fetched rather than imported**, and that is the point: a bundled import would fix the list at
 * build time, so restarting the demo against a different set of bundles would need a viewer rebuild.
 * It also means this file no longer has to be rewritten in the repository to change what is served —
 * the previous shape rewrote a *tracked* `presets.json` on every run.
 *
 * A missing or malformed document is not an error. Falling back keeps `npm run dev` against a
 * hand-started server working, which is what the environment variables are for.
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
    presets: []
  };
  try {
    const response = await fetch('/datasets.json', {cache: 'no-store'});
    if (!response.ok) return [fallback];
    const body = (await response.json()) as {datasets?: Dataset[]};
    const datasets = (body.datasets ?? []).filter((d) => d.id && d.viewerUrl && d.sessionUrl);
    return datasets.length > 0 ? datasets : [fallback];
  } catch {
    return [fallback];
  }
}
