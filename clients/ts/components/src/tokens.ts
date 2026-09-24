import {css} from 'lit';

/**
 * The theme: every colour, font, spacing and radius is a public `--tessera-*` custom property. The
 * elements never declare the public names. Each element's host declares a private
 * `--_tessera-*` twin as `var(--tessera-x, <default>)` and its styles read only the twin, so a
 * value set on the element or on any ancestor, including an enclosing `<tessera-explorer>`, wins,
 * and the default applies only where nothing is set. Each colour default is a `light-dark()` pair
 * resolved against the `color-scheme` the element inherits from the page.
 *
 * The map's data palette is not a token: it encodes data, and it is the map's `palette` property.
 */
export const tokens = css`
  :host {
    color-scheme: inherit;
    --_tessera-surface: var(--tessera-surface, light-dark(#fbfbfa, #151719));
    --_tessera-surface-2: var(--tessera-surface-2, light-dark(#f2f2ef, #1d2024));
    --_tessera-surface-3: var(--tessera-surface-3, light-dark(#e7e7e2, #272b30));
    --_tessera-ink: var(--tessera-ink, light-dark(#1c1f23, #e8eaec));
    --_tessera-ink-2: var(--tessera-ink-2, light-dark(#555b63, #aab0b7));
    --_tessera-ink-3: var(--tessera-ink-3, light-dark(#6f757d, #868d95));
    --_tessera-line: var(--tessera-line, light-dark(#d6d6d0, #30353b));
    --_tessera-line-2: var(--tessera-line-2, light-dark(#e6e6e1, #262a2f));
    --_tessera-accent: var(--tessera-accent, light-dark(#2457a3, #86b0f0));
    --_tessera-accent-ink: var(--tessera-accent-ink, light-dark(#ffffff, #0d1a2e));
    --_tessera-accent-soft: var(--tessera-accent-soft, light-dark(#e4ecf8, #1f2d42));
    /* The highlight's own accent, distinct from the filter's blue and from warn's amber, so a
       chip reads as filter or highlight at a glance. */
    --_tessera-highlight: var(--tessera-highlight, light-dark(#6b3fa0, #c3a6ee));
    --_tessera-highlight-ink: var(--tessera-highlight-ink, light-dark(#ffffff, #1b1430));
    --_tessera-highlight-soft: var(--tessera-highlight-soft, light-dark(#efe6fa, #2c2340));
    --_tessera-warn: var(--tessera-warn, light-dark(#7a5600, #e6b84a));
    --_tessera-warn-soft: var(--tessera-warn-soft, light-dark(#fff1cf, #3a2e0e));
    --_tessera-refuse: var(--tessera-refuse, light-dark(#a12b2b, #f29a9a));
    --_tessera-refuse-soft: var(--tessera-refuse-soft, light-dark(#fbe5e5, #3e1c1c));
    --_tessera-ok: var(--tessera-ok, light-dark(#226b44, #6cc38e));
    --_tessera-map-bg: var(--tessera-map-bg, light-dark(#f7f7f4, #0c0e11));
    --_tessera-radius: var(--tessera-radius, 4px);
    --_tessera-font: var(--tessera-font, 'IBM Plex Sans', system-ui, sans-serif);
    --_tessera-font-mono: var(--tessera-font-mono, 'IBM Plex Mono', ui-monospace, monospace);
    --_tessera-shadow: var(
      --tessera-shadow,
      light-dark(0 1px 2px rgba(20, 22, 25, 0.08), 0 1px 2px rgba(0, 0, 0, 0.4)),
      light-dark(0 4px 16px rgba(20, 22, 25, 0.08), 0 6px 20px rgba(0, 0, 0, 0.45))
    );
    --_tessera-space: var(--tessera-space, calc(8px * var(--tessera-density, 1)));
    --_tessera-map-height: var(--tessera-map-height, 420px);
    font-family: var(--_tessera-font);
    font-size: 13px;
    line-height: 1.45;
    color: var(--_tessera-ink);
    -webkit-font-smoothing: antialiased;
    box-sizing: border-box;
  }
  :host *,
  :host *::before,
  :host *::after {
    box-sizing: inherit;
  }
  @media (prefers-reduced-motion: reduce) {
    :host * {
      transition: none !important;
      animation: none !important;
    }
  }
`;

