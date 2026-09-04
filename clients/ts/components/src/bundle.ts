/**
 * The single-file distribution's entry (design §5.9): everything the root entry defines, with the
 * decode worker **inlined**. A relative worker file does not exist inside one file, and the
 * built decoder would otherwise fall back to the main thread silently, at tens of milliseconds a
 * response. Vite's `?worker&inline` makes the worker from a Blob URL and falls back to a data
 * URL where a host's `worker-src` refuses blob; where both are refused, the constructor throws,
 * the decoder catches it and decodes inline — the only case that path is meant for.
 *
 * Installed before any element could build a store, since a store builds its decoder on its
 * first response.
 *
 * This file is also the notebook widget's `_esm` (design §7): anywidget takes the module's default
 * export, `{initialize, render}` from `widget.ts`. A page with no build step that loads the same
 * file gets a default export it never calls.
 */
import DecodeWorker from '@tesseradb/client/src/decode.worker.ts?worker&inline';
import {setWorkerFactory} from '@tesseradb/client';

setWorkerFactory(() => new DecodeWorker());

export * from './index.js';
export {draftOf, type WidgetModel, type KernelMessage, type PageMessage} from './widget.js';
// anywidget takes the entry as a default export holding `initialize` and `render`; a named
// `render` export is the pre-0.9 shape it warns about.
export {default} from './widget.js';
