import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import { tokenStore } from './api/http';
import { INFO, mockFetch, renderApp } from './test/render';

const STUB = {
  id: '6f1c3b8e-0000-4000-8000-000000000001',
  name: 'Get order',
  priority: 5,
  request: { method: 'GET', urlPathTemplate: '/orders/{id}' },
  response: { status: 200 },
  enabled: true,
};

function entry(id: number, minutesAgo: number, matched: boolean, durationMs = 5) {
  return {
    id,
    at: new Date(Date.now() - minutesAgo * 60_000).toISOString(),
    source: 'stubs',
    method: 'GET',
    path: matched ? `/orders/${String(id)}` : `/missing/${String(id)}`,
    query: '',
    headers: {},
    body: '',
    status: matched ? 200 : 404,
    responseBody: '',
    stubId: matched ? STUB.id : null,
    matched,
    durationMs,
  };
}

function api(role: 'viewer' | 'editor' | 'admin', stubs: unknown[] = [STUB]) {
  const requests = [entry(1, 1, true, 3), entry(2, 2, true, 9), entry(3, 3, false, 1), entry(4, 30, true, 40)];
  return mockFetch((url, init) => {
    if (url.pathname === '/openapi.json') return { body: { paths: {}, components: { schemas: {} } } };
    if (url.pathname === '/__admin/info') return { body: INFO(role) };
    if (url.pathname === '/__admin/stubs' && init.method === 'GET') return { body: { total: stubs.length, offset: 0, stubs } };
    if (url.pathname === '/__admin/requests/count') return { body: { count: url.searchParams.get('matched') === 'false' ? 1 : 4 } };
    if (url.pathname === '/__admin/requests') {
      const m = url.searchParams.get('matched');
      const list = m === null ? requests : requests.filter((r) => String(r.matched) === m);
      return { body: { requests: list, total: list.length, offset: 0, limit: 100 } };
    }
    if (url.pathname === '/__admin/callbacks') return { body: { callbacks: [] } };
    if (url.pathname === '/__admin/stubs/validate') return { body: { valid: true } };
    return undefined;
  });
}

beforeEach(() => {
  tokenStore.set('good');
  window.sessionStorage.clear();
  window.localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('command palette', () => {
  it('opens with Ctrl+K, offers screens the principal may open and navigates', async () => {
    api('editor');
    renderApp(<AppRoutes />, '/stubs');
    await screen.findByTestId('stub-row');
    fireEvent.keyDown(document.documentElement, { key: 'k', code: 'KeyK', ctrlKey: true });
    const input = await screen.findByTestId('palette-input');
    await userEvent.type(input, 'journal');
    const options = await screen.findAllByRole('option');
    expect(options[0]).toHaveTextContent('Request journal');
    expect(screen.queryByRole('option', { name: /audit log/i })).not.toBeInTheDocument();
    await userEvent.keyboard('{Enter}');
    expect(await screen.findByRole('heading', { name: 'Request journal' })).toBeInTheDocument();
  });

  it('finds stubs by name', async () => {
    api('admin');
    renderApp(<AppRoutes />, '/');
    await userEvent.click(await screen.findByTestId('open-palette'));
    await userEvent.type(await screen.findByTestId('palette-input'), 'Get order');
    expect(await screen.findByRole('option', { name: /Get order/ })).toBeInTheDocument();
  });
});

describe('shell', () => {
  it('collapses the sidebar to icons and keeps the link names', async () => {
    api('admin');
    renderApp(<AppRoutes />, '/stubs');
    await screen.findByTestId('stub-row');
    await userEvent.click(screen.getByRole('button', { name: 'Collapse sidebar' }));
    const nav = screen.getByRole('navigation', { name: 'Main' });
    expect(within(nav).getByRole('link', { name: 'Stubs' })).toBeInTheDocument();
    expect(within(nav).queryByText('Request journal')).not.toBeInTheDocument();
    expect(window.localStorage.getItem('fastmock.nav.collapsed')).toBe('1');
    await userEvent.click(screen.getByRole('button', { name: 'Expand sidebar' }));
    expect(within(nav).getByText('Request journal')).toBeInTheDocument();
  });

  it('folds a navigation group but never the one holding the current screen', async () => {
    api('admin');
    renderApp(<AppRoutes />, '/stubs');
    await screen.findByTestId('stub-row');
    const traffic = screen.getByRole('button', { name: 'Traffic section' });
    await userEvent.click(traffic);
    expect(screen.queryByRole('link', { name: 'Request journal' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Mocks section' })).toBeDisabled();
  });
});

describe('dashboard', () => {
  it('summarises traffic, latency and the top stubs from the journal', async () => {
    api('editor');
    renderApp(<AppRoutes />, '/');
    expect(await screen.findByTestId('stat-journal')).toHaveTextContent('4');
    const top = await screen.findByTestId('top-stubs');
    expect(top).toHaveTextContent('Get order');
    expect(top).toHaveTextContent('3');
    expect(screen.getByTestId('latency')).toHaveTextContent('40 ms');
    expect(screen.getByTestId('status-classes')).toHaveTextContent('75%');
    const unmatched = await screen.findByTestId('recent-unmatched');
    expect(within(unmatched).getByTestId('dashboard-stub-from-request')).toBeInTheDocument();
    expect(screen.getByRole('img', { name: /Requests per 2 minutes/ })).toBeInTheDocument();
  });

  it('offers no stub creation to viewers', async () => {
    api('viewer');
    renderApp(<AppRoutes />, '/');
    await screen.findByTestId('recent-unmatched');
    expect(screen.queryByTestId('dashboard-stub-from-request')).not.toBeInTheDocument();
  });
});

describe('empty states and the stub editor', () => {
  it('invites an editor to create or import the first stub', async () => {
    api('editor', []);
    renderApp(<AppRoutes />, '/stubs');
    expect(await screen.findByText('No stubs yet')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Create your first mock' })).toHaveAttribute('href', '/quick-mock');
    expect(screen.getByRole('link', { name: 'Blank stub' })).toHaveAttribute('href', '/stubs/new');
    expect(screen.getByRole('link', { name: /How stubs match requests/ })).toHaveAttribute('target', '_blank');
  });

  it('shows no create action to a viewer', async () => {
    api('viewer', []);
    renderApp(<AppRoutes />, '/stubs');
    expect(await screen.findByText('No stubs yet')).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: 'Create your first mock' })).not.toBeInTheDocument();
  });

  it('validates the URL inline and keeps the save bar', async () => {
    api('editor');
    renderApp(<AppRoutes />, '/stubs/new');
    const url = await screen.findByTestId('stub-url');
    await userEvent.clear(url);
    expect(await screen.findByText('Enter a path, for example /orders')).toBeInTheDocument();
    await userEvent.type(url, 'orders');
    expect(await screen.findByText('A path starts with /')).toBeInTheDocument();
    expect(screen.getByTestId('save-stub')).toHaveTextContent('Create stub');
    expect(screen.getByText('1 field to fix')).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText('Unsaved changes')).toBeInTheDocument());
  });
});
