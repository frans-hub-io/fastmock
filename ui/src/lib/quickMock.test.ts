import { describe, expect, it } from 'vitest';
import {
  buildStubs,
  codeSample,
  DEFAULT_FORM,
  failureShare,
  formIssues,
  makeForm,
  needsPreflight,
  pathParams,
  PRESETS,
  QUICK_TAG,
  quickSample,
  type QuickForm,
} from './quickMock';

const CORS = { 'access-control-allow-origin': '*', 'access-control-expose-headers': '*' };

function main(f: QuickForm, space?: string | null) {
  const s = buildStubs(f, space)[0];
  if (!s) throw new Error('no main stub');
  return s.stub;
}

function roles(f: QuickForm) {
  return buildStubs(f).map((s) => s.role);
}

describe('quick mock: essentials', () => {
  it('maps the default form to one stub with a JSON body and CORS headers', () => {
    expect(buildStubs(DEFAULT_FORM)).toEqual([
      {
        role: 'main',
        title: 'Mock',
        stub: {
          name: 'GET /hello',
          request: { method: 'GET', urlPath: '/hello' },
          response: { status: 200, headers: CORS, jsonBody: { message: 'Hello from FastMock' } },
          tags: [QUICK_TAG],
        },
      },
    ]);
    expect(formIssues(DEFAULT_FORM)).toEqual([]);
  });

  it('uses a path template when the path has parameters', () => {
    const s = main(makeForm({ name: 'order', method: 'DELETE', path: '/orders/{id}/items/{item}', status: 204, bodyType: 'none' }));
    expect(s.name).toBe('order');
    expect(s.request).toEqual({ method: 'DELETE', urlPathTemplate: '/orders/{id}/items/{item}' });
    expect(s.response).toEqual({ status: 204, headers: CORS });
    expect(pathParams('/orders/{id}/items/{item}')).toEqual(['id', 'item']);
  });

  it('sends text bodies and response headers as given', () => {
    const s = main(makeForm({ bodyType: 'text', body: 'pong', headers: [{ name: 'content-type', value: 'text/plain' }, { name: ' ', value: '' }], b: { cors: { on: false } } }));
    expect(s.response).toEqual({ status: 200, headers: { 'content-type': 'text/plain' }, body: 'pong' });
  });

  it('puts the stub into the chosen space', () => {
    expect(main(DEFAULT_FORM, 'billing').space).toBe('billing');
    expect(main(DEFAULT_FORM, null)).not.toHaveProperty('space');
  });

  it('reports invalid essentials', () => {
    const fields = (f: QuickForm) => formIssues(f).map((i) => i.field);
    expect(fields(makeForm({ path: '' }))).toEqual(['path']);
    expect(fields(makeForm({ path: 'orders' }))).toEqual(['path']);
    expect(fields(makeForm({ path: '/orders?x=1' }))).toEqual(['path']);
    expect(fields(makeForm({ path: '/orders/{id' }))).toEqual(['path']);
    expect(fields(makeForm({ path: '/orders/{a-b}' }))).toEqual(['path']);
    expect(fields(makeForm({ status: 99 }))).toEqual(['status']);
    expect(fields(makeForm({ status: '' }))).toEqual(['status']);
    expect(fields(makeForm({ body: '{"a":' }))).toEqual(['body']);
    expect(fields(makeForm({ status: 204 }))).toEqual(['body']);
    expect(fields(makeForm({ headers: [{ name: 'x a', value: '1' }] }))).toEqual(['headers.0']);
    expect(fields(makeForm({ headers: [{ name: 'X-A', value: '1' }, { name: 'x-a', value: '2' }] }))).toEqual(['headers.1']);
  });
});

