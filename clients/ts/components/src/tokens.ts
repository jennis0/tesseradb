import {css} from 'lit';

/**
 * The tokens (design §5.6 rung 2): every colour, font, spacing and radius is a `--tessera-*`
 * custom property with a neutral default that follows `color-scheme`, so light and dark come
 * from the host page unconfigured. A host restyles by setting the property on any ancestor.
 *
 * The map's data palette is deliberately not here — it encodes data, and brand colours are the
 * wrong thing to encode data in; it is the map's `palette` property.
 */
export const tokens = css`
  :host {
    color-scheme: light dark;
    --tessera-font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
    --tessera-font-size-small: 11px;
    --tessera-fg: light-dark(#1c2128, #dfe3e8);
    --tessera-fg-strong: light-dark(#0b0e12, #eaeef3);
    --tessera-fg-muted: light-dark(#5b6675, #7d8794);
    --tessera-bg: light-dark(#ffffff, #0d0f12);
    --tessera-panel-bg: light-dark(rgba(255, 255, 255, 0.94), rgba(13, 15, 18, 0.92));
    --tessera-control-bg: light-dark(#f3f5f8, #171a1f);
    --tessera-border: light-dark(#d5dae2, #2a2f36);
    --tessera-border-strong: light-dark(#b9c0cb, #3d454f);
    --tessera-accent: light-dark(#1f6fd0, #8fd3ff);
    --tessera-bad: light-dark(#b42318, #ff8f7a);
    --tessera-warn: light-dark(#8a5a00, #ffd25a);
    --tessera-radius: 4px;
    --tessera-density: 1;
    --tessera-space: calc(6px * var(--tessera-density));
    --tessera-map-height: 480px;
    --tessera-map-bg: light-dark(#f6f7f9, #0d0f12);
    font: var(--tessera-font);
    color: var(--tessera-fg);
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

/** The shared control and panel rules, so every element's markup reads the same. */
export const chrome = css`
  h2,
  [part='title'] {
    margin: 0 0 var(--tessera-space);
    font-size: var(--tessera-font-size-small);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--tessera-fg-muted);
    font-weight: 600;
  }
  .row {
    display: flex;
    justify-content: space-between;
    gap: calc(var(--tessera-space) * 1.6);
  }
  .muted,
  [part='label'] {
    color: var(--tessera-fg-muted);
  }
  [part='value'] {
    color: var(--tessera-fg-strong);
  }
  [part='refusal'] {
    color: var(--tessera-bad);
  }
  select,
  input,
  button {
    background: var(--tessera-control-bg);
    color: var(--tessera-fg);
    border: 1px solid var(--tessera-border);
    border-radius: var(--tessera-radius);
    padding: calc(var(--tessera-space) * 0.66);
    font: inherit;
  }
  button {
    cursor: pointer;
  }
  button:hover:not([disabled]) {
    border-color: var(--tessera-border-strong);
  }
  button[disabled] {
    cursor: not-allowed;
    opacity: 0.55;
  }
  [part='state'] {
    display: inline-flex;
    align-items: center;
    gap: var(--tessera-space);
    color: var(--tessera-fg-muted);
  }
  [part='state'][data-state='refused'],
  [part='state'][data-state='expired'] {
    color: var(--tessera-bad);
  }
  [part='state'][data-state='stale'],
  [part='state'][data-state='retrying'] {
    color: var(--tessera-warn);
  }
  [part='state'] .badge {
    font-size: var(--tessera-font-size-small);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    border: 1px solid currentColor;
    border-radius: var(--tessera-radius);
    padding: 0 calc(var(--tessera-space) * 0.8);
  }
  .skeleton {
    display: inline-block;
    width: 8ch;
    height: 1em;
    border-radius: var(--tessera-radius);
    background: var(--tessera-border);
    opacity: 0.6;
  }
`;
