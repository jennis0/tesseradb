import {afterEach, describe, expect, it} from 'vitest';
import '../src/store-element.js';
import '../src/status.js';
import {TesseraStatus} from '../src/status.js';
import {TesseraCount} from '../src/count.js';
import {defineOnce} from '../src/define.js';
import {emit} from '../src/base.js';
import {fakeStore, mount, settle, status} from './fake-store.js';

/**
 * The mechanics of §5.9 a test can hold: store precedence — property, context, own, detached —
 * decided at connection with a later provider adopted only where the element built none;
 * `defineOnce` on a double import; events bubbling composed with decimal-string ids.
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