describe('quick mock: behaviours', () => {
  it('request matching: headers, query, JSONPath and body text', () => {
    const f = makeForm({
      method: 'POST',
      b: {
        conditions: {
          on: true,
          rows: [
            { source: 'header', name: 'X-Client', op: 'equalTo', value: 'web' },
            { source: 'query', name: 'debug', op: 'absent', value: '' },
            { source: 'query', name: 'page', op: 'matches', value: '[0-9]+' },
            { source: 'jsonPath', name: '$.user.id', op: 'contains', value: '7' },
            { source: 'jsonPath', name: '$.legacy', op: 'absent', value: '' },
            { source: 'body', name: '', op: 'contains', value: 'hello' },
          ],
        },
      },
    });
    expect(main(f).request).toEqual({
      method: 'POST',
      urlPath: '/hello',
      headers: { 'X-Client': { equalTo: 'web' } },
      query: { debug: { absent: true }, page: { matches: '[0-9]+' } },
      body: [{ jsonPath: '$.user.id', value: { contains: '7' } }, { jsonPath: '$.legacy', absent: true }, { contains: 'hello' }],
    });
    expect(formIssues(f)).toEqual([]);
    const off = makeForm({ b: { conditions: { on: false } } });
    expect(main(off).request).toEqual({ method: 'GET', urlPath: '/hello' });
  });

  it('request matching: flags missing names and bad regular expressions', () => {
    const f = makeForm({
      b: {
        conditions: {
          on: true,
          rows: [
            { source: 'header', name: '', op: 'equalTo', value: 'x' },
            { source: 'jsonPath', name: 'user', op: 'equalTo', value: 'x' },
            { source: 'query', name: 'q', op: 'matches', value: '(' },
            { source: 'query', name: 'q', op: 'contains', value: '' },
          ],
        },
      },
    });
    expect(formIssues(f).map((i) => i.field)).toEqual(['conditions.0', 'conditions.1', 'conditions.2', 'conditions.3']);
  });

  it('authentication: bearer token with a 401 fallback stub', () => {
    const f = makeForm({ name: 'me', b: { auth: { on: true, kind: 'bearer' }, cors: { on: false } } });
    const [m, fallback, ...rest] = buildStubs(f);
    expect(rest).toEqual([]);
    expect(m?.stub.request).toEqual({ method: 'GET', urlPath: '/hello', headers: { Authorization: { matches: 'Bearer .+' } } });
    expect(fallback).toEqual({
      role: 'auth',
      title: '401 fallback',
      stub: {
        name: 'me (401 without credentials)',
        priority: 6,
        request: { method: 'GET', urlPath: '/hello' },
        response: { status: 401, headers: { 'www-authenticate': 'Bearer' }, jsonBody: { error: 'unauthorized', message: 'send a bearer token' } },
        tags: [QUICK_TAG],
      },
    });
  });

  it('authentication: exact token, API key and basic auth', () => {
    const exact = makeForm({ b: { auth: { on: true, kind: 'bearerExact', token: 't0k' } } });
    expect(main(exact).request.headers).toEqual({ Authorization: { equalTo: 'Bearer t0k' } });
    const key = makeForm({ b: { auth: { on: true, kind: 'apiKey', header: 'X-Key', key: 'k1' } } });
    expect(main(key).request.headers).toEqual({ 'X-Key': { equalTo: 'k1' } });
    expect(buildStubs(key)[1]?.stub.response).toEqual({ status: 401, headers: CORS, jsonBody: { error: 'unauthorized', message: 'send the X-Key header' } });
    const basic = makeForm({ b: { auth: { on: true, kind: 'basic', username: 'u', password: 'p' } } });
    expect(main(basic).request.basicAuth).toEqual({ username: 'u', password: 'p' });
    expect(buildStubs(basic)[1]?.stub.response?.headers?.['www-authenticate']).toBe('Basic realm="fastmock"');
    expect(formIssues(makeForm({ b: { auth: { on: true, kind: 'bearerExact', token: ' ' } } })).map((i) => i.field)).toEqual(['auth']);
    expect(formIssues(makeForm({ b: { auth: { on: true, kind: 'apiKey', header: 'bad header', key: 'k' } } })).map((i) => i.field)).toEqual(['auth']);
    expect(formIssues(makeForm({ b: { auth: { on: true, kind: 'basic', username: '' } } })).map((i) => i.field)).toEqual(['auth']);
  });

  it('authentication: the fallback keeps the other conditions', () => {
    const f = makeForm({ b: { auth: { on: true }, conditions: { on: true, rows: [{ source: 'query', name: 'v', op: 'equalTo', value: '2' }] } } });
    expect(buildStubs(f)[1]?.stub.request).toEqual({ method: 'GET', urlPath: '/hello', query: { v: { equalTo: '2' } } });
  });

  it('delay: fixed, random range and realistic', () => {
    expect(main(makeForm({ b: { delay: { on: true, kind: 'fixed', fixedMs: 250 } } })).response?.delay).toEqual({ fixedMs: 250 });
    expect(main(makeForm({ b: { delay: { on: true, kind: 'range', minMs: 10, maxMs: 90 } } })).response?.delay).toEqual({ minMs: 10, maxMs: 90 });
    expect(main(makeForm({ b: { delay: { on: true, kind: 'realistic', medianMs: 120, sigma: 0.5 } } })).response?.delay).toEqual({ lognormal: { medianMs: 120, sigma: 0.5 } });
    expect(main(DEFAULT_FORM).response).not.toHaveProperty('delay');
    expect(formIssues(makeForm({ b: { delay: { on: true, kind: 'range', minMs: 90, maxMs: 10 } } })).map((i) => i.field)).toEqual(['delay']);
    expect(formIssues(makeForm({ b: { delay: { on: true, kind: 'fixed', fixedMs: '' } } })).map((i) => i.field)).toEqual(['delay']);
  });

  it('network fault on every call', () => {
    expect(main(makeForm({ b: { fault: { on: true, fault: 'EMPTY_RESPONSE' } } })).response?.fault).toBe('EMPTY_RESPONSE');
  });

  it('random failures become random responses in the right proportion', () => {
    expect(failureShare(30)).toEqual({ failures: 3, total: 10 });
    expect(failureShare(50)).toEqual({ failures: 1, total: 2 });
    expect(failureShare(20)).toEqual({ failures: 1, total: 5 });
    const f = makeForm({ b: { flaky: { on: true, percent: 20, failWith: 'status', status: 503, body: '{"error":"down"}' }, cors: { on: false } } });
    const s = main(f);
    expect(s).not.toHaveProperty('response');
    expect(s.responseMode).toBe('random');
    expect(s.responses).toEqual([
      { status: 503, jsonBody: { error: 'down' } },
      { status: 200, jsonBody: { message: 'Hello from FastMock' } },
      { status: 200, jsonBody: { message: 'Hello from FastMock' } },
      { status: 200, jsonBody: { message: 'Hello from FastMock' } },
      { status: 200, jsonBody: { message: 'Hello from FastMock' } },
    ]);
    const fault = main(makeForm({ b: { flaky: { on: true, percent: 50, failWith: 'fault', fault: 'CONNECTION_RESET' }, delay: { on: true, kind: 'fixed', fixedMs: 5 } } }));
    expect(fault.responses?.[0]).toEqual({ fault: 'CONNECTION_RESET', delay: { fixedMs: 5 } });
    expect(fault.responses?.[1]?.delay).toEqual({ fixedMs: 5 });
    expect(formIssues(makeForm({ b: { flaky: { on: true, status: 700 } } })).map((i) => i.field)).toEqual(['flaky']);
  });

  it('dynamic values: templating is on by default and can be switched off everywhere', () => {
    expect(main(DEFAULT_FORM).response).not.toHaveProperty('templated');
    const off = makeForm({ b: { templating: { on: false }, flaky: { on: true, percent: 50 } } });
    expect(main(off).responses?.every((r) => r.templated === false)).toBe(true);
  });

  it('response sequence: the essentials answer first, then the steps', () => {
    const f = makeForm({ status: 202, body: '{"status":"PENDING"}', b: { sequence: { on: true, mode: 'cycle', steps: [{ status: 200, body: '{"status":"DONE"}' }, { status: 410, body: 'gone' }] }, cors: { on: false } } });
    const s = main(f);
    expect(s).not.toHaveProperty('response');
    expect(s.responseMode).toBe('cycle');
    expect(s.responses).toEqual([
      { status: 202, jsonBody: { status: 'PENDING' } },
      { status: 200, jsonBody: { status: 'DONE' } },
      { status: 410, body: 'gone' },
    ]);
    expect(formIssues(makeForm({ b: { sequence: { on: true, steps: [] } } })).map((i) => i.field)).toEqual(['sequence']);
    expect(formIssues(makeForm({ b: { sequence: { on: true, steps: [{ status: 1, body: '' }] } } })).map((i) => i.field)).toEqual(['sequence.0']);
  });

  it('conflicting behaviours are reported', () => {
    const f = makeForm({ b: { sequence: { on: true }, flaky: { on: true }, fault: { on: true } } });
    expect(formIssues(f).map((i) => i.field)).toEqual(['fault', 'fault', 'flaky']);
  });

  it('stateful scenario', () => {
    expect(main(makeForm({ b: { scenario: { on: true, name: ' order ', requiredState: 'Started', newState: 'PAID' } } })).scenario).toEqual({ name: 'order', requiredState: 'Started', newState: 'PAID' });
    expect(main(makeForm({ b: { scenario: { on: true, name: 'x', requiredState: '', newState: '' } } })).scenario).toEqual({ name: 'x' });
    expect(formIssues(makeForm({ b: { scenario: { on: true, name: '' } } })).map((i) => i.field)).toEqual(['scenario']);
  });

  it('webhook callback', () => {
    const f = makeForm({ b: { callback: { on: true, url: 'https://hooks.example.com/a', method: 'PUT', body: '{"id":"{{uuid}}"}', delayMs: 500, maxAttempts: 5 } } });
    expect(main(f).callbacks).toEqual([{ url: 'https://hooks.example.com/a', method: 'PUT', jsonBody: { id: '{{uuid}}' }, delay: { fixedMs: 500 }, maxAttempts: 5 }]);
    const text = makeForm({ b: { callback: { on: true, body: 'ping', delayMs: 0 } } });
    expect(main(text).callbacks?.[0]).toEqual({ url: 'https://webhook.example.com/events', method: 'POST', body: 'ping', maxAttempts: 3 });
    expect(formIssues(makeForm({ b: { callback: { on: true, url: 'ftp://x' } } })).map((i) => i.field)).toEqual(['callback']);
    expect(formIssues(makeForm({ b: { callback: { on: true, url: '{{request.json.callbackUrl}}' } } }))).toEqual([]);
    expect(formIssues(makeForm({ b: { callback: { on: true, maxAttempts: 51 } } })).map((i) => i.field)).toEqual(['callback']);
  });

  it('CORS: headers on every response, a preflight stub only when the browser sends one', () => {
    expect(needsPreflight(DEFAULT_FORM)).toBe(false);
    expect(roles(DEFAULT_FORM)).toEqual(['main']);
    const put = makeForm({ method: 'PUT', path: '/orders/{id}', b: { auth: { on: true, kind: 'apiKey', header: 'X-Key', key: 'k' }, cors: { on: true, origin: 'https://app.example.com' } } });
    expect(roles(put)).toEqual(['main', 'auth', 'preflight']);
    expect(buildStubs(put)[2]?.stub).toEqual({
      name: 'PUT /orders/{id} (CORS preflight)',
      request: { method: 'OPTIONS', urlPathTemplate: '/orders/{id}' },
      response: {
        status: 204,
        headers: {
          'access-control-allow-origin': 'https://app.example.com',
          'access-control-allow-methods': 'PUT, OPTIONS',
          'access-control-max-age': '600',
          'access-control-allow-headers': 'x-key, content-type',
        },
      },
      tags: [QUICK_TAG],
    });
    expect(main(put).response?.headers?.['access-control-allow-origin']).toBe('https://app.example.com');
    const noCors = makeForm({ method: 'PUT', b: { cors: { on: false } } });
    expect(roles(noCors)).toEqual(['main']);
    expect(main(noCors).response).not.toHaveProperty('headers');
  });

  it('temporary mock: number of calls and lifetime', () => {
    const s = main(makeForm({ b: { limits: { on: true, times: 3, ttlMinutes: 10 } } }));
    expect(s.times).toBe(3);
    expect(s.ttlSeconds).toBe(600);
    expect(main(makeForm({ b: { limits: { on: true, times: '', ttlMinutes: 5 } } }))).not.toHaveProperty('times');
    expect(formIssues(makeForm({ b: { limits: { on: true, times: '', ttlMinutes: '' } } })).map((i) => i.field)).toEqual(['limits']);
    expect(formIssues(makeForm({ b: { limits: { on: true, times: 0 } } })).map((i) => i.field)).toEqual(['limits']);
  });
});

