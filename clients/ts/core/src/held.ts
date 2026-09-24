import type {Shape, ShapeKind} from './types.js';

/**
 * Artifact shapes fetched by identifier, each asked for at most once.
 *
 * An id in `asked` and not in `held` is in flight or was refused, and is not asked again: a refused
 * shape is drawn as the artifact's box, as for a layer that declares no shape. A `derived` shape is
 * computed over one principal's visible members, so {@link forget} can drop those alone and keep
 * `predicate` and `authored` shapes, which are the same for every principal.
 */
export class HeldShapes {
  private held = new Map<bigint, Shape>();
  private kinds = new Map<bigint, ShapeKind>();
  private asked = new Set<bigint>();
  /** Moved by {@link forget}, so an answer asked for before it is not held. */
  private epoch = 0;
  private disposed = false;

  constructor(
    private readonly fetch: (id: bigint) => Promise<Shape | null>,
    /** The kind the artifact's layer declares, or null where the store does not know it. */
    private readonly kindOf: (id: bigint) => ShapeKind | null,
    private readonly publish: (shapes: ReadonlyMap<bigint, Shape>) => void
  ) {}

  /** A new map on every change, so a reader compares by identity. */
  get shapes(): ReadonlyMap<bigint, Shape> {
    return this.held;
  }

  need(id: bigint): void {
    if (this.asked.has(id)) return;
    this.asked.add(id);
    const epoch = this.epoch;
    void this.fetch(id).then(
      (shape) => {
        // An answer asked for before a forget may be another principal's, even where the id has
        // been asked for again since.
        if (this.disposed || epoch !== this.epoch) return;
        this.hold(id, shape);
      },
      () => {}
    );
  }

  /** Hold a shape that arrived with another answer, so {@link need} does not ask for it. */
  hold(id: bigint, shape: Shape | null): void {
    if (!shape) return;
    this.asked.add(id);
    this.held = new Map(this.held).set(id, shape);
    // An unknown kind is treated as `derived`, the kind that is dropped with the principal.
    this.kinds = new Map(this.kinds).set(id, this.kindOf(id) ?? 'derived');
    this.publish(this.held);
  }

  forget(which: 'derived' | 'all'): void {
    this.epoch += 1;
    if (this.held.size === 0 && this.asked.size === 0) return;
    if (which === 'all') {
      this.held = new Map();
      this.kinds = new Map();
      this.asked = new Set();
    } else {
      const keep = new Map<bigint, Shape>();
      const kinds = new Map<bigint, ShapeKind>();
      for (const [id, shape] of this.held) {
        const kind = this.kinds.get(id);
        if (kind !== undefined && kind !== 'derived') {
          keep.set(id, shape);
          kinds.set(id, kind);
        }
      }
      this.held = keep;
      this.kinds = kinds;
      this.asked = new Set(keep.keys());
    }
    this.publish(this.held);
  }

  dispose(): void {
    this.disposed = true;
  }
}

/**
 * Item records asked for by a hover, one request per id. A refusal is held as `null`, so a mark the
 * pointer keeps crossing is not asked about again.
 */
export class HeldRecords {
  private held = new Map<bigint, Record<string, unknown> | null>();
  private inFlight = new Map<bigint, Promise<Record<string, unknown> | null>>();
  /** Moved by {@link forget}, so an answer asked for before it is not held. */
  private epoch = 0;
  private disposed = false;

  constructor(private readonly fetch: (id: bigint) => Promise<Record<string, unknown>>) {}

  describe(id: bigint): Promise<Record<string, unknown> | null> {
    const held = this.held.get(id);
    if (held !== undefined) return Promise.resolve(held);
    const inFlight = this.inFlight.get(id);
    if (inFlight) return inFlight;
    const epoch = this.epoch;
    const request = this.fetch(id)
      .catch(() => null)
      .then((fields) => {
        if (this.disposed || epoch !== this.epoch) return fields;
        this.inFlight.delete(id);
        this.held.set(id, fields);
        return fields;
      });
    this.inFlight.set(id, request);
    return request;
  }

  forget(): void {
    this.held.clear();
    this.inFlight.clear();
    this.epoch += 1;
  }

  dispose(): void {
    this.disposed = true;
  }
}
