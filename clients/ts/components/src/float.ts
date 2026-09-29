/**
 * A list that opens over what sits below it: shown in the top layer, so no scrolling card clips it,
 * under the control it belongs to and as wide, or over the control where the window has more room
 * above. It follows the control each frame while it is open, as a card scrolls or the page moves,
 * and hides while the control is scrolled out of the card that holds it. Where the browser has no
 * popover API the list is placed under the control inside the card instead.
 */

/** The gap between a control and its list, and the least room kept at the window's edges. */
const GAP = 4;
const EDGE = 8;
/** The tallest a list grows before it scrolls. */
const TALLEST = 320;

export class FloatingList {
  private frame: number | null = null;
  /** The elements around the control that clip what they hold, found as the list opens. */
  private clips: Element[] = [];

  /** `find` returns the list while it is rendered, with the control it opens from; `null` while closed. */
  constructor(private readonly find: () => {list: HTMLElement; anchor: HTMLElement} | null) {}

  /** Show and place the list where it is rendered, and follow its control until it closes. Call after each render. */
  update(): void {
    const found = this.find();
    if (!found) {
      this.stop();
      return;
    }
    const {list, anchor} = found;
    if (typeof list.showPopover !== 'function') {
      below(list, anchor);
      return;
    }
    if (!list.matches(':popover-open')) {
      try {
        list.showPopover();
      } catch {
        // Shown already; the list is in the page either way.
      }
    }
    if (this.frame === null) this.clips = clipsAround(anchor);
    this.place(list, anchor);
    if (this.frame === null && typeof requestAnimationFrame !== 'undefined') {
      const tick = () => {
        const at = this.find();
        if (!at || !at.list.isConnected) {
          this.frame = null;
          return;
        }
        this.place(at.list, at.anchor);
        this.frame = requestAnimationFrame(tick);
      };
      this.frame = requestAnimationFrame(tick);
    }
  }

  /** Stop following. */
  stop(): void {
    if (this.frame !== null) cancelAnimationFrame(this.frame);
    this.frame = null;
    this.clips = [];
  }

  private place(list: HTMLElement, anchor: HTMLElement): void {
    const a = anchor.getBoundingClientRect();
    const hidden = this.clips.some((c) => {
      const r = c.getBoundingClientRect();
      return a.bottom <= r.top || a.top >= r.bottom || a.right <= r.left || a.left >= r.right;
    });
    list.style.visibility = hidden ? 'hidden' : '';
    const vh = typeof innerHeight === 'number' ? innerHeight : 768;
    const room = {below: vh - a.bottom - GAP - EDGE, above: a.top - GAP - EDGE};
    const wanted = Math.min(TALLEST, list.scrollHeight || TALLEST);
    const up = room.below < wanted && room.above > room.below;
    const most = Math.max(80, Math.min(TALLEST, up ? room.above : room.below));
    const height = Math.min(wanted, most);
    list.style.left = `${Math.round(a.left)}px`;
    list.style.width = `${Math.round(a.width)}px`;
    list.style.maxHeight = `${Math.round(most)}px`;
    list.style.top = `${Math.round(up ? a.top - GAP - height : a.bottom + GAP)}px`;
  }
}

/** Under the control, in the flow of the card that holds both, where there is no top layer. */
function below(list: HTMLElement, anchor: HTMLElement): void {
  const inside = list.parentElement === anchor;
  list.style.position = 'absolute';
  list.style.left = `${inside ? 0 : anchor.offsetLeft}px`;
  list.style.top = `${(inside ? 0 : anchor.offsetTop) + anchor.offsetHeight + GAP}px`;
  list.style.width = `${anchor.offsetWidth}px`;
  list.style.maxHeight = `${TALLEST}px`;
}

const CLIPPING = new Set(['auto', 'scroll', 'hidden', 'clip']);

/** The elements above `el`, across shadow roots, whose overflow clips what they hold. */
function clipsAround(el: Element): Element[] {
  const out: Element[] = [];
  let at: Node | null = el.parentNode;
  while (at) {
    if (at instanceof ShadowRoot) {
      at = at.host;
      continue;
    }
    if (!(at instanceof Element)) break;
    const style = getComputedStyle(at);
    if (CLIPPING.has(style.overflowY) || CLIPPING.has(style.overflowX)) out.push(at);
    at = at.parentNode;
  }
  return out;
}
