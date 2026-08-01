/**
 * Escape a value for interpolation into a panel's HTML.
 *
 * The panels build markup as strings, and several of the values they interpolate are **not ours**:
 * a scalar column's contents come from whatever corpus was built into the bundle, and an error
 * `detail` comes off the wire. Neither is markup and neither may become markup.
 *
 * This is hygiene rather than a security boundary — the bundle and the server are inside the same
 * trust boundary as this page. It is here because the alternative is remembering to be careful at
 * every interpolation site, which is exactly the discipline that fails quietly.
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
