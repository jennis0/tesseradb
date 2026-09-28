import {TesseraError} from './client.js';
import type {Clock} from './driver.js';
import type {Refusal} from './presented.js';
import type {Store} from './store.js';

/**
 * A function the store calls for a viewer token, and calls again to renew it. It resolves to the
 * token and `expiresAt`, in seconds since the Unix epoch, as `/session/authorise` reports
 * `expires_at`; `Infinity` is a token that does not expire.
 *
 * The store renews 30 seconds before expiry, or halfway through a lifetime shorter than a minute,
 * and calls again before a request where the token has 5 seconds or less left. Concurrent requests
 * share one call. A rejection refuses the request that was waiting for the token.
 *
 * A store serves one viewer: to show another, call {@link Store.clear} or create a new store, as
 * {@link Store} sets out. Without that, the previous viewer's data stays published until this
 * supplier is next called, at the renewal, and one answer after it.
 *
 * @category Store
 */
export type TokenSupplier = () => Promise<{token: string; expiresAt: number}>;

function disposedError(): Error {
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
  /** Moved by {@link forget}, so a supplier call made before it installs nothing. */
  private generation = 0;

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

  /**
   * Drop the supplied token, so the next {@link get} calls the supplier. A fixed token is kept. A
   * supplier call in flight installs nothing, and its callers wait for the next call.
   */
  forget(): void {
    if (!this.supplier) return;
    this.generation += 1;
    this.token = null;
    this.expiresAtMs = Infinity;
    this.used = false;
    this.renewing = null;
    if (this.renewTimer) this.clock.cancel(this.renewTimer);
    this.renewTimer = null;
  }

  dispose(): void {
    this.disposed = true;
    if (this.renewTimer) this.clock.cancel(this.renewTimer);
  }

  private renew(supplier: TokenSupplier): Promise<string> {
    if (!this.renewing) {
      const renewing: Promise<string> = this.renewOnce(supplier).finally(() => {
        if (this.renewing === renewing) this.renewing = null;
      });
      this.renewing = renewing;
    }
    return this.renewing;
  }

  private async renewOnce(supplier: TokenSupplier): Promise<string> {
    const generation = this.generation;
    const got = await supplier();
    if (this.disposed) throw disposedError();
    if (generation !== this.generation) return this.get();
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
