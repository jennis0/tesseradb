import type {ArtifactChannel} from './artifactChannel.js';
import type {Clock} from './driver.js';
import type {Presenter} from './presented.js';
import type {Replica} from './replica.js';

/**
 * One view's geometry machinery. Bands are quantised under one view's frame, a presenter draws one
 * view, and a channel's served set is in one view's row space, so each view has its own.
 */
export type ViewMachinery = {replica: Replica; presenter: Presenter; channel: ArtifactChannel};

/**
 * How long a view waits, after becoming current, before it asks for the camera it inherited. A
 * slider stepping through views faster than this issues one request, for the view it stops on.
 */
const VIEW_SETTLE_MS = 140;

/**
 * Every view's machinery, built on first visit and kept on leaving, and which view is current. A
 * view that is not current has nothing in flight and no timer armed.
 */
export class HeldViews {
  private readonly byId = new Map<string, ViewMachinery>();
  private currentId: string;
  private settleTimer: unknown = null;

  constructor(
    private readonly clock: Clock,
    /** Builds a view's machinery. While it runs, {@link id} is still the view being left. */
    private readonly build: (id: string) => ViewMachinery,
    id: string
  ) {
    this.currentId = id;
  }

  /** The current view's id: the one named before meta, then one `meta.views` lists. */
  get id(): string {
    return this.currentId;
  }

  /** The current view's machinery, or null before the first {@link enter}. */
  get current(): ViewMachinery | null {
    return this.byId.get(this.currentId) ?? null;
  }

  /** Whether a switch is waiting out its settle before it asks. */
  get settling(): boolean {
    return this.settleTimer !== null;
  }

  /** Every view held, the current one included. */
  all(): Iterable<ViewMachinery> {
    return this.byId.values();
  }

  /** Name the current view before any view is built. */
  name(id: string): void {
    this.currentId = id;
  }

  /**
   * Make `id` current: cancel what the view being left has in flight and any pending settle, and
   * build the incoming view on its first visit. It is built before it becomes current, so it
   * publishes nothing while it is built.
   */
  enter(id: string): ViewMachinery {
    const outgoing = this.current;
    outgoing?.presenter.cancel();
    outgoing?.channel.cancel();
    this.cancelSettle();
    let incoming = this.byId.get(id);
    if (!incoming) {
      incoming = this.build(id);
      this.byId.set(id, incoming);
    }
    this.currentId = id;
    return incoming;
  }

  /**
   * Drop every view's machinery and any pending settle, keeping the current id. The caller cancels
   * what each view has in flight first.
   */
  forget(): void {
    this.byId.clear();
    this.cancelSettle();
  }

  /** Call `fire` after the settle, unless another switch comes first. */
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
