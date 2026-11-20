import { describe, expect, it } from 'vitest';
import type { JournalEntry, Stub } from '../api/types';
import { parseHeaderLines, shellQuote, toCurl } from './curl';
import { diffLines, jsonDiff } from './diff';
import { formatDuration, formatRelative, prettyBody } from './format';
import { mapToRows, matcherToRow, rowsToMap } from './matchers';
import { detectImportKind, duplicateStub, sampleRequest, setUrl, stubFromJournal, stubTitle, urlKind } from './stubs';

describe('stubs helpers', () => {
  it('builds a sample request from a path template, query and headers', () => {
    const stub: Stub = {
      request: {
        method: 'POST',
        urlPathTemplate: '/orders/{orderId}/items/{item}',
        pathParams: { item: { equalTo: 'abc' } },
        query: { tenant: { equalTo: 'a' } },
        headers: { 'x-api-key': {} },
        body: [{ equalToJson: { a: 1 } }],
      },
    };
    const r = sampleRequest(stub);
    expect(r.method).toBe('POST');
    expect(r.path).toBe('/orders/1/items/abc');
    expect(r.query).toBe('tenant=a');
    expect(r.headers['x-api-key']).toBe('x');
    expect(JSON.parse(r.body)).toEqual({ a: 1 });
    expect(r.headers['content-type']).toBe('application/json');
  });

  it('splits url matchers into path and query', () => {
    const r = sampleRequest({ request: { url: '/a?b=1' } });
    expect(r).toMatchObject({ method: 'GET', path: '/a', query: 'b=1' });
  });

  it('switches the URL matcher kind and keeps exactly one', () => {
    const next = setUrl({ urlPath: '/a', method: 'GET' }, 'urlPathPattern', '/a/.*');
    expect(next).toEqual({ method: 'GET', urlPathPattern: '/a/.*' });
    expect(urlKind(next)).toBe('urlPathPattern');
    expect(setUrl(next, 'any', '')).toEqual({ method: 'GET' });
  });

  it('turns a journal entry into a stub', () => {
    const e: JournalEntry = {
      id: 1,
      at: '2026-01-01T00:00:00Z',
      source: 'stubs',
      method: 'post',
      path: '/pay',
      query: 'x=1',
      headers: { 'content-type': 'application/json; charset=utf-8', authorization: '***' },
      body: '{"amount":5}',
      status: 404,
      responseBody: 'nope',
      matched: false,
      durationMs: 1,
    };
    const s = stubFromJournal(e);
    expect(s.request).toEqual({
      method: 'POST',
      url: '/pay?x=1',
      headers: { 'content-type': { contains: 'application/json' } },
      body: [{ equalToJson: { amount: 5 }, ignoreExtraElements: true }],
    });
    expect(s.response).toEqual({ status: 200 });
  });

  it('duplicates without id and timestamps', () => {
    const d = duplicateStub({ id: 'x', updatedAt: 'y', name: 'n', request: {} });
    expect(d.id).toBeUndefined();
    expect(d.updatedAt).toBeUndefined();
    expect(d.name).toBe('n (copy)');
    expect(stubTitle({ request: { method: 'get', urlPath: '/z' } })).toBe('GET /z');
  });

  it('detects import formats', () => {
    expect(detectImportKind('{"openapi":"3.0.0"}').kind).toBe('openapi');
    expect(detectImportKind('openapi: 3.1.0\ninfo: {}').kind).toBe('openapi');
    expect(detectImportKind('{"mappings":[]}').kind).toBe('wiremock');
    expect(detectImportKind('{"stubs":[]}').kind).toBe('fastmock');
    expect(detectImportKind('{"log":{"entries":[]}}').kind).toBe('har');
    expect(detectImportKind('{"asyncapi":"2.6.0","channels":{}}').kind).toBe('asyncapi');
    expect(detectImportKind('asyncapi: 3.0.0\nchannels: {}').kind).toBe('asyncapi');
    expect(detectImportKind('<definitions xmlns="http://schemas.xmlsoap.org/wsdl/"/>').kind).toBe('wsdl');
    expect(detectImportKind('<?xml version="1.0"?><con:soapui-project name="x"/>').kind).toBe('soapui');
    expect(detectImportKind('type Query { a: Int }').kind).toBe('graphql');
    expect(detectImportKind('apiVersion: mocks.microcks.io/v1alpha1\nkind: APIExamples').kind).toBe('microcks');
    expect(detectImportKind('{"info":{"schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"}}').kind).toBe('postman');
    expect(detectImportKind('[{"httpRequest":{}}]').kind).toBe('mockserver');
    expect(detectImportKind('<definitions/>').kind).toBe('wsdl');
    expect(detectImportKind('<root/>').kind).toBe('unknown');
  });
});

