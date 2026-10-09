import {describe, expect, it, vi} from 'vitest';
import {MosaicaError} from '../src/client.js';
import type {ClauseVerb} from '../src/filters.js';
import type {FilterExpr} from '../src/types.js';
import {Suggestions, type SuggestState} from '../src/suggestions.js';
import type {SuggestResult} from '../src/types.js';
import {fakeClock} from './support.js';

type Ask = (column: string, q: string, filters: FilterExpr | null, signal: AbortSignal) => Promise<SuggestResult>;

/** A typeahead over `ask`, and the state it last published. */
function typeahead(ask: Ask, filtersFor: (column: string, verb: ClauseVerb) => FilterExpr | null = () => null) {
  const clock = fakeClock();
  let state: SuggestState = {suggestions: {}, suggestErrors: {}, suggestEpoch: 0};
  const part = new Suggestions(clock, filtersFor, ask, (next) => (state = next));
  return {clock, part, state: () => state};
}

const ok = (column: string, q: string, more = false) => ({status: 'ok' as const, column, q, values: [], more});

describe('the category typeahead', () => {
  it('debounces per column: a burst of keystrokes issues one request, for the last q named', async () => {
    const ask = vi.fn(async (column: string, q: string) => ok(column, q));
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'm', 'filter');
    part.suggest('admin4', 'ma', 'filter');
    part.suggest('admin4', 'mac', 'filter');
    await clock.advance(200);
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ask).toHaveBeenCalledWith('admin4', 'mac', null, expect.any(AbortSignal));
    expect(state().suggestions['admin4']).toEqual({q: 'mac', verb: 'filter', values: [], more: false, total: null});
  });

  it('discards a slower page that answers an earlier keystroke once a later one has landed', async () => {
    const releases = new Map<string, () => void>();
    const ask = vi.fn(
      (column: string, q: string) =>
        new Promise<SuggestResult>((resolve) => {
          releases.set(q, () => resolve({status: 'ok', column, q, values: [{code: 1, key: q, title: null, match: {field: 'key', start: 0, len: q.length}}], more: false}));
        })
    );
    const {clock, part, state} = typeahead(ask);

    // Two debounce windows apart, so both requests are in flight together.
    part.suggest('admin4', 'ma', 'filter');
    await clock.advance(200);
    part.suggest('admin4', 'mac', 'filter');
    await clock.advance(200);
    expect(releases.size).toBe(2);

    // The later request lands first; the earlier one must not overwrite it.
    releases.get('mac')!();
    await clock.advance(1);
    expect(state().suggestions['admin4']?.q).toBe('mac');
    releases.get('ma')!();
    await clock.advance(1);
    expect(state().suggestions['admin4']?.q).toBe('mac');
  });

  it('retries a shed (429) suggest after retryAfterS, and still applies the q-echo guard', async () => {
    let calls = 0;
    const ask = vi.fn(async (column: string, q: string): Promise<SuggestResult> => {
      calls++;
      if (calls === 1) return {status: 'shed', retryAfterS: 1, detail: null};
      return ok(column, q);
    });
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr', 'filter');
    await clock.advance(200);
    expect(calls).toBe(1);
    expect(state().suggestions['admin4']).toBeUndefined();
    await clock.advance(1000);
    expect(calls).toBe(2);
    expect(state().suggestions['admin4']).toEqual({q: 'fr', verb: 'filter', values: [], more: false, total: null});
  });

  it('still asks when the same q is asked again faster than the debounce', async () => {
    const ask = vi.fn(async (column: string, q: string) => ok(column, q));
    const {clock, part, state} = typeahead(ask);

    // A caller that does not dedupe its own asks re-asks the identical q faster than the debounce.
    for (let i = 0; i < 50; i++) {
      part.suggest('admin4', 'fr', 'filter');
      await clock.advance(10);
    }
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ask).toHaveBeenCalledWith('admin4', 'fr', null, expect.any(AbortSignal));
    expect(state().suggestions['admin4']).toEqual({q: 'fr', verb: 'filter', values: [], more: false, total: null});
  });

  it('a landed success clears a refusal the same column carried, and a refusal clears a stale page', async () => {
    let fail = true;
    const ask = vi.fn(async (column: string, q: string) => {
      if (fail) throw new MosaicaError(500, 'fail-closed', 'admin4 postings unreadable');
      return ok(column, q);
    });
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr', 'filter');
    await clock.advance(200);
    expect(state().suggestErrors['admin4']).toEqual({code: 'fail-closed', detail: 'admin4 postings unreadable'});
    expect(state().suggestions['admin4']).toBeUndefined();

    fail = false;
    part.suggest('admin4', 'fra', 'filter');
    await clock.advance(200);
    expect(state().suggestErrors['admin4']).toBeUndefined();
    expect(state().suggestions['admin4']).toEqual({q: 'fra', verb: 'filter', values: [], more: false, total: null});
  });

  it('a 429 with retry_after_s = 0 floors the retry delay rather than spinning, and stops after a bounded number of retries', async () => {
    let calls = 0;
    const ask = vi.fn(async (): Promise<SuggestResult> => {
      calls++;
      return {status: 'shed', retryAfterS: 0, detail: null};
    });
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr', 'filter');
    await clock.advance(120); // the debounce fires: call 1, shed, at t=120
    expect(calls).toBe(1);
    // The retry is floored at 250 ms from call 1 (t=370); 200 ms further is short of it.
    await clock.advance(200);
    expect(calls).toBe(1);
    await clock.advance(100);
    expect(calls).toBe(2);

    // Every retry is shed too: the retries stop and the last refusal is published.
    await clock.advance(5_000);
    const stalled = calls;
    await clock.advance(5_000);
    expect(calls).toBe(stalled);
    expect(state().suggestErrors['admin4']?.code).toBe('backpressure');
    expect(state().suggestions['admin4']).toBeUndefined();
  });

  it('an in-flight response straddling a reset is dropped, even where its (column, q) matches a fresh ask that followed the reset', async () => {
    // A control re-asks the empty-q page straight after a reset, so the stale response and the
    // fresh one share `(column, q)` and only the epoch tells them apart.
    const calls: ((v: SuggestResult) => void)[] = [];
    const ask = vi.fn(() => new Promise<SuggestResult>((resolve) => calls.push(resolve)));
    const {clock, part, state} = typeahead(ask);

    part.suggest('archive', '', 'filter');
    await clock.advance(200);
    expect(calls.length).toBe(1);

    part.reset();
    part.suggest('archive', '', 'filter');
    await clock.advance(200);
    expect(calls.length).toBe(2);

    calls[1]!(ok('archive', '', true));
    await clock.advance(1);
    expect(state().suggestions['archive']).toEqual({q: '', verb: 'filter', values: [], more: true, total: null});

    calls[0]!(ok('archive', '', false));
    await clock.advance(1);
    expect(state().suggestions['archive']).toEqual({q: '', verb: 'filter', values: [], more: true, total: null});
  });

  it('keeps the page its position asked for, with the total the server counted over', async () => {
    const ask = vi.fn(async (column: string, q: string) => ({...ok(column, q), total: 1234}));
    const {clock, part, state} = typeahead(ask, (_column, verb) => ({archive: {in: [verb]}}));

    part.suggest('archive', 'c', 'highlight');
    await clock.advance(200);
    expect(ask).toHaveBeenCalledWith('archive', 'c', {archive: {in: ['highlight']}}, expect.any(AbortSignal));
    expect(state().suggestions['archive']).toEqual({q: 'c', verb: 'highlight', values: [], more: false, total: 1234});

    // The same q from the other position is another question.
    part.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    expect(ask).toHaveBeenCalledTimes(2);
    expect(state().suggestions['archive']?.verb).toBe('filter');
  });

  it('publishes the server’s detail when an ask is still shed after its retries', async () => {
    const ask = vi.fn(async (): Promise<SuggestResult> => ({status: 'shed', retryAfterS: 0, detail: 'one suggest in flight'}));
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr', 'filter');
    await clock.advance(10_000);
    expect(state().suggestErrors['admin4']).toEqual({code: 'backpressure', detail: 'one suggest in flight'});
  });
});
