// The custom-elements manifest the Components reference is rendered from:
// `npx custom-elements-manifest analyze` in this directory writes custom-elements.json beside it.
export default {
  globs: ['src/**/*.ts'],
  exclude: ['src/bundle.ts', 'src/widget.ts'],
  outdir: '.',
  litelement: true,
  packagejson: false
};
