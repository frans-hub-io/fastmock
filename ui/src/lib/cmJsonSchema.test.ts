import { CompletionContext } from '@codemirror/autocomplete';
import { json } from '@codemirror/lang-json';
import { ensureSyntaxTree } from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { describe, expect, it } from 'vitest';
import { findNode, schemaCompletion, schemaDiagnostics } from './cmJsonSchema';
import type { Schema } from './jsonSchema';

const root: Schema = {
  components: {
    schemas: {
      Stub: {
        type: 'object',
        required: ['request'],
        additionalProperties: false,
        properties: {
          name: { type: 'string', description: 'Display name' },
          request: { type: 'object', additionalProperties: false, properties: { method: { type: 'string' }, urlPath: { type: 'string' } } },
          enabled: { type: 'boolean' },
        },
      },
    },
  },
};
const ctx = { root, schema: { $ref: '#/components/schemas/Stub' } };

function state(doc: string) {
  const s = EditorState.create({ doc, extensions: [json()] });
  ensureSyntaxTree(s, s.doc.length, 5000);
  return s;
}

describe('schema diagnostics', () => {
  it('points at the unknown key', () => {
    const doc = '{"request": {"urlPth": "/a"}}';
    const d = schemaDiagnostics(state(doc), ctx);
    expect(d).toHaveLength(1);
    expect(doc.slice(d[0]?.from, d[0]?.to)).toBe('"urlPth"');
  });

  it('points at a wrongly typed value', () => {
    const doc = '{"request": {}, "enabled": "yes"}';
    const d = schemaDiagnostics(state(doc), ctx);
    expect(d).toHaveLength(1);
    expect(doc.slice(d[0]?.from, d[0]?.to)).toBe('"yes"');
    expect(d[0]?.message).toBe('expected boolean, got string');
  });

  it('leaves syntax errors to the JSON parser', () => {
    expect(schemaDiagnostics(state('{"request": '), ctx)).toEqual([]);
  });

  it('finds nodes by path', () => {
    const s = state('{"a": [1, {"b": true}]}');
    const hit = findNode(s, ['a', 1, 'b']);
    expect(hit && s.doc.sliceString(hit.node.from, hit.node.to)).toBe('true');
  });
});

describe('schema completion', () => {
  it('suggests missing properties of the current object', () => {
    const doc = '{"request": {"method": "GET", }}';
    const pos = doc.indexOf(', }') + 2;
    const s = state(doc);
    const result = schemaCompletion(() => ctx)(new CompletionContext(s, pos, true));
    expect(result).not.toBeNull();
    const labels = result && 'options' in result ? result.options.map((o) => o.label) : [];
    expect(labels).toEqual(['urlPath']);
  });

  it('suggests top-level properties with required first', () => {
    const doc = '{ }';
    const s = state(doc);
    const result = schemaCompletion(() => ctx)(new CompletionContext(s, 2, true));
    const opts = result && 'options' in result ? result.options : [];
    expect(opts.map((o) => o.label).sort()).toEqual(['enabled', 'name', 'request']);
    expect(opts.find((o) => o.label === 'request')?.boost).toBe(2);
  });

  it('suggests booleans for boolean values', () => {
    const doc = '{"enabled": }';
    const s = state(doc);
    const result = schemaCompletion(() => ctx)(new CompletionContext(s, doc.indexOf('}'), true));
    const labels = result && 'options' in result ? result.options.map((o) => o.label) : [];
    expect(labels).toEqual(['true', 'false']);
  });
});
