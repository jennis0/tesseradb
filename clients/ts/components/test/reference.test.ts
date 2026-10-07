import {readFileSync, readdirSync} from 'node:fs';
import {join} from 'node:path';
import ts from 'typescript';
import {describe, expect, it} from 'vitest';

/**
 * The TypeScript and components reference is generated from doc comments, so a public item with no
 * comment is a gap in the reference. The packages and entries are the ones `typedoc.json` names,
 * read here rather than listed again.
 *
 * Public means exported from an entry and not tagged `@internal`. A public class, interface or
 * object type documents each of its public members, including those it inherits from a class in
 * these packages; a member that overrides a Lit or deck.gl one belongs to that library's contract
 * and is left out, as are the fields of a union's variants, which the union's comment describes.
 *
 * Each element's class comment names what it renders, since the custom-elements manifest and the
 * element's page are made from it: its `@fires` against the events it emits, its `@slot` against
 * its `<slot>`s, its `@csspart` against its `part` attributes and the parts it forwards, and its
 * `@cssprop` against the tokens it reads. `tokens` documents every token its styles declare.
 */

const CLIENTS = join(import.meta.dirname, '../..');
const SRC = join(import.meta.dirname, '../src');
const typedoc = JSON.parse(readFileSync(join(CLIENTS, 'typedoc.json'), 'utf8')) as {entryPoints: string[]; tsconfig: string};

function compile(): ts.Program {
  const parsed = ts.getParsedCommandLineOfConfigFile(join(CLIENTS, typedoc.tsconfig), {}, {
    ...ts.sys,
    onUnRecoverableConfigFileDiagnostic: (d) => {
      throw new Error(ts.flattenDiagnosticMessageText(d.messageText, '\n'));
    }
  });
  if (!parsed) throw new Error(`cannot read ${typedoc.tsconfig}`);
  return ts.createProgram({rootNames: parsed.fileNames, options: parsed.options});
}

const external = (node: ts.Node) => node.getSourceFile().fileName.includes('/node_modules/');
const tagged = (node: ts.Node, tag: string) => ts.getJSDocTags(node).some((t) => t.tagName.text === tag);
const documented = (node: ts.Node) => ts.getJSDocCommentsAndTags(node).some((d) => ts.isJSDoc(d) && ts.getTextOfJSDocComment(d.comment)?.trim());

