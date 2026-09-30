import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';

/**
 * `authorise` sends the candidate term list as base64, which the session route decodes and reads as
 * UTF-8 JSON. So the encoding must be base64 of the UTF-8 bytes. `btoa` alone maps each UTF-16 code
 * unit to one byte: it throws above U+00FF and emits Latin-1 below it, so a term with an en dash or
 * an accent would be refused or authorised against the wrong bytes.
 */
const captureAuthData = () => {
  const seen: string[] = [];
  const fetchStub = vi.fn(async (_url: string, init: {body: string}) => {
    seen.push(JSON.parse(init.body).auth_data as string);
    return {
      ok: true,
      json: async () => ({token: 't', token_id: 1, expires_at: 2})
    };
  });
  vi.stubGlobal('fetch', fetchStub);
  return seen;
};

const decoded = (authData: string): string =>
  new TextDecoder().decode(Uint8Array.from(atob(authData), (c) => c.charCodeAt(0)));

describe('the authorise term encoding', () => {
  // The stub is this file's alone: `fetch` goes back to the runtime's before the next file runs.
  afterEach(() => vi.unstubAllGlobals());

  const client = () =>
    new TesseraClient({viewerUrl: 'http://v', sessionUrl: 'http://s', sessionCredential: 'cred'});

  it('carries a term above U+00FF rather than refusing the request', async () => {
    const seen = captureAuthData();
    // An en dash, a curly apostrophe and a caron: `btoa` throws `InvalidCharacterError` on each.
    const terms = [
      'University of Wisconsin–La Crosse',
      'Estonian Naturalists’ Society',
      'Institut Ruđer Bošković'
    ];
    await client().authorise(terms);
    expect(JSON.parse(decoded(seen[0]!))).toEqual({terms});
  });

  it('encodes an accented term as UTF-8 and not as Latin-1', async () => {
    const seen = captureAuthData();
    const terms = ['Université de Montréal Biodiversity Centre'];
    await client().authorise(terms);
    // The bytes, not the code points: `é` is two bytes in UTF-8 and one in Latin-1, so a Latin-1
    // encoding round-trips through `atob` to the same string and is caught only on the bytes.
    const bytes = Uint8Array.from(atob(seen[0]!), (c) => c.charCodeAt(0));
    expect(bytes).toEqual(new TextEncoder().encode(JSON.stringify({terms})));
    expect(JSON.parse(decoded(seen[0]!))).toEqual({terms});
  });

  it('encodes a plain ASCII list exactly as before', async () => {
    const seen = captureAuthData();
    const terms = ['iNaturalist.org', 'observation.org'];
    await client().authorise(terms);
    expect(seen[0]).toBe(btoa(JSON.stringify({terms})));
  });
});
