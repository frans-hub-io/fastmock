import type { JournalEntry, RequestPattern, StringMatcher, Stub } from '../api/types';
import { headerValue } from './format';
import type { SampleRequest } from './curl';

export const URL_KINDS = ['urlPath', 'urlPathTemplate', 'urlPathPattern', 'url', 'urlPattern'] as const;
export type UrlKind = (typeof URL_KINDS)[number] | 'any';

export const URL_KIND_LABELS: Record<UrlKind, string> = {
  urlPath: 'Path equals',
  urlPathTemplate: 'Path template',
  urlPathPattern: 'Path regex',
  url: 'Path + query equals',
  urlPattern: 'Path + query regex',
  any: 'Any path',
};

export function urlKind(req: RequestPattern): UrlKind {
  for (const k of URL_KINDS) if (typeof req[k] === 'string') return k;
  return 'any';
}

export function urlValue(req: RequestPattern): string {
  const k = urlKind(req);
  return k === 'any' ? '' : (req[k] ?? '');
}

export function setUrl(req: RequestPattern, kind: UrlKind, value: string): RequestPattern {
  const next: RequestPattern = { ...req };
  for (const k of URL_KINDS) delete next[k];
  if (kind !== 'any') next[kind] = value;
  return next;
}

export function stubPath(s: Stub): string {
  const r = s.request;
  const k = urlKind(r);
  return k === 'any' ? '*' : (r[k] ?? '');
}

export function stubTitle(s: Stub): string {
  return s.name?.trim() || `${(s.request.method ?? 'ANY').toUpperCase()} ${stubPath(s)}`;
}

function matcherSample(m: StringMatcher | undefined): string | undefined {
  if (!m) return undefined;
  if (typeof m.equalTo === 'string') return m.equalTo;
  if (typeof m.contains === 'string') return m.contains;
  if (m.absent) return undefined;
  return '';
}

function sampleFromRegex(re: string): string {
  return re
    .replace(/^\^/, '')
    .replace(/\$$/, '')
    .replace(/\\d\+?|\[0-9\]\+?/g, '1')
    .replace(/\\w\+?|\[\^\/\]\+|\.\+|\.\*|\[a-z\]\+/gi, 'x')
    .replace(/\\(.)/g, '$1')
    .replace(/[()?*+]/g, '');
}

export function sampleRequest(stub: Stub): SampleRequest {
  const r = stub.request;
  const method = (r.method && r.method !== 'ANY' ? r.method : 'GET').toUpperCase();
  let path = '/';
  let query = '';
  const kind = urlKind(r);
  const v = urlValue(r);
  if (kind === 'urlPath') path = v;
  else if (kind === 'urlPathTemplate') {
    path = v.replace(/\{([^}.]+)(\.\.\.)?\}/g, (_m, name: string) => {
      const pm = r.pathParams?.[name];
      return encodeURIComponent(matcherSample(pm) || (/id$/i.test(name) ? '1' : name));
    });
  } else if (kind === 'urlPathPattern') path = sampleFromRegex(v);
  else if (kind === 'url') {
    const i = v.indexOf('?');
    path = i >= 0 ? v.slice(0, i) : v;
    query = i >= 0 ? v.slice(i + 1) : '';
  } else if (kind === 'urlPattern') path = sampleFromRegex(v);
  if (!path.startsWith('/')) path = `/${path}`;

  if (r.query) {
    const p = new URLSearchParams(query);
    for (const [k, m] of Object.entries(r.query)) {
      const s = matcherSample(m);
      if (s !== undefined) p.set(k, s);
    }
    query = p.toString();
  }
  const headers: Record<string, string> = {};
  for (const [k, m] of Object.entries(r.headers ?? {})) {
    const s = matcherSample(m);
    if (s !== undefined) headers[k] = s || 'x';
  }
  let body = '';
  for (const b of r.body ?? []) {
    if (b.negate) continue;
    if (b.equalToJson !== undefined) {
      body = typeof b.equalToJson === 'string' ? b.equalToJson : JSON.stringify(b.equalToJson, null, 2);
      headers['content-type'] ??= 'application/json';
      break;
    }
    if (typeof b.equalTo === 'string') {
      body = b.equalTo;
      break;
    }
    if (typeof b.equalToXml === 'string') {
      body = b.equalToXml;
      headers['content-type'] ??= 'application/xml';
      break;
    }
    if (typeof b.contains === 'string') body = b.contains;
  }
  if (!body && ['POST', 'PUT', 'PATCH'].includes(method) && r.body?.some((b) => b.jsonPath || b.jsonSchema)) {
    body = '{}';
    headers['content-type'] ??= 'application/json';
  }
  return { method, path, query, headers, body };
}

