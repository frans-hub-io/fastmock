import type { BodyMatcher, Delay, Fault, RequestPattern, ResponseDef, StringMatcher, Stub } from '../api/types';
import { joinUrl, toCurl, type SampleRequest } from './curl';

/** A number field of the form; empty while the user clears the input. */
export type Num = number | '';

export const QUICK_METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE'] as const;
export const STATUS_CHIPS = [200, 201, 204, 400, 404, 500] as const;
/** Every stub the quick mock page creates carries this tag, so they are easy to find and remove together. */
export const QUICK_TAG = 'quick-mock';

export type BodyType = 'json' | 'text' | 'none';
export type ConditionSource = 'header' | 'query' | 'jsonPath' | 'body';
export type ConditionOp = 'equalTo' | 'contains' | 'matches' | 'absent';
export type AuthKind = 'bearer' | 'bearerExact' | 'apiKey' | 'basic';
export type DelayKind = 'fixed' | 'range' | 'realistic';
export type FailWith = 'status' | 'fault';
export type SequenceMode = 'sequential' | 'cycle';

export interface HeaderRow {
  name: string;
  value: string;
}

export interface ConditionRow {
  source: ConditionSource;
  name: string;
  op: ConditionOp;
  value: string;
}

export interface SequenceStep {
  status: Num;
  body: string;
}

export interface Behaviours {
  conditions: { on: boolean; rows: ConditionRow[] };
  auth: { on: boolean; kind: AuthKind; token: string; header: string; key: string; username: string; password: string };
  delay: { on: boolean; kind: DelayKind; fixedMs: Num; minMs: Num; maxMs: Num; medianMs: Num; sigma: Num };
  fault: { on: boolean; fault: Fault };
  flaky: { on: boolean; percent: number; failWith: FailWith; status: Num; body: string; fault: Fault };
  templating: { on: boolean };
  sequence: { on: boolean; mode: SequenceMode; steps: SequenceStep[] };
  scenario: { on: boolean; name: string; requiredState: string; newState: string };
  callback: { on: boolean; url: string; method: string; body: string; delayMs: Num; maxAttempts: Num };
  cors: { on: boolean; origin: string };
  limits: { on: boolean; times: Num; ttlMinutes: Num };
}

export type BehaviourKey = keyof Behaviours;

export interface QuickForm {
  name: string;
  method: string;
  path: string;
  status: Num;
  bodyType: BodyType;
  body: string;
  headers: HeaderRow[];
  b: Behaviours;
}

/** Behaviours that write the same part of the stub and cannot be combined. */
export const CONFLICTS: Partial<Record<BehaviourKey, BehaviourKey[]>> = {
  fault: ['flaky', 'sequence'],
  flaky: ['fault', 'sequence'],
  sequence: ['fault', 'flaky'],
};

export const DEFAULT_BEHAVIOURS: Behaviours = {
  conditions: { on: false, rows: [{ source: 'header', name: 'x-client', op: 'equalTo', value: 'web' }] },
  auth: { on: false, kind: 'bearer', token: 'secret-token', header: 'X-API-Key', key: 'demo-key', username: 'demo', password: 'demo' },
  delay: { on: false, kind: 'fixed', fixedMs: 300, minMs: 100, maxMs: 800, medianMs: 120, sigma: 0.4 },
  fault: { on: false, fault: 'CONNECTION_RESET' },
  flaky: { on: false, percent: 30, failWith: 'status', status: 503, body: '{\n  "error": "service unavailable"\n}', fault: 'CONNECTION_RESET' },
  templating: { on: true },
  sequence: { on: false, mode: 'sequential', steps: [{ status: 200, body: '{\n  "status": "DONE"\n}' }] },
  scenario: { on: false, name: 'my-flow', requiredState: 'Started', newState: 'DONE' },
  callback: { on: false, url: 'https://webhook.example.com/events', method: 'POST', body: '{\n  "event": "created",\n  "id": "{{uuid}}"\n}', delayMs: 1000, maxAttempts: 3 },
  cors: { on: true, origin: '*' },
  limits: { on: false, times: 1, ttlMinutes: 60 },
};

