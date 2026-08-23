import {
  UNMAPPED,
  colourOfFraction,
  colourOfRank,
  css,
  paletteValues
} from '../colour.js';
import {esc, panel, row} from '../html.js';
import type {AppState} from '../state.js';

/**
 * The colour-by control and its legend.
 *
 * **The legend is built from the values actually resolved, not from the vocabulary.** A legend
 * enumerating every value a bundle declares would name values that exist only in items this
 * principal cannot see — C11's shape, reached through the UI rather than through the API. The
 * viewer asks `/v1/categories` for the codes it *drew*, so what appears here is exactly what is on
 * the map.
 *
 * **A numeric legend must state where its domain came from.** It is derived from the marks served,
 * so it moves as the viewport moves; a ramp presented without that caveat reads as a corpus-wide
 * scale, which is the one thing it must never be mistaken for.
 */
export function renderLegend(state: AppState): string {
  // **Only the rendered columns**, because only they arrive. A `render: false` column lives in
  // entity space or the record blob and no viewport response carries a value for it, so offering it
  // here would offer an encoding whose every mark is unmapped — the `title` and `abstract` columns
  // are filterable and unpaintable, and the two lists differ for exactly that reason.
  const columns = (state.meta?.declaredScalars ?? []).filter((c) => c.render);
  if (columns.length === 0) {
    const declared = state.meta?.declaredScalars.length ?? 0;
    return panel(
      'Colour',
      declared === 0
        ? '<div class="muted">this bundle declares no per-item columns</div>'
        : `<div class="muted">none of this bundle's ${declared} columns is rendered, so no value
           reaches a mark to be coloured by</div>`
    );
  }

  const options = [
    `<option value=""${state.colourBy === null ? ' selected' : ''}>uniform</option>`,
    ...columns.map((c) => {
      const kind = c.category ? 'category' : c.arrowType;
      const selected = state.colourBy === c.name ? ' selected' : '';
      return `<option value="${esc(c.name)}"${selected}>${esc(c.name)} · ${esc(kind)}</option>`;
    })
  ].join('');
  const select = `<select id="colour-by">${options}</select>`;

  if (state.colourBy === null) {
    return panel(
      'Colour',
      `${select}<div class="muted">every mark one colour — pick a column to encode it. No request
        is issued either way.</div>`
    );
  }

  const column = columns.find((c) => c.name === state.colourBy);
  if (!column) return panel('Colour', `${select}<div class="bad">no such column</div>`);

  const error = state.categoryErrors[column.name];
  if (error) {
    // A refusal is shown as a refusal. `derived` is the expected one today: the gate is
    // specified and unbuilt, so the server declines rather than publishing an ungated set.
    return panel(
      'Colour',
      `${select}
       <div class="bad">${esc(error.code)}: ${esc(error.detail)}</div>
       <div class="muted">drawn unmapped — every served mark is still on the map, only its value is
         unnamed.</div>`
    );
  }

  return panel('Colour', select + (column.category ? categoryBody(state, column.name) : rampBody(state, column.name)));
}

function categoryBody(state: AppState, name: string): string {
  const values = state.categories[name];
  if (!values) return '<div class="muted">resolving values…</div>';
  if (values.length === 0) {
    return `<div class="muted">no values resolved for the marks on screen — every mark carries
      <em>absent</em> here.</div>`;
  }

  // Rank order, so the legend reads commonest-first — the same order the palette was assigned in,
  // which is what makes the list match what dominates the map.
  const shown = paletteValues(values, state.ranks[name] ?? {});
  const overflow = values.length - shown.length;

  const swatches = shown
    .map(({value, rank}) => {
      const colour = css(colourOfRank(rank));
      // The key is the identity and the label is presentation, so the key is always shown; the
      // label joins it rather than replacing it, or a renamed value becomes unrecognisable.
      const text = value.title && value.title !== value.key ? `${value.key} — ${value.title}` : value.key;
      return `<div class="row"><span class="swatch" style="background:${colour}"></span>
        <span class="v" title="code ${esc(value.code)}">${esc(text)}</span></div>`;
    })
    .join('');

  // The palette holds a fixed number of colours and they go to the values commonest among the marks on
  // screen. The rest share the unmapped colour rather than reusing a hue, which would imply two
  // values are one.
  const rest = overflow > 0
    ? `<div class="row"><span class="swatch" style="background:${css(UNMAPPED)}"></span>
       <span class="v">${overflow} rarer value${overflow === 1 ? '' : 's'}</span></div>`
    : '';

  return `<div class="legend">${swatches}${rest}
      <div class="row"><span class="swatch" style="background:${css(UNMAPPED)}"></span>
        <span class="v">absent / unresolved</span></div>
    </div>
    <div class="muted">values on screen, not the whole vocabulary</div>`;
}

function rampBody(state: AppState, name: string): string {
  const domain = state.domains[name];
  if (!domain) return '<div class="muted">no numeric values on screen</div>';
  const arrowType = state.meta?.declaredScalars.find((c) => c.name === name)?.arrowType;

  const stops = Array.from({length: 12}, (_, i) => css(colourOfFraction(i / 11))).join(', ');
  // `timestamp_us` carries its unit in its type rather than in a convention, which is the whole
  // reason the type exists — so the bound reads as a date and not as 6.69e+14 microseconds.
  const fmt = (n: number) => {
    if (arrowType === 'timestamp_us') return new Date(n / 1000).toISOString().slice(0, 10);
    if (Math.abs(n) >= 1e6 || (n !== 0 && Math.abs(n) < 1e-3)) return n.toExponential(2);
    return n.toLocaleString('en-GB');
  };

  // The domain is the range of the marks *served*, widened as you pan and never narrowed. It is not
  // the corpus range: a corpus-wide min/max would be an aggregate over items this principal cannot
  // see, which is what I2 forbids. The caption says so because the difference is not visible.
  return `<div class="ramp" style="background:linear-gradient(to right, ${stops})"></div>
    ${row('min', fmt(domain.min))}
    ${row('max', fmt(domain.max))}
    <div class="muted">range of marks served, not of the corpus</div>`;
}
