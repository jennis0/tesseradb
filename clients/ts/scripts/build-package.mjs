#!/usr/bin/env node
// Build the package in the working directory into `dist/`: JavaScript and declarations from
// `tsconfig.build.json`. `dist/` is emptied first, so a deleted source file leaves nothing behind
// in a packed tarball.
//
// The build resolves the other workspace packages through their `dist/` declarations, so they are
// built first: `npm run build` at `clients/ts` runs the four in dependency order.
import {execFileSync} from 'node:child_process';
import {rmSync} from 'node:fs';
import {createRequire} from 'node:module';

const tsc = createRequire(import.meta.url).resolve('typescript/bin/tsc');
rmSync('dist', {recursive: true, force: true});
execFileSync(process.execPath, [tsc, '-p', 'tsconfig.build.json'], {stdio: 'inherit'});