describe('matcher rows', () => {
  it('round-trips simple matchers', () => {
    const m = { a: { equalTo: 'x', caseInsensitive: true }, b: { matches: 'y.*' }, c: {}, d: { absent: true } };
    expect(rowsToMap(mapToRows(m))).toEqual(m);
  });

  it('keeps complex matchers untouched', () => {
    const complex = { or: [{ equalTo: 'a' }, { equalTo: 'b' }] };
    const row = matcherToRow('h', complex);
    expect(row.op).toBe('complex');
    expect(rowsToMap([row])).toEqual({ h: complex });
  });

  it('drops rows without a key', () => {
    expect(rowsToMap([{ key: ' ', op: 'equalTo', value: 'x', caseInsensitive: false }])).toBeUndefined();
  });
});

describe('curl', () => {
  it('quotes for POSIX shells', () => {
    expect(shellQuote("it's")).toBe(`'it'\\''s'`);
    expect(shellQuote('plain')).toBe('plain');
  });

  it('builds a command', () => {
    expect(toCurl('http://h:8080/', { method: 'POST', path: '/a', query: 'q=1', headers: { 'content-type': 'application/json' }, body: '{"a":1}' })).toBe(
      `curl -i -X POST -H 'content-type: application/json' --data-raw '{"a":1}' 'http://h:8080/a?q=1'`,
    );
    expect(parseHeaderLines('A: 1\nbad\nB:  two ')).toEqual({ A: '1', B: 'two' });
  });
});

describe('diff', () => {
  it('finds added and removed lines', () => {
    expect(diffLines('a\nb\nc', 'a\nc\nd')).toEqual([
      { kind: 'same', text: 'a' },
      { kind: 'del', text: 'b' },
      { kind: 'same', text: 'c' },
      { kind: 'add', text: 'd' },
    ]);
  });

  it('ignores key order in JSON', () => {
    expect(jsonDiff({ a: 1, b: 2 }, { b: 2, a: 1 }).every((o) => o.kind === 'same')).toBe(true);
  });
});

describe('format', () => {
  it('pretty prints JSON and XML bodies', () => {
    expect(prettyBody('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(prettyBody('<a><b>1</b></a>', 'application/xml')).toBe('<a>\n  <b>1</b>\n</a>');
    expect(prettyBody('plain text')).toBe('plain text');
  });

  it('formats durations and relative times', () => {
    expect(formatDuration(3725)).toBe('1h 2m');
    expect(formatDuration(59)).toBe('59s');
    expect(formatRelative(Date.now() - 120_000)).toBe('2m ago');
    const now = Date.now();
    expect(formatRelative(now + 7 * 86_400_000 - 3_000, now)).toBe('7d from now');
    expect(formatRelative(now + 86_400_000 - 1_000, now)).toBe('1d from now');
    expect(formatRelative(now - 3_599_000, now)).toBe('1h ago');
    expect(formatRelative(now - 90 * 60_000, now)).toBe('2h ago');
    expect(formatRelative(now + 6.4 * 86_400_000, now)).toBe('6d from now');
  });
});
