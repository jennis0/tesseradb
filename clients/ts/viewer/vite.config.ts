import {fileURLToPath} from 'node:url';
import {defineConfig, searchForWorkspaceRoot} from 'vite';
import {tesseraDecorators} from '@tesseradb/components/vite-plugin-decorators';

// Where `run_demo.sh` puts everything it produces — bundles, presets, and the `datasets.json` the
// picker reads. It is outside this package on purpose (the demo writes nothing into the source
// tree), so the dev server has to be told it may serve from there: Vite's `fs.allow` is what makes
// `/@fs/<absolute path>` reachable, and the URL the demo prints uses that form.
const demoDir =
  process.env.TESSERA_DEMO_DIR ?? fileURLToPath(new URL('../../../tessera-demo', import.meta.url));

export default defineConfig({
  plugins: [tesseraDecorators()],
  // strictPort, because the origin is enumerated in the server's `serve.dev_cors_origins`: a
  // silent fallback to another port would produce a CORS failure that reads as a broken server.
  // `VITE_PORT` runs a second viewer beside one already holding 5173; `run_demo.sh` writes
  // whichever port it is given into the deployments it generates.
  server: {
    port: Number(process.env.VITE_PORT ?? 5173),
    strictPort: true,
    fs: {allow: [searchForWorkspaceRoot(process.cwd()), demoDir]}
  }
});
