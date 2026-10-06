import {css} from 'lit';

/**
 * The theme every element shares, as a Lit `CSSResult` a host element can add to its own `styles`.
 * Every colour and font, the corner radii and the shadow are `--tessera-*` custom properties, as
 * are the map's height and the spacing of the map's corners; other spacing is fixed. The elements
 * do not declare those names: each reads a private twin that falls back to the default, so a value
 * set on the element or on any ancestor, including an enclosing `<tessera-explorer>`, wins, and the
 * default applies where nothing is set. Each colour's default is a light and a dark value, chosen
 * by the `color-scheme` the element inherits from the page.
 *
 * The interface is neutral so that colour on screen belongs to the data. The map's data palette is
 * not a token: it encodes data, and it is the map's `palette` property.
 *
 * The default font is Instrument Sans where the page has loaded it; the elements fetch no font
 * themselves, so a page that wants it links it.
 *
 * @cssprop --tessera-surface - The background of panels, cards, inputs and buttons.
 * @cssprop --tessera-surface-2 - The background of a chip, a hovered row and a chosen row.
 * @cssprop --tessera-surface-3 - The background of a pressed toggle, a hovered row button and a
 *   loading skeleton.
 * @cssprop --tessera-ink - The colour of body text, values and map tools.
 * @cssprop --tessera-ink-2 - The colour of labels, section headings and secondary text.
 * @cssprop --tessera-ink-3 - The colour of faint text: placeholders, hints and disabled controls,
 *   and of the dot in the loading states.
 * @cssprop --tessera-line - The colour of borders around panels, cards, buttons, chips and
 *   the map toolbar.
 * @cssprop --tessera-line-2 - The colour of the rules between sections and between the status
 *   strip's cells.
 * @cssprop --tessera-line-control - The colour of borders around inputs and of a slider's unfilled
 *   track, at 3:1 against the surface.
 * @cssprop --tessera-accent - The colour of the active map tool, primary buttons, badges, checked
 *   boxes and focus rings.
 * @cssprop --tessera-accent-ink - The text colour on an accent background.
 * @cssprop --tessera-highlight - The colour of a highlight chip, a highlighted row's text and its ×.
 * @cssprop --tessera-highlight-soft - The background of a highlight chip and a highlighted row.
 * @cssprop --tessera-bar - The colour of a field card's bars for the items in view.
 * @cssprop --tessera-bar-highlight - The colour of a field card's bars for the highlighted items.
 * @cssprop --tessera-bar-match - The colour of a field card's pale bars, for everything matching.
 * @cssprop --tessera-warn - The colour of the dot in the data updated and reconnecting states.
 * @cssprop --tessera-refuse - The colour of the refused and expired states and their dot.
 * @cssprop --tessera-ok - The colour of the status dot when the view is up to date.
 * @cssprop --tessera-map-bg - The map canvas's background.
 * @cssprop --tessera-radius - The corner radius of panels, cards, the status strip, the map toolbar
 *   and popovers.
 * @cssprop --tessera-radius-control - The corner radius of inputs, buttons, chips and the map
 *   tools inside the toolbar.
 * @cssprop --tessera-font - The font family of all text but identifiers. Figures use it with
 *   tabular numerals.
 * @cssprop --tessera-font-mono - The font family of identifiers.
 * @cssprop --tessera-shadow - The shadow under anything that floats over the map: the status
 *   strip, the map toolbar, the map tooltip, cards and popovers.
 * @cssprop --tessera-space - The gap between the map's corner content and the map's edges; half of
 *   it separates items in one corner.
 * @cssprop --tessera-density - A multiplier on `--tessera-space`'s default.
 * @cssprop --tessera-map-height - The map's height.
 */
