import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import { tokenStore } from './api/http';
import { INFO, mockFetch, renderApp } from './test/render';

const STUB = {
  id: '6f1c3b8e-0000-4000-8000-000000000001',
  name: 'hello',
  priority: 5,
  request: { method: 'GET', urlPath: '/hello' },
  response: { status: 200, jsonBody: { hello: 'world' } },
  tags: ['demo'],
  enabled: true,
};

function api(role: 'viewer' | 'editor' | 'admin', validToken = 'good') {
  return mockFetch((url, init) => {
    const auth = init.headers.get('authorization');
    if (url.pathname === '/openapi.json') return { body: { paths: {}, components: { schemas: {} } } };
    if (url.pathname.startsWith('/__admin') && auth !== `Bearer ${validToken}`) return { status: 401, body: { error: 'unauthorized' } };
    if (url.pathname === '/__admin/info') return { body: INFO(role) };
    if (url.pathname === '/__admin/stubs' && init.method === 'GET') return { body: { total: 1, offset: 0, stubs: [STUB] } };
    if (url.pathname === `/__admin/stubs/${STUB.id}` && init.method === 'DELETE') return { status: 204 };
    if (url.pathname === `/__admin/stubs/${STUB.id}` && init.method === 'PATCH') return { body: { ...STUB, enabled: false } };
    return undefined;
  });
}

beforeEach(() => {
  tokenStore.set(null);
  window.sessionStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('login', () => {
  it('rejects a bad token and keeps it out of storage', async () => {
    api('admin');
    renderApp(<AppRoutes />, '/login');
    await userEvent.type(await screen.findByTestId('token-input'), 'bad');
    await userEvent.click(screen.getByTestId('login-submit'));
    expect(await screen.findByText('The server rejected this token')).toBeInTheDocument();
    expect(window.sessionStorage.getItem('fastmock.token')).toBeNull();
  });

  it('signs in, keeps the token in sessionStorage only, and shows the principal', async () => {
    api('editor');
    renderApp(<AppRoutes />, '/login');
    await userEvent.type(await screen.findByTestId('token-input'), 'good');
    await userEvent.click(screen.getByTestId('login-submit'));
    expect(await screen.findByTestId('role-badge')).toHaveTextContent('editor');
    expect(window.sessionStorage.getItem('fastmock.token')).toBe('good');
    expect(window.localStorage.getItem('fastmock.token')).toBeNull();
  });

  it('returns to the login page when the server answers 401', async () => {
    tokenStore.set('good');
    api('admin', 'rotated');
    renderApp(<AppRoutes />, '/stubs');
    expect(await screen.findByTestId('login-submit')).toBeInTheDocument();
    expect(tokenStore.get()).toBeNull();
  });
});

describe('stubs list', () => {
  it('lets an editor toggle and delete with confirmation', async () => {
    tokenStore.set('good');
    const calls = api('editor');
    renderApp(<AppRoutes />, '/stubs');
    const row = await screen.findByTestId('stub-row');
    expect(within(row).getByText('hello')).toBeInTheDocument();
    await userEvent.click(within(row).getByRole('switch'));
    await waitFor(() => expect(calls.some((c) => c.method === 'PATCH' && c.body === '{"enabled":false}')).toBe(true));
    const patch = calls.find((c) => c.method === 'PATCH');
    expect(patch?.headers.get('content-type')).toBe('application/merge-patch+json');
    await userEvent.click(within(row).getByRole('button', { name: /actions for hello/i }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /delete/i }));
    expect(calls.some((c) => c.method === 'DELETE')).toBe(false);
    await userEvent.click(await screen.findByTestId('confirm-button'));
    await waitFor(() => expect(calls.some((c) => c.method === 'DELETE' && c.url === `/__admin/stubs/${STUB.id}`)).toBe(true));
  });

  it('is read-only for viewers', async () => {
    tokenStore.set('good');
    api('viewer');
    renderApp(<AppRoutes />, '/stubs');
    const row = await screen.findByTestId('stub-row');
    expect(within(row).getByRole('switch')).toBeDisabled();
    expect(screen.queryByTestId('new-stub')).not.toBeInTheDocument();
    expect(screen.getByText(/read-only view/i)).toBeInTheDocument();
  });

  it('hides the audit log from non-admins', async () => {
    tokenStore.set('good');
    api('editor');
    renderApp(<AppRoutes />, '/stubs');
    await screen.findByTestId('stub-row');
    expect(screen.queryByRole('link', { name: /audit log/i })).not.toBeInTheDocument();
  });
});
