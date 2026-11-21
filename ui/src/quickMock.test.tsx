import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import { tokenStore } from './api/http';
import { buildStubs, PRESETS } from './lib/quickMock';
import { INFO, mockFetch, renderApp } from './test/render';

function api(role: 'viewer' | 'editor') {
  let n = 0;
  return mockFetch((url, init) => {
    if (url.pathname === '/openapi.json') return { body: { paths: {}, components: { schemas: {} } } };
    if (url.pathname === '/__admin/info') return { body: INFO(role) };
    if (url.pathname === '/__admin/stubs/validate') return { body: { valid: true } };
    if (url.pathname === '/__admin/stubs' && init.method === 'POST') {
      n += 1;
      return { status: 201, body: { ...(JSON.parse(init.body ?? '{}') as object), id: `00000000-0000-4000-8000-00000000000${String(n)}` } };
    }
    if (url.pathname === '/__admin/stubs' && init.method === 'GET') return { body: { total: 0, offset: 0, stubs: [] } };
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

describe('quick mock page', () => {
  it('creates every stub of a template through the admin API and offers to try it', async () => {
    const calls = api('editor');
    const user = userEvent.setup();
    renderApp(<AppRoutes />, '/quick-mock');
    expect(await screen.findByRole('heading', { name: 'Quick mock', level: 2 })).toBeInTheDocument();
    expect(screen.getAllByRole('link', { name: /Quick mock/ }).length).toBeGreaterThan(0);

    await user.click(screen.getByTestId('preset-protected'));
    const preview = screen.getByTestId('quick-preview');
    expect(within(preview).getByRole('radio', { name: '401 fallback' })).toBeInTheDocument();
    expect(within(preview).getByRole('radio', { name: 'CORS preflight' })).toBeInTheDocument();
    expect(screen.getByTestId('behaviour-auth')).toHaveAttribute('data-on', 'true');
    await screen.findByTestId('quick-valid', undefined, { timeout: 3000 });

    await user.click(screen.getByTestId('quick-create'));
    expect(await screen.findByTestId('quick-code')).toHaveTextContent('Authorization: Bearer demo-token');
    const posted = calls.filter((c) => c.method === 'POST' && c.url === '/__admin/stubs').map((c) => JSON.parse(c.body ?? '{}') as unknown);
    const form = PRESETS.find((p) => p.id === 'protected')?.form;
    expect(form).toBeDefined();
    if (form) expect(posted).toEqual(buildStubs(form).map((s) => s.stub));
    await user.click(screen.getByRole('tab', { name: 'Stub JSON' }));
    expect(await screen.findByTestId('quick-created')).toHaveTextContent('Mock is live');
    expect(screen.getByRole('button', { name: 'Update mock' })).toBeInTheDocument();
  });

  it('turns behaviours into the preview and blocks conflicting ones', async () => {
    api('editor');
    const user = userEvent.setup();
    renderApp(<AppRoutes />, '/quick-mock');
    await screen.findByTestId('quick-step-behaviour');
    const flaky = screen.getByTestId('behaviour-flaky');
    await user.click(within(flaky).getByRole('switch'));
    expect(within(flaky).getByTestId('flaky-share')).toHaveTextContent('30% (3 of every 10)');
    const seq = screen.getByTestId('behaviour-sequence');
    expect(within(seq).getByRole('switch')).toBeDisabled();
    await waitFor(() => expect(screen.getByTestId('quick-json')).toHaveTextContent('"responseMode": "random"'));
  });

  it('explains the status issue and keeps Create disabled', async () => {
    api('editor');
    const user = userEvent.setup();
    renderApp(<AppRoutes />, '/quick-mock');
    const status = await screen.findByTestId('quick-status');
    await user.clear(status);
    await user.type(status, '99');
    expect(await screen.findByTestId('quick-issues')).toHaveTextContent('Use a status from 200 to 599');
    expect(screen.getByTestId('quick-create')).toBeDisabled();
    await user.click(screen.getByRole('button', { name: '404' }));
    await waitFor(() => expect(screen.queryByTestId('quick-issues')).not.toBeInTheDocument());
    expect(screen.getByRole('button', { name: '404' })).toHaveAttribute('aria-pressed', 'true');
  });

  it('is not offered to a viewer, who cannot write stubs', async () => {
    api('viewer');
    renderApp(<AppRoutes />, '/quick-mock');
    expect(await screen.findByTestId('no-access')).toHaveTextContent('stubs:write');
    expect(screen.queryByRole('link', { name: /Quick mock/ })).not.toBeInTheDocument();
  });
});
