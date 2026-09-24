import {transform} from 'esbuild';
import type {Plugin} from 'vite';

/**
 * Lower the standard decorators (`@property() accessor x`) the elements are written with.
 *
 * The elements use stage-3 decorators with `accessor`, since under the ES2022 target a legacy
 * `@property` on a plain field is shadowed by the class field and has no effect. Vite 8's oxc
 * lowers only the legacy form and passes the standard form through, which the browser cannot
 * parse. esbuild (0.21.3 and later) lowers it, so this plugin runs esbuild first
 * (`enforce: 'pre'`) over the component sources, stripping types in the same pass. The dev server
 * and the single-file bundle share it. It can go once oxc lowers stage-3 decorators.
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
