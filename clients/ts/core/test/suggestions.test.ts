import {describe, expect, it, vi} from 'vitest';
import {TesseraError} from '../src/client.js';
import {Suggestions, type SuggestState} from '../src/suggestions.js';
import type {SuggestResult} from '../src/types.js';
import {fakeClock} from './support.js';

type Ask = (column: string, q: string) => Promise<SuggestResult>;

/** A typeahead over `ask`, and the state it last published. */
function typeahead(ask: Ask) {
  const clock = fakeClock();
  let state: SuggestState = {suggestions: {}, suggestErrors: {}, suggestEpoch: 0};
  const part = new Suggestions(clock, ask, (next) => (state = next));
  return {clock, part, state: () => state};
}

const ok = (column: string, q: string, more = false) => ({status: 'ok' as const, column, q, values: [], more});

describe('the category typeahead', () => {
  it('debounces per column: a burst of keystrokes issues one request, for the last q named', async () => {
    const ask = vi.fn(async (column: string, q: string) => ok(column, q));
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'm');
    part.suggest('admin4', 'ma');
    part.suggest('admin4', 'mac');
    await clock.advance(200);
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ask).toHaveBeenCalledWith('admin4', 'mac');
    expect(state().suggestions['admin4']).toEqual({q: 'mac', values: [], more: false});
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
    part.suggest('admin4', 'ma');
    await clock.advance(200);
    part.suggest('admin4', 'mac');
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

  it('retries a superseded (429) suggest after retryAfterS, and still applies the q-echo guard', async () => {
    let calls = 0;
    const ask = vi.fn(async (column: string, q: string): Promise<SuggestResult> => {
      calls++;
      if (calls === 1) return {status: 'superseded', retryAfterS: 1};
      return ok(column, q);
    });
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr');
    await clock.advance(200);
    expect(calls).toBe(1);
    expect(state().suggestions['admin4']).toBeUndefined();
    await clock.advance(1000);
    expect(calls).toBe(2);
    expect(state().suggestions['admin4']).toEqual({q: 'fr', values: [], more: false});
  });

  it('churn re-asking the same q, faster than the debounce, does not starve it — the request still fires', async () => {
    const ask = vi.fn(async (column: string, q: string) => ok(column, q));
    const {clock, part, state} = typeahead(ask);

    // A caller that does not dedupe its own asks re-asks the identical q faster than the debounce.
    for (let i = 0; i < 50; i++) {
      part.suggest('admin4', 'fr');
      await clock.advance(10);
    }
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ask).toHaveBeenCalledWith('admin4', 'fr');
    expect(state().suggestions['admin4']).toEqual({q: 'fr', values: [], more: false});
  });

  it('a landed success clears a refusal the same column carried, and a refusal clears a stale page', async () => {
    let fail = true;
    const ask = vi.fn(async (column: string, q: string) => {
      if (fail) throw new TesseraError(500, 'fail-closed', 'admin4 postings unreadable');
      return ok(column, q);
    });
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr');
    await clock.advance(200);
    expect(state().suggestErrors['admin4']).toEqual({code: 'fail-closed', detail: 'admin4 postings unreadable'});
    expect(state().suggestions['admin4']).toBeUndefined();

    fail = false;
    part.suggest('admin4', 'fra');
    await clock.advance(200);
    expect(state().suggestErrors['admin4']).toBeUndefined();
    expect(state().suggestions['admin4']).toEqual({q: 'fra', values: [], more: false});
  });

  it('a 429 with retry_after_s = 0 floors the retry delay rather than spinning, and stops after a bounded number of retries', async () => {
    let calls = 0;
    const ask = vi.fn(async (): Promise<SuggestResult> => {
      calls++;
      return {status: 'superseded', retryAfterS: 0};
    });
    const {clock, part, state} = typeahead(ask);

    part.suggest('admin4', 'fr');
    await clock.advance(120); // the debounce fires: call 1, superseded, at t=120
    expect(calls).toBe(1);
    // The retry is floored at 250 ms from call 1 (t=370); 200 ms further is short of it.
    await clock.advance(200);
    expect(calls).toBe(1);
    await clock.advance(100);
    expect(calls).toBe(2);

    // Every retry is superseded too: the retries stop and the last refusal is published.
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

    part.suggest('archive', '');
    await clock.advance(200);
    expect(calls.length).toBe(1);

    part.reset();
    part.suggest('archive', '');
    await clock.advance(200);
    expect(calls.length).toBe(2);

    calls[1]!(ok('archive', '', true));
    await clock.advance(1);
    expect(state().suggestions['archive']).toEqual({q: '', values: [], more: true});

    calls[0]!(ok('archive', '', false));
    await clock.advance(1);
    expect(state().suggestions['archive']).toEqual({q: '', values: [], more: true});
  });
});
