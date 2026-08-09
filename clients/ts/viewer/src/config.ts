export type ViewerConfig = {
  viewerUrl: string;
  sessionUrl: string;
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
    viewerUrl: env.VITE_TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
    sessionUrl: env.VITE_TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
    sessionCredential: env.VITE_TESSERA_SESSION_CREDENTIAL ?? ''
  };
}
