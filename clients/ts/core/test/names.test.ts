import {describe, expect, it} from 'vitest';
import type {Artifact} from '../src/types.js';
import {artifactName, attachedTextOf} from '../src/names.js';
import {artifact} from './support.js';

/** What an element calls an artifact: its own text, else the label attached to it, else nothing. */

const cluster = (id: bigint, content: string[] = []) => artifact(id, {layer: 'clusters', content});
const label = (layer: string, id: bigint, target: bigint, text: string, over: Partial<Artifact> = {}) => artifact(id, {layer, content: [text], target, ...over});
const flat = (...names: string[]) => names.map((name) => ({name, hierarchy: {kind: 'flat' as const, pruneChildren: false}}));

describe('artifactName', () => {
  it('is the artifact’s own text, else its attached label, else null, and never its key', () => {
    const attached = new Map([[2n, 'decoders, thresholds']]);
    expect(artifactName(cluster(1n, ['quantum error correction']), attached)).toBe('quantum error correction');
    expect(artifactName(cluster(2n), attached)).toBe('decoders, thresholds');
    expect(artifactName(cluster(3n), attached)).toBeNull();
    const keyed: Artifact = {...cluster(3n), key: 'hdb-3'};
    expect(artifactName(keyed, attached)).toBeNull();
  });

  it('takes an empty text as none, on an artifact and on a browse row', () => {
    const attached = new Map([[2n, 'decoders, thresholds']]);
    expect(artifactName(cluster(1n, ['']))).toBeNull();
    expect(artifactName(cluster(2n, ['']), attached)).toBe('decoders, thresholds');
    expect(artifactName({mosaicaId: 1n, name: ''})).toBeNull();
    expect(artifactName({mosaicaId: 1n, name: null})).toBeNull();
    expect(artifactName({mosaicaId: 1n, name: 'Neoplasms'})).toBe('Neoplasms');
  });
});

describe('attachedTextOf', () => {
  it('keys each label’s first text by its target, and skips an artifact with no target or no text', () => {
    const text = attachedTextOf([cluster(1n, ['own']), label('topics', 9n, 1n, 'spin magnetic effect'), label('topics', 8n, 2n, '')], flat('clusters', 'topics'));
    expect([...text]).toEqual([[1n, 'spin magnetic effect']]);
  });

  it('names a target two label layers attach to by the one declared first, whatever order they arrive in', () => {
    const tfidf = label('topics/tfidf', 9n, 1n, 'spin magnetic effect');
    const llm = label('topics/llm', 8n, 1n, 'Spintronics');
    const order = flat('clusters', 'topics/llm', 'topics/tfidf');
    expect(attachedTextOf([tfidf, llm], order).get(1n)).toBe('Spintronics');
    expect(attachedTextOf([llm, tfidf], order).get(1n)).toBe('Spintronics');
    // A layer the declaration does not list comes after every listed one.
    expect(attachedTextOf([label('unlisted', 7n, 1n, 'other'), tfidf], order).get(1n)).toBe('spin magnetic effect');
  });

  it('within one layer takes the lower level, then a key before none, then the lower key, then the first met', () => {
    const levels = [{name: 'topics', hierarchy: {kind: 'stacked' as const, pruneChildren: false}}];
    const deep = label('topics', 9n, 1n, 'deep', {key: 'a', rung: 1});
    const shallow = label('topics', 8n, 1n, 'shallow', {key: 'z', rung: 0});
    expect(attachedTextOf([deep, shallow], levels).get(1n)).toBe('shallow');
    const z = label('topics', 7n, 1n, 'by z', {key: 'z'});
    const a = label('topics', 6n, 1n, 'by a', {key: 'a'});
    const bare = label('topics', 5n, 1n, 'keyless', {key: null});
    expect(attachedTextOf([bare, z, a], flat('topics')).get(1n)).toBe('by a');
    expect(attachedTextOf([z, bare], flat('topics')).get(1n)).toBe('by z');
    const first = label('topics', 4n, 1n, 'first', {key: null});
    const second = label('topics', 3n, 1n, 'second', {key: null});
    expect(attachedTextOf([first, second], flat('topics')).get(1n)).toBe('first');
    // On a layer with one level, `rung` is a depth and does not order.
    const deeper = label('topics', 2n, 1n, 'deeper', {key: 'a', rung: 2});
    expect(attachedTextOf([z, deeper], flat('topics')).get(1n)).toBe('deeper');
  });
});
