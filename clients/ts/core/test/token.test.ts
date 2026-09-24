import {describe, expect, it} from 'vitest';
import {TokenSupply} from '../src/token.js';
import {fakeClock} from './support.js';

describe('when a refusal means the session ended', () => {
  it('reads bad-credential as expiry only once the token has been used, and expired-token always', async () => {
    const tokens = new TokenSupply(undefined, 'tok', fakeClock(), () => {});
    const badCredential = {code: 'bad-credential', detail: 'no such session'};
    const expired = {code: 'expired-token', detail: 'the token has expired'};
    expect(tokens.isExpiry(badCredential)).toBe(false);
    expect(tokens.isExpiry(expired)).toBe(true);

    await tokens.get();
    expect(tokens.isExpiry(badCredential)).toBe(false);

    await tokens.use();
    expect(tokens.isExpiry(badCredential)).toBe(true);
    expect(tokens.isExpiry(null)).toBe(false);
  });
});
