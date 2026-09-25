#!/usr/bin/env node
// Generate the TypeScript and components reference from the client sources, into two gitignored
// directories that the documentation site includes:
//
//   docs/reference/typescript/   TypeDoc over the package entries typedoc.json names
//   docs/reference/components/   one page per element, the theme tokens and the events, from the
//                                custom-elements manifest the analyzer writes
//
// It fails on any TypeDoc warning, on a documented item whose type names an item the reference
// leaves out, and when a token or an event in the code is missing from the pages. That each
// element documents every part, slot and event it renders is checked by
// components/test/reference.test.ts, which scripts/check-docs.sh runs first.
import {execFileSync} from 'node:child_process';
import {mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {dirname, join, relative} from 'node:path';
import {fileURLToPath} from 'node:url';
import {Application, normalizePath} from 'typedoc';
import ts from 'typescript';

const clients = join(dirname(fileURLToPath(import.meta.url)), '..');
const repo = join(clients, '../..');
const components = join(clients, 'components');
const outComponents = join(repo, 'docs/reference/components');

/**
 * Types a documented item may name although the reference leaves them out, each described in the
 * comment of the item that names it.
 */
const UNDOCUMENTED_TYPES = new Set([
  // `ArtifactsProjection.status`, whose values its comment names.
  '@tesseradb/client:ArtifactChannelState',
  // `StoreOptions.replica`, whose four fields its comment describes.
  '@tesseradb/client:ReplicaOptions',
  // `Store.subscribe`'s callback, `() => void`.
  '@tesseradb/client:Listener',
  // The props type `TesseraLayer` is declared over, which adds `slab` for `<tessera-map>`, and so
  // appears in its inherited constructor.
  '@tesseradb/deck:TesseraLayerInternalProps',
  '@tesseradb/deck:MarkSlab'
]);

let failures = 0;
function fail(message) {
  console.error(`reference: ${message}`);
  failures++;
}

// The TypeScript reference.
process.chdir(clients);
// The element classes are documented on the components pages, so a type naming one links there.
const elementClasses = elementClassesByTag();
const app = await Application.bootstrapWithPlugins({
  options: join(clients, 'typedoc.json'),
  externalSymbolLinkMappings: {
    '@tesseradb/components': Object.fromEntries([...elementClasses].map(([cls, tag]) => [cls, `../../components/${tag}.md`]))
  }
});
const project = await app.convert();
if (!project || app.logger.hasErrors()) {
  console.error('reference: TypeDoc failed (above). Every warning counts: fix the comment it names.');
  process.exit(1);
}
await app.generateOutputs(project);
if (app.logger.hasErrors()) process.exit(1);
// The package pages are the reference; docs/reference/typescript.md is its index.
const outTypescript = app.options.getValue('out');
rmSync(join(outTypescript, 'index.md'), {force: true});
// TypeDoc writes an array type's `[]` bare, and `[][]` reads to Python-Markdown as a reference link.
for (const file of markdownFiles(outTypescript)) {
  const text = readFileSync(file, 'utf8');
  const escaped = text
    .split('\n')
    .map((line) => line.split(/(`+[^`]*`+)/).map((part, i) => (i % 2 ? part : part.replace(/(?<!\\)\[\]/g, '\\[\\]'))).join(''))
    .join('\n');
  if (escaped !== text) writeFileSync(file, escaped);
}

for (const [target, users] of danglingReferences(app.serializer.projectToObject(project, normalizePath(clients)))) {
  if (UNDOCUMENTED_TYPES.has(target) || (target.startsWith('@tesseradb/components:') && elementClasses.has(target.split(':')[1]))) continue;
  fail(`${[...users].join(', ')} names ${target}, which the reference leaves out. Export and document it, or mark the member that names it @internal.`);
}

// The components reference.
const cem = join(dirname(createRequire(import.meta.url).resolve('@custom-elements-manifest/analyzer/package.json')), 'cem.js');
execFileSync(process.execPath, [cem, 'analyze', '--config', 'custom-elements-manifest.config.mjs', '--quiet'], {cwd: components, stdio: 'inherit'});
const manifest = JSON.parse(readFileSync(join(components, 'custom-elements.json'), 'utf8'));
const exportsBySource = subpaths(JSON.parse(readFileSync(join(components, 'package.json'), 'utf8')).exports);

const elements = [];
for (const module of manifest.modules) {
  for (const declaration of module.declarations ?? []) {
    if (declaration.kind === 'class' && declaration.tagName) elements.push({...declaration, path: module.path});
  }
}
elements.sort((a, b) => a.tagName.localeCompare(b.tagName));

const events = eventDetails(join(components, 'src/events.ts'));
const tokens = themeTokens(join(components, 'src/tokens.ts'), elements);

rmSync(outComponents, {recursive: true, force: true});
mkdirSync(outComponents, {recursive: true});
const pages = new Map();
for (const element of elements) pages.set(`${element.tagName}.md`, elementPage(element));
pages.set('events.md', eventsPage(events, elements));
pages.set('tokens.md', tokensPage(tokens));
for (const [name, text] of pages) writeFileSync(join(outComponents, name), text);

// What the code has, against what the pages say.
const tokensText = pages.get('tokens.md');
for (const name of tokenNamesInCode(join(components, 'src'))) {
  if (!tokensText.includes(`\`${name}\``)) fail(`the token ${name} is in the code and not on tokens.md; document it with @cssprop on \`tokens\` or on its element.`);
}
for (const name of events.keys()) {
  if (!pages.get('events.md').includes(`\`${name}\``)) fail(`the event ${name} is not on events.md.`);
  if (!elements.some((e) => (e.events ?? []).some((x) => x.name === name))) fail(`no element documents the event ${name} with @fires.`);
}
for (const element of elements) {
  const text = pages.get(`${element.tagName}.md`);
  const named = [
    ...(element.attributes ?? []).map((a) => a.name),
    ...publicMembers(element).map((m) => m.name),
    ...(element.events ?? []).map((e) => e.name),
    ...(element.slots ?? []).map((s) => s.name).filter(Boolean),
    ...(element.cssParts ?? []).map((p) => p.name),
    ...(element.cssProperties ?? []).map((p) => p.name)
  ];
  for (const name of named) if (!text.includes(`\`${name}`)) fail(`${element.tagName}.md does not list \`${name}\`.`);
}

// The index, docs/reference/components.md, lists every element with its summary.
const index = readFileSync(join(repo, 'docs/reference/components.md'), 'utf8');
for (const element of elements) {
  const entry = `| [\`<${element.tagName}>\`](components/${element.tagName}.md) | ${element.summary} |`;
  if (!index.includes(entry)) fail(`docs/reference/components.md has no row \`${entry}\`; the summary is the element's @summary.`);
}

if (failures > 0) process.exit(1);
console.log(`reference: wrote ${relative(repo, outTypescript)}/ and ${relative(repo, outComponents)}/ (${pages.size} pages)`);

function markdownFiles(dir) {
  return readdirSync(dir, {recursive: true, encoding: 'utf8'}).filter((f) => f.endsWith('.md')).map((f) => join(dir, f));
}

/** Each element class, by the tag its comment's `@tagname` gives. */
function elementClassesByTag() {
  const classes = new Map();
  const dir = join(components, 'src');
  for (const file of readdirSync(dir)) {
    const text = readFileSync(join(dir, file), 'utf8');
    for (const match of text.matchAll(/@tagname ([a-z-]+)[\s\S]*?\*\/\s*export class (\w+)/g)) classes.set(match[2], match[1]);
  }
  return classes;
}

/** References to a reflection the output does not include, by target, with the items that make them. */
function danglingReferences(json) {
  const found = new Map();
  const walk = (node, path) => {
    if (Array.isArray(node)) return node.forEach((n) => walk(n, path));
    if (!node || typeof node !== 'object') return;
    const here = typeof node.name === 'string' && typeof node.kind === 'number' ? (path ? `${path}.${node.name}` : node.name) : path;
    const excluded = node.target === -1 || (typeof node.target === 'object' && node.target !== null);
    if (node.type === 'reference' && excluded && !node.refersToTypeParameter && (node.package ?? '').startsWith('@tesseradb/')) {
      const key = `${node.package}:${node.name}`;
      if (!found.has(key)) found.set(key, new Set());
      found.get(key).add(here);
    }
    for (const [key, value] of Object.entries(node)) if (key !== 'target') walk(value, here);
  };
  walk(json, '');
  return found;
}

/** The package subpath that loads each source file, from the `exports` map's source condition. */
function subpaths(exportsMap) {
  const out = new Map();
  for (const [subpath, target] of Object.entries(exportsMap)) {
    const source = typeof target === 'object' ? target['tessera-source'] : null;
    if (source) out.set(source.replace(/^\.\//, ''), `@tesseradb/components${subpath === '.' ? '' : subpath.slice(1)}`);
  }
  return out;
}

/** The public fields and methods of an element, less the reactive properties that are attributes. */
function publicMembers(element) {
  return (element.members ?? []).filter((m) => !m.privacy && !m.static && m.name !== 'styles');
}

/** Each event's `detail` type and description, from `TesseraEventDetails`. */
function eventDetails(file) {
  const source = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true);
  const out = new Map();
  source.forEachChild((node) => {
    if (!ts.isTypeAliasDeclaration(node) || node.name.text !== 'TesseraEventDetails' || !ts.isTypeLiteralNode(node.type)) return;
    for (const member of node.type.members) {
      if (!ts.isPropertySignature(member) || !member.type) continue;
      const name = member.name.getText(source).replace(/^'|'$/g, '');
      out.set(name, {detail: oneLine(member.type.getText(source)), description: commentText(member)});
    }
  });
  if (out.size === 0) fail('found no events in TesseraEventDetails.');
  return out;
}

/** The tokens `tokens` documents, with their defaults, then the ones only an element declares. */
function themeTokens(file, elements) {
  const text = readFileSync(file, 'utf8');
  const source = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
  let tags = [];
  source.forEachChild((node) => {
    if (ts.isVariableStatement(node) && node.declarationList.declarations.some((d) => d.name.getText(source) === 'tokens')) {
      tags = ts.getJSDocTags(node).filter((t) => t.tagName.text === 'cssprop').map((t) => tagText(t));
    }
  });
  const defaults = varDefaults(text);
  const out = [];
  for (const tag of tags) {
    const {name, description} = splitTag(tag);
    out.push({name, description, value: defaults.get(name) ?? null, element: null});
  }
  for (const element of elements) {
    const own = varDefaults(readFileSync(join(components, element.path), 'utf8'));
    for (const prop of element.cssProperties ?? []) {
      if (out.some((t) => t.name === prop.name)) continue;
      out.push({name: prop.name, description: oneLine(prop.description ?? ''), value: own.get(prop.name) ?? null, element: element.tagName});
    }
  }
  return out;
}

/** `var(--tessera-name, default)` in some CSS, as name to default. */
function varDefaults(text) {
  const out = new Map();
  for (const match of text.matchAll(/var\(\s*(--tessera-[a-z0-9-]+)\s*,/g)) {
    let depth = 1;
    let i = match.index + match[0].length;
    const start = i;
    for (; i < text.length && depth > 0; i++) {
      if (text[i] === '(') depth++;
      else if (text[i] === ')') depth--;
    }
    if (!out.has(match[1])) out.set(match[1], oneLine(text.slice(start, i - 1)));
  }
  return out;
}

/** A `light-dark(a, b)` default as its light and dark values; any other as the same value twice. */
function lightDark(value) {
  const pick = (which) => {
    let out = '';
    let i = 0;
    while (i < value.length) {
      const at = value.indexOf('light-dark(', i);
      if (at < 0) {
        out += value.slice(i);
        break;
      }
      out += value.slice(i, at);
      let depth = 1;
      let j = at + 'light-dark('.length;
      const parts = [];
      let from = j;
      for (; j < value.length && depth > 0; j++) {
        if (value[j] === '(') depth++;
        else if (value[j] === ')') depth--;
        if ((value[j] === ',' && depth === 1) || depth === 0) {
          parts.push(value.slice(from, j).trim());
          from = j + 1;
        }
      }
      out += parts[which] ?? '';
      i = j;
    }
    return out;
  };
  return {light: pick(0), dark: pick(1)};
}

function tokenNamesInCode(dir) {
  const names = new Set();
  for (const file of readdirSync(dir)) {
    if (!file.endsWith('.ts')) continue;
    for (const match of readFileSync(join(dir, file), 'utf8').matchAll(/(?:var\(|setProperty\(')\s*(--tessera-[a-z0-9]+(?:-[a-z0-9]+)*)/g)) names.add(match[1]);
  }
  return names;
}

function elementPage(element) {
  const lines = [`# \`<${element.tagName}>\``, '', element.summary ?? '', ''];
  if (element.description) lines.push(element.description, '');
  const subpath = exportsBySource.get(element.path);
  if (subpath) lines.push('```js', `import '${subpath}';`, '```', '');

  const attributes = element.attributes ?? [];
  const fields = new Map(publicMembers(element).map((m) => [m.name, m]));
  if (attributes.length > 0) {
    lines.push('## Attributes', '', '| Attribute | Property | Type | Default | Description |', '| --- | --- | --- | --- | --- |');
    for (const a of attributes) {
      const field = a.fieldName ? fields.get(a.fieldName) : undefined;
      lines.push(row([code(a.name), a.fieldName ? code(a.fieldName) : '', code(a.type?.text ?? field?.type?.text), code(a.default ?? field?.default), a.description ?? field?.description ?? '']));
    }
    lines.push('');
  }
  const attributed = new Set(attributes.map((a) => a.fieldName));
  const properties = [...fields.values()].filter((m) => m.kind === 'field' && !attributed.has(m.name));
  if (properties.length > 0) {
    lines.push('## Properties', '', 'Set as JavaScript properties; they have no attribute.', '', '| Property | Type | Default | Description |', '| --- | --- | --- | --- |');
    for (const p of properties) lines.push(row([code(p.name), code(p.type?.text), p.readonly ? 'read-only' : code(p.default), p.description ?? '']));
    lines.push('');
  }
  const methods = [...fields.values()].filter((m) => m.kind === 'method');
  if (methods.length > 0) {
    lines.push('## Methods', '', '| Method | Returns | Description |', '| --- | --- | --- |');
    for (const m of methods) {
      const params = (m.parameters ?? []).map((p) => `${p.name}${p.optional ? '?' : ''}: ${p.type?.text ?? 'unknown'}`).join(', ');
      lines.push(row([code(`${m.name}(${params})`), code(m.return?.type?.text ?? 'void'), m.description ?? '']));
    }
    lines.push('');
  }
  if ((element.events ?? []).length > 0) {
    lines.push('## Events', '', 'Each bubbles and is composed. [Events](events.md) gives each `detail`.', '', '| Event | Fired when |', '| --- | --- |');
    for (const e of element.events) lines.push(row([`[${code(e.name)}](events.md#${e.name})`, e.description ?? '']));
    lines.push('');
  }
  if ((element.slots ?? []).length > 0) {
    lines.push('## Slots', '', '| Slot | Description |', '| --- | --- |');
    for (const s of element.slots) lines.push(row([s.name ? code(s.name) : '(default)', s.description ?? '']));
    lines.push('');
  }
  if ((element.cssParts ?? []).length > 0) {
    lines.push('## Parts', '', 'Styled from outside the element with `::part()`.', '', '| Part | Description |', '| --- | --- |');
    for (const p of element.cssParts) lines.push(row([code(p.name), p.description ?? '']));
    lines.push('');
  }
  if ((element.cssProperties ?? []).length > 0 || (element.cssParts ?? []).length > 0) {
    lines.push('## CSS custom properties', '');
    if ((element.cssProperties ?? []).length > 0) {
      lines.push('| Property | Default | Description |', '| --- | --- | --- |');
      for (const p of element.cssProperties) lines.push(row([code(p.name), defaultText(tokens.find((t) => t.name === p.name)?.value ?? null), p.description ?? '']));
      lines.push('');
    }
    lines.push('The [theme tokens](tokens.md) apply to this element.', '');
  }
  return lines.join('\n');
}

function eventsPage(events, elements) {
  const lines = [
    '# Events',
    '',
    "Every event the elements fire, with its `detail`. Each is a `CustomEvent` that bubbles and is composed, so a host listens on the element or on any ancestor. A `tessera_id` crosses as a decimal string. `TesseraEventDetails` in `@tesseradb/components` types every `detail`.",
    ''
  ];
  for (const [name, {detail, description}] of events) {
    const firedBy = elements.filter((e) => (e.events ?? []).some((x) => x.name === name)).map((e) => `[${code(`<${e.tagName}>`)}](${e.tagName}.md)`);
    lines.push(`## \`${name}\``, '', description, '', `Detail: ${code(detail)}`, '', `Fired by: ${firedBy.join(', ')}.`, '');
  }
  return lines.join('\n');
}

function tokensPage(tokens) {
  // A colour token's default is a light-dark() pair of colours; the shadow's pairs hold shadows.
  const colour = /^(#[0-9a-f]{3,8}|(rgb|hsl)a?\(.*\)|[a-z]+)$/i;
  const colours = tokens.filter((t) => {
    if (!t.value?.includes('light-dark(')) return false;
    const {light, dark} = lightDark(t.value);
    return colour.test(light) && colour.test(dark);
  });
  const others = tokens.filter((t) => !colours.includes(t));
  const lines = [
    '# Theme tokens',
    '',
    "The elements take their colours, fonts, corner radius and shadow from these CSS custom properties, and the map its height and the spacing of its corners; other spacing is fixed. Set one on an element or on any ancestor, such as `:root` or an enclosing `<tessera-explorer>`, and the elements inside take it. Where none is set, the default below applies. A colour has a light and a dark default, chosen by the `color-scheme` the element inherits from the page.",
    '',
    '## Colours',
    '',
    '| Token | Light default | Dark default | What it sets |',
    '| --- | --- | --- | --- |'
  ];
  for (const t of colours) {
    const {light, dark} = lightDark(t.value);
    lines.push(row([code(t.name), code(light), code(dark), t.description]));
  }
  lines.push('', '## Fonts, sizes, spacing and shadow', '', '| Token | Default | What it sets |', '| --- | --- | --- |');
  for (const t of others) lines.push(row([code(t.name), defaultText(t.value), t.element ? `${t.description} Read by \`<${t.element}>\` alone.` : t.description]));
  lines.push('');
  return lines.join('\n');
}

/** A token's default as a table cell: one value, or its light and dark values. */
function defaultText(value) {
  if (value === null) return 'none';
  const {light, dark} = lightDark(value);
  return light === dark ? code(light) : `${code(light)} (light), ${code(dark)} (dark)`;
}

/** A table row. A pipe outside a code span is escaped; Python-Markdown leaves one inside a code span alone. */
function row(cells) {
  const cell = (c) => oneLine(c ?? '').split(/(`+[^`]*`+)/).map((part, i) => (i % 2 ? part : part.replace(/\|/g, '\\|'))).join('');
  return `| ${cells.map(cell).join(' | ')} |`;
}

function code(text) {
  if (text === undefined || text === null || text === '') return '';
  const t = oneLine(String(text));
  return t.includes('`') ? `\`\` ${t} \`\`` : `\`${t}\``;
}

function oneLine(text) {
  return String(text).replace(/\s*\n\s*/g, ' ').trim();
}

function commentText(node) {
  const docs = node.jsDoc ?? [];
  const last = docs[docs.length - 1];
  if (!last || !last.comment) return '';
  return oneLine(typeof last.comment === 'string' ? last.comment : last.comment.map((c) => c.text).join(''));
}

function tagText(tag) {
  return typeof tag.comment === 'string' ? tag.comment : (tag.comment ?? []).map((c) => c.text).join('');
}

/** `--name - description` as its two halves. */
function splitTag(text) {
  const match = /^\s*(--[a-z0-9-]+)\s*-\s*([\s\S]*)$/.exec(text);
  if (!match) {
    fail(`cannot read the @cssprop tag "${text}"; write it as \`@cssprop --tessera-name - What it sets.\``);
    return {name: text.trim(), description: ''};
  }
  return {name: match[1], description: oneLine(match[2])};
}