export const DEFAULT_FORM: QuickForm = {
  name: '',
  method: 'GET',
  path: '/hello',
  status: 200,
  bodyType: 'json',
  body: '{\n  "message": "Hello from FastMock"\n}',
  headers: [],
  b: DEFAULT_BEHAVIOURS,
};

/** A new form: the defaults with the given fields and behaviours replaced. */
export function makeForm(over: Partial<Omit<QuickForm, 'b'>> & { b?: { [K in BehaviourKey]?: Partial<Behaviours[K]> } } = {}): QuickForm {
  const { b, ...rest } = over;
  const merged = structuredClone(DEFAULT_BEHAVIOURS);
  for (const [k, v] of Object.entries(b ?? {}) as [BehaviourKey, object][]) Object.assign(merged[k], v);
  return { ...structuredClone(DEFAULT_FORM), ...structuredClone(rest), b: merged };
}

export interface Preset {
  id: string;
  label: string;
  description: string;
  form: QuickForm;
}

export const PRESETS: Preset[] = [
  {
    id: 'hello',
    label: 'Hello JSON',
    description: 'GET with a path parameter and dynamic values',
    form: makeForm({
      name: 'hello',
      path: '/hello/{name}',
      body: '{\n  "message": "Hello, {{request.pathParams.name}}!",\n  "id": "{{uuid}}",\n  "at": "{{now}}"\n}',
    }),
  },
  {
    id: 'login',
    label: 'Login with token',
    description: 'POST credentials, get a fresh access token',
    form: makeForm({
      name: 'login',
      method: 'POST',
      path: '/auth/login',
      body: '{\n  "accessToken": "{{randomValue length=32 type=\'ALPHANUMERIC\'}}",\n  "tokenType": "Bearer",\n  "expiresIn": 3600,\n  "user": "{{request.json.username}}"\n}',
      b: {
        conditions: {
          on: true,
          rows: [
            { source: 'jsonPath', name: '$.username', op: 'matches', value: '.+' },
            { source: 'jsonPath', name: '$.password', op: 'matches', value: '.+' },
          ],
        },
      },
    }),
  },
  {
    id: 'protected',
    label: 'Protected resource',
    description: 'Needs a bearer token, answers 401 without one',
    form: makeForm({
      name: 'current user',
      path: '/api/me',
      body: '{\n  "id": 42,\n  "name": "{{randomFullName seed=request.headers.authorization}}",\n  "email": "{{randomEmail seed=request.headers.authorization}}"\n}',
      b: { auth: { on: true, kind: 'bearer' } },
    }),
  },
  {
    id: 'flaky',
    label: 'Flaky endpoint',
    description: 'Slow, and fails 30% of the calls with 503',
    form: makeForm({
      name: 'flaky order',
      path: '/api/orders/{id}',
      body: '{\n  "id": "{{request.pathParams.id}}",\n  "status": "SHIPPED"\n}',
      b: { delay: { on: true, kind: 'range', minMs: 50, maxMs: 800 }, flaky: { on: true, percent: 30 } },
    }),
  },
  {
    id: 'paginated',
    label: 'Paginated list',
    description: 'Reads page and size from the query',
    form: makeForm({
      name: 'list items',
      path: '/api/items',
      body:
        '{\n  "page": "{{json (math (default request.query.page 1) \'+\' 0)}}",\n  "size": "{{json (math (default request.query.size 10) \'+\' 0)}}",\n  "nextPage": "{{json (math (default request.query.page 1) \'+\' 1)}}",\n  "total": 120,\n  "items": [\n    {"id": "{{json (randomInt 1 1000)}}", "name": "{{randomWord}}"},\n    {"id": "{{json (randomInt 1 1000)}}", "name": "{{randomWord}}"},\n    {"id": "{{json (randomInt 1 1000)}}", "name": "{{randomWord}}"}\n  ]\n}',
    }),
  },
  {
    id: 'webhook',
    label: 'Webhook after create',
    description: 'Answers 201, then calls your webhook',
    form: makeForm({
      name: 'create order',
      method: 'POST',
      path: '/api/orders',
      status: 201,
      body: '{\n  "id": "{{request.json.reference}}",\n  "status": "CREATED"\n}',
      b: {
        conditions: { on: true, rows: [{ source: 'jsonPath', name: '$.reference', op: 'matches', value: '.+' }] },
        callback: {
          on: true,
          url: 'https://webhook.example.com/orders',
          body: '{\n  "event": "order.created",\n  "id": "{{request.json.reference}}"\n}',
          delayMs: 1000,
        },
      },
    }),
  },
];

