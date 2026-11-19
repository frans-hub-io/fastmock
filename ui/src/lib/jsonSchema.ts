export type JsonPath = (string | number)[];

export interface SchemaIssue {
  path: JsonPath;
  message: string;
  kind: 'type' | 'required' | 'unknown' | 'enum' | 'constraint' | 'variant';
}

export type Schema = Record<string, unknown>;

export interface SchemaContext {
  root: Schema;
  schema: Schema;
}

const MAX_DEPTH = 64;
const MAX_ISSUES = 200;

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function asSchema(v: unknown): Schema | undefined {
  return isObject(v) ? v : undefined;
}

export function resolveRef(root: Schema, ref: string): Schema | undefined {
  if (!ref.startsWith('#/')) return undefined;
  let cur: unknown = root;
  for (const raw of ref.slice(2).split('/')) {
    const key = raw.replace(/~1/g, '/').replace(/~0/g, '~');
    if (!isObject(cur)) return undefined;
    cur = cur[key];
  }
  return asSchema(cur);
}

export function deref(root: Schema, schema: Schema | undefined, seen = 0): Schema | undefined {
  if (!schema) return undefined;
  const ref = schema.$ref;
  if (typeof ref === 'string' && seen < 32) {
    const target = resolveRef(root, ref);
    if (!target) return undefined;
    const rest = Object.fromEntries(Object.entries(schema).filter(([k]) => k !== '$ref'));
    const resolved = deref(root, target, seen + 1);
    return Object.keys(rest).length ? { ...resolved, ...rest } : resolved;
  }
  return schema;
}

function schemaList(v: unknown): Schema[] {
  return Array.isArray(v) ? v.filter(isObject) : [];
}

function typeOf(v: unknown): string {
  if (v === null) return 'null';
  if (Array.isArray(v)) return 'array';
  if (typeof v === 'number') return Number.isInteger(v) ? 'integer' : 'number';
  return typeof v;
}

function typeMatches(expected: string, actual: string): boolean {
  if (expected === actual) return true;
  if (expected === 'number' && actual === 'integer') return true;
  return false;
}

function describeTypes(t: string[]): string {
  return t.length === 1 ? (t[0] ?? '') : t.join(' or ');
}

function short(v: unknown): string {
  const s = JSON.stringify(v);
  return s.length > 40 ? `${s.slice(0, 37)}...` : s;
}

export function validate(value: unknown, ctx: SchemaContext): SchemaIssue[] {
  const issues: SchemaIssue[] = [];
  walk(value, ctx.schema, ctx.root, [], issues, 0);
  return issues.slice(0, MAX_ISSUES);
}

