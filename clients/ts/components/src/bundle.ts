/**
 * The self-contained bundle's entry: everything the root entry defines, with the decode worker
 * inlined. One file has no sibling worker file to load, so the worker is built from the bundle
 * itself: Vite's `?worker&inline` makes it from a Blob URL, and from a data URL where a host's
 * `worker-src` refuses blob. Where both are refused the constructor throws, and the decoder
 * decodes on the main thread.
 *
 * The factory is installed before any element can build a store, since a store builds its decoder
 * on its first response.
 *
 * This file is also the notebook widget's `_esm`: anywidget takes the module's default export,
 * `{initialize, render}` from `widget.ts`. A page with no build step that loads the same file gets
 * a default export it never calls.
 */
import DecodeWorker from '@tesseradb/client/decode.worker?worker&inline';
import {setWorkerFactory} from '@tesseradb/client';

setWorkerFactory(() => new DecodeWorker());

export * from './index.js';
export {draftOf, type WidgetModel, type KernelMessage, type PageMessage} from './widget.js';
// anywidget takes the entry as a default export holding `initialize` and `render`; a named
// `render` export is the pre-0.9 shape it warns about.
export {default} from './widget.js';
