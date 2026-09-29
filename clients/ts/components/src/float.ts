/**
 * A list that opens over what sits below it: shown in the top layer, so no scrolling card clips it,
 * under the control it belongs to and as wide, or over the control where the window has more room
 * above. It follows the control each frame while it is open, as a card scrolls or the page moves.
 */

/** The gap between a control and its list, and the least room kept at the window's edges. */
const GAP = 4;
const EDGE = 8;
/** The tallest a list grows before it scrolls. */
const TALLEST = 320;

export class FloatingList {
  private frame: number | null = null;

  /** `find` returns the list while it is rendered, with the control it opens from; `null` while closed. */
  constructor(private readonly find: () => {list: HTMLElement; anchor: HTMLElement} | null) {}

  /** Show and place the list where it is rendered, and follow its control until it closes. Call after each render. */
  update(): void {
    const found = this.find();
    if (!found) {
      this.stop();
      return;
    }
    const {list} = found;
    if (typeof list.showPopover === 'function' && !list.matches(':popover-open')) {
      try {
        list.showPopover();
      } catch {
        // Shown already; the list is in the page either way.
      }
    }
    place(found.list, found.anchor);
    if (this.frame === null && typeof requestAnimationFrame !== 'undefined') {
      const tick = () => {
        const at = this.find();
        if (!at || !at.list.isConnected) {
          this.frame = null;
          return;
        }
        place(at.list, at.anchor);
        this.frame = requestAnimationFrame(tick);
      };
      this.frame = requestAnimationFrame(tick);
    }
  }

  /** Stop following. */
  stop(): void {
    if (this.frame !== null) cancelAnimationFrame(this.frame);
    this.frame = null;
  }
}

function place(list: HTMLElement, anchor: HTMLElement): void {
  const a = anchor.getBoundingClientRect();
  const vh = typeof innerHeight === 'number' ? innerHeight : 768;
  const below = vh - a.bottom - GAP - EDGE;
  const above = a.top - GAP - EDGE;
  const wanted = Math.min(TALLEST, list.scrollHeight || TALLEST);
  const up = below < wanted && above > below;
  const room = Math.max(80, Math.min(TALLEST, up ? above : below));
  const height = Math.min(wanted, room);
  list.style.left = `${Math.round(a.left)}px`;
  list.style.width = `${Math.round(a.width)}px`;
  list.style.maxHeight = `${Math.round(room)}px`;
  list.style.top = `${Math.round(up ? a.top - GAP - height : a.bottom + GAP)}px`;
}
