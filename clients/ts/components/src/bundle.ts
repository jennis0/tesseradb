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
 * This file is also the notebook widget's `_esm` (design §7): anywidget looks for `initialize`
 * and `render` on the module it evaluates, and finds `widget.ts`'s. A page with no build step
 * that loads the same file gets two extra exports it never calls.
 */
import DecodeWorker from '@tesseradb/client/src/decode.worker.ts?worker&inline';
import {setWorkerFactory} from '@tesseradb/client';

setWorkerFactory(() => new DecodeWorker());

export * from './index.js';
export {initialize, render, draftOf, type WidgetModel, type KernelMessage, type PageMessage} from './widget.js';
