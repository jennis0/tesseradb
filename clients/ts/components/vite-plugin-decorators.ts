import {transform} from 'esbuild';
import type {Plugin} from 'vite';

/**
 * Lower the standard decorators (`@property() accessor x`) the elements are written with.
 *
 * The elements use TC39 stage-3 decorators with `accessor`, as design §5.9 decides — under the
 * workspace's ES2022 target a legacy `@property` on a plain field is shadowed by class-field
 * definition and silently does nothing. Vite 8 transforms TypeScript with oxc, which lowers only
 * the legacy (`experimentalDecorators`) form and passes the standard form through untouched, so
 * the browser receives `accessor` and a decorator it cannot parse ("Invalid or unexpected token"
 * at the first element). Found by the smoke script on the first dev run of the components.
 *
 * esbuild lowers the standard form (since 0.21.3), so this runs it first — `enforce: 'pre'` —
 * over the component sources only, with types stripped in the same pass; oxc then sees plain
 * ES2022. The dev server and the single-file bundle share it. Remove it when oxc lowers
 * stage-3 decorators.
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
