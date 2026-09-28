import {css} from 'lit';
import type {DensityColours, DensityMode} from '@tesseradb/deck';
import {densityStops} from '@tesseradb/deck/internal';

/**
 * What the Display section of the explorer's Layers popover shares with the elements that render
 * choices like it: the settings it holds, the arrow keys of a radio group, and its styles.
 */

/** Every display setting, as the explorer holds it and `tessera-displaychange` reports it. */
export type DisplaySettings = {
  points: boolean;
  radius: number | null;
  pointOpacity: number | null;
  density: DensityMode;
  densityColours: DensityColours | null;
  densityStrength: number;
};

/** A density ramp as a CSS gradient, sparse to dense, on `scheme`'s ground. */
export function densityGradient(colours: DensityColours, scheme: 'light' | 'dark'): string {
  return `linear-gradient(to right, ${densityStops(colours, scheme)
    .map(([r, g, b]) => `rgb(${r}, ${g}, ${b})`)
    .join(', ')})`;
}

/**
 * The arrow keys in a radio group move the choice, wrapping, and Home and End go to the ends, as a
 * native radio group does. `choose` is called with the index chosen; focus follows it.
 */
export function radioKeys(e: KeyboardEvent, count: number, at: number, choose: (i: number) => void): void {
  const next = {ArrowRight: (at + 1) % count, ArrowDown: (at + 1) % count, ArrowLeft: (at - 1 + count) % count, ArrowUp: (at - 1 + count) % count, Home: 0, End: count - 1}[e.key];
  if (next === undefined) return;
  e.preventDefault();
  choose(next);
  const group = (e.currentTarget as HTMLElement).closest('[role="radiogroup"]');
  void Promise.resolve().then(() => group?.querySelectorAll<HTMLElement>('[role="radio"]')[next]?.focus());
}

/** The Display section's styles, for the element that renders it. */
export const displayStyles = css`
  .display {
    padding: 12px 14px 4px;
  }
  .display .hd {
    margin-bottom: 4px;
  }
  .display .line {
    display: flex;
    align-items: center;
    justify-content: space-between;
    min-height: 32px;
  }
  .display .lead {
    font-weight: 500;
  }
  .display .rule {
    height: 1px;
    margin: 4px -14px 8px;
    background: var(--_tessera-line-2);
  }
  .display .gap {
    height: 8px;
  }
  .sliders {
    display: grid;
    grid-template-columns: 64px minmax(0, 1fr);
    align-items: center;
    gap: 6px 0;
    padding: 2px 0 10px;
    font-size: 12px;
    color: var(--_tessera-ink-2);
  }
  .sliders input[type='range'] {
    margin: 0;
    height: auto;
    padding: 0;
    border: 0;
    background: none;
    accent-color: var(--_tessera-accent);
  }
  .sliders input[type='range']:disabled {
    opacity: 0.4;
  }
  .modes {
    display: grid;
    grid-template-columns: repeat(5, minmax(0, 1fr));
    margin: 4px 0 10px;
    padding: 3px;
    border-radius: var(--_tessera-radius);
    background: var(--_tessera-surface-3);
  }
  .modes button {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 3px;
    padding: 6px 0 5px;
    border-radius: var(--_tessera-radius-control);
    font-size: 11px;
    color: var(--_tessera-ink-2);
  }
  .modes button[aria-checked='true'] {
    background: var(--_tessera-surface);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.1);
    color: var(--_tessera-ink);
    font-weight: 500;
  }
  .ramp-choice {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    padding: 4px 8px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
    background: var(--_tessera-surface);
    font-size: 12px;
    color: var(--_tessera-ink);
  }
  .ramp-choice svg {
    margin-left: auto;
  }
  .bar {
    display: block;
    flex: none;
    width: 64px;
    height: 8px;
    border-radius: 2px;
  }
  .ramp-list {
    grid-column: 2;
    display: flex;
    flex-direction: column;
    padding: 3px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
  }
  .ramp-list button {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 6px;
    border-radius: 4px;
    font-size: 12px;
    color: var(--_tessera-ink);
  }
  .ramp-list button:hover,
  .ramp-list button[aria-checked='true'] {
    background: var(--_tessera-surface-2);
  }
`;
