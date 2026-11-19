import { describe, expect, it } from 'vitest';
import { describeSchema, enumValues, pathToString, propertiesOf, schemaAt, validate, type Schema } from './jsonSchema';

const root: Schema = {
  components: {
    schemas: {
      Stub: {
        type: 'object',
        required: ['request'],
        additionalProperties: false,
        properties: {
          name: { type: ['string', 'null'], description: 'Display name' },
          priority: { type: 'integer', minimum: -10, maximum: 10 },
          request: { $ref: '#/components/schemas/RequestPattern' },
          response: { $ref: '#/components/schemas/ResponseDef' },
          tags: { type: 'array', items: { type: 'string' } },
          scenario: { oneOf: [{ type: 'null' }, { $ref: '#/components/schemas/ScenarioRule' }] },
        },
      },
      RequestPattern: {
        type: 'object',
        additionalProperties: false,
        properties: {
          method: { type: ['string', 'null'] },
          urlPath: { type: ['string', 'null'] },
          headers: { type: 'object', additionalProperties: { $ref: '#/components/schemas/StringMatcher' } },
        },
      },
      StringMatcher: {
        type: 'object',
        additionalProperties: false,
        properties: { equalTo: { type: ['string', 'null'] }, caseInsensitive: { type: 'boolean' } },
      },
      ResponseDef: {
        type: 'object',
        additionalProperties: false,
        properties: {
          status: { type: 'integer', minimum: 100, maximum: 599 },
          fault: { oneOf: [{ type: 'null' }, { $ref: '#/components/schemas/Fault' }] },
        },
      },
      Fault: { type: 'string', enum: ['CONNECTION_RESET', 'EMPTY_RESPONSE'] },
      ScenarioRule: { type: 'object', required: ['name'], properties: { name: { type: 'string' } } },
    },
  },
};
const ctx = { root, schema: { $ref: '#/components/schemas/Stub' } };

describe('validate', () => {
  it('accepts a valid stub', () => {
    expect(validate({ request: { method: 'GET', urlPath: '/a', headers: { accept: { equalTo: 'x' } } }, response: { status: 200 } }, ctx)).toEqual([]);
  });

  it('reports missing required properties at the object', () => {
    const issues = validate({}, ctx);
    expect(issues).toEqual([{ path: [], kind: 'required', message: 'missing required property "request"' }]);
  });

  it('reports unknown properties with their path', () => {
    const issues = validate({ request: { urlPth: '/a' } }, ctx);
    expect(issues).toHaveLength(1);
    expect(issues[0]?.path).toEqual(['request', 'urlPth']);
    expect(issues[0]?.kind).toBe('unknown');
  });

  it('checks types, ranges and nested additionalProperties schemas', () => {
    const issues = validate({ request: { headers: { a: { equalTo: 5 } } }, response: { status: 700 }, priority: 'high' }, ctx);
    const byPath = Object.fromEntries(issues.map((i) => [pathToString(i.path), i.message]));
    expect(byPath['request.headers.a.equalTo']).toBe('expected string or null, got integer');
    expect(byPath['response.status']).toBe('must be <= 599');
    expect(byPath.priority).toBe('expected integer, got string');
  });

  it('validates enums behind oneOf with null', () => {
    expect(validate({ request: {}, response: { fault: 'CONNECTION_RESET' } }, ctx)).toEqual([]);
    expect(validate({ request: {}, response: { fault: null } }, ctx)).toEqual([]);
    const issues = validate({ request: {}, response: { fault: 'NOPE' } }, ctx);
    expect(issues).toHaveLength(1);
    expect(issues[0]?.path).toEqual(['response', 'fault']);
  });

  it('validates array items with indexes', () => {
    const issues = validate({ request: {}, tags: ['a', 3] }, ctx);
    expect(issues[0]?.path).toEqual(['tags', 1]);
  });

  it('stops at a bounded depth on self-referencing schemas', () => {
    const loop: Schema = { components: { schemas: { N: { type: 'object', properties: { next: { $ref: '#/components/schemas/N' } } } } } };
    let v: Record<string, unknown> = {};
    const top = v;
    for (let i = 0; i < 500; i++) {
      const n: Record<string, unknown> = {};
      v.next = n;
      v = n;
    }
    expect(() => validate(top, { root: loop, schema: { $ref: '#/components/schemas/N' } })).not.toThrow();
  });
});

describe('schema navigation', () => {
  it('lists properties with requiredness and descriptions', () => {
    const props = propertiesOf(root, ctx.schema);
    expect(props.map((p) => p.name)).toContain('request');
    expect(props.find((p) => p.name === 'request')?.required).toBe(true);
    expect(props.find((p) => p.name === 'name')?.description).toBe('Display name');
  });

  it('resolves nested paths through refs, oneOf and additionalProperties', () => {
    expect(propertiesOf(root, schemaAt(root, ctx.schema, ['request'])).map((p) => p.name)).toEqual(['method', 'urlPath', 'headers']);
    expect(propertiesOf(root, schemaAt(root, ctx.schema, ['request', 'headers', 'x-any'])).map((p) => p.name)).toEqual(['equalTo', 'caseInsensitive']);
    expect(propertiesOf(root, schemaAt(root, ctx.schema, ['scenario'])).map((p) => p.name)).toEqual(['name']);
  });

  it('offers enum values and describes types', () => {
    expect(enumValues(root, schemaAt(root, ctx.schema, ['response', 'fault']))).toEqual(['CONNECTION_RESET', 'EMPTY_RESPONSE']);
    expect(describeSchema(root, schemaAt(root, ctx.schema, ['priority']))).toBe('integer');
  });
});
