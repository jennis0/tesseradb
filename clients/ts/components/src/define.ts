import {ContextRoot} from '@lit/context';

/**
 * Module-level set-up every entry shares.
 *
 * Every define is guarded: anywidget evaluates `_esm` once per model, and a page may hold two
 * copies of the package. The first class to claim a tag keeps it.
 *
 * A context root is attached to the document body once, on import. A Lit `context-request`
 * dispatched before its provider connects is otherwise lost, so a panel upgraded before the
 * explorer would stay detached; the root replays those requests. The guard is a global, so two
 * copies of this module attach one root.
 */

const ROOT_KEY = '__tesseradbContextRoot';

export function attachContextRoot(): void {
  if (typeof document === 'undefined') return;
  const holder = document as unknown as Record<string, unknown>;
  if (holder[ROOT_KEY]) return;
  const root = new ContextRoot();
  const attach = () => root.attach(document.body);
  if (document.body) attach();
  else document.addEventListener('DOMContentLoaded', attach, {once: true});
  holder[ROOT_KEY] = root;
}

export function defineOnce(tag: string, cls: CustomElementConstructor): void {
  if (typeof customElements === 'undefined') return;
  if (customElements.get(tag)) return;
  customElements.define(tag, cls);
}