/** Every public item of the documented entries, and every public member of one, with no doc comment. */
function undocumented(program: ts.Program): string[] {
  const checker = program.getTypeChecker();
  const missing = new Set<string>();
  const seen = new Set<ts.Node>();

  const resolve = (symbol: ts.Symbol) => (symbol.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(symbol) : symbol);

  /** The classes a class extends, as far as they are declared in these packages. */
  const ownBases = (cls: ts.ClassLikeDeclaration): ts.ClassLikeDeclaration[] => {
    const out: ts.ClassLikeDeclaration[] = [];
    for (let current = cls; ; ) {
      const heritage = current.heritageClauses?.find((h) => h.token === ts.SyntaxKind.ExtendsKeyword)?.types[0];
      const symbol = heritage && checker.getSymbolAtLocation(heritage.expression);
      const base = symbol && resolve(symbol).declarations?.find(ts.isClassLike);
      if (!base || external(base)) return out;
      out.push(base);
      current = base;
    }
  };

  const hidden = (member: ts.Node) =>
    tagged(member, 'internal') ||
    (ts.getCombinedModifierFlags(member as ts.Declaration) & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected)) !== 0 ||
    ((member as ts.NamedDeclaration).name !== undefined && ts.isPrivateIdentifier((member as ts.NamedDeclaration).name!));

  const checkMembers = (owner: string, members: readonly ts.Node[], overridesOk: (name: string) => boolean) => {
    const byName = new Map<string, ts.Node[]>();
    for (const member of members) {
      if (ts.isConstructorDeclaration(member) || ts.isIndexSignatureDeclaration(member) || ts.isClassStaticBlockDeclaration(member) || ts.isSemicolonClassElement(member)) continue;
      const name = (member as ts.NamedDeclaration).name?.getText();
      if (!name || hidden(member)) continue;
      const overrides = ts.canHaveModifiers(member) && ts.getModifiers(member)?.some((m) => m.kind === ts.SyntaxKind.OverrideKeyword);
      if (overrides && !overridesOk(name)) continue;
      byName.set(name, [...(byName.get(name) ?? []), member]);
    }
    for (const [name, declarations] of byName) if (!declarations.some(documented)) missing.add(`${owner}.${name}`);
  };

  const checkClass = (name: string, cls: ts.ClassLikeDeclaration) => {
    const bases = ownBases(cls);
    const overrides = (m: ts.Node) => ts.canHaveModifiers(m) && ts.getModifiers(m)?.some((x) => x.kind === ts.SyntaxKind.OverrideKeyword);
    const ownNames = (c: ts.ClassLikeDeclaration) => new Set(c.members.filter((m) => !overrides(m)).map((m) => m.name?.getText()).filter(Boolean));
    for (const c of [cls, ...bases]) {
      if (seen.has(c)) continue;
      seen.add(c);
      const above = bases.slice(bases.indexOf(c as ts.ClassDeclaration) + 1);
      // An override of a member one of these packages declares is documented; one of Lit's or
      // deck.gl's, directly or through a class here, is theirs.
      checkMembers(c === cls ? name : `${name} (from ${c.name?.text})`, c.members, (member) => above.some((b) => ownNames(b).has(member)));
      const constructor = c.members.find(ts.isConstructorDeclaration);
      for (const parameter of constructor?.parameters ?? []) {
        if (!ts.isParameterPropertyDeclaration(parameter, constructor!) || hidden(parameter)) continue;
        const described = documented(parameter) || ts.getJSDocParameterTags(parameter).some((t) => ts.getTextOfJSDocComment(t.comment)?.trim());
        if (!described) missing.add(`${name}.${parameter.name.getText()}`);
      }
    }
  };

  const checkType = (name: string, type: ts.TypeNode) => {
    if (ts.isParenthesizedTypeNode(type)) return checkType(name, type.type);
    if (ts.isTypeLiteralNode(type)) return checkMembers(name, type.members, () => true);
    if (ts.isIntersectionTypeNode(type)) for (const part of type.types) checkType(name, part);
  };

  for (const entry of typedoc.entryPoints) {
    const source = program.getSourceFile(join(CLIENTS, entry));
    const module = source && checker.getSymbolAtLocation(source);
    if (!module) throw new Error(`${entry} is not in the program ${typedoc.tsconfig} makes`);
    for (const exported of checker.getExportsOfModule(module)) {
      const declarations = (resolve(exported).declarations ?? []).filter((d) => !external(d));
      if (declarations.length === 0 || declarations.some((d) => tagged(d, 'internal'))) continue;
      const name = `${entry}: ${exported.name}`;
      const first = declarations[0]!;
      if (seen.has(first)) continue;
      if (!ts.isClassLike(first)) seen.add(first);
      if (!declarations.some(documented)) missing.add(name);
      if (ts.isClassLike(first)) checkClass(name, first);
      else if (ts.isInterfaceDeclaration(first)) checkMembers(name, declarations.flatMap((d) => (ts.isInterfaceDeclaration(d) ? [...d.members] : [])), () => true);
      else if (ts.isTypeAliasDeclaration(first)) checkType(name, first.type);
    }
  }
  return [...missing].sort();
}

describe('the TypeScript reference', () => {
  it('has a doc comment for every public export and every public member of one', () => {
    expect(undocumented(compile())).toEqual([]);
  }, 60_000);
});

type Element = {file: string; text: string; tag: string; doc: readonly ts.JSDocTag[]};

/** A tag's text, and the name before its ` - ` (empty for an unnamed slot). */
function named(tag: ts.JSDocTag): {name: string; text: string; type: string | null} {
  let text = ts.getTextOfJSDocComment(tag.comment) ?? '';
  let type: string | null = null;
  const typed = /^\{([^{}]*)\}\s+/.exec(text);
  if (typed) {
    type = typed[1]!;
    text = text.slice(typed[0].length);
  }
  const match = /^(?:(\S+)\s+)?-\s/.exec(text);
  return {name: match?.[1] ?? '', text, type};
}

