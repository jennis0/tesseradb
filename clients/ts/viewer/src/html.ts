/**
 * Escape a value for interpolation into a panel's HTML. The panels build markup as strings from
 * values that come from the corpus or the server, such as column contents and error details.
 */
export function esc(value: unknown): string {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

/** A titled panel section. */
export function panel(title: string, body: string): string {
  return `<section class="panel"><h2>${esc(title)}</h2>${body}</section>`;
}

/** A label/value row. `value` is escaped; pass pre-built markup through `body` instead. */
export function row(label: string, value: string, cls = 'v'): string {
  return `<div class="row"><span class="muted">${esc(label)}</span><span class="${cls}">${esc(
    value
  )}</span></div>`;
}
