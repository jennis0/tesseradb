export type ViewerConfig = {
  viewerUrl: string;
  sessionUrl: string;
  sessionCredential: string;
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
  return {
    viewerUrl: env.VITE_TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
    sessionUrl: env.VITE_TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
    sessionCredential: env.VITE_TESSERA_SESSION_CREDENTIAL ?? ''
  };
}
