import {css} from 'lit';
import type {DensityColours} from '@tesseradb/deck';
import type {TesseraEventDetails} from './events.js';
import {densityStops} from '@tesseradb/deck/internal';

/**
 * What the display sections of the explorer's Layers popover share with the elements that render
 * choices like them: the settings they hold, the arrow keys of a radio group, and their styles.
 */

/** Every display setting, as the explorer holds it and `tessera-displaychange` reports it. */
export type DisplaySettings = TesseraEventDetails['tessera-displaychange'];

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

/** The display sections' styles, for the element that renders them. */
export const displayStyles = css`
  .sec {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 12px 14px;
  }
  .sec + .sec {
    border-top: 1px solid var(--_tessera-line-2);
  }
  .sec .hd {
    margin: 0;
  }
  .sec-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .switch.small {
    width: 30px;
    height: 18px;
    border-radius: 9px;
  }
  .switch.small .knob {
    width: 14px;
    height: 14px;
  }
  .most {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .most-top {
    display: flex;
    justify-content: space-between;
  }
  .most-top label {
    font-weight: 500;
  }
  .most-value {
    font-size: 12px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .ticks {
    position: relative;
    height: 16px;
    font-size: 12px;
    color: var(--_tessera-ink-3);
    font-variant-numeric: tabular-nums;
  }
  .ticks span {
    position: absolute;
  }
  .ticks .mid {
    transform: translateX(-50%);
  }
  .sliders {
    display: grid;
    grid-template-columns: 72px minmax(0, 1fr);
    align-items: center;
    gap: 10px;
  }
  .sliders > span,
  .sliders > label {
    font-weight: 500;
  }
  /* A slider: a thin track filled to the thumb, as far as its --fill says. */
  .slider {
    -webkit-appearance: none;
    appearance: none;
    width: 100%;
    height: 16px;
    margin: 0;
    padding: 0;
    border: 0;
    background: none;
    cursor: pointer;
  }
  .slider::-webkit-slider-runnable-track {
    height: 4px;
    border-radius: 2px;
    background: linear-gradient(to right, var(--_tessera-accent) var(--fill, 0%), var(--_tessera-line-control) var(--fill, 0%));
  }
  .slider::-moz-range-track {
    height: 4px;
    border-radius: 2px;
    background: linear-gradient(to right, var(--_tessera-accent) var(--fill, 0%), var(--_tessera-line-control) var(--fill, 0%));
  }
  .slider::-webkit-slider-thumb {
    -webkit-appearance: none;
    width: 14px;
    height: 14px;
    margin-top: -5px;
    box-sizing: border-box;
    border-radius: 50%;
    border: 1.5px solid var(--_tessera-accent);
    background: var(--_tessera-surface);
  }
  .slider::-moz-range-thumb {
    width: 14px;
    height: 14px;
    box-sizing: border-box;
    border-radius: 50%;
    border: 1.5px solid var(--_tessera-accent);
    background: var(--_tessera-surface);
  }
  .slider:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .modes {
    display: flex;
    gap: 2px;
    padding: 2px;
    border-radius: 7px;
    background: var(--_tessera-surface-3);
  }
  .modes button {
    flex: 1 1 0;
    height: 24px;
    border-radius: 5px;
    font-size: 12px;
    font-weight: 500;
    color: var(--_tessera-ink-2);
  }
  .modes button[aria-checked='true'] {
    background: var(--_tessera-surface);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
    color: var(--_tessera-ink);
    font-weight: 600;
  }
  /* The readout column the other sliders keep on their right is kept here too, so the tracks end
     together; this slider's readout is the line under it. */
  .resolution,
  .resolution + span + .ends {
    margin-right: 48px;
  }
  .resolution {
    position: relative;
    display: flex;
    align-items: center;
  }
  .resolution input[type='range'] {
    flex: 1 1 auto;
    min-width: 0;
  }
  /* The stops the server will not count for this view, struck through on the track. */
  .resolution .past {
    position: absolute;
    right: 8px;
    top: 50%;
    height: 6px;
    margin-top: -3px;
    border-radius: 3px;
    background: repeating-linear-gradient(135deg, var(--_tessera-ink-3) 0 1.5px, transparent 1.5px 4px) var(--_tessera-surface);
    opacity: 0.8;
    pointer-events: none;
  }
  .ends {
    display: flex;
    justify-content: space-between;
    margin-top: -2px;
    font-size: 11px;
    color: var(--_tessera-ink-3);
  }
  .ends .readout {
    color: var(--_tessera-ink);
    font-weight: 500;
  }
  .ramp-choice {
    display: flex;
    align-items: center;
    justify-self: start;
    gap: 4px;
    min-width: 0;
    max-width: 100%;
    padding: 3px 8px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
    background: var(--_tessera-surface);
    font-size: 12px;
    font-weight: 500;
    color: var(--_tessera-ink);
  }
  .ramp-choice.stretch {
    justify-self: stretch;
    gap: 8px;
    padding: 4px 8px;
  }
  .ramp-choice.stretch .t {
    flex: 1 1 auto;
    text-align: left;
  }
  .ramp-choice svg {
    flex: none;
  }
  .strip {
    display: flex;
    flex: none;
    gap: 2px;
  }
  .strip span {
    width: 8px;
    height: 10px;
    border-radius: 1px;
  }
  .bar.wide {
    width: 90px;
    height: 10px;
  }
  .scale-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .toggle {
    padding: 3px 8px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
    background: var(--_tessera-surface);
    font-size: 12px;
    font-weight: 500;
  }
  .toggle[aria-pressed='true'] {
    border-color: var(--_tessera-accent);
    background: var(--_tessera-accent);
    color: var(--_tessera-accent-ink);
  }
  .ramp-choice .t {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ramp-choice:disabled {
    opacity: 0.4;
  }
  /* A slider with its value beside it, and the two sliders of a size range either side of theirs. */
  .with-readout,
  .sliders .range {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .with-readout input[type='range'] {
    flex: 1 1 auto;
    min-width: 0;
  }
  .sliders .range input[type='range'] {
    flex: 1 1 0;
    min-width: 0;
  }
  .readout {
    flex: none;
    color: var(--_tessera-ink);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .with-readout .readout {
    min-width: 40px;
    text-align: right;
  }
  .seg {
    display: inline-flex;
    justify-self: start;
    gap: 2px;
    height: auto;
    padding: 2px;
    border: 0;
    border-radius: 7px;
    background: var(--_tessera-surface-3);
  }
  .seg button {
    height: 24px;
    padding: 0 10px;
    border-radius: 5px;
    font-size: 12px;
    font-weight: 500;
    color: var(--_tessera-ink-2);
  }
  .seg button + button {
    border-left: 0;
  }
  .seg button[aria-checked='true'] {
    background: var(--_tessera-surface);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
    color: var(--_tessera-ink);
    font-weight: 600;
  }
  /* The Size by menu, in the top layer beside the popover. */
  .size-menu {
    position: fixed;
    inset: auto;
    margin: 0;
    width: 220px;
    padding: 6px 0;
    box-sizing: border-box;
    background: var(--_tessera-surface);
    color: var(--_tessera-ink);
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius);
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.1);
    font-size: 13px;
  }
  .size-menu .hd {
    margin: 0;
    padding: 6px 14px 4px;
  }
  .size-menu [role='menuitemradio'] {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    padding: 7px 14px;
    text-align: left;
  }
  .size-menu [role='menuitemradio']:hover,
  .size-menu [role='menuitemradio'][aria-checked='true'] {
    background: var(--_tessera-surface-2);
  }
  .size-menu [role='menuitemradio'][aria-checked='true'] {
    font-weight: 500;
  }
  .size-menu [role='menuitemradio']:focus-visible {
    outline-offset: -2px;
  }
  .size-menu.wide {
    width: 260px;
  }
  .size-menu .lead {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .size-menu .kind {
    font-size: 12px;
    font-weight: 400;
    color: var(--_tessera-ink-3);
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
