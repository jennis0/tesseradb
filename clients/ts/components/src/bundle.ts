/**
 * The single-file distribution's entry: everything the root entry defines, with the decode worker
 * inlined, since a single file has no relative worker file to load. Vite's `?worker&inline` makes
 * the worker from a Blob URL, falling back to a data URL where a host's `worker-src` refuses blob.
 * Where both are refused the decoder decodes on the main thread, at tens of milliseconds a
 * response. The factory is installed before any element can build a store.
 *
 * This file is also the notebook widget's `_esm`: anywidget takes the default export,
 * `{initialize, render}` from `widget.ts`.
 */
import DecodeWorker from '@mosaicajs/client/decode.worker?worker&inline';
import {setWorkerFactory} from '@mosaicajs/client';

setWorkerFactory(() => new DecodeWorker());

export * from './index.js';
export {draftOf, type WidgetModel, type KernelMessage, type PageMessage} from './widget.js';
// anywidget warns about a named `render` export; it wants the default export.
export {default} from './widget.js';
