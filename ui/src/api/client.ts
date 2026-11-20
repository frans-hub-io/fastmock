import createClient, { type Middleware } from 'openapi-fetch';
import { ApiError, authHeaders, credentialsFor, tokenStore } from './http';
import type { paths } from './schema';

const auth: Middleware = {
  onRequest({ request }) {
    for (const [k, v] of Object.entries(authHeaders())) request.headers.set(k, v);
    if (!request.headers.has('Accept')) request.headers.set('Accept', 'application/json');
    return request;
  },
  onResponse({ response }) {
    if (response.status === 401) tokenStore.notifyUnauthorized();
    return response;
  },
};

export const client = createClient<paths>({
  baseUrl: window.location.origin,
  credentials: 'omit',
  cache: 'no-store',
  fetch: (req: Request) => globalThis.fetch(new Request(req, { credentials: credentialsFor() })),
});
client.use(auth);

interface FetchResult<T> {
  data?: T;
  error?: unknown;
  response: Response;
}

function messageOf(error: unknown, response: Response): string {
  if (error && typeof error === 'object' && 'error' in error) {
    const e = (error).error;
    if (typeof e === 'string') return e;
  }
  if (typeof error === 'string' && error) return error.slice(0, 500);
  return `${response.status} ${response.statusText}`.trim();
}

export async function unwrap<T>(p: Promise<FetchResult<T>>): Promise<T> {
  const { data, error, response } = await p;
  if (!response.ok) throw new ApiError(response.status, messageOf(error, response), error);
  return data as T;
}

export async function unwrapAllowing<T>(p: Promise<FetchResult<T>>, ...statuses: number[]): Promise<T> {
  const { data, error, response } = await p;
  if (statuses.includes(response.status)) return (data ?? error) as T;
  if (!response.ok) throw new ApiError(response.status, messageOf(error, response), error);
  return data as T;
}