const SKIP_HEADERS = new Set([
  'host',
  'content-length',
  'connection',
  'accept-encoding',
  'user-agent',
  'accept',
  'traceparent',
  'tracestate',
  'x-request-id',
  'authorization',
  'cookie',
]);

export function stubFromJournal(e: JournalEntry): Stub {
  const ct = headerValue(e.headers, 'content-type') ?? '';
  const request: RequestPattern = { method: e.method.toUpperCase() };
  if (e.query) request.url = `${e.path}?${e.query}`;
  else request.urlPath = e.path;
  const keepHeaders = Object.entries(e.headers).filter(([k, v]) => !SKIP_HEADERS.has(k.toLowerCase()) && v !== '***' && k.toLowerCase() === 'content-type');
  if (keepHeaders.length) {
    request.headers = Object.fromEntries(keepHeaders.map(([k, v]) => [k, { contains: v.split(';')[0]?.trim() ?? v }]));
  }
  if (e.body) {
    if (ct.includes('json')) {
      try {
        request.body = [{ equalToJson: JSON.parse(e.body) as unknown, ignoreExtraElements: true }];
      } catch {
        request.body = [{ equalTo: e.body }];
      }
    } else {
      request.body = [{ equalTo: e.body }];
    }
  }
  const stub: Stub = {
    name: `${e.method.toUpperCase()} ${e.path}`,
    request,
    response: { status: e.matched ? e.status : 200 },
  };
  const respBody = e.matched ? e.responseBody : '';
  if (respBody && stub.response) {
    try {
      stub.response.jsonBody = JSON.parse(respBody) as unknown;
    } catch {
      stub.response.body = respBody;
    }
  }
  return stub;
}

export function duplicateStub(s: Stub): Stub {
  const copy: Stub = structuredClone(s);
  delete copy.id;
  delete copy.updatedAt;
  copy.name = `${stubTitle(s)} (copy)`;
  return copy;
}

export function stripServerFields(s: Stub): Stub {
  const copy: Stub = structuredClone(s);
  delete copy.updatedAt;
  return copy;
}

export type ImportKind = 'fastmock' | 'wiremock' | 'openapi' | 'postman' | 'har' | 'mockserver' | 'unknown';

export function detectImportKind(text: string): { kind: ImportKind; json: unknown } {
  const t = text.trimStart();
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch {
    if (/^(openapi|swagger)\s*:/m.test(t)) return { kind: 'openapi', json: undefined };
    return { kind: 'unknown', json: undefined };
  }
  if (!json || typeof json !== 'object') return { kind: 'unknown', json };
  const o = json as Record<string, unknown>;
  if (typeof o.openapi === 'string' || typeof o.swagger === 'string') return { kind: 'openapi', json };
  const info = o.info as Record<string, unknown> | undefined;
  if (info && typeof info.schema === 'string' && info.schema.includes('postman')) return { kind: 'postman', json };
  if (o.log && typeof o.log === 'object' && Array.isArray((o.log as Record<string, unknown>).entries)) return { kind: 'har', json };
  if (Array.isArray(o.mappings)) return { kind: 'wiremock', json };
  if (Array.isArray(o.stubs)) return { kind: 'fastmock', json };
  const list = Array.isArray(json) ? (json as unknown[]) : [json];
  const first = list[0] as Record<string, unknown> | undefined;
  if (first && typeof first === 'object') {
    if ('httpRequest' in first || 'httpResponse' in first) return { kind: 'mockserver', json };
    const req = first.request as Record<string, unknown> | undefined;
    if (req && typeof req === 'object') {
      if ('urlPath' in req || 'urlPathTemplate' in req || 'urlPathPattern' in req || 'pathParams' in req) {
        if ('response' in first && first.response && typeof first.response === 'object' && 'jsonBody' in (first.response)) {
          return { kind: Array.isArray(json) ? 'fastmock' : 'wiremock', json };
        }
      }
      return { kind: Array.isArray(json) ? 'fastmock' : 'wiremock', json };
    }
  }
  return { kind: 'unknown', json };
}
