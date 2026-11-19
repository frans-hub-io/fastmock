const TOKEN_KEY = 'fastmock.token';

export class ApiError extends Error {
  readonly status: number;
  readonly body: unknown;

  constructor(status: number, message: string, body?: unknown) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.body = body;
  }
}

type Listener = () => void;

function readStored(): string | null {
  try {
    return window.sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

let current: string | null = readStored();
const unauthorizedListeners = new Set<Listener>();

export const tokenStore = {
  get(): string | null {
    return current;
  },
  set(token: string | null): void {
    current = token;
    try {
      if (token) window.sessionStorage.setItem(TOKEN_KEY, token);
      else window.sessionStorage.removeItem(TOKEN_KEY);
    } catch {
      /* sessionStorage may be unavailable; the token stays in memory */
    }
  },
  onUnauthorized(cb: Listener): () => void {
    unauthorizedListeners.add(cb);
    return () => unauthorizedListeners.delete(cb);
  },
  notifyUnauthorized(): void {
    for (const cb of unauthorizedListeners) cb();
  },
};

export function authHeaders(token = tokenStore.get()): Record<string, string> {
  return token ? { Authorization: `Bearer ${token}` } : {};
}

export type Query = Record<string, string | number | boolean | null | undefined>;

export function buildQuery(q?: Query): string {
  if (!q) return '';
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) {
    if (v === undefined || v === null || v === '') continue;
    p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : '';
}

export async function errorFromResponse(res: Response): Promise<ApiError> {
  let body: unknown;
  let message = `${res.status} ${res.statusText}`.trim();
  try {
    const text = await res.text();
    if (text) {
      try {
        body = JSON.parse(text) as unknown;
        if (body && typeof body === 'object' && 'error' in body) {
          const e = (body).error;
          if (typeof e === 'string') message = e;
        }
      } catch {
        body = text;
        message = text.slice(0, 500);
      }
    }
  } catch {
    /* ignore unreadable bodies */
  }
  return new ApiError(res.status, message, body);
}

export interface RequestOptions {
  query?: Query;
  body?: unknown;
  rawBody?: BodyInit;
  contentType?: string;
  token?: string | null;
  signal?: AbortSignal;
  accept?: string;
  skipAuthRedirect?: boolean;
}

export async function rawFetch(method: string, path: string, opts: RequestOptions = {}): Promise<Response> {
  const headers: Record<string, string> = {
    ...authHeaders(opts.token === undefined ? tokenStore.get() : opts.token),
    Accept: opts.accept ?? 'application/json',
  };
  let body: BodyInit | undefined;
  if (opts.rawBody !== undefined) {
    body = opts.rawBody;
    if (opts.contentType) headers['Content-Type'] = opts.contentType;
  } else if (opts.body !== undefined) {
    body = JSON.stringify(opts.body);
    headers['Content-Type'] = opts.contentType ?? 'application/json';
  }
  const res = await fetch(path + buildQuery(opts.query), {
    method,
    headers,
    body: body ?? null,
    signal: opts.signal ?? null,
    credentials: 'omit',
    cache: 'no-store',
  });
  if (res.status === 401 && !opts.skipAuthRedirect) tokenStore.notifyUnauthorized();
  return res;
}

export async function request<T>(method: string, path: string, opts: RequestOptions = {}): Promise<T> {
  const res = await rawFetch(method, path, opts);
  if (!res.ok) throw await errorFromResponse(res);
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (!text) return undefined as T;
  const ct = res.headers.get('content-type') ?? '';
  if (ct.includes('json')) return JSON.parse(text) as T;
  return text as T;
}

export const api = {
  get: <T>(path: string, opts?: RequestOptions) => request<T>('GET', path, opts),
  post: <T>(path: string, body?: unknown, opts?: RequestOptions) => request<T>('POST', path, { ...opts, body }),
  put: <T>(path: string, body?: unknown, opts?: RequestOptions) => request<T>('PUT', path, { ...opts, body }),
  patch: <T>(path: string, body?: unknown, opts?: RequestOptions) => request<T>('PATCH', path, { ...opts, body }),
  del: <T>(path: string, opts?: RequestOptions) => request<T>('DELETE', path, opts),
};

export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

export function isNotFound(e: unknown): boolean {
  return e instanceof ApiError && (e.status === 404 || e.status === 405);
}

export function isForbidden(e: unknown): boolean {
  return e instanceof ApiError && e.status === 403;
}
