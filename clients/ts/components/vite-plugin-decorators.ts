import {transform} from 'esbuild';
import type {Plugin} from 'vite';

/**
 * Lower the standard decorators (`@property() accessor x`) the elements are written with, for Vite
 * serving or bundling the component sources: the viewer, the examples and the self-contained
 * bundle. The npm build lowers them with `tsc`, so a host consuming `dist/` needs nothing.
 *
 * Vite 8 transforms TypeScript with oxc, which lowers only the legacy (`experimentalDecorators`)
 * form and passes the standard form through to a browser that cannot parse it. esbuild lowers the
 * standard form, so this runs it first (`enforce: 'pre'`) over the component sources only,
 * stripping types in the same pass; oxc then sees plain ES2022. It is unnecessary once oxc lowers
 * standard decorators.
 */
export function tesseraDecorators(): Plugin {
  return {
    name: 'tessera-decorators',
    enforce: 'pre',
    async transform(code, id) {
      if (!/\/components\/src\/[^?]+\.ts$/.test(id)) return null;
      if (!/@(property|state|query)\b|\baccessor\b/.test(code)) return null;
      const result = await transform(code, {
        loader: 'ts',
        target: 'es2022',
        sourcemap: true,
        sourcefile: id,
        tsconfigRaw: {compilerOptions: {experimentalDecorators: false, useDefineForClassFields: true}}
      });
      return {code: result.code, map: result.map};
    }
  };
}