describe('quick mock: presets', () => {
  it.each(PRESETS.map((p) => [p.label, p] as const))('%s is valid', (_label, p) => {
    expect(formIssues(p.form)).toEqual([]);
    expect(buildStubs(p.form).length).toBeGreaterThan(0);
  });

  it('protected resource adds the 401 fallback', () => {
    const p = PRESETS.find((x) => x.id === 'protected');
    expect(p && roles(p.form)).toEqual(['main', 'auth', 'preflight']);
  });

  it('webhook after create adds a callback and a preflight for the JSON body', () => {
    const p = PRESETS.find((x) => x.id === 'webhook');
    expect(p && main(p.form).callbacks?.[0]?.url).toBe('https://webhook.example.com/orders');
    expect(p && roles(p.form)).toEqual(['main', 'preflight']);
  });
});

describe('quick mock: trying it', () => {
  it('builds a request the mock answers', () => {
    const f = makeForm({
      method: 'POST',
      path: '/users/{userId}/greet/{name}',
      b: {
        auth: { on: true, kind: 'basic', username: 'ann', password: 'pw' },
        conditions: {
          on: true,
          rows: [
            { source: 'header', name: 'X-Client', op: 'equalTo', value: 'web' },
            { source: 'query', name: 'lang', op: 'matches', value: 'en|uk' },
            { source: 'query', name: 'debug', op: 'absent', value: '' },
            { source: 'jsonPath', name: '$.user.name', op: 'equalTo', value: 'Ann' },
          ],
        },
      },
    });
    expect(quickSample(f)).toEqual({
      method: 'POST',
      path: '/users/42/greet/world',
      query: 'lang=example',
      headers: { 'X-Client': 'web', Authorization: `Basic ${btoa('ann:pw')}`, 'content-type': 'application/json' },
      body: JSON.stringify({ user: { name: 'Ann' } }, null, 2),
    });
    expect(quickSample(makeForm({ b: { auth: { on: true, kind: 'bearer' } } })).headers).toEqual({ Authorization: 'Bearer demo-token' });
    expect(quickSample(DEFAULT_FORM)).toEqual({ method: 'GET', path: '/hello', query: '', headers: {}, body: '' });
  });

  it('writes code for every language', () => {
    const req = { method: 'POST', path: '/orders', query: 'a=1', headers: { 'content-type': 'application/json' }, body: '{"name":"Ölga 😀"}' };
    const base = 'http://127.0.0.1:8080';
    expect(codeSample('curl', base, req)).toBe(`curl -i -X POST -H 'content-type: application/json' --data-raw '{"name":"Ölga 😀"}' 'http://127.0.0.1:8080/orders?a=1'`);
    expect(codeSample('fetch', base, req)).toBe(
      [
        'const response = await fetch("http://127.0.0.1:8080/orders?a=1", {',
        '  method: "POST",',
        '  headers: {',
        '    "content-type": "application/json",',
        '  },',
        '  body: "{\\"name\\":\\"\\u00d6lga \\ud83d\\ude00\\"}",',
        '});',
        'console.log(response.status, await response.text());',
      ].join('\n'),
    );
    const py = codeSample('python', base, req);
    expect(py).toContain('import requests');
    expect(py).toContain('    data="{\\"name\\":\\"\\u00d6lga \\U0001f600\\"}",');
    const java = codeSample('java', base, req);
    expect(java).toContain('.header("content-type", "application/json")');
    expect(java).toContain('.method("POST", HttpRequest.BodyPublishers.ofString(');
    expect(codeSample('java', base, { ...req, method: 'GET', body: '' })).toContain('.method("GET", HttpRequest.BodyPublishers.noBody())');
    expect(codeSample('fetch', base, { ...req, method: 'GET', headers: {}, body: '' })).not.toContain('body:');
  });
});