function elements(): Element[] {
  const out: Element[] = [];
  for (const file of readdirSync(SRC).filter((f) => f.endsWith('.ts'))) {
    const text = readFileSync(join(SRC, file), 'utf8');
    const source = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
    source.forEachChild((node) => {
      if (!ts.isClassDeclaration(node) || !tagged(node, 'tagname')) return;
      const doc = ts.getJSDocTags(node);
      out.push({file, text, tag: named(doc.find((t) => t.tagName.text === 'tagname')!).text.trim(), doc});
    });
  }
  return out;
}

const ELEMENTS = elements();
const tagsOf = (element: Element, tag: string) => element.doc.filter((t) => t.tagName.text === tag).map(named);
/** A string in single quotes, double quotes or backticks; `text` is what is between them. */
const STRING = String.raw`(?<quote>['"\`])(?<text>(?:(?!\k<quote>)[^\\])*)\k<quote>`;
const literals = (text: string) => [...text.matchAll(new RegExp(STRING, 'g'))].map((m) => m.groups!['text']!);
/** The text of the string at `%s` in every match of `pattern`. */
const quoted = (text: string, pattern: string) => [...text.matchAll(new RegExp(pattern.replace('%s', STRING), 'g'))].map((m) => m.groups!['text']!);

/** The functions in these sources that emit an event, with the events each emits. */
const EMITTERS = new Map<string, Set<string>>();
for (const file of readdirSync(SRC).filter((f) => f.endsWith('.ts'))) {
  const text = readFileSync(join(SRC, file), 'utf8');
  for (const match of text.matchAll(/export function (\w+)\([^)]*\)[^{]*\{([\s\S]*?)\n\}/g)) {
    const events = quoted(match[2]!, String.raw`\bemit\(\w+,\s*%s`);
    if (events.length > 0) EMITTERS.set(match[1]!, new Set(events));
  }
}

/** What an element emits itself: `emit(this, ...)`, and the emitting functions it calls. */
function emits(element: Element): Set<string> {
  const out = new Set(quoted(element.text, String.raw`\bemit\(this,\s*%s`));
  for (const [fn, events] of EMITTERS) if (new RegExp(`\\b${fn}\\(this\\b`).test(element.text)) for (const e of events) out.add(e);
  return out;
}

/** What an element emits and what the elements it renders emit, which bubble out of it. */
function emitsWithin(element: Element, seen = new Set<string>()): Set<string> {
  const out = emits(element);
  seen.add(element.tag);
  for (const match of element.text.matchAll(/<(tessera-[a-z-]+)[\s>]/g)) {
    const child = ELEMENTS.find((e) => e.tag === match[1]);
    if (child && !seen.has(child.tag)) for (const e of emitsWithin(child, seen)) out.add(e);
  }
  return out;
}