function walk(value: unknown, raw: Schema | undefined, root: Schema, path: JsonPath, out: SchemaIssue[], depth: number): void {
  if (depth > MAX_DEPTH || out.length >= MAX_ISSUES) return;
  const schema = deref(root, raw);
  if (!schema) return;

  for (const sub of schemaList(schema.allOf)) walk(value, sub, root, path, out, depth + 1);

  const variants = schemaList(schema.oneOf).length ? schemaList(schema.oneOf) : schemaList(schema.anyOf);
  if (variants.length) {
    const results = variants.map((v) => {
      const sub: SchemaIssue[] = [];
      walk(value, v, root, path, sub, depth + 1);
      return sub;
    });
    const ok = results.filter((r) => r.length === 0).length;
    if (ok === 0) {
      const best = results.reduce((a, b) => (score(b, path) < score(a, path) ? b : a));
      const typeOnly = best.every((i) => i.kind === 'type' && i.path.length === path.length);
      if (typeOnly) {
        const types = variants.map((v) => expectedTypes(deref(root, v))).flat();
        out.push({
          path,
          kind: 'type',
          message: types.length ? `expected ${describeTypes([...new Set(types)])}, got ${typeOf(value)}` : 'value does not match any allowed shape',
        });
      } else {
        out.push(...best);
      }
    }
  }

  const actual = typeOf(value);
  const types = expectedTypes(schema);
  if (schema.nullable === true && value === null) return;
  if (types.length && !types.some((t) => typeMatches(t, actual))) {
    out.push({ path, kind: 'type', message: `expected ${describeTypes(types)}, got ${actual}` });
    return;
  }

  if (Array.isArray(schema.enum) && !schema.enum.some((e) => JSON.stringify(e) === JSON.stringify(value))) {
    out.push({ path, kind: 'enum', message: `must be one of ${schema.enum.map(short).join(', ')}` });
  }
  if ('const' in schema && JSON.stringify(schema.const) !== JSON.stringify(value)) {
    out.push({ path, kind: 'enum', message: `must be ${short(schema.const)}` });
  }

  if (typeof value === 'string') {
    if (typeof schema.minLength === 'number' && value.length < schema.minLength)
      out.push({ path, kind: 'constraint', message: `must be at least ${schema.minLength} characters` });
    if (typeof schema.maxLength === 'number' && value.length > schema.maxLength)
      out.push({ path, kind: 'constraint', message: `must be at most ${schema.maxLength} characters` });
    if (typeof schema.pattern === 'string') {
      try {
        if (!new RegExp(schema.pattern, 'u').test(value))
          out.push({ path, kind: 'constraint', message: `must match ${schema.pattern}` });
      } catch {
        /* patterns that JavaScript cannot compile are checked by the server */
      }
    }
  }

  if (typeof value === 'number') {
    if (typeof schema.minimum === 'number' && value < schema.minimum)
      out.push({ path, kind: 'constraint', message: `must be >= ${schema.minimum}` });
    if (typeof schema.maximum === 'number' && value > schema.maximum)
      out.push({ path, kind: 'constraint', message: `must be <= ${schema.maximum}` });
    if (typeof schema.exclusiveMinimum === 'number' && value <= schema.exclusiveMinimum)
      out.push({ path, kind: 'constraint', message: `must be > ${schema.exclusiveMinimum}` });
    if (typeof schema.exclusiveMaximum === 'number' && value >= schema.exclusiveMaximum)
      out.push({ path, kind: 'constraint', message: `must be < ${schema.exclusiveMaximum}` });
  }

  if (Array.isArray(value)) {
    if (typeof schema.minItems === 'number' && value.length < schema.minItems)
      out.push({ path, kind: 'constraint', message: `must have at least ${schema.minItems} items` });
    if (typeof schema.maxItems === 'number' && value.length > schema.maxItems)
      out.push({ path, kind: 'constraint', message: `must have at most ${schema.maxItems} items` });
    const items = asSchema(schema.items);
    if (items) value.forEach((item, i) => walk(item, items, root, [...path, i], out, depth + 1));
  }

  if (isObject(value)) {
    const props = asSchema(schema.properties) ?? {};
    const required = Array.isArray(schema.required) ? schema.required.filter((r): r is string => typeof r === 'string') : [];
    for (const r of required) {
      if (!(r in value)) out.push({ path, kind: 'required', message: `missing required property "${r}"` });
    }
    const additional = schema.additionalProperties;
    for (const [k, v] of Object.entries(value)) {
      const ps = asSchema(props[k]);
      if (ps) {
        walk(v, ps, root, [...path, k], out, depth + 1);
      } else if (additional === false) {
        out.push({ path: [...path, k], kind: 'unknown', message: `unknown property "${k}"` });
      } else if (isObject(additional)) {
        walk(v, additional, root, [...path, k], out, depth + 1);
      }
    }
  }
}

function score(issues: SchemaIssue[], base: JsonPath): number {
  if (!issues.length) return 0;
  const typeAtBase = issues.some((i) => i.kind === 'type' && i.path.length === base.length);
  return (typeAtBase ? 1000 : 0) + issues.length;
}

