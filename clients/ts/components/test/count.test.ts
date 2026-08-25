import {afterEach, describe, expect, it} from 'vitest';
import '../src/count.js';
import type {TesseraCount} from '../src/count.js';
import {deep, mount, settle} from './fake-store.js';

/** `<tessera-count>` for every branch of the formatter's rule. */

afterEach(() => {
  document.body.innerHTML = '';
});

async function render(set: (el: TesseraCount) => void): Promise<Element> {
  const host = await mount('<tessera-count label="shown"></tessera-count>');
  const el = host.querySelector('tessera-count') as TesseraCount;
  set(el);
  await settle(host);
  return deep(host, '[part="count"]')!;
}

describe('<tessera-count>', () => {
  it('a sample: both figures when exact', async () => {
    const c = await render((el) => (el.count = {shown: 221, total: 1_994_089, exact: true}));
    expect(c.textContent).toBe('221 of 1,994,089');
    expect(c.getAttribute('data-kind')).toBe('sample');
    expect(c.getAttribute('data-empty')).toBe('false');
  });
  it('a sample: neither when not exact', async () => {
    const c = await render((el) => (el.count = {shown: 221, total: 1_994_089, exact: false}));
    expect(c.textContent).toBe('');
    expect(c.getAttribute('data-empty')).toBe('true');
  });
  it('a sample: neither against a stale view', async () => {
    const c = await render((el) => {
      el.count = {shown: 221, total: 1_994_089, exact: true};
      el.stale = true;
    });
    expect(c.textContent).toBe('');
  });
  it('a scalar: one figure when exact', async () => {
    const c = await render((el) => (el.masked = {value: 12_465, exact: true}));
    expect(c.textContent).toBe('12,465');
    expect(c.getAttribute('data-kind')).toBe('scalar');
  });
  it('a scalar: marked approximate when inexact, never hidden', async () => {
    const c = await render((el) => (el.masked = {value: 12_465, exact: false}));
    expect(c.textContent).toBe('≈ 12,465');
    expect(c.getAttribute('data-exact')).toBe('false');
  });
  it('a scalar: nothing against a stale view', async () => {
    const c = await render((el) => {
      el.masked = {value: 12_465, exact: true};
      el.stale = true;
    });
    expect(c.textContent).toBe('');
  });
  it('nothing at all with neither a count nor a scalar', async () => {
    const c = await render(() => {});
    expect(c.textContent).toBe('');
    expect(c.getAttribute('data-kind')).toBe('none');
  });
  it('a label is shown only beside a figure', async () => {
    const host = await mount('<tessera-count label="shown"></tessera-count>');
    const el = host.querySelector('tessera-count') as TesseraCount;
    el.count = {shown: 1, total: 2, exact: false};
    await settle(host);
    expect(deep(host, '[part="label"]')).toBeNull();
    el.count = {shown: 1, total: 2, exact: true};
    await settle(host);
    expect(deep(host, '[part="label"]')?.textContent).toBe('shown');
  });
});
