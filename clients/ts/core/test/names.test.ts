import {describe, expect, it} from 'vitest';
import type {Artifact} from '../src/types.js';
import {artifactName, attachedTextOf} from '../src/names.js';
import {artifact} from './support.js';

/** What an element calls an artifact: its own text, else the label attached to it, else nothing. */

const cluster = (id: bigint, content: string[] = []) => artifact(id, {layer: 'clusters', content});
const label = (layer: string, id: bigint, target: bigint, text: string) => artifact(id, {layer, content: [text], target});

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
    expect(artifactName({tesseraId: 1n, name: ''})).toBeNull();
    expect(artifactName({tesseraId: 1n, name: null})).toBeNull();
    expect(artifactName({tesseraId: 1n, name: 'Neoplasms'})).toBe('Neoplasms');
  });
});

describe('attachedTextOf', () => {
  it('keys each label’s first text by its target, and skips an artifact with no target or no text', () => {
    const text = attachedTextOf([cluster(1n, ['own']), label('topics', 9n, 1n, 'spin magnetic effect'), label('topics', 8n, 2n, '')], ['clusters', 'topics']);
    expect([...text]).toEqual([[1n, 'spin magnetic effect']]);
  });

  it('names a target two label layers attach to by the one declared first, whatever order they arrive in', () => {
    const tfidf = label('topics/tfidf', 9n, 1n, 'spin magnetic effect');
    const llm = label('topics/llm', 8n, 1n, 'Spintronics');
    const order = ['clusters', 'topics/llm', 'topics/tfidf'];
    expect(attachedTextOf([tfidf, llm], order).get(1n)).toBe('Spintronics');
    expect(attachedTextOf([llm, tfidf], order).get(1n)).toBe('Spintronics');
    // A layer the declaration does not list comes after every listed one.
    expect(attachedTextOf([label('unlisted', 7n, 1n, 'other'), tfidf], order).get(1n)).toBe('spin magnetic effect');
  });
});
