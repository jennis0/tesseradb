import {TesseraError} from './client.js';
import type {Clock} from './driver.js';
import type {Refusal} from './presented.js';

/**
 * How the store gets a viewer token it renews before expiry. `expiresAt` is seconds since the Unix
 * epoch, as `/session/authorise` reports `expires_at`; `Infinity` is a token that does not expire.
 */
export type TokenSupplier = () => Promise<{token: string; expiresAt: number}>;

export function disposedError(): Error {
  return new Error('the store is disposed; create another store to ask again');
}

/**
 * The store's viewer token: a fixed one, or one from a supplier, renewed on a timer before it
 * expires. Concurrent callers waiting on a renewal share one supplier call.
 */
export class TokenSupply {
  private token: string | null;
  private expiresAtMs = Infinity;
  private renewTimer: unknown = null;
  private renewing: Promise<string> | null = null;
  private used = false;
  private disposed = false;

  constructor(
    private readonly supplier: TokenSupplier | undefined,
    fixed: string | undefined,
    private readonly clock: Clock,
    /** Called after each renewal; `changed` when it replaced a token held before. */
    private readonly onRenewed: (changed: boolean) => void
  ) {
    this.token = fixed ?? null;
  }

  /** The token held, or `null` before the first one lands. */
  get current(): string | null {
    return this.token;
  }

  /** The token to ask with: the one held while it has more than 5 s left, else the supplier's next. */
  async get(): Promise<string> {
    if (this.disposed) throw disposedError();
    if (this.token && Date.now() < this.expiresAtMs - 5_000) return this.token;
    if (!this.supplier) {
      if (!this.token) throw new TesseraError(401, 'bad-credential', 'no token');
      return this.token;
    }
    return this.renew(this.supplier);
  }

  /** {@link get}, recording that a request was made with the token, so a later 401 reads as expiry. */
  async use(): Promise<string> {
    const token = await this.get();
    this.used = true;
    return token;
  }

  /**
   * Whether a refusal means the session ended. A `bad-credential` on a token this store has used
   * is a swept session; before any use it is a bad option.
   */
  isExpiry(refusal: Refusal | null): boolean {
    if (!refusal) return false;
    if (refusal.code === 'expired-token') return true;
    return refusal.code === 'bad-credential' && this.used;
  }

  dispose(): void {
    this.disposed = true;
    if (this.renewTimer) this.clock.cancel(this.renewTimer);
  }

  private renew(supplier: TokenSupplier): Promise<string> {
    this.renewing ??= this.renewOnce(supplier).finally(() => {
      this.renewing = null;
    });
    return this.renewing;
  }

  private async renewOnce(supplier: TokenSupplier): Promise<string> {
    const got = await supplier();
    if (this.disposed) throw disposedError();
    const changed = this.token !== null && this.token !== got.token;
    this.token = got.token;
    this.expiresAtMs = got.expiresAt * 1000;
    this.armRenewal();
    this.onRenewed(changed);
    return this.token;
  }

  /** Renew 30 s before expiry, or halfway through a lifetime shorter than a minute. */
  private armRenewal(): void {
    if (this.renewTimer) this.clock.cancel(this.renewTimer);
    const supplier = this.supplier;
    if (!supplier || !Number.isFinite(this.expiresAtMs)) return;
    const left = this.expiresAtMs - Date.now();
    const wait = Math.max(0, left / 2, left - 30_000);
    this.renewTimer = this.clock.after(wait, () => {
      void this.renew(supplier).catch(() => {});
    });
  }
}
