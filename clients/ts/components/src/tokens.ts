import {css} from 'lit';

/**
 * The tokens (design §5.6 rung 2): every colour, font, spacing and radius is a `--tessera-*`
 * custom property with a neutral default, the values of the design boards
 * (`docs/evidence/mockups/client-components/gen.py`, `TOKENS_LIGHT` / `TOKENS_DARK`). Each is a
 * `light-dark()` pair, and the host page's `color-scheme` chooses — the elements inherit it
 * rather than declaring their own, so a page that says nothing is light and a page that says
 * `dark` is dark, unconfigured either way. A host restyles by setting a property on any ancestor.
 *
 * The map's data palette is deliberately not here — it encodes data, and brand colours are the
 * wrong thing to encode data in; it is the map's `palette` property.
 */
export const tokens = css`
  :host {
    color-scheme: inherit;
    --tessera-surface: light-dark(#fbfbfa, #151719);
    --tessera-surface-2: light-dark(#f2f2ef, #1d2024);
    --tessera-surface-3: light-dark(#e7e7e2, #272b30);
    --tessera-ink: light-dark(#1c1f23, #e8eaec);
    --tessera-ink-2: light-dark(#555b63, #aab0b7);
    --tessera-ink-3: light-dark(#6f757d, #868d95);
    --tessera-line: light-dark(#d6d6d0, #30353b);
    --tessera-line-2: light-dark(#e6e6e1, #262a2f);
    --tessera-accent: light-dark(#2457a3, #86b0f0);
    --tessera-accent-ink: light-dark(#ffffff, #0d1a2e);
    --tessera-accent-soft: light-dark(#e4ecf8, #1f2d42);
    /* The highlight's own accent, distinct from the filter's blue and from warn's amber: the two
       verbs sit side by side on one chip and a viewer has to read which is on at a glance
       (highlight-and-hierarchy §5.2). */
    --tessera-highlight: light-dark(#6b3fa0, #c3a6ee);
    --tessera-highlight-ink: light-dark(#ffffff, #1b1430);
    --tessera-highlight-soft: light-dark(#efe6fa, #2c2340);
    --tessera-warn: light-dark(#7a5600, #e6b84a);
    --tessera-warn-soft: light-dark(#fff1cf, #3a2e0e);
    --tessera-refuse: light-dark(#a12b2b, #f29a9a);
    --tessera-refuse-soft: light-dark(#fbe5e5, #3e1c1c);
    --tessera-ok: light-dark(#226b44, #6cc38e);
    --tessera-map-bg: light-dark(#f7f7f4, #0c0e11);
    --tessera-radius: 4px;
    --tessera-font: 'IBM Plex Sans', system-ui, sans-serif;
    --tessera-font-mono: 'IBM Plex Mono', ui-monospace, monospace;
    --tessera-shadow: light-dark(
      0 1px 2px rgba(20, 22, 25, 0.08),
      0 1px 2px rgba(0, 0, 0, 0.4)
    ),
    light-dark(0 4px 16px rgba(20, 22, 25, 0.08), 0 6px 20px rgba(0, 0, 0, 0.45));
    --tessera-density: 1;
    --tessera-space: calc(8px * var(--tessera-density));
    --tessera-map-height: 420px;
    font-family: var(--tessera-font);
    font-size: 13px;
    line-height: 1.45;
    color: var(--tessera-ink);
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
    outline: 2px solid var(--tessera-accent);
    outline-offset: 2px;
  }
  .mono,
  .num {
    font-family: var(--tessera-font-mono);
    font-variant-numeric: tabular-nums;
  }
  .muted {
    color: var(--tessera-ink-2);
  }
  .faint {
    color: var(--tessera-ink-3);
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
    border-bottom: 1px solid var(--tessera-line-2);
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
    color: var(--tessera-ink-2);
  }
  h2 .summary,
  [part='title'] .summary,
  .hd .summary {
    text-transform: none;
    letter-spacing: 0;
    font-weight: 400;
    color: var(--tessera-ink-3);
  }
  [part='label'],
  .k {
    color: var(--tessera-ink-2);
  }
  [part='value'],
  .v {
    color: var(--tessera-ink);
  }
  [part='refusal'] {
    color: var(--tessera-refuse);
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
    font-family: var(--tessera-font-mono);
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
    border: 1px solid var(--tessera-line);
    border-radius: var(--tessera-radius);
    background: var(--tessera-surface);
    color: var(--tessera-ink);
    padding: 0 10px;
    font: inherit;
    font-size: 13px;
  }
  select {
    padding: 0 8px 0 10px;
    width: 100%;
  }
  input::placeholder {
    color: var(--tessera-ink-3);
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
    outline: 2px solid var(--tessera-accent);
    outline-offset: 1px;
  }
  /* A segmented toggle. */
  .seg {
    display: inline-flex;
    border: 1px solid var(--tessera-line);
    border-radius: var(--tessera-radius);
    overflow: hidden;
    height: 28px;
  }
  .seg button {
    padding: 0 10px;
    font-size: 12px;
    color: var(--tessera-ink-2);
  }
  .seg button[aria-pressed='true'] {
    background: var(--tessera-surface-3);
    color: var(--tessera-ink);
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
    accent-color: var(--tessera-accent);
  }
  /* Buttons. */
  .btn {
    height: 30px;
    padding: 0 12px;
    border: 1px solid var(--tessera-line);
    border-radius: var(--tessera-radius);
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-weight: 500;
    font-size: 12.5px;
    background: var(--tessera-surface);
  }
  .btn.primary {
    background: var(--tessera-accent);
    color: var(--tessera-accent-ink);
    border-color: var(--tessera-accent);
  }
  .btn.quiet {
    border: 0;
    color: var(--tessera-ink-2);
  }
  .btn[disabled],
  .btn.off {
    color: var(--tessera-ink-3);
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
    background: var(--tessera-accent-soft);
    color: var(--tessera-accent);
    font-size: 12px;
    font-weight: 500;
  }
  /* A chip whose clause is in the highlight position — the same chip in the other colour, so the
     position reads before the words do. */
  .chip[data-verb='highlight'] {
    background: var(--tessera-highlight-soft);
    color: var(--tessera-highlight);
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
    background: var(--tessera-accent-soft);
  }
  .list > .item .n {
    margin-left: auto;
    font-family: var(--tessera-font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--tessera-ink-2);
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
    color: var(--tessera-ink-2);
    font-size: 12px;
  }
  [part='state']:empty {
    display: none;
  }
  [part='state'][data-state='refused'],
  [part='state'][data-state='expired'] {
    color: var(--tessera-refuse);
  }
  [part='state'][data-state='stale'],
  [part='state'][data-state='retrying'] {
    color: var(--tessera-warn);
  }
  .skel,
  .skeleton {
    display: inline-block;
    height: 10px;
    width: 44px;
    border-radius: 2px;
    background: var(--tessera-surface-3);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--tessera-ok);
    flex: none;
  }
`;
