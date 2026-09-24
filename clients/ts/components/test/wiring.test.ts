import {afterEach, describe, expect, it, vi} from 'vitest';
import type {Store} from '@tesseradb/client';
import '../src/store-element.js';
import '../src/status.js';
import {TesseraStatus} from '../src/status.js';
import {TesseraCount} from '../src/count.js';
import {defineOnce} from '../src/define.js';
import {emit} from '../src/base.js';
import {fakeStore, mount, settle, status} from './fake-store.js';

/**
 * Store precedence (property, context, own, detached), an own store that follows its attributes,
 * `defineOnce` on a double import, and events bubbling composed with decimal-string ids.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

// An own store opens a session at once; there is no server here, and the refusal is the point.
globalThis.fetch = async () => {
  throw new Error('no server in this test');
};

describe('store precedence', () => {
  it('a .store property outranks a provider above', async () => {
    const provided = fakeStore({status: status({status: 'empty'})});
    const own = fakeStore({status: status({status: 'shown'})});
    const host = await mount('<tessera-store><tessera-status></tessera-status></tessera-store>');
    (host.querySelector('tessera-store') as unknown as {store: unknown}).store = provided;
    const el = host.querySelector('tessera-status') as TesseraStatus;
    el.store = own;
    await settle(host);
    expect(el.activeStore).toBe(own);
    expect(el.source).toBe('property');
  });

  it('a provider above answers at connection', async () => {
    const provided = fakeStore({status: status({status: 'empty'})});
    const host = await mount('<tessera-store><tessera-status></tessera-status></tessera-store>');
    (host.querySelector('tessera-store') as unknown as {store: unknown}).store = provided;
    await settle(host);
    const el = host.querySelector('tessera-status') as TesseraStatus;
    expect(el.activeStore).toBe(provided);
    expect(el.source).toBe('context');
  });

  it('a provider that connects later is adopted by a detached element — the context root replays', async () => {
    const host = await mount('<div id="wrap"><tessera-status></tessera-status></div>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    expect(el.source).toBe('detached');
    // Wrap it in a provider after the fact.
    const provider = document.createElement('tessera-store');
    const wrap = host.querySelector('#wrap')!;
    wrap.replaceChild(provider, el);
    provider.append(el);
    const provided = fakeStore({status: status({status: 'empty'})});
    (provider as unknown as {store: unknown}).store = provided;
    await settle(host);
    expect(el.activeStore).toBe(provided);
    expect(el.source).toBe('context');
  });

  it('a panel that cannot build its own store is detached without a provider or a property', async () => {
    const host = await mount('<tessera-status viewer-url="http://x" token="t"></tessera-status>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    expect(el.source).toBe('detached');
    expect(el.activeStore).toBeNull();
  });

  it('<tessera-store> builds its own store from attributes and provides it', async () => {
    const host = await mount('<tessera-store viewer-url="http://127.0.0.1:1" token="t"><tessera-status></tessera-status></tessera-store>');
    const provider = host.querySelector('tessera-store') as unknown as {source: string; activeStore: unknown; dispose(): void};
    expect(provider.source).toBe('own');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    expect(el.activeStore).toBe(provider.activeStore);
    provider.dispose();
  });

  it('disconnecting never disposes the store; reconnecting keeps it', async () => {
    const provided = fakeStore({status: status({status: 'empty'})});
    const host = await mount('<tessera-status></tessera-status>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    el.store = provided;
    await settle(host);
    el.remove();
    expect(provided.calls.some((c) => c.name === 'dispose')).toBe(false);
    host.append(el);
    await settle(host);
    expect(el.activeStore).toBe(provided);
  });
});

describe('configuration after connection', () => {
  type Provider = HTMLElement & {source: string; activeStore: Store | null; store: Store | null; viewerUrl: string; token: string; authorise: (() => Promise<string>) | null; dispose(): void};

  async function bare(markup = '<tessera-store><tessera-status></tessera-status></tessera-store>') {
    const host = await mount(markup);
    const el = host.querySelector('tessera-store') as Provider;
    return {host, el, child: host.querySelector('tessera-status') as TesseraStatus};
  }

  it('builds a store once viewer-url and a token arrive after insertion, and provides it', async () => {
    const {host, el, child} = await bare();
    expect(el.source).toBe('detached');
    el.setAttribute('viewer-url', 'http://127.0.0.1:1');
    await settle(host);
    expect(el.source).toBe('detached');
    el.setAttribute('token', 't');
    await settle(host);
    expect(el.source).toBe('own');
    expect(el.activeStore).not.toBeNull();
    expect(child.activeStore).toBe(el.activeStore);
    el.dispose();
  });

  it('builds one from an authorise supplier set after insertion', async () => {
    const {host, el} = await bare('<tessera-store viewer-url="http://127.0.0.1:1"></tessera-store>');
    el.authorise = async () => 't';
    await settle(host);
    expect(el.source).toBe('own');
    el.dispose();
  });

  it('rebuilds on a changed token, viewer-url or authorise, disposing the store it built', async () => {
    const {host, el, child} = await bare('<tessera-store viewer-url="http://127.0.0.1:1" token="a"><tessera-status></tessera-status></tessera-store>');
    const built: Store[] = [el.activeStore!];
    const disposed: Store[] = [];
    const watch = (s: Store) => vi.spyOn(s, 'dispose').mockImplementation(() => void disposed.push(s));
    watch(built[0]!);
    for (const change of [() => (el.token = 'b'), () => (el.viewerUrl = 'http://127.0.0.1:2'), () => (el.authorise = async () => 'c')]) {
      change();
      await settle(host);
      const next = el.activeStore!;
      expect(next).not.toBe(built.at(-1));
      expect(disposed).toEqual(built);
      expect(child.activeStore).toBe(next);
      built.push(next);
      watch(next);
    }
    // Nothing that names a store left: detached, and the last one disposed.
    el.viewerUrl = '';
    await settle(host);
    expect(el.source).toBe('detached');
    expect(el.activeStore).toBeNull();
    expect(child.activeStore).toBeNull();
    expect(disposed).toEqual(built);
  });

  it('keeps a store handed in by property through a configuration change, and never disposes it', async () => {
    const {host, el} = await bare('<tessera-store viewer-url="http://127.0.0.1:1" token="a"></tessera-store>');
    const own = el.activeStore!;
    const ownDisposed = vi.spyOn(own, 'dispose');
    const given = fakeStore({status: status({})});
    el.store = given;
    await settle(host);
    expect(el.source).toBe('property');
    expect(ownDisposed).toHaveBeenCalled();
    el.token = 'b';
    await settle(host);
    expect(el.activeStore).toBe(given);
    // Withdrawn, the element falls back to building its own from what it is configured with.
    el.store = null;
    await settle(host);
    expect(el.source).toBe('own');
    el.dispose();
    expect(given.calls.some((c) => c.name === 'dispose')).toBe(false);
  });

  it('keeps a store from a provider through its own configuration, and never disposes it', async () => {
    const provided = fakeStore({status: status({})});
    const host = await mount('<tessera-store><tessera-store id="inner"></tessera-store></tessera-store>');
    (host.querySelector('tessera-store') as Provider).store = provided;
    await settle(host);
    const inner = host.querySelector('#inner') as Provider;
    expect(inner.source).toBe('context');
    inner.viewerUrl = 'http://127.0.0.1:1';
    inner.token = 't';
    await settle(host);
    expect(inner.activeStore).toBe(provided);
    inner.dispose();
    expect(provided.calls.some((c) => c.name === 'dispose')).toBe(false);
  });

  for (const tag of ['tessera-map', 'tessera-explorer']) {
    it(`${tag} builds its own store from attributes set after insertion`, async () => {
      await import('../src/explorer.js');
      const host = await mount(`<${tag}></${tag}>`);
      const el = host.querySelector(tag) as unknown as Provider;
      expect(el.source).toBe('detached');
      el.viewerUrl = 'http://127.0.0.1:1';
      el.token = 't';
      await settle(host);
      expect(el.source).toBe('own');
      el.dispose();
      expect(el.activeStore).toBeNull();
    });
  }
});

describe('defineOnce', () => {
  it('a second definition of a tag is a no-op, so a double import does not throw', () => {
    expect(customElements.get('tessera-status')).toBe(TesseraStatus);
    expect(() => defineOnce('tessera-status', class extends HTMLElement {})).not.toThrow();
    expect(customElements.get('tessera-status')).toBe(TesseraStatus);
    expect(() => defineOnce('tessera-count', TesseraCount)).not.toThrow();
  });
});

describe('events', () => {
  it('bubble through shadow roots, composed, carrying ids as decimal strings', async () => {
    const host = await mount('<tessera-store><tessera-status></tessera-status></tessera-store>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    const inner = el.shadowRoot!.querySelector('[part="strip"]') as HTMLElement;
    let seen: CustomEvent | null = null;
    document.body.addEventListener('tessera-pick', (e) => (seen = e as CustomEvent));
    emit(inner, 'tessera-pick', {id: (2n ** 64n - 1n).toString(10)});
    expect(seen).not.toBeNull();
    expect(seen!.composed).toBe(true);
    expect(seen!.bubbles).toBe(true);
    expect(seen!.detail.id).toBe('18446744073709551615');
  });
});

describe('event details', () => {
  it('are typed on HTMLElementEventMap for a plain TypeScript listener', () => {
    const el = document.createElement('div');
    const seen: string[] = [];
    el.addEventListener('tessera-clausechange', (e) => seen.push(`${e.detail.layer}:${e.detail.id}:${e.detail.verb}`));
    el.addEventListener('tessera-levelchange', (e) => seen.push(String(e.detail.level)));
    // @ts-expect-error: a close names what closed, and carries no id.
    el.addEventListener('tessera-close', (e) => seen.push(e.detail.id));
    emit(el, 'tessera-clausechange', {id: '7', layer: 'mesh', outside: false, verb: 'highlight', on: true});
    emit(el, 'tessera-levelchange', {level: 2});
    expect(seen).toEqual(['mesh:7:highlight', '2']);
  });

  it('carry an artifact selection’s id as a decimal string, so the detail is JSON', async () => {
    const {TesseraMap} = await import('../src/map.js');
    const host = await mount('<tessera-map></tessera-map>');
    const map = host.querySelector('tessera-map') as InstanceType<typeof TesseraMap>;
    const store = fakeStore({status: status({})});
    map.store = store;
    await settle(host);
    const details: unknown[] = [];
    host.addEventListener('tessera-selectchange', (e) => details.push(e.detail));
    const shape = {kind: 'artifact', id: 2n ** 64n - 1n} as const;
    map.select(shape);
    store.set('region', {shape, status: 'shown', refusal: null, visible: null, matched: {value: 3, exact: true}, served: {shown: 3, total: 3, exact: true}, verdict: null, held: {ids: new BigUint64Array(), positions: new Float32Array(), count: 0}});
    expect(details).toHaveLength(2);
    for (const d of details) expect(JSON.parse(JSON.stringify(d)).shape).toEqual({kind: 'artifact', id: '18446744073709551615'});
  });
});

describe('<tessera-map> defaults', () => {
  it('draws no density wash unless a host asks for one', async () => {
    // Owner direction, 2026-08-26: how density should be rendered is its own conversation, and
    // the wash was confounding a pass over the map's hierarchy. The property and the machinery
    // behind it are untouched — only the default moved.
    await import('../src/map.js');
    const map = document.createElement('tessera-map') as unknown as {wash: boolean};
    expect(map.wash).toBe(false);
    const asked = document.createElement('div');
    asked.innerHTML = '<tessera-map wash></tessera-map>';
    document.body.append(asked);
    expect((asked.firstElementChild as unknown as {wash: boolean}).wash).toBe(true);
    asked.remove();
  });
});
