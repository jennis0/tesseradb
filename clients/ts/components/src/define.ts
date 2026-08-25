import {ContextRoot} from '@lit/context';

/**
 * The two module-level mechanics every entry shares (design client-components §5.9).
 *
 * **Every define is guarded.** `customElements.define` throws on a second definition, and a
 * second definition is ordinary here: anywidget evaluates `_esm` once per model, and a page may
 * hold both the unbundled and the single-file distribution. The first class to claim a tag keeps
 * it; a later copy of the package defines nothing and its elements upgrade to the first's.
 *
 * **The context root is attached once per document, on import.** Lit's context protocol is a
 * one-shot event: a `context-request` dispatched before its provider connected is lost, so a
 * panel rendered above the explorer in the DOM — or upgraded before it — would stay detached. A
 * `ContextRoot` on the document body buffers those requests and replays them when a provider
 * connects. Guarded through a global rather than a module variable, because two copies of this
 * module would otherwise attach two roots.
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