export function expectedTypes(schema: Schema | undefined): string[] {
  if (!schema) return [];
  const t = schema.type;
  if (typeof t === 'string') return [t];
  if (Array.isArray(t)) return t.filter((x): x is string => typeof x === 'string');
  return [];
}

export interface PropertyInfo {
  name: string;
  schema: Schema;
  required: boolean;
  description?: string;
}

function collectProps(root: Schema, raw: Schema | undefined, acc: Map<string, PropertyInfo>, depth: number): void {
  if (depth > 16) return;
  const schema = deref(root, raw);
  if (!schema) return;
  const required = new Set(Array.isArray(schema.required) ? schema.required.filter((r): r is string => typeof r === 'string') : []);
  const props = asSchema(schema.properties);
  if (props) {
    for (const [name, ps] of Object.entries(props)) {
      const s = asSchema(ps);
      if (!s || acc.has(name)) continue;
      const resolved = deref(root, s) ?? s;
      const description = typeof s.description === 'string' ? s.description : typeof resolved.description === 'string' ? resolved.description : undefined;
      acc.set(name, { name, schema: s, required: required.has(name), ...(description ? { description } : {}) });
    }
  }
  for (const key of ['allOf', 'oneOf', 'anyOf'] as const) {
    for (const sub of schemaList(schema[key])) collectProps(root, sub, acc, depth + 1);
  }
}

export function propertiesOf(root: Schema, schema: Schema | undefined): PropertyInfo[] {
  const acc = new Map<string, PropertyInfo>();
  collectProps(root, schema, acc, 0);
  return [...acc.values()];
}

export function schemaAt(root: Schema, schema: Schema | undefined, path: JsonPath): Schema | undefined {
  let cur = deref(root, schema);
  for (const seg of path) {
    if (!cur) return undefined;
    if (typeof seg === 'number') {
      const items = asSchema(cur.items) ?? firstVariantWith(root, cur, 'items');
      cur = deref(root, items);
    } else {
      const prop = propertiesOf(root, cur).find((p) => p.name === seg);
      if (prop) cur = deref(root, prop.schema);
      else if (isObject(cur.additionalProperties)) cur = deref(root, cur.additionalProperties);
      else return undefined;
    }
  }
  return cur;
}

function firstVariantWith(root: Schema, schema: Schema, key: string): Schema | undefined {
  for (const k of ['oneOf', 'anyOf', 'allOf'] as const) {
    for (const v of schemaList(schema[k])) {
      const d = deref(root, v);
      const found = d ? asSchema(d[key]) : undefined;
      if (found) return found;
    }
  }
  return undefined;
}

export function enumValues(root: Schema, schema: Schema | undefined): unknown[] {
  const s = deref(root, schema);
  if (!s) return [];
  const out: unknown[] = [];
  if (Array.isArray(s.enum)) out.push(...(s.enum as unknown[]));
  if ('const' in s) out.push(s.const);
  for (const k of ['oneOf', 'anyOf'] as const) {
    for (const v of schemaList(s[k])) out.push(...enumValues(root, v));
  }
  if (expectedTypes(s).includes('boolean')) out.push(true, false);
  return [...new Map(out.map((v) => [JSON.stringify(v), v])).values()];
}

export function describeSchema(root: Schema, schema: Schema | undefined): string {
  const s = deref(root, schema);
  if (!s) return 'any';
  const t = expectedTypes(s);
  if (t.length) return describeTypes(t);
  const variants = [...schemaList(s.oneOf), ...schemaList(s.anyOf)];
  if (variants.length) {
    return [...new Set(variants.map((v) => describeSchema(root, v)))].join(' | ');
  }
  if (s.properties) return 'object';
  return 'any';
}

export function pathToString(path: JsonPath): string {
  if (!path.length) return '(root)';
  return path.map((p, i) => (typeof p === 'number' ? `[${p}]` : i === 0 ? p : `.${p}`)).join('');
}