export const tokens = css`
  :host {
    color-scheme: inherit;
    --_tessera-surface: var(--tessera-surface, light-dark(#ffffff, #1a1d22));
    --_tessera-surface-2: var(--tessera-surface-2, light-dark(#f3f3f0, #23272d));
    --_tessera-surface-3: var(--tessera-surface-3, light-dark(#efefeb, #2a2e35));
    --_tessera-ink: var(--tessera-ink, light-dark(#1b1d21, #eceef1));
    --_tessera-ink-2: var(--tessera-ink-2, light-dark(#565c64, #a7adb6));
    --_tessera-ink-3: var(--tessera-ink-3, light-dark(#6a6f76, #8b919a));
    --_tessera-line: var(--tessera-line, light-dark(#e5e5e1, #2b2f36));
    --_tessera-line-2: var(--tessera-line-2, light-dark(#efefeb, #24272d));
    --_tessera-line-control: var(--tessera-line-control, light-dark(#8a8f96, #6b7079));
    --_tessera-accent: var(--tessera-accent, light-dark(#1b1d21, #eceef1));
    --_tessera-accent-ink: var(--tessera-accent-ink, light-dark(#ffffff, #111317));
    --_tessera-highlight: var(--tessera-highlight, light-dark(#5b3fc4, #c3a6ee));
    --_tessera-highlight-soft: var(--tessera-highlight-soft, light-dark(#f1edfb, #2c2340));
    /* Each bar stands 3:1 against the surface and a hovered row, and a solid bar 3:1 against the
       pale one beneath. */
    --_tessera-bar: var(--tessera-bar, light-dark(#3a3e45, #d5d9df));
    --_tessera-bar-highlight: var(--tessera-bar-highlight, light-dark(#42269a, #d7c4f7));
    --_tessera-bar-match: var(--tessera-bar-match, light-dark(#868b92, #6b7079));
    --_tessera-warn: var(--tessera-warn, light-dark(#c98a0a, #e0a940));
    --_tessera-refuse: var(--tessera-refuse, light-dark(#b42318, #f0857a));
    --_tessera-ok: var(--tessera-ok, light-dark(#1f7a4d, #5cc98a));
    --_tessera-map-bg: var(--tessera-map-bg, light-dark(#f6f6f4, #111317));
    --_tessera-radius: var(--tessera-radius, 8px);
    --_tessera-radius-control: var(--tessera-radius-control, 6px);
    --_tessera-font: var(--tessera-font, 'Instrument Sans', system-ui, sans-serif);
    --_tessera-font-mono: var(--tessera-font-mono, ui-monospace, 'SF Mono', Menlo, Consolas, monospace);
    /* light-dark() chooses only colours, so each scheme's shadow is a layer transparent in the other. */
    --_tessera-shadow: var(--tessera-shadow, 0 2px 10px light-dark(rgba(0, 0, 0, 0.06), transparent), 0 4px 16px light-dark(transparent, rgba(0, 0, 0, 0.35)));
    /* A layout such as the explorer's compact form sets the private base the default scales. */
    --_tessera-space: var(--tessera-space, calc(var(--_tessera-space-base, 16px) * var(--tessera-density, 1)));
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

/**
 * The control and panel rules every element shares. The type scale is four sizes, 20 px for a
 * figure, 15 px for a title, 13 px for body text and 12 px for small text, and 11 px capitals for a
 * section heading. Actions are buttons; only a "show more" is underlined text.
 */
export const chrome = css`
  button {
    font: inherit;
    color: inherit;
    background: none;
    border: 0;
    cursor: pointer;
    padding: 0;
  }
  /* The focus ring: the accent at half strength, close to the control, so it marks the control
     without outshining the data. */
  :focus-visible {
    outline: 2px solid color-mix(in srgb, var(--_tessera-accent) 50%, transparent);
    outline-offset: 1px;
  }
  input[type='checkbox'],
  input[type='radio'] {
    accent-color: var(--_tessera-accent);
  }
  .mono {
    font-family: var(--_tessera-font-mono);
  }
  .num {
    font-variant-numeric: tabular-nums;
  }
  .muted {
    color: var(--_tessera-ink-2);
  }
  .faint {
    color: var(--_tessera-ink-3);
  }
  [data-unnamed] {
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
  /* A panel: padding, a rule beneath, a section heading. A container such as the explorer's card
     sets the two private properties for the panels inside it. */
  .panel {
    padding: var(--_tessera-panel-padding, 14px 16px);
    border-bottom: 1px solid var(--_tessera-panel-rule, var(--_tessera-line-2));
  }
  h2,
  [part='title'],
  .hd {
    margin: 0 0 10px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: 11px;
    font-weight: 600;
    line-height: 1.45;
    letter-spacing: 0.02em;
    text-transform: uppercase;
    color: var(--_tessera-ink-2);
  }
  h2 .summary,
  [part='title'] .summary,
  .hd .summary {
    text-transform: none;
    letter-spacing: 0;
    font-size: 12px;
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
    user-select: text;
  }
  [part='refusal'] {
    color: var(--_tessera-refuse);
    font-weight: 500;
  }
  /* The key/value grid of a card. A container short of room sets the private sizes. */
  .field {
    display: grid;
    grid-template-columns: var(--_tessera-key-width, 88px) minmax(0, 1fr);
    gap: var(--_tessera-field-gap, 6px 12px);
    align-items: baseline;
  }
  .field .k,
  .field .v {
    overflow-wrap: break-word;
  }
  .kv {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 4px 16px;
    align-items: baseline;
  }
  .kv .v {
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  /* A card's title is text a reader may copy, wherever the card's chrome takes no selection. */
  .card-title {
    user-select: text;
    font-size: var(--_tessera-title-size, 15px);
    font-weight: 600;
    line-height: 1.3;
    text-wrap: pretty;
    overflow-wrap: anywhere;
  }
  /* Inputs and selects. The generic rule weighs nothing, so a bare input inside a search box
     keeps the box's single border. */
  :where(.input, input:not([type='checkbox']):not([type='radio']), select) {
    height: 30px;
    border: 1px solid var(--_tessera-line-control);
    border-radius: var(--_tessera-radius-control);
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
    color: var(--_tessera-ink-2);
  }
  .input input {
    border: 0;
    background: none;
    padding: 0;
    height: 28px;
    flex: 1 1 0;
    min-width: 0;
    color: var(--_tessera-ink);
    font: inherit;
  }
  .input input:focus {
    outline: none;
  }
  .input:focus-within {
    outline: 2px solid color-mix(in srgb, var(--_tessera-accent) 50%, transparent);
    outline-offset: 1px;
  }
  /* A select drawn as text with a chevron, such as the view and colour choices. */
  .choice {
    position: relative;
    display: inline-flex;
    align-items: center;
    min-width: 0;
    max-width: 100%;
  }
  .choice select {
    appearance: none;
    field-sizing: content;
    width: auto;
    max-width: 100%;
    height: auto;
    padding: 0 18px 0 0;
    border: 0;
    border-radius: var(--_tessera-radius-control);
    background: none;
    color: inherit;
    font: inherit;
    cursor: pointer;
    text-overflow: ellipsis;
  }
  .choice svg {
    position: absolute;
    right: 0;
    pointer-events: none;
  }
  /* A segmented toggle. */
  .seg {
    display: inline-flex;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
    overflow: hidden;
    height: 28px;
  }
  .seg button {
    padding: 0 10px;
    font-size: 12px;
    color: var(--_tessera-ink-2);
  }
  .seg button + button {
    border-left: 1px solid var(--_tessera-line);
  }
  .seg button[aria-pressed='true'] {
    background: var(--_tessera-surface-2);
    color: var(--_tessera-ink);
    font-weight: 600;
  }
  /* An on-off switch: a button with role="switch" and a knob inside it. */
  .switch {
    width: 34px;
    height: 20px;
    padding: 2px;
    border-radius: 10px;
    background: color-mix(in srgb, var(--_tessera-ink-3) 35%, var(--_tessera-surface));
    display: flex;
    justify-content: flex-start;
    flex: none;
  }
  .switch[aria-checked='true'] {
    background: var(--_tessera-accent);
    justify-content: flex-end;
  }
  .switch .knob {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--_tessera-surface);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.2);
  }
  /* A checkbox row. */
  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 26px;
    cursor: pointer;
  }
  .check input {
    width: 14px;
    height: 14px;
    margin: 0;
    flex: none;
  }
  /* Buttons: bordered for an action, filled for the primary one. */
  .btn {
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    font-weight: 500;
    font-size: 12px;
    white-space: nowrap;
    background: var(--_tessera-surface);
    color: var(--_tessera-ink);
  }
  .btn.primary {
    background: var(--_tessera-accent);
    color: var(--_tessera-accent-ink);
    border-color: var(--_tessera-accent);
  }
  .btn.small {
    height: 22px;
    padding: 0 8px;
    border-radius: 5px;
  }
  .btn[disabled],
  .btn.off {
    color: var(--_tessera-ink-3);
    cursor: not-allowed;
  }
  .btn svg {
    flex: none;
  }
  /* Quiet text at the right of a section heading, such as Clear all, no taller than the heading's
     line, so the heading does not move as it comes and goes. */
  .quiet {
    padding-top: 0;
    padding-bottom: 0;
    line-height: 16px;
    font-size: 12px;
    font-weight: 500;
    letter-spacing: 0;
    text-transform: none;
    color: var(--_tessera-ink-2);
  }
  /* The one underlined kind of button: a "show more". */
  .more-link {
    align-self: flex-start;
    font-size: 12px;
    font-weight: 500;
    color: var(--_tessera-ink);
    text-decoration: underline;
    text-underline-offset: 2px;
    text-align: left;
  }
  /* A chip: neutral, wrapping its text rather than overflowing the next. */
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 24px;
    max-width: 100%;
    padding: 3px 5px 3px 8px;
    border: 1px solid var(--_tessera-line);
    border-radius: var(--_tessera-radius-control);
    background: var(--_tessera-surface-2);
    color: var(--_tessera-ink);
    font-size: 12px;
    font-weight: 500;
    line-height: 1.45;
    overflow-wrap: anywhere;
    text-align: left;
  }
  /* A chip whose clause is in the highlight position, in the highlight colour. */
  .chip[data-verb='highlight'] {
    border-color: color-mix(in srgb, var(--_tessera-highlight) 18%, var(--_tessera-highlight-soft));
    background: var(--_tessera-highlight-soft);
    color: var(--_tessera-highlight);
  }
  /* The word on a highlight chip that says where its clause is. */
  .chip .verb {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    height: 18px;
    padding: 0 5px;
    margin-left: -3px;
    border-radius: 4px;
    background: color-mix(in srgb, currentColor 10%, transparent);
    font-size: 11px;
    text-transform: lowercase;
    flex: none;
  }
  .chip button {
    display: inline-flex;
    flex: none;
    color: var(--_tessera-ink-2);
  }
  .chip[data-verb='highlight'] button {
    color: inherit;
  }
  /* A list of rows with a count at the right. A row keeps its height and the list scrolls. */
  .list {
    display: flex;
    flex-direction: column;
  }
  .list > * {
    flex: none;
  }
  .list > .item {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 30px;
    padding: 0 6px 0 calc(6px + var(--depth, 0) * 16px);
    border-radius: var(--_tessera-radius-control);
    cursor: pointer;
  }
  .list > .item:hover {
    background: var(--_tessera-surface-2);
  }
  .list > .item[aria-selected='true'],
  .list > .item.on {
    background: var(--_tessera-surface-2);
    font-weight: 500;
  }
  .list > .item .n {
    margin-left: auto;
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
    --depth: 1;
  }
  /* The state region: one line. */
  [part='state'] {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--_tessera-ink-2);
    font-size: 12px;
  }
  [part='state'] svg {
    flex: none;
  }
  [part='state']:empty {
    display: none;
  }
  [part='state'][data-state='refused'],
  [part='state'][data-state='expired'] {
    color: var(--_tessera-refuse);
    font-weight: 500;
  }
  .skel,
  .skeleton {
    display: inline-block;
    height: 9px;
    width: 44px;
    border-radius: 4px;
    background: var(--_tessera-surface-3);
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--_tessera-ok);
    flex: none;
  }
  .dot.quiet {
    background: var(--_tessera-ink-3);
  }
  .dot.warn {
    background: var(--_tessera-warn);
  }
  .dot.refuse {
    background: var(--_tessera-refuse);
  }
`;
