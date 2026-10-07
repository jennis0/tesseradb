import {afterAll, beforeAll, describe, expect, it, type TestContext} from 'vitest';
import {TesseraClient} from '../src/client.js';
import {createStore, type Store} from '../src/store.js';
import type {Meta, Session} from '../src/types.js';
import {camera} from './support.js';
import {start, type Served} from './served.js';

/**
 * A filter that matches nothing, against a real `tessera serve` over the notebook corpus
 * (`served.ts`): the store draws nothing and the served artifacts say none of them matches.
 */

let served: Served | string = 'the server has not started';
let client: TesseraClient;
let session: Session;
let meta: Meta;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  client = new TesseraClient({viewerUrl: served.viewerUrl, sessionUrl: served.sessionUrl, sessionCredential: served.operatorCredential});
  session = await client.authorise({terms: ['cs.LG', 'cs.CV', 'hep-ph']});
  meta = await client.meta(session.token);
}, 120_000);

afterAll(() => {
  if (typeof served !== 'string') served.stop();
  client?.close();
});

function live(ctx: TestContext): void {
  if (typeof served === 'string') ctx.skip(served);
}

async function until(what: string, ok: () => boolean): Promise<void> {
  const deadline = Date.now() + 20_000;
  while (!ok()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

const drawn = (store: Store) => store.get('marks').bands.reduce((n, b) => n + b.ids.length, 0) + store.get('marks').standIn.reduce((n, p) => n + p.limit, 0);

describe('a filter matching nothing against a live server', () => {
  it('draws no point, and every served artifact reads as unmatched', async (ctx) => {
    live(ctx);
    const store = createStore({viewerUrl: (served as Served).viewerUrl, token: session.token, prefetch: false, artifacts: {perTile: meta.selection.maxArtifactsPerTile}});
    try {
      store.setLayers(['clusters/hdbscan']);
      const q = meta.views.find((v) => v.id === 's0')!.quantisation;
      store.setView(camera(q, [q.xMin, q.yMin, q.xMax, q.yMax], 800, 800));
      await until('the first frame and its artifacts', () => store.get('status').status === 'shown' && drawn(store) > 0 && store.get('artifacts').served.length > 0);

      // Every paper has one archive, and hep-ph is not one of cs's categories.
      store.setFilters({filter: {archive: {family: 'category', keys: ['cs']}, primary_category: {family: 'category', keys: ['hep-ph']}}, highlight: {}});
      await until('the empty answer', () => store.get('status').status === 'empty' && drawn(store) === 0);
      expect(store.get('view').matched.value).toBe(0);
      // Every tile is counted, though none serves a point.
      expect(store.get('view').visible.value).toBeGreaterThan(0);
      expect(store.get('tiles').tiles.every((t) => t.counts === null || t.counts.matched === 0n)).toBe(true);

      await until('the artifacts under the filter', () => store.get('artifacts').served.length > 0 && store.get('artifacts').served.every((a) => a.matched !== null));
      expect(store.get('artifacts').served.filter((a) => a.matched === true)).toEqual([]);
    } finally {
      store.dispose();
    }
  });
});
