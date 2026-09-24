import type {ArtifactChannel} from './artifactChannel.js';
import type {Clock} from './driver.js';
import type {Presenter} from './presented.js';
import type {Replica} from './replica.js';

/**
 * One view's geometry machinery. Bands are quantised under one view's frame, a presenter draws one
 * view, and a channel's served set is in one view's row space, so each view has its own.
 */
export type ViewMachinery = {id: string; replica: Replica; presenter: Presenter; channel: ArtifactChannel};

/**
 * How long a view waits, after becoming current, before it asks for the camera it inherited. A
 * slider stepping through views faster than this issues one request, for the view it stops on.
 */
export const VIEW_SETTLE_MS = 140;

/**
 * Every view's machinery, built on first visit and kept on leaving, and which one is current. A
 * view that is not current has nothing in flight and no timer armed.
 */
export class HeldViews {
  private readonly byId = new Map<string, ViewMachinery>();
  private bound: ViewMachinery | null = null;
  private settleTimer: unknown = null;

  constructor(
    private readonly clock: Clock,
    private readonly build: (id: string) => ViewMachinery
  ) {}

  /** The current view's machinery, or null before the first {@link enter}. */
  get current(): ViewMachinery | null {
    return this.bound;
  }

  /** Whether a switch is waiting out {@link VIEW_SETTLE_MS} before it asks. */
  get settling(): boolean {
    return this.settleTimer !== null;
  }

  /** Every view held, the current one included. */
  all(): Iterable<ViewMachinery> {
    return this.byId.values();
  }

  /**
   * Make `id` current: cancel what the view being left has in flight and any pending settle, and
   * build the incoming view on its first visit. The machinery is built before the caller moves its
   * current id, so it publishes nothing while it is built.
   */
  enter(id: string): ViewMachinery {
    const outgoing = this.bound;
    outgoing?.presenter.cancel();
    outgoing?.channel.cancel();
    this.cancelSettle();
    let incoming = this.byId.get(id);
    if (!incoming) {
      incoming = this.build(id);
      this.byId.set(id, incoming);
    }
    this.bound = incoming;
    return incoming;
  }

  /** Call `fire` after {@link VIEW_SETTLE_MS}, unless another switch comes first. */
  settle(fire: () => void): void {
    this.cancelSettle();
    this.settleTimer = this.clock.after(VIEW_SETTLE_MS, () => {
      this.settleTimer = null;
      fire();
    });
  }

  cancelSettle(): void {
    if (this.settleTimer === null) return;
    this.clock.cancel(this.settleTimer);
    this.settleTimer = null;
  }
}
