import type {Browser} from 'playwright';
import {afterAll, beforeAll, describe, expect, it} from 'vitest';
import {ELEMENTS, launch, open, serve} from '../scripts/serve.mjs';

/**
 * The gallery loads every element in every state it builds without an exception or a console
 * error, and no specimen fails to build or settle.
 */

let server: {url: string; close: () => Promise<void>};
let browser: Browser;

beforeAll(async () => {
  server = await serve();
  browser = await launch();
});

afterAll(async () => {
  await browser?.close();
  await server?.close();
});

async function load(query: Record<string, string>): Promise<{errors: string[]; failed: number; specimens: number}> {
  const page = await browser.newPage({viewport: {width: 1280, height: 900}});
  const errors: string[] = [];
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text());
  });
  page.on('pageerror', (e) => errors.push(e.message));
  try {
    await open(page, server.url, query);
    const failed = await page.locator('.frame.failed').count();
    const specimens = await page.locator('figure.specimen').count();
    return {errors, failed, specimens};
  } finally {
    await page.close();
  }
}

describe('the gallery', () => {
  for (const el of ELEMENTS) {
    it(`renders every state of tessera-${el} without an error`, async () => {
      const {errors, failed, specimens} = await load({el});
      expect(errors).toEqual([]);
      expect(failed).toBe(0);
      expect(specimens).toBeGreaterThan(0);
    });
  }

  it('renders the whole page in each foreign theme without an error', async () => {
    for (const theme of ['editorial', 'console']) {
      const {errors, failed} = await load({theme, scheme: 'dark', width: '280'});
      expect(errors).toEqual([]);
      expect(failed).toBe(0);
    }
  });
});
