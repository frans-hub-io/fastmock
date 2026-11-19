import type { Completion, CompletionContext, CompletionResult, CompletionSource } from '@codemirror/autocomplete';
import { syntaxTree } from '@codemirror/language';
import type { Diagnostic } from '@codemirror/lint';
import type { EditorState, Text } from '@codemirror/state';
import { hoverTooltip, type EditorView, type Tooltip } from '@codemirror/view';
import type { SyntaxNode } from '@lezer/common';
import {
  describeSchema,
  enumValues,
  expectedTypes,
  propertiesOf,
  schemaAt,
  validate,
  deref,
  type JsonPath,
  type SchemaContext,
} from './jsonSchema';

const VALUE_NODES = new Set(['Object', 'Array', 'String', 'Number', 'True', 'False', 'Null']);

function unquote(doc: Text, node: SyntaxNode): string {
  const raw = doc.sliceString(node.from, node.to);
  try {
    const v: unknown = JSON.parse(raw);
    return typeof v === 'string' ? v : raw;
  } catch {
    return raw.replace(/^"|"$/g, '');
  }
}

function valueChildren(node: SyntaxNode): SyntaxNode[] {
  const out: SyntaxNode[] = [];
  for (let c = node.firstChild; c; c = c.nextSibling) {
    if (VALUE_NODES.has(c.name)) out.push(c);
  }
  return out;
}

function propertyValue(prop: SyntaxNode): SyntaxNode | null {
  for (let c = prop.firstChild; c; c = c.nextSibling) {
    if (VALUE_NODES.has(c.name)) return c;
  }
  return null;
}

function propertyName(prop: SyntaxNode): SyntaxNode | null {
  return prop.getChild('PropertyName');
}

export function pathOfNode(doc: Text, node: SyntaxNode): JsonPath {
  const path: JsonPath = [];
  let cur: SyntaxNode | null = node;
  while (cur) {
    const parent: SyntaxNode | null = cur.parent;
    if (!parent) break;
    if (parent.name === 'Property') {
      const name = propertyName(parent);
      if (name && cur.name !== 'PropertyName') path.unshift(unquote(doc, name));
      cur = parent.parent;
      continue;
    }
    if (parent.name === 'Array') {
      const idx = valueChildren(parent).findIndex((c) => c.from === cur?.from && c.to === cur.to);
      if (idx >= 0) path.unshift(idx);
    }
    cur = parent;
  }
  return path;
}

export function findNode(state: EditorState, path: JsonPath): { node: SyntaxNode; key: SyntaxNode | null } | null {
  const doc = state.doc;
  const top = syntaxTree(state).topNode;
  let cur: SyntaxNode | null = valueChildren(top)[0] ?? null;
  let key: SyntaxNode | null = null;
  for (const seg of path) {
    if (!cur) return null;
    if (typeof seg === 'number' && cur.name === 'Array') {
      cur = valueChildren(cur)[seg] ?? null;
      key = null;
    } else if (typeof seg === 'string' && cur.name === 'Object') {
      let found: SyntaxNode | null = null;
      for (let c = cur.firstChild; c; c = c.nextSibling) {
        if (c.name !== 'Property') continue;
        const n = propertyName(c);
        if (n && unquote(doc, n) === seg) found = c;
      }
      if (!found) return null;
      key = propertyName(found);
      cur = propertyValue(found);
    } else {
      return null;
    }
  }
  return cur ? { node: cur, key } : null;
}