export interface QuickStub {
  role: 'main' | 'auth' | 'preflight';
  title: string;
  stub: Stub;
}

const PARAM_RE = /\{([^}/]*)\}/g;

/** The names of the `{param}` placeholders of a path. */
export function pathParams(path: string): string[] {
  return [...path.matchAll(PARAM_RE)].map((m) => (m[1] ?? '').replace(/\.\.\.$/, ''));
}

function num(v: Num): number {
  return v === '' ? NaN : v;
}

function parseJson(text: string): { ok: true; value: unknown } | { ok: false; error: string } {
  try {
    return { ok: true, value: JSON.parse(text) as unknown };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** The body of a response or callback: JSON when the text parses, otherwise text; nothing when empty. */
function bodyFields(text: string, preferJson: boolean): Pick<ResponseDef, 'body' | 'jsonBody'> {
  if (!text.trim()) return {};
  if (preferJson) {
    const p = parseJson(text);
    if (p.ok) return { jsonBody: p.value };
  }
  return { body: text };
}

function corsHeaders(f: QuickForm): Record<string, string> {
  if (!f.b.cors.on) return {};
  return { 'access-control-allow-origin': f.b.cors.origin.trim() || '*', 'access-control-expose-headers': '*' };
}

function responseHeaders(f: QuickForm): Record<string, string> | undefined {
  const out: Record<string, string> = { ...corsHeaders(f) };
  for (const h of f.headers) {
    const k = h.name.trim();
    if (k) out[k] = h.value;
  }
  return Object.keys(out).length ? out : undefined;
}

function delayOf(f: QuickForm): Delay | undefined {
  const d = f.b.delay;
  if (!d.on) return undefined;
  if (d.kind === 'fixed') return { fixedMs: num(d.fixedMs) };
  if (d.kind === 'range') return { minMs: num(d.minMs), maxMs: num(d.maxMs) };
  return { lognormal: { medianMs: num(d.medianMs), sigma: num(d.sigma) } };
}

function stringMatcher(op: ConditionOp, value: string): StringMatcher {
  if (op === 'absent') return { absent: true };
  return { [op]: value };
}

/** The URL field of the request: a template when the path has `{params}`, otherwise an exact path. */
function urlFields(f: QuickForm): Pick<RequestPattern, 'urlPath' | 'urlPathTemplate'> {
  const path = f.path.trim();
  return pathParams(path).length ? { urlPathTemplate: path } : { urlPath: path };
}

/** Method, URL and the request conditions, without authentication. */
function baseRequest(f: QuickForm): RequestPattern {
  const r: RequestPattern = { method: f.method, ...urlFields(f) };
  if (!f.b.conditions.on) return r;
  const headers: Record<string, StringMatcher> = {};
  const query: Record<string, StringMatcher> = {};
  const body: BodyMatcher[] = [];
  for (const c of f.b.conditions.rows) {
    const name = c.name.trim();
    if (c.source === 'header' && name) headers[name] = stringMatcher(c.op, c.value);
    else if (c.source === 'query' && name) query[name] = stringMatcher(c.op, c.value);
    else if (c.source === 'jsonPath' && name) body.push(c.op === 'absent' ? { jsonPath: name, absent: true } : { jsonPath: name, value: stringMatcher(c.op, c.value) });
    else if (c.source === 'body') {
      if (c.op === 'absent') body.push({ absent: true });
      else if (c.op === 'equalTo') body.push({ equalTo: c.value });
      else body.push({ [c.op]: c.value });
    }
  }
  if (Object.keys(headers).length) r.headers = headers;
  if (Object.keys(query).length) r.query = query;
  if (body.length) r.body = body;
  return r;
}

function withAuth(f: QuickForm, r: RequestPattern): RequestPattern {
  const a = f.b.auth;
  if (!a.on) return r;
  if (a.kind === 'basic') return { ...r, basicAuth: { username: a.username, password: a.password } };
  const headers = { ...(r.headers ?? {}) };
  if (a.kind === 'bearer') headers.Authorization = { matches: 'Bearer .+' };
  else if (a.kind === 'bearerExact') headers.Authorization = { equalTo: `Bearer ${a.token}` };
  else headers[a.header.trim() || 'X-API-Key'] = { equalTo: a.key };
  return { ...r, headers };
}

/** The main response, before sequences and random failures. */
function mainResponse(f: QuickForm): ResponseDef {
  const r: ResponseDef = { status: num(f.status) };
  const headers = responseHeaders(f);
  if (headers) r.headers = headers;
  if (f.bodyType === 'json') Object.assign(r, bodyFields(f.body, true));
  else if (f.bodyType === 'text' && f.body) r.body = f.body;
  return decorate(f, r);
}

function decorate(f: QuickForm, r: ResponseDef): ResponseDef {
  if (!f.b.templating.on) r.templated = false;
  const delay = delayOf(f);
  if (delay) r.delay = delay;
  return r;
}

function gcd(a: number, b: number): number {
  return b === 0 ? a : gcd(b, a % b);
}

/** How a failure share becomes random responses: `failures` of `total` entries fail. */
export function failureShare(percent: number): { failures: number; total: number } {
  const p = Math.min(90, Math.max(10, Math.round(percent / 10) * 10));
  const g = gcd(p, 100);
  return { failures: p / g, total: 100 / g };
}

function failureResponse(f: QuickForm): ResponseDef {
  const fl = f.b.flaky;
  if (fl.failWith === 'fault') return decorate(f, { fault: fl.fault });
  const r: ResponseDef = { status: num(fl.status) };
  const headers = corsHeaders(f);
  if (Object.keys(headers).length) r.headers = headers;
  Object.assign(r, bodyFields(fl.body, true));
  return decorate(f, r);
}

function stepResponse(f: QuickForm, s: SequenceStep): ResponseDef {
  const r: ResponseDef = { status: num(s.status) };
  const headers = responseHeaders(f);
  if (headers) r.headers = headers;
  Object.assign(r, bodyFields(s.body, f.bodyType !== 'text'));
  return decorate(f, r);
}

function callbackOf(f: QuickForm): NonNullable<Stub['callbacks']>[number] {
  const c = f.b.callback;
  const cb: NonNullable<Stub['callbacks']>[number] = { url: c.url.trim(), method: c.method };
  Object.assign(cb, bodyFields(c.body, true));
  const ms = num(c.delayMs);
  if (ms > 0) cb.delay = { fixedMs: ms };
  cb.maxAttempts = num(c.maxAttempts);
  return cb;
}

/** Request headers a browser asks permission for in a CORS preflight, in lower case. */
export function preflightHeaders(f: QuickForm): string[] {
  const out = new Set<string>();
  const a = f.b.auth;
  if (a.on) out.add(a.kind === 'apiKey' ? (a.header.trim() || 'X-API-Key').toLowerCase() : 'authorization');
  if (f.b.conditions.on) for (const c of f.b.conditions.rows) if (c.source === 'header' && c.name.trim()) out.add(c.name.trim().toLowerCase());
  if (['POST', 'PUT', 'PATCH'].includes(f.method)) out.add('content-type');
  return [...out];
}

/** Whether a browser sends a CORS preflight before calling this mock with the sample request. */
export function needsPreflight(f: QuickForm): boolean {
  return f.b.cors.on && (!['GET', 'HEAD', 'POST'].includes(f.method) || preflightHeaders(f).length > 0);
}

function displayName(f: QuickForm): string {
  return f.name.trim() || `${f.method} ${f.path.trim()}`;
}

/**
 * The stubs the form describes, exactly as `POST /__admin/stubs` takes them: the mock itself, a fallback that
 * answers 401 when authentication is required, and an OPTIONS stub for browser preflights when CORS needs one.
 */
export function buildStubs(f: QuickForm, space?: string | null): QuickStub[] {
  const name = displayName(f);
  const common: Pick<Stub, 'tags' | 'space'> = { tags: [QUICK_TAG] };
  if (space) common.space = space;

  const main: Stub = { name, request: withAuth(f, baseRequest(f)) };
  const base = mainResponse(f);
  if (f.b.fault.on) base.fault = f.b.fault.fault;
  if (f.b.flaky.on) {
    const { failures, total } = failureShare(f.b.flaky.percent);
    const fail = failureResponse(f);
    main.responseMode = 'random';
    main.responses = Array.from({ length: total }, (_, i) => structuredClone(i < failures ? fail : base));
  } else if (f.b.sequence.on) {
    main.responseMode = f.b.sequence.mode;
    main.responses = [base, ...f.b.sequence.steps.map((s) => stepResponse(f, s))];
  } else main.response = base;
  if (f.b.scenario.on) {
    const s = f.b.scenario;
    main.scenario = { name: s.name.trim() };
    if (s.requiredState.trim()) main.scenario.requiredState = s.requiredState.trim();
    if (s.newState.trim()) main.scenario.newState = s.newState.trim();
  }
  if (f.b.callback.on) main.callbacks = [callbackOf(f)];
  if (f.b.limits.on) {
    if (f.b.limits.times !== '') main.times = f.b.limits.times;
    if (f.b.limits.ttlMinutes !== '') main.ttlSeconds = f.b.limits.ttlMinutes * 60;
  }
  Object.assign(main, common);
  const out: QuickStub[] = [{ role: 'main', title: 'Mock', stub: main }];

  if (f.b.auth.on) {
    const a = f.b.auth;
    const headers: Record<string, string> = { ...corsHeaders(f) };
    if (a.kind === 'basic') headers['www-authenticate'] = 'Basic realm="fastmock"';
    else if (a.kind !== 'apiKey') headers['www-authenticate'] = 'Bearer';
    const message = a.kind === 'apiKey' ? `send the ${a.header.trim() || 'X-API-Key'} header` : a.kind === 'basic' ? 'send a user name and password' : 'send a bearer token';
    out.push({
      role: 'auth',
      title: '401 fallback',
      stub: {
        name: `${name} (401 without credentials)`,
        priority: 6,
        request: baseRequest(f),
        response: { status: 401, ...(Object.keys(headers).length ? { headers } : {}), jsonBody: { error: 'unauthorized', message } },
        ...common,
      },
    });
  }

  if (needsPreflight(f)) {
    const allow = preflightHeaders(f);
    const headers: Record<string, string> = {
      'access-control-allow-origin': f.b.cors.origin.trim() || '*',
      'access-control-allow-methods': `${f.method}, OPTIONS`,
      'access-control-max-age': '600',
    };
    if (allow.length) headers['access-control-allow-headers'] = allow.join(', ');
    out.push({
      role: 'preflight',
      title: 'CORS preflight',
      stub: { name: `${name} (CORS preflight)`, request: { method: 'OPTIONS', ...urlFields(f) }, response: { status: 204, headers }, ...common },
    });
  }
  return out;
}

export interface Issue {
  field: string;
  message: string;
}

const HEADER_NAME = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
const MAX_DELAY_MS = 24 * 3600 * 1000;

function statusIssue(v: Num, field: string, out: Issue[]) {
  const n = num(v);
  if (!Number.isInteger(n) || n < 200 || n > 599) out.push({ field, message: 'Use a status from 200 to 599' });
}

function regexIssue(re: string, field: string, out: Issue[]) {
  try {
    new RegExp(re.replace(/^\(\?i\)/, ''));
  } catch {
    out.push({ field, message: 'Not a valid regular expression' });
  }
}

function nonNegative(v: Num, field: string, label: string, out: Issue[], max = MAX_DELAY_MS) {
  const n = num(v);
  if (!Number.isFinite(n) || n < 0 || n > max) out.push({ field, message: `${label}: 0 to ${String(max)}` });
}

/** What keeps the form from making valid stubs; the field names the input the message belongs to. */
export function formIssues(f: QuickForm): Issue[] {
  const out: Issue[] = [];
  const path = f.path.trim();
  if (!path) out.push({ field: 'path', message: 'Enter a path, for example /orders' });
  else if (!path.startsWith('/')) out.push({ field: 'path', message: 'A path starts with /' });
  else if (/[\s?#]/.test(path)) out.push({ field: 'path', message: 'No spaces, ? or # in the path; match query parameters under Request matching' });
  else if ((path.match(/\{/g) ?? []).length !== (path.match(/\}/g) ?? []).length) out.push({ field: 'path', message: 'Close every { with }' });
  else if (pathParams(path).some((p) => !/^[A-Za-z_][A-Za-z0-9_]*$/.test(p))) out.push({ field: 'path', message: 'Name parameters with letters, digits and _, e.g. /orders/{id}' });
  statusIssue(f.status, 'status', out);
  if (f.bodyType === 'json' && f.body.trim()) {
    const p = parseJson(f.body);
    if (!p.ok) out.push({ field: 'body', message: `Invalid JSON: ${p.error}` });
  }
  if (num(f.status) === 204 && f.bodyType !== 'none' && f.body.trim()) out.push({ field: 'body', message: 'A 204 response has no body; choose None or another status' });
  const seen = new Set<string>();
  f.headers.forEach((h, i) => {
    const k = h.name.trim();
    if (!k && !h.value) return;
    if (!HEADER_NAME.test(k)) out.push({ field: `headers.${String(i)}`, message: 'Header names use letters, digits and - only' });
    else if (seen.has(k.toLowerCase())) out.push({ field: `headers.${String(i)}`, message: `Header ${k} is set twice` });
    seen.add(k.toLowerCase());
  });

  const b = f.b;
  if (b.conditions.on) {
    if (!b.conditions.rows.length) out.push({ field: 'conditions', message: 'Add a condition or turn request matching off' });
    b.conditions.rows.forEach((c, i) => {
      const field = `conditions.${String(i)}`;
      if (c.source !== 'body' && !c.name.trim()) out.push({ field, message: c.source === 'jsonPath' ? 'Enter a JSONPath, e.g. $.id' : 'Enter a name' });
      else if (c.source === 'jsonPath' && !c.name.trim().startsWith('$')) out.push({ field, message: 'A JSONPath starts with $' });
      else if (c.source === 'header' && !HEADER_NAME.test(c.name.trim())) out.push({ field, message: 'Header names use letters, digits and - only' });
      else if (c.op !== 'absent' && c.op !== 'equalTo' && !c.value) out.push({ field, message: 'Enter a value' });
      else if (c.op === 'matches') regexIssue(c.value, field, out);
    });
  }
  if (b.auth.on) {
    const a = b.auth;
    if (a.kind === 'bearerExact' && !a.token.trim()) out.push({ field: 'auth', message: 'Enter the token' });
    if (a.kind === 'apiKey' && (!HEADER_NAME.test(a.header.trim()) || !a.key)) out.push({ field: 'auth', message: 'Enter the header name and the key' });
    if (a.kind === 'basic' && !a.username) out.push({ field: 'auth', message: 'Enter a user name' });
  }
  if (b.delay.on) {
    const d = b.delay;
    if (d.kind === 'fixed') nonNegative(d.fixedMs, 'delay', 'Delay in ms', out);
    else if (d.kind === 'range') {
      nonNegative(d.minMs, 'delay', 'Minimum in ms', out);
      nonNegative(d.maxMs, 'delay', 'Maximum in ms', out);
      if (num(d.minMs) > num(d.maxMs)) out.push({ field: 'delay', message: 'The maximum must not be below the minimum' });
    } else {
      nonNegative(d.medianMs, 'delay', 'Median in ms', out);
      nonNegative(d.sigma, 'delay', 'Spread (sigma)', out, 10);
    }
  }
  if (b.flaky.on && b.flaky.failWith === 'status') {
    statusIssue(b.flaky.status, 'flaky', out);
    if (b.flaky.body.trim().startsWith('{') && !parseJson(b.flaky.body).ok) out.push({ field: 'flaky', message: 'The failure body looks like JSON but does not parse' });
  }
  if (b.sequence.on) {
    if (!b.sequence.steps.length) out.push({ field: 'sequence', message: 'Add at least one more response' });
    if (b.sequence.steps.length > 99) out.push({ field: 'sequence', message: 'At most 100 responses' });
    b.sequence.steps.forEach((s, i) => statusIssue(s.status, `sequence.${String(i)}`, out));
  }
  if (b.scenario.on && !b.scenario.name.trim()) out.push({ field: 'scenario', message: 'Name the scenario' });
  if (b.callback.on) {
    const c = b.callback;
    if (!c.url.includes('{{') && !/^https?:\/\/[^/\s]+/.test(c.url.trim())) out.push({ field: 'callback', message: 'Enter an http:// or https:// URL' });
    nonNegative(c.delayMs, 'callback', 'Delay in ms', out, 7 * 24 * 3600 * 1000);
    const n = num(c.maxAttempts);
    if (!Number.isInteger(n) || n < 1 || n > 50) out.push({ field: 'callback', message: 'Attempts: 1 to 50' });
  }
  if (b.limits.on) {
    const { times, ttlMinutes } = b.limits;
    if (times === '' && ttlMinutes === '') out.push({ field: 'limits', message: 'Set a number of calls, a lifetime or both' });
    if (times !== '' && (!Number.isInteger(times) || times < 1)) out.push({ field: 'limits', message: 'Calls: at least 1' });
    if (ttlMinutes !== '' && (!Number.isInteger(ttlMinutes) || ttlMinutes < 1 || ttlMinutes > 525_600)) out.push({ field: 'limits', message: 'Lifetime: 1 to 525600 minutes' });
  }
  for (const [k, others] of Object.entries(CONFLICTS) as [BehaviourKey, BehaviourKey[]][]) {
    for (const o of others) if (k < o && b[k].on && b[o].on) out.push({ field: k, message: `${BEHAVIOUR_TITLES[k]} and ${BEHAVIOUR_TITLES[o]} cannot be combined` });
  }
  return out;
}

export const BEHAVIOUR_TITLES: Record<BehaviourKey, string> = {
  conditions: 'Request matching',
  auth: 'Require authentication',
  delay: 'Delay',
  fault: 'Network fault',
  flaky: 'Random failures',
  templating: 'Dynamic values',
  sequence: 'Response sequence',
  scenario: 'Stateful scenario',
  callback: 'Webhook callback',
  cors: 'Browser access (CORS)',
  limits: 'Temporary mock',
};

function sampleParam(name: string): string {
  if (/id$/i.test(name)) return '42';
  if (/name$/i.test(name)) return 'world';
  return 'example';
}

function sampleValue(op: ConditionOp, value: string): string | undefined {
  if (op === 'absent') return undefined;
  if (op === 'matches') return /^[\w-]+$/.test(value) ? value : 'example';
  return value;
}

function setJsonPath(target: Record<string, unknown>, path: string, value: string) {
  const keys = path
    .replace(/^\$\.?/, '')
    .split('.')
    .filter(Boolean);
  if (!keys.length || keys.some((k) => !/^[A-Za-z_][\w-]*$/.test(k))) return;
  let node = target;
  keys.forEach((k, i) => {
    if (i === keys.length - 1) node[k] = value;
    else {
      const next = node[k];
      node = typeof next === 'object' && next !== null ? (next as Record<string, unknown>) : (node[k] = {});
    }
  });
}

function base64(text: string): string {
  return btoa(String.fromCharCode(...new TextEncoder().encode(text)));
}

/** A request that the created mock answers with its main response. */
export function quickSample(f: QuickForm): SampleRequest {
  const path = f.path.trim().replace(PARAM_RE, (_m, name: string) => encodeURIComponent(sampleParam(name.replace(/\.\.\.$/, ''))));
  const headers: Record<string, string> = {};
  const query = new URLSearchParams();
  const json: Record<string, unknown> = {};
  let text = '';
  if (f.b.conditions.on) {
    for (const c of f.b.conditions.rows) {
      const v = sampleValue(c.op, c.value);
      if (v === undefined) continue;
      if (c.source === 'header' && c.name.trim()) headers[c.name.trim()] = v;
      else if (c.source === 'query' && c.name.trim()) query.set(c.name.trim(), v);
      else if (c.source === 'jsonPath') setJsonPath(json, c.name.trim(), v);
      else if (c.source === 'body') text = v;
    }
  }
  const a = f.b.auth;
  if (a.on) {
    if (a.kind === 'bearer') headers.Authorization = 'Bearer demo-token';
    else if (a.kind === 'bearerExact') headers.Authorization = `Bearer ${a.token}`;
    else if (a.kind === 'apiKey') headers[a.header.trim() || 'X-API-Key'] = a.key;
    else headers.Authorization = `Basic ${base64(`${a.username}:${a.password}`)}`;
  }
  let body = '';
  if (['POST', 'PUT', 'PATCH', 'DELETE'].includes(f.method)) {
    if (Object.keys(json).length) body = JSON.stringify(json, null, 2);
    else if (text) body = text;
    else if (f.method !== 'DELETE') body = '{\n  "example": true\n}';
    if (body && !text) headers['content-type'] = 'application/json';
  }
  return { method: f.method, path, query: query.toString(), headers, body };
}

export type CodeLang = 'curl' | 'fetch' | 'python' | 'java';

export const CODE_LANGS: { value: CodeLang; label: string }[] = [
  { value: 'curl', label: 'cURL' },
  { value: 'fetch', label: 'JavaScript' },
  { value: 'python', label: 'Python' },
  { value: 'java', label: 'Java' },
];

/**
 * A double-quoted string literal that JavaScript, Python and Java all read back as the same text: JSON escapes,
 * with every non-ASCII character as `\uXXXX` (JSON never escapes `"` or `\` that way, which Java would trip on).
 */
function lit(s: string): string {
  return JSON.stringify(s).replace(/[\u007f-￿]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);
}

const javaLit = lit;

/** Python reads a surrogate pair as two characters, so characters outside the BMP become `\UXXXXXXXX`. */
function pyLit(s: string): string {
  return lit(s).replace(/\\u(d[89ab][0-9a-f]{2})\\u(d[c-f][0-9a-f]{2})/g, (_m, hi: string, lo: string) => {
    const cp = (parseInt(hi, 16) - 0xd800) * 0x400 + (parseInt(lo, 16) - 0xdc00) + 0x10000;
    return `\\U${cp.toString(16).padStart(8, '0')}`;
  });
}

/** Code that sends the sample request to the mock: cURL, JavaScript `fetch`, Python `requests`, Java `HttpClient`. */
export function codeSample(lang: CodeLang, base: string, req: SampleRequest): string {
  const url = joinUrl(base, req.path, req.query);
  const headers = Object.entries(req.headers).filter(([k]) => k.trim());
  const hasBody = req.body !== '' && req.method !== 'GET' && req.method !== 'HEAD';
  switch (lang) {
    case 'curl':
      return toCurl(base, req);
    case 'fetch': {
      const opts = [`  method: ${lit(req.method)},`];
      if (headers.length) opts.push(`  headers: {\n${headers.map(([k, v]) => `    ${lit(k)}: ${lit(v)},`).join('\n')}\n  },`);
      if (hasBody) opts.push(`  body: ${lit(req.body)},`);
      return `const response = await fetch(${lit(url)}, {\n${opts.join('\n')}\n});\nconsole.log(response.status, await response.text());`;
    }
    case 'python': {
      const args = [`    ${pyLit(req.method)},`, `    ${pyLit(url)},`];
      if (headers.length) args.push(`    headers={\n${headers.map(([k, v]) => `        ${pyLit(k)}: ${pyLit(v)},`).join('\n')}\n    },`);
      if (hasBody) args.push(`    data=${pyLit(req.body)},`);
      return `import requests\n\nresponse = requests.request(\n${args.join('\n')}\n)\nprint(response.status_code, response.text)`;
    }
    case 'java': {
      const lines = [`HttpRequest request = HttpRequest.newBuilder(URI.create(${javaLit(url)}))`];
      for (const [k, v] of headers) lines.push(`        .header(${javaLit(k)}, ${javaLit(v)})`);
      lines.push(`        .method(${javaLit(req.method)}, ${hasBody ? `HttpRequest.BodyPublishers.ofString(${javaLit(req.body)})` : 'HttpRequest.BodyPublishers.noBody()'})`);
      lines.push('        .build();');
      return [
        'import java.net.URI;',
        'import java.net.http.HttpClient;',
        'import java.net.http.HttpRequest;',
        'import java.net.http.HttpResponse;',
        '',
        'HttpClient client = HttpClient.newHttpClient();',
        ...lines,
        'HttpResponse<String> response = client.send(request, HttpResponse.BodyHandlers.ofString());',
        'System.out.println(response.statusCode() + " " + response.body());',
      ].join('\n');
    }
  }
}
