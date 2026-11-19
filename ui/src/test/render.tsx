import { MantineProvider } from '@mantine/core';
import { ModalsProvider } from '@mantine/modals';
import { Notifications } from '@mantine/notifications';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render } from '@testing-library/react';
import type { ReactElement } from 'react';
import { MemoryRouter } from 'react-router';
import { vi } from 'vitest';
import { AuthProvider } from '../auth/AuthProvider';
import { theme } from '../theme';

export type Handler = (url: URL, init: { method: string; headers: Headers; body: string | null }) => { status?: number; body?: unknown } | undefined;

export function mockFetch(handler: Handler) {
  const calls: { method: string; url: string; headers: Headers; body: string | null }[] = [];
  const fn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const req = input instanceof Request ? input : new Request(new URL(String(input), 'http://localhost'), init);
    const url = new URL(req.url, 'http://localhost');
    const body = req.method === 'GET' || req.method === 'HEAD' ? null : await req.text();
    calls.push({ method: req.method, url: url.pathname + url.search, headers: req.headers, body });
    const r = handler(url, { method: req.method, headers: req.headers, body }) ?? { status: 404, body: { error: 'not found' } };
    const status = r.status ?? 200;
    return new Response(status === 204 ? null : JSON.stringify(r.body ?? {}), { status, headers: { 'content-type': 'application/json' } });
  });
  vi.stubGlobal('fetch', fn);
  return calls;
}

export function renderApp(ui: ReactElement, route = '/') {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(
    <MantineProvider theme={theme} withCssVariables={false} withGlobalClasses={false}>
      <QueryClientProvider client={client}>
        <ModalsProvider>
          <Notifications />
          <MemoryRouter initialEntries={[route]}>
            <AuthProvider>{ui}</AuthProvider>
          </MemoryRouter>
        </ModalsProvider>
      </QueryClientProvider>
    </MantineProvider>,
  );
}

export const INFO = (role: 'viewer' | 'editor' | 'admin') => ({
  name: 'FastMock',
  version: '0.1.0',
  storage: 'memory',
  uptimeSeconds: 5,
  stubsLoaded: 1,
  plugins: [],
  principal: { name: `ci-${role}`, role },
  authDisabled: false,
});