function issueRange(state: EditorState, path: JsonPath, kind: string): { from: number; to: number } {
  const hit = findNode(state, path);
  if (!hit) {
    if (path.length) return issueRange(state, path.slice(0, -1), kind);
    return { from: 0, to: Math.min(state.doc.length, 1) };
  }
  if (kind === 'unknown' && hit.key) return { from: hit.key.from, to: hit.key.to };
  if (kind === 'required' && hit.node.name === 'Object') {
    const parentProp = hit.key;
    if (parentProp) return { from: parentProp.from, to: parentProp.to };
    return { from: hit.node.from, to: hit.node.from + 1 };
  }
  if (hit.node.name === 'Object' || hit.node.name === 'Array') {
    if (hit.key) return { from: hit.key.from, to: hit.key.to };
    return { from: hit.node.from, to: hit.node.from + 1 };
  }
  return { from: hit.node.from, to: hit.node.to };
}

export function schemaDiagnostics(state: EditorState, ctx: SchemaContext): Diagnostic[] {
  const text = state.doc.toString();
  if (!text.trim()) return [];
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return [];
  }
  return validate(value, ctx).map((issue) => {
    const r = issueRange(state, issue.path, issue.kind);
    return {
      from: r.from,
      to: Math.max(r.to, r.from + 1),
      severity: 'error',
      source: 'schema',
      message: issue.message,
    } satisfies Diagnostic;
  });
}

interface CursorContext {
  objectNode: SyntaxNode | null;
  objectPath: JsonPath;
  replaceFrom: number;
  replaceTo: number;
  mode: 'key' | 'value';
  valuePath: JsonPath;
}

function cursorContext(state: EditorState, pos: number): CursorContext | null {
  const doc = state.doc;
  const tree = syntaxTree(state);
  let node: SyntaxNode | null = tree.resolveInner(pos, -1);
  if (node.name === 'PropertyName' || (node.name === 'String' && node.parent?.name === 'Object')) {
    const obj = node.parent?.name === 'Property' ? node.parent.parent : node.parent;
    return {
      objectNode: obj ?? null,
      objectPath: obj ? pathOfNode(doc, obj) : [],
      replaceFrom: node.from,
      replaceTo: node.to,
      mode: 'key',
      valuePath: [],
    };
  }
  if (node.parent?.name === 'Property' && VALUE_NODES.has(node.name) && node.name !== 'Object' && node.name !== 'Array') {
    return {
      objectNode: node.parent.parent,
      objectPath: [],
      replaceFrom: node.from,
      replaceTo: node.to,
      mode: 'value',
      valuePath: pathOfNode(doc, node),
    };
  }
  if (node.name === '⚠' && node.parent) node = node.parent;
  while (node && node.name !== 'Object' && node.name !== 'Array' && node.name !== 'Property') node = node.parent;
  if (!node) return null;
  if (node.name === 'Property') {
    const colon = node.getChild(':');
    if (colon && pos > colon.from) {
      const name = propertyName(node);
      const objPath = node.parent ? pathOfNode(doc, node.parent) : [];
      return {
        objectNode: node.parent,
        objectPath: objPath,
        replaceFrom: pos,
        replaceTo: pos,
        mode: 'value',
        valuePath: name ? [...objPath, unquote(doc, name)] : objPath,
      };
    }
    node = node.parent;
    if (!node) return null;
  }
  if (node.name === 'Object') {
    const before = doc.sliceString(Math.max(node.from, pos - 200), pos);
    const m = /"?[\w$-]*$/.exec(before);
    const typed = m ? m[0].length : 0;
    return {
      objectNode: node,
      objectPath: pathOfNode(doc, node),
      replaceFrom: pos - typed,
      replaceTo: pos,
      mode: 'key',
      valuePath: [],
    };
  }
  return null;
}

function existingKeys(doc: Text, obj: SyntaxNode | null): Set<string> {
  const keys = new Set<string>();
  if (!obj) return keys;
  for (let c = obj.firstChild; c; c = c.nextSibling) {
    if (c.name !== 'Property') continue;
    const n = propertyName(c);
    if (n) keys.add(unquote(doc, n));
  }
  return keys;
}