const EVENT_NAMES = new Set(quoted(readFileSync(join(SRC, 'events.ts'), 'utf8'), String.raw`\n  %s\??:`));
const STATE_PARTS = literals(/const STATE = \[([^\]]*)\]/.exec(readFileSync(join(SRC, 'parts.ts'), 'utf8'))![1]!);
/** Templates shared between elements, each with what an element that renders it calls; its parts are the element's. */
const SHARED = [{use: /\bnew ColourPicker\(/, text: readFileSync(join(SRC, 'colour-picker.ts'), 'utf8')}];
const tokenNames = (text: string) => new Set([...text.matchAll(/var\(\s*(--tessera-[a-z0-9-]+)/g)].map((m) => m[1]!));

describe('each element documents what it renders', () => {
  it('finds every element the sources define', () => {
    const defined = readdirSync(SRC).flatMap((f) => quoted(readFileSync(join(SRC, f), 'utf8'), String.raw`\bdefineOnce\(%s`));
    expect(ELEMENTS.map((e) => e.tag).sort()).toEqual(defined.sort());
  });

  for (const element of ELEMENTS) {
    describe(`<${element.tag}>`, () => {
      it('is defined under the tag its comment names, with a summary', () => {
        expect(element.text).toContain(`defineOnce('${element.tag}',`);
        expect(tagsOf(element, 'summary').map((s) => s.text.trim()).filter(Boolean)).toHaveLength(1);
      });

      it('documents the events it emits, with their detail type', () => {
        const fires = tagsOf(element, 'fires');
        const documented = new Set(fires.map((f) => f.name));
        for (const event of emits(element)) expect(documented, `@fires ${event}`).toContain(event);
        for (const event of documented) {
          expect(EVENT_NAMES, `${event} is in TesseraEventDetails`).toContain(event);
          expect(emitsWithin(element), `${event} is emitted by the element or one it renders`).toContain(event);
        }
        for (const f of fires) expect(f.type).toBe(`CustomEvent<TesseraEventDetails['${f.name}']>`);
      });

      it('documents its slots', () => {
        const normal = (name: string) => name.replace(/<[^>]*>/g, '<*>').replace(/\$\{[^}]*\}/g, '<*>');
        // A slot's name is a quoted attribute, or a string or template literal in `${...}`.
        const named = quoted(element.text, String.raw`<slot\s[^>]*?\bname=(?:\$\{\s*)?%s`);
        const unnamed = [...element.text.matchAll(/<slot(?=[\s>])(?![^>]*\bname=)/g)].map(() => '');
        const inCode = new Set([...named, ...unnamed].map(normal));
        expect(new Set(tagsOf(element, 'slot').map((s) => normal(s.name)))).toEqual(inCode);
      });

      it('documents its parts, and the parts it forwards', () => {
        const parts = tagsOf(element, 'csspart').map((p) => p.name);
        const plain = new Set(parts.filter((p) => !p.endsWith('-<part>')));
        const rendered = [element.text, ...SHARED.filter((t) => t.use.test(element.text)).map((t) => t.text)];
        const literal = new Set(
          rendered.flatMap((text) => [
            // Not a CSS selector's `[part='...']`, which styles a part and renders none.
            ...quoted(text, String.raw`(?<![\w[-])part=%s`),
            ...[...text.matchAll(/(?<![\w-])part=\$\{([^}]*)\}/g)].flatMap((m) => literals(m[1]!))
          ])
        );
        const states = /\brenderState\(/.test(element.text);
        for (const part of literal) expect(plain, `@csspart ${part}`).toContain(part);
        if (states) expect(plain, '@csspart state').toContain('state');
        for (const part of plain) expect([...literal, ...(states ? STATE_PARTS : [])], `${part} is rendered`).toContain(part);
        const forwarded = new Set(quoted(element.text, String.raw`\b(?:exportparts|forwarded)\(%s`).map((name) => `${name}-<part>`));
        expect(new Set(parts.filter((p) => p.endsWith('-<part>')))).toEqual(forwarded);
      });

      it('documents the tokens it reads beyond the shared ones', () => {
        const own = tokenNames(element.text);
        const shared = tokenNames(readFileSync(join(SRC, 'tokens.ts'), 'utf8'));
        const props = new Set(tagsOf(element, 'cssprop').map((p) => p.name));
        for (const token of own) expect(props, `@cssprop ${token}`).toContain(token);
        for (const token of props) expect([...own, ...shared], `${token} is read`).toContain(token);
      });
    });
  }
});

describe('the theme', () => {
  it('documents every token its styles declare, and no other', () => {
    const text = readFileSync(join(SRC, 'tokens.ts'), 'utf8');
    const source = ts.createSourceFile('tokens.ts', text, ts.ScriptTarget.Latest, true);
    const statement = source.statements.find((s) => ts.isVariableStatement(s) && s.declarationList.declarations.some((d) => d.name.getText(source) === 'tokens'))!;
    const documented = new Set(ts.getJSDocTags(statement).filter((t) => t.tagName.text === 'cssprop').map((t) => named(t).name));
    expect(documented).toEqual(tokenNames(text));
  });
});