/** The shared control and panel rules — the boards' component CSS, so every element reads the same. */
export const chrome = css`
  button {
    font: inherit;
    color: inherit;
    background: none;
    border: 0;
    cursor: pointer;
    padding: 0;
  }
  :focus-visible {
    outline: 2px solid var(--_tessera-accent);
    outline-offset: 2px;
  }
  .mono,
  .num {
    font-family: var(--_tessera-font-mono);
    font-variant-numeric: tabular-nums;
  }
  .muted {
    color: var(--_tessera-ink-2);
  }
  .faint {
    color: var(--_tessera-ink-3);
  }
  .sm {
    font-size: 12px;
  }
  .xs {
    font-size: 11px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .col {
    display: flex;
    flex-direction: column;
  }
  .grow {
    flex-grow: 1;
    min-width: 0;
  }
  /* A panel, as the boards draw one: padding, a rule beneath, an uppercase heading. */
  .panel {
    padding: 14px 16px;
    border-bottom: 1px solid var(--_tessera-line-2);
  }
  h2,
  [part='title'],
  .hd {
    margin: 0 0 10px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--_tessera-ink-2);
  }
  h2 .summary,
  [part='title'] .summary,
  .hd .summary {
    text-transform: none;
    letter-spacing: 0;
    font-weight: 400;
    color: var(--_tessera-ink-3);
  }
  [part='label'],
  .k {
    color: var(--_tessera-ink-2);
  }
  [part='value'],
  .v {
    color: var(--_tessera-ink);
  }
  [part='refusal'] {
    color: var(--_tessera-refuse);
  }
  /* The key/value grid of a card. */
  .field {
    display: grid;
    grid-template-columns: 112px 1fr;
    gap: 6px 12px;
    align-items: baseline;
  }
  .field .k {
    font-size: 12px;
  }
  .kv {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 4px 16px;
    align-items: baseline;
  }
  .kv .v {
    font-family: var(--_tessera-font-mono);
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  .card-title {
    font-size: 14.5px;
    font-weight: 600;
    line-height: 1.35;
    text-wrap: pretty;
  }
  /* Inputs and selects. */
  .input,
  input:not([type='checkbox']):not([type='radio']),
  select {
    height: 30px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius);
    background: var(--_tessera-surface);
    color: var(--_tessera-ink);
    padding: 0 10px;
    font: inherit;
    font-size: 13px;
  }
  select {
    padding: 0 8px 0 10px;
    width: 100%;
  }
  input::placeholder {
    color: var(--_tessera-ink-3);
  }
  .input {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .input input {
    border: 0;
    background: none;
    padding: 0;
    height: 28px;
    flex: 1 1 0;
    min-width: 0;
  }
  .input input:focus {
    outline: none;
  }
  .input:focus-within {
    outline: 2px solid var(--_tessera-accent);
    outline-offset: 1px;
  }
  /* A segmented toggle. */
  .seg {
    display: inline-flex;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius);
    overflow: hidden;
    height: 28px;
  }
  .seg button {
    padding: 0 10px;
    font-size: 12px;
    color: var(--_tessera-ink-2);
  }
  .seg button[aria-pressed='true'] {
    background: var(--_tessera-surface-3);
    color: var(--_tessera-ink);
    font-weight: 600;
  }
  /* A checkbox row. */
  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    height: 26px;
    cursor: pointer;
  }
  .check input {
    width: 15px;
    height: 15px;
    margin: 0;
    accent-color: var(--_tessera-accent);
  }
  /* Buttons. */
  .btn {
    height: 30px;
    padding: 0 12px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius);
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-weight: 500;
    font-size: 12.5px;
    background: var(--_tessera-surface);
  }
  .btn.primary {
    background: var(--_tessera-accent);
    color: var(--_tessera-accent-ink);
    border-color: var(--_tessera-accent);
  }
  .btn.quiet {
    border: 0;
    color: var(--_tessera-ink-2);
  }
  .btn[disabled],
  .btn.off {
    color: var(--_tessera-ink-3);
    border-style: dashed;
    cursor: not-allowed;
  }
  .btn svg {
    flex: none;
  }
  /* A chip. */
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 24px;
    padding: 0 6px 0 8px;
    border-radius: 3px;
    background: var(--_tessera-accent-soft);
    color: var(--_tessera-accent);
    font-size: 12px;
    font-weight: 500;
  }
  /* A chip whose clause is in the highlight position — the same chip in the other colour, so the
     position reads before the words do. */
  .chip[data-verb='highlight'] {
    background: var(--_tessera-highlight-soft);
    color: var(--_tessera-highlight);
  }
  /* The verb toggle on a chip: the word for where the clause is, clicked to move it. */
  .chip .verb {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    height: 18px;
    padding: 0 5px;
    margin-left: -3px;
    border-radius: 2px;
    background: color-mix(in srgb, currentColor 14%, transparent);
    font-size: 11px;
    letter-spacing: 0.02em;
    text-transform: lowercase;
  }
  .chip button {
    display: inline-flex;
    opacity: 0.8;
    color: inherit;
  }
  /* A list of rows with a count at the right. */
  .list {
    display: flex;
    flex-direction: column;
  }
  .list > .item {
    display: flex;
    align-items: center;
    gap: 8px;
    height: 30px;
    padding: 0 6px;
    border-radius: 3px;
    cursor: pointer;
  }
  .list > .item[aria-selected='true'],
  .list > .item.on {
    background: var(--_tessera-accent-soft);
  }
  .list > .item .n {
    margin-left: auto;
    font-family: var(--_tessera-font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--_tessera-ink-2);
    font-size: 12px;
  }
  .list > .item .name {
    flex: 1 1 0;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .list > .item.child {
    padding-left: 24px;
  }
  /* The state region: an answer in one line, never a paragraph. */
  [part='state'] {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--_tessera-ink-2);
    font-size: 12px;
  }
  [part='state']:empty {
    display: none;
  }
  [part='state'][data-state='refused'],
  [part='state'][data-state='expired'] {
    color: var(--_tessera-refuse);
  }
  [part='state'][data-state='stale'],
  [part='state'][data-state='retrying'] {
    color: var(--_tessera-warn);
  }
  .skel,
  .skeleton {
    display: inline-block;
    height: 10px;
    width: 44px;
    border-radius: 2px;
    background: var(--_tessera-surface-3);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--_tessera-ok);
    flex: none;
  }
`;