function placeholderFor(ctx: SchemaContext, schema: Record<string, unknown>): string {
  const s = deref(ctx.root, schema);
  const t = expectedTypes(s).filter((x) => x !== 'null');
  const variants = s && Array.isArray(s.oneOf) ? (s.oneOf as Record<string, unknown>[]) : [];
  const first = t[0] ?? (variants.length ? expectedTypes(deref(ctx.root, variants.find((v) => !expectedTypes(deref(ctx.root, v)).includes('null')))).at(0) : undefined);
  const hasProps = !!s?.properties || propertiesOf(ctx.root, s).length > 0;
  switch (first ?? (hasProps ? 'object' : '')) {
    case 'object':
      return '{}';
    case 'array':
      return '[]';
    case 'string':
      return '""';
    case 'integer':
    case 'number':
      return '0';
    case 'boolean':
      return 'false';
    default:
      return hasProps ? '{}' : 'null';
  }
}

export function schemaCompletion(getCtx: () => SchemaContext | null): CompletionSource {
  return (context: CompletionContext): CompletionResult | null => {
    const ctx = getCtx();
    if (!ctx) return null;
    const cc = cursorContext(context.state, context.pos);
    if (!cc) return null;
    if (cc.mode === 'key') {
      const objSchema = schemaAt(ctx.root, ctx.schema, cc.objectPath);
      if (!objSchema) return null;
      const present = existingKeys(context.state.doc, cc.objectNode);
      const options: Completion[] = propertiesOf(ctx.root, objSchema)
        .filter((p) => !present.has(p.name) || context.state.doc.sliceString(cc.replaceFrom, cc.replaceTo).replace(/"/g, '') === p.name)
        .map((p) => ({
          label: p.name,
          type: 'property',
          detail: `${describeSchema(ctx.root, p.schema)}${p.required ? ' (required)' : ''}`,
          ...(p.description ? { info: p.description } : {}),
          boost: p.required ? 2 : 0,
          apply: `"${p.name}": ${placeholderFor(ctx, p.schema)}`,
        }));
      if (!options.length) return null;
      if (!context.explicit && cc.replaceFrom === cc.replaceTo) {
        const prev = context.state.doc.sliceString(Math.max(0, cc.replaceFrom - 1), cc.replaceFrom);
        if (!/[{,\s]/.test(prev)) return null;
      }
      return { from: cc.replaceFrom, to: cc.replaceTo, options, validFor: /^"?[\w$-]*"?$/ };
    }
    const vs = schemaAt(ctx.root, ctx.schema, cc.valuePath);
    const values = enumValues(ctx.root, vs);
    if (!values.length) return null;
    return {
      from: cc.replaceFrom,
      to: cc.replaceTo,
      options: values.map((v) => ({ label: JSON.stringify(v), type: 'enum' })),
    };
  };
}

export function schemaHover(getCtx: () => SchemaContext | null) {
  return hoverTooltip((view: EditorView, pos: number): Tooltip | null => {
    const ctx = getCtx();
    if (!ctx) return null;
    const node = syntaxTree(view.state).resolveInner(pos, 1);
    if (node.name !== 'PropertyName' || !node.parent) return null;
    const value = propertyValue(node.parent);
    const name = unquote(view.state.doc, node);
    const path = value ? pathOfNode(view.state.doc, value) : [...pathOfNode(view.state.doc, node.parent), name];
    const parentSchema = schemaAt(ctx.root, ctx.schema, path.slice(0, -1));
    const prop = propertiesOf(ctx.root, parentSchema).find((p) => p.name === name);
    if (!prop) return null;
    return {
      pos: node.from,
      end: node.to,
      above: true,
      create() {
        const dom = document.createElement('div');
        dom.className = 'cm-schema-tooltip';
        const title = document.createElement('strong');
        title.textContent = `${name}: ${describeSchema(ctx.root, prop.schema)}`;
        dom.appendChild(title);
        if (prop.description) {
          const p = document.createElement('div');
          p.textContent = prop.description;
          dom.appendChild(p);
        }
        return { dom };
      },
    };
  });
}
